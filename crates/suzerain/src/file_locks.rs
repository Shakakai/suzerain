//! Shared per-path async lock registry.
//!
//! JSONL files under the data dir (`<data>/logs/<agent_id>.jsonl`,
//! `<data>/audit.jsonl`) are appended to by daemon-log/audit writers and
//! periodically read-modify-written whole by the retention sweep
//! (`retention.rs::prune_file`). With no shared lock, an append landing
//! between the sweep's read and its write is silently dropped when the
//! sweep overwrites the whole file. `FileLocks` gives every writer/pruner
//! of a given path the same `tokio::sync::Mutex` to hold across their
//! critical section, so the two never interleave.
//!
//! Mirrors the `agent_locks: Arc<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>>`
//! pattern already used in `control.rs`, keyed by `PathBuf` instead.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct FileLocks(Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>);

impl FileLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get (or create) the lock for `path`. Callers should `.lock().await`
    /// the returned `Arc<Mutex<()>>` and hold the guard across their whole
    /// read-modify-write / append critical section.
    pub async fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        self.0
            .lock()
            .await
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

/// Process-wide registry, shared by the retention sweep and every JSONL
/// appender (`control.rs::handle_logs`, `audit.rs::append`) so they all
/// contend on the *same* per-path locks instead of three independent maps.
/// One control-plane process ever touches a given data dir's JSONL files,
/// so a single global instance (mirroring the `GOSSIP_TX` static already
/// used in `control.rs`) is simpler than plumbing a `FileLocks` field
/// through every call site that can append or prune.
static GLOBAL: std::sync::OnceLock<FileLocks> = std::sync::OnceLock::new();

pub fn global() -> &'static FileLocks {
    GLOBAL.get_or_init(FileLocks::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A "slow prune" (read, sleep, write pruned content) racing an append
    /// through the same `FileLocks` must never lose the append: without
    /// per-path locking the prune's read happens before the append, and its
    /// write (of the stale pre-append content) then clobbers it.
    #[tokio::test]
    async fn append_survives_concurrent_prune() {
        let dir = std::env::temp_dir().join(format!("suz-filelocks-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        tokio::fs::write(&path, "{\"at\":\"old\"}\n").await.unwrap();

        let locks = FileLocks::new();

        let prune_path = path.clone();
        let prune_locks = locks.clone();
        let prune = tokio::spawn(async move {
            let lock = prune_locks.lock_for(&prune_path).await;
            let _guard = lock.lock().await;
            let content = tokio::fs::read_to_string(&prune_path).await.unwrap();
            // Simulate a slow prune: read now, write later.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            // Pretend everything old was pruned away.
            let _ = content;
            tokio::fs::write(&prune_path, "").await.unwrap();
        });

        // Give the prune task a head start so it acquires the lock first.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let append_path = path.clone();
        let append_locks = locks.clone();
        let append = tokio::spawn(async move {
            let lock = append_locks.lock_for(&append_path).await;
            let _guard = lock.lock().await;
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&append_path)
                .await
                .unwrap();
            file.write_all(b"{\"at\":\"new\"}\n").await.unwrap();
            file.flush().await.unwrap();
        });

        prune.await.unwrap();
        append.await.unwrap();

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            final_content.contains("\"new\""),
            "append was lost, final content: {final_content:?}"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
