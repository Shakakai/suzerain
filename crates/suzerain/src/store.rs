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
    pub labels: String,
    pub max_agents: u32,
    pub last_seen: String,
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
);
type AgentTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
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

        let backend = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let pool = sqlx::postgres::PgPoolOptions::new().connect(&url).await?;
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
        tracing::info!(
            backend = if matches!(backend, Backend::Pg(_)) {
                "postgres"
            } else {
                "sqlite"
            },
            "store opened"
        );
        Ok(Self {
            backend: std::sync::Arc::new(backend),
        })
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
            "INSERT INTO daemons (endpoint_id, hostname, os, arch, labels, max_agents, online, last_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET
                hostname = excluded.hostname, os = excluded.os, arch = excluded.arch,
                labels = excluded.labels, max_agents = excluded.max_agents,
                online = excluded.online, last_seen = excluded.last_seen",
        );
        let labels = serde_json::to_string(&info.labels)?;
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
        Ok(())
    }

    pub async fn list_daemons(&self) -> Result<Vec<DaemonRow>> {
        let sql = self.sql(
            "SELECT endpoint_id, approved, online, hostname, os, arch, labels, max_agents, last_seen
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
        Ok(())
    }

    pub async fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentRow>> {
        let sql = self.sql(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file
             FROM agents WHERE name = ?",
        );
        let row: Option<AgentTuple> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).bind(name).fetch_optional(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).bind(name).fetch_optional(p).await?,
        };
        row.map(row_to_agent).transpose()
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let sql = self.sql(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file
             FROM agents ORDER BY name",
        );
        let rows: Vec<AgentTuple> = match self.backend.as_ref() {
            Backend::Sqlite(p) => sqlx::query_as(&sql).fetch_all(p).await?,
            Backend::Pg(p) => sqlx::query_as(&sql).fetch_all(p).await?,
        };
        rows.into_iter().map(row_to_agent).collect()
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
}

fn row_to_agent(
    r: (
        String,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
    ),
) -> Result<AgentRow> {
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
