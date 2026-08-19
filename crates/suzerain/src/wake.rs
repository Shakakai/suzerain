//! Transparent wake: durable message queue + wake orchestration (the
//! "Activator" of the auto-suspend design). Messages addressed to
//! sleeping/failed agents are persisted (`pending_messages`), coalesced
//! while a wake is in flight, and delivered as one batch once the agent is
//! Active. Wakes prefer the same-daemon checkpoint fast path, fall back to
//! scheduler placement + bundle restore, and retry excluding daemons that
//! already failed. Terminal failure flags the agent `needs_attention`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use suzerain_protocol::control::{AttachMessage, StreamHello};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use suzerain_protocol::order::Order;
use suzerain_protocol::state::AgentState;
use tokio::sync::{broadcast, Mutex, Notify};
use tracing::{info, warn};
use uuid::Uuid;

use crate::control::ControlPlane;
use crate::store::AgentRow;

/// Bound on one wake attempt phase (order or bundle restore) so a wedged
/// daemon can't hang a wake forever; the next attempt tries elsewhere.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(240);
/// Bound on callers waiting for a wake (ask/attach/chat have their own
/// 300s budgets; the wake must fit inside them).
const WAIT_TIMEOUT: Duration = Duration::from_secs(280);

/// Progress notifications for UIs/waiters (synthetic chat status lines).
#[derive(Debug, Clone, Serialize)]
pub struct WakeEvent {
    pub agent_id: Uuid,
    /// queued | starting | restoring | ready | failed
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

pub struct WakeService {
    inflight: Mutex<HashMap<Uuid, Arc<WakeAttempt>>>,
    events: broadcast::Sender<WakeEvent>,
}

/// One shared wake attempt; any number of waiters join it.
pub struct WakeAttempt {
    done: Notify,
    outcome: Mutex<Option<Result<(), String>>>,
}

impl WakeAttempt {
    fn new() -> Self {
        Self {
            done: Notify::new(),
            outcome: Mutex::new(None),
        }
    }

    async fn complete(&self, result: Result<(), String>) {
        *self.outcome.lock().await = Some(result);
        self.done.notify_waiters();
    }

    pub async fn wait(&self) -> Result<(), String> {
        loop {
            let notified = self.done.notified();
            if let Some(outcome) = self.outcome.lock().await.clone() {
                return outcome;
            }
            notified.await;
        }
    }
}

impl Default for WakeService {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeService {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            inflight: Mutex::new(HashMap::new()),
            events,
        }
    }

    /// Subscribe to wake progress (web chat synthetic status lines).
    pub fn subscribe(&self) -> broadcast::Receiver<WakeEvent> {
        self.events.subscribe()
    }

    pub fn emit(&self, agent_id: Uuid, status: &str, detail: Option<String>) {
        let _ = self.events.send(WakeEvent {
            agent_id,
            status: status.to_string(),
            detail,
        });
    }

    /// Ensure a wake is in flight for the agent; returns the shared
    /// attempt to await. Idempotent: concurrent callers join the same
    /// attempt (their queued messages coalesce into one batch).
    pub async fn ensure(
        self: &Arc<Self>,
        cp: &Arc<ControlPlane>,
        agent: &AgentRow,
    ) -> Arc<WakeAttempt> {
        let mut map = self.inflight.lock().await;
        if let Some(existing) = map.get(&agent.id) {
            return Arc::clone(existing);
        }
        let attempt = Arc::new(WakeAttempt::new());
        map.insert(agent.id, Arc::clone(&attempt));
        let this = Arc::clone(self);
        let cp = Arc::clone(cp);
        let agent = agent.clone();
        let att = Arc::clone(&attempt);
        tokio::spawn(async move {
            let result = wake_agent(&cp, &agent, &this).await;
            att.complete(result.map_err(|e| format!("{e:#}"))).await;
            this.inflight.lock().await.remove(&agent.id);
        });
        attempt
    }
}

/// True when the agent is Active and its daemon is reachable.
pub async fn is_awake(cp: &ControlPlane, agent: &AgentRow) -> bool {
    if agent.state != AgentState::Active {
        return false;
    }
    let Ok(daemon) = agent.daemon_endpoint_id.parse::<iroh::EndpointId>() else {
        return false;
    };
    cp.session(&daemon).await.is_some()
}

/// Deliver a message to an agent, waking it transparently if needed.
///
/// Returns `true` when the message was **queued** (the wake task delivers
/// it — the caller must NOT prompt again); `false` means the agent is
/// awake and the caller should use the direct attach path.
pub async fn deliver_message(
    cp: &Arc<ControlPlane>,
    agent: &AgentRow,
    message: &str,
) -> Result<bool> {
    if is_awake(cp, agent).await {
        return Ok(false);
    }
    if agent.state == AgentState::Failed {
        // Message to a failed agent: queue it and attempt a wake; a
        // terminal failure flags the agent for human intervention.
        info!(agent = %agent.name, "queueing message for failed agent; attempting wake");
    }
    cp.store().enqueue_message(&agent.id, message).await?;
    // Race guard: the agent may have woken between our is_awake check and
    // the enqueue (or a joined wake may have drained before our message
    // landed). If it's awake now, flush directly; otherwise wait.
    if is_awake(cp, agent).await {
        flush_pending(cp, agent).await?;
        return Ok(true);
    }
    let attempt = cp.wake().ensure(cp, agent).await;
    tokio::time::timeout(WAIT_TIMEOUT, attempt.wait())
        .await
        .context("timed out waiting for the agent to wake")?
        .map_err(anyhow::Error::msg)?;
    // Post-wake: our message was normally delivered by the wake task, but
    // if it landed after the task's drain, deliver it now.
    flush_pending(cp, agent).await?;
    Ok(true)
}

/// Ensure the agent is awake without sending a message (attach path).
pub async fn ensure_awake(cp: &Arc<ControlPlane>, agent: &AgentRow) -> Result<()> {
    if is_awake(cp, agent).await {
        return Ok(());
    }
    let attempt = cp.wake().ensure(cp, agent).await;
    tokio::time::timeout(WAIT_TIMEOUT, attempt.wait())
        .await
        .context("timed out waiting for the agent to wake")?
        .map_err(anyhow::Error::msg)
}

/// Boot recovery: resume wakes for agents with undelivered messages.
pub async fn resume_pending(cp: &Arc<ControlPlane>) {
    let ids = cp
        .store()
        .agents_with_pending_messages()
        .await
        .unwrap_or_default();
    for id in ids {
        match cp.store().get_agent(&id).await {
            Ok(Some(agent)) => {
                info!(agent = %agent.name, "resuming pending wake after restart");
                cp.wake().ensure(cp, &agent).await;
            }
            // Agent gone: its queued messages can never be delivered.
            Ok(None) => {
                let pending = cp.store().pending_messages(&id).await.unwrap_or_default();
                let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
                cp.store()
                    .set_message_status(&ids, "failed", Some("agent no longer exists"))
                    .await
                    .ok();
            }
            Err(err) => warn!("resume pending wake failed: {err:#}"),
        }
    }
}

// ── orchestration ─────────────────────────────────────────────────────────

async fn daemon_online(cp: &ControlPlane, endpoint_id: &str) -> bool {
    match endpoint_id.parse() {
        Ok(id) => cp.session(&id).await.is_some(),
        Err(_) => false,
    }
}

async fn wake_agent(
    cp: &Arc<ControlPlane>,
    agent: &AgentRow,
    wake: &Arc<WakeService>,
) -> Result<()> {
    // Serialize against a concurrent auto-suspend of the same agent.
    let lock = cp.agent_lock(&agent.id).await;
    let _guard = lock.lock().await;

    // Reload: a previous wake may have completed while we waited.
    let mut agent = cp
        .store()
        .get_agent(&agent.id)
        .await?
        .context("agent vanished from the registry")?;
    if is_awake(cp, &agent).await {
        return flush_pending(cp, &agent).await;
    }
    // A create/restore is already in flight: wait for it rather than
    // double-provisioning (a StartAgent order now would race it).
    if matches!(
        agent.state,
        AgentState::Provisioning | AgentState::Restoring
    ) {
        wake.emit(agent.id, "waking", Some("provisioning in progress".into()));
        let deadline = std::time::Instant::now() + ATTEMPT_TIMEOUT;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let cur = cp
                .store()
                .get_agent(&agent.id)
                .await?
                .context("agent vanished from the registry")?;
            if is_awake(cp, &cur).await {
                agent = cur;
                return wake_ok(cp, &mut agent, wake).await;
            }
            if cur.state != agent.state || std::time::Instant::now() > deadline {
                agent = cur;
                break; // state changed (e.g. failed) or timed out: normal wake
            }
        }
    }

    let cfg = crate::retention::load_config().unwrap_or_default();
    let attempts = cfg.auto_suspend.wake_retry_attempts.max(1);
    wake.emit(agent.id, "queued", None);
    let mut excluded: Vec<String> = Vec::new();
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=attempts {
        // Fast path: the last daemon may still hold local state (VM
        // checkpoint) — a StartAgent there resumes in seconds.
        if !excluded.contains(&agent.daemon_endpoint_id) {
            if daemon_online(cp, &agent.daemon_endpoint_id).await {
                wake.emit(agent.id, "starting", Some(format!("attempt {attempt}")));
                let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
                let order = tokio::time::timeout(
                    ATTEMPT_TIMEOUT,
                    cp.order(
                        &daemon,
                        &Order::StartAgent {
                            agent_id: agent.id,
                            force: false,
                        },
                    ),
                )
                .await;
                match order {
                    Ok(Ok(ack)) if ack.success => {
                        return wake_ok(cp, &mut agent, wake).await;
                    }
                    Ok(Ok(ack)) => {
                        info!(
                            agent = %agent.name,
                            "same-daemon wake refused: {}",
                            ack.message.unwrap_or_default()
                        );
                        excluded.push(agent.daemon_endpoint_id.clone());
                    }
                    Ok(Err(e)) => {
                        info!(agent = %agent.name, "same-daemon wake failed: {e:#}");
                        excluded.push(agent.daemon_endpoint_id.clone());
                    }
                    Err(_) => {
                        info!(agent = %agent.name, "same-daemon wake timed out");
                        excluded.push(agent.daemon_endpoint_id.clone());
                    }
                }
            } else {
                excluded.push(agent.daemon_endpoint_id.clone());
            }
        }

        // Restore path: centrally stored bundle → scheduler placement
        // (excluding daemons that already failed this wake; may preempt
        // idle agents to free capacity). A failed target is excluded from
        // the next attempt — retries spread across the fleet.
        wake.emit(agent.id, "restoring", Some(format!("attempt {attempt}")));
        let target = match crate::scheduler::place_or_preempt(
            cp,
            &crate::scheduler::Constraints {
                require: Default::default(),
                pin: None,
                manifest: agent.manifest.clone(),
                exclude: excluded.clone(),
            },
        )
        .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!(agent = %agent.name, attempt, "wake placement failed: {e:#}");
                last_err = Some(e);
                continue;
            }
        };
        let target_id = target.endpoint_id.to_string();
        match tokio::time::timeout(ATTEMPT_TIMEOUT, restore_from_bundle(cp, &agent, &target)).await
        {
            Ok(Ok(daemon_id)) => {
                agent.daemon_endpoint_id = daemon_id;
                return wake_ok(cp, &mut agent, wake).await;
            }
            Ok(Err(e)) => {
                warn!(agent = %agent.name, attempt, daemon = %target_id, "wake restore failed: {e:#}");
                last_err = Some(e);
                excluded.push(target_id);
            }
            Err(_) => {
                warn!(agent = %agent.name, attempt, daemon = %target_id, "wake restore timed out");
                last_err = Some(anyhow::anyhow!("restore timed out"));
                excluded.push(target_id);
            }
        }
    }

    // Terminal failure: flag for human intervention, fail queued messages.
    let msg = last_err
        .map(|e| format!("{e:#}"))
        .unwrap_or_else(|| "wake failed".into());
    cp.store()
        .update_agent_state(&agent.id, AgentState::Failed)
        .await?;
    cp.store().set_needs_attention(&agent.id, true).await?;
    let pending = cp.store().pending_messages(&agent.id).await?;
    let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
    cp.store()
        .set_message_status(&ids, "failed", Some(&msg))
        .await?;
    wake.emit(agent.id, "failed", Some(msg.clone()));
    crate::audit::record(
        "agent_wake_failed",
        serde_json::json!({"name": agent.name, "id": agent.id, "error": msg}),
    )
    .await;
    anyhow::bail!("agent '{}' failed to wake: {msg}", agent.name)
}

/// Mark the agent awake and flush its queued (coalesced) messages.
async fn wake_ok(
    cp: &Arc<ControlPlane>,
    agent: &mut AgentRow,
    wake: &Arc<WakeService>,
) -> Result<()> {
    cp.store()
        .update_agent_state(&agent.id, AgentState::Active)
        .await?;
    cp.store().set_agent_woke_at(&agent.id).await?;
    cp.store().set_needs_attention(&agent.id, false).await?;
    agent.state = AgentState::Active;
    flush_pending(cp, agent).await?;
    wake.emit(agent.id, "ready", None);
    crate::audit::record(
        "agent_wake",
        serde_json::json!({"name": agent.name, "id": agent.id, "daemon": agent.daemon_endpoint_id}),
    )
    .await;
    info!(agent = %agent.name, "agent awake");
    Ok(())
}

/// Deliver all queued messages to an **awake** agent as one coalesced
/// batch. Drains until empty so messages enqueued mid-delivery (coalescing
/// window) are not stranded.
pub async fn flush_pending(cp: &Arc<ControlPlane>, agent: &AgentRow) -> Result<()> {
    loop {
        let pending = cp.store().pending_messages(&agent.id).await?;
        if pending.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = pending.iter().map(|m| m.id).collect();
        cp.store().set_message_status(&ids, "waking", None).await?;
        let body = pending
            .iter()
            .map(|m| m.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        match prompt_agent(cp, agent, &body).await {
            Ok(()) => {
                cp.store()
                    .set_message_status(&ids, "delivered", None)
                    .await?;
            }
            Err(e) => {
                cp.store()
                    .set_message_status(&ids, "failed", Some(&format!("{e:#}")))
                    .await?;
                return Err(e);
            }
        }
    }
}

/// Send one prompt to an awake agent over the attach stream and confirm
/// the handshake.
async fn prompt_agent(cp: &Arc<ControlPlane>, agent: &AgentRow, message: &str) -> Result<()> {
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
    let (mut send, mut recv) = cp
        .open_stream_retry(&daemon, &StreamHello::Attach { agent_id: agent.id })
        .await?;
    write_jsonl(
        &mut send,
        &AttachMessage::Prompt {
            message: message.into(),
        },
    )
    .await?;
    let first = tokio::time::timeout(
        Duration::from_secs(30),
        read_jsonl::<_, AttachMessage>(&mut recv),
    )
    .await
    .context("agent did not acknowledge the prompt")??;
    match first {
        AttachMessage::Notice { message } if message == "attached" => Ok(()),
        AttachMessage::Notice { message } => anyhow::bail!("prompt rejected: {message}"),
        AttachMessage::Event { .. } => Ok(()), // live events = agent alive
        _ => Ok(()),
    }
}

/// Bundle restore onto a scheduler-chosen daemon (preemption of idle
/// agents may already have freed capacity). Returns the new daemon
/// endpoint id.
async fn restore_from_bundle(
    cp: &Arc<ControlPlane>,
    agent: &AgentRow,
    target: &crate::scheduler::Placement,
) -> Result<String> {
    let bundle = crate::bundle::load(&agent.id).await?;
    cp.store()
        .update_agent_state(&agent.id, AgentState::Restoring)
        .await?;

    let (mut send, mut recv) = cp
        .open_stream(
            &target.endpoint_id,
            &StreamHello::Restore { agent_id: agent.id },
        )
        .await?;
    use suzerain_protocol::control::{BundleAck, BundleMessage};
    write_jsonl(
        &mut send,
        &BundleMessage::Start {
            manifest: Box::new(bundle.manifest.clone()),
            session_file: bundle.session_file.clone(),
            secrets: Some(crate::secrets::slice_for(&bundle.manifest)?),
        },
    )
    .await?;
    for (rel, abs) in &bundle.files {
        let data = tokio::fs::read(abs).await?;
        if let Some(want) = bundle.hashes.get(rel) {
            let got = suzerain_protocol::framing::sha256_hex(&data);
            if &got != want {
                anyhow::bail!(
                    "stored bundle for '{}' failed integrity check ({rel})",
                    agent.name
                );
            }
        }
        write_jsonl(
            &mut send,
            &BundleMessage::File {
                path: rel.clone(),
                sha256: Some(suzerain_protocol::framing::sha256_hex(&data)),
                data: crate::bundle::base64_encode(&data),
                last: true,
            },
        )
        .await?;
    }
    write_jsonl(&mut send, &BundleMessage::End).await?;
    send.finish()?;
    let ack: BundleAck = read_jsonl(&mut recv).await?;
    if !ack.success {
        anyhow::bail!("restore failed: {}", ack.message.unwrap_or_default());
    }
    let daemon_id = target.endpoint_id.to_string();
    cp.store().set_agent_daemon(&agent.id, &daemon_id).await?;
    Ok(daemon_id)
}
