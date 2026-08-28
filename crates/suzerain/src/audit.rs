//! Audit log: append-only JSONL record of control-plane actions.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::identity::data_dir;
use crate::store::castellan_time_now;

pub async fn record(action: &str, detail: Value) {
    crate::events::emit("audit", json!({"action": action}));
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
    // Shared with the retention sweep's prune_file (same path, same
    // registry) so an append can never land between the sweep's read and
    // its overwrite of the whole file.
    let lock = crate::file_locks::global().lock_for(&path).await;
    let _guard = lock.lock().await;
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

/// Test-only support for isolating `$SUZERAIN_HOME` (the process-global env
/// var `data_dir()` reads) across the several test modules in this crate
/// (audit.rs, control.rs, web.rs, web_session.rs) that need a real,
/// isolated audit log / data dir. `cargo test` runs the unit tests in this
/// crate's lib binary concurrently, so mutating a process env var without
/// serializing would let two tests race on which directory is "current"
/// mid-await; `lock_env_home` hands back a guard (held for the caller's
/// whole test) plus a fresh, private temp dir already installed as
/// `SUZERAIN_HOME`.
#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    pub(crate) async fn lock_env_home() -> (tokio::sync::MutexGuard<'static, ()>, PathBuf) {
        let guard = ENV_LOCK.lock().await;
        let dir = std::env::temp_dir().join(format!("suz-test-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create test SUZERAIN_HOME");
        // SAFETY: serialized by ENV_LOCK above — no other test in this
        // binary observes/mutates SUZERAIN_HOME while the guard is held.
        unsafe { std::env::set_var("SUZERAIN_HOME", &dir) };
        (guard, dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::lock_env_home;

    #[tokio::test]
    async fn record_appends_a_readable_entry_with_action_and_detail() {
        let (_guard, dir) = lock_env_home().await;

        record("agent_create", json!({"name": "foo"})).await;

        let tail = read_tail(10).await.unwrap();
        assert_eq!(tail.len(), 1, "expected exactly one audit entry: {tail:?}");
        let entry = &tail[0];
        assert_eq!(entry["action"], "agent_create");
        assert_eq!(entry["detail"]["name"], "foo");
        assert_eq!(entry["actor"], "operator");
        assert!(
            entry["at"].as_str().is_some_and(|s| !s.is_empty()),
            "entry missing a timestamp: {entry:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_tail_returns_only_the_last_n_entries_in_order() {
        let (_guard, dir) = lock_env_home().await;

        for i in 0..5 {
            record("marker", json!({"i": i})).await;
        }

        let tail = read_tail(2).await.unwrap();
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0]["detail"]["i"], 3);
        assert_eq!(tail[1]["detail"]["i"], 4);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_tail_on_missing_audit_log_is_empty_not_an_error() {
        let (_guard, dir) = lock_env_home().await;

        // No `record` call in this test: audit.jsonl never gets created.
        let tail = read_tail(50).await.unwrap();
        assert!(tail.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Security-relevant events are only useful as an audit trail if
    /// concurrent writers can't clobber or interleave each other's entries.
    /// `append` takes the same lock the retention sweep's rewrite uses, so
    /// this also stands in for that "never observe a torn write" guarantee.
    #[tokio::test]
    async fn concurrent_records_are_all_persisted_and_individually_valid() {
        let (_guard, dir) = lock_env_home().await;

        let n = 50;
        let mut handles = Vec::new();
        for i in 0..n {
            handles.push(tokio::spawn(async move {
                record("concurrent_marker", json!({"token": i})).await;
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let tail = read_tail(n + 10).await.unwrap();
        assert_eq!(
            tail.len(),
            n,
            "expected every concurrent append to land exactly once"
        );
        let mut tokens: Vec<i64> = tail
            .iter()
            .map(|e| e["detail"]["token"].as_i64().unwrap())
            .collect();
        tokens.sort_unstable();
        let expected: Vec<i64> = (0..n as i64).collect();
        assert_eq!(tokens, expected, "no entry should be lost or duplicated");

        std::fs::remove_dir_all(&dir).ok();
    }
}
