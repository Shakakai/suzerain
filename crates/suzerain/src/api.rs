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
        let cp = Arc::clone(&cp);
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let id = msg["id"].clone();
                let result = dispatch(&msg, &cp).await;
                let reply = match result {
                    Ok(value) => json!({"id": id, "ok": true, "result": value}),
                    Err(err) => json!({"id": id, "ok": false, "error": format!("{err:#}")}),
                };
                let mut buf = serde_json::to_vec(&reply).unwrap();
                buf.push(b'\n');
                if writer.write_all(&buf).await.is_err() {
                    break;
                }
                writer.flush().await.ok();
            }
        });
    }
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
                    &Order::CreateAgent { agent_id, manifest },
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
            let agent = store.get_agent_by_name(&row.name).await?.unwrap();
            Ok(serde_json::to_value(agent)?)
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
            let ack = cp.order(&daemon, &order).await?;
            if !ack.success {
                bail!("daemon: {}", ack.message.unwrap_or_default());
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
