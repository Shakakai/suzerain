//! `ChatStore` — pluggable chat/transcript event storage
//! (docs/UNIFIED-AGENT-API-DESIGN.md §4.6).
//!
//! Unlike [`crate::registry::Registry`] and [`crate::bundle::SnapshotStore`],
//! this trait is **new construction, not a wrap** — there was no existing
//! `ChatStore`-shaped module before this. Event-log file paths
//! (`<data>/logs/<agent_id>.jsonl`) were constructed inline and redundantly
//! at several call sites (`api.rs`, `control.rs`, `web_session.rs`,
//! `web.rs`), there was no `history_since`-equivalent anywhere, and
//! `log_index`'s `acked_through` bookkeeping already lived in `Registry`
//! rather than alongside the events it tracks.
//!
//! Per the decision to back this with SQLite: events live in a `chat_events`
//! table (`crate::store::Store`, migration v6) — one row per event, indexed
//! by `(agent_id, seq)` — sharing the same connection pool/backend as
//! `Registry` (one database, not two storage systems). `Store` implements
//! this trait the same way it implements `Registry`: by delegating to
//! identically-named inherent methods defined in `store.rs` (where the
//! private `backend`/`sql()` internals live).
//!
//! **Migration status**: this lands the trait, the table, and the write
//! path (`control.rs`'s `handle_logs` now writes every shipped event here
//! *in addition to* the existing JSONL file, not instead of it). The read
//! call sites (`api.rs`/`web_session.rs`/`web.rs`'s `agent_ask`/`attach`/
//! `logs`/session-history endpoints) still read the JSONL file today — each
//! has different replay/tailing semantics (session-boundary markers,
//! streaming attach relay, etc.) and deserves its own careful, individually
//! tested cutover rather than a single blind rewrite of nine call sites.
//! The JSONL file stays the source of truth for reads until that follow-up
//! work migrates them one at a time and (only once every reader is off the
//! file) the dual-write can be dropped.

use anyhow::Result;
use async_trait::async_trait;
use suzerain_protocol::event::LogEvent;
use uuid::Uuid;

use crate::store::Store;

/// Chat/transcript event storage: append-only per agent, ordered by `seq`.
#[async_trait]
pub trait ChatStore: Send + Sync {
    async fn append(&self, agent_id: &Uuid, event: &LogEvent) -> Result<()>;
    async fn tail(&self, agent_id: &Uuid, n: usize) -> Result<Vec<LogEvent>>;
    async fn history_since(&self, agent_id: &Uuid, seq: u64) -> Result<Vec<LogEvent>>;
}

#[async_trait]
impl ChatStore for Store {
    async fn append(&self, agent_id: &Uuid, event: &LogEvent) -> Result<()> {
        self.append(agent_id, event).await
    }
    async fn tail(&self, agent_id: &Uuid, n: usize) -> Result<Vec<LogEvent>> {
        self.tail(agent_id, n).await
    }
    async fn history_since(&self, agent_id: &Uuid, seq: u64) -> Result<Vec<LogEvent>> {
        self.history_since(agent_id, seq).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_store() -> Store {
        // A named, shared-cache in-memory sqlite DB, unique per test, via
        // `open_with_url` — avoids mutating the process-wide
        // `SUZERAIN_DATABASE_URL` env var, which `cargo test`'s parallel
        // execution would otherwise race on.
        let name = format!("chat-store-test-{}", uuid::Uuid::new_v4().simple());
        let url = format!("sqlite://file:{name}?mode=memory&cache=shared");
        Store::open_with_url(&url)
            .await
            .expect("open in-memory store")
    }

    fn event(agent_id: Uuid, seq: u64, kind: &str) -> LogEvent {
        LogEvent {
            agent_id,
            seq,
            at: "2026-08-24T00:00:00Z".into(),
            kind: kind.into(),
            payload: serde_json::json!({"n": seq}),
        }
    }

    #[tokio::test]
    async fn append_tail_and_history_since_round_trip() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        for seq in 1..=5u64 {
            ChatStore::append(&store, &agent_id, &event(agent_id, seq, "message_update"))
                .await
                .unwrap();
        }

        let tail = ChatStore::tail(&store, &agent_id, 2).await.unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);

        let since = ChatStore::history_since(&store, &agent_id, 3)
            .await
            .unwrap();
        assert_eq!(since.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[tokio::test]
    async fn append_is_idempotent_on_agent_and_seq() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        ChatStore::append(&store, &agent_id, &event(agent_id, 1, "a"))
            .await
            .unwrap();
        // Re-delivery of the same seq (at-least-once shipping) must not
        // duplicate or error.
        ChatStore::append(&store, &agent_id, &event(agent_id, 1, "a"))
            .await
            .unwrap();
        let all = ChatStore::history_since(&store, &agent_id, 0)
            .await
            .unwrap();
        assert_eq!(all.len(), 1);
    }
}
