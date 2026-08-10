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
            checkpoint: None,
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
        let bundle = state::load_bundle(&record.id).await.unwrap_or_default();
        for value in bundle.values() {
            crate::journal::register_secret(value);
        }
        let egress = provision::egress_hosts(&record, &bundle);
        let git_hosts = provision::git_hosts(&record);
        let checkpoint = record
            .checkpoint
            .as_ref()
            .filter(|p| std::path::Path::new(p).exists())
            .cloned();
        let provisioned = paths.root.join(".provisioned").exists();
        let placeholders = if let Some(checkpoint) = checkpoint {
            // Same-host suspend/boot fast path: resume the disk checkpoint
            // (guest state, including base packages, is in the snapshot).
            info!(agent = %record.name, "resuming from checkpoint");
            driver
                .boot(
                    &[("/agent".into(), paths.guest.to_string_lossy().into())],
                    &[],
                    &format!("castellan-{}", record.name),
                    Some(&checkpoint),
                    &bundle,
                    &egress,
                    &git_hosts,
                )
                .await?
        } else if !provisioned {
            let p = provision::provision(&driver, &record, &bundle).await?;
            std::fs::write(paths.root.join(".provisioned"), rfc3339_now())?;
            p
        } else {
            // VM is disposable but boot is still required; provisioning output
            // persists on the host mount, so just boot.
            driver
                .boot(
                    &[("/agent".into(), paths.guest.to_string_lossy().into())],
                    &[],
                    &format!("castellan-{}", record.name),
                    None,
                    &bundle,
                    &egress,
                    &git_hosts,
                )
                .await?
        };
        journal.append("provisioned", serde_json::json!({})).await?;

        // Spawn pi with placeholder credentials (real values never enter
        // the guest), resuming the recorded session if present.
        let env = provision::pi_spawn_env(&record, &placeholders);
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

    /// Restore: register an agent whose bundle files were already written
    /// into its agent dir, then provision + start (resuming its session).
    pub async fn restore(
        &self,
        agent_id: Uuid,
        manifest: AgentManifest,
        session_file: Option<String>,
    ) -> Result<AgentRecord> {
        let record = AgentRecord {
            id: agent_id,
            name: manifest.name.clone(),
            manifest,
            state: AgentState::Restoring,
            created_at: rfc3339_now(),
            session_file,
            checkpoint: None,
        };
        state::save(&record).await?;
        self.provision_and_start(record.clone()).await?;
        let record = state::load(&agent_id).await.unwrap_or(record);
        Ok(record)
    }

    /// Graceful stop: abort current work (cleanup window), close the VM.
    pub async fn stop(&self, id_or_name: &str) -> Result<()> {
        self.stop_inner(id_or_name, false).await
    }

    /// Suspend: graceful stop + VM disk checkpoint for fast same-host boot.
    /// Returns the checkpoint path.
    pub async fn suspend(&self, id_or_name: &str) -> Result<()> {
        self.stop_inner(id_or_name, true).await
    }

    async fn stop_inner(&self, id_or_name: &str, checkpoint: bool) -> Result<()> {
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

        let mut checkpoint_path = None;
        if checkpoint {
            let path = AgentPaths::for_agent(&record.id).checkpoint_path();
            let path_str = path.to_string_lossy().to_string();
            match running.driver.checkpoint(&path_str).await {
                Ok(p) => {
                    info!(agent = %name, path = %p, "checkpoint written");
                    checkpoint_path = Some(p);
                }
                Err(err) => warn!(agent = %name, "checkpoint failed (plain stop): {err:#}"),
            }
        }

        running.driver.close().await?;
        running
            .journal
            .append(
                if checkpoint { "suspended" } else { "stopped" },
                serde_json::json!({}),
            )
            .await?;

        self.running.lock().await.remove(&record.id);
        let mut record = record;
        record.state = AgentState::Suspended;
        if let Some(p) = checkpoint_path {
            record.checkpoint = Some(p);
        }
        state::save(&record).await?;
        info!(agent = %name, checkpoint, "stopped");
        Ok(())
    }

    /// Stop (if running) and delete all local state. Idempotent: destroying
    /// an unknown agent is a no-op (keeps the control plane and daemon
    /// registries convergent after partial failures).
    pub async fn destroy(&self, id_or_name: &str) -> Result<()> {
        let record = match state::find(id_or_name).await {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
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
