//! Lifecycle states and registry records shared by both sides.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Provisioning,
    Active,
    Suspended,
    Restoring,
    Failed,
    Decommissioned,
}

/// Registration/heartbeat facts a castellan reports to suzerain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    /// iroh EndpointId string (the daemon's public identity).
    pub endpoint_id: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    /// Free-form scheduling labels (e.g. "gpu", "office", repo-locality).
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Max agents this daemon is willing to run concurrently.
    pub max_agents: u32,
    /// Currently running agents.
    #[serde(default)]
    pub agents: Vec<Uuid>,
}
