//! Auto-suspend sweep: the control plane is the **single authority** on
//! suspend decisions (castellans only report ground truth). Periodically
//! evaluates each agent's effective policy against daemon-reported idle
//! facts and issues *guarded* suspend orders — the daemon re-validates at
//! execution time and refuses if the agent went busy since the snapshot.

use std::sync::Arc;

use anyhow::Result;
use suzerain_protocol::manifest::{AutoSuspendPolicy, Lifecycle};
use suzerain_protocol::order::Order;
use suzerain_protocol::state::AgentState;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::control::ControlPlane;
use crate::retention::Config;
use crate::store::AgentRow;

/// Preemption grace: a recently woken agent is not a resource-pressure
/// preemption candidate (anti-thrash).
pub const WAKE_GRACE_SECS: u64 = 300;

pub async fn run(cp: Arc<ControlPlane>) {
    loop {
        let cfg = crate::retention::load_config().unwrap_or_default();
        if cfg.auto_suspend.enabled {
            if let Err(err) = sweep(&cp, &cfg).await {
                warn!("auto-suspend sweep failed: {err:#}");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(
            cfg.auto_suspend.sweep_interval_secs(),
        ))
        .await;
    }
}

/// Effective auto-suspend timeout for an agent, or None when exempt
/// ("never"). Priority: runtime override → manifest → global config.
pub fn effective_timeout_secs(agent: &AgentRow, cfg: &Config) -> Option<u64> {
    let policy = if agent.auto_suspend_override.is_some() {
        Lifecycle {
            auto_suspend: agent.auto_suspend_override.clone(),
        }
        .auto_suspend_policy()
    } else {
        agent.manifest.lifecycle.auto_suspend_policy()
    };
    match policy {
        Ok(AutoSuspendPolicy::Never) => None,
        Ok(AutoSuspendPolicy::After(secs)) => Some(secs),
        Ok(AutoSuspendPolicy::Inherit) | Err(_) => {
            if cfg.auto_suspend.enabled {
                Some(cfg.auto_suspend.idle_timeout_secs())
            } else {
                None
            }
        }
    }
}

/// Skew-immune idle estimate: daemon-reported idle seconds plus the age of
/// the report on the control plane's own clock.
pub fn extrapolated_idle_secs(agent: &AgentRow) -> u64 {
    let Some(idle) = agent.idle_secs else {
        return 0;
    };
    let age = agent
        .activity_reported_at
        .as_deref()
        .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
        .map(|t| (OffsetDateTime::now_utc() - t).whole_seconds().max(0) as u64)
        .unwrap_or(0);
    idle as u64 + age
}

/// Is this agent preemptible under resource pressure? Must be Active,
/// authoritatively idle (daemon ground truth), not policy-exempt, and past
/// the wake grace window.
pub fn is_preemptible(agent: &AgentRow, cfg: &Config) -> bool {
    if agent.state != AgentState::Active || agent.busy != Some(false) {
        return false;
    }
    if effective_timeout_secs(agent, cfg).is_none() {
        return false; // "never" exempts from both timeout and preemption
    }
    if let Some(woke) = &agent.woke_at {
        if let Ok(t) = OffsetDateTime::parse(woke, &Rfc3339) {
            let since = (OffsetDateTime::now_utc() - t).whole_seconds().max(0) as u64;
            if since < WAKE_GRACE_SECS {
                return false;
            }
        }
    }
    true
}

async fn sweep(cp: &Arc<ControlPlane>, cfg: &Config) -> Result<()> {
    for agent in cp.store().list_agents().await? {
        // Only suspend on daemon ground truth: busy=None (never reported)
        // is treated as unknown, not idle.
        if agent.state != AgentState::Active || agent.busy != Some(false) {
            continue;
        }
        let Some(timeout) = effective_timeout_secs(&agent, cfg) else {
            continue;
        };
        if extrapolated_idle_secs(&agent) < timeout {
            continue;
        }
        let Ok(daemon) = agent.daemon_endpoint_id.parse() else {
            continue;
        };
        if cp.session(&daemon).await.is_none() {
            continue;
        }
        // Serialize against a concurrent wake of the same agent.
        let lock = cp.agent_lock(&agent.id).await;
        let _guard = lock.lock().await;
        let Some(agent) = cp.store().get_agent(&agent.id).await? else {
            continue;
        };
        if agent.state != AgentState::Active || agent.busy != Some(false) {
            continue; // woke or went busy while we waited on the lock
        }
        info!(agent = %agent.name, timeout, "auto-suspending idle agent");
        let ack = cp
            .order(
                &daemon,
                &Order::SuspendAgent {
                    agent_id: agent.id,
                    only_if_idle: true,
                    not_since: agent.activity_reported_at.clone(),
                },
            )
            .await;
        match ack {
            Ok(a) if a.success => {
                cp.store()
                    .update_agent_state(&agent.id, AgentState::Suspended)
                    .await?;
                crate::audit::record(
                    "agent_auto_suspend",
                    serde_json::json!({"name": agent.name, "id": agent.id}),
                )
                .await;
            }
            // Daemon ground truth disagreed (went busy) — back off.
            Ok(a) => info!(
                agent = %agent.name,
                "suspend refused by daemon: {}",
                a.message.unwrap_or_default()
            ),
            Err(e) => warn!(agent = %agent.name, "auto-suspend order failed: {e:#}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use suzerain_protocol::manifest::AgentManifest;

    fn agent(manifest_policy: Option<&str>, override_: Option<&str>) -> AgentRow {
        let toml = format!(
            "name = \"a\"\nharness = {{ type = \"pi\", version = \"1\" }}\nmodel = {{ provider = \"p\", id = \"m\" }}\n{}",
            manifest_policy
                .map(|p| format!("[lifecycle]\nauto_suspend = \"{p}\""))
                .unwrap_or_default()
        );
        let manifest: AgentManifest = toml::from_str(&toml).unwrap();
        AgentRow {
            id: uuid::Uuid::new_v4(),
            name: "a".into(),
            daemon_endpoint_id: "d".into(),
            manifest,
            state: AgentState::Active,
            created_at: String::new(),
            session_file: None,
            idle_secs: None,
            busy: None,
            activity_reported_at: None,
            needs_attention: false,
            auto_suspend_override: override_.map(str::to_string),
            woke_at: None,
        }
    }

    #[test]
    fn policy_resolution() {
        let cfg = Config::default(); // global 30m
        assert_eq!(
            effective_timeout_secs(&agent(None, None), &cfg),
            Some(30 * 60)
        );
        assert_eq!(
            effective_timeout_secs(&agent(Some("10m"), None), &cfg),
            Some(600)
        );
        assert_eq!(
            effective_timeout_secs(&agent(Some("never"), None), &cfg),
            None
        );
        // Runtime override beats the manifest.
        assert_eq!(
            effective_timeout_secs(&agent(Some("10m"), Some("2h")), &cfg),
            Some(7200)
        );
        assert_eq!(
            effective_timeout_secs(&agent(Some("10m"), Some("never")), &cfg),
            None
        );
        // "default" override clears back to inherit.
        assert_eq!(
            effective_timeout_secs(&agent(Some("10m"), Some("default")), &cfg),
            Some(30 * 60)
        );
    }

    #[test]
    fn disabled_globally_means_no_inherit() {
        let mut cfg = Config::default();
        cfg.auto_suspend.enabled = false;
        assert_eq!(effective_timeout_secs(&agent(None, None), &cfg), None);
        // Explicit per-agent timeout still applies when globally disabled.
        assert_eq!(
            effective_timeout_secs(&agent(Some("10m"), None), &cfg),
            Some(600)
        );
    }
}
