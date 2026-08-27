//! The iroh control server: accepts castellan connections on `suz/control/0`,
//! enforces the EndpointId allowlist (enrollment), tracks per-daemon sessions,
//! dispatches orders, receives log batches, and joins the fleet gossip topic
//! for presence.
//!
//! Wire shape (see docs/PHASE0-FINDINGS.md for why): one long-lived
//! connection per daemon. The daemon opens stream 0 with `Register`; that
//! stream then carries orders (suzerain writes `Order`, daemon replies
//! `OrderAck`) and heartbeats. The daemon opens further bi-streams labeled
//! with `StreamHello` (logs). Suzerain may also open streams daemon-side
//! (attach relay).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use iroh::endpoint::Connection;
use iroh::{endpoint::presets, protocol::Router, Endpoint, EndpointId};
use iroh_gossip::Gossip;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use n0_future::StreamExt;
use suzerain_protocol::alpn;
use suzerain_protocol::control::{
    BundleAck, BundleMessage, Register, RegisterResponse, StateReport, StreamHello,
};
use suzerain_protocol::event::{LogAck, LogBatch, LogEvent};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use suzerain_protocol::order::{Order, OrderAck};
use tokio::io::BufReader;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{info, warn};
use uuid::Uuid;

use crate::identity::data_dir;
use crate::store::Store;

/// Generous enough to outlast castellan's own provisioning bound
/// (`PROVISION_TIMEOUT`, 15 minutes) plus margin: a `CreateAgent`/`StartAgent`
/// order's ack doesn't arrive until the daemon's provision attempt finishes
/// (success or failure), so this must never be shorter than that.
const ORDER_TIMEOUT: Duration = Duration::from_secs(16 * 60);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

pub struct DaemonSession {
    pub info: suzerain_protocol::state::DaemonInfo,
    /// Registration epoch: only the newest session for a daemon may remove
    /// itself / mark the daemon offline (fences stale sessions, G2).
    epoch: u64,
    conn: Connection,
}

#[derive(Clone)]
pub struct ControlPlane {
    store: Store,
    sessions: Arc<Mutex<HashMap<EndpointId, Arc<DaemonSession>>>>,
    next_epoch: Arc<std::sync::atomic::AtomicU64>,
    endpoint: Endpoint,
    wake: Arc<crate::wake::WakeService>,
    /// Per-agent lifecycle mutex: serializes auto-suspend vs. wake (and
    /// concurrent wakes) for one agent.
    agent_locks: Arc<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
    /// Suzy operator EndpointIds allowed on the operator channel. Shared
    /// with `OperatorHandler` and mutated live by the `operator_approve`
    /// socket command — no control-plane restart needed.
    operator_allow: Arc<std::sync::RwLock<BTreeSet<EndpointId>>>,
}

impl ControlPlane {
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Full iroh address (direct addrs + relay) — for dialing this control
    /// plane in tests and for display to operators.
    pub fn addr(&self) -> iroh::EndpointAddr {
        self.endpoint.addr()
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The same store, viewed through the [`crate::registry::Registry`]
    /// trait object. Additive per docs/UNIFIED-AGENT-API-DESIGN.md §4.2 —
    /// `store()` is unchanged and still the concrete-type accessor every
    /// existing caller uses; this exists so new code can depend on the
    /// trait instead of the concrete `Store` type without anyone having to
    /// migrate today.
    pub fn registry(&self) -> &dyn crate::registry::Registry {
        &self.store
    }

    pub async fn online_daemon(&self) -> Option<(EndpointId, Arc<DaemonSession>)> {
        self.sessions
            .lock()
            .await
            .iter()
            .next()
            .map(|(id, s)| (*id, s.clone()))
    }

    pub async fn session(&self, id: &EndpointId) -> Option<Arc<DaemonSession>> {
        self.sessions.lock().await.get(id).cloned()
    }

    pub fn wake(&self) -> &Arc<crate::wake::WakeService> {
        &self.wake
    }

    /// EndpointIds currently allowed on the operator channel.
    pub fn operator_allow(&self) -> Vec<EndpointId> {
        self.operator_allow
            .read()
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Allow an operator EndpointId immediately (no restart).
    pub fn add_operator_allow(&self, id: EndpointId) {
        if let Ok(mut s) = self.operator_allow.write() {
            s.insert(id);
        }
    }

    /// The live allow set shared with `OperatorHandler`.
    pub(crate) fn operator_allow_shared(&self) -> Arc<std::sync::RwLock<BTreeSet<EndpointId>>> {
        self.operator_allow.clone()
    }

    pub async fn agent_lock(&self, id: &Uuid) -> Arc<Mutex<()>> {
        self.agent_locks
            .lock()
            .await
            .entry(*id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Send an order to a daemon and await the ack.
    ///
    /// Each call opens its own bi-stream (`StreamHello::Order`) rather than
    /// sharing one persistent order pipe: previously every order for a
    /// daemon was written/read on the single register stream, one at a
    /// time, so a slow order (e.g. a 15-minute provision) blocked the ack
    /// for every unrelated order — including Stop/Suspend/Destroy for other
    /// agents on the same daemon — behind it. With one stream per order,
    /// concurrent orders to the same daemon no longer serialize behind each
    /// other, and a timed-out order no longer risks desyncing a shared
    /// FIFO ack stream (each order's ack is read from its own stream), so a
    /// timeout no longer needs to drop the whole daemon session.
    pub async fn order(&self, daemon: &EndpointId, order: &Order) -> Result<OrderAck> {
        let session = self
            .session(daemon)
            .await
            .ok_or_else(|| anyhow!("daemon {daemon} is not online"))?;
        let result = timeout(ORDER_TIMEOUT, async {
            let (mut send, mut recv) = self.open_stream(daemon, &StreamHello::Order).await?;
            write_jsonl(&mut send, order).await?;
            send.finish()?;
            let ack: OrderAck = read_jsonl(&mut recv).await?;
            Ok(ack)
        })
        .await;
        match result {
            Ok(Ok(ack)) => Ok(ack),
            Ok(Err(e)) => {
                // A genuine transport-level failure opening/using this
                // order's own stream means the connection itself is in
                // trouble (a healthy connection just opens another stream);
                // other in-flight orders each have their own stream, so
                // this doesn't desync them — but the connection is still
                // worth dropping so the daemon reconnects and resyncs.
                self.drop_session(daemon, &session, "order transport error")
                    .await;
                Err(e)
            }
            Err(_) => Err(anyhow!(
                "order timed out after {}s",
                ORDER_TIMEOUT.as_secs()
            )),
        }
    }

    /// Drop a daemon session after an order-stream failure: close the
    /// connection (fencing by epoch so a newer session is untouched) and
    /// mark the daemon offline. The daemon's control client notices the
    /// drop and re-registers, resyncing agent states via snapshot.
    async fn drop_session(&self, daemon: &EndpointId, session: &Arc<DaemonSession>, reason: &str) {
        warn!(daemon = %daemon, reason, "dropping daemon session");
        {
            let mut sessions = self.sessions.lock().await;
            let still_current = sessions
                .get(daemon)
                .map(|s| s.epoch == session.epoch)
                .unwrap_or(false);
            if still_current {
                sessions.remove(daemon);
            }
        }
        session.conn.close(1u32.into(), b"order stream desync");
        if let Err(err) = mark_offline(&self.store, daemon).await {
            warn!("marking daemon offline failed: {err:#}");
        }
    }

    /// Like open_stream, but retries through transient daemon-offline
    /// windows (reconnect backoff after suzerain restarts / link flaps).
    pub async fn open_stream_retry(
        &self,
        daemon: &EndpointId,
        hello: &StreamHello,
    ) -> Result<(
        iroh::endpoint::SendStream,
        BufReader<iroh::endpoint::RecvStream>,
    )> {
        let mut last_err = None;
        for _ in 0..6 {
            match self.open_stream(daemon, hello).await {
                Ok(pair) => return Ok(pair),
                Err(err) => {
                    last_err = Some(err);
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("daemon unreachable")))
    }

    /// Open a new bi-stream to a daemon with the given hello label (attach…).
    pub async fn open_stream(
        &self,
        daemon: &EndpointId,
        hello: &StreamHello,
    ) -> Result<(
        iroh::endpoint::SendStream,
        BufReader<iroh::endpoint::RecvStream>,
    )> {
        let session = self
            .session(daemon)
            .await
            .ok_or_else(|| anyhow!("daemon {daemon} is not online"))?;
        let (mut send, recv) = session.conn.open_bi().await?;
        write_jsonl(&mut send, hello).await?;
        Ok((send, BufReader::new(recv)))
    }

    async fn register(&self, conn: Connection) -> Result<()> {
        let remote = conn.remote_id();
        let (mut send, recv) = conn.accept_bi().await?;
        let mut recv = BufReader::new(recv);
        let register: Register = read_jsonl(&mut recv).await?;
        let mut info = register.info;
        info.endpoint_id = remote.to_string();

        if register.protocol_version != suzerain_protocol::control::PROTOCOL_VERSION {
            warn!(
                daemon = %remote,
                daemon_version = register.protocol_version,
                our_version = suzerain_protocol::control::PROTOCOL_VERSION,
                "rejecting daemon: protocol version mismatch"
            );
            write_jsonl(
                &mut send,
                &RegisterResponse {
                    accepted: false,
                    message: Some(format!(
                        "protocol version mismatch: daemon={}, control plane={}",
                        register.protocol_version,
                        suzerain_protocol::control::PROTOCOL_VERSION
                    )),
                    protocol_version: suzerain_protocol::control::PROTOCOL_VERSION,
                },
            )
            .await?;
            send.finish()?;
            conn.close(1u32.into(), b"protocol version mismatch");
            return Ok(());
        }

        if !self.store.daemon_approved(&remote.to_string()).await? {
            // Track as a pending enrollment for one-click approval (M4).
            self.store.upsert_pending_daemon(&info).await.ok();
            warn!(daemon = %remote, "rejecting unapproved daemon");
            write_jsonl(
                &mut send,
                &RegisterResponse {
                    accepted: false,
                    message: Some("endpoint not approved; run `suz daemon approve <id>`".into()),
                    protocol_version: suzerain_protocol::control::PROTOCOL_VERSION,
                },
            )
            .await?;
            send.finish()?;
            conn.close(1u32.into(), b"not approved");
            return Ok(());
        }

        self.store.upsert_daemon(&info, true).await?;
        write_jsonl(
            &mut send,
            &RegisterResponse {
                accepted: true,
                message: None,
                protocol_version: suzerain_protocol::control::PROTOCOL_VERSION,
            },
        )
        .await?;
        info!(daemon = %remote, hostname = %info.hostname, "daemon registered");

        // Re-check approval immediately before installing the live session:
        // the checks above (and the writes/awaits since) leave a window in
        // which a concurrent `suz daemon revoke`/un-approve can flip the
        // daemon to unapproved. Without this, a rejected daemon could still
        // end up with a live session inserted below.
        if !self.store.daemon_approved(&remote.to_string()).await? {
            warn!(daemon = %remote, "daemon un-approved during registration; dropping session");
            self.store
                .set_daemon_online(&remote.to_string(), false)
                .await
                .ok();
            conn.close(1u32.into(), b"not approved");
            return Ok(());
        }

        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let session = Arc::new(DaemonSession {
            info,
            epoch,
            conn: conn.clone(),
        });
        self.sessions.lock().await.insert(remote, session.clone());

        // The register stream's only job was the handshake above: orders
        // now each open their own stream (see `order()`), so finish this
        // one instead of holding it open unused for the life of the
        // session. `recv` is simply dropped — castellan finishes its end
        // too once it reads the response.
        send.finish()?;
        drop(recv);

        // Announce presence on the fleet topic (best-effort).
        announce(&format!("daemon-online:{remote}")).await;

        // Accept daemon-opened streams (logs) until the connection drops.
        let store = self.store.clone();
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            while let Ok((send, recv)) = conn.accept_bi().await {
                let store = store.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_stream(store, Some(remote), send, recv).await {
                        warn!("stream error: {err:#}");
                    }
                });
            }
            // Fencing: a newer session for this daemon supersedes us — do
            // NOT mark it offline when our stale connection dies.
            let still_current = sessions
                .lock()
                .await
                .get(&remote)
                .map(|s| s.epoch == epoch)
                .unwrap_or(false);
            if still_current {
                sessions.lock().await.remove(&remote);
                if let Err(err) = mark_offline(&store, &remote).await {
                    warn!("marking daemon offline failed: {err:#}");
                }
                announce(&format!("daemon-offline:{remote}")).await;
                info!(daemon = %remote, "daemon disconnected");
            } else {
                info!(daemon = %remote, "stale session closed (superseded)");
            }
        });

        // Heartbeats (each its own order stream) keep liveness fresh.
        let cp = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                match cp.order(&remote, &Order::Ping { nonce: 0 }).await {
                    Ok(ack) if ack.success => {
                        cp.store
                            .set_daemon_online(&remote.to_string(), true)
                            .await
                            .ok();
                    }
                    _ => break,
                }
            }
        });

        Ok(())
    }
}

async fn mark_offline(store: &Store, id: &EndpointId) -> Result<()> {
    store.set_daemon_online(&id.to_string(), false).await
}

/// Best-effort fleet gossip (presence). Wired in start(); no-ops before that.
static GOSSIP_TX: tokio::sync::OnceCell<iroh_gossip::api::GossipSender> =
    tokio::sync::OnceCell::const_new();

async fn announce(message: &str) {
    if let Some(tx) = GOSSIP_TX.get() {
        let _ = tx.broadcast(message.as_bytes().to_vec().into()).await;
    }
}

async fn handle_stream(
    store: Store,
    daemon_id: Option<EndpointId>,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let mut recv = BufReader::new(recv);
    let hello: StreamHello = read_jsonl(&mut recv).await?;
    match hello {
        StreamHello::Logs { agent_id } => handle_logs(store, agent_id, send, recv).await,
        StreamHello::BundleUpload { agent_id } => handle_bundle_upload(agent_id, send, recv).await,
        StreamHello::StateReport => handle_state_reports(store, daemon_id, recv).await,
        StreamHello::Secrets { agent_id } => {
            handle_secrets_pull(store, daemon_id, agent_id, send).await
        }
        other => bail!("unexpected daemon-initiated stream: {other:?}"),
    }
}

/// Apply agent state reports from a daemon: snapshot after registration,
/// then incremental transitions. Entries are only applied for agents whose
/// registry row belongs to the reporting daemon (anti-spoofing).
async fn handle_state_reports(
    store: Store,
    daemon_id: Option<EndpointId>,
    mut recv: BufReader<iroh::endpoint::RecvStream>,
) -> Result<()> {
    let daemon_id = daemon_id.context("state reports require a known daemon")?;
    loop {
        match read_jsonl::<_, StateReport>(&mut recv).await {
            Ok(report) => {
                // Full snapshot: agents owned by this daemon that are absent
                // from the report are lost (e.g. wiped) — mark Failed.
                if report.full {
                    let daemon_str = daemon_id.to_string();
                    let reported: std::collections::HashSet<uuid::Uuid> =
                        report.agents.iter().map(|a| a.agent_id).collect();
                    for agent in store.list_agents().await? {
                        if agent.daemon_endpoint_id == daemon_str
                            && !reported.contains(&agent.id)
                            && !matches!(
                                agent.state,
                                suzerain_protocol::AgentState::Decommissioned
                                    | suzerain_protocol::AgentState::Failed
                            )
                        {
                            warn!(agent = %agent.name, "agent missing from daemon snapshot; marking failed");
                            store
                                .update_agent_state(
                                    &agent.id,
                                    suzerain_protocol::AgentState::Failed,
                                )
                                .await?;
                        }
                    }
                }
                for entry in report.agents {
                    let Some(agent) = store.get_agent_by_name(&entry.name).await? else {
                        continue; // unknown to the registry; nothing to converge
                    };
                    if agent.daemon_endpoint_id != daemon_id.to_string()
                        || agent.id != entry.agent_id
                    {
                        warn!(
                            agent = %entry.name,
                            "ignoring state report for foreign agent"
                        );
                        continue;
                    }
                    // Activity facts ride every report (idle/busy ground
                    // truth for the auto-suspend sweep + preemption).
                    if entry.idle_secs.is_some() || entry.busy.is_some() {
                        store
                            .set_agent_activity(&agent.id, entry.idle_secs, entry.busy)
                            .await?;
                    }
                    // Session eras: a changed session file means a new pi
                    // session began (rotation on wake) — close the open
                    // era and start a new one.
                    reconcile_session(&store, &agent, entry.session_file.as_deref()).await?;
                    if entry.state == suzerain_protocol::AgentState::Decommissioned {
                        info!(agent = %entry.name, "decommissioned report; deleting row");
                        store.delete_agent(&agent.id).await?;
                    } else if agent.state != entry.state {
                        info!(agent = %entry.name, from = ?agent.state, to = ?entry.state, "state report applied");
                        store.update_agent_state(&agent.id, entry.state).await?;
                    }
                }
            }
            Err(suzerain_protocol::framing::FramingError::Eof) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

/// Track pi session eras from daemon reports. A session file change
/// closes the open era and opens a new one; an unchanged file just
/// backfills the open row for agents that predate session tracking.
async fn reconcile_session(
    store: &Store,
    agent: &crate::store::AgentRow,
    reported: Option<&str>,
) -> Result<()> {
    let Some(file) = reported.filter(|f| !f.is_empty()) else {
        return Ok(());
    };
    if agent.session_file.as_deref() == Some(file) {
        store
            .ensure_open_session(&agent.id, file, &agent.created_at)
            .await?;
    } else {
        info!(agent = %agent.name, session = %file, "new agent session era");
        store.start_agent_session(&agent.id, file).await?;
        store.set_agent_session_file(&agent.id, file).await?;
    }
    Ok(())
}

/// A daemon pulls a freshly-sliced secret bundle for an agent it owns
/// (G7: daemon keeps bundles in memory only and re-pulls after restart).
async fn handle_secrets_pull(
    store: Store,
    daemon_id: Option<EndpointId>,
    agent_id: Uuid,
    mut send: iroh::endpoint::SendStream,
) -> Result<()> {
    use suzerain_protocol::framing::write_jsonl;
    let daemon_id = daemon_id.context("secrets pull requires a known daemon")?;
    let reply = async {
        // Find the agent and verify ownership before slicing anything.
        let agent = store
            .list_agents()
            .await?
            .into_iter()
            .find(|a| a.id == agent_id)
            .context("unknown agent")?;
        if agent.daemon_endpoint_id != daemon_id.to_string() {
            bail!("secrets pull for a foreign agent");
        }
        crate::secrets::slice_for(&agent.manifest)
    }
    .await;
    let payload = match &reply {
        Ok(bundle) => serde_json::to_value(bundle)?,
        Err(err) => serde_json::json!({"error": format!("{err:#}")}),
    };
    write_jsonl(&mut send, &payload).await?;
    send.finish()?;
    reply.map(|_| ())
}

/// Receive an agent restore bundle (uploaded on suspend).
async fn handle_bundle_upload(
    agent_id: Uuid,
    mut send: iroh::endpoint::SendStream,
    mut recv: BufReader<iroh::endpoint::RecvStream>,
) -> Result<()> {
    let result = async {
        match read_jsonl(&mut recv).await? {
            BundleMessage::Start {
                manifest,
                session_file,
                secrets: _,
            } => {
                crate::bundle::write_start(&agent_id, &manifest, session_file.as_deref()).await?;
            }
            other => bail!("expected bundle start, got {other:?}"),
        }
        loop {
            match read_jsonl(&mut recv).await? {
                BundleMessage::File {
                    path,
                    data,
                    last: _,
                    sha256,
                } => {
                    crate::bundle::write_file(&agent_id, &path, &data, sha256.as_deref()).await?;
                }
                BundleMessage::End => break,
                other => bail!("unexpected bundle message: {other:?}"),
            }
        }
        info!(%agent_id, "restore bundle stored");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let ack = match &result {
        Ok(()) => BundleAck {
            success: true,
            message: None,
        },
        Err(err) => BundleAck {
            success: false,
            message: Some(format!("{err:#}")),
        },
    };
    write_jsonl(&mut send, &ack).await?;
    send.finish()?;
    result
}

/// Receive log batches; append to the central JSONL store; ack contiguous
/// seqs. Dedupes on (agent_id, seq) — the daemon ships at-least-once.
async fn handle_logs(
    store: Store,
    agent_id: Uuid,
    mut send: iroh::endpoint::SendStream,
    mut recv: BufReader<iroh::endpoint::RecvStream>,
) -> Result<()> {
    let log_dir = data_dir().join("logs");
    tokio::fs::create_dir_all(&log_dir).await?;
    let path = log_dir.join(format!("{agent_id}.jsonl"));
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;

    loop {
        match read_jsonl::<_, LogBatch>(&mut recv).await {
            Ok(batch) => {
                let mut acked = store.acked_through(&agent_id).await?;
                let mut written = 0u64;
                for event in batch.events {
                    if event.seq <= acked {
                        continue; // duplicate
                    }
                    store_event(&mut file, &event).await?;
                    // Dual-write into the new SQLite-backed ChatStore
                    // (docs/UNIFIED-AGENT-API-DESIGN.md §4.6) alongside the
                    // JSONL file, which stays authoritative for reads until
                    // the read call sites (api.rs/web_session.rs/web.rs)
                    // are migrated in a follow-up pass.
                    crate::chat_store::ChatStore::append(&store, &agent_id, &event).await?;
                    acked = acked.max(event.seq);
                    written += 1;
                }
                store.set_acked_through(&agent_id, acked).await?;
                write_jsonl(
                    &mut send,
                    &LogAck {
                        agent_id,
                        acked_through: acked,
                    },
                )
                .await?;
                info!(%agent_id, written, acked, "log batch stored");
            }
            Err(suzerain_protocol::framing::FramingError::Eof) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

async fn store_event(file: &mut tokio::fs::File, event: &LogEvent) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    file.write_all(&line).await?;
    file.flush().await?;
    Ok(())
}

/// Accept handler for the control ALPN.
struct ControlHandler {
    cp: ControlPlane,
}

impl std::fmt::Debug for ControlHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlHandler").finish()
    }
}

impl iroh::protocol::ProtocolHandler for ControlHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        // Registration runs inline; the session task then owns the connection.
        if let Err(err) = self.cp.register(connection.clone()).await {
            warn!("register failed: {err:#}");
            connection.close(2u32.into(), b"register failed");
        }
        Ok(())
    }
}

/// Start the control plane's iroh endpoint: control ALPN + fleet gossip +
/// the operator channel (when an allow list is configured).
pub async fn start(store: Store, operator_allow: Vec<iroh::EndpointId>) -> Result<ControlPlane> {
    // No sessions exist yet: any online flag from a previous run is stale.
    store.set_all_daemons_offline().await.ok();
    let secret_key = crate::identity::load_or_create_secret_key()?;
    // Connection-level idle timeout raised to 60s (default ~15s): provisioning
    // orders and quiet periods between heartbeats must not kill links.
    let transport = iroh::endpoint::QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(60)
                .try_into()
                .expect("idle timeout"),
        ))
        .build();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .transport_config(transport)
        .bind()
        .await?;
    let mdns = MdnsAddressLookup::builder().build(endpoint.id())?;
    endpoint.address_lookup()?.add(mdns);

    let gossip = Gossip::builder().spawn(endpoint.clone());
    let operator_allow: Arc<std::sync::RwLock<BTreeSet<EndpointId>>> =
        Arc::new(std::sync::RwLock::new(operator_allow.into_iter().collect()));
    let cp = ControlPlane {
        store,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        next_epoch: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        endpoint: endpoint.clone(),
        wake: Arc::new(crate::wake::WakeService::new()),
        agent_locks: Arc::new(Mutex::new(HashMap::new())),
        operator_allow,
    };
    let handler = ControlHandler { cp: cp.clone() };
    let mut router_builder = Router::builder(endpoint)
        .accept(alpn::CONTROL, handler)
        .accept(iroh_gossip::ALPN, gossip.clone());
    if cp.operator_allow().is_empty() {
        info!("operator channel: no [operator] allow entries — suzy connections will be rejected");
    }
    router_builder = router_builder.accept(
        alpn::OPERATOR,
        crate::operator::OperatorHandler::new(Arc::new(cp.clone()), cp.operator_allow_shared()),
    );
    let _router = router_builder.spawn();
    std::mem::forget(_router); // keep alive for the process lifetime

    // Join the fleet topic and log announcements (presence only).
    let topic = iroh_gossip::TopicId::from_bytes(alpn::FLEET_TOPIC);
    let (sender, mut receiver) = gossip.subscribe(topic, vec![]).await?.split();
    GOSSIP_TX.set(sender).ok();
    tokio::spawn(async move {
        while let Some(event) = receiver.next().await {
            if let Ok(iroh_gossip::api::Event::Received(msg)) = event {
                info!(fleet = %String::from_utf8_lossy(&msg.content), "gossip");
            }
        }
    });

    info!(endpoint_id = %cp.endpoint_id(), "suzerain control plane online");
    Ok(cp)
}
