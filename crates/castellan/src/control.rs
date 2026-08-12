//! Control client: connects this daemon to suzerain over iroh, registers,
//! serves orders on the long-lived register stream, accepts attach streams,
//! and ships event logs with ack-based pruning.
//!
//! Connection discipline (docs/PHASE0-FINDINGS.md): the control connection
//! is established FIRST and held; gossip joins afterwards.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use iroh::{endpoint::presets, Endpoint, EndpointId, SecretKey};
use iroh_gossip::Gossip;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use suzerain_protocol::alpn;
use suzerain_protocol::control::{
    AttachMessage, BundleAck, BundleMessage, Register, RegisterResponse, StreamHello,
};
use suzerain_protocol::event::{LogAck, LogBatch, LogEvent};
use suzerain_protocol::framing::{read_jsonl, write_jsonl, FramingError};
use suzerain_protocol::order::{Order, OrderAck};
use suzerain_protocol::secrets::SecretBundle;
use tokio::io::BufReader;
use tracing::{info, warn};

use crate::journal::Journal;
use crate::state::{self, AgentPaths};
use crate::supervisor::Supervisor;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CastellanConfig {
    /// Suzerain's EndpointId (set by `castellan init --suzerain <id>`).
    #[serde(default)]
    pub suzerain_endpoint_id: Option<String>,
    /// Free-form scheduling labels reported at registration.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_max_agents")]
    pub max_agents: u32,
    /// Bundle upload debounce: upload after this many seconds of journal
    /// quiet (event-driven freshness; 0 disables event-driven uploads).
    #[serde(default = "default_bundle_quiet_secs")]
    pub bundle_quiet_secs: u64,
    /// Max bundle staleness: force an upload after this many seconds even if
    /// the agent never goes quiet (backstop; 0 disables).
    #[serde(default = "default_bundle_refresh_secs")]
    pub bundle_refresh_secs: u64,
    /// Host headroom reserved from scheduling (vCPU / MiB memory).
    #[serde(default)]
    pub reserve: Reserve,
}

/// Resources kept free for the host itself during fit checks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reserve {
    #[serde(default)]
    pub vcpu: u32,
    #[serde(default)]
    pub memory_mib: u64,
}

fn default_bundle_quiet_secs() -> u64 {
    30
}

fn default_bundle_refresh_secs() -> u64 {
    900
}

impl Default for CastellanConfig {
    fn default() -> Self {
        Self {
            suzerain_endpoint_id: None,
            labels: Default::default(),
            max_agents: default_max_agents(),
            bundle_quiet_secs: default_bundle_quiet_secs(),
            bundle_refresh_secs: default_bundle_refresh_secs(),
            reserve: Reserve::default(),
        }
    }
}

fn default_max_agents() -> u32 {
    4
}

pub fn config_path() -> PathBuf {
    state::data_dir().join("config.toml")
}

pub fn load_config() -> Result<CastellanConfig> {
    let path = config_path();
    if !path.exists() {
        return Ok(CastellanConfig::default());
    }
    Ok(toml::from_str(&std::fs::read_to_string(&path)?)?)
}

pub fn save_config(config: &CastellanConfig) -> Result<()> {
    std::fs::create_dir_all(state::data_dir())?;
    std::fs::write(config_path(), toml::to_string_pretty(config)?)?;
    Ok(())
}

/// Load or create this daemon's iroh identity.
pub fn identity() -> Result<SecretKey> {
    let path = state::data_dir().join("identity.key");
    if path.exists() {
        let bytes = std::fs::read(&path)?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .context("identity.key is not 32 bytes")?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    std::fs::create_dir_all(state::data_dir())?;
    let key = SecretKey::generate();
    std::fs::write(&path, key.to_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Connect to suzerain and serve forever (reconnects with backoff).
pub async fn run_control_client(supervisor: Arc<Supervisor>) -> Result<()> {
    let config = load_config()?;
    let Some(suzerain) = config.suzerain_endpoint_id.clone() else {
        info!("no suzerain configured (castellan init --suzerain <id>) — standalone mode");
        return Ok(());
    };
    let suzerain_id: EndpointId = suzerain.parse().context("invalid suzerain endpoint id")?;
    let secret = identity()?;

    let mut backoff = Duration::from_secs(1);
    loop {
        match connect_and_serve(&supervisor, &secret, suzerain_id, &config).await {
            Ok(()) => info!("control connection closed; reconnecting"),
            Err(err) => warn!("control connection failed: {err:#}"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

use tokio::sync::broadcast;
use uuid::Uuid;

async fn connect_and_serve(
    supervisor: &Arc<Supervisor>,
    secret: &SecretKey,
    suzerain: EndpointId,
    config: &CastellanConfig,
) -> Result<()> {
    // See suzerain control.rs: raise connection idle timeout so busy
    // provisioning periods and heartbeat gaps don't flap the link.
    let transport = iroh::endpoint::QuicTransportConfig::builder()
        .max_idle_timeout(Some(
            std::time::Duration::from_secs(60)
                .try_into()
                .expect("idle timeout"),
        ))
        .build();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret.clone())
        .transport_config(transport)
        .bind()
        .await?;
    let mdns = MdnsAddressLookup::builder().build(endpoint.id())?;
    endpoint.address_lookup()?.add(mdns);

    // Control connection FIRST (see findings), gossip after.
    let conn = endpoint.connect(suzerain, alpn::CONTROL).await?;
    let (mut order_tx, order_rx) = conn.open_bi().await?;
    let mut order_rx = BufReader::new(order_rx);

    let capacity = crate::probe::capacity(&state::data_dir());
    let info = suzerain_protocol::state::DaemonInfo {
        endpoint_id: endpoint.id().to_string(),
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        labels: config.labels.clone(),
        max_agents: config.max_agents,
        agents: vec![],
        usage: crate::probe::usage(&state::data_dir(), &capacity),
        capacity,
    };
    write_jsonl(&mut order_tx, &Register { info }).await?;
    let response: RegisterResponse = read_jsonl(&mut order_rx).await?;
    if !response.accepted {
        bail!(
            "suzerain rejected us: {}",
            response.message.unwrap_or_default()
        );
    }
    info!(suzerain = %suzerain, "registered with control plane");
    let handle = ControlHandle { conn: conn.clone() };

    // State reporting (G2): full snapshot now, transitions as they happen.
    {
        let report_conn = conn.clone();
        let sup = Arc::clone(supervisor);
        tokio::spawn(async move {
            if let Err(err) = run_state_reporter(report_conn, sup).await {
                warn!("state reporter exited: {err:#}");
            }
        });
    }

    // Gossip presence after the control link is up.
    let gossip = Gossip::builder().spawn(endpoint.clone());
    let router = iroh::protocol::Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();
    let topic = iroh_gossip::TopicId::from_bytes(alpn::FLEET_TOPIC);
    let (_gossip_tx, mut gossip_rx) = gossip.subscribe(topic, vec![suzerain]).await?.split();
    tokio::spawn(async move { while let Some(_event) = gossip_rx.next().await {} });

    // Task: accept suzerain-initiated streams (attach relays).
    let conn_streams = conn.clone();
    let sup_streams = Arc::clone(supervisor);
    let stream_task = tokio::spawn(async move {
        while let Ok((send, recv)) = conn_streams.accept_bi().await {
            let sup = Arc::clone(&sup_streams);
            tokio::spawn(async move {
                if let Err(err) = handle_inbound_stream(sup, send, recv).await {
                    warn!("inbound stream error: {err:#}");
                }
            });
        }
    });

    // Task: ship logs for all locally running agents + periodic bundle
    // refresh (G3: bundles were previously only uploaded at suspend).
    let ship_conn = conn.clone();
    let ship_sup = Arc::clone(supervisor);
    let bundle_quiet_secs = config.bundle_quiet_secs;
    let bundle_max_stale_secs = config.bundle_refresh_secs;
    let ship_task = tokio::spawn(async move {
        let handle = ControlHandle {
            conn: ship_conn.clone(),
        };
        let mut ship_state: HashMap<Uuid, ShipState> = HashMap::new();
        loop {
            if let Err(err) = ship_pending_logs(&ship_conn, &ship_sup, &mut ship_state).await {
                warn!("log shipping error: {err:#}");
            }
            if bundle_quiet_secs > 0 || bundle_max_stale_secs > 0 {
                if let Err(err) = refresh_bundles(
                    &handle,
                    &ship_sup,
                    &mut ship_state,
                    bundle_quiet_secs,
                    bundle_max_stale_secs,
                )
                .await
                {
                    warn!("bundle refresh error: {err:#}");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });

    // Main loop: read orders, dispatch, ack.
    loop {
        let order: Order = match read_jsonl(&mut order_rx).await {
            Ok(o) => o,
            Err(FramingError::Eof) => break,
            Err(err) => return Err(err.into()),
        };
        let ack = dispatch_order(supervisor, order, &handle).await;
        write_jsonl(&mut order_tx, &ack).await?;
    }

    stream_task.abort();
    ship_task.abort();
    router.shutdown().await.ok();
    endpoint.close().await;
    Ok(())
}

/// Report local agent states to suzerain: snapshot at registration, then
/// every transition observed on the supervisor's state-event channel.
async fn run_state_reporter(
    conn: iroh::endpoint::Connection,
    supervisor: Arc<Supervisor>,
) -> Result<()> {
    let (mut send, _recv) = conn.open_bi().await?;
    write_jsonl(&mut send, &StreamHello::StateReport).await?;

    let snapshot = state::list().await?;
    let entries: Vec<suzerain_protocol::AgentStateEntry> = snapshot
        .iter()
        .map(|r| suzerain_protocol::AgentStateEntry {
            agent_id: r.id,
            name: r.name.clone(),
            state: r.state,
        })
        .collect();
    write_jsonl(
        &mut send,
        &suzerain_protocol::StateReport {
            agents: entries,
            full: true,
        },
    )
    .await?;

    let mut rx = supervisor.subscribe_state_events();
    loop {
        match rx.recv().await {
            Ok(entry) => {
                write_jsonl(
                    &mut send,
                    &suzerain_protocol::StateReport {
                        agents: vec![entry],
                        full: false,
                    },
                )
                .await?;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

async fn dispatch_order(
    supervisor: &Arc<Supervisor>,
    order: Order,
    handle: &ControlHandle,
) -> OrderAck {
    let result: Result<Value> = async {
        match order {
            Order::CreateAgent {
                agent_id,
                manifest,
                secrets,
            } => {
                if let Ok(existing) = state::load(&agent_id).await {
                    // Duplicate delivery (ack lost on a flapping link): the
                    // order is idempotent — return the existing record.
                    return Ok(serde_json::to_value(existing)?);
                }
                if !secrets.is_empty() {
                    crate::secrets::put(agent_id, secrets);
                } else if crate::secrets::get(&agent_id).is_none() {
                    let bundle = handle.pull_secrets(agent_id).await?;
                    crate::secrets::put(agent_id, bundle);
                }
                let record = supervisor.create(Some(agent_id), manifest).await?;
                Ok(serde_json::to_value(record)?)
            }
            Order::StartAgent { agent_id } => {
                if crate::secrets::get(&agent_id).is_none() {
                    let bundle = handle.pull_secrets(agent_id).await?;
                    crate::secrets::put(agent_id, bundle);
                }
                let record = supervisor.start(&agent_id.to_string()).await?;
                Ok(serde_json::to_value(record)?)
            }
            Order::StopAgent { agent_id, .. } => {
                supervisor.stop(&agent_id.to_string()).await?;
                Ok(json!({}))
            }
            Order::SuspendAgent { agent_id } => {
                // Graceful stop + disk checkpoint, then ship the restore
                // bundle (session files + pi-home) to the control plane.
                supervisor.suspend(&agent_id.to_string()).await?;
                let record = state::load(&agent_id).await?;
                handle.upload_bundle(&record).await?;
                Ok(json!({}))
            }
            Order::DestroyAgent { agent_id } => {
                supervisor.destroy(&agent_id.to_string()).await?;
                Ok(json!({}))
            }
            Order::Ping { .. } => {
                let capacity = crate::probe::capacity(&state::data_dir());
                let usage = crate::probe::usage(&state::data_dir(), &capacity);
                Ok(json!({"pong": true, "capacity": capacity, "usage": usage}))
            }
            other => bail!("unsupported order: {other:?}"),
        }
    }
    .await;

    match result {
        Ok(data) => OrderAck {
            success: true,
            message: None,
            data: Some(data),
        },
        Err(err) => OrderAck {
            success: false,
            message: Some(format!("{err:#}")),
            data: None,
        },
    }
}

/// Suzerain-initiated streams (attach relay).
async fn handle_inbound_stream(
    supervisor: Arc<Supervisor>,
    mut send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let mut recv = BufReader::new(recv);
    let hello: StreamHello = read_jsonl(&mut recv).await?;
    match hello {
        StreamHello::Attach { agent_id } => {
            let mut events = supervisor.subscribe(&agent_id.to_string()).await?;
            loop {
                tokio::select! {
                    msg = read_jsonl::<_, AttachMessage>(&mut recv) => {
                        match msg {
                            Ok(AttachMessage::Prompt { message }) => {
                                supervisor.prompt(&agent_id.to_string(), &message).await?;
                            }
                            Ok(AttachMessage::Steer { message }) => {
                                if let Some(running) = supervisor.running(&agent_id).await {
                                    running.pi().await.steer(&message).await?;
                                }
                            }
                            Ok(AttachMessage::FollowUp { message }) => {
                                if let Some(running) = supervisor.running(&agent_id).await {
                                    running.pi().await.follow_up(&message).await?;
                                }
                            }
                            Ok(AttachMessage::Abort) => {
                                if let Some(running) = supervisor.running(&agent_id).await {
                                    running.abort().await?;
                                }
                            }
                            Ok(_) => {}
                            Err(FramingError::Eof) => break,
                            Err(err) => return Err(err.into()),
                        }
                    }
                    event = events.recv() => {
                        match event {
                            Ok(ev) => {
                                write_jsonl(&mut send, &AttachMessage::Event { event: ev }).await?;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
            Ok(())
        }
        StreamHello::Restore { agent_id } => handle_restore(supervisor, agent_id, send, recv).await,
        other => bail!("unexpected stream hello: {other:?}"),
    }
}

/// Receive a restore bundle from suzerain: write files into the agent dir,
/// then provision + start with session resume. Replies with a BundleAck.
async fn handle_restore(
    supervisor: Arc<Supervisor>,
    agent_id: Uuid,
    mut send: iroh::endpoint::SendStream,
    mut recv: BufReader<iroh::endpoint::RecvStream>,
) -> Result<()> {
    let paths = AgentPaths::for_agent(&agent_id);
    let result = async {
        // Start message first.
        let (manifest, session_file) = match read_jsonl(&mut recv).await? {
            BundleMessage::Start {
                manifest,
                session_file,
                secrets,
            } => {
                if let Some(bundle) = secrets {
                    crate::secrets::put(agent_id, bundle);
                }
                (manifest, session_file)
            }
            other => bail!("expected bundle start, got {other:?}"),
        };
        // File chunks until End.
        loop {
            match read_jsonl(&mut recv).await? {
                BundleMessage::File {
                    path,
                    data,
                    last: _,
                    sha256,
                } => {
                    if path.contains("..") {
                        bail!("unsafe bundle path: {path}");
                    }
                    let decoded = base64_decode(&data)?;
                    if let Some(want) = sha256 {
                        let got = suzerain_protocol::framing::sha256_hex(&decoded);
                        if got != want {
                            bail!("bundle checksum mismatch for {path}");
                        }
                    }
                    let dest = paths.guest.join(&path);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, decoded)?;
                }
                BundleMessage::End => break,
                other => bail!("unexpected bundle message: {other:?}"),
            }
        }
        // Fresh boot + provision + session resume.
        let record = supervisor
            .restore(agent_id, *manifest, session_file)
            .await?;
        Ok::<_, anyhow::Error>(record)
    }
    .await;

    let ack = match &result {
        Ok(_) => BundleAck {
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
    result?;
    Ok(())
}

/// Shared handle to the live control connection (for bundle uploads
/// triggered from order handlers).
#[derive(Clone)]
pub struct ControlHandle {
    conn: iroh::endpoint::Connection,
}

impl ControlHandle {
    /// Pull a freshly-sliced secret bundle for an agent (G7: memory-only
    /// secrets; re-pulled after daemon restart).
    pub async fn pull_secrets(&self, agent_id: Uuid) -> Result<SecretBundle> {
        let (mut send, recv) = self.conn.open_bi().await?;
        write_jsonl(&mut send, &StreamHello::Secrets { agent_id }).await?;
        send.finish()?;
        let mut recv = BufReader::new(recv);
        let payload: Value = read_jsonl(&mut recv).await?;
        if let Some(err) = payload["error"].as_str() {
            bail!("secrets pull rejected: {err}");
        }
        Ok(serde_json::from_value(payload)?)
    }

    /// Upload an agent's restore bundle (session files + pi-home) to the
    /// control plane.
    pub async fn upload_bundle(&self, record: &state::AgentRecord) -> Result<()> {
        let (mut send, recv) = self.conn.open_bi().await?;
        write_jsonl(
            &mut send,
            &StreamHello::BundleUpload {
                agent_id: record.id,
            },
        )
        .await?;
        write_jsonl(
            &mut send,
            &BundleMessage::Start {
                manifest: Box::new(record.manifest.clone()),
                session_file: record.session_file.clone(),
                secrets: None, // never persisted in bundles; re-sliced on restore
            },
        )
        .await?;

        let paths = AgentPaths::for_agent(&record.id);
        for rel in bundle_files(&paths) {
            let abs = paths.guest.join(&rel);
            let data = std::fs::read(&abs).with_context(|| format!("reading {}", abs.display()))?;
            write_jsonl(
                &mut send,
                &BundleMessage::File {
                    path: rel,
                    sha256: Some(suzerain_protocol::framing::sha256_hex(&data)),
                    data: base64_encode(&data),
                    last: true,
                },
            )
            .await?;
        }
        write_jsonl(&mut send, &BundleMessage::End).await?;
        send.finish()?;

        let mut recv = BufReader::new(recv);
        let ack: BundleAck = read_jsonl(&mut recv).await?;
        if !ack.success {
            bail!(
                "bundle upload rejected: {}",
                ack.message.unwrap_or_default()
            );
        }
        Ok(())
    }
}

/// The files that make up a restore bundle (relative to the guest dir):
/// pi session logs + pi-home config. Workspace re-clones from git; the
/// toolchain reinstalls (manifest-pinned).
fn bundle_files(paths: &AgentPaths) -> Vec<String> {
    let mut out = Vec::new();
    for sub in ["sessions", "pi-home"] {
        let dir = paths.guest.join(sub);
        collect_files(&dir, &mut out, sub);
    }
    out
}

fn collect_files(dir: &std::path::Path, out: &mut Vec<String>, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = format!("{}/{}", prefix, entry.file_name().to_string_lossy());
        if path.is_dir() {
            collect_files(&path, out, &rel);
        } else if path.is_file() {
            out.push(rel);
        }
    }
}

fn base64_encode(data: &[u8]) -> String {
    // Minimal base64 (std) encoder to avoid a dependency for one call site.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Result<u32> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => bail!("invalid base64 character"),
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            if *c != b'=' {
                n |= val(*c)? << (18 - 6 * i);
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Ship unacked journal events for every local agent; prune fully-acked
/// journals of agents that are not running (logs live on suzerain forever).
/// Per-agent ship/bundle bookkeeping (in-memory; debounce only).
#[derive(Default)]
struct ShipState {
    last_activity: Option<Instant>,
    last_upload: Option<Instant>,
}

async fn ship_pending_logs(
    conn: &iroh::endpoint::Connection,
    supervisor: &Arc<Supervisor>,
    ship_state: &mut HashMap<Uuid, ShipState>,
) -> Result<()> {
    let agents = state::list().await?;
    for record in agents {
        let paths = AgentPaths::for_agent(&record.id);
        let journal_path = paths.root.join("journal.jsonl");
        if !journal_path.exists() {
            continue;
        }
        let watermark_path = paths.root.join(".shipped");
        let shipped: u64 = std::fs::read_to_string(&watermark_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let events: Vec<LogEvent> = Journal::read_all(&paths.root)
            .await?
            .into_iter()
            .filter(|e| e.seq > shipped)
            .collect();
        if events.is_empty() {
            continue;
        }
        let max_seq = events.iter().map(|e| e.seq).max().unwrap_or(shipped);

        let (mut send, recv) = conn.open_bi().await?;
        write_jsonl(
            &mut send,
            &StreamHello::Logs {
                agent_id: record.id,
            },
        )
        .await?;
        write_jsonl(
            &mut send,
            &LogBatch {
                agent_id: record.id,
                events,
            },
        )
        .await?;
        send.finish()?;
        let mut recv = BufReader::new(recv);
        let ack: LogAck = read_jsonl(&mut recv).await?;
        if ack.acked_through >= max_seq {
            std::fs::write(&watermark_path, ack.acked_through.to_string())?;
            ship_state.entry(record.id).or_default().last_activity = Some(Instant::now());
            // Prune only agents that are genuinely not running: active
            // agents' logs are in use, and rewriting the journal under a
            // running agent's open append handle would detach it.
            let not_running = supervisor.running(&record.id).await.is_none()
                && matches!(
                    record.state,
                    suzerain_protocol::state::AgentState::Suspended
                );
            if not_running {
                prune_journal(&paths, ack.acked_through).await?;
            }
        }
    }
    Ok(())
}

/// Event-driven bundle freshness (G3): upload once the agent has been quiet
/// for `quiet_secs` after journal activity, with a `max_stale_secs` backstop
/// so a continuously busy agent still gets refreshed. Idle agents upload
/// nothing.
async fn refresh_bundles(
    handle: &ControlHandle,
    supervisor: &Arc<Supervisor>,
    ship_state: &mut HashMap<Uuid, ShipState>,
    quiet_secs: u64,
    max_stale_secs: u64,
) -> Result<()> {
    for record in state::list().await? {
        if supervisor.running(&record.id).await.is_none() {
            continue; // only running agents need fresh bundles
        }
        let st = ship_state.entry(record.id).or_default();
        let now = Instant::now();
        let since_activity = st.last_activity.map(|t| now.duration_since(t).as_secs());
        let since_upload = st.last_upload.map(|t| now.duration_since(t).as_secs());

        let dirty = st
            .last_activity
            .zip(st.last_upload)
            .map(|(a, u)| a > u)
            .unwrap_or(st.last_activity.is_some());
        let quiet_enough = since_activity.map(|s| s >= quiet_secs).unwrap_or(false);
        let too_stale = since_upload.map(|s| s >= max_stale_secs).unwrap_or(false);

        let due = (dirty && quiet_enough) || too_stale;
        if !due {
            continue;
        }
        // Refresh the record (session file advances over the agent's life).
        let record = state::load(&record.id).await.unwrap_or(record);
        handle.upload_bundle(&record).await?;
        ship_state.entry(record.id).or_default().last_upload = Some(Instant::now());
        let paths = AgentPaths::for_agent(&record.id);
        std::fs::write(paths.root.join(".bundle_uploaded"), "")?;
        tracing::info!(agent = %record.name, "bundle refreshed");
    }
    Ok(())
}

/// Drop acked events from the local journal (suzerain has them durably).
async fn prune_journal(paths: &AgentPaths, acked_through: u64) -> Result<()> {
    let events = Journal::read_all(&paths.root).await?;
    let kept: Vec<&LogEvent> = events.iter().filter(|e| e.seq > acked_through).collect();
    if kept.len() == events.len() {
        return Ok(());
    }
    let mut buf = Vec::new();
    for ev in kept {
        buf.extend_from_slice(&serde_json::to_vec(ev)?);
        buf.push(b'\n');
    }
    let tmp = paths.root.join("journal.jsonl.tmp");
    tokio::fs::write(&tmp, &buf).await?;
    tokio::fs::rename(&tmp, paths.root.join("journal.jsonl")).await?;
    Ok(())
}
