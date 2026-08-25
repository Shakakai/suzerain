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
    pub async fn start_agent_session(&self, agent_id: &Uuid, session_file: &str) -> Result<()> {
        let id_s = agent_id.to_string();
        let check = self.sql(
            "SELECT session_file FROM agent_sessions WHERE agent_id = ? AND ended_at IS NULL
             ORDER BY id DESC LIMIT 1",
        );
        let open: Option<String> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query(&check)
                .bind(&id_s)
                .fetch_optional(p)
                .await?
                .map(|r| r.get::<String, _>(0)),
            Backend::Pg(p) => sqlx::query(&check)
                .bind(&id_s)
                .fetch_optional(p)
                .await?
                .map(|r| r.get::<String, _>(0)),
        };
        if open.as_deref() == Some(session_file) {
            return Ok(());
        }
        let now = castellan_time_now();
        let close = self
            .sql("UPDATE agent_sessions SET ended_at = ? WHERE agent_id = ? AND ended_at IS NULL");
        let insert = self.sql(
            "INSERT INTO agent_sessions (agent_id, session_file, started_at) VALUES (?, ?, ?)",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&close)
                    .bind(&now)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
                sqlx::query(&insert)
                    .bind(&id_s)
                    .bind(session_file)
                    .bind(&now)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&close)
                    .bind(&now)
                    .bind(&id_s)
                    .execute(p)
                    .await?;
                sqlx::query(&insert)
                    .bind(&id_s)
                    .bind(session_file)
                    .bind(&now)
                    .execute(p)
                    .await?;
            }
        }
        Ok(())
    }

    /// Ensure an open session row exists (backfill for agents that
    /// predate session tracking). `fallback_start` (the agent's
    /// created_at) is used when a row must be inserted.
    pub async fn ensure_open_session(
        &self,
        agent_id: &Uuid,
        session_file: &str,
        fallback_start: &str,
    ) -> Result<()> {
        let id_s = agent_id.to_string();
        let check =
            self.sql("SELECT COUNT(*) FROM agent_sessions WHERE agent_id = ? AND ended_at IS NULL");
        let count: i64 = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query(&check)
                .bind(&id_s)
                .fetch_one(p)
                .await?
                .get::<i64, _>(0),
            Backend::Pg(p) => sqlx::query(&check)
                .bind(&id_s)
                .fetch_one(p)
                .await?
                .get::<i64, _>(0),
        };
        if count > 0 {
            return Ok(());
        }
        let insert = self.sql(
            "INSERT INTO agent_sessions (agent_id, session_file, started_at) VALUES (?, ?, ?)",
        );
        match self.backend.as_ref() {
            Backend::Sqlite(p) => {
                sqlx::query(&insert)
                    .bind(&id_s)
                    .bind(session_file)
                    .bind(fallback_start)
                    .execute(p)
                    .await?;
            }
            Backend::Pg(p) => {
                sqlx::query(&insert)
                    .bind(&id_s)
                    .bind(session_file)
                    .bind(fallback_start)
                    .execute(p)
                    .await?;
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
        for id in ids {
            let sql = self.sql(
                "UPDATE pending_messages SET status = ?, last_error = COALESCE(?, last_error),
                 delivered_at = COALESCE(?, delivered_at) WHERE id = ?",
            );
            match self.backend.as_ref() {
                Backend::Sqlite(p) => {
                    sqlx::query(&sql)
                        .bind(status)
                        .bind(error)
                        .bind(&delivered)
                        .bind(id)
                        .execute(p)
                        .await?;
                }
                Backend::Pg(p) => {
                    sqlx::query(&sql)
                        .bind(status)
                        .bind(error)
                        .bind(&delivered)
                        .bind(id)
                        .execute(p)
                        .await?;
                }
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
