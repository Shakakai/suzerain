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

use std::collections::HashMap;
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

const ORDER_TIMEOUT: Duration = Duration::from_secs(300);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

pub struct DaemonSession {
    pub info: suzerain_protocol::state::DaemonInfo,
    /// Registration epoch: only the newest session for a daemon may remove
    /// itself / mark the daemon offline (fences stale sessions, G2).
    epoch: u64,
    conn: Connection,
    /// The register stream, reused for orders/heartbeats.
    order_tx: Mutex<iroh::endpoint::SendStream>,
    order_rx: Mutex<BufReader<iroh::endpoint::RecvStream>>,
}

#[derive(Clone)]
pub struct ControlPlane {
    store: Store,
    sessions: Arc<Mutex<HashMap<EndpointId, Arc<DaemonSession>>>>,
    next_epoch: Arc<std::sync::atomic::AtomicU64>,
    endpoint: Endpoint,
}

impl ControlPlane {
    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn store(&self) -> &Store {
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

    /// Send an order to a daemon and await the ack.
    pub async fn order(&self, daemon: &EndpointId, order: &Order) -> Result<OrderAck> {
        let session = self
            .session(daemon)
            .await
            .ok_or_else(|| anyhow!("daemon {daemon} is not online"))?;
        timeout(ORDER_TIMEOUT, async {
            let mut tx = session.order_tx.lock().await;
            let mut rx = session.order_rx.lock().await;
            write_jsonl(&mut *tx, order).await?;
            let ack: OrderAck = read_jsonl(&mut *rx).await?;
            Ok(ack)
        })
        .await
        .context("order timed out")?
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

        if !self.store.daemon_approved(&remote.to_string()).await? {
            warn!(daemon = %remote, "rejecting unapproved daemon");
            write_jsonl(
                &mut send,
                &RegisterResponse {
                    accepted: false,
                    message: Some("endpoint not approved; run `suz daemon approve <id>`".into()),
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
            },
        )
        .await?;
        info!(daemon = %remote, hostname = %info.hostname, "daemon registered");

        let epoch = self
            .next_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let session = Arc::new(DaemonSession {
            info,
            epoch,
            conn: conn.clone(),
            order_tx: Mutex::new(send),
            order_rx: Mutex::new(recv),
        });
        self.sessions.lock().await.insert(remote, session.clone());

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

        // Heartbeats on the order stream keep liveness fresh.
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

/// Start the control plane's iroh endpoint: control ALPN + fleet gossip.
pub async fn start(store: Store) -> Result<ControlPlane> {
    let secret_key = crate::identity::load_or_create_secret_key()?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .bind()
        .await?;
    let mdns = MdnsAddressLookup::builder().build(endpoint.id())?;
    endpoint.address_lookup()?.add(mdns);

    let gossip = Gossip::builder().spawn(endpoint.clone());
    let cp = ControlPlane {
        store,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        next_epoch: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        endpoint: endpoint.clone(),
    };
    let handler = ControlHandler { cp: cp.clone() };
    let _router = Router::builder(endpoint)
        .accept(alpn::CONTROL, handler)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();
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
