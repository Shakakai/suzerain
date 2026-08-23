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
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
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
/// Outer bound for a full provision+start. Individual driver commands have
/// their own timeouts (360s), but the sequence as a whole must also be
/// bounded: an unbounded hang wedges the agent in `provisioning` forever —
/// no monitor, no failure event, stale registry state (the 2026-08-12
/// my-agent incident). Cold provision is ~60s; this is generous.
const PROVISION_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// Live activity facts for a running agent — the daemon's ground truth for
/// auto-suspend decisions made by the control plane. `last` is bumped on
/// every pi event, inbound prompt, and attach session; `busy` latches while
/// a turn is in flight. An agent mid-turn (e.g. a 30-minute test run emits
/// events continuously) is therefore never mistaken for idle.
pub struct Activity {
    last: StdRwLock<(Instant, OffsetDateTime)>,
    busy: AtomicBool,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            last: StdRwLock::new((Instant::now(), OffsetDateTime::now_utc())),
            busy: AtomicBool::new(false),
        }
    }
}

impl Activity {
    pub fn note(&self) {
        *self.last.write().unwrap() = (Instant::now(), OffsetDateTime::now_utc());
    }

    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
        if busy {
            self.note();
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn idle_secs(&self) -> u64 {
        self.last.read().unwrap().0.elapsed().as_secs()
    }

    pub fn last_wall(&self) -> OffsetDateTime {
        self.last.read().unwrap().1
    }

    pub fn last_wall_rfc3339(&self) -> String {
        self.last_wall()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "unknown".into())
    }
}

pub struct RunningAgent {
    pub record: AgentRecord,
    pi: RwLock<Arc<PiAgent>>,
    driver: RwLock<Arc<DriverClient>>,
    placeholders: StdRwLock<BTreeMap<String, String>>,
    pub journal: Arc<Journal>,
    pub activity: Activity,
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
        self.activity.note();
        self.pi().await.prompt(message).await
    }

    pub async fn abort(&self) -> Result<()> {
        self.activity.note();
        self.pi().await.abort().await
    }

    pub async fn last_text(&self) -> Result<Option<String>> {
        self.pi().await.get_last_assistant_text().await
    }
}

#[derive(Clone)]
pub struct Supervisor {
    running: Arc<Mutex<HashMap<Uuid, Arc<RunningAgent>>>>,
    /// Agents with a provision/start in flight (between "no running entry"
    /// and "running entry inserted"). The journal pruner must treat these
    /// as live: their freshly opened Journal appends into the file the
    /// pruner would otherwise detach mid-wake.
    lifecycle_in_flight: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    /// Agent lifecycle transitions, for state reporting to suzerain (G2).
    state_events: broadcast::Sender<suzerain_protocol::AgentStateEntry>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        let (state_events, _) = broadcast::channel(256);
        Self {
            running: Arc::new(Mutex::new(HashMap::new())),
            lifecycle_in_flight: Arc::new(Mutex::new(std::collections::HashSet::new())),
            state_events,
        }
    }

    /// Lifecycle transitions of all agents on this daemon.
    pub fn subscribe_state_events(
        &self,
    ) -> broadcast::Receiver<suzerain_protocol::AgentStateEntry> {
        self.state_events.subscribe()
    }

    fn report_state(&self, record: &AgentRecord) {
        let _ = self.state_events.send(suzerain_protocol::AgentStateEntry {
            agent_id: record.id,
            name: record.name.clone(),
            state: record.state,
            // Enriched with live idle/busy facts by the state reporter.
            idle_secs: None,
            busy: None,
            session_file: record.session_file.clone(),
        });
    }

    pub async fn running(&self, id: &Uuid) -> Option<Arc<RunningAgent>> {
        self.running.lock().await.get(id).cloned()
    }

    /// True while an agent has an open journal that must not be detached:
    /// running, or mid-provision (the running entry is only inserted after
    /// the VM boots, so the provision window needs its own marker).
    pub async fn lifecycle_busy(&self, id: &Uuid) -> bool {
        self.running.lock().await.contains_key(id)
            || self.lifecycle_in_flight.lock().await.contains(id)
    }

    /// Bump an agent's activity clock (attach opened, inbound message).
    pub async fn touch(&self, id: &Uuid) {
        if let Some(r) = self.running(id).await {
            r.activity.note();
        }
    }

    /// Live (idle_secs, busy) for a running agent; None if not running.
    pub async fn activity(&self, id: &Uuid) -> Option<(u64, bool)> {
        self.running(id)
            .await
            .map(|r| (r.activity.idle_secs(), r.activity.is_busy()))
    }

    /// Periodically flush activity clocks to disk so the inactivity timer
    /// survives a daemon restart.
    pub fn spawn_activity_flusher(self: &Arc<Self>) {
        let sup = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let ids: Vec<Uuid> = sup.running.lock().await.keys().cloned().collect();
                for id in ids {
                    let Some(r) = sup.running(&id).await else {
                        continue;
                    };
                    if let Ok(mut rec) = state::load(&id).await {
                        rec.last_activity_at = Some(r.activity.last_wall_rfc3339());
                        state::save(&rec).await.ok();
                    }
                }
            }
        });
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
            last_activity_at: None,
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
                self.report_state(&rec);
                Ok(rec)
            }
            Err(err) => {
                let mut rec = record;
                rec.state = AgentState::Failed;
                state::save(&rec).await.ok();
                self.report_state(&rec);
                Err(err.context("provisioning failed"))
            }
        }
    }

    /// Start an existing (stopped/suspended) agent.
    ///
    /// `force`: if the supervisor still holds a running entry (believes the
    /// agent is running) but the agent is wedged/unresponsive, tear the
    /// entry down first and start fresh instead of refusing with
    /// "already running".
    pub async fn start(&self, id_or_name: &str, force: bool) -> Result<AgentRecord> {
        let mut record = state::find(id_or_name).await?;
        if let Some(running) = self.running(&record.id).await {
            if !force {
                bail!("agent '{}' is already running", record.name);
            }
            // Force-restart: the running entry is presumed wedged (that's
            // the only legitimate reason to force). Stop the monitor from
            // respawning, close the driver/VM, drop the entry, then fall
            // through to a clean provision+start.
            warn!(agent = %record.name, "force-start: tearing down stale running entry");
            running.stopping.store(true, Ordering::SeqCst);
            let _ = running.abort().await;
            running
                .journal
                .append("force_restart", serde_json::json!({}))
                .await
                .ok();
            if let Err(err) = running.driver().await.close().await {
                warn!(agent = %record.name, "force-start driver close failed: {err:#}");
            }
            self.running.lock().await.remove(&record.id);
        }
        self.provision_and_start(record.clone()).await?;
        record = state::load(&record.id).await.unwrap_or(record);
        Ok(record)
    }

    async fn provision_and_start(&self, record: AgentRecord) -> Result<()> {
        self.lifecycle_in_flight.lock().await.insert(record.id);
        let paths0 = AgentPaths::for_agent(&record.id);
        let journal0 = Arc::new(Journal::open(&paths0.root, record.id).await?);
        let inner = self.provision_and_start_inner(record.clone(), &journal0);
        let result = match tokio::time::timeout(PROVISION_TIMEOUT, inner).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "provisioning timed out after {}s (VM boot hung? check host memory pressure)",
                PROVISION_TIMEOUT.as_secs()
            )),
        };
        self.lifecycle_in_flight.lock().await.remove(&record.id);
        if let Err(err) = result {
            journal0
                .append("failed", serde_json::json!({"reason": format!("{err:#}")}))
                .await
                .ok();
            // Mark + report the failure for EVERY caller (create already did
            // this itself; start/restore left the record stale — a key part
            // of the wedge: registry said "active" while the agent was dead).
            let mut rec = record;
            rec.state = AgentState::Failed;
            state::save(&rec).await.ok();
            self.report_state(&rec);
            return Err(err.context("provisioning failed"));
        }
        Ok(())
    }

    async fn provision_and_start_inner(
        &self,
        mut record: AgentRecord,
        journal: &Arc<Journal>,
    ) -> Result<()> {
        let paths = AgentPaths::for_agent(&record.id);
        journal
            .append("state", serde_json::json!({"state": "provisioning"}))
            .await?;

        let driver = DriverClient::spawn().await?;
        let bundle = crate::secrets::get(&record.id).with_context(|| {
            format!(
                "no secret bundle for '{}' — start it via the control plane so secrets can be re-pulled",
                record.name
            )
        })?;
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
            let p = boot_vm(
                &driver,
                &paths,
                &record.name,
                Some(&checkpoint),
                &bundle,
                &egress,
                &git_hosts,
                &record.manifest.resources,
            )
            .await?;
            // The snapshot already carries the ssh config — refresh it
            // idempotently in case the bundle changed while suspended.
            provision::configure_git_ssh(&driver, &bundle).await?;
            p
        } else if !provisioned {
            let p = provision::provision(&driver, &record, &bundle).await?;
            std::fs::write(paths.root.join(".provisioned"), rfc3339_now())?;
            p
        } else {
            // VM is disposable but boot is still required; provisioning output
            // persists on the host mount, so just boot. The rootfs is fresh,
            // though — re-point guest git/ssh at the host-side proxy.
            let p = boot_vm(
                &driver,
                &paths,
                &record.name,
                None,
                &bundle,
                &egress,
                &git_hosts,
                &record.manifest.resources,
            )
            .await?;
            provision::configure_git_ssh(&driver, &bundle).await?;
            p
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
            activity: Activity::default(),
            stopping: AtomicBool::new(false),
            restart_lock: Mutex::new(()),
        });
        self.running.lock().await.insert(record.id, running.clone());

        // Record the pi session file for future resume. A CHANGED session
        // file means a new session era began (fresh spawn or rotation on
        // wake) — journal it so conversation logs can be segmented.
        if let Ok(state_resp) = pi.get_state().await {
            if let Some(file) = state_resp["sessionFile"].as_str() {
                if record.session_file.as_deref() != Some(file) {
                    journal
                        .append("session_started", serde_json::json!({"session_file": file}))
                        .await
                        .ok();
                }
                record.session_file = Some(file.to_string());
                record.state = AgentState::Active;
                state::save(&record).await?;
                self.report_state(&record);
            }
        }

        self.spawn_monitor(running);
        Ok(())
    }

    /// Event monitor: journals everything; on crash, respawns with backoff
    /// and crash-loop detection (see module docs).
    fn spawn_monitor(&self, agent: Arc<RunningAgent>) {
        let running_map = Arc::clone(&self.running);
        let state_events = self.state_events.clone();
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
                        // Every event is activity; a turn in flight latches
                        // busy so long quiet workloads are never "idle".
                        agent.activity.note();
                        match kind.as_str() {
                            "turn_start" => agent.activity.set_busy(true),
                            "agent_end" | "agent_settled" | "pi_exit" | "driver_died" => {
                                agent.activity.set_busy(false)
                            }
                            _ => {}
                        }
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
                        match restart_with_backoff(
                            &agent,
                            &journal,
                            &state_events,
                            &mut restart_times,
                        )
                        .await
                        {
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
            last_activity_at: None,
        };
        state::save(&record).await?;
        self.provision_and_start(record.clone()).await?;
        let record = state::load(&agent_id).await.unwrap_or(record);
        Ok(record)
    }

    /// Graceful stop: abort current work (cleanup window), close the VM.
    /// Local/destroy path: no session rotation, no checkpoint.
    pub async fn stop(&self, id_or_name: &str) -> Result<()> {
        self.prepare_suspend(id_or_name).await?;
        self.finish_suspend(id_or_name, false, false).await
    }

    /// Guard for auto-suspend / preemption: re-validate ground truth at
    /// execution time. The control plane's idle/busy view can be ~60s
    /// stale; refuse ("busy") if the agent is mid-turn or saw activity
    /// after `not_since` (RFC3339).
    pub async fn check_suspendable(&self, id_or_name: &str, not_since: Option<&str>) -> Result<()> {
        let record = state::find(id_or_name).await?;
        if let Some(running) = self.running(&record.id).await {
            if running.activity.is_busy() {
                bail!("busy: agent '{}' has a turn in flight", record.name);
            }
            if let Some(ns) = not_since {
                if let Ok(ns) = OffsetDateTime::parse(ns, &Rfc3339) {
                    if running.activity.last_wall() > ns {
                        bail!("busy: agent '{}' saw activity after {ns}", record.name);
                    }
                }
            }
        }
        Ok(())
    }

    /// Suspend phase 1: intentional-exit flag, journal, abort, cleanup
    /// window. The VM stays up; between prepare and finish the caller
    /// uploads the restore bundle (the session must be preserved centrally
    /// BEFORE finish rotates it off the guest disk).
    pub async fn prepare_suspend(&self, id_or_name: &str) -> Result<()> {
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
        Ok(())
    }

    /// Suspend phase 2: optional session rotation, optional VM disk
    /// checkpoint, close, mark suspended.
    ///
    /// `rotate_session`: remove the pi session files from the guest disk
    /// (they were uploaded to the control plane in full before this call)
    /// and clear the record's session pointer — the next wake starts a
    /// fresh pi session. Rotation happens on every control-plane suspend.
    pub async fn finish_suspend(
        &self,
        id_or_name: &str,
        checkpoint: bool,
        rotate_session: bool,
    ) -> Result<()> {
        let record = state::find(id_or_name).await?;
        let name = record.name.clone();
        let running = self
            .running(&record.id)
            .await
            .with_context(|| format!("agent '{name}' is not running"))?;
        let paths = AgentPaths::for_agent(&record.id);
        let mut record = record;

        if rotate_session {
            // Delete session files from the host-mounted guest dir BEFORE
            // the checkpoint, so the disk snapshot doesn't carry them.
            if paths.sessions.is_dir() {
                for entry in std::fs::read_dir(&paths.sessions)? {
                    let entry = entry?;
                    if entry.path().is_file() {
                        std::fs::remove_file(entry.path()).ok();
                    }
                }
            }
            record.session_file = None;
            // Persist immediately: if the checkpoint/close below fails, the
            // record must not point at a session file that no longer exists.
            state::save(&record).await?;
            running
                .journal
                .append("session_rotated", serde_json::json!({}))
                .await
                .ok();
        }

        let mut checkpoint_path = None;
        if checkpoint {
            let path = paths.checkpoint_path();
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
        record.state = AgentState::Suspended;
        // Persist the final activity timestamp with the suspended record so
        // the control plane's idle clock continues across the suspension.
        record.last_activity_at = Some(running.activity.last_wall_rfc3339());
        if let Some(p) = checkpoint_path {
            record.checkpoint = Some(p);
        }
        state::save(&record).await?;
        self.report_state(&record);
        info!(agent = %name, checkpoint, rotate_session, "stopped");
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
        let mut tombstone = record;
        tombstone.state = AgentState::Decommissioned;
        self.report_state(&tombstone);
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
#[allow(clippy::too_many_arguments)]
async fn boot_vm(
    driver: &DriverClient,
    paths: &AgentPaths,
    name: &str,
    checkpoint: Option<&str>,
    bundle: &suzerain_protocol::secrets::SecretBundle,
    egress: &[String],
    git_hosts: &[String],
    resources: &suzerain_protocol::manifest::Resources,
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
            resources,
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
    state_events: &broadcast::Sender<suzerain_protocol::AgentStateEntry>,
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
                let _ = state_events.send(suzerain_protocol::AgentStateEntry {
                    agent_id: rec.id,
                    name: rec.name.clone(),
                    state: rec.state,
                    idle_secs: None,
                    busy: None,
                    session_file: rec.session_file.clone(),
                });
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
        let bundle = crate::secrets::get(&record.id).unwrap_or_default();
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
            &record.manifest.resources,
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

    // Refresh the recorded session file for the next resume. (Crash
    // respawn resumes the same session, so no session_started event here
    // unless pi actually rolled to a new file.)
    if let Ok(state_resp) = pi.get_state().await {
        if let Some(file) = state_resp["sessionFile"].as_str() {
            if let Ok(mut rec) = state::load(&record.id).await {
                if rec.session_file.as_deref() != Some(file) {
                    agent
                        .journal
                        .append("session_started", serde_json::json!({"session_file": file}))
                        .await
                        .ok();
                }
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
