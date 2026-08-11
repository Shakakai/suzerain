//! Operator API: unix-socket JSONL server for the `suz` CLI. Same pattern as
//! castellan's Phase 1 socket; a browser/remote API is a later phase.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::order::Order;
use suzerain_protocol::state::AgentState;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::info;
use uuid::Uuid;

use crate::audit;
use crate::control::ControlPlane;
use crate::identity::data_dir;
use crate::scheduler;
use crate::store::AgentRow;

pub fn socket_path() -> PathBuf {
    data_dir().join("suzerain.sock")
}

pub async fn serve(cp: Arc<ControlPlane>) -> Result<()> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::remove_file(&path).await.ok();
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    info!(socket = %path.display(), "suzerain api listening");

    loop {
        let (stream, _) = listener.accept().await?;
        // G6: same-local-user only (single-operator model).
        if !suzerain_protocol::peercred::same_user(&stream) {
            tracing::warn!("rejected operator connection from a different uid");
            continue;
        }
        let cp = Arc::clone(&cp);
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, cp).await {
                tracing::warn!("api connection error: {err:#}");
            }
        });
    }
}

async fn handle_conn(stream: tokio::net::UnixStream, cp: Arc<ControlPlane>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg["id"].clone();
        let cmd = msg["cmd"].as_str().unwrap_or("").to_string();
        if cmd == "agent_attach" {
            // Streaming mode: reply, send history, then relay live.
            let name = msg["name"].as_str().unwrap_or("").to_string();
            let reply = match attach_setup(&cp, &name).await {
                Ok(v) => json!({"id": id, "ok": true, "result": v}),
                Err(err) => json!({"id": id, "ok": false, "error": format!("{err:#}")}),
            };
            let mut buf = serde_json::to_vec(&reply)?;
            buf.push(b'\n');
            writer.write_all(&buf).await?;
            writer.flush().await?;
            if reply["ok"].as_bool() == Some(true) {
                return attach_relay(&cp, &name, lines, writer).await;
            }
            continue;
        }
        let result = dispatch(&msg, &cp).await;
        let reply = match result {
            Ok(value) => json!({"id": id, "ok": true, "result": value}),
            Err(err) => json!({"id": id, "ok": false, "error": format!("{err:#}")}),
        };
        let mut buf = serde_json::to_vec(&reply)?;
        buf.push(b'\n');
        if writer.write_all(&buf).await.is_err() {
            break;
        }
        writer.flush().await.ok();
    }
    Ok(())
}

/// Validate the agent exists and is running somewhere; returns its row info.
async fn attach_setup(cp: &Arc<ControlPlane>, name: &str) -> Result<Value> {
    let store = cp.store();
    let agent = store
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    Ok(json!({"agent_id": agent.id, "daemon": agent.daemon_endpoint_id}))
}

/// Attach relay: history from the central log, then live events both ways.
async fn attach_relay(
    cp: &Arc<ControlPlane>,
    name: &str,
    mut lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let agent = cp
        .store()
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;

    // 1. History: message_end events from the central log.
    let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
    if let Ok(content) = tokio::fs::read_to_string(&log).await {
        for line in content.lines() {
            let Ok(ev) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if ev["kind"] == "message_end" {
                let mut buf =
                    serde_json::to_vec(&json!({"event": ev["payload"], "history": true}))?;
                buf.push(b'\n');
                writer.write_all(&buf).await?;
            }
        }
        writer.flush().await?;
    }
    let mut marker = serde_json::to_vec(&json!({"event": {"type": "history_end"}}))?;
    marker.push(b'\n');
    writer.write_all(&marker).await?;
    writer.flush().await?;

    // 2. Live relay via the daemon attach stream.
    let (mut send, mut recv) = cp
        .open_stream(
            &daemon,
            &suzerain_protocol::control::StreamHello::Attach { agent_id: agent.id },
        )
        .await?;
    use suzerain_protocol::control::AttachMessage;
    use suzerain_protocol::framing::{read_jsonl, write_jsonl};
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                if msg["cmd"] == "prompt" {
                    let message = msg["message"].as_str().unwrap_or("").to_string();
                    write_jsonl(&mut send, &AttachMessage::Prompt { message }).await?;
                }
            }
            msg = read_jsonl::<_, AttachMessage>(&mut recv) => {
                match msg {
                    Ok(AttachMessage::Event { event }) => {
                        let mut buf = serde_json::to_vec(&json!({"event": event}))?;
                        buf.push(b'\n');
                        if writer.write_all(&buf).await.is_err() { break; }
                        writer.flush().await.ok();
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}

async fn dispatch(msg: &Value, cp: &Arc<ControlPlane>) -> Result<Value> {
    let cmd = msg["cmd"].as_str().unwrap_or("");
    let store = cp.store().clone();
    match cmd {
        "endpoint_id" => Ok(json!({"endpoint_id": cp.endpoint_id().to_string()})),
        "daemon_approve" => {
            let id = msg["endpoint_id"]
                .as_str()
                .context("endpoint_id required")?;
            // Validate it parses as an EndpointId.
            id.parse::<iroh::EndpointId>()
                .context("invalid endpoint id")?;
            store.approve_daemon(id).await?;
            audit::record("daemon_approve", json!({"endpoint_id": id})).await;
            Ok(json!({"approved": id}))
        }
        "daemon_list" => {
            let daemons = store.list_daemons().await?;
            Ok(serde_json::to_value(daemons)?)
        }
        "agent_create" => {
            let manifest: AgentManifest =
                serde_json::from_value(msg["manifest"].clone()).context("invalid manifest")?;
            if store.get_agent_by_name(&manifest.name).await?.is_some() {
                bail!("an agent named '{}' already exists", manifest.name);
            }
            let placement = scheduler::place(cp, msg["daemon"].as_str()).await?;
            let agent_id = Uuid::new_v4();
            let row = AgentRow {
                id: agent_id,
                name: manifest.name.clone(),
                daemon_endpoint_id: placement.endpoint_id.to_string(),
                manifest: manifest.clone(),
                state: AgentState::Provisioning,
                created_at: crate::store::castellan_time_now(),
                session_file: None,
            };
            store.create_agent(&row).await?;
            let ack = cp
                .order(
                    &placement.endpoint_id,
                    &Order::CreateAgent {
                        agent_id,
                        secrets: crate::secrets::slice_for(&manifest)?,
                        manifest,
                    },
                )
                .await?;
            if !ack.success {
                store
                    .update_agent_state(&agent_id, AgentState::Failed)
                    .await?;
                bail!(
                    "daemon rejected create: {}",
                    ack.message.unwrap_or_default()
                );
            }
            // Daemon returns its record (with session file) as ack data.
            if let Some(data) = &ack.data {
                if let Some(sf) = data["session_file"].as_str() {
                    store.set_agent_session_file(&agent_id, sf).await?;
                }
            }
            store
                .update_agent_state(&agent_id, AgentState::Active)
                .await?;
            audit::record(
                "agent_create",
                json!({"name": row.name, "id": agent_id, "daemon": row.daemon_endpoint_id}),
            )
            .await;
            let agent = store.get_agent_by_name(&row.name).await?.unwrap();
            Ok(serde_json::to_value(agent)?)
        }
        "secrets_status" => Ok(json!({"entries": crate::secrets::status()})),
        "audit_tail" => {
            let n = msg["tail"].as_u64().unwrap_or(50) as usize;
            Ok(json!({"entries": audit::read_tail(n).await?}))
        }
        "agent_list" => Ok(serde_json::to_value(store.list_agents().await?)?),
        "agent_start" | "agent_stop" | "agent_suspend" | "agent_destroy" => {
            let name = msg["name"].as_str().context("name required")?;
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
            let order = match cmd {
                "agent_start" => Order::StartAgent { agent_id: agent.id },
                "agent_stop" => Order::StopAgent {
                    agent_id: agent.id,
                    cleanup_timeout_secs: 30,
                },
                "agent_suspend" => Order::SuspendAgent { agent_id: agent.id },
                _ => Order::DestroyAgent { agent_id: agent.id },
            };
            let ack = cp.order(&daemon, &order).await;
            match &ack {
                Ok(ack) if !ack.success => {
                    // Destroy tolerates a daemon that has no record (e.g. a
                    // failed create left only the control-plane row).
                    let tolerable = cmd == "agent_destroy"
                        && ack.message.as_deref().unwrap_or("").contains("no agent");
                    if !tolerable {
                        bail!("daemon: {}", ack.message.clone().unwrap_or_default());
                    }
                }
                Err(_) if cmd != "agent_destroy" => {
                    bail!("order failed: daemon unreachable");
                }
                Err(_) => {}
                _ => {}
            }
            match cmd {
                "agent_start" => {
                    store
                        .update_agent_state(&agent.id, AgentState::Active)
                        .await?
                }
                "agent_stop" | "agent_suspend" => {
                    store
                        .update_agent_state(&agent.id, AgentState::Suspended)
                        .await?
                }
                _ => {
                    store.delete_agent(&agent.id).await?;
                }
            }
            audit::record(
                cmd.trim_start_matches("agent_"),
                json!({"name": name, "id": agent.id}),
            )
            .await;
            Ok(json!({"ok": true}))
        }
        "agent_ask" => {
            let name = msg["name"].as_str().context("name required")?;
            let message = msg["message"].as_str().context("message required")?;
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
            let (mut send, mut recv) = cp
                .open_stream(
                    &daemon,
                    &suzerain_protocol::control::StreamHello::Attach { agent_id: agent.id },
                )
                .await?;
            use suzerain_protocol::control::AttachMessage;
            use suzerain_protocol::framing::{read_jsonl, write_jsonl};
            write_jsonl(
                &mut send,
                &AttachMessage::Prompt {
                    message: message.into(),
                },
            )
            .await?;
            // Track the final assistant text from message_end events directly
            // on the stream (the central log lags by the ship interval).
            let mut last_text = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(300), async {
                while let Ok(AttachMessage::Event { event }) =
                    read_jsonl::<_, AttachMessage>(&mut recv).await
                {
                    let t = event["type"].as_str().unwrap_or("");
                    if t == "message_end" {
                        let msg = &event["message"];
                        if msg["role"] == "assistant" {
                            if let Some(parts) = msg["content"].as_array() {
                                let text: String = parts
                                    .iter()
                                    .filter(|p| p["type"] == "text")
                                    .filter_map(|p| p["text"].as_str())
                                    .collect();
                                if !text.is_empty() {
                                    last_text = text;
                                }
                            }
                        }
                    }
                    if t == "agent_end" || t == "agent_settled" {
                        break;
                    }
                }
            })
            .await
            .context("ask timed out")?;
            Ok(json!({"text": last_text}))
        }
        "agent_restore" => {
            let name = msg["name"].as_str().context("name required")?;
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            if agent.state == AgentState::Active {
                // Active means running somewhere. If the owning daemon is
                // offline, that conviction is stale — restore may proceed.
                let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
                if cp.session(&daemon).await.is_some() {
                    bail!("agent '{name}' is currently active — stop or suspend it first");
                }
            }
            let bundle = crate::bundle::load(&agent.id).await?;
            let target = scheduler::place(cp, msg["daemon"].as_str()).await?;
            store
                .update_agent_state(&agent.id, AgentState::Restoring)
                .await?;

            let (mut send, mut recv) = cp
                .open_stream(
                    &target.endpoint_id,
                    &suzerain_protocol::control::StreamHello::Restore { agent_id: agent.id },
                )
                .await?;
            use suzerain_protocol::control::{BundleAck, BundleMessage};
            use suzerain_protocol::framing::{read_jsonl, write_jsonl};
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
                write_jsonl(
                    &mut send,
                    &BundleMessage::File {
                        path: rel.clone(),
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
                store
                    .update_agent_state(&agent.id, AgentState::Failed)
                    .await?;
                bail!("restore failed: {}", ack.message.unwrap_or_default());
            }
            store
                .set_agent_daemon(&agent.id, &target.endpoint_id.to_string())
                .await?;
            store
                .update_agent_state(&agent.id, AgentState::Active)
                .await?;
            audit::record(
                "agent_restore",
                json!({"name": name, "id": agent.id, "daemon": target.endpoint_id.to_string()}),
            )
            .await;
            Ok(json!({"restored": name, "daemon": target.endpoint_id.to_string()}))
        }
        "agent_logs" => {
            let name = msg["name"].as_str().context("name required")?;
            let tail = msg["tail"].as_u64().unwrap_or(50) as usize;
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
            let content = tokio::fs::read_to_string(&log).await.unwrap_or_default();
            let events: Vec<Value> = content
                .lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect();
            let start = events.len().saturating_sub(tail);
            Ok(json!({"events": &events[start..]}))
        }
        _ => bail!("unknown command '{cmd}'"),
    }
}
