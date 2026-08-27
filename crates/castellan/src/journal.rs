//! Append-only event journal. Every pi RPC event and castellan lifecycle
//! event for an agent lands here, seq-numbered — the local source of truth
//! that Phase 2 ships to suzerain and restores are built from.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, RwLock};

use anyhow::{Context, Result};
use suzerain_protocol::event::LogEvent;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Deep-redact every string inside a JSON value, using the given agent's own
/// registered secrets (see the "Secret redaction" section below).
fn redact_value(agent_id: Uuid, value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => serde_json::Value::String(redact(agent_id, &s)),
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .into_iter()
                .map(|v| redact_value(agent_id, v))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .map(|(k, v)| (k, redact_value(agent_id, v)))
                .collect(),
        ),
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
// Every plaintext secret delivered to this daemon is registered here, keyed
// by the agent it belongs to; journal appends replace occurrences with
// [REDACTED] so logs (which ship to the control plane) never carry
// credentials.
//
// Scoped per-agent rather than one flat list for two reasons: an agent's
// secrets are removed (`unregister_secrets`) when its bundle is dropped
// (`secrets::remove`, e.g. on destroy) instead of living in this list for
// the rest of the daemon's life, and every journal append only has to scan
// against ITS OWN agent's secrets rather than every secret ever seen by the
// daemon across every agent it has ever run.
static REDACT_VALUES: LazyLock<RwLock<HashMap<Uuid, Vec<String>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Register the full set of secret values for `agent_id`, replacing
/// whatever was registered before (so a re-pulled/rotated bundle doesn't
/// accumulate stale values alongside the fresh ones). Short values are
/// skipped: redacting them would mangle ordinary content.
pub fn register_secrets(agent_id: Uuid, values: impl IntoIterator<Item = String>) {
    let filtered: Vec<String> = values.into_iter().filter(|v| v.len() >= 12).collect();
    let mut map = REDACT_VALUES.write().unwrap();
    if filtered.is_empty() {
        map.remove(&agent_id);
    } else {
        map.insert(agent_id, filtered);
    }
}

/// Drop `agent_id`'s registered secrets. Call this whenever its bundle is
/// removed (destroy, or any other path that drops secrets for good) so the
/// list doesn't grow for the rest of the daemon's life.
pub fn unregister_secrets(agent_id: &Uuid) {
    REDACT_VALUES.write().unwrap().remove(agent_id);
}

pub fn redact(agent_id: Uuid, text: &str) -> String {
    let map = REDACT_VALUES.read().unwrap();
    let Some(secrets) = map.get(&agent_id) else {
        return text.to_string();
    };
    let mut out = text.to_string();
    for secret in secrets {
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
            payload: redact_value(self.agent_id, payload),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_scopes_to_the_registering_agent() {
        let agent_a = Uuid::new_v4();
        let agent_b = Uuid::new_v4();
        register_secrets(agent_a, vec!["super-secret-value".to_string()]);

        assert_eq!(
            redact(agent_a, "token=super-secret-value"),
            "token=[REDACTED]"
        );
        // A different agent's journal must not redact agent_a's secret —
        // and, more importantly for issue 5, must not even scan against it.
        assert_eq!(
            redact(agent_b, "token=super-secret-value"),
            "token=super-secret-value"
        );

        unregister_secrets(&agent_a);
    }

    #[test]
    fn unregister_secrets_removes_the_agent_from_the_redaction_list() {
        let agent = Uuid::new_v4();
        register_secrets(agent, vec!["another-super-secret".to_string()]);
        assert_eq!(redact(agent, "another-super-secret"), "[REDACTED]");

        unregister_secrets(&agent);
        // Once unregistered, the value passes through unredacted — proof
        // the entry was actually dropped from REDACT_VALUES, not just
        // shadowed, so the list doesn't grow across the agent's destroy.
        assert_eq!(
            redact(agent, "another-super-secret"),
            "another-super-secret"
        );
    }

    #[test]
    fn register_secrets_replaces_rather_than_accumulates() {
        let agent = Uuid::new_v4();
        register_secrets(agent, vec!["first-secret-value".to_string()]);
        register_secrets(agent, vec!["second-secret-value".to_string()]);

        // The stale first value must be gone, not merely joined by the new
        // one — otherwise a re-pulled/rotated bundle would leak the old
        // secret list forever.
        assert_eq!(redact(agent, "first-secret-value"), "first-secret-value");
        assert_eq!(redact(agent, "second-secret-value"), "[REDACTED]");

        unregister_secrets(&agent);
    }

    #[test]
    fn short_values_are_never_registered() {
        let agent = Uuid::new_v4();
        register_secrets(agent, vec!["short".to_string()]);
        // Too short to redact safely — must pass through unchanged.
        assert_eq!(redact(agent, "a short value"), "a short value");
    }
}
