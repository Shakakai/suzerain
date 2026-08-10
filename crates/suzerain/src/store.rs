//! SQLite-backed control-plane store: daemon registry (allowlist), agent
//! registry, and the event-log index. Log payloads live as append-only JSONL
//! files under the data dir; the DB indexes them.
//!
//! Zero-config default: a single SQLite file. The trait boundary is where a
//! Postgres backend lands later (docs/PLAN.md §9).

use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
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

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub async fn open() -> Result<Self> {
        let db_path: PathBuf = data_dir().join("suzerain.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS daemons (
                endpoint_id TEXT PRIMARY KEY,
                approved INTEGER NOT NULL DEFAULT 0,
                online INTEGER NOT NULL DEFAULT 0,
                hostname TEXT NOT NULL DEFAULT '',
                os TEXT NOT NULL DEFAULT '',
                arch TEXT NOT NULL DEFAULT '',
                labels TEXT NOT NULL DEFAULT '{}',
                max_agents INTEGER NOT NULL DEFAULT 4,
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
                acked_through INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL DEFAULT ''
            );",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── daemons ─────────────────────────────────────────────────────────

    pub async fn approve_daemon(&self, endpoint_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO daemons (endpoint_id, approved) VALUES (?, 1)
             ON CONFLICT(endpoint_id) DO UPDATE SET approved = 1",
        )
        .bind(endpoint_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn daemon_approved(&self, endpoint_id: &str) -> Result<bool> {
        let row = sqlx::query("SELECT approved FROM daemons WHERE endpoint_id = ?")
            .bind(endpoint_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<i64, _>(0) == 1).unwrap_or(false))
    }

    pub async fn upsert_daemon(&self, info: &DaemonInfo, online: bool) -> Result<()> {
        sqlx::query(
            "INSERT INTO daemons (endpoint_id, hostname, os, arch, labels, max_agents, online, last_seen)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET
                hostname = excluded.hostname, os = excluded.os, arch = excluded.arch,
                labels = excluded.labels, max_agents = excluded.max_agents,
                online = excluded.online, last_seen = excluded.last_seen",
        )
        .bind(&info.endpoint_id)
        .bind(&info.hostname)
        .bind(&info.os)
        .bind(&info.arch)
        .bind(serde_json::to_string(&info.labels)?)
        .bind(info.max_agents as i64)
        .bind(online)
        .bind(castellan_time_now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_daemon_online(&self, endpoint_id: &str, online: bool) -> Result<()> {
        sqlx::query("UPDATE daemons SET online = ?, last_seen = ? WHERE endpoint_id = ?")
            .bind(online)
            .bind(castellan_time_now())
            .bind(endpoint_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_daemons(&self) -> Result<Vec<DaemonRow>> {
        let rows = sqlx::query(
            "SELECT endpoint_id, approved, online, hostname, os, arch, labels, max_agents, last_seen
             FROM daemons ORDER BY endpoint_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DaemonRow {
                endpoint_id: r.get(0),
                approved: r.get::<i64, _>(1) == 1,
                online: r.get::<i64, _>(2) == 1,
                hostname: r.get(3),
                os: r.get(4),
                arch: r.get(5),
                labels: r.get(6),
                max_agents: r.get::<i64, _>(7) as u32,
                last_seen: r.get(8),
            })
            .collect())
    }

    // ── agents ──────────────────────────────────────────────────────────

    pub async fn create_agent(&self, row: &AgentRow) -> Result<()> {
        sqlx::query(
            "INSERT INTO agents (id, name, daemon_endpoint_id, manifest, state, created_at, session_file)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.id.to_string())
        .bind(&row.name)
        .bind(&row.daemon_endpoint_id)
        .bind(serde_json::to_string(&row.manifest)?)
        .bind(state_str(row.state))
        .bind(&row.created_at)
        .bind(&row.session_file)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_agent_state(&self, id: &Uuid, state: AgentState) -> Result<()> {
        sqlx::query("UPDATE agents SET state = ? WHERE id = ?")
            .bind(state_str(state))
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_agent_session_file(&self, id: &Uuid, session_file: &str) -> Result<()> {
        sqlx::query("UPDATE agents SET session_file = ? WHERE id = ?")
            .bind(session_file)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn set_agent_daemon(&self, id: &Uuid, daemon_endpoint_id: &str) -> Result<()> {
        sqlx::query("UPDATE agents SET daemon_endpoint_id = ? WHERE id = ?")
            .bind(daemon_endpoint_id)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn delete_agent(&self, id: &Uuid) -> Result<()> {
        sqlx::query("DELETE FROM agents WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentRow>> {
        let row = sqlx::query(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file
             FROM agents WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_agent).transpose()
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentRow>> {
        let rows = sqlx::query(
            "SELECT id, name, daemon_endpoint_id, manifest, state, created_at, session_file
             FROM agents ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(row_to_agent).collect()
    }

    // ── log index ───────────────────────────────────────────────────────

    pub async fn acked_through(&self, agent_id: &Uuid) -> Result<u64> {
        let row = sqlx::query("SELECT acked_through FROM log_index WHERE agent_id = ?")
            .bind(agent_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.get::<i64, _>(0) as u64).unwrap_or(0))
    }

    pub async fn set_acked_through(&self, agent_id: &Uuid, seq: u64) -> Result<()> {
        sqlx::query(
            "INSERT INTO log_index (agent_id, acked_through, updated_at) VALUES (?, ?, ?)
             ON CONFLICT(agent_id) DO UPDATE SET acked_through = excluded.acked_through,
             updated_at = excluded.updated_at",
        )
        .bind(agent_id.to_string())
        .bind(seq as i64)
        .bind(castellan_time_now())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn row_to_agent(r: sqlx::sqlite::SqliteRow) -> Result<AgentRow> {
    let id = Uuid::parse_str(r.get::<String, _>(0).as_str())?;
    let manifest: AgentManifest = serde_json::from_str(r.get::<String, _>(3).as_str())?;
    let state = parse_state(r.get::<String, _>(4).as_str());
    Ok(AgentRow {
        id,
        name: r.get(1),
        daemon_endpoint_id: r.get(2),
        manifest,
        state,
        created_at: r.get(5),
        session_file: r.get(6),
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
