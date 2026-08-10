//! Local agent registry: per-agent state persisted under the castellan data
//! dir. Phase 2 replaces the authoritative copy with suzerain's registry;
//! this stays as the daemon-local cache.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::AgentState;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: Uuid,
    pub name: String,
    pub manifest: AgentManifest,
    pub state: AgentState,
    pub created_at: String,
    /// pi session file path *inside the guest* (under /agent/sessions),
    /// recorded after first start for resume.
    #[serde(default)]
    pub session_file: Option<String>,
}

/// Root data dir for this daemon.
pub fn data_dir() -> PathBuf {
    std::env::var("CASTELLAN_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_home().join(".local/share/castellan"))
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn agent_dir(id: &Uuid) -> PathBuf {
    data_dir().join("agents").join(id.to_string())
}

/// Per-agent dir layout (host side; `guest/` is mounted at /agent in the VM).
pub struct AgentPaths {
    pub root: PathBuf,
    pub guest: PathBuf,
    pub workspace: PathBuf,
    pub pi_home: PathBuf,
    pub sessions: PathBuf,
    pub extensions: PathBuf,
}

impl AgentPaths {
    pub fn for_agent(id: &Uuid) -> Self {
        let root = agent_dir(id);
        let guest = root.join("guest");
        Self {
            workspace: guest.join("workspace"),
            pi_home: guest.join("pi-home"),
            sessions: guest.join("sessions"),
            extensions: guest.join("pi-home").join("extensions"),
            guest,
            root,
        }
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }
}

pub async fn save(record: &AgentRecord) -> Result<()> {
    let paths = AgentPaths::for_agent(&record.id);
    let tmp = paths.root.join("state.json.tmp");
    tokio::fs::write(&tmp, serde_json::to_string_pretty(record)?).await?;
    tokio::fs::rename(&tmp, paths.state_file()).await?;
    Ok(())
}

pub async fn load(id: &Uuid) -> Result<AgentRecord> {
    let path = AgentPaths::for_agent(id).state_file();
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

pub async fn list() -> Result<Vec<AgentRecord>> {
    let mut out = Vec::new();
    let agents = data_dir().join("agents");
    let mut entries = match tokio::fs::read_dir(&agents).await {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    while let Some(entry) = entries.next_entry().await? {
        let state = entry.path().join("state.json");
        if let Ok(text) = tokio::fs::read_to_string(&state).await {
            if let Ok(record) = serde_json::from_str::<AgentRecord>(&text) {
                out.push(record);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub async fn find_by_name(name: &str) -> Result<AgentRecord> {
    for record in list().await? {
        if record.name == name {
            return Ok(record);
        }
    }
    bail!("no agent named '{name}'")
}
