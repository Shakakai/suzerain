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
pub async fn create_agent(
    cp: &Arc<ControlPlane>,
    manifest: AgentManifest,
    require_extra: BTreeMap<String, String>,
    pin: Option<String>,
) -> Result<(AgentRow, String)> {
    let store = cp.store().clone();
    if store.get_agent_by_name(&manifest.name).await?.is_some() {
        bail!("an agent named '{}' already exists", manifest.name);
    }
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
    let ack = cp
        .order(
            &placement.endpoint_id,
            &Order::CreateAgent {
                agent_id,
                secrets: crate::secrets::slice_for(&manifest)?,
                manifest,
            },
        )
        .await?;
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
    let agent = store
        .get_agent_by_name(&row.name)
        .await?
        .context("agent vanished after create")?;
    Ok((agent, placement.daemon_hostname))
}

/// Start/stop/suspend/destroy an agent by name.
pub async fn lifecycle(cp: &Arc<ControlPlane>, name: &str, action: Lifecycle) -> Result<()> {
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
        Lifecycle::Start => Order::StartAgent { agent_id: agent.id },
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
            let tolerable = matches!(action, Lifecycle::Destroy)
                && ack.message.as_deref().unwrap_or("").contains("no agent");
            if !tolerable {
                bail!("daemon: {}", ack.message.clone().unwrap_or_default());
            }
        }
        Err(_) if !matches!(action, Lifecycle::Destroy) => {
            bail!("order failed: daemon unreachable");
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
        json!({"name": name, "id": agent.id}),
    )
    .await;
    Ok(())
}
