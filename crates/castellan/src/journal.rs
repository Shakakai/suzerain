//! Append-only event journal. Every pi RPC event and castellan lifecycle
//! event for an agent lands here, seq-numbered — the local source of truth
//! that Phase 2 ships to suzerain and restores are built from.

use std::path::Path;

use anyhow::{Context, Result};
use suzerain_protocol::event::LogEvent;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

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

impl Journal {
    /// Open (or create) the journal for an agent, resuming the seq counter
    /// from the last line if the file already exists.
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
            payload,
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
