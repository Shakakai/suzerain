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
