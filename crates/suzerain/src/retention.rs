//! Central log retention. Default policy is keep-everything (Q-F); setting
//! `[retention] days = N` in `<data>/config.toml` prunes central log events
//! older than N days (hourly sweep).

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tracing::{info, warn};

use crate::identity::data_dir;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub web: Web,
    #[serde(default)]
    pub auto_suspend: AutoSuspend,
    #[serde(default)]
    pub bundles: Bundles,
    #[serde(default)]
    pub operator: Operator,
    #[serde(default)]
    pub role: Role,
}

/// Which half of the merged binary this process runs as
/// (docs/UNIFIED-AGENT-API-DESIGN.md §4.1). Overridable per-invocation via
/// `suzerain run --mode <standalone|control|agent>`; this config value is
/// only the default when `--mode` is omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Role {
    #[serde(default)]
    pub mode: RoleMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum RoleMode {
    /// Both the control plane and a co-located agent-hosting process, one
    /// binary, two OS processes (the default — "one box, zero config").
    #[default]
    Standalone,
    /// Control-plane only: registry, scheduling, client-facing API. No
    /// local agent-hosting.
    Control,
    /// Agent-hosting only (today's `castellan run`): provisions/supervises
    /// agent VMs, reports to a `control`/`standalone` node elsewhere.
    Agent,
}

/// iroh operator channel (Suzy desktop clients): which operator public
/// keys may use `suz/operator/0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operator {
    #[serde(default = "default_operator_enabled")]
    pub enabled: bool,
    /// EndpointIds allowed to use the operator channel. Empty = the
    /// channel accepts no one (denials are logged with the caller's id).
    #[serde(default)]
    pub allow: Vec<String>,
}

impl Default for Operator {
    fn default() -> Self {
        Self {
            enabled: default_operator_enabled(),
            allow: Vec::new(),
        }
    }
}

fn default_operator_enabled() -> bool {
    true
}

/// Auto-suspend policy: agents that have been idle (no turn in flight, no
/// activity) for `idle_timeout` are suspended automatically — VM
/// checkpoint + bundle upload — and woken transparently when a message
/// arrives. Per-agent overrides live in the manifest `[lifecycle]` block
/// or a runtime override (`suz agent config`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoSuspend {
    #[serde(default = "default_auto_suspend_enabled")]
    pub enabled: bool,
    /// Global default inactivity timeout ("30m", "2h", …).
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout: String,
    /// How often the control plane evaluates idle agents.
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval: String,
    /// Wake attempts before a message fails (failed daemons are excluded
    /// from subsequent attempts).
    #[serde(default = "default_wake_retry_attempts")]
    pub wake_retry_attempts: u32,
}

impl Default for AutoSuspend {
    fn default() -> Self {
        Self {
            enabled: default_auto_suspend_enabled(),
            idle_timeout: default_idle_timeout(),
            sweep_interval: default_sweep_interval(),
            wake_retry_attempts: default_wake_retry_attempts(),
        }
    }
}

fn default_auto_suspend_enabled() -> bool {
    true
}
fn default_idle_timeout() -> String {
    "30m".into()
}
fn default_sweep_interval() -> String {
    "30s".into()
}
fn default_wake_retry_attempts() -> u32 {
    3
}

impl AutoSuspend {
    pub fn idle_timeout_secs(&self) -> u64 {
        suzerain_protocol::state::parse_duration_secs(&self.idle_timeout).unwrap_or(30 * 60)
    }

    pub fn sweep_interval_secs(&self) -> u64 {
        suzerain_protocol::state::parse_duration_secs(&self.sweep_interval).unwrap_or(30)
    }
}

/// Where agent restore bundles (snapshots) are stored. Point this at a
/// large/external disk when bundles outgrow the system drive.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bundles {
    /// Absolute path; default `<data>/bundles`.
    #[serde(default)]
    pub dir: Option<String>,
}

/// Embedded web UI (local-only, docs/WEB-UI.md).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Web {
    #[serde(default = "default_web_enabled")]
    pub enabled: bool,
    #[serde(default = "default_web_port")]
    pub port: u16,
}

impl Default for Web {
    fn default() -> Self {
        Self {
            enabled: default_web_enabled(),
            port: default_web_port(),
        }
    }
}

fn default_web_enabled() -> bool {
    true
}

fn default_web_port() -> u16 {
    8484
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Retention {
    /// Days to keep central log events. 0 = keep forever (default).
    #[serde(default)]
    pub days: u32,
    /// Days to keep restore bundles for decommissioned/unknown agents.
    /// 0 = keep forever (default).
    #[serde(default)]
    pub bundle_days: u32,
    /// Days to keep audit log entries. 0 = keep forever (default).
    #[serde(default)]
    pub audit_days: u32,
}

pub fn config_path() -> PathBuf {
    // `suzerain.toml` in the shared fleet home (castellan.toml sits beside
    // it); a legacy `config.toml` is renamed on first access.
    crate::identity::migrate_name(&data_dir(), "config.toml", "suzerain.toml")
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
}

pub fn save_config(config: &Config) -> Result<()> {
    write_config_to(&config_path(), config)
}

fn write_config_to(path: &std::path::Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(config)?)?;
    Ok(())
}

/// Add an EndpointId to `[operator] allow` in the config file at `path`
/// (created if missing). Returns true when the entry was newly added.
/// Note: rewriting the file drops any hand-written comments.
pub fn add_operator_allow_to(path: &std::path::Path, endpoint_id: &str) -> Result<bool> {
    let mut config: Config = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        Config::default()
    };
    if config.operator.allow.iter().any(|e| e == endpoint_id) {
        return Ok(false);
    }
    config.operator.allow.push(endpoint_id.to_string());
    write_config_to(path, &config)?;
    Ok(true)
}

/// Add an EndpointId to `[operator] allow` in `$SUZERAIN_HOME/config.toml`.
pub fn add_operator_allow(endpoint_id: &str) -> Result<bool> {
    add_operator_allow_to(&config_path(), endpoint_id)
}

/// Run the retention sweep hourly, forever.
pub async fn run() {
    loop {
        if let Err(err) = sweep().await {
            warn!("retention sweep failed: {err:#}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

async fn sweep() -> Result<()> {
    let config = load_config()?;
    if config.retention.days > 0 {
        sweep_logs(data_dir().join("logs"), config.retention.days).await?;
    }
    if config.retention.audit_days > 0 {
        sweep_logs(data_dir().join("audit.jsonl"), config.retention.audit_days).await?;
    }
    if config.retention.bundle_days > 0 {
        sweep_bundles(config.retention.bundle_days).await?;
    }
    // Delivered/failed wake-queue rows follow the audit retention window.
    if config.retention.audit_days > 0 {
        if let Ok(store) = crate::store::Store::open().await {
            store.prune_messages(config.retention.audit_days).await.ok();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_operator_allow_roundtrip() {
        let dir = std::env::temp_dir().join(format!("suz-cfgtest-{}", uuid::Uuid::new_v4()));
        let path = dir.join("suzerain.toml");

        // Creates the file (and parent dir) from nothing.
        assert!(add_operator_allow_to(&path, "id-one").unwrap());
        // Duplicate add is a no-op.
        assert!(!add_operator_allow_to(&path, "id-one").unwrap());
        assert!(add_operator_allow_to(&path, "id-two").unwrap());

        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.operator.allow, vec!["id-one", "id-two"]);
        assert!(config.operator.enabled);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn add_operator_allow_preserves_other_sections() {
        let dir = std::env::temp_dir().join(format!("suz-cfgtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("suzerain.toml");
        std::fs::write(&path, "[retention]\ndays = 30\n").unwrap();

        assert!(add_operator_allow_to(&path, "id-one").unwrap());
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.retention.days, 30);
        assert_eq!(config.operator.allow, vec!["id-one"]);

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// Prune events older than `days` from a JSONL file (or every *.jsonl in a
/// directory), preserving order. Files stay; stale events leave.
async fn sweep_logs(target: PathBuf, days: u32) -> Result<()> {
    let cutoff = OffsetDateTime::now_utc() - Duration::days(days as i64);
    let logs = target;
    if logs.is_file() {
        return prune_file(&logs, cutoff).await;
    }
    let mut entries = match tokio::fs::read_dir(&logs).await {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl") != Some(true) {
            continue;
        }
        prune_file(&path, cutoff).await?;
    }
    Ok(())
}

async fn prune_file(path: &std::path::Path, cutoff: OffsetDateTime) -> Result<()> {
    {
        let content = tokio::fs::read_to_string(path).await?;
        let mut kept = 0usize;
        let mut dropped = 0usize;
        let mut buf = String::new();
        for line in content.lines() {
            let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let keep = ev["at"]
                .as_str()
                .and_then(|at| OffsetDateTime::parse(at, &Rfc3339).ok())
                .map(|at| at >= cutoff)
                .unwrap_or(true);
            if keep {
                buf.push_str(line);
                buf.push('\n');
                kept += 1;
            } else {
                dropped += 1;
            }
        }
        if dropped > 0 {
            tokio::fs::write(&path, &buf).await?;
            info!(file = %path.display(), kept, dropped, "retention pruned");
        }
        Ok(())
    }
}

/// Delete restore bundles for agents no longer in the registry, or whose
/// meta is older than `days`.
async fn sweep_bundles(days: u32) -> Result<()> {
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(days as u64 * 86400);
    let bundles = crate::bundle::bundle_root();
    let mut entries = match tokio::fs::read_dir(&bundles).await {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let meta = entry.path().join("meta.json");
        let stale = match tokio::fs::metadata(&meta).await.and_then(|m| m.modified()) {
            Ok(mtime) => mtime < cutoff,
            Err(_) => false,
        };
        if stale {
            tokio::fs::remove_dir_all(entry.path()).await.ok();
            info!(bundle = %entry.path().display(), "retention removed stale bundle");
        }
    }
    Ok(())
}
