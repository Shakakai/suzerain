//! Pluggable control-plane store: daemon registry (allowlist), agent
//! registry, and the event-log index. Log payloads live as append-only JSONL
//! files under the data dir; the DB indexes them.
//!
//! Backends (Q3/P5): `sqlite://…` (**default, zero-config**) or
//! `postgres://…` via `SUZERAIN_DATABASE_URL`. SQL is kept portable across
//! both (TEXT/INTEGER columns only, `?` placeholders rewritten to `$n` for
//! postgres, upserts via `ON CONFLICT … DO UPDATE … = excluded.…`).

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{PgPool, Row};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::{AgentState, DaemonInfo};
use uuid::Uuid;

use crate::identity::data_dir;

#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonRow {
    pub endpoint_id: String,
    pub approved: bool,
    pub online: bool,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    /// Daemon-reported labels (JSON object).
    pub labels: String,
    /// Operator-side label overrides (JSON object; overrides win).
    pub label_overrides: String,
    pub max_agents: u32,
    pub last_seen: String,
    /// Static capacity + latest dynamic usage (JSON).
    pub capacity_json: String,
    pub usage_json: String,
}

impl DaemonRow {
    /// Effective labels: daemon-reported ∪ operator overrides (overrides win).
    pub fn effective_labels(&self) -> std::collections::BTreeMap<String, String> {
        let mut out: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&self.labels).unwrap_or_default();
        let overrides: std::collections::BTreeMap<String, String> =
            serde_json::from_str(&self.label_overrides).unwrap_or_default();
        out.extend(overrides);
        out
    }

    pub fn capacity(&self) -> suzerain_protocol::NodeCapacity {
        serde_json::from_str(&self.capacity_json).unwrap_or_default()
    }

    pub fn usage(&self) -> suzerain_protocol::NodeUsage {
        serde_json::from_str(&self.usage_json).unwrap_or_default()
    }
}

/// Error from [`resolve_daemon`]: either nothing matched, or more than one
/// daemon matched an ambiguous (non-exact) `endpoint_id` prefix/hostname.
#[derive(Debug, Clone)]
pub enum DaemonLookupError {
    NotFound,
    /// Full `endpoint_id`s of every daemon that matched.
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for DaemonLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DaemonLookupError::NotFound => write!(f, "no daemon found"),
            DaemonLookupError::Ambiguous(matches) => {
                write!(f, "ambiguous daemon id, matches: {}", matches.join(", "))
            }
        }
    }
}

impl std::error::Error for DaemonLookupError {}

/// Resolve a daemon by exact `endpoint_id`, then (if no exact match) by
/// unique `endpoint_id` prefix or exact `hostname` match. Unlike a plain
/// `Iterator::find`, an ambiguous (non-exact) prefix/hostname match across
/// more than one daemon is an error rather than an arbitrary pick — the
/// `endpoint_id` is an identity, so silently picking "whichever comes
/// first" is a correctness/security problem.
pub fn resolve_daemon<'a>(
    daemons: &'a [DaemonRow],
    id: &str,
) -> Result<&'a DaemonRow, DaemonLookupError> {
    if let Some(d) = daemons.iter().find(|d| d.endpoint_id == id) {
        return Ok(d);
    }
    let matches: Vec<&DaemonRow> = daemons
        .iter()
        .filter(|d| d.endpoint_id.starts_with(id) || d.hostname == id)
        .collect();
    match matches.as_slice() {
        [] => Err(DaemonLookupError::NotFound),
        [only] => Ok(only),
        many => Err(DaemonLookupError::Ambiguous(
            many.iter().map(|d| d.endpoint_id.clone()).collect(),
        )),
    }
}

#[cfg(test)]
mod daemon_lookup_tests {
    use super::*;

    fn daemon(endpoint_id: &str, hostname: &str) -> DaemonRow {
        DaemonRow {
            endpoint_id: endpoint_id.to_string(),
            approved: true,
            online: true,
            hostname: hostname.to_string(),
            os: "linux".into(),
            arch: "x86_64".into(),
            labels: "{}".into(),
            label_overrides: "{}".into(),
            max_agents: 10,
            last_seen: "2026-08-27T00:00:00Z".into(),
            capacity_json: "{}".into(),
            usage_json: "{}".into(),
        }
    }

    #[test]
    fn ambiguous_prefix_is_an_error() {
        let daemons = vec![daemon("abc123def", "host-a"), daemon("abc456ghi", "host-b")];
        match resolve_daemon(&daemons, "abc") {
            Err(DaemonLookupError::Ambiguous(mut matches)) => {
                matches.sort();
                assert_eq!(
                    matches,
                    vec!["abc123def".to_string(), "abc456ghi".to_string()]
                );
            }
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn exact_match_wins_even_if_a_prefix_of_nothing_else() {
        let daemons = vec![daemon("abc123def", "host-a"), daemon("abc456ghi", "host-b")];
        let d = resolve_daemon(&daemons, "abc123def").unwrap();
        assert_eq!(d.endpoint_id, "abc123def");
    }

    #[test]
    fn unique_prefix_matches() {
        let daemons = vec![daemon("abc123def", "host-a"), daemon("abc456ghi", "host-b")];
        let d = resolve_daemon(&daemons, "abc1").unwrap();
        assert_eq!(d.endpoint_id, "abc123def");
    }

    #[test]
    fn hostname_matches() {
        let daemons = vec![daemon("abc123def", "host-a"), daemon("abc456ghi", "host-b")];
        let d = resolve_daemon(&daemons, "host-b").unwrap();
        assert_eq!(d.endpoint_id, "abc456ghi");
    }

    #[test]
    fn no_match_is_not_found() {
        let daemons = vec![daemon("abc123def", "host-a")];
        assert!(matches!(
            resolve_daemon(&daemons, "zzz"),
            Err(DaemonLookupError::NotFound)
        ));
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentRow {
    pub id: Uuid,
    pub name: String,
    pub daemon_endpoint_id: String,
    pub manifest: AgentManifest,
    pub state: AgentState,
    pub created_at: String,
    pub session_file: Option<String>,
    /// Daemon-reported idle seconds at `activity_reported_at` (skew-immune
    /// extrapolation: idle_now ≈ idle_secs + age of the report).
    #[serde(default)]
    pub idle_secs: Option<i64>,
    /// Daemon-reported ground truth: a turn is in flight.
    #[serde(default)]
    pub busy: Option<bool>,
    #[serde(default)]
    pub activity_reported_at: Option<String>,
    /// Set when an unrecoverable wake failure needs human intervention.
    #[serde(default)]
    pub needs_attention: bool,
    /// Runtime auto-suspend override ("never" / duration / NULL = inherit).
    #[serde(default)]
    pub auto_suspend_override: Option<String>,
    /// Last time the agent (re)entered Active — preemption grace window.
    #[serde(default)]
    pub woke_at: Option<String>,
}

/// A pi session era for an agent. Sessions rotate on every suspend: the
/// row opens when the daemon reports a new session file and closes when
/// the next one opens. Conversation logs are segmented by these rows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentSessionRow {
    pub id: i64,
    pub agent_id: Uuid,
    /// Guest path of the pi session file (e.g. /agent/sessions/<ts>_<uuid>.jsonl).
    pub session_file: String,
    pub started_at: String,
    /// Set when the session was superseded by the next rotation.
    pub ended_at: Option<String>,
}

/// A chat message held while its agent wakes (durable Activator pattern).
#[derive(Debug, Clone, serde::Serialize)]
pub struct PendingMessage {
    pub id: i64,
    pub agent_id: Uuid,
    pub body: String,
    /// queued | waking | delivered | failed
    pub status: String,
    pub created_at: String,
    pub delivered_at: Option<String>,
    pub last_error: Option<String>,
}

type DaemonTuple = (
    String,
    i64,
    i64,
    String,
    String,
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
);
type AgentTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
    Option<String>,
    Option<String>,
);

enum Backend {
    Sqlite(SqlitePool),
    Pg(PgPool),
}

#[derive(Clone)]
pub struct Store {
    backend: std::sync::Arc<Backend>,
}

const MIGRATIONS: &str = "
CREATE TABLE IF NOT EXISTS daemons (
    endpoint_id TEXT PRIMARY KEY,
    approved BIGINT NOT NULL DEFAULT 0,
    online BIGINT NOT NULL DEFAULT 0,
    hostname TEXT NOT NULL DEFAULT '',
    os TEXT NOT NULL DEFAULT '',
    arch TEXT NOT NULL DEFAULT '',
    labels TEXT NOT NULL DEFAULT '{}',
    max_agents BIGINT NOT NULL DEFAULT 4,
    last_seen TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    daemon_endpoint_id TEXT NOT NULL,
    manifest TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    session_file TEXT
);
CREATE TABLE IF NOT EXISTS log_index (
    agent_id TEXT PRIMARY KEY,
    acked_through BIGINT NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL DEFAULT ''
);";

impl Store {
    pub async fn open() -> Result<Self> {
        let url = std::env::var("SUZERAIN_DATABASE_URL").unwrap_or_else(|_| {
            let path: PathBuf = data_dir().join("suzerain.db");
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            format!("sqlite://{}", path.display())
        });
        Self::open_with_url(&url).await
    }

    /// Same as [`Store::open`], but with the connection URL passed in
    /// explicitly instead of read from `SUZERAIN_DATABASE_URL` — lets tests
    /// (e.g. an in-memory sqlite DB per test) avoid mutating a process-wide
    /// env var, which is racy under `cargo test`'s parallel test execution.
    pub async fn open_with_url(url: &str) -> Result<Self> {
        // Additive migrations (v2 columns). sqlite has no IF NOT EXISTS for
        // ADD COLUMN; postgres does. Duplicates are tolerated below.
        let backend = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let pool = sqlx::postgres::PgPoolOptions::new().connect(url).await?;
            for stmt in MIGRATIONS.split(';').filter(|s| !s.trim().is_empty()) {
                sqlx::query(stmt).execute(&pool).await?;
            }
            Backend::Pg(pool)
        } else {
            let path = url.trim_start_matches("sqlite://");
            let options = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
                .create_if_missing(true)
                .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
            let pool = SqlitePoolOptions::new().connect_with(options).await?;
            for stmt in MIGRATIONS.split(';').filter(|s| !s.trim().is_empty()) {
                sqlx::query(stmt).execute(&pool).await?;
            }
            Backend::Sqlite(pool)
        };
        let store = Self {
            backend: std::sync::Arc::new(backend),
        };
        store.migrate_v2().await?;
        store.migrate_v3().await?;
        store.migrate_v4().await?;
        store.migrate_v5().await?;
        store.migrate_v6().await?;
        store.migrate_v7().await?;
        tracing::info!(
            backend = if matches!(store.backend.as_ref(), Backend::Pg(_)) {
                "postgres"
            } else {
                "sqlite"
            },
            "store opened"
        );
        Ok(store)
    }

    /// Additive v2 columns. Duplicates tolerated (sqlite errors on re-add).
    async fn migrate_v2(&self) -> Result<()> {
        for stmt in [
            "ALTER TABLE daemons ADD COLUMN label_overrides TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE daemons ADD COLUMN capacity_json TEXT NOT NULL DEFAULT '{}'",
            "ALTER TABLE daemons ADD COLUMN usage_json TEXT NOT NULL DEFAULT '{}'",
        ] {
            let sql = if matches!(self.backend.as_ref(), Backend::Pg(_)) {
                stmt.replace("ADD COLUMN", "ADD COLUMN IF NOT EXISTS")
            } else {
                stmt.to_string()
            };
            match self.backend.as_ref() {
                Backend::Sqlite(p) => {
                    if let Err(e) = sqlx::query(&sql).execute(p).await {
                        if !e.to_string().contains("duplicate column") {
                            return Err(e.into());
                        }
                    }
                }
                Backend::Pg(p) => {
                    sqlx::query(&sql).execute(p).await?;
                }
            }
        }
        Ok(())
    }

    /// v3: pending daemon enrollments (M4).
    async fn migrate_v3(&self) -> Result<()> {
        let sql = "CREATE TABLE IF NOT EXISTS pending_daemons (
            endpoint_id TEXT PRIMARY KEY,
            hostname TEXT NOT NULL DEFAULT '',
            os TEXT NOT NULL DEFAULT '',
            arch TEXT NOT NULL DEFAULT '',
            capacity_json TEXT NOT NULL DEFAULT '{}',
            first_seen TEXT NOT NULL DEFAULT '',
            last_seen TEXT NOT NULL DEFAULT ''
        )";
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(sql).execute(p).await?;
            }
        }
        Ok(())
    }

    /// v4: activity tracking + durable message queue (auto-suspend &
    /// transparent wake).
    async fn migrate_v4(&self) -> Result<()> {
        for stmt in [
            "ALTER TABLE agents ADD COLUMN idle_secs BIGINT",
            "ALTER TABLE agents ADD COLUMN busy BIGINT",
            "ALTER TABLE agents ADD COLUMN activity_reported_at TEXT",
            "ALTER TABLE agents ADD COLUMN needs_attention BIGINT NOT NULL DEFAULT 0",
            "ALTER TABLE agents ADD COLUMN auto_suspend_override TEXT",
            "ALTER TABLE agents ADD COLUMN woke_at TEXT",
        ] {
            let sql = if matches!(self.backend.as_ref(), Backend::Pg(_)) {
                stmt.replace("ADD COLUMN", "ADD COLUMN IF NOT EXISTS")
            } else {
                stmt.to_string()
            };
            match self.backend.as_ref() {
                Backend::Sqlite(p) => {
                    if let Err(e) = sqlx::query(&sql).execute(p).await {
                        if !e.to_string().contains("duplicate column") {
                            return Err(e.into());
                        }
                    }
                }
                Backend::Pg(p) => {
                    sqlx::query(&sql).execute(p).await?;
                }
            }
        }
        let sql = match self.backend.as_ref() {
            Backend::Sqlite(_) => {
                "CREATE TABLE IF NOT EXISTS pending_messages (
                    id INTEGER PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'queued',
                    created_at TEXT NOT NULL,
                    delivered_at TEXT,
                    last_error TEXT
                )"
            }
            Backend::Pg(_) => {
                "CREATE TABLE IF NOT EXISTS pending_messages (
                    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    body TEXT NOT NULL,
                    status TEXT NOT NULL DEFAULT 'queued',
                    created_at TEXT NOT NULL,
                    delivered_at TEXT,
                    last_error TEXT
                )"
            }
        };
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(sql).execute(p).await?;
            }
        }
        Ok(())
    }

    /// v5: pi session eras (sessions rotate on every suspend; conversation
    /// logs are segmented by these rows).
    async fn migrate_v5(&self) -> Result<()> {
        let sql = match self.backend.as_ref() {
            Backend::Sqlite(_) => {
                "CREATE TABLE IF NOT EXISTS agent_sessions (
                    id INTEGER PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    session_file TEXT NOT NULL DEFAULT '',
                    started_at TEXT NOT NULL,
                    ended_at TEXT
                )"
            }
            Backend::Pg(_) => {
                "CREATE TABLE IF NOT EXISTS agent_sessions (
                    id BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
                    agent_id TEXT NOT NULL,
                    session_file TEXT NOT NULL DEFAULT '',
                    started_at TEXT NOT NULL,
                    ended_at TEXT
                )"
            }
        };
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(sql).execute(p).await?;
            }
        }
        Ok(())
    }

    /// v6: chat/transcript event log (docs/UNIFIED-AGENT-API-DESIGN.md §4.6).
    /// One row per `LogEvent`; `(agent_id, seq)` is the primary key, giving
    /// natural dedup and an efficient `history_since` query. `payload` is
    /// TEXT (a serialized JSON string), not a native JSON/JSONB column —
    /// matching this module's existing TEXT/INTEGER-only portability
    /// convention (see the module doc comment) rather than introducing a
    /// sqlite-vs-postgres type divergence for one table.
    async fn migrate_v6(&self) -> Result<()> {
        let sql = "CREATE TABLE IF NOT EXISTS chat_events (
                agent_id TEXT NOT NULL,
                seq BIGINT NOT NULL,
                at TEXT NOT NULL,
                kind TEXT NOT NULL,
                payload TEXT NOT NULL,
                PRIMARY KEY (agent_id, seq)
            )";
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(sql).execute(p).await?;
            }
        }
        Ok(())
    }

    /// v7: enforce at most one open (`ended_at IS NULL`) session row per
    /// agent at the DB level — a backstop against the check-then-act race
    /// in [`Store::start_agent_session`] / [`Store::ensure_open_session`]
    /// producing duplicate open rows and corrupting log segmentation.
    async fn migrate_v7(&self) -> Result<()> {
        let sql = "CREATE UNIQUE INDEX IF NOT EXISTS agent_sessions_open_idx
            ON agent_sessions (agent_id) WHERE ended_at IS NULL";
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(sql).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(sql).execute(p).await?;
            }
        }
        Ok(())
    }

    /// Rewrite `?` placeholders to `$1..$n` for postgres.
    fn sql(&self, sql: &str) -> String {
        if matches!(self.backend.as_ref(), Backend::Pg(_)) {
            let mut out = String::with_capacity(sql.len() + 8);
            let mut n = 0;
            for ch in sql.chars() {
                if ch == '?' {
                    n += 1;
                    out.push_str(&format!("${n}"));
                } else {
                    out.push(ch);
                }
            }
            out
        } else {
            sql.to_string()
        }
    }

    // ── daemons ─────────────────────────────────────────────────────────

    pub async fn approve_daemon(&self, endpoint_id: &str) -> Result<()> {
        self.delete_pending_daemon(endpoint_id).await.ok();
        let sql = self.sql(
            "INSERT INTO daemons (endpoint_id, approved) VALUES (?, 1)
             ON CONFLICT(endpoint_id) DO UPDATE SET approved = 1",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(endpoint_id).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(endpoint_id).execute(p).await?;
            }
        }
        Ok(())
    }

    pub async fn daemon_approved(&self, endpoint_id: &str) -> Result<bool> {
        let sql = self.sql("SELECT approved FROM daemons WHERE endpoint_id = ?");
        let approved: Option<i64> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query(&sql)
                .bind(endpoint_id)
                .fetch_optional(p)
                .await?
                .map(|r| r.get::<i64, _>(0)),
            Backend::Pg(p) => sqlx::query(&sql)
                .bind(endpoint_id)
                .fetch_optional(p)
                .await?
                .map(|r| r.get::<i64, _>(0)),
        };
        Ok(approved == Some(1))
    }

    pub async fn upsert_daemon(&self, info: &DaemonInfo, online: bool) -> Result<()> {
        let sql = self.sql(
            "INSERT INTO daemons (endpoint_id, hostname, os, arch, labels, max_agents, online, last_seen, capacity_json, usage_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET
                hostname = excluded.hostname, os = excluded.os, arch = excluded.arch,
                labels = excluded.labels, max_agents = excluded.max_agents,
                online = excluded.online, last_seen = excluded.last_seen,
                capacity_json = excluded.capacity_json, usage_json = excluded.usage_json",
        );
        let labels = serde_json::to_string(&info.labels)?;
        let capacity = serde_json::to_string(&info.capacity)?;
        let usage = serde_json::to_string(&info.usage)?;
        let now = castellan_time_now();
        let max = info.max_agents as i64;
        let online = online as i64;
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(&info.endpoint_id)
                    .bind(&info.hostname)
                    .bind(&info.os)
                    .bind(&info.arch)
                    .bind(&labels)
                    .bind(max)
                    .bind(online)
                    .bind(&now)
                    .bind(&capacity)
                    .bind(&usage)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(&info.endpoint_id)
                    .bind(&info.hostname)
                    .bind(&info.os)
                    .bind(&info.arch)
                    .bind(&labels)
                    .bind(max)
                    .bind(online)
                    .bind(&now)
                    .bind(&capacity)
                    .bind(&usage)
                    .execute(p)
                    .await?;
            }
        }
        crate::events::emit(
            "daemon",
            serde_json::json!({"endpoint_id": info.endpoint_id.as_str(), "online": online != 0}),
        );
        Ok(())
    }

    /// Refresh a daemon's dynamic usage (heartbeat acks carry snapshots).
    pub async fn set_daemon_usage(&self, endpoint_id: &str, usage_json: &str) -> Result<()> {
        let sql =
            self.sql("UPDATE daemons SET usage_json = ?, last_seen = ? WHERE endpoint_id = ?");
        let now = castellan_time_now();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(usage_json)
                    .bind(&now)
                    .bind(endpoint_id)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(usage_json)
                    .bind(&now)
                    .bind(endpoint_id)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    /// Operator-side label overrides (merged over daemon-reported labels).
    pub async fn set_label_overrides(&self, endpoint_id: &str, overrides_json: &str) -> Result<()> {
        let sql = self.sql("UPDATE daemons SET label_overrides = ? WHERE endpoint_id = ?");
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(overrides_json)
                    .bind(endpoint_id)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(overrides_json)
                    .bind(endpoint_id)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn set_daemon_online(&self, endpoint_id: &str, online: bool) -> Result<()> {
        let sql = self.sql("UPDATE daemons SET online = ?, last_seen = ? WHERE endpoint_id = ?");
        let now = castellan_time_now();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(online as i64)
                    .bind(&now)
                    .bind(endpoint_id)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(online as i64)
                    .bind(&now)
                    .bind(endpoint_id)
                    .execute(p)
                    .await?;
            }
        }
        crate::events::emit(
            "daemon",
            serde_json::json!({"endpoint_id": endpoint_id, "online": online}),
        );
        Ok(())
    }

    /// Control-plane boot: no sessions exist yet, so any online flag left
    /// over from a previous run is stale. Daemons flip back online as they
    /// re-register.
    pub async fn set_all_daemons_offline(&self) -> Result<()> {
        let sql = self.sql("UPDATE daemons SET online = 0");
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).execute(p).await?;
            }
        }
        Ok(())
    }

    pub async fn list_daemons(&self) -> Result<Vec<DaemonRow>> {
        let sql = self.sql(
            "SELECT endpoint_id, approved, online, hostname, os, arch, labels, max_agents, last_seen,
                    label_overrides, capacity_json, usage_json
             FROM daemons ORDER BY endpoint_id",
        );
        let rows_to_daemons = |rows: Vec<DaemonTuple>| {
            rows.into_iter()
                .map(|r| DaemonRow {
                    endpoint_id: r.0,
                    approved: r.1 == 1,
                    online: r.2 == 1,
                    hostname: r.3,
                    os: r.4,
                    arch: r.5,
                    labels: r.6,
                    max_agents: r.7 as u32,
                    last_seen: r.8,
                    label_overrides: r.9,
                    capacity_json: r.10,
                    usage_json: r.11,
                })
                .collect()
        };
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                let rows: Vec<DaemonTuple> = sqlx::query_as(&sql).fetch_all(p).await?;
                Ok(rows_to_daemons(rows))
            }
            Backend::Pg(p) => {
                let rows: Vec<DaemonTuple> = sqlx::query_as(&sql).fetch_all(p).await?;
                Ok(rows_to_daemons(rows))
            }
        }
    }

    // ── pending enrollments (M4) ─────────────────────────────────────────

    pub async fn upsert_pending_daemon(&self, info: &DaemonInfo) -> Result<()> {
        let sql = self.sql(
            "INSERT INTO pending_daemons (endpoint_id, hostname, os, arch, capacity_json, first_seen, last_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET
                hostname = excluded.hostname, os = excluded.os, arch = excluded.arch,
                capacity_json = excluded.capacity_json, last_seen = excluded.last_seen",
        );
        let now = castellan_time_now();
        let capacity = serde_json::to_string(&info.capacity)?;
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(&info.endpoint_id)
                    .bind(&info.hostname)
                    .bind(&info.os)
                    .bind(&info.arch)
                    .bind(&capacity)
                    .bind(&now)
                    .bind(&now)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(&info.endpoint_id)
                    .bind(&info.hostname)
                    .bind(&info.os)
                    .bind(&info.arch)
                    .bind(&capacity)
                    .bind(&now)
                    .bind(&now)
                    .execute(p)
                    .await?;
            }
        }
        crate::events::emit(
            "pending_daemon",
            serde_json::json!({"endpoint_id": info.endpoint_id.as_str(), "hostname": info.hostname.as_str()}),
        );
        Ok(())
    }

    pub async fn delete_pending_daemon(&self, endpoint_id: &str) -> Result<()> {
        let sql = self.sql("DELETE FROM pending_daemons WHERE endpoint_id = ?");
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(endpoint_id).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(endpoint_id).execute(p).await?;
            }
        }
        Ok(())
    }

    pub async fn list_pending_daemons(&self) -> Result<Vec<serde_json::Value>> {
        let sql = self.sql(
            "SELECT endpoint_id, hostname, os, arch, capacity_json, first_seen, last_seen
             FROM pending_daemons ORDER BY first_seen",
        );
        type Row7 = (String, String, String, String, String, String, String);
        let rows: Vec<Row7> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).fetch_all(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).fetch_all(p).await?,
        };
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "endpoint_id": r.0,
                    "hostname": r.1,
                    "os": r.2,
                    "arch": r.3,
                    "capacity": serde_json::from_str::<serde_json::Value>(&r.4).unwrap_or_default(),
                    "first_seen": r.5,
                    "last_seen": r.6,
                })
            })
            .collect())
    }

    // ── agents ──────────────────────────────────────────────────────────

    pub async fn create_agent(&self, row: &AgentRow) -> Result<()> {
        let sql = self.sql(
            "INSERT INTO agents (id, name, daemon_endpoint_id, manifest, state, created_at, session_file)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        );
        let id = row.id.to_string();
        let manifest = serde_json::to_string(&row.manifest)?;
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(&id)
                    .bind(&row.name)
                    .bind(&row.daemon_endpoint_id)
                    .bind(&manifest)
                    .bind(state_str(row.state))
                    .bind(&row.created_at)
                    .bind(&row.session_file)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(&id)
                    .bind(&row.name)
                    .bind(&row.daemon_endpoint_id)
                    .bind(&manifest)
                    .bind(state_str(row.state))
                    .bind(&row.created_at)
                    .bind(&row.session_file)
                    .execute(p)
                    .await?;
            }
        }
        crate::events::emit(
            "agent",
            serde_json::json!({"op": "created", "id": id, "name": row.name.as_str()}),
        );
        Ok(())
    }

    pub async fn update_agent_state(&self, id: &Uuid, state: AgentState) -> Result<()> {
        let sql = self.sql("UPDATE agents SET state = ? WHERE id = ?");
        let id_s = id.to_string();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(state_str(state))
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(state_str(state))
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
        }
        crate::events::emit(
            "agent_state",
            serde_json::json!({"id": id_s, "state": state_str(state)}),
        );
        Ok(())
    }

    pub async fn set_agent_session_file(&self, id: &Uuid, session_file: &str) -> Result<()> {
        let sql = self.sql("UPDATE agents SET session_file = ? WHERE id = ?");
        let id_s = id.to_string();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(session_file)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(session_file)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    // ── agent sessions (pi session eras) ────────────────────────────────

    /// Open a new session era: close any open row for the agent and insert
    /// a new one starting now. Idempotent: if the open row already tracks
    /// this session file (ack/report arrival races at create), do nothing.
    ///
    /// The check-close-insert sequence runs inside a single transaction
    /// that serializes concurrent callers for the same `agent_id` (SQLite:
    /// `BEGIN IMMEDIATE` takes the write lock up front; postgres: a
    /// per-agent advisory lock, since there may be no existing row to take
    /// a row lock on) — otherwise two concurrent calls could each observe
    /// "no open row yet" and both insert, leaving two open rows for one
    /// agent. The `agent_sessions_open_idx` partial unique index (v7
    /// migration) is a DB-level backstop against that outcome.
    pub async fn start_agent_session(&self, agent_id: &Uuid, session_file: &str) -> Result<()> {
        let id_s = agent_id.to_string();
        let now = castellan_time_now();
        let check = self.sql(
            "SELECT session_file FROM agent_sessions WHERE agent_id = ? AND ended_at IS NULL
             ORDER BY id DESC LIMIT 1",
        );
        let close = self
            .sql("UPDATE agent_sessions SET ended_at = ? WHERE agent_id = ? AND ended_at IS NULL");
        let insert = self.sql(
            "INSERT INTO agent_sessions (agent_id, session_file, started_at) VALUES (?, ?, ?)",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                let mut conn = p.acquire().await?;
                sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
                let result: Result<()> = async {
                    let open: Option<String> = sqlx::query(&check)
                        .bind(&id_s)
                        .fetch_optional(&mut *conn)
                        .await?
                        .map(|r| r.get::<String, _>(0));
                    if open.as_deref() != Some(session_file) {
                        sqlx::query(&close)
                            .bind(&now)
                            .bind(&id_s)
                            .execute(&mut *conn)
                            .await?;
                        sqlx::query(&insert)
                            .bind(&id_s)
                            .bind(session_file)
                            .bind(&now)
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) => sqlx::query("COMMIT").execute(&mut *conn).await?,
                    Err(e) => {
                        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                        return Err(e);
                    }
                };
            }
            Backend::Pg(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
                    .bind(&id_s)
                    .execute(&mut *tx)
                    .await?;
                let open: Option<String> = sqlx::query(&check)
                    .bind(&id_s)
                    .fetch_optional(&mut *tx)
                    .await?
                    .map(|r| r.get::<String, _>(0));
                if open.as_deref() != Some(session_file) {
                    sqlx::query(&close)
                        .bind(&now)
                        .bind(&id_s)
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query(&insert)
                        .bind(&id_s)
                        .bind(session_file)
                        .bind(&now)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }

    /// Ensure an open session row exists (backfill for agents that
    /// predate session tracking). `fallback_start` (the agent's
    /// created_at) is used when a row must be inserted.
    ///
    /// Serialized the same way as [`Store::start_agent_session`] — see its
    /// doc comment — to prevent two concurrent callers from both seeing
    /// `count == 0` and both inserting an open row.
    pub async fn ensure_open_session(
        &self,
        agent_id: &Uuid,
        session_file: &str,
        fallback_start: &str,
    ) -> Result<()> {
        let id_s = agent_id.to_string();
        let check =
            self.sql("SELECT COUNT(*) FROM agent_sessions WHERE agent_id = ? AND ended_at IS NULL");
        let insert = self.sql(
            "INSERT INTO agent_sessions (agent_id, session_file, started_at) VALUES (?, ?, ?)",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                let mut conn = p.acquire().await?;
                sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
                let result: Result<()> = async {
                    let count: i64 = sqlx::query(&check)
                        .bind(&id_s)
                        .fetch_one(&mut *conn)
                        .await?
                        .get::<i64, _>(0);
                    if count == 0 {
                        sqlx::query(&insert)
                            .bind(&id_s)
                            .bind(session_file)
                            .bind(fallback_start)
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) => sqlx::query("COMMIT").execute(&mut *conn).await?,
                    Err(e) => {
                        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                        return Err(e);
                    }
                };
            }
            Backend::Pg(p) => {
                let mut tx = p.begin().await?;
                sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)")
                    .bind(&id_s)
                    .execute(&mut *tx)
                    .await?;
                let count: i64 = sqlx::query(&check)
                    .bind(&id_s)
                    .fetch_one(&mut *tx)
                    .await?
                    .get::<i64, _>(0);
                if count == 0 {
                    sqlx::query(&insert)
                        .bind(&id_s)
                        .bind(session_file)
                        .bind(fallback_start)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }

    /// All session eras for an agent, oldest first (for conversation-log
    /// segmentation).
    pub async fn list_agent_sessions(&self, agent_id: &Uuid) -> Result<Vec<AgentSessionRow>> {
        let sql = self.sql(
            "SELECT id, agent_id, session_file, started_at, ended_at
             FROM agent_sessions WHERE agent_id = ? ORDER BY id",
        );
        let id_s = agent_id.to_string();
        type Row = (i64, String, String, String, Option<String>);
        let rows: Vec<Row> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).bind(&id_s).fetch_all(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).bind(&id_s).fetch_all(p).await?,
        };
        Ok(rows
            .into_iter()
            .map(|r| AgentSessionRow {
                id: r.0,
                agent_id: Uuid::parse_str(&r.1).unwrap_or_default(),
                session_file: r.2,
                started_at: r.3,
                ended_at: r.4,
            })
            .collect())
    }

    pub async fn set_agent_daemon(&self, id: &Uuid, daemon_endpoint_id: &str) -> Result<()> {
        let sql = self.sql("UPDATE agents SET daemon_endpoint_id = ? WHERE id = ?");
        let id_s = id.to_string();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(daemon_endpoint_id)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(daemon_endpoint_id)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn delete_agent(&self, id: &Uuid) -> Result<()> {
        let sql = self.sql("DELETE FROM agents WHERE id = ?");
        let id_s = id.to_string();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(&id_s).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(&id_s).execute(p).await?;
            }
        }
        crate::events::emit("agent", serde_json::json!({"op": "removed", "id": id_s}));
        Ok(())
    }

    pub async fn delete_daemon(&self, endpoint_id: &str) -> Result<()> {
        let sql = self.sql("DELETE FROM daemons WHERE endpoint_id = ?");
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(endpoint_id).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(endpoint_id).execute(p).await?;
            }
        }
        Ok(())
    }

    pub async fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentRow>> {
        let sql = self.sql(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file,
                    idle_secs, busy, activity_reported_at, needs_attention,
                    auto_suspend_override, woke_at
             FROM agents WHERE name = ?",
        );
        let row: Option<AgentTuple> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).bind(name).fetch_optional(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).bind(name).fetch_optional(p).await?,
        };
        row.map(row_to_agent).transpose()
    }

    pub async fn get_agent(&self, id: &Uuid) -> Result<Option<AgentRow>> {
        let sql = self.sql(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file,
                    idle_secs, busy, activity_reported_at, needs_attention,
                    auto_suspend_override, woke_at
             FROM agents WHERE id = ?",
        );
        let id_s = id.to_string();
        let row: Option<AgentTuple> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).bind(&id_s).fetch_optional(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).bind(&id_s).fetch_optional(p).await?,
        };
        row.map(row_to_agent).transpose()
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let sql = self.sql(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file,
                    idle_secs, busy, activity_reported_at, needs_attention,
                    auto_suspend_override, woke_at
             FROM agents ORDER BY name",
        );
        let rows: Vec<AgentTuple> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).fetch_all(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).fetch_all(p).await?,
        };
        rows.into_iter().map(row_to_agent).collect()
    }

    /// Record daemon-reported activity facts (state-report stream).
    pub async fn set_agent_activity(
        &self,
        id: &Uuid,
        idle_secs: Option<u64>,
        busy: Option<bool>,
    ) -> Result<()> {
        let sql = self.sql(
            "UPDATE agents SET idle_secs = ?, busy = ?, activity_reported_at = ? WHERE id = ?",
        );
        let id_s = id.to_string();
        let now = castellan_time_now();
        let idle = idle_secs.map(|s| s as i64);
        let busy = busy.map(|b| b as i64);
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(idle)
                    .bind(busy)
                    .bind(&now)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(idle)
                    .bind(busy)
                    .bind(&now)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
        }
        crate::events::emit(
            "agent_activity",
            serde_json::json!({"id": id_s, "busy": busy.map(|b| b != 0)}),
        );
        Ok(())
    }

    pub async fn set_needs_attention(&self, id: &Uuid, needs: bool) -> Result<()> {
        let sql = self.sql("UPDATE agents SET needs_attention = ? WHERE id = ?");
        let id_s = id.to_string();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(needs as i64)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(needs as i64)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    /// Runtime auto-suspend policy override; None clears to inherit.
    pub async fn set_auto_suspend_override(&self, id: &Uuid, value: Option<&str>) -> Result<()> {
        let sql = self.sql("UPDATE agents SET auto_suspend_override = ? WHERE id = ?");
        let id_s = id.to_string();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(value).bind(&id_s).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(value).bind(&id_s).execute(p).await?;
            }
        }
        Ok(())
    }

    /// Stamp the agent's (re)entry into Active (preemption grace window).
    pub async fn set_agent_woke_at(&self, id: &Uuid) -> Result<()> {
        let sql = self.sql("UPDATE agents SET woke_at = ? WHERE id = ?");
        let id_s = id.to_string();
        let now = castellan_time_now();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(&now).bind(&id_s).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(&now).bind(&id_s).execute(p).await?;
            }
        }
        Ok(())
    }

    // ── pending messages (durable wake queue) ───────────────────────────

    pub async fn enqueue_message(&self, agent_id: &Uuid, body: &str) -> Result<i64> {
        let sql = self.sql(
            "INSERT INTO pending_messages (agent_id, body, status, created_at)
             VALUES (?, ?, 'queued', ?)",
        );
        let id_s = agent_id.to_string();
        let now = castellan_time_now();
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                let row = sqlx::query(&sql)
                    .bind(&id_s)
                    .bind(body)
                    .bind(&now)
                    .execute(p)
                    .await?;
                Ok(row.last_insert_rowid())
            }
            Backend::Pg(p) => {
                let row: (i64,) = sqlx::query_as(&format!("{} RETURNING id", self.sql(&sql)))
                    .bind(&id_s)
                    .bind(body)
                    .bind(&now)
                    .fetch_one(p)
                    .await?;
                Ok(row.0)
            }
        }
    }

    /// Undelivered messages for an agent (queued + in-delivery), oldest first.
    pub async fn pending_messages(&self, agent_id: &Uuid) -> Result<Vec<PendingMessage>> {
        let sql = self.sql(
            "SELECT id, agent_id, body, status, created_at, delivered_at, last_error
             FROM pending_messages
             WHERE agent_id = ? AND status IN ('queued', 'waking')
             ORDER BY id",
        );
        let id_s = agent_id.to_string();
        type Row = (
            i64,
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).bind(&id_s).fetch_all(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).bind(&id_s).fetch_all(p).await?,
        };
        Ok(rows
            .into_iter()
            .map(|r| PendingMessage {
                id: r.0,
                agent_id: Uuid::parse_str(&r.1).unwrap_or_default(),
                body: r.2,
                status: r.3,
                created_at: r.4,
                delivered_at: r.5,
                last_error: r.6,
            })
            .collect())
    }

    /// Agents with undelivered messages (boot recovery: resume their wakes).
    pub async fn agents_with_pending_messages(&self) -> Result<Vec<Uuid>> {
        let sql = self.sql(
            "SELECT DISTINCT agent_id FROM pending_messages WHERE status IN ('queued', 'waking')",
        );
        let rows: Vec<(String,)> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).fetch_all(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).fetch_all(p).await?,
        };
        Ok(rows
            .into_iter()
            .filter_map(|r| Uuid::parse_str(&r.0).ok())
            .collect())
    }

    /// All-or-nothing: every id in `ids` is updated inside a single
    /// transaction, so a mid-batch failure (e.g. a dropped connection)
    /// can't leave the durable wake queue partially updated.
    pub async fn set_message_status(
        &self,
        ids: &[i64],
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let delivered = if status == "delivered" {
            Some(castellan_time_now())
        } else {
            None
        };
        let sql = self.sql(
            "UPDATE pending_messages SET status = ?, last_error = COALESCE(?, last_error),
             delivered_at = COALESCE(?, delivered_at) WHERE id = ?",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                let mut conn = p.acquire().await?;
                sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
                let result: Result<()> = async {
                    for id in ids {
                        sqlx::query(&sql)
                            .bind(status)
                            .bind(error)
                            .bind(&delivered)
                            .bind(id)
                            .execute(&mut *conn)
                            .await?;
                    }
                    Ok(())
                }
                .await;
                match result {
                    Ok(()) => sqlx::query("COMMIT").execute(&mut *conn).await?,
                    Err(e) => {
                        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                        return Err(e);
                    }
                };
            }
            Backend::Pg(p) => {
                let mut tx = p.begin().await?;
                for id in ids {
                    sqlx::query(&sql)
                        .bind(status)
                        .bind(error)
                        .bind(&delivered)
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
        }
        Ok(())
    }

    /// Retention: drop delivered/failed message rows older than `days`.
    pub async fn prune_messages(&self, days: u32) -> Result<()> {
        let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(days as i64);
        let cutoff = cutoff
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let sql = self.sql(
            "DELETE FROM pending_messages
             WHERE status IN ('delivered', 'failed') AND created_at < ?",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql).bind(&cutoff).execute(p).await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql).bind(&cutoff).execute(p).await?;
            }
        }
        Ok(())
    }

    // ── log index ───────────────────────────────────────────────────────

    pub async fn acked_through(&self, agent_id: &Uuid) -> Result<u64> {
        let sql = self.sql("SELECT acked_through FROM log_index WHERE agent_id = ?");
        let id_s = agent_id.to_string();
        let value: Option<i64> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query(&sql)
                .bind(&id_s)
                .fetch_optional(p)
                .await?
                .map(|r: SqliteRow| r.get::<i64, _>(0)),
            Backend::Pg(p) => sqlx::query(&sql)
                .bind(&id_s)
                .fetch_optional(p)
                .await?
                .map(|r: sqlx::postgres::PgRow| r.get::<i64, _>(0)),
        };
        Ok(value.unwrap_or(0) as u64)
    }

    pub async fn set_acked_through(&self, agent_id: &Uuid, seq: u64) -> Result<()> {
        let sql = self.sql(
            "INSERT INTO log_index (agent_id, acked_through, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(agent_id) DO UPDATE SET acked_through = excluded.acked_through,
             updated_at = excluded.updated_at",
        );
        let id_s = agent_id.to_string();
        let now = castellan_time_now();
        let seq = seq as i64;
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(&id_s)
                    .bind(seq)
                    .bind(&now)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(&id_s)
                    .bind(seq)
                    .bind(&now)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    // -- chat/transcript event log (v6, docs/UNIFIED-AGENT-API-DESIGN.md §4.6) --
    // These back `crate::chat_store::ChatStore` the same way the methods
    // above back `crate::registry::Registry`: named identically to the
    // trait methods, so `impl ChatStore for Store` can delegate to them —
    // Rust resolves `self.append(...)` against these inherent methods
    // before the trait's, so the delegation doesn't recurse.

    /// Append one event. Idempotent on `(agent_id, seq)` — `handle_logs`
    /// already dedupes against `acked_through` before calling this, but a
    /// second write of the same seq (e.g. a retried batch) is a no-op
    /// rather than an error either way.
    pub async fn append(
        &self,
        agent_id: &Uuid,
        event: &suzerain_protocol::event::LogEvent,
    ) -> Result<()> {
        let sql = self.sql(
            "INSERT INTO chat_events (agent_id, seq, at, kind, payload) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(agent_id, seq) DO NOTHING",
        );
        let id_s = agent_id.to_string();
        let seq = event.seq as i64;
        let payload = serde_json::to_string(&event.payload)?;
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&sql)
                    .bind(&id_s)
                    .bind(seq)
                    .bind(&event.at)
                    .bind(&event.kind)
                    .bind(&payload)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&sql)
                    .bind(&id_s)
                    .bind(seq)
                    .bind(&event.at)
                    .bind(&event.kind)
                    .bind(&payload)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    /// Last `n` events for an agent, oldest first — a real indexed query
    /// (`ORDER BY seq DESC LIMIT n`, then reversed) instead of reading an
    /// entire file and slicing it.
    pub async fn tail(
        &self,
        agent_id: &Uuid,
        n: usize,
    ) -> Result<Vec<suzerain_protocol::event::LogEvent>> {
        let sql = self.sql(
            "SELECT seq, at, kind, payload FROM chat_events
             WHERE agent_id = ? ORDER BY seq DESC LIMIT ?",
        );
        let id_s = agent_id.to_string();
        let limit = n as i64;
        type Row = (i64, String, String, String);
        let rows: Vec<Row> = match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query_as(&sql)
                    .bind(&id_s)
                    .bind(limit)
                    .fetch_all(p)
                    .await?
            }
            Backend::Pg(p) => {
                sqlx::query_as(&sql)
                    .bind(&id_s)
                    .bind(limit)
                    .fetch_all(p)
                    .await?
            }
        };
        let mut events = rows
            .into_iter()
            .map(|r| chat_row_to_event(*agent_id, r))
            .collect::<Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// Every event with `seq > seq`, oldest first — the query the JSONL-file
    /// arrangement had no equivalent for at all.
    pub async fn history_since(
        &self,
        agent_id: &Uuid,
        seq: u64,
    ) -> Result<Vec<suzerain_protocol::event::LogEvent>> {
        let sql = self.sql(
            "SELECT seq, at, kind, payload FROM chat_events
             WHERE agent_id = ? AND seq > ? ORDER BY seq ASC",
        );
        let id_s = agent_id.to_string();
        let since = seq as i64;
        type Row = (i64, String, String, String);
        let rows: Vec<Row> = match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query_as(&sql)
                    .bind(&id_s)
                    .bind(since)
                    .fetch_all(p)
                    .await?
            }
            Backend::Pg(p) => {
                sqlx::query_as(&sql)
                    .bind(&id_s)
                    .bind(since)
                    .fetch_all(p)
                    .await?
            }
        };
        rows.into_iter()
            .map(|r| chat_row_to_event(*agent_id, r))
            .collect()
    }
}

fn chat_row_to_event(
    agent_id: Uuid,
    r: (i64, String, String, String),
) -> Result<suzerain_protocol::event::LogEvent> {
    Ok(suzerain_protocol::event::LogEvent {
        agent_id,
        seq: r.0 as u64,
        at: r.1,
        kind: r.2,
        payload: serde_json::from_str(&r.3)?,
    })
}

fn row_to_agent(r: AgentTuple) -> Result<AgentRow> {
    let id = Uuid::parse_str(&r.0)?;
    let manifest: AgentManifest = serde_json::from_str(&r.3)?;
    let state = parse_state(&r.4);
    Ok(AgentRow {
        id,
        name: r.1,
        daemon_endpoint_id: r.2,
        manifest,
        state,
        created_at: r.5,
        session_file: r.6,
        idle_secs: r.7,
        busy: r.8.map(|b| b == 1),
        activity_reported_at: r.9,
        needs_attention: r.10 == Some(1),
        auto_suspend_override: r.11,
        woke_at: r.12,
    })
}

pub fn state_str(state: AgentState) -> &'static str {
    match state {
        AgentState::Provisioning => "provisioning",
        AgentState::Active => "active",
        AgentState::Suspended => "suspended",
        AgentState::Restoring => "restoring",
        AgentState::Failed => "failed",
        AgentState::Decommissioned => "decommissioned",
    }
}

fn parse_state(s: &str) -> AgentState {
    match s {
        "active" => AgentState::Active,
        "suspended" => AgentState::Suspended,
        "restoring" => AgentState::Restoring,
        "failed" => AgentState::Failed,
        "decommissioned" => AgentState::Decommissioned,
        _ => AgentState::Provisioning,
    }
}

pub fn castellan_time_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown".into())
}

#[cfg(test)]
mod session_bookkeeping_tests {
    use super::*;

    async fn memory_store() -> Store {
        // Named, shared-cache in-memory sqlite DB, unique per test — avoids
        // mutating the process-wide `SUZERAIN_DATABASE_URL` env var, which
        // `cargo test`'s parallel execution would otherwise race on.
        let name = format!("session-bookkeeping-test-{}", Uuid::new_v4().simple());
        let url = format!("sqlite://file:{name}?mode=memory&cache=shared");
        Store::open_with_url(&url)
            .await
            .expect("open in-memory store")
    }

    async fn open_session_count(store: &Store, agent_id: &Uuid) -> i64 {
        let sql = store
            .sql("SELECT COUNT(*) FROM agent_sessions WHERE agent_id = ? AND ended_at IS NULL");
        match store.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query(&sql)
                .bind(agent_id.to_string())
                .fetch_one(p)
                .await
                .unwrap()
                .get::<i64, _>(0),
            Backend::Pg(_) => unreachable!("test uses sqlite backend"),
        }
    }

    /// Concurrent `start_agent_session` calls for the same agent must never
    /// leave more than one open row — a race here corrupts log
    /// segmentation (see module doc on `agent_sessions`).
    #[tokio::test]
    async fn start_agent_session_concurrent_calls_leave_one_open_row() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let mut handles = Vec::new();
        for i in 0..16 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .start_agent_session(&agent_id, &format!("session-{i}.jsonl"))
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(open_session_count(&store, &agent_id).await, 1);
    }

    /// Concurrent `ensure_open_session` calls (agents predating session
    /// tracking) must insert exactly one backfill row, not one per caller.
    #[tokio::test]
    async fn ensure_open_session_concurrent_calls_insert_once() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let mut handles = Vec::new();
        for i in 0..16 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .ensure_open_session(
                        &agent_id,
                        &format!("session-{i}.jsonl"),
                        "2026-08-27T00:00:00Z",
                    )
                    .await
                    .unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(open_session_count(&store, &agent_id).await, 1);
        let all = store.list_agent_sessions(&agent_id).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    /// A batch update via `set_message_status` must apply atomically:
    /// every id in the batch ends up with matching status + delivered_at,
    /// consistent with the single transaction commit.
    #[tokio::test]
    async fn set_message_status_updates_batch_atomically() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(
                store
                    .enqueue_message(&agent_id, &format!("msg-{i}"))
                    .await
                    .unwrap(),
            );
        }

        store
            .set_message_status(&ids, "delivered", None)
            .await
            .unwrap();

        let sql = store.sql("SELECT status, delivered_at FROM pending_messages WHERE agent_id = ?");
        let rows: Vec<(String, Option<String>)> = match store.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql)
                .bind(agent_id.to_string())
                .fetch_all(p)
                .await
                .unwrap(),
            Backend::Pg(_) => unreachable!("test uses sqlite backend"),
        };
        assert_eq!(rows.len(), ids.len());
        for (status, delivered_at) in &rows {
            assert_eq!(status, "delivered");
            assert!(delivered_at.is_some());
        }
    }
}

#[cfg(test)]
mod store_crud_tests {
    use super::*;
    use suzerain_protocol::event::LogEvent;
    use suzerain_protocol::manifest::{Harness, ModelSpec};
    use suzerain_protocol::{DaemonInfo, NodeCapacity, NodeUsage};

    async fn memory_store() -> Store {
        // Named, shared-cache in-memory sqlite DB, unique per test — see
        // `session_bookkeeping_tests::memory_store` for why this form (not
        // `sqlite::memory:`, which cargo test's parallelism would race on
        // via env var mutation) is used.
        let name = format!("store-crud-test-{}", Uuid::new_v4().simple());
        let url = format!("sqlite://file:{name}?mode=memory&cache=shared");
        Store::open_with_url(&url)
            .await
            .expect("open in-memory store")
    }

    fn daemon_info(endpoint_id: &str, hostname: &str) -> DaemonInfo {
        DaemonInfo {
            endpoint_id: endpoint_id.to_string(),
            hostname: hostname.to_string(),
            os: "linux".into(),
            arch: "x86_64".into(),
            labels: Default::default(),
            max_agents: 4,
            agents: Vec::new(),
            capacity: NodeCapacity::default(),
            usage: NodeUsage::default(),
        }
    }

    fn agent_row(name: &str, daemon: &str) -> AgentRow {
        AgentRow {
            id: Uuid::new_v4(),
            name: name.to_string(),
            daemon_endpoint_id: daemon.to_string(),
            manifest: AgentManifest {
                name: name.to_string(),
                harness: Harness {
                    kind: "pi".into(),
                    version: "0.1.0".into(),
                },
                model: ModelSpec {
                    provider: "anthropic".into(),
                    id: "claude-test".into(),
                    thinking: None,
                },
                resources: Default::default(),
                schedule: Default::default(),
                toolchain: Default::default(),
                repos: Vec::new(),
                extensions: Vec::new(),
                prompt: Default::default(),
                secrets: Default::default(),
                egress: Default::default(),
                observability: Default::default(),
                lifecycle: Default::default(),
                provision: None,
            },
            state: AgentState::Provisioning,
            created_at: castellan_time_now(),
            session_file: None,
            idle_secs: None,
            busy: None,
            activity_reported_at: None,
            needs_attention: false,
            auto_suspend_override: None,
            woke_at: None,
        }
    }

    async fn raw_count(store: &Store, sql: &str) -> i64 {
        let sql = store.sql(sql);
        match store.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query(&sql)
                .fetch_one(p)
                .await
                .unwrap()
                .get::<i64, _>(0),
            Backend::Pg(_) => unreachable!("test uses sqlite backend"),
        }
    }

    // ── daemons ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn approve_daemon_roundtrip() {
        let store = memory_store().await;
        assert!(!store.daemon_approved("d1").await.unwrap());
        store.approve_daemon("d1").await.unwrap();
        assert!(store.daemon_approved("d1").await.unwrap());
        // Unknown daemon is not approved (not an error).
        assert!(!store.daemon_approved("unknown").await.unwrap());
    }

    #[tokio::test]
    async fn approve_daemon_clears_pending_entry() {
        let store = memory_store().await;
        store
            .upsert_pending_daemon(&daemon_info("d1", "host-a"))
            .await
            .unwrap();
        assert_eq!(store.list_pending_daemons().await.unwrap().len(), 1);

        store.approve_daemon("d1").await.unwrap();

        assert_eq!(store.list_pending_daemons().await.unwrap().len(), 0);
        assert!(store.daemon_approved("d1").await.unwrap());
    }

    #[tokio::test]
    async fn upsert_daemon_insert_then_update_preserves_identity() {
        let store = memory_store().await;
        let mut info = daemon_info("d1", "host-a");
        store.upsert_daemon(&info, true).await.unwrap();

        let rows = store.list_daemons().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hostname, "host-a");
        assert!(rows[0].online);
        assert!(!rows[0].approved);

        // Update: same endpoint_id, changed fields — must update in place,
        // not create a second row (ON CONFLICT DO UPDATE).
        info.hostname = "host-a-renamed".to_string();
        store.upsert_daemon(&info, false).await.unwrap();

        let rows = store.list_daemons().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].hostname, "host-a-renamed");
        assert!(!rows[0].online);
    }

    #[tokio::test]
    async fn list_daemons_ordered_by_endpoint_id() {
        let store = memory_store().await;
        store
            .upsert_daemon(&daemon_info("zzz", "host-z"), true)
            .await
            .unwrap();
        store
            .upsert_daemon(&daemon_info("aaa", "host-a"), true)
            .await
            .unwrap();
        store
            .upsert_daemon(&daemon_info("mmm", "host-m"), true)
            .await
            .unwrap();

        let rows = store.list_daemons().await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.endpoint_id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "mmm", "zzz"]);
    }

    #[tokio::test]
    async fn set_daemon_usage_updates_usage_only() {
        let store = memory_store().await;
        store
            .upsert_daemon(&daemon_info("d1", "host-a"), true)
            .await
            .unwrap();

        store
            .set_daemon_usage("d1", r#"{"memory_mib_free":123}"#)
            .await
            .unwrap();

        let rows = store.list_daemons().await.unwrap();
        assert_eq!(rows[0].usage().memory_mib_free, 123);
        assert_eq!(rows[0].hostname, "host-a"); // untouched
    }

    #[tokio::test]
    async fn label_overrides_win_over_reported_labels() {
        let store = memory_store().await;
        let mut info = daemon_info("d1", "host-a");
        info.labels
            .insert("region".to_string(), "us-east".to_string());
        store.upsert_daemon(&info, true).await.unwrap();

        store
            .set_label_overrides("d1", r#"{"region":"eu-west","gpu":"true"}"#)
            .await
            .unwrap();

        let rows = store.list_daemons().await.unwrap();
        let effective = rows[0].effective_labels();
        assert_eq!(effective.get("region").map(String::as_str), Some("eu-west"));
        assert_eq!(effective.get("gpu").map(String::as_str), Some("true"));
    }

    #[tokio::test]
    async fn set_all_daemons_offline_flips_every_row() {
        let store = memory_store().await;
        store
            .upsert_daemon(&daemon_info("d1", "host-a"), true)
            .await
            .unwrap();
        store
            .upsert_daemon(&daemon_info("d2", "host-b"), true)
            .await
            .unwrap();

        store.set_all_daemons_offline().await.unwrap();

        let rows = store.list_daemons().await.unwrap();
        assert!(rows.iter().all(|r| !r.online));
    }

    #[tokio::test]
    async fn delete_daemon_removes_row() {
        let store = memory_store().await;
        store
            .upsert_daemon(&daemon_info("d1", "host-a"), true)
            .await
            .unwrap();
        store.delete_daemon("d1").await.unwrap();
        assert!(store.list_daemons().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_daemons_upsert_and_list_ordered_by_first_seen() {
        let store = memory_store().await;
        store
            .upsert_pending_daemon(&daemon_info("d1", "host-a"))
            .await
            .unwrap();
        store
            .upsert_pending_daemon(&daemon_info("d2", "host-b"))
            .await
            .unwrap();
        // Re-registering d1 must update in place, not duplicate.
        store
            .upsert_pending_daemon(&daemon_info("d1", "host-a-2"))
            .await
            .unwrap();

        let pending = store.list_pending_daemons().await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0]["endpoint_id"], "d1");
        assert_eq!(pending[0]["hostname"], "host-a-2");
    }

    #[tokio::test]
    async fn delete_pending_daemon_removes_row() {
        let store = memory_store().await;
        store
            .upsert_pending_daemon(&daemon_info("d1", "host-a"))
            .await
            .unwrap();
        store.delete_pending_daemon("d1").await.unwrap();
        assert!(store.list_pending_daemons().await.unwrap().is_empty());
    }

    // ── agents ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn create_agent_then_get_by_id_and_name() {
        let store = memory_store().await;
        let row = agent_row("agent-a", "d1");
        store.create_agent(&row).await.unwrap();

        let by_id = store
            .get_agent(&row.id)
            .await
            .unwrap()
            .expect("found by id");
        assert_eq!(by_id.name, "agent-a");
        assert_eq!(by_id.daemon_endpoint_id, "d1");
        assert_eq!(by_id.manifest.model.id, "claude-test");

        let by_name = store
            .get_agent_by_name("agent-a")
            .await
            .unwrap()
            .expect("found by name");
        assert_eq!(by_name.id, row.id);

        assert!(store
            .get_agent_by_name("nonexistent")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn create_agent_duplicate_name_is_rejected() {
        let store = memory_store().await;
        let a = agent_row("dup-name", "d1");
        let mut b = agent_row("dup-name", "d1");
        b.id = Uuid::new_v4();

        store.create_agent(&a).await.unwrap();
        let result = store.create_agent(&b).await;
        assert!(
            result.is_err(),
            "expected UNIQUE constraint violation on agents.name"
        );

        // Original row is unaffected.
        assert_eq!(store.list_agents().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_agents_ordered_by_name() {
        let store = memory_store().await;
        store
            .create_agent(&agent_row("charlie", "d1"))
            .await
            .unwrap();
        store.create_agent(&agent_row("alice", "d1")).await.unwrap();
        store.create_agent(&agent_row("bob", "d1")).await.unwrap();

        let names: Vec<String> = store
            .list_agents()
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, vec!["alice", "bob", "charlie"]);
    }

    #[tokio::test]
    async fn update_agent_state_persists() {
        let store = memory_store().await;
        let row = agent_row("agent-a", "d1");
        store.create_agent(&row).await.unwrap();

        store
            .update_agent_state(&row.id, AgentState::Active)
            .await
            .unwrap();

        let got = store.get_agent(&row.id).await.unwrap().unwrap();
        assert!(matches!(got.state, AgentState::Active));
    }

    #[tokio::test]
    async fn set_agent_session_file_and_daemon() {
        let store = memory_store().await;
        let row = agent_row("agent-a", "d1");
        store.create_agent(&row).await.unwrap();

        store
            .set_agent_session_file(&row.id, "/agent/sessions/x.jsonl")
            .await
            .unwrap();
        store.set_agent_daemon(&row.id, "d2").await.unwrap();

        let got = store.get_agent(&row.id).await.unwrap().unwrap();
        assert_eq!(got.session_file.as_deref(), Some("/agent/sessions/x.jsonl"));
        assert_eq!(got.daemon_endpoint_id, "d2");
    }

    #[tokio::test]
    async fn agent_activity_flags_roundtrip() {
        let store = memory_store().await;
        let row = agent_row("agent-a", "d1");
        store.create_agent(&row).await.unwrap();

        store
            .set_agent_activity(&row.id, Some(42), Some(true))
            .await
            .unwrap();
        store.set_needs_attention(&row.id, true).await.unwrap();
        store
            .set_auto_suspend_override(&row.id, Some("never"))
            .await
            .unwrap();
        store.set_agent_woke_at(&row.id).await.unwrap();

        let got = store.get_agent(&row.id).await.unwrap().unwrap();
        assert_eq!(got.idle_secs, Some(42));
        assert_eq!(got.busy, Some(true));
        assert!(got.needs_attention);
        assert_eq!(got.auto_suspend_override.as_deref(), Some("never"));
        assert!(got.woke_at.is_some());

        // Clearing the override (None) must clear the column, not no-op.
        store
            .set_auto_suspend_override(&row.id, None)
            .await
            .unwrap();
        let got = store.get_agent(&row.id).await.unwrap().unwrap();
        assert_eq!(got.auto_suspend_override, None);
    }

    #[tokio::test]
    async fn delete_agent_removes_row() {
        let store = memory_store().await;
        let row = agent_row("agent-a", "d1");
        store.create_agent(&row).await.unwrap();
        store.delete_agent(&row.id).await.unwrap();
        assert!(store.get_agent(&row.id).await.unwrap().is_none());
    }

    // ── agent sessions ──────────────────────────────────────────────────

    #[tokio::test]
    async fn start_agent_session_rotates_and_closes_previous() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        store
            .start_agent_session(&agent_id, "s1.jsonl")
            .await
            .unwrap();
        store
            .start_agent_session(&agent_id, "s2.jsonl")
            .await
            .unwrap();

        let sessions = store.list_agent_sessions(&agent_id).await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_file, "s1.jsonl");
        assert!(sessions[0].ended_at.is_some());
        assert_eq!(sessions[1].session_file, "s2.jsonl");
        assert!(sessions[1].ended_at.is_none());
    }

    #[tokio::test]
    async fn start_agent_session_is_idempotent_for_same_file() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        store
            .start_agent_session(&agent_id, "s1.jsonl")
            .await
            .unwrap();
        // Re-reporting the same open session file must not open a new row.
        store
            .start_agent_session(&agent_id, "s1.jsonl")
            .await
            .unwrap();

        let sessions = store.list_agent_sessions(&agent_id).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].ended_at.is_none());
    }

    // ── pending messages ────────────────────────────────────────────────

    #[tokio::test]
    async fn enqueue_and_fetch_pending_messages_oldest_first() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let id1 = store.enqueue_message(&agent_id, "first").await.unwrap();
        let id2 = store.enqueue_message(&agent_id, "second").await.unwrap();
        assert!(id2 > id1);

        let pending = store.pending_messages(&agent_id).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].body, "first");
        assert_eq!(pending[1].body, "second");
        assert_eq!(pending[0].status, "queued");
    }

    #[tokio::test]
    async fn pending_messages_excludes_delivered_and_failed() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        let id1 = store.enqueue_message(&agent_id, "a").await.unwrap();
        let id2 = store.enqueue_message(&agent_id, "b").await.unwrap();

        store
            .set_message_status(&[id1], "delivered", None)
            .await
            .unwrap();
        store
            .set_message_status(&[id2], "failed", Some("boom"))
            .await
            .unwrap();

        assert!(store.pending_messages(&agent_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn agents_with_pending_messages_lists_distinct_agents() {
        let store = memory_store().await;
        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        store.enqueue_message(&a1, "x").await.unwrap();
        store.enqueue_message(&a1, "y").await.unwrap();
        let id = store.enqueue_message(&a2, "z").await.unwrap();
        store
            .set_message_status(&[id], "delivered", None)
            .await
            .unwrap();

        let agents = store.agents_with_pending_messages().await.unwrap();
        assert_eq!(agents, vec![a1]);
    }

    #[tokio::test]
    async fn prune_messages_only_removes_old_terminal_rows() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        let old_delivered = store
            .enqueue_message(&agent_id, "old-delivered")
            .await
            .unwrap();
        let recent_delivered = store
            .enqueue_message(&agent_id, "recent-delivered")
            .await
            .unwrap();
        let old_queued = store
            .enqueue_message(&agent_id, "old-queued")
            .await
            .unwrap();

        store
            .set_message_status(&[old_delivered], "delivered", None)
            .await
            .unwrap();
        store
            .set_message_status(&[recent_delivered], "delivered", None)
            .await
            .unwrap();
        // old_queued stays queued (not terminal) despite being "old".

        let old_ts = (time::OffsetDateTime::now_utc() - time::Duration::days(90))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let backdate = store.sql("UPDATE pending_messages SET created_at = ? WHERE id = ?");
        match store.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&backdate)
                    .bind(&old_ts)
                    .bind(old_delivered)
                    .execute(p)
                    .await
                    .unwrap();
                sqlx::query(&backdate)
                    .bind(&old_ts)
                    .bind(old_queued)
                    .execute(p)
                    .await
                    .unwrap();
            }
            Backend::Pg(_) => unreachable!("test uses sqlite backend"),
        }

        store.prune_messages(30).await.unwrap();

        let remaining = raw_count(&store, "SELECT COUNT(*) FROM pending_messages").await;
        assert_eq!(
            remaining, 2,
            "old delivered row pruned, recent delivered + old queued kept"
        );
        assert!(!store.pending_messages(&agent_id).await.unwrap().is_empty());
    }

    // ── log index ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn acked_through_defaults_to_zero_and_upserts() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        assert_eq!(store.acked_through(&agent_id).await.unwrap(), 0);

        store.set_acked_through(&agent_id, 10).await.unwrap();
        assert_eq!(store.acked_through(&agent_id).await.unwrap(), 10);

        // Second call updates in place rather than erroring or duplicating.
        store.set_acked_through(&agent_id, 25).await.unwrap();
        assert_eq!(store.acked_through(&agent_id).await.unwrap(), 25);
    }

    // ── chat/transcript event log ───────────────────────────────────────

    fn log_event(agent_id: Uuid, seq: u64) -> LogEvent {
        LogEvent {
            agent_id,
            seq,
            at: castellan_time_now(),
            kind: "message_update".to_string(),
            payload: serde_json::json!({"seq": seq}),
        }
    }

    #[tokio::test]
    async fn append_then_tail_and_history_since() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        for seq in 1..=5u64 {
            store
                .append(&agent_id, &log_event(agent_id, seq))
                .await
                .unwrap();
        }

        let tail = store.tail(&agent_id, 2).await.unwrap();
        assert_eq!(tail.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);

        let since = store.history_since(&agent_id, 3).await.unwrap();
        assert_eq!(since.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 5]);
    }

    #[tokio::test]
    async fn append_is_idempotent_on_agent_and_seq() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();
        let ev = log_event(agent_id, 1);

        store.append(&agent_id, &ev).await.unwrap();
        // Re-append of the same (agent_id, seq): no-op, not an error, no dup.
        store.append(&agent_id, &ev).await.unwrap();

        let all = store.history_since(&agent_id, 0).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    // ── concurrency ─────────────────────────────────────────────────────

    /// Concurrent `enqueue_message` calls for the same agent must each get
    /// a distinct id and produce exactly one row per call — an
    /// autoincrement race here would mean a lost or duplicated message in
    /// the durable wake queue.
    #[tokio::test]
    async fn concurrent_enqueue_message_yields_distinct_ids_and_no_lost_rows() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let mut handles = Vec::new();
        for i in 0..20 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store
                    .enqueue_message(&agent_id, &format!("msg-{i}"))
                    .await
                    .unwrap()
            }));
        }
        let mut ids = Vec::new();
        for h in handles {
            ids.push(h.await.unwrap());
        }

        let unique: std::collections::HashSet<i64> = ids.iter().copied().collect();
        assert_eq!(
            unique.len(),
            20,
            "every concurrent enqueue must get a distinct id"
        );

        let pending = store.pending_messages(&agent_id).await.unwrap();
        assert_eq!(
            pending.len(),
            20,
            "no message lost under concurrent inserts"
        );
    }

    /// Concurrent `append` calls carrying the *same* `(agent_id, seq)` (as
    /// could happen if a batch is retried by two racing callers) must
    /// collapse to exactly one row — the `ON CONFLICT DO NOTHING` idempotency
    /// contract documented on `Store::append`.
    #[tokio::test]
    async fn concurrent_append_same_seq_collapses_to_one_row() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let mut handles = Vec::new();
        for i in 0..16 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                let ev = LogEvent {
                    agent_id,
                    seq: 1,
                    at: castellan_time_now(),
                    kind: "message_update".to_string(),
                    payload: serde_json::json!({"attempt": i}),
                };
                store.append(&agent_id, &ev).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let all = store.history_since(&agent_id, 0).await.unwrap();
        assert_eq!(all.len(), 1, "same (agent_id, seq) must dedup to one row");
    }

    /// Concurrent `set_acked_through` calls for the same agent must never
    /// panic or error out from the upsert (`ON CONFLICT DO UPDATE`) racing
    /// with itself, and the row must end up holding one of the written
    /// values (last-write-wins is acceptable; a torn/partial row is not).
    #[tokio::test]
    async fn concurrent_set_acked_through_leaves_one_consistent_row() {
        let store = memory_store().await;
        let agent_id = Uuid::new_v4();

        let mut handles = Vec::new();
        for seq in 1..=10u64 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                store.set_acked_through(&agent_id, seq).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let count = raw_count(&store, "SELECT COUNT(*) FROM log_index").await;
        assert_eq!(
            count, 1,
            "upsert must never leave more than one row per agent"
        );
        let final_value = store.acked_through(&agent_id).await.unwrap();
        assert!((1..=10).contains(&final_value));
    }
}
