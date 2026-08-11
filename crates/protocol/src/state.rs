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
    /// Static node capacity, probed at registration.
    #[serde(default)]
    pub capacity: NodeCapacity,
    /// Dynamic usage snapshot (also refreshed via heartbeat acks).
    #[serde(default)]
    pub usage: NodeUsage,
}

/// Static resources of a daemon host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeCapacity {
    #[serde(default)]
    pub vcpu_total: u32,
    #[serde(default)]
    pub memory_mib_total: u64,
    #[serde(default)]
    pub disk_mib_total: u64,
    #[serde(default)]
    pub gpus: Vec<GpuInfo>,
}

/// Dynamic usage of a daemon host (sampled periodically).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeUsage {
    #[serde(default)]
    pub memory_mib_free: u64,
    /// 1-minute load average.
    #[serde(default)]
    pub cpu_load1: f64,
    #[serde(default)]
    pub disk_mib_free: u64,
    #[serde(default)]
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuInfo {
    pub index: u32,
    pub kind: GpuKind,
    pub name: String,
    /// nvidia = measured; apple = unified system memory; other = absent.
    #[serde(default)]
    pub vram_total_mib: Option<u64>,
    #[serde(default)]
    pub vram_free_mib: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GpuKind {
    Nvidia,
    Apple,
    Other,
}
