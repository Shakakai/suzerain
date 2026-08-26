//! Mock suzerain control plane for Suzy UI/integration tests.
//!
//! Serves the real `/api/v1` surface (REST + SSE + the shell WebSocket)
//! from canned state on an ephemeral localhost port. The shell endpoint
//! pipes frames to a real local `sh` process — so the app's full shell
//! path (WS client → base64 frames → process stdio → terminal widget) is
//! exercised without QEMU/Gondolin.

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Mock {
    state: Arc<Mutex<MockState>>,
    /// Fleet-event hints (drives the app's refetch loop).
    events: broadcast::Sender<Value>,
    /// Live session events (drives chat round trips).
    session: broadcast::Sender<Value>,
}

pub struct MockState {
    pub agents: Vec<Value>,
    pub prompts_received: Vec<(String, String)>,
    pub pending: Vec<Value>,
    pub audit: Vec<Value>,
    pub secrets: Vec<Value>,
    pub destroyed: Vec<String>,
}

impl Mock {
    pub fn new() -> Self {
        let agent = json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "name": "demo-1",
            "daemon_endpoint_id": "mock-daemon-eid-0001",
            "daemon_hostname": "mockbox",
            "manifest": {
                "name": "demo-1",
                "harness": {"type": "pi", "version": "0.84.1"},
                "model": {"provider": "kimi-coding", "id": "kimi-for-coding"},
            },
            "state": "active",
            "status": "idle",
            "busy": false,
            "idle_secs": 42,
            "needs_attention": false,
            "auto_suspend_override": null,
            "created_at": "2026-08-12T00:00:00Z",
            "session_file": "/agent/sessions/s1.jsonl",
        });
        Self {
            state: Arc::new(Mutex::new(MockState {
                agents: vec![agent],
                prompts_received: Vec::new(),
                pending: vec![json!({
                    "endpoint_id": "mock-pending-eid-0002",
                    "hostname": "pendingbox",
                    "os": "linux", "arch": "x86_64",
                    "capacity": {}, "first_seen": "", "last_seen": "",
                })],
                audit: vec![json!({
                    "at": "2026-08-12T00:00:00Z", "actor": "operator",
                    "action": "daemon_approve", "detail": {"endpoint_id": "mock-daemon-eid-0001"},
                })],
                secrets: vec![json!({"kind": "provider", "name": "kimi-coding", "used_by": 1})],
                destroyed: Vec::new(),
            })),
            events: broadcast::channel(64).0,
            session: broadcast::channel(64).0,
        }
    }

    pub fn state(&self) -> Arc<Mutex<MockState>> {
        self.state.clone()
    }

    fn hint(&self, kind: &str) {
        let _ = self
            .events
            .send(json!({"kind": kind, "at": "2026-08-12T00:00:00Z"}));
    }

    fn audit(&self, action: &str, detail: Value) {
        self.state.lock().unwrap().audit.push(json!({
            "at": "2026-08-12T00:00:01Z", "actor": "operator",
            "action": action, "detail": detail,
        }));
        self.hint("audit");
    }

    /// Bind an iroh endpoint (no discovery — tests dial the direct
    /// address) and serve the operator protocol forever. Returns
    /// (endpoint_id, addr).
    pub async fn start(self) -> (String, suzerain_client::iroh::EndpointAddr) {
        use suzerain_client::iroh::{endpoint::presets, Endpoint, SecretKey};
        // The test binary links multiple rustls providers; pick ring.
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::ring::default_provider(),
        );
        let router = self.clone().router();
        let mock = self;
        let endpoint = Endpoint::builder(presets::Empty)
            .secret_key(SecretKey::generate())
            .alpns(vec![suzerain_protocol::alpn::OPERATOR.to_vec()])
            .crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
            .bind()
            .await
            .unwrap();
        let id = endpoint.id().to_string();
        let addr = endpoint.addr();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                let Ok(connecting) = incoming.accept() else {
                    continue;
                };
                let Ok(conn) = connecting.await else { continue };
                let mock = mock.clone();
                let router = router.clone();
                tokio::spawn(async move {
                    while let Ok((send, recv)) = conn.accept_bi().await {
                        let mock = mock.clone();
                        let router = router.clone();
                        tokio::spawn(async move {
                            let _ = handle_op(mock, router, send, recv).await;
                        });
                    }
                });
            }
        });
        (id, addr)
    }

    fn router(self) -> Router {
        Router::new()
            .route("/api/v1/endpoint", get(endpoint))
            .route("/api/v1/overview", get(overview))
            .route("/api/v1/daemons", get(daemons))
            .route("/api/v1/daemons/pending", get(pending))
            .route(
                "/api/v1/daemons/pending/{id}/approve",
                post(pending_approve),
            )
            .route(
                "/api/v1/daemons/pending/{id}/dismiss",
                post(pending_dismiss),
            )
            .route("/api/v1/daemons/{id}/labels", post(labels))
            .route("/api/v1/agents", get(agents).post(agent_create))
            .route(
                "/api/v1/agents/{name}",
                get(agent_details).patch(agent_update),
            )
            .route("/api/v1/agents/{name}/destroy", post(agent_destroy))
            .route("/api/v1/agents/{name}/logs", get(agent_logs))
            .route("/api/v1/agents/{name}/session", get(session_sse))
            .route(
                "/api/v1/agents/{name}/session/history",
                get(session_history),
            )
            .route("/api/v1/agents/{name}/session_state", get(session_state))
            .route("/api/v1/agents/{name}/prompt", post(prompt))
            .route("/api/v1/events", get(events_sse))
            .route("/api/v1/audit", get(audit))
            .route("/api/v1/secrets", get(secrets))
            .route("/api/v1/secrets/reveal", post(secret_reveal))
            .route(
                "/api/v1/secrets/providers/{id}",
                axum::routing::put(secret_set_provider).delete(secret_delete_provider),
            )
            .route("/api/v1/providers", get(providers))
            .route("/api/v1/harnesses", get(harnesses))
            .with_state(self)
    }
}

// ── canned payloads ──────────────────────────────────────────────────────

fn daemon_json() -> Value {
    json!({
        "endpoint_id": "mock-daemon-eid-0001",
        "approved": true, "online": true,
        "hostname": "mockbox", "os": "linux", "arch": "x86_64",
        "labels": {"zone": "test"}, "reported_labels": {"zone": "test"},
        "label_overrides": {}, "max_agents": 4, "last_seen": "2026-08-12T00:00:00Z",
        "capacity": {"vcpu_total": 8, "memory_mib_total": 16384, "disk_mib_total": 102400, "gpus": []},
        "usage": {"memory_mib_free": 8192, "cpu_load1": 0.5, "disk_mib_free": 51200, "gpus": []},
    })
}

fn history_items() -> Vec<Value> {
    vec![
        json!({"role": "user", "parts": [{"type": "text", "text": "hello demo"}]}),
        json!({"role": "assistant", "parts": [{"type": "text", "text": "mock reply: hi there"}]}),
    ]
}

// ── handlers ─────────────────────────────────────────────────────────────

async fn endpoint() -> Json<Value> {
    Json(json!({"endpoint_id": "mock-suzerain-eid-0000", "version": "0.0.0-test"}))
}

async fn overview(State(m): State<Mock>) -> Json<Value> {
    let n = m.state.lock().unwrap().agents.len();
    Json(json!({
        "endpoint_id": "mock-suzerain-eid-0000",
        "daemons_total": 1, "daemons_online": 1,
        "agents_total": n, "agents_by_state": {"active": n},
    }))
}

async fn daemons() -> Json<Value> {
    Json(json!({"daemons": [daemon_json()]}))
}

async fn pending(State(m): State<Mock>) -> Json<Value> {
    let p = m.state.lock().unwrap().pending.clone();
    Json(json!({"pending": p}))
}

async fn pending_approve(State(m): State<Mock>, Path(id): Path<String>) -> Json<Value> {
    m.state
        .lock()
        .unwrap()
        .pending
        .retain(|p| p["endpoint_id"] != id);
    m.audit("daemon_approve", json!({"endpoint_id": id}));
    m.hint("daemon");
    Json(json!({"approved": id}))
}

async fn pending_dismiss(State(m): State<Mock>, Path(id): Path<String>) -> Json<Value> {
    m.state
        .lock()
        .unwrap()
        .pending
        .retain(|p| p["endpoint_id"] != id);
    m.hint("daemon");
    Json(json!({"ok": true}))
}

async fn labels(State(m): State<Mock>, Path(id): Path<String>) -> Json<Value> {
    m.audit("daemon_label", json!({"endpoint_id": id}));
    Json(json!({"effective_labels": {"zone": "test"}}))
}

async fn agents(State(m): State<Mock>) -> Json<Value> {
    let agents = m.state.lock().unwrap().agents.clone();
    Json(json!({"agents": agents}))
}

async fn agent_details(Path(name): Path<String>) -> Json<Value> {
    Json(json!({
        "name": name,
        "state": "active", "status": "idle",
        "created_at": "2026-08-12T00:00:00Z",
        "auto_suspend_override": null,
        "manifest_toml": "name = \"demo-1\"\nharness = { type = \"pi\", version = \"0.84.1\" }\nmodel = { provider = \"kimi-coding\", id = \"kimi-for-coding\" }\n",
        "sessions": [{"id": 1, "agent_id": "11111111-1111-1111-1111-111111111111",
                      "session_file": "/agent/sessions/s1.jsonl",
                      "started_at": "2026-08-12T00:00:00Z", "ended_at": null}],
        "last_event_at": null, "event_count": 0,
    }))
}

async fn agent_update(
    State(m): State<Mock>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    m.audit(
        "agent_config",
        json!({"name": name, "auto_suspend": body["auto_suspend"]}),
    );
    Json(json!({"ok": true}))
}

async fn agent_create(State(m): State<Mock>, Json(body): Json<Value>) -> Json<Value> {
    let toml_text = body["manifest_toml"].as_str().unwrap_or_default();
    // Parse for real (same type the real control plane validates against),
    // so a test can assert the created agent actually reflects the form's
    // provider/model choice instead of a hardcoded stand-in.
    let manifest: suzerain_protocol::AgentManifest =
        toml::from_str(toml_text).expect("test-submitted manifest TOML parses");
    let name = manifest.name.clone();
    let agent = json!({
        "id": "22222222-2222-2222-2222-222222222222",
        "name": name, "daemon_endpoint_id": "mock-daemon-eid-0001",
        "daemon_hostname": "mockbox",
        "manifest": manifest,
        "state": "active", "status": "idle", "busy": false, "idle_secs": 0,
        "needs_attention": false, "auto_suspend_override": null,
        "created_at": "2026-08-12T00:00:01Z", "session_file": null,
    });
    m.state.lock().unwrap().agents.push(agent.clone());
    m.audit("agent_create", json!({"name": name}));
    m.hint("agent");
    Json(agent)
}

async fn agent_destroy(State(m): State<Mock>, Path(name): Path<String>) -> Json<Value> {
    m.state.lock().unwrap().agents.retain(|a| a["name"] != name);
    m.state.lock().unwrap().destroyed.push(name.clone());
    m.audit("agent_destroy", json!({"name": name}));
    m.hint("agent");
    Json(json!({"ok": true}))
}

async fn agent_logs() -> Json<Value> {
    Json(json!({
        "events": [
            {"kind": "spawned", "at": "2026-08-12T00:00:00Z", "payload": {"session_file": "s1"}},
            {"kind": "message_end", "at": "2026-08-12T00:00:01Z",
             "payload": {"message": {"role": "user", "content": "hello demo"}}},
        ],
        "total_matching": 2,
    }))
}

async fn session_history() -> Json<Value> {
    Json(
        json!({"items": history_items(), "total_matching": 2, "streaming": false, "state": "active"}),
    )
}

async fn session_state() -> Json<Value> {
    Json(json!({"state": "active", "status": "idle", "busy": false,
                "needs_attention": false, "streaming": false,
                "model": {"provider": "kimi-coding", "id": "kimi-for-coding"}}))
}

async fn prompt(
    State(m): State<Mock>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let message = body["message"].as_str().unwrap_or("").to_string();
    let mode = body["mode"].as_str().unwrap_or("prompt");
    m.state
        .lock()
        .unwrap()
        .prompts_received
        .push((name.clone(), format!("{mode}:{message}")));
    if mode == "prompt" {
        // The "agent" replies: a live message_end on the session stream.
        let _ = m.session.send(json!({
            "type": "message_end",
            "message": {"role": "assistant",
                        "content": [{"type": "text", "text": format!("mock reply to: {message}")}]},
        }));
        let _ = m.session.send(json!({"type": "turn_end"}));
    }
    Json(json!({"ok": true}))
}

async fn session_sse(
    State(m): State<Mock>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::StreamExt;
    let history: Vec<Result<Event, std::convert::Infallible>> = history_items()
        .into_iter()
        .map(|item| Ok(Event::default().event("history").data(item.to_string())))
        .chain(std::iter::once(Ok(Event::default()
            .event("history_end")
            .data("{}"))))
        .collect();
    let live = tokio_stream::wrappers::BroadcastStream::new(m.session.subscribe()).filter_map(
        |r| async move {
            r.ok()
                .map(|ev| Ok(Event::default().event("event").data(ev.to_string())))
        },
    );
    Sse::new(futures_util::stream::iter(history).chain(live))
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

async fn events_sse(
    State(m): State<Mock>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use futures_util::StreamExt;
    let stream = tokio_stream::wrappers::BroadcastStream::new(m.events.subscribe()).filter_map(
        |r| async move {
            r.ok().map(|v| {
                Ok(Event::default()
                    .event(v["kind"].as_str().unwrap_or("event"))
                    .data(v.to_string()))
            })
        },
    );
    Sse::new(stream).keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
}

async fn audit(State(m): State<Mock>) -> Json<Value> {
    let entries = m.state.lock().unwrap().audit.clone();
    Json(json!({"entries": entries}))
}

async fn secrets(State(m): State<Mock>) -> Json<Value> {
    let entries = m.state.lock().unwrap().secrets.clone();
    Json(json!({"entries": entries, "store_present": true}))
}

async fn secret_reveal() -> Json<Value> {
    Json(json!({"value": "sk-mock-revealed-once"}))
}

async fn secret_set_provider(State(m): State<Mock>, Path(id): Path<String>) -> Json<Value> {
    let mut st = m.state.lock().unwrap();
    if !st.secrets.iter().any(|e| e["name"] == id) {
        st.secrets
            .push(json!({"kind": "provider", "name": id, "used_by": 0}));
    }
    drop(st);
    m.audit("secret_set", json!({"kind": "provider", "name": id}));
    Json(json!({"ok": true}))
}

async fn secret_delete_provider(State(m): State<Mock>, Path(id): Path<String>) -> Json<Value> {
    m.state.lock().unwrap().secrets.retain(|e| e["name"] != id);
    m.audit("secret_delete", json!({"kind": "provider", "name": id}));
    Json(json!({"ok": true}))
}

async fn providers() -> Json<Value> {
    Json(json!({"providers": {
        "kimi-coding": {
            "models": [{"id": "kimi-for-coding", "name": "Kimi for Coding"}],
            "key_injectable": true, "key_configured": true,
        },
        "openrouter": {
            "models": [{"id": "stealth/ox-alpha", "name": "Stealth: Ox Alpha"}],
            "key_injectable": true, "key_configured": true,
        },
        // Configured but not injectable (OAuth-only) — must NOT appear in
        // the create-agent form's provider list even though it has a key.
        "github-copilot": {
            "models": [{"id": "gpt-5", "name": "GPT-5"}],
            "key_injectable": false, "key_configured": true,
        },
        // Injectable but no key configured — must NOT appear either.
        "anthropic": {
            "models": [{"id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5"}],
            "key_injectable": true, "key_configured": false,
        },
    }}))
}

async fn harnesses() -> Json<Value> {
    Json(json!({"harnesses": {"pi": {"label": "pi", "versions": ["0.84.1"]}}}))
}

// ── operator protocol dispatch (mirrors crates/suzerain/src/operator.rs) ──

async fn handle_op(
    mock: Mock,
    router: Router,
    mut send: suzerain_client::iroh::endpoint::SendStream,
    recv: suzerain_client::iroh::endpoint::RecvStream,
) -> anyhow::Result<()> {
    use suzerain_protocol::control::{OperatorFrame, OperatorHello};
    use suzerain_protocol::framing::{read_jsonl, write_jsonl};

    let mut recv = tokio::io::BufReader::new(recv);
    let hello: OperatorHello = read_jsonl(&mut recv).await?;
    match hello {
        OperatorHello::Rest { method, path, body } => {
            use tower::ServiceExt;
            let req = axum::http::Request::builder()
                .method(method.as_str())
                .uri(&path)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    body.map(|v| serde_json::to_vec(&v).unwrap())
                        .unwrap_or_default(),
                ))?;
            let resp = router.oneshot(req).await.unwrap();
            let status = resp.status().as_u16();
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
                .await
                .unwrap_or_default();
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            write_jsonl(&mut send, &OperatorFrame::Reply { status, body }).await?;
        }
        OperatorHello::Stream { path } => {
            use futures_util::StreamExt;
            use tower::ServiceExt;
            let req = axum::http::Request::builder()
                .method("GET")
                .uri(&path)
                .body(axum::body::Body::empty())?;
            let resp = router.oneshot(req).await.unwrap();
            let mut body = http_body_util::BodyStream::new(resp.into_body());
            while let Some(frame) = body.next().await {
                match frame {
                    Ok(frame) => {
                        if let Ok(data) = frame.into_data() {
                            let chunk = OperatorFrame::Chunk {
                                data: suzerain_client::b64_encode(&data),
                            };
                            if write_jsonl(&mut send, &chunk).await.is_err() {
                                return Ok(());
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = write_jsonl(&mut send, &OperatorFrame::End).await;
        }
        OperatorHello::Shell { name: _ } => {
            shell_op(send, recv).await?;
        }
    }
    let _ = mock;
    Ok(())
}

/// Shell op: ShellMessage frames piped to a real local `sh` — the client
/// exercises its full shell path (iroh frames → process stdio → terminal
/// widget) without QEMU/Gondolin.
async fn shell_op(
    mut send: suzerain_client::iroh::endpoint::SendStream,
    mut recv: tokio::io::BufReader<suzerain_client::iroh::endpoint::RecvStream>,
) -> anyhow::Result<()> {
    use suzerain_protocol::control::ShellMessage;
    use suzerain_protocol::framing::{read_jsonl, write_jsonl};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    write_jsonl(
        &mut send,
        &ShellMessage::Notice {
            message: "shell".to_string(),
        },
    )
    .await?;

    let mut child = tokio::process::Command::new("sh")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    let tx2 = out_tx.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if tx2.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    loop {
        tokio::select! {
            msg = read_jsonl::<_, ShellMessage>(&mut recv) => {
                match msg {
                    Ok(ShellMessage::Data { data }) => {
                        let bytes = suzerain_client::b64_decode(&data)?;
                        if stdin.write_all(&bytes).await.is_err() { break; }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            chunk = out_rx.recv() => {
                match chunk {
                    Some(bytes) => {
                        let msg = ShellMessage::Data {
                            data: suzerain_client::b64_encode(&bytes),
                        };
                        if write_jsonl(&mut send, &msg).await.is_err() { break; }
                    }
                    None => {
                        let _ = write_jsonl(&mut send, &ShellMessage::Exit { code: 0 }).await;
                        break;
                    }
                }
            }
        }
    }
    let _ = child.kill().await;
    Ok(())
}
