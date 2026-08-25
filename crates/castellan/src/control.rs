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
    config_path_in(&state::data_dir())
}

/// `castellan.toml` within `dir` (disjoint from suzerain's `suzerain.toml`
/// in the shared fleet home); a legacy `config.toml` is renamed in place.
pub fn config_path_in(dir: &std::path::Path) -> PathBuf {
    let new = dir.join("castellan.toml");
    let legacy = dir.join("config.toml");
    if !new.exists() && legacy.exists() {
        match std::fs::rename(&legacy, &new) {
            Ok(()) => tracing::info!("migrated config.toml → castellan.toml"),
            Err(err) => {
                tracing::warn!("renaming config.toml failed ({err:#}); using legacy name");
                return legacy;
            }
        }
    }
    new
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

/// `castellan.key` within `dir` (disjoint from suzerain's `suzerain.key`
/// in the shared fleet home); a legacy `identity.key` is renamed in place.
pub fn identity_path_in(dir: &std::path::Path) -> std::path::PathBuf {
    let new = dir.join("castellan.key");
    let legacy = dir.join("identity.key");
    if !new.exists() && legacy.exists() {
        match std::fs::rename(&legacy, &new) {
            Ok(()) => tracing::info!("migrated identity.key → castellan.key"),
            Err(err) => {
                tracing::warn!("renaming identity.key failed ({err:#}); using legacy name");
                return legacy;
            }
        }
    }
    new
}

/// Load or create this daemon's iroh identity.
pub fn identity() -> Result<SecretKey> {
    let path = identity_path_in(&state::data_dir());
    if path.exists() {
        let bytes = std::fs::read(&path)?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .with_context(|| format!("{} is not 32 bytes", path.display()))?;
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
        info!(
            "no suzerain configured — run `castellan init --suzerain <id>` to point this \
             agent at a control plane, or run `suzerain run` for a merged single-box \
             deployment instead of `castellan run` directly; nothing to do, exiting"
        );
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
    write_jsonl(
        &mut order_tx,
        &Register {
            info,
            protocol_version: suzerain_protocol::control::PROTOCOL_VERSION,
        },
    )
    .await?;
    let response: RegisterResponse = read_jsonl(&mut order_rx).await?;
    if !response.accepted {
        bail!(
            "suzerain rejected us (its protocol_version={}): {}",
            response.protocol_version,
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

    // Main loop: read orders, dispatch, ack. Dispatches are sequential
    // (acks must stay FIFO — suzerain matches them strictly by read order),
    // but the read stays live while a dispatch runs so a wedged/long
    // dispatch (e.g. provisioning under host memory pressure) doesn't
    // blind us to connection death: on EOF the in-flight dispatch is
    // aborted and the control client reconnects, resyncing state.
    let mut in_flight: Option<tokio::task::JoinHandle<OrderAck>> = None;
    loop {
        let order: Order = tokio::select! {
            read = read_jsonl(&mut order_rx) => {
                match read {
                    Ok(o) => o,
                    Err(FramingError::Eof) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            ack = async { in_flight.as_mut().unwrap().await }, if in_flight.is_some() => {
                let ack = ack.unwrap_or_else(|e| OrderAck {
                    success: false,
                    message: Some(format!("dispatch aborted: {e}")),
                    data: None,
                });
                in_flight = None;
                write_jsonl(&mut order_tx, &ack).await?;
                continue;
            }
        };
        if in_flight.is_some() {
            // Previous dispatch still running: suzerain sends strictly one
            // order at a time (its request/response lock), so this only
            // happens after a suzerain-side timeout abandoned an order.
            // Read but don't pile up: wait for the in-flight dispatch.
            let ack = in_flight
                .take()
                .unwrap()
                .await
                .unwrap_or_else(|e| OrderAck {
                    success: false,
                    message: Some(format!("dispatch aborted: {e}")),
                    data: None,
                });
            write_jsonl(&mut order_tx, &ack).await?;
        }
        in_flight = Some(tokio::spawn(dispatch_order(
            Arc::clone(supervisor),
            order,
            handle.clone(),
        )));
    }
    if let Some(handle) = in_flight.take() {
        handle.abort();
    }

    stream_task.abort();
    ship_task.abort();
    router.shutdown().await.ok();
    endpoint.close().await;
    Ok(())
}

/// Report local agent states to suzerain: snapshot at registration, a
/// periodic full re-snapshot (so the registry reconverges even when a
/// transition report was lost — e.g. during a provisioning wedge), plus
/// every transition observed on the supervisor's state-event channel.
async fn run_state_reporter(
    conn: iroh::endpoint::Connection,
    supervisor: Arc<Supervisor>,
) -> Result<()> {
    let (mut send, _recv) = conn.open_bi().await?;
    write_jsonl(&mut send, &StreamHello::StateReport).await?;

    let full_snapshot = |sup: Arc<Supervisor>| async move {
        let snapshot = state::list().await?;
        let mut entries: Vec<suzerain_protocol::AgentStateEntry> = snapshot
            .iter()
            .map(|r| suzerain_protocol::AgentStateEntry {
                agent_id: r.id,
                name: r.name.clone(),
                state: r.state,
                idle_secs: None,
                busy: None,
                session_file: r.session_file.clone(),
            })
            .collect();
        for entry in &mut entries {
            enrich_activity(&sup, entry).await;
        }
        Ok::<_, anyhow::Error>(suzerain_protocol::StateReport {
            agents: entries,
            full: true,
        })
    };
    write_jsonl(&mut send, &full_snapshot(Arc::clone(&supervisor)).await?).await?;

    let mut rx = supervisor.subscribe_state_events();
    let mut resnapshot = tokio::time::interval(Duration::from_secs(60));
    resnapshot.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            entry = rx.recv() => {
                match entry {
                    Ok(mut entry) => {
                        enrich_activity(&supervisor, &mut entry).await;
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
            _ = resnapshot.tick() => {
                write_jsonl(&mut send, &full_snapshot(Arc::clone(&supervisor)).await?).await?;
            }
        }
    }
    Ok(())
}

/// Fill in live idle/busy facts for a state entry. Running agents report
/// their in-memory clock; stopped agents derive idle time from the
/// persisted last-activity timestamp so the clock survives restarts and
/// suspensions.
async fn enrich_activity(
    supervisor: &Arc<Supervisor>,
    entry: &mut suzerain_protocol::AgentStateEntry,
) {
    if let Ok(rec) = state::load(&entry.agent_id).await {
        entry.session_file = rec.session_file.clone();
        if let Some((idle, busy)) = supervisor.activity(&entry.agent_id).await {
            entry.idle_secs = Some(idle);
            entry.busy = Some(busy);
            return;
        }
        entry.busy = Some(false);
        if let Some(at) = &rec.last_activity_at {
            if let Ok(t) =
                time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
            {
                let secs = (time::OffsetDateTime::now_utc() - t).whole_seconds();
                entry.idle_secs = Some(secs.max(0) as u64);
            }
        }
        return;
    }
    entry.busy = supervisor
        .activity(&entry.agent_id)
        .await
        .map(|(_, busy)| busy);
}

async fn dispatch_order(
    supervisor: Arc<Supervisor>,
    order: Order,
    handle: ControlHandle,
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
            Order::StartAgent { agent_id, force } => {
                if crate::secrets::get(&agent_id).is_none() {
                    let bundle = handle.pull_secrets(agent_id).await?;
                    crate::secrets::put(agent_id, bundle);
                }
                let record = supervisor.start(&agent_id.to_string(), force).await?;
                Ok(serde_json::to_value(record)?)
            }
            Order::StopAgent { agent_id, .. } => {
                supervisor.stop(&agent_id.to_string()).await?;
                Ok(json!({}))
            }
            Order::SuspendAgent {
                agent_id,
                only_if_idle,
                not_since,
            } => {
                // Auto-suspend/preemption orders are guarded: re-validate
                // idleness against ground truth and refuse if the agent
                // went busy since the control plane's stale snapshot.
                if only_if_idle {
                    supervisor
                        .check_suspendable(&agent_id.to_string(), not_since.as_deref())
                        .await?;
                }
                // Order matters: graceful stop → upload the bundle (the
                // session preserved centrally in full) → THEN rotate the
                // session off the guest disk, checkpoint, and close.
                supervisor.prepare_suspend(&agent_id.to_string()).await?;
                let record = state::load(&agent_id).await?;
                handle.upload_bundle(&record).await?;
                supervisor
                    .finish_suspend(&agent_id.to_string(), true, true)
                    .await?;
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
            // Attach handshake: acknowledge immediately (or explain the
            // rejection) so senders fail loudly instead of writing into a
            // dying stream and reporting success.
            let mut events = match supervisor.subscribe(&agent_id.to_string()).await {
                Ok(events) => events,
                Err(err) => {
                    let _ = write_jsonl(
                        &mut send,
                        &AttachMessage::Notice {
                            message: format!("{err:#}"),
                        },
                    )
                    .await;
                    return Err(err);
                }
            };
            write_jsonl(
                &mut send,
                &AttachMessage::Notice {
                    message: "attached".to_string(),
                },
            )
            .await?;
            supervisor.touch(&agent_id).await;
            loop {
                tokio::select! {
                    msg = read_jsonl::<_, AttachMessage>(&mut recv) => {
                        match msg {
                            Ok(AttachMessage::Prompt { message }) => {
                                // Prompt failures (e.g. pi died mid-attach)
                                // surface as notices; the stream stays up
                                // for events.
                                if let Err(err) = supervisor.prompt(&agent_id.to_string(), &message).await {
                                    write_jsonl(&mut send, &AttachMessage::Notice {
                                        message: format!("prompt rejected: {err:#}"),
                                    }).await?;
                                }
                            }
                            Ok(AttachMessage::Steer { message }) => {
                                supervisor.touch(&agent_id).await;
                                if let Some(running) = supervisor.running(&agent_id).await {
                                    running.pi().await.steer(&message).await?;
                                }
                            }
                            Ok(AttachMessage::FollowUp { message }) => {
                                supervisor.touch(&agent_id).await;
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
        StreamHello::Shell { agent_id } => handle_shell(supervisor, agent_id, send, recv).await,
        StreamHello::Restore { agent_id } => handle_restore(supervisor, agent_id, send, recv).await,
        other => bail!("unexpected stream hello: {other:?}"),
    }
}

/// Interactive pty shell relay (Suzy terminal tab): spawns a shell in the
/// agent's VM and shuttles base64 byte frames both ways. The agent must be
/// running — suzerain wakes sleeping agents before opening the stream.
async fn handle_shell(
    supervisor: Arc<Supervisor>,
    agent_id: Uuid,
    mut send: iroh::endpoint::SendStream,
    mut recv: BufReader<iroh::endpoint::RecvStream>,
) -> Result<()> {
    use suzerain_protocol::control::ShellMessage;
    let Some(running) = supervisor.running(&agent_id).await else {
        write_jsonl(
            &mut send,
            &ShellMessage::Notice {
                message: format!("agent {agent_id} is not running"),
            },
        )
        .await?;
        bail!("agent {agent_id} is not running");
    };
    let driver = running.driver().await;
    let shell_id = uuid::Uuid::new_v4().as_u128() as u32;
    if let Err(err) = driver
        .shell_spawn(
            shell_id,
            &["/bin/sh", "-l"],
            Some("/agent/workspace"),
            80,
            24,
        )
        .await
    {
        write_jsonl(
            &mut send,
            &ShellMessage::Notice {
                message: format!("shell spawn failed: {err:#}"),
            },
        )
        .await?;
        return Err(err);
    }
    write_jsonl(
        &mut send,
        &ShellMessage::Notice {
            message: "shell".to_string(),
        },
    )
    .await?;
    supervisor.touch(&agent_id).await;

    let mut events = driver.subscribe();
    loop {
        tokio::select! {
            msg = read_jsonl::<_, ShellMessage>(&mut recv) => {
                match msg {
                    Ok(ShellMessage::Data { data }) => {
                        supervisor.touch(&agent_id).await;
                        driver.shell_stdin(shell_id, &data).await?;
                    }
                    Ok(ShellMessage::Resize { cols, rows }) => {
                        driver.shell_resize(shell_id, cols, rows).await?;
                    }
                    Ok(_) => {}
                    Err(FramingError::Eof) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(crate::driver::DriverEvent::ShellData { shell, data }) if shell == shell_id => {
                        write_jsonl(&mut send, &ShellMessage::Data { data }).await?;
                    }
                    Ok(crate::driver::DriverEvent::ShellExit { shell, code }) if shell == shell_id => {
                        let _ = write_jsonl(&mut send, &ShellMessage::Exit { code: code as i64 }).await;
                        break;
                    }
                    Ok(crate::driver::DriverEvent::DriverDied) => break,
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    driver.shell_close(shell_id).await.ok();
    Ok(())
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
        // Start message first. `session_file` is accepted for wire
        // compatibility but deliberately ignored: sessions rotate on every
        // suspend and wakes always start a fresh pi session.
        let (manifest, _session_file) = match read_jsonl(&mut recv).await? {
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
                    // Sessions rotate on every suspend: old session files
                    // live in the control plane's bundle store; the agent
                    // starts a FRESH pi session on wake, so don't write
                    // them back into the guest.
                    if path.starts_with("sessions/") {
                        continue;
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
        // Fresh boot + provision + NEW pi session (rotation on suspend).
        let record = supervisor.restore(agent_id, *manifest, None).await?;
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
            // Prune only agents whose journal is genuinely closed: active
            // agents' logs are in use, an in-flight provision has a freshly
            // opened Journal appending into this very file, and rewriting
            // under an open append handle detaches it (events lost).
            let not_running = !supervisor.lifecycle_busy(&record.id).await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("cast-cfgtest-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn config_path_prefers_castellan_toml() {
        let dir = tempdir("new");
        assert_eq!(config_path_in(&dir), dir.join("castellan.toml"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_path_migrates_legacy_config_toml() {
        let dir = tempdir("legacy");
        std::fs::write(dir.join("config.toml"), "max_agents = 7\n").unwrap();
        let path = config_path_in(&dir);
        assert_eq!(path, dir.join("castellan.toml"));
        assert!(path.exists());
        assert!(!dir.join("config.toml").exists());
        let cfg: CastellanConfig =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.max_agents, 7);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn config_path_keeps_new_name_when_both_exist() {
        let dir = tempdir("both");
        std::fs::write(dir.join("config.toml"), "max_agents = 1\n").unwrap();
        std::fs::write(dir.join("castellan.toml"), "max_agents = 9\n").unwrap();
        let path = config_path_in(&dir);
        assert_eq!(path, dir.join("castellan.toml"));
        // Legacy file left untouched (operator can reconcile by hand).
        assert!(dir.join("config.toml").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identity_path_migrates_legacy_identity_key() {
        let dir = tempdir("ident");
        std::fs::write(dir.join("identity.key"), [7u8; 32]).unwrap();
        let path = identity_path_in(&dir);
        assert_eq!(path, dir.join("castellan.key"));
        assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; 32]);
        assert!(!dir.join("identity.key").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fleet_home_names_do_not_overlap() {
        let dir = tempdir("disjoint");
        let castellan = [
            config_path_in(&dir),
            identity_path_in(&dir),
            dir.join("castellan.sock"),
            dir.join("castellan.lock"),
        ];
        let suzerain = [
            dir.join("suzerain.toml"),
            dir.join("suzerain.key"),
            dir.join("suzerain.sock"),
            dir.join("suzerain.db"),
            dir.join("secrets.age"),
            dir.join("age-keys.txt"),
        ];
        for c in &castellan {
            assert!(!suzerain.contains(c), "name overlap: {}", c.display());
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
