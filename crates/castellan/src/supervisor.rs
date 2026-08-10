//! Agent supervision: owns running agents (driver + VM + pi process), the
//! event pump into the journal, **crash respawn with exponential backoff and
//! crash-loop detection (G1)**, and graceful stop/destroy. Driven by the
//! daemon socket server in Phase 1; by suzerain orders in Phase 2.
//!
//! Respawn policy: a pi process exit (`pi_exit`) respawns just pi inside the
//! live VM; a driver/VM death (`driver_died`) re-boots the VM and resumes the
//! session. More than MAX_RESTARTS inside RESTART_WINDOW marks the agent
//! Failed instead of looping forever. Deliberate stops set `stopping` first,
//! so the monitor never respawns those exits.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::Value;
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::AgentState;
use tokio::sync::{broadcast, Mutex, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::driver::DriverClient;
use crate::journal::{rfc3339_now, Journal};
use crate::provision;
use crate::rpc::PiAgent;
use crate::state::{self, AgentPaths, AgentRecord};

const MAX_RESTARTS: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(600);
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

pub struct RunningAgent {
    pub record: AgentRecord,
    pi: RwLock<Arc<PiAgent>>,
    driver: RwLock<Arc<DriverClient>>,
    placeholders: StdRwLock<BTreeMap<String, String>>,
    pub journal: Arc<Journal>,
    stopping: AtomicBool,
    restart_lock: Mutex<()>,
}

impl RunningAgent {
    pub async fn pi(&self) -> Arc<PiAgent> {
        self.pi.read().await.clone()
    }

    pub async fn driver(&self) -> Arc<DriverClient> {
        self.driver.read().await.clone()
    }

    pub async fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.pi().await.subscribe()
    }

    pub async fn prompt(&self, message: &str) -> Result<()> {
        self.pi().await.prompt(message).await
    }

    pub async fn abort(&self) -> Result<()> {
        self.pi().await.abort().await
    }

    pub async fn last_text(&self) -> Result<Option<String>> {
        self.pi().await.get_last_assistant_text().await
    }
}

#[derive(Default, Clone)]
pub struct Supervisor {
    running: Arc<Mutex<HashMap<Uuid, Arc<RunningAgent>>>>,
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
            boot_vm(
                &driver,
                &paths,
                &record.name,
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
            boot_vm(
                &driver,
                &paths,
                &record.name,
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
            pi: RwLock::new(pi.clone()),
            driver: RwLock::new(driver.clone()),
            placeholders: StdRwLock::new(placeholders),
            journal: journal.clone(),
            stopping: AtomicBool::new(false),
            restart_lock: Mutex::new(()),
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

        self.spawn_monitor(running);
        Ok(())
    }

    /// Event monitor: journals everything; on crash, respawns with backoff
    /// and crash-loop detection (see module docs).
    fn spawn_monitor(&self, agent: Arc<RunningAgent>) {
        let running_map = Arc::clone(&self.running);
        tokio::spawn(async move {
            let name = agent.record.name.clone();
            let agent_id = agent.record.id;
            let journal = agent.journal.clone();
            let mut rx = agent.subscribe().await;
            let mut restart_times: VecDeque<Instant> = VecDeque::new();

            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let kind = event["type"].as_str().unwrap_or("unknown").to_string();
                        let is_crash = kind == "pi_exit" || kind == "driver_died";
                        if let Err(err) = journal.append(&kind, event).await {
                            error!(agent = %name, "journal append failed: {err}");
                        }
                        if !is_crash {
                            continue;
                        }
                        if agent.stopping.load(Ordering::SeqCst) {
                            break;
                        }
                        warn!(agent = %name, kind = %kind, "agent crashed");
                        match restart_with_backoff(&agent, &journal, &mut restart_times).await {
                            Some(new_rx) => rx = new_rx,
                            None => {
                                running_map.lock().await.remove(&agent_id);
                                break; // crash loop: marked Failed inside
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(agent = %name, "event pump lagged by {n}");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
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

        // Tell the monitor this exit is intentional BEFORE any shutdown work.
        running.stopping.store(true, Ordering::SeqCst);

        running
            .journal
            .append("stopping", serde_json::json!({}))
            .await?;
        // Cleanup window: let the agent finish its current turn.
        let _ = running.abort().await;
        tokio::time::sleep(Duration::from_secs(1)).await;

        let mut checkpoint_path = None;
        if checkpoint {
            let path = AgentPaths::for_agent(&record.id).checkpoint_path();
            let path_str = path.to_string_lossy().to_string();
            match running.driver().await.checkpoint(&path_str).await {
                Ok(p) => {
                    info!(agent = %name, path = %p, "checkpoint written");
                    checkpoint_path = Some(p);
                }
                Err(err) => warn!(agent = %name, "checkpoint failed (plain stop): {err:#}"),
            }
        }

        running.driver().await.close().await?;
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
        running.prompt(message).await
    }

    /// Subscribe to a running agent's event stream (for attach).
    pub async fn subscribe(&self, id_or_name: &str) -> Result<broadcast::Receiver<Value>> {
        let record = state::find(id_or_name).await?;
        let name = record.name.clone();
        let running = self
            .running(&record.id)
            .await
            .with_context(|| format!("agent '{name}' is not running"))?;
        Ok(running.subscribe().await)
    }
}

/// Boot (or resume) the agent VM with secrets/egress wired; returns the
/// placeholder env map for the agent's secrets.
async fn boot_vm(
    driver: &DriverClient,
    paths: &AgentPaths,
    name: &str,
    checkpoint: Option<&str>,
    bundle: &suzerain_protocol::secrets::SecretBundle,
    egress: &[String],
    git_hosts: &[String],
) -> Result<BTreeMap<String, String>> {
    driver
        .boot(
            &[("/agent".into(), paths.guest.to_string_lossy().into())],
            &[],
            &format!("castellan-{name}"),
            checkpoint,
            bundle,
            egress,
            git_hosts,
        )
        .await
}

/// Restart an agent after a crash with exponential backoff. First attempt
/// is pi-only (VM presumed alive); retries escalate to a full VM re-boot.
/// Returns the new event receiver, or None after the crash-loop cap — in
/// which case the agent is marked Failed.
async fn restart_with_backoff(
    agent: &Arc<RunningAgent>,
    journal: &Arc<Journal>,
    restart_times: &mut VecDeque<Instant>,
) -> Option<broadcast::Receiver<Value>> {
    let name = &agent.record.name;
    let agent_id = agent.record.id;
    let _guard = agent.restart_lock.lock().await;
    let mut backoff = INITIAL_BACKOFF;
    let mut attempt = 0usize;
    loop {
        while restart_times
            .front()
            .map(|t| t.elapsed() > RESTART_WINDOW)
            .unwrap_or(false)
        {
            restart_times.pop_front();
        }
        if restart_times.len() >= MAX_RESTARTS {
            error!(agent = %name, "crash loop detected; marking agent failed");
            journal
                .append("failed", serde_json::json!({"reason": "crash_loop"}))
                .await
                .ok();
            if let Ok(mut rec) = state::load(&agent_id).await {
                rec.state = AgentState::Failed;
                state::save(&rec).await.ok();
            }
            return None;
        }
        restart_times.push_back(Instant::now());
        journal
            .append(
                "respawning",
                serde_json::json!({"attempt": attempt + 1, "backoff_secs": backoff.as_secs()}),
            )
            .await
            .ok();
        tokio::time::sleep(backoff).await;
        match respawn(agent, attempt > 0).await {
            Ok(new_rx) => {
                info!(agent = %name, "agent respawned");
                journal
                    .append("respawned", serde_json::json!({}))
                    .await
                    .ok();
                return Some(new_rx);
            }
            Err(err) => {
                warn!(agent = %name, "respawn failed: {err:#}");
                attempt += 1;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Respawn the agent after a crash. `full_reboot` replaces the driver and
/// boots a fresh VM; otherwise pi is simply re-spawned inside the live VM.
/// In both cases pi resumes its recorded session.
async fn respawn(
    agent: &Arc<RunningAgent>,
    full_reboot: bool,
) -> Result<broadcast::Receiver<Value>> {
    // Refresh the record: session file advances over the agent's life.
    let record = state::load(&agent.record.id)
        .await
        .unwrap_or_else(|_| agent.record.clone());
    let paths = AgentPaths::for_agent(&record.id);

    let driver = if full_reboot {
        let d = DriverClient::spawn().await?;
        let bundle = state::load_bundle(&record.id).await.unwrap_or_default();
        let egress = provision::egress_hosts(&record, &bundle);
        let git_hosts = provision::git_hosts(&record);
        let checkpoint = record
            .checkpoint
            .as_ref()
            .filter(|p| std::path::Path::new(p).exists())
            .cloned();
        let placeholders = boot_vm(
            &d,
            &paths,
            &record.name,
            checkpoint.as_deref(),
            &bundle,
            &egress,
            &git_hosts,
        )
        .await?;
        *agent.placeholders.write().unwrap() = placeholders;
        *agent.driver.write().await = d.clone();
        d
    } else {
        agent.driver().await
    };

    let placeholders = agent.placeholders.read().unwrap().clone();
    let env = provision::pi_spawn_env(&record, &placeholders);
    let pi = PiAgent::spawn(
        driver,
        "/agent/workspace",
        &env,
        &record.manifest.model.provider,
        &record.manifest.model.id,
        record.session_file.as_deref(),
    )
    .await?;

    // Refresh the recorded session file for the next resume.
    if let Ok(state_resp) = pi.get_state().await {
        if let Some(file) = state_resp["sessionFile"].as_str() {
            if let Ok(mut rec) = state::load(&record.id).await {
                rec.session_file = Some(file.to_string());
                rec.state = AgentState::Active;
                state::save(&rec).await?;
            }
        }
    }

    let rx = pi.subscribe();
    *agent.pi.write().await = pi;
    Ok(rx)
}
