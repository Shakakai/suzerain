//! Append-only event journal. Every pi RPC event and castellan lifecycle
//! event for an agent lands here, seq-numbered — the local source of truth
//! that Phase 2 ships to suzerain and restores are built from.

use std::path::Path;
use std::sync::RwLock;

use anyhow::{Context, Result};
use suzerain_protocol::event::LogEvent;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Deep-redact every string inside a JSON value.
fn redact_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact(&s)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(redact_value).collect())
        }
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(map.into_iter().map(|(k, v)| (k, redact_value(v))).collect())
        }
        other => other,
    }
}

pub struct Journal {
    agent_id: Uuid,
    seq: Mutex<u64>,
    file: Mutex<tokio::fs::File>,
}

pub fn rfc3339_now() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

// ── Secret redaction ─────────────────────────────────────────────────────
// Every plaintext secret delivered to this daemon is registered here; journal
// appends replace occurrences with [REDACTED] so logs (which ship to the
// control plane) never carry credentials.

static REDACT_VALUES: RwLock<Vec<String>> = RwLock::new(Vec::new());

pub fn register_secret(value: &str) {
    // Skip short values: replacing them would mangle ordinary content.
    if value.len() >= 12 {
        REDACT_VALUES.write().unwrap().push(value.to_string());
    }
}

pub fn redact(text: &str) -> String {
    let mut out = text.to_string();
    for secret in REDACT_VALUES.read().unwrap().iter() {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), "[REDACTED]");
        }
    }
    out
}

impl Journal {
    /// Open (or create) the journal for an agent, resuming the seq counter
    /// from the last line if the file already exists — floored at the shipped
    /// watermark so pruning can never rewind the sequence.
    pub async fn open(agent_dir: &Path, agent_id: Uuid) -> Result<Self> {
        let path = agent_dir.join("journal.jsonl");
        let mut seq = 0u64;
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            for line in content.lines() {
                if let Ok(ev) = serde_json::from_str::<LogEvent>(line) {
                    seq = seq.max(ev.seq);
                }
            }
        }
        if let Ok(wm) = tokio::fs::read_to_string(agent_dir.join(".shipped")).await {
            if let Ok(shipped) = wm.trim().parse::<u64>() {
                seq = seq.max(shipped);
            }
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("opening journal {}", path.display()))?;
        Ok(Self {
            agent_id,
            seq: Mutex::new(seq),
            file: Mutex::new(file),
        })
    }

    pub async fn append(&self, kind: &str, payload: serde_json::Value) -> Result<LogEvent> {
        let mut seq = self.seq.lock().await;
        *seq += 1;
        let ev = LogEvent {
            agent_id: self.agent_id,
            seq: *seq,
            at: rfc3339_now(),
            kind: kind.to_string(),
            payload: redact_value(payload),
        };
        let mut line = serde_json::to_vec(&ev)?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(ev)
    }

    pub async fn read_all(agent_dir: &Path) -> Result<Vec<LogEvent>> {
        let path = agent_dir.join("journal.jsonl");
        let content = tokio::fs::read_to_string(&path).await?;
        Ok(content
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }
}
