//! Control client: connects this daemon to suzerain over iroh, registers,
//! serves orders on the long-lived register stream, accepts attach streams,
//! and ships event logs with ack-based pruning.
//!
//! Connection discipline (docs/PHASE0-FINDINGS.md): the control connection
//! is established FIRST and held; gossip joins afterwards.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::{endpoint::presets, Endpoint, EndpointId, SecretKey};
use iroh_gossip::Gossip;
use iroh_mdns_address_lookup::MdnsAddressLookup;
use n0_future::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use suzerain_protocol::alpn;
use suzerain_protocol::control::{AttachMessage, Register, RegisterResponse, StreamHello};
use suzerain_protocol::event::{LogAck, LogBatch, LogEvent};
use suzerain_protocol::framing::{read_jsonl, write_jsonl, FramingError};
use suzerain_protocol::order::{Order, OrderAck};
use tokio::io::BufReader;
use tracing::{info, warn};

use crate::journal::Journal;
use crate::state::{self, AgentPaths};
use crate::supervisor::Supervisor;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CastellanConfig {
    /// Suzerain's EndpointId (set by `castellan init --suzerain <id>`).
    #[serde(default)]
    pub suzerain_endpoint_id: Option<String>,
    /// Free-form scheduling labels reported at registration.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_max_agents")]
    pub max_agents: u32,
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

async fn connect_and_serve(
    supervisor: &Arc<Supervisor>,
    secret: &SecretKey,
    suzerain: EndpointId,
    config: &CastellanConfig,
) -> Result<()> {
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret.clone())
        .bind()
        .await?;
    let mdns = MdnsAddressLookup::builder().build(endpoint.id())?;
    endpoint.address_lookup()?.add(mdns);

    // Control connection FIRST (see findings), gossip after.
    let conn = endpoint.connect(suzerain, alpn::CONTROL).await?;
    let (mut order_tx, order_rx) = conn.open_bi().await?;
    let mut order_rx = BufReader::new(order_rx);

    let info = suzerain_protocol::state::DaemonInfo {
        endpoint_id: endpoint.id().to_string(),
        hostname: hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        labels: config.labels.clone(),
        max_agents: config.max_agents,
        agents: vec![],
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

    // Task: ship logs for all locally running agents.
    let ship_conn = conn.clone();
    let ship_sup = Arc::clone(supervisor);
    let ship_task = tokio::spawn(async move {
        loop {
            if let Err(err) = ship_pending_logs(&ship_conn, &ship_sup).await {
                warn!("log shipping error: {err:#}");
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
        let ack = dispatch_order(supervisor, order).await;
        write_jsonl(&mut order_tx, &ack).await?;
    }

    stream_task.abort();
    ship_task.abort();
    router.shutdown().await.ok();
    endpoint.close().await;
    Ok(())
}

async fn dispatch_order(supervisor: &Arc<Supervisor>, order: Order) -> OrderAck {
    let result: Result<Value> = async {
        match order {
            Order::CreateAgent { agent_id, manifest } => {
                let record = supervisor.create(Some(agent_id), manifest).await?;
                Ok(serde_json::to_value(record)?)
            }
            Order::StartAgent { agent_id } => {
                let record = supervisor.start(&agent_id.to_string()).await?;
                Ok(serde_json::to_value(record)?)
            }
            Order::StopAgent { agent_id, .. } | Order::SuspendAgent { agent_id } => {
                supervisor.stop(&agent_id.to_string()).await?;
                // Final log flush happens in the shipper; give it a beat.
                Ok(json!({}))
            }
            Order::DestroyAgent { agent_id } => {
                supervisor.destroy(&agent_id.to_string()).await?;
                Ok(json!({}))
            }
            Order::Ping { .. } => Ok(json!({"pong": true})),
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
        other => bail!("unexpected stream hello: {other:?}"),
    }
}

use tokio::sync::broadcast;

/// Ship unacked journal events for every local agent; prune fully-acked
/// journals of agents that are not running (logs live on suzerain forever).
async fn ship_pending_logs(
    conn: &iroh::endpoint::Connection,
    supervisor: &Arc<Supervisor>,
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
