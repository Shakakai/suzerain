//! Agent session over SSE (M3): full transcript reconstruction from the
//! central log, then a live attach-stream relay. Prompts/steer/follow-ups/
//! aborts go over short-lived separate attach streams (the daemon accepts
//! concurrent attach streams), so they work with or without an open SSE
//! connection.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use serde_json::{json, Value};
use suzerain_protocol::control::{AttachMessage, StreamHello};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use tokio_stream::wrappers::ReceiverStream;

use crate::control::ControlPlane;
use crate::identity::data_dir;
use crate::store::{AgentRow, Store};
use crate::web::WebState;

/// `GET …/session` — SSE: `history` items, `history_end`, then live `event`s.
pub async fn session_sse(
    State(s): State<WebState>,
    Path(name): Path<String>,
) -> Result<impl axum::response::IntoResponse, (axum::http::StatusCode, axum::Json<Value>)> {
    let agent = lookup(&s.store, &name).await.map_err(|e| {
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("{e:#}")})),
        )
    })?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(512);
    let cp = s.cp.clone();
    let store = s.store.clone();
    tokio::spawn(async move {
        if let Err(err) = run_session(store, cp, agent, tx.clone()).await {
            let _ = tx
                .send(Ok(Event::default().event("error").data(err.to_string())))
                .await;
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn lookup(store: &Store, name: &str) -> Result<AgentRow> {
    store
        .get_agent_by_name(name)
        .await?
        .with_context(|| format!("no agent named '{name}'"))
}

async fn run_session(
    _store: Store,
    cp: Arc<ControlPlane>,
    agent: AgentRow,
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) -> Result<()> {
    let send = |event: &str, data: Value| {
        let tx = tx.clone();
        let data = data.to_string();
        let event = event.to_string();
        async move {
            tx.send(Ok(Event::default().event(event).data(data)))
                .await
                .map_err(|_| ())
        }
    };

    // 1. History: reconstructed transcript items from the central log.
    // Crash-level events (pi_exit/pi_stderr/driver_died) are replayed too,
    // as system lines — otherwise a dead agent looks like a silent one.
    for item in history_items(&agent).await {
        send("history", item)
            .await
            .map_err(|_| anyhow::anyhow!("client gone"))?;
    }
    send("history_end", json!({}))
        .await
        .map_err(|_| anyhow::anyhow!("client gone"))?;

    // 2. Live relay via the attach stream. The send half must stay alive —
    // dropping it signals EOF and the daemon tears the relay down.
    // Retry through transient daemon-offline windows (reconnect backoff).
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse()?;
    let (_send_stream, mut recv) = cp
        .open_stream_retry(&daemon, &StreamHello::Attach { agent_id: agent.id })
        .await?;
    loop {
        match read_jsonl::<_, AttachMessage>(&mut recv).await {
            Ok(AttachMessage::Event { event }) => {
                send("event", event)
                    .await
                    .map_err(|_| anyhow::anyhow!("client gone"))?;
            }
            // Attach-level notices (prompt rejections, daemon-side errors)
            // surface as system lines in the chat.
            Ok(AttachMessage::Notice { message }) => {
                send("event", json!({"type": "notice", "message": message}))
                    .await
                    .map_err(|_| anyhow::anyhow!("client gone"))?;
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// `POST …/prompt` — short-lived attach stream carrying one message.
pub async fn session_prompt(
    State(s): State<WebState>,
    Path(name): Path<String>,
    axum::Json(body): axum::Json<Value>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, axum::Json<Value>)> {
    let (store, cp) = (s.store.clone(), s.cp.clone());
    let fail = |e: anyhow::Error| {
        (
            axum::http::StatusCode::CONFLICT,
            axum::Json(json!({"error": format!("{e:#}")})),
        )
    };
    let agent = lookup(&store, &name).await.map_err(fail)?;
    let message = body["message"].as_str().unwrap_or("").to_string();
    if message.is_empty() {
        return Err((
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({"error": "message required"})),
        ));
    }
    let msg = match body["mode"].as_str().unwrap_or("prompt") {
        "prompt" => AttachMessage::Prompt { message },
        "steer" => AttachMessage::Steer { message },
        "follow_up" => AttachMessage::FollowUp { message },
        "abort" => AttachMessage::Abort,
        other => {
            return Err((
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(json!({"error": format!("unknown mode '{other}'")})),
            ));
        }
    };
    let daemon: iroh::EndpointId = agent
        .daemon_endpoint_id
        .parse()
        .map_err(anyhow::Error::from)
        .map_err(fail)?;
    let (mut send, mut recv) = cp
        .open_stream_retry(&daemon, &StreamHello::Attach { agent_id: agent.id })
        .await
        .map_err(fail)?;
    write_jsonl(&mut send, &msg)
        .await
        .map_err(anyhow::Error::from)
        .map_err(fail)?;
    let _ = send.finish();

    // Attach handshake: the daemon acknowledges immediately ("attached")
    // or explains the rejection (e.g. "agent 'x' is not running"). Wait for
    // that first frame so a send to a wedged agent fails loudly instead of
    // returning {"ok": true} into the void.
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        read_jsonl::<_, AttachMessage>(&mut recv),
    )
    .await;
    match first {
        Ok(Ok(AttachMessage::Notice { message })) if message == "attached" => {}
        Ok(Ok(AttachMessage::Notice { message })) => {
            return Err((
                axum::http::StatusCode::CONFLICT,
                axum::Json(json!({"error": format!("agent cannot accept messages: {message}")})),
            ));
        }
        Ok(Ok(AttachMessage::Event { .. })) => {} // live events = agent alive
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            return Err((
                axum::http::StatusCode::CONFLICT,
                axum::Json(
                    json!({"error": format!("attach stream closed before the agent acknowledged (agent not running?): {e}")}),
                ),
            ));
        }
        Err(_) => {
            return Err((
                axum::http::StatusCode::CONFLICT,
                axum::Json(
                    json!({"error": "agent did not acknowledge the attach within 10s (wedged or not running — try agent_start with force)"}),
                ),
            ));
        }
    }
    Ok(axum::Json(json!({"ok": true})))
}

/// `GET …/session_state` — streaming if the last turn hasn't settled.
pub async fn session_state(
    State(s): State<WebState>,
    Path(name): Path<String>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, axum::Json<Value>)> {
    let store = s.store.clone();
    let fail = |e: anyhow::Error| {
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("{e:#}")})),
        )
    };
    let agent = lookup(&store, &name).await.map_err(fail)?;
    let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
    let content = tokio::fs::read_to_string(&log).await.unwrap_or_default();
    let mut streaming = false;
    for line in content.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev["kind"].as_str() {
            Some("turn_start") => streaming = true,
            Some("turn_end") | Some("agent_settled") => streaming = false,
            _ => {}
        }
    }
    Ok(axum::Json(json!({
        "state": crate::store::state_str(agent.state),
        "streaming": streaming,
        "model": agent.manifest.model,
    })))
}

// ── transcript reconstruction (decision #6: full reconstruction) ──────────

/// Reconstructed transcript items from the agent's central event log: one
/// item per `message_end`, plus crash-level events as `system` items.
/// Shared by the SSE replay and the JSON session-history endpoint (MCP).
async fn history_items(agent: &AgentRow) -> Vec<Value> {
    let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
    let content = tokio::fs::read_to_string(&log).await.unwrap_or_default();
    let mut items = Vec::new();
    for line in content.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = ev["kind"].as_str().unwrap_or("");
        if kind == "message_end" {
            if let Some(item) = transcript_item(&ev["payload"]["message"]) {
                items.push(item);
            }
            continue;
        }
        let sys_text = match kind {
            "pi_stderr" => Some(format!(
                "pi: {}",
                ev["payload"]["line"].as_str().unwrap_or("")
            )),
            "pi_exit" => Some(format!(
                "pi exited (code {}) — the agent cannot process messages; check its provider/model config or restart it",
                ev["payload"]["code"].as_i64().map(|c| c.to_string()).unwrap_or_else(|| "?".into())
            )),
            "driver_died" => Some("agent VM driver died — the agent is unavailable".to_string()),
            _ => None,
        };
        if let Some(text) = sys_text {
            items.push(json!({"role": "system", "parts": [{"type": "text", "text": text}]}));
        }
    }
    items
}

/// Whether the agent's last turn hasn't settled (from the event log).
async fn is_streaming(agent: &AgentRow) -> bool {
    let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
    let content = tokio::fs::read_to_string(&log).await.unwrap_or_default();
    let mut streaming = false;
    for line in content.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev["kind"].as_str() {
            Some("turn_start") => streaming = true,
            Some("turn_end") | Some("agent_settled") => streaming = false,
            _ => {}
        }
    }
    streaming
}

#[derive(serde::Deserialize)]
pub struct SessionHistoryQuery {
    /// Keep only the last N items (after role filtering).
    tail: Option<usize>,
    /// Comma-separated roles to include (user, assistant, toolResult,
    /// system). Default: all.
    roles: Option<String>,
}

/// `GET …/session/history` — the session transcript as a JSON snapshot
/// (request/response-friendly sibling of the SSE replay, for MCP clients).
pub async fn session_history(
    State(s): State<WebState>,
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<SessionHistoryQuery>,
) -> Result<axum::Json<Value>, (axum::http::StatusCode, axum::Json<Value>)> {
    let agent = lookup(&s.store, &name).await.map_err(|e| {
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(json!({"error": format!("{e:#}")})),
        )
    })?;
    let roles: Option<std::collections::BTreeSet<String>> = q.roles.map(|r| {
        r.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let mut items: Vec<Value> = history_items(&agent)
        .await
        .into_iter()
        .filter(|item| {
            roles
                .as_ref()
                .is_none_or(|rs| rs.contains(item["role"].as_str().unwrap_or_default()))
        })
        .collect();
    let total = items.len();
    if let Some(tail) = q.tail {
        items = items.split_off(items.len().saturating_sub(tail));
    }
    Ok(axum::Json(json!({
        "items": items,
        "total_matching": total,
        "streaming": is_streaming(&agent).await,
        "state": crate::store::state_str(agent.state),
    })))
}

/// One transcript item per `message_end`: role + parts (text/thinking/tool).
fn transcript_item(message: &Value) -> Option<Value> {
    let role = message["role"].as_str()?;
    let mut parts: Vec<Value> = Vec::new();

    match role {
        "assistant" => {
            for c in message["content"].as_array()?.iter() {
                match c["type"].as_str() {
                    Some("text") => {
                        parts.push(json!({"type": "text", "text": c["text"]}));
                    }
                    Some("thinking") => {
                        parts.push(json!({"type": "thinking", "text": c["thinking"]}));
                    }
                    Some("toolCall") => {
                        parts.push(json!({
                            "type": "tool_call",
                            "id": c["id"],
                            "name": c["name"],
                            "arguments": c["arguments"],
                        }));
                    }
                    _ => {}
                }
            }
            // Errored/aborted turns (e.g. upstream LLM request failed):
            // content is usually empty, so without this the chat shows an
            // empty assistant bubble with no explanation.
            if matches!(
                message["stopReason"].as_str(),
                Some("error") | Some("aborted")
            ) {
                let detail = message["errorMessage"].as_str().unwrap_or("");
                parts.push(json!({
                    "type": "error",
                    "text": if detail.is_empty() {
                        format!("turn ended: {}", message["stopReason"].as_str().unwrap_or("error"))
                    } else {
                        format!("LLM request failed: {detail}")
                    },
                }));
            }
        }
        "toolResult" => {
            let text: String = message["content"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter(|c| c["type"] == "text")
                        .filter_map(|c| c["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            parts.push(json!({
                "type": "tool_result",
                "tool_call_id": message["toolCallId"],
                "name": message["toolName"],
                "text": text,
                "is_error": message["isError"].as_bool().unwrap_or(false),
            }));
        }
        _ => {
            // user (and others): plain text content
            let text = match &message["content"] {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            if !text.trim().is_empty() {
                parts.push(json!({"type": "text", "text": text}));
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(json!({"role": role, "parts": parts}))
    }
}
