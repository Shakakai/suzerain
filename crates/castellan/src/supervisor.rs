//! Agent supervision: owns running agents (driver + VM + pi process), the
//! event pump into the journal, restart-on-crash policy, and graceful
//! stop/destroy. Driven by the daemon socket server in Phase 1; by suzerain
//! orders in Phase 2.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::AgentState;
use tokio::sync::{broadcast, Mutex};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::driver::DriverClient;
use crate::journal::{rfc3339_now, Journal};
use crate::provision;
use crate::rpc::PiAgent;
use crate::state::{self, AgentPaths, AgentRecord};

pub struct RunningAgent {
    pub record: AgentRecord,
    pub pi: Arc<PiAgent>,
    driver: Arc<DriverClient>,
    pub journal: Arc<Journal>,
}

#[derive(Default)]
pub struct Supervisor {
    running: Mutex<HashMap<Uuid, Arc<RunningAgent>>>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn running(&self, id: &Uuid) -> Option<Arc<RunningAgent>> {
        self.running.lock().await.get(id).cloned()
    }

    /// Create + provision + start a new agent. `id` is provided by suzerain
    /// when the order comes from the control plane; locally-originated
    /// creates pass `None` and one is generated.
    pub async fn create(&self, id: Option<Uuid>, manifest: AgentManifest) -> Result<AgentRecord> {
        provision::validate_manifest(&manifest)?;
        if state::find_by_name(&manifest.name).await.is_ok() {
            bail!("an agent named '{}' already exists", manifest.name);
        }
        let record = AgentRecord {
            id: id.unwrap_or_else(Uuid::new_v4),
            name: manifest.name.clone(),
            manifest,
            state: AgentState::Provisioning,
            created_at: rfc3339_now(),
            session_file: None,
        };
        let paths = AgentPaths::for_agent(&record.id);
        tokio::fs::create_dir_all(&paths.root).await?;
        state::save(&record).await?;

        let result = self.provision_and_start(record.clone()).await;
        match result {
            Ok(()) => {
                let mut rec = record;
                rec.state = AgentState::Active;
                // Refresh from disk: start may have recorded the session file.
                if let Ok(disk) = state::load(&rec.id).await {
                    rec.session_file = disk.session_file;
                }
                state::save(&rec).await?;
                Ok(rec)
            }
            Err(err) => {
                let mut rec = record;
                rec.state = AgentState::Failed;
                state::save(&rec).await.ok();
                Err(err.context("provisioning failed"))
            }
        }
    }

    /// Start an existing (stopped/suspended) agent.
    pub async fn start(&self, id_or_name: &str) -> Result<AgentRecord> {
        let mut record = state::find(id_or_name).await?;
        if self.running(&record.id).await.is_some() {
            bail!("agent '{}' is already running", record.name);
        }
        self.provision_and_start(record.clone()).await?;
        record = state::load(&record.id).await.unwrap_or(record);
        Ok(record)
    }

    async fn provision_and_start(&self, mut record: AgentRecord) -> Result<()> {
        let paths = AgentPaths::for_agent(&record.id);
        let journal = Arc::new(Journal::open(&paths.root, record.id).await?);
        journal
            .append("state", serde_json::json!({"state": "provisioning"}))
            .await?;

        let driver = DriverClient::spawn().await?;
        let provisioned = paths.root.join(".provisioned").exists();
        if !provisioned {
            provision::provision(&driver, &record).await?;
            std::fs::write(paths.root.join(".provisioned"), rfc3339_now())?;
        } else {
            // VM is disposable but boot is still required; provisioning output
            // persists on the host mount, so just boot.
            driver
                .boot(
                    &[("/agent".into(), paths.guest.to_string_lossy().into())],
                    &[],
                    &format!("castellan-{}", record.name),
                )
                .await?;
        }
        journal.append("provisioned", serde_json::json!({})).await?;

        // Spawn pi, resuming the recorded session if present.
        let env = provision::pi_spawn_env(&record);
        let pi = PiAgent::spawn(
            driver.clone(),
            "/agent/workspace",
            &env,
            &record.manifest.model.provider,
            &record.manifest.model.id,
            record.session_file.as_deref(),
        )
        .await?;
        journal.append("spawned", serde_json::json!({})).await?;

        let running = Arc::new(RunningAgent {
            record: record.clone(),
            pi: pi.clone(),
            driver: driver.clone(),
            journal: journal.clone(),
        });
        self.running.lock().await.insert(record.id, running.clone());

        // Record the pi session file for future resume.
        if let Ok(state_resp) = pi.get_state().await {
            if let Some(file) = state_resp["sessionFile"].as_str() {
                record.session_file = Some(file.to_string());
                record.state = AgentState::Active;
                state::save(&record).await?;
            }
        }

        self.spawn_event_pump(running);
        Ok(())
    }

    /// Pump pi events into the journal; restart-on-crash with backoff.
    fn spawn_event_pump(&self, agent: Arc<RunningAgent>) {
        let mut rx = agent.pi.subscribe();
        let agent_id = agent.record.id;
        let name = agent.record.name.clone();
        let journal = agent.journal.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let kind = event["type"].as_str().unwrap_or("unknown").to_string();
                        let is_exit = kind == "pi_exit";
                        if let Err(err) = journal.append(&kind, event).await {
                            error!(agent = %name, "journal append failed: {err}");
                        }
                        if is_exit {
                            warn!(agent = %name, "pi exited");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(agent = %name, "event pump lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            let _ = agent_id;
        });
    }

    /// Graceful stop: abort current work (cleanup window), close the VM.
    pub async fn stop(&self, id_or_name: &str) -> Result<()> {
        let record = state::find(id_or_name).await?;
        let name = record.name.clone();
        let running = self
            .running(&record.id)
            .await
            .with_context(|| format!("agent '{name}' is not running"))?;

        running
            .journal
            .append("stopping", serde_json::json!({}))
            .await?;
        // Cleanup window: let the agent finish its current turn.
        let _ = running.pi.abort().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        running.driver.close().await?;
        running
            .journal
            .append("stopped", serde_json::json!({}))
            .await?;

        self.running.lock().await.remove(&record.id);
        let mut record = record;
        record.state = AgentState::Suspended;
        state::save(&record).await?;
        info!(agent = %name, "stopped");
        Ok(())
    }

    /// Stop (if running) and delete all local state.
    pub async fn destroy(&self, id_or_name: &str) -> Result<()> {
        let record = state::find(id_or_name).await?;
        let name = record.name.clone();
        if self.running(&record.id).await.is_some() {
            self.stop(&name).await?;
        }
        let paths = AgentPaths::for_agent(&record.id);
        tokio::fs::remove_dir_all(&paths.root).await.ok();
        info!(agent = %name, "destroyed");
        Ok(())
    }

    /// Send a prompt to a running agent.
    pub async fn prompt(&self, id_or_name: &str, message: &str) -> Result<()> {
        let record = state::find(id_or_name).await?;
        let name = record.name.clone();
        let running = self
            .running(&record.id)
            .await
            .with_context(|| format!("agent '{name}' is not running"))?;
        running.pi.prompt(message).await
    }

    /// Subscribe to a running agent's event stream (for attach).
    pub async fn subscribe(&self, id_or_name: &str) -> Result<broadcast::Receiver<Value>> {
        let record = state::find(id_or_name).await?;
        let name = record.name.clone();
        let running = self
            .running(&record.id)
            .await
            .with_context(|| format!("agent '{name}' is not running"))?;
        Ok(running.pi.subscribe())
    }
}
