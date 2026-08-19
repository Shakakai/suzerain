//! Operator API: unix-socket JSONL server for the `suz` CLI. Same pattern as
//! castellan's Phase 1 socket; a browser/remote API is a later phase.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use suzerain_protocol::manifest::AgentManifest;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tracing::info;

use crate::audit;
use crate::control::ControlPlane;
use crate::identity::data_dir;

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

/// Validate the agent exists; returns its row info.
async fn attach_setup(cp: &Arc<ControlPlane>, name: &str) -> Result<Value> {
    let store = cp.store();
    let agent = store
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    Ok(json!({"agent_id": agent.id, "daemon": agent.daemon_endpoint_id}))
}

/// Attach relay: wake the agent if needed (with synthetic progress
/// notices), then history from the central log, then live events both ways.
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

    // Transparent wake: connect immediately, narrate progress, wait.
    if !crate::wake::is_awake(cp, &agent).await {
        let notice = serde_json::to_vec(
            &json!({"notice": format!("agent '{name}' is sleeping — waking…")}),
        )?;
        writer.write_all(&notice).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        if let Err(err) = crate::wake::ensure_awake(cp, &agent).await {
            let notice = serde_json::to_vec(&json!({"notice": format!("wake failed: {err:#}")}))?;
            writer.write_all(&notice).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            return Err(err);
        }
        let notice = serde_json::to_vec(&json!({"notice": "agent is awake"}))?;
        writer.write_all(&notice).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    // Reload: a wake may have moved the agent to another daemon.
    let agent = cp
        .store()
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))?;
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;

    // 1. History: message_end events from the central log, plus session
    // boundary markers so the transcript is segmented into eras.
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
            if ev["kind"] == "session_started" {
                let mut buf = serde_json::to_vec(&json!({
                    "event": {"type": "session_boundary", "session_file": ev["payload"]["session_file"], "at": ev["at"]},
                    "history": true,
                }))?;
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
                    // Forward attach-level notices (handshake ack, prompt
                    // rejections) so the operator sees them.
                    Ok(AttachMessage::Notice { message }) => {
                        let mut buf = serde_json::to_vec(&json!({"notice": message}))?;
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
        "operator_approve" => {
            let id = msg["endpoint_id"]
                .as_str()
                .context("endpoint_id required")?;
            let eid = id
                .parse::<iroh::EndpointId>()
                .context("invalid endpoint id")?;
            // Live: the running control plane accepts the id immediately
            // (no restart). Persistent: written to [operator] allow in
            // config.toml so it survives restarts.
            cp.add_operator_allow(eid);
            crate::retention::add_operator_allow(id)?;
            audit::record("operator_approve", json!({"endpoint_id": id})).await;
            Ok(json!({"approved": id}))
        }
        "operator_list" => {
            let allow: Vec<String> = cp.operator_allow().iter().map(|e| e.to_string()).collect();
            Ok(json!({"allow": allow}))
        }
        "agent_create" => {
            let manifest: AgentManifest =
                serde_json::from_value(msg["manifest"].clone()).context("invalid manifest")?;
            let mut require_extra = std::collections::BTreeMap::new();
            if let Some(obj) = msg["require"].as_object() {
                for (k, v) in obj {
                    if let Some(vs) = v.as_str() {
                        require_extra.insert(k.clone(), vs.to_string());
                    }
                }
            }
            let pin = msg["daemon"].as_str().map(str::to_string);
            let (agent, daemon_hostname) =
                crate::actions::create_agent(cp, manifest, require_extra, pin).await?;
            let mut out = serde_json::to_value(agent)?;
            out["daemon_hostname"] = json!(daemon_hostname);
            Ok(out)
        }
        "daemon_label" => {
            let id_prefix = msg["endpoint_id"]
                .as_str()
                .context("endpoint_id required")?;
            let mut daemons = store.list_daemons().await?;
            let d = daemons
                .iter_mut()
                .find(|d| d.endpoint_id.starts_with(id_prefix) || d.hostname == id_prefix)
                .with_context(|| format!("no daemon matching '{id_prefix}'"))?;
            let mut overrides: std::collections::BTreeMap<String, String> =
                serde_json::from_str(&d.label_overrides).unwrap_or_default();
            if let Some(obj) = msg["set"].as_object() {
                for (k, v) in obj {
                    if let Some(vs) = v.as_str() {
                        overrides.insert(k.clone(), vs.to_string());
                    }
                }
            }
            if let Some(arr) = msg["remove"].as_array() {
                for k in arr {
                    if let Some(ks) = k.as_str() {
                        overrides.remove(ks);
                    }
                }
            }
            store
                .set_label_overrides(&d.endpoint_id, &serde_json::to_string(&overrides)?)
                .await?;
            d.label_overrides = serde_json::to_string(&overrides)?;
            audit::record(
                "daemon_label",
                json!({"endpoint_id": d.endpoint_id, "overrides": overrides}),
            )
            .await;
            Ok(json!({"effective_labels": d.effective_labels()}))
        }
        "secrets_status" => Ok(json!({"entries": crate::secrets::status()})),
        "secret_set" => {
            let kind = msg["kind"].as_str().context("kind required")?;
            let value = msg["value"].as_str().context("value required")?;
            let name = match kind {
                "provider" => {
                    let name = msg["name"].as_str().context("name required")?;
                    crate::secrets::set_provider(name, value)?;
                    name.to_string()
                }
                "extra" => {
                    let name = msg["name"].as_str().context("name required")?;
                    crate::secrets::set_extra(name, value)?;
                    name.to_string()
                }
                "deploy_key" => {
                    crate::secrets::set_deploy_key(value)?;
                    "deploy_key".to_string()
                }
                other => bail!("unknown secret kind '{other}' (provider|extra|deploy_key)"),
            };
            audit::record(
                "secret_set",
                json!({"kind": kind, "name": name, "via": "cli"}),
            )
            .await;
            Ok(json!({"ok": true, "kind": kind, "name": name}))
        }
        "secret_delete" => {
            let kind = msg["kind"].as_str().context("kind required")?;
            let name = match kind {
                "provider" => {
                    let name = msg["name"].as_str().context("name required")?;
                    crate::secrets::delete_provider(name)?;
                    name.to_string()
                }
                "extra" => {
                    let name = msg["name"].as_str().context("name required")?;
                    crate::secrets::delete_extra(name)?;
                    name.to_string()
                }
                "deploy_key" => {
                    crate::secrets::delete_deploy_key()?;
                    "deploy_key".to_string()
                }
                other => bail!("unknown secret kind '{other}' (provider|extra|deploy_key)"),
            };
            audit::record(
                "secret_delete",
                json!({"kind": kind, "name": name, "via": "cli"}),
            )
            .await;
            Ok(json!({"ok": true, "kind": kind, "name": name}))
        }
        "audit_tail" => {
            let n = msg["tail"].as_u64().unwrap_or(50) as usize;
            Ok(json!({"entries": audit::read_tail(n).await?}))
        }
        "agent_list" => {
            let agents = store.list_agents().await?;
            let mut out = serde_json::to_value(&agents)?;
            for (i, a) in agents.iter().enumerate() {
                out[i]["status"] = json!(suzerain_protocol::state::public_status(
                    a.state,
                    a.busy == Some(true)
                ));
                out[i]["idle_secs"] = json!(crate::lifecycle::extrapolated_idle_secs(a));
            }
            Ok(out)
        }
        "agent_destroy" => {
            let name = msg["name"].as_str().context("name required")?;
            let force = msg["force"].as_bool().unwrap_or(false);
            crate::actions::destroy_agent(cp, name, force).await?;
            Ok(json!({"ok": true}))
        }
        "agent_config" => {
            let name = msg["name"].as_str().context("name required")?;
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            let value = msg["auto_suspend"]
                .as_str()
                .context("auto_suspend required")?;
            // Validate; "default"/"inherit" clears the runtime override.
            let policy = suzerain_protocol::manifest::Lifecycle {
                auto_suspend: Some(value.to_string()),
            }
            .auto_suspend_policy()
            .map_err(|e| anyhow::anyhow!(e))?;
            let stored = match policy {
                suzerain_protocol::manifest::AutoSuspendPolicy::Inherit => None,
                _ => Some(value),
            };
            store.set_auto_suspend_override(&agent.id, stored).await?;
            audit::record(
                "agent_config",
                json!({"name": name, "id": agent.id, "auto_suspend": value}),
            )
            .await;
            Ok(json!({"ok": true, "auto_suspend": stored}))
        }
        "agent_ask" => {
            let name = msg["name"].as_str().context("name required")?;
            let message = msg["message"].as_str().context("message required")?;
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            // Transparent wake: if the agent is sleeping the message is
            // queued durably and delivered by the wake task (coalesced);
            // otherwise we prompt directly below.
            let queued = crate::wake::deliver_message(cp, &agent, message).await?;
            // Reload: a wake may have moved the agent to another daemon.
            let agent = store
                .get_agent_by_name(name)
                .await?
                .with_context(|| format!("no agent named '{name}'"))?;
            let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
            let (mut send, mut recv) = cp
                .open_stream_retry(
                    &daemon,
                    &suzerain_protocol::control::StreamHello::Attach { agent_id: agent.id },
                )
                .await?;
            use suzerain_protocol::control::AttachMessage;
            use suzerain_protocol::framing::{read_jsonl, write_jsonl};
            if !queued {
                write_jsonl(
                    &mut send,
                    &AttachMessage::Prompt {
                        message: message.into(),
                    },
                )
                .await?;
            }
            // Track the final assistant text from message_end events directly
            // on the stream (the central log lags by the ship interval).
            let mut last_text = String::new();
            tokio::time::timeout(std::time::Duration::from_secs(300), async {
                loop {
                    match read_jsonl::<_, AttachMessage>(&mut recv).await {
                        Ok(AttachMessage::Event { event }) => {
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
                        // Attach handshake ack: keep listening. Anything
                        // else is the daemon explaining a rejection.
                        Ok(AttachMessage::Notice { message }) if message == "attached" => {}
                        Ok(AttachMessage::Notice { message }) => bail!("{message}"),
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await
            .context("ask timed out")??;
            // Fast replies can stream before this attach subscribed; fall
            // back to the central log's last assistant message.
            if last_text.is_empty() {
                let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
                if let Ok(content) = tokio::fs::read_to_string(&log).await {
                    for line in content.lines().rev() {
                        let Ok(ev) = serde_json::from_str::<Value>(line) else {
                            continue;
                        };
                        if ev["kind"] == "message_end"
                            && ev["payload"]["message"]["role"] == "assistant"
                        {
                            if let Some(parts) = ev["payload"]["message"]["content"].as_array() {
                                let text: String = parts
                                    .iter()
                                    .filter(|p| p["type"] == "text")
                                    .filter_map(|p| p["text"].as_str())
                                    .collect();
                                if !text.is_empty() {
                                    last_text = text;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
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
