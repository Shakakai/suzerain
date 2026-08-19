//! Shared agent orchestration used by the operator-socket API and the web
//! API: create and destroy. Start/stop/suspend/restore are no longer
//! user-facing verbs — the control plane suspends idle agents automatically
//! and wakes them transparently on demand (see `lifecycle` and `wake`).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::json;
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::order::Order;
use suzerain_protocol::state::AgentState;
use uuid::Uuid;

use crate::audit;
use crate::control::ControlPlane;
use crate::scheduler::{self, Constraints};
use crate::store::AgentRow;

/// Create an agent: name check → schedule → order → registry + audit.
/// Scheduler rejections surface verbatim (per-candidate reasons).
///
/// Synchronous: blocks on the daemon's provisioning ack (up to the order
/// timeout). Used by the operator-socket API; the web API uses
/// `create_agent_background` so the UI returns immediately.
pub async fn create_agent(
    cp: &Arc<ControlPlane>,
    manifest: AgentManifest,
    require_extra: BTreeMap<String, String>,
    pin: Option<String>,
) -> Result<(AgentRow, String)> {
    let (row, placement) = prepare_create(cp, manifest, require_extra, pin).await?;
    complete_create(cp, &row, &placement).await?;
    let agent = cp
        .store()
        .get_agent_by_name(&row.name)
        .await?
        .context("agent vanished after create")?;
    Ok((agent, placement.daemon_hostname))
}

/// Create an agent without waiting for provisioning: the registry row is
/// inserted (state `provisioning`) and the daemon order runs in a
/// background task. Fast failures (duplicate name, invalid model,
/// scheduler rejection) still return synchronously; daemon-side failures
/// land in the row's state + audit log.
pub async fn create_agent_background(
    cp: &Arc<ControlPlane>,
    manifest: AgentManifest,
    require_extra: BTreeMap<String, String>,
    pin: Option<String>,
) -> Result<(AgentRow, String)> {
    let (row, placement) = prepare_create(cp, manifest, require_extra, pin).await?;
    let hostname = placement.daemon_hostname.clone();
    let cp = Arc::clone(cp);
    let bg_row = row.clone();
    tokio::spawn(async move {
        if let Err(err) = complete_create(&cp, &bg_row, &placement).await {
            tracing::warn!(agent = %bg_row.name, "background create failed: {err:#}");
        }
    });
    Ok((row, hostname))
}

/// Phase 1: validate, schedule, insert the registry row (state
/// `provisioning`). Returns the row and the chosen placement.
async fn prepare_create(
    cp: &Arc<ControlPlane>,
    manifest: AgentManifest,
    require_extra: BTreeMap<String, String>,
    pin: Option<String>,
) -> Result<(AgentRow, scheduler::Placement)> {
    let store = cp.store().clone();
    if store.get_agent_by_name(&manifest.name).await?.is_some() {
        bail!("an agent named '{}' already exists", manifest.name);
    }
    // Fail fast on providers/models pi can't resolve (crash-loops in the VM
    // otherwise; see catalog module docs).
    crate::catalog::validate_model(&manifest.model.provider, &manifest.model.id)?;
    // Fail fast on secrets the store doesn't hold — before the row exists
    // and before any daemon order, so a bad manifest can't wedge an agent
    // in `provisioning` (slice_for's failure would only surface in the
    // background create task).
    crate::secrets::preflight(&manifest)?;
    let mut require = manifest.schedule.require.clone();
    require.extend(require_extra);
    let pin = pin.or_else(|| manifest.schedule.daemon.clone());
    // May suspend idle agents to free capacity (resource-pressure
    // preemption of authoritatively-idle agents).
    let placement = scheduler::place_or_preempt(
        cp,
        &Constraints {
            require,
            pin,
            manifest: manifest.clone(),
            exclude: vec![],
        },
    )
    .await?;
    let agent_id = Uuid::new_v4();
    let row = AgentRow {
        id: agent_id,
        name: manifest.name.clone(),
        daemon_endpoint_id: placement.endpoint_id.to_string(),
        manifest: manifest.clone(),
        state: AgentState::Provisioning,
        created_at: crate::store::castellan_time_now(),
        session_file: None,
        idle_secs: None,
        busy: None,
        activity_reported_at: None,
        needs_attention: false,
        auto_suspend_override: None,
        woke_at: None,
    };
    store.create_agent(&row).await?;
    Ok((row, placement))
}

/// Phase 2: send the create order and settle the row state from the ack.
async fn complete_create(
    cp: &Arc<ControlPlane>,
    row: &AgentRow,
    placement: &scheduler::Placement,
) -> Result<()> {
    let store = cp.store().clone();
    let agent_id = row.id;
    let ack = cp
        .order(
            &placement.endpoint_id,
            &Order::CreateAgent {
                agent_id,
                secrets: crate::secrets::slice_for(&row.manifest)?,
                manifest: row.manifest.clone(),
            },
        )
        .await
        .map_err(|e| {
            // Transient transport failure: the daemon may still be
            // provisioning fine. Do NOT mark Failed — the row stays
            // Provisioning and a retry is idempotent daemon-side.
            e.context("create order transport error (agent left provisioning; retry is safe)")
        })?;
    if !ack.success {
        store
            .update_agent_state(&agent_id, AgentState::Failed)
            .await?;
        bail!(
            "daemon rejected create: {}",
            ack.message.unwrap_or_default()
        );
    }
    if let Some(data) = &ack.data {
        if let Some(sf) = data["session_file"].as_str() {
            store.set_agent_session_file(&agent_id, sf).await?;
            // Open the agent's first session era.
            store.start_agent_session(&agent_id, sf).await?;
        }
    }
    store
        .update_agent_state(&agent_id, AgentState::Active)
        .await?;
    store.set_agent_woke_at(&agent_id).await?;
    audit::record(
        "agent_create",
        json!({"name": row.name, "id": agent_id, "daemon": row.daemon_endpoint_id}),
    )
    .await;
    Ok(())
}

/// Destroy an agent: daemon order (graceful, then forced teardown) →
/// registry row removal. Tolerates a daemon-side "no agent" rejection and,
/// with `force`, an unreachable daemon (the VM may keep running orphaned;
/// the audit entry records the forcing). Queued wake messages are failed.
pub async fn destroy_agent(cp: &Arc<ControlPlane>, name: &str, force: bool) -> Result<()> {
    let store = cp.store().clone();
    let agent = store
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
    let ack = cp
        .order(&daemon, &Order::DestroyAgent { agent_id: agent.id })
        .await;
    match &ack {
        Ok(ack) if !ack.success => {
            let msg = ack.message.as_deref().unwrap_or("");
            // Tolerate "no agent" daemon-side: the desired end state
            // (nothing running) already holds.
            if !msg.contains("no agent") {
                bail!("daemon: {msg}");
            }
        }
        Err(_) if !force => {
            bail!("order failed: daemon unreachable (retry with force)");
        }
        _ => {}
    }
    store.delete_agent(&agent.id).await?;
    let pending = store.pending_messages(&agent.id).await.unwrap_or_default();
    let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
    store
        .set_message_status(&ids, "failed", Some("agent destroyed"))
        .await
        .ok();
    audit::record(
        "destroy",
        json!({"name": name, "id": agent.id, "force": force}),
    )
    .await;
    Ok(())
}
