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
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Retention {
    /// Days to keep central log events. 0 = keep forever (default).
    #[serde(default)]
    pub days: u32,
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
    if config.retention.days == 0 {
        return Ok(()); // keep everything
    }
    let cutoff = OffsetDateTime::now_utc() - Duration::days(config.retention.days as i64);
    let logs = data_dir().join("logs");
    let mut entries = match tokio::fs::read_dir(&logs).await {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().map(|e| e == "jsonl") != Some(true) {
            continue;
        }
        let content = tokio::fs::read_to_string(&path).await?;
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
            info!(file = %path.display(), kept, dropped, "retention pruned central log");
        }
    }
    Ok(())
}
