//! Orders sent suzerain → castellan over the `suz/control/0` ALPN, and their
//! acknowledgements. Each order is one JSON object on a bi-stream; the ack is
//! the response object on the same stream.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::manifest::AgentManifest;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Order {
    /// Provision and start a new agent from this manifest. Suzerain assigns
    /// the agent id so it is identical in both registries.
    CreateAgent {
        agent_id: Uuid,
        manifest: AgentManifest,
        /// Secrets sliced from the store for exactly this agent's needs.
        #[serde(default)]
        secrets: crate::secrets::SecretBundle,
    },
    /// Start a previously created (stopped) agent on this daemon.
    /// `force`: tear down any stale running entry first — the recovery path
    /// for an agent the supervisor believes is running but is actually
    /// wedged (e.g. after a failed provisioning left a zombie).
    StartAgent {
        agent_id: Uuid,
        #[serde(default)]
        force: bool,
    },
    /// Graceful stop: notify agent, allow a cleanup window, checkpoint, stop.
    StopAgent {
        agent_id: Uuid,
        cleanup_timeout_secs: u32,
    },
    /// Suspend: graceful stop + snapshot for later boot (same host) or
    /// restore (any host).
    ///
    /// `only_if_idle` (auto-suspend/preemption path): the daemon
    /// re-validates ground truth at execution time and REFUSES the order
    /// (ack failure "busy") if the agent is mid-turn or saw activity after
    /// `not_since`. The control plane's view can be ~60s stale; the
    /// daemon's never is.
    SuspendAgent {
        agent_id: Uuid,
        #[serde(default)]
        only_if_idle: bool,
        #[serde(default)]
        not_since: Option<String>,
    },
    /// Restore an agent from its centrally stored bundle.
    RestoreAgent {
        agent_id: Uuid,
        manifest: AgentManifest,
    },
    /// Graceful, then forced, teardown; delete local state.
    DestroyAgent { agent_id: Uuid },
    /// Replace the manifest of an existing agent (applied on next start).
    UpdateManifest {
        agent_id: Uuid,
        manifest: AgentManifest,
    },
    /// Liveness/heartbeat from control plane side (also used to measure RTT).
    Ping { nonce: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderAck {
    pub success: bool,
    #[serde(default)]
    pub message: Option<String>,
    /// Optional result payload (e.g. agent record after create).
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> AgentManifest {
        let json = serde_json::json!({
            "name": "agent-1",
            "harness": { "type": "pi", "version": "0.84.1" },
            "model": { "provider": "anthropic", "id": "claude-sonnet-4-5" }
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn create_agent_round_trips_and_tags_type_field() {
        let agent_id = Uuid::new_v4();
        let order = Order::CreateAgent {
            agent_id,
            manifest: sample_manifest(),
            secrets: Default::default(),
        };
        let json = serde_json::to_value(&order).unwrap();
        assert_eq!(json["type"], "create_agent");
        assert_eq!(json["agent_id"], agent_id.to_string());

        let back: Order = serde_json::from_value(json).unwrap();
        match back {
            Order::CreateAgent {
                agent_id: id,
                manifest,
                secrets,
            } => {
                assert_eq!(id, agent_id);
                assert_eq!(manifest.name, "agent-1");
                assert!(secrets.is_empty());
            }
            other => panic!("expected CreateAgent, got {other:?}"),
        }
    }

    #[test]
    fn create_agent_defaults_secrets_when_absent() {
        let agent_id = Uuid::new_v4();
        let manifest = serde_json::to_value(sample_manifest()).unwrap();
        let json = serde_json::json!({
            "type": "create_agent",
            "agent_id": agent_id,
            "manifest": manifest
        });
        let order: Order = serde_json::from_value(json).unwrap();
        match order {
            Order::CreateAgent { secrets, .. } => assert!(secrets.is_empty()),
            other => panic!("expected CreateAgent, got {other:?}"),
        }
    }

    #[test]
    fn start_agent_defaults_force_to_false() {
        let agent_id = Uuid::new_v4();
        let json = serde_json::json!({ "type": "start_agent", "agent_id": agent_id });
        let order: Order = serde_json::from_value(json).unwrap();
        match order {
            Order::StartAgent { force, .. } => assert!(!force),
            other => panic!("expected StartAgent, got {other:?}"),
        }
    }

    #[test]
    fn suspend_agent_defaults_only_if_idle_and_not_since() {
        let agent_id = Uuid::new_v4();
        let json = serde_json::json!({ "type": "suspend_agent", "agent_id": agent_id });
        let order: Order = serde_json::from_value(json).unwrap();
        match order {
            Order::SuspendAgent {
                only_if_idle,
                not_since,
                ..
            } => {
                assert!(!only_if_idle);
                assert_eq!(not_since, None);
            }
            other => panic!("expected SuspendAgent, got {other:?}"),
        }
    }

    #[test]
    fn stop_agent_round_trips() {
        let agent_id = Uuid::new_v4();
        let order = Order::StopAgent {
            agent_id,
            cleanup_timeout_secs: 30,
        };
        let json = serde_json::to_string(&order).unwrap();
        let back: Order = serde_json::from_str(&json).unwrap();
        match back {
            Order::StopAgent {
                agent_id: id,
                cleanup_timeout_secs,
            } => {
                assert_eq!(id, agent_id);
                assert_eq!(cleanup_timeout_secs, 30);
            }
            other => panic!("expected StopAgent, got {other:?}"),
        }
    }

    #[test]
    fn destroy_agent_round_trips() {
        let agent_id = Uuid::new_v4();
        let order = Order::DestroyAgent { agent_id };
        let json = serde_json::to_value(&order).unwrap();
        assert_eq!(json["type"], "destroy_agent");
        let back: Order = serde_json::from_value(json).unwrap();
        assert!(matches!(back, Order::DestroyAgent { agent_id: id } if id == agent_id));
    }

    #[test]
    fn ping_round_trips() {
        let order = Order::Ping { nonce: 12345 };
        let json = serde_json::to_value(&order).unwrap();
        assert_eq!(json["type"], "ping");
        assert_eq!(json["nonce"], 12345);
        let back: Order = serde_json::from_value(json).unwrap();
        assert!(matches!(back, Order::Ping { nonce } if nonce == 12345));
    }

    #[test]
    fn order_rejects_unknown_variant() {
        let json = serde_json::json!({ "type": "not_a_real_order" });
        assert!(serde_json::from_value::<Order>(json).is_err());
    }

    #[test]
    fn order_ack_round_trips_defaults() {
        let json = serde_json::json!({ "success": true });
        let ack: OrderAck = serde_json::from_value(json).unwrap();
        assert!(ack.success);
        assert_eq!(ack.message, None);
        assert_eq!(ack.data, None);

        let ack = OrderAck {
            success: false,
            message: Some("busy".to_string()),
            data: Some(serde_json::json!({"agent": "x"})),
        };
        let json = serde_json::to_string(&ack).unwrap();
        let back: OrderAck = serde_json::from_str(&json).unwrap();
        assert!(!back.success);
        assert_eq!(back.message.as_deref(), Some("busy"));
        assert_eq!(back.data.unwrap()["agent"], "x");
    }
}
