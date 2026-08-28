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
use tokio::sync::broadcast;
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
    store: Store,
    cp: Arc<ControlPlane>,
    agent: AgentRow,
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) -> Result<()> {
    let name = agent.name.clone();
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

    // 2. Live relay. The agent may be sleeping (auto-suspended) or suspend
    // mid-session: emit synthetic status lines and wait for a wake rather
    // than dying. Reconnects until the client goes away.
    let mut wake_rx = cp.wake().subscribe();
    loop {
        let agent = lookup(&store, &name).await?;
        if !crate::wake::is_awake(&cp, &agent).await {
            send(
                "event",
                json!({"type": "status", "status": suzerain_protocol::state::public_status(agent.state, agent.busy == Some(true)), "message": "agent is sleeping — send a message to wake it"}),
            )
            .await
            .map_err(|_| anyhow::anyhow!("client gone"))?;
            // Wait for the agent to become awake: narrate wake progress.
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    ev = wake_rx.recv() => {
                        match ev {
                            Ok(ev) if ev.agent_id == agent.id => {
                                let message = ev.detail.clone().unwrap_or_else(|| ev.status.clone());
                                send("event", json!({"type": "status", "status": ev.status, "message": message}))
                                    .await
                                    .map_err(|_| anyhow::anyhow!("client gone"))?;
                            }
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(_)) => {}
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
                let cur = lookup(&store, &name).await?;
                if crate::wake::is_awake(&cp, &cur).await {
                    send(
                        "event",
                        json!({"type": "status", "status": "ready", "message": "agent is awake"}),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("client gone"))?;
                    break;
                }
            }
        }

        // Live relay via the attach stream. The send half must stay alive —
        // dropping it signals EOF and the daemon tears the relay down.
        let agent = lookup(&store, &name).await?;
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
                // Stream died (agent suspended mid-session, daemon flap):
                // loop back — re-check state, wait for wake if needed.
                Err(_) => break,
            }
        }
    }
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

    // Transparent wake: if the agent is sleeping, only prompts are
    // meaningful — queue durably and kick the wake off; the SSE session
    // narrates progress and the wake task delivers the message.
    if !crate::wake::is_awake(&cp, &agent).await {
        return match msg {
            AttachMessage::Prompt { message } => {
                store
                    .enqueue_message(&agent.id, &message)
                    .await
                    .map_err(fail)?;
                // Race guard: if the agent woke between our check and the
                // enqueue, deliver directly instead of waiting on a wake.
                if crate::wake::is_awake(&cp, &agent).await {
                    crate::wake::flush_pending(&cp, &agent)
                        .await
                        .map_err(fail)?;
                } else {
                    cp.wake().ensure(&cp, &agent).await;
                }
                Ok(axum::Json(
                    json!({"ok": true, "queued": true, "status": "waking"}),
                ))
            }
            _ => Err((
                axum::http::StatusCode::CONFLICT,
                axum::Json(json!({"error": "agent is sleeping — send a prompt to wake it"})),
            )),
        };
    }
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
        "status": suzerain_protocol::state::public_status(agent.state, agent.busy == Some(true)),
        "busy": agent.busy,
        "needs_attention": agent.needs_attention,
        "streaming": streaming,
        "model": agent.manifest.model,
    })))
}

// ── transcript reconstruction (decision #6: full reconstruction) ──────────

/// Reconstructed transcript items from the agent's central event log: one
/// item per `message_end`, session boundaries (`session_started` /
/// `session_rotated`) as typed markers, plus crash-level events as
/// `system` items. Shared by the SSE replay and the JSON session-history
/// endpoint (MCP).
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
        // Session boundaries: sessions rotate on every suspend; these
        // markers segment the conversation log into eras.
        if kind == "session_started" {
            let file = ev["payload"]["session_file"].as_str().unwrap_or("");
            let short = file.rsplit('/').next().unwrap_or(file);
            items.push(json!({"role": "system", "parts": [{
                "type": "session_boundary",
                "text": format!("── session started ({short}) ──"),
                "session_file": file,
                "at": ev["at"],
            }]}));
            continue;
        }
        if kind == "session_rotated" {
            items.push(json!({"role": "system", "parts": [{
                "type": "session_boundary",
                "text": "── session ended (suspended; history archived) ──",
                "at": ev["at"],
            }]}));
            continue;
        }
        let sys_text = match kind {
            "pi_stderr" => Some(format!(
                "pi: {}",
                ev["payload"]["line"].as_str().unwrap_or("")
            )),
            // code -1 = intentional kill (routine suspend/stop shutdown),
            // not a crash — don't render it as an error in the transcript.
            "pi_exit" if ev["payload"]["code"].as_i64() == Some(-1) => None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::test_support::lock_env_home;
    use crate::store::Store;
    use axum::body::Body;
    use axum::extract::{Path as ExtractPath, State};
    use axum::http::Request;
    use suzerain_protocol::manifest::AgentManifest;
    use tower::ServiceExt;

    // ── transcript_item: pure reconstruction from a raw pi message ─────────

    #[test]
    fn transcript_item_assistant_text_and_tool_call() {
        let message = json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "hi there"},
                {"type": "thinking", "thinking": "pondering"},
                {"type": "toolCall", "id": "t1", "name": "shell", "arguments": {"cmd": "ls"}},
            ],
        });
        let item = transcript_item(&message).expect("should produce an item");
        assert_eq!(item["role"], "assistant");
        let parts = item["parts"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "hi there");
        assert_eq!(parts[1]["type"], "thinking");
        assert_eq!(parts[2]["type"], "tool_call");
        assert_eq!(parts[2]["name"], "shell");
    }

    /// An aborted/errored turn usually has empty `content` — without the
    /// synthesized error part, the transcript would show a blank assistant
    /// bubble with no explanation of what happened.
    #[test]
    fn transcript_item_surfaces_errored_turns_even_with_empty_content() {
        let message = json!({
            "role": "assistant",
            "content": [],
            "stopReason": "error",
            "errorMessage": "upstream 500",
        });
        let item = transcript_item(&message).expect("errored turn must still surface");
        let parts = item["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "error");
        assert!(parts[0]["text"].as_str().unwrap().contains("upstream 500"));
    }

    #[test]
    fn transcript_item_tool_result_and_user_text() {
        let tool_result = json!({
            "role": "toolResult",
            "toolCallId": "t1",
            "toolName": "shell",
            "isError": true,
            "content": [{"type": "text", "text": "boom"}],
        });
        let item = transcript_item(&tool_result).unwrap();
        assert_eq!(item["parts"][0]["type"], "tool_result");
        assert_eq!(item["parts"][0]["is_error"], true);
        assert_eq!(item["parts"][0]["text"], "boom");

        let user = json!({"role": "user", "content": "hello"});
        let item = transcript_item(&user).unwrap();
        assert_eq!(item["parts"][0]["text"], "hello");
    }

    /// A message with no renderable content (e.g. blank user text) must not
    /// produce a phantom transcript entry.
    #[test]
    fn transcript_item_returns_none_for_empty_content() {
        let user = json!({"role": "user", "content": "   "});
        assert!(transcript_item(&user).is_none());
    }

    // ── history reconstruction + the JSON history endpoint ─────────────────

    async fn memory_store() -> Store {
        let name = format!("web-session-test-{}", uuid::Uuid::new_v4().simple());
        let url = format!("sqlite://file:{name}?mode=memory&cache=shared");
        Store::open_with_url(&url).await.expect("open store")
    }

    fn agent_row(name: &str) -> AgentRow {
        let toml = format!(
            "name = \"{name}\"\nharness = {{ type = \"pi\", version = \"1\" }}\nmodel = {{ provider = \"p\", id = \"m\" }}\n"
        );
        let manifest: AgentManifest = toml::from_str(&toml).unwrap();
        AgentRow {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            daemon_endpoint_id: "d".into(),
            manifest,
            state: suzerain_protocol::AgentState::Active,
            created_at: crate::store::castellan_time_now(),
            session_file: None,
            idle_secs: None,
            busy: None,
            activity_reported_at: None,
            needs_attention: false,
            auto_suspend_override: None,
            woke_at: None,
        }
    }

    /// Write a synthetic central event-log JSONL for `agent`, as castellan's
    /// log shipping would have produced it.
    async fn write_log(agent: &AgentRow, lines: &[Value]) {
        let dir = data_dir().join("logs");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join(format!("{}.jsonl", agent.id));
        let mut content = String::new();
        for line in lines {
            content.push_str(&line.to_string());
            content.push('\n');
        }
        tokio::fs::write(&path, content).await.unwrap();
    }

    #[tokio::test]
    async fn history_items_reconstructs_messages_and_session_boundaries() {
        let (_guard, dir) = lock_env_home().await;
        let agent = agent_row("hist-agent");

        write_log(
            &agent,
            &[
                json!({"kind": "session_started", "at": "t0", "payload": {"session_file": "/x/session-1.jsonl"}}),
                json!({"kind": "message_end", "at": "t1", "payload": {"message": {"role": "user", "content": "hi"}}}),
                json!({"kind": "message_end", "at": "t2", "payload": {"message": {"role": "assistant", "content": [{"type": "text", "text": "hello"}]}}}),
                json!({"kind": "session_rotated", "at": "t3"}),
                json!({"kind": "pi_exit", "at": "t4", "payload": {"code": 1}}),
            ],
        )
        .await;

        let items = history_items(&agent).await;
        assert_eq!(items.len(), 5, "{items:?}");
        assert_eq!(items[0]["parts"][0]["type"], "session_boundary");
        assert_eq!(items[1]["role"], "user");
        assert_eq!(items[2]["role"], "assistant");
        assert_eq!(items[3]["parts"][0]["type"], "session_boundary");
        assert!(items[3]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("suspended"));
        // A real (non -1) pi_exit must surface as a system line — silently
        // dropping it would make a crashed agent look merely quiet.
        assert_eq!(items[4]["role"], "system");
        assert!(items[4]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pi exited (code 1)"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `pi_exit` with code -1 is a routine, intentional shutdown (suspend),
    /// not a crash: it must not be rendered as an error line.
    #[tokio::test]
    async fn history_items_suppresses_intentional_kill_exit() {
        let (_guard, dir) = lock_env_home().await;
        let agent = agent_row("kill-agent");
        write_log(
            &agent,
            &[json!({"kind": "pi_exit", "at": "t1", "payload": {"code": -1}})],
        )
        .await;

        let items = history_items(&agent).await;
        assert!(items.is_empty(), "{items:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn session_history_endpoint_filters_by_role_and_tail() {
        let (_guard, dir) = lock_env_home().await;
        let store = memory_store().await;
        let agent = agent_row("history-endpoint-agent");
        store.create_agent(&agent).await.unwrap();
        write_log(
            &agent,
            &[
                json!({"kind": "message_end", "payload": {"message": {"role": "user", "content": "one"}}}),
                json!({"kind": "message_end", "payload": {"message": {"role": "assistant", "content": [{"type": "text", "text": "two"}]}}}),
                json!({"kind": "message_end", "payload": {"message": {"role": "user", "content": "three"}}}),
            ],
        )
        .await;

        let resp = session_history(
            State(WebState {
                store: store.clone(),
                cp: test_cp(store.clone()).await,
            }),
            ExtractPath("history-endpoint-agent".to_string()),
            axum::extract::Query(SessionHistoryQuery {
                tail: None,
                roles: None,
            }),
        )
        .await
        .expect("should succeed");
        let items = resp.0["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(resp.0["total_matching"], 3);

        let resp = session_history(
            State(WebState {
                store: store.clone(),
                cp: test_cp(store.clone()).await,
            }),
            ExtractPath("history-endpoint-agent".to_string()),
            axum::extract::Query(SessionHistoryQuery {
                tail: Some(1),
                roles: Some("user".to_string()),
            }),
        )
        .await
        .expect("should succeed");
        let items = resp.0["items"].as_array().unwrap();
        // Two "user" messages match the role filter; tail=1 keeps the last.
        assert_eq!(resp.0["total_matching"], 2);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["parts"][0]["text"], "three");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn session_history_for_unknown_agent_is_404() {
        let (_guard, dir) = lock_env_home().await;
        let store = memory_store().await;
        let result = session_history(
            State(WebState {
                store: store.clone(),
                cp: test_cp(store).await,
            }),
            ExtractPath("ghost".to_string()),
            axum::extract::Query(SessionHistoryQuery {
                tail: None,
                roles: None,
            }),
        )
        .await;
        assert!(result.is_err());
        let (status, _) = result.unwrap_err();
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── session_prompt: validation that happens before any live daemon I/O ──

    async fn test_cp(store: Store) -> Arc<ControlPlane> {
        Arc::new(
            crate::control::start(store, vec![])
                .await
                .expect("control plane"),
        )
    }

    async fn call_prompt(
        store: &Store,
        cp: &Arc<ControlPlane>,
        name: &str,
        body: Value,
    ) -> (axum::http::StatusCode, Value) {
        let router = crate::web::build_router(WebState {
            store: store.clone(),
            cp: cp.clone(),
        });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/agents/{name}/prompt"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn session_prompt_rejects_empty_message_before_touching_the_daemon() {
        let (_guard, dir) = lock_env_home().await;
        let store = memory_store().await;
        let cp = test_cp(store.clone()).await;
        store
            .create_agent(&agent_row("prompt-agent"))
            .await
            .unwrap();

        let (status, body) = call_prompt(
            &store,
            &cp,
            "prompt-agent",
            json!({"message": "", "mode": "prompt"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("message required"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn session_prompt_rejects_unknown_mode() {
        let (_guard, dir) = lock_env_home().await;
        let store = memory_store().await;
        let cp = test_cp(store.clone()).await;
        store
            .create_agent(&agent_row("prompt-agent2"))
            .await
            .unwrap();

        let (status, body) = call_prompt(
            &store,
            &cp,
            "prompt-agent2",
            json!({"message": "hi", "mode": "teleport"}),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown mode"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn session_prompt_for_unknown_agent_is_rejected() {
        let (_guard, dir) = lock_env_home().await;
        let store = memory_store().await;
        let cp = test_cp(store.clone()).await;

        let (status, _) = call_prompt(&store, &cp, "no-such-agent", json!({"message": "hi"})).await;
        assert_eq!(status, axum::http::StatusCode::CONFLICT);

        std::fs::remove_dir_all(&dir).ok();
    }
}
