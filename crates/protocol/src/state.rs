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

/// The public status vocabulary shown on every surface (web UI, MCP, CLI).
/// Internal lifecycle states stay richer; this is the only mapping users
/// ever see. `busy` is daemon-reported ground truth (a turn in flight).
pub fn public_status(state: AgentState, busy: bool) -> &'static str {
    match state {
        AgentState::Active => {
            if busy {
                "running"
            } else {
                "idle"
            }
        }
        AgentState::Suspended => "sleeping",
        AgentState::Provisioning | AgentState::Restoring => "waking",
        AgentState::Failed => "failed",
        AgentState::Decommissioned => "decommissioned",
    }
}

/// Parse a human duration ("30s", "10m", "2h", "1d") into seconds.
pub fn parse_duration_secs(text: &str) -> Result<u64, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration '{text}'"))?;
    let mult = match unit.trim() {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86400,
        other => return Err(format!("unknown duration unit '{other}' in '{text}'")),
    };
    Ok(n * mult)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_status_mapping() {
        assert_eq!(public_status(AgentState::Active, true), "running");
        assert_eq!(public_status(AgentState::Active, false), "idle");
        assert_eq!(public_status(AgentState::Suspended, false), "sleeping");
        assert_eq!(public_status(AgentState::Provisioning, false), "waking");
        assert_eq!(public_status(AgentState::Restoring, true), "waking");
        assert_eq!(public_status(AgentState::Failed, false), "failed");
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_secs("30s"), Ok(30));
        assert_eq!(parse_duration_secs("30m"), Ok(1800));
        assert_eq!(parse_duration_secs("2h"), Ok(7200));
        assert_eq!(parse_duration_secs("1d"), Ok(86400));
        assert_eq!(parse_duration_secs("45"), Ok(45));
        assert!(parse_duration_secs("soon").is_err());
        assert!(parse_duration_secs("10x").is_err());
    }
}
