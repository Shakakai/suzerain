//! Shared agent orchestration used by both the operator-socket API and the
//! web API: create, lifecycle orders, restore. Single source of truth for
//! the order flows (registry update + daemon order + audit).

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

pub enum Lifecycle {
    Start,
    Stop,
    Suspend,
    Destroy,
}

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
    let placement = scheduler::place(
        cp,
        &Constraints {
            require,
            pin,
            manifest: manifest.clone(),
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
        }
    }
    store
        .update_agent_state(&agent_id, AgentState::Active)
        .await?;
    audit::record(
        "agent_create",
        json!({"name": row.name, "id": agent_id, "daemon": row.daemon_endpoint_id}),
    )
    .await;
    Ok(())
}

/// Start/stop/suspend/destroy an agent by name.
///
/// `force` only affects Stop: when the daemon is unreachable the registry
/// is still marked suspended (the VM may keep running orphaned; the audit
/// entry records the forcing). Stop always tolerates a daemon-side
/// "no agent" rejection — e.g. an agent stuck in `provisioning` whose
/// create order never landed — so any state can be stopped.
pub async fn lifecycle(
    cp: &Arc<ControlPlane>,
    name: &str,
    action: Lifecycle,
    force: bool,
) -> Result<()> {
    let store = cp.store().clone();
    let cmd = match action {
        Lifecycle::Start => "agent_start",
        Lifecycle::Stop => "agent_stop",
        Lifecycle::Suspend => "agent_suspend",
        Lifecycle::Destroy => "agent_destroy",
    };
    let agent = store
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
    let order = match action {
        Lifecycle::Start => Order::StartAgent {
            agent_id: agent.id,
            force,
        },
        Lifecycle::Stop => Order::StopAgent {
            agent_id: agent.id,
            cleanup_timeout_secs: 30,
        },
        Lifecycle::Suspend => Order::SuspendAgent { agent_id: agent.id },
        Lifecycle::Destroy => Order::DestroyAgent { agent_id: agent.id },
    };
    let ack = cp.order(&daemon, &order).await;
    match &ack {
        Ok(ack) if !ack.success => {
            let msg = ack.message.as_deref().unwrap_or("");
            // Stop and Destroy tolerate "no agent" daemon-side: the desired
            // end state (nothing running) already holds.
            let tolerable =
                msg.contains("no agent") && matches!(action, Lifecycle::Stop | Lifecycle::Destroy);
            if !tolerable {
                bail!("daemon: {msg}");
            }
        }
        Err(_) => {
            let forced = force && matches!(action, Lifecycle::Stop | Lifecycle::Destroy);
            if !forced && !matches!(action, Lifecycle::Destroy) {
                bail!("order failed: daemon unreachable");
            }
        }
        _ => {}
    }
    match action {
        Lifecycle::Start => {
            store
                .update_agent_state(&agent.id, AgentState::Active)
                .await?
        }
        Lifecycle::Stop | Lifecycle::Suspend => {
            store
                .update_agent_state(&agent.id, AgentState::Suspended)
                .await?
        }
        Lifecycle::Destroy => {
            store.delete_agent(&agent.id).await?;
        }
    }
    audit::record(
        cmd.trim_start_matches("agent_"),
        json!({"name": name, "id": agent.id, "force": force}),
    )
    .await;
    Ok(())
}

/// Restore a suspended agent onto a (possibly different) daemon: bundle
/// integrity check → schedule → bundle upload → registry + audit. Shared
/// by the operator-socket API and the web/MCP REST route.
pub async fn restore_agent(
    cp: &Arc<ControlPlane>,
    name: &str,
    pin: Option<String>,
) -> Result<serde_json::Value> {
    let store = cp.store().clone();
    let agent = store
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    if agent.state == AgentState::Active {
        // Active means running somewhere. If the owning daemon is offline,
        // that conviction is stale — restore may proceed.
        let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
        if cp.session(&daemon).await.is_some() {
            bail!("agent '{name}' is currently active — stop or suspend it first");
        }
    }
    let bundle = crate::bundle::load(&agent.id).await?;
    let target = scheduler::place(
        cp,
        &Constraints {
            require: Default::default(),
            pin,
            manifest: agent.manifest.clone(),
        },
    )
    .await?;
    store
        .update_agent_state(&agent.id, AgentState::Restoring)
        .await?;

    let (mut send, mut recv) = cp
        .open_stream(
            &target.endpoint_id,
            &suzerain_protocol::control::StreamHello::Restore { agent_id: agent.id },
        )
        .await?;
    use suzerain_protocol::control::{BundleAck, BundleMessage};
    use suzerain_protocol::framing::{read_jsonl, write_jsonl};
    write_jsonl(
        &mut send,
        &BundleMessage::Start {
            manifest: Box::new(bundle.manifest.clone()),
            session_file: bundle.session_file.clone(),
            secrets: Some(crate::secrets::slice_for(&bundle.manifest)?),
        },
    )
    .await?;
    for (rel, abs) in &bundle.files {
        let data = tokio::fs::read(abs).await?;
        if let Some(want) = bundle.hashes.get(rel) {
            let got = suzerain_protocol::framing::sha256_hex(&data);
            if &got != want {
                bail!(
                    "stored bundle for '{name}' failed integrity check ({rel}): possible tampering or disk corruption"
                );
            }
        }
        write_jsonl(
            &mut send,
            &BundleMessage::File {
                path: rel.clone(),
                sha256: Some(suzerain_protocol::framing::sha256_hex(&data)),
                data: crate::bundle::base64_encode(&data),
                last: true,
            },
        )
        .await?;
    }
    write_jsonl(&mut send, &BundleMessage::End).await?;
    send.finish()?;
    let ack: BundleAck = read_jsonl(&mut recv).await?;
    if !ack.success {
        store
            .update_agent_state(&agent.id, AgentState::Failed)
            .await?;
        bail!("restore failed: {}", ack.message.unwrap_or_default());
    }
    store
        .set_agent_daemon(&agent.id, &target.endpoint_id.to_string())
        .await?;
    store
        .update_agent_state(&agent.id, AgentState::Active)
        .await?;
    audit::record(
        "agent_restore",
        json!({"name": name, "id": agent.id, "daemon": target.endpoint_id.to_string()}),
    )
    .await;
    Ok(json!({"restored": name, "daemon": target.endpoint_id.to_string()}))
}
