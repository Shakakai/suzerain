//! Audit log: append-only JSONL record of control-plane actions.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::identity::data_dir;
use crate::store::castellan_time_now;

pub async fn record(action: &str, detail: Value) {
    if let Err(err) = append(action, detail).await {
        tracing::warn!("audit append failed: {err:#}");
    }
}

async fn append(action: &str, detail: Value) -> Result<()> {
    let path = data_dir().join("audit.jsonl");
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let entry = json!({
        "at": castellan_time_now(),
        "actor": "operator",
        "action": action,
        "detail": detail,
    });
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    file.write_all(&line).await?;
    file.flush().await?;
    Ok(())
}

pub async fn read_tail(n: usize) -> Result<Vec<Value>> {
    let path = data_dir().join("audit.jsonl");
    let content = tokio::fs::read_to_string(&path).await.unwrap_or_default();
    let entries: Vec<Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let start = entries.len().saturating_sub(n);
    Ok(entries[start..].to_vec())
}
