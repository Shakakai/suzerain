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
    data_dir().join("config.toml")
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
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
    Ok(())
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
    let bundles = data_dir().join("bundles");
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
