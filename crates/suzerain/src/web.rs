//! Embedded web UI (local-only, docs/WEB-UI.md): axum server on
//! 127.0.0.1 serving a vanilla-JS SPA plus a REST/JSON API backed directly
//! by the Store and ControlPlane.

use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::audit;
use crate::control::ControlPlane;
use crate::identity::data_dir;
use crate::store::{AgentRow, DaemonRow, Store};

#[derive(Clone)]
pub struct WebState {
    pub store: Store,
    pub cp: Arc<ControlPlane>,
}

/// Start the web server (blocks forever). Binds localhost only.
pub async fn serve(store: Store, cp: Arc<ControlPlane>, port: u16) -> Result<()> {
    let state = WebState { store, cp };
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    info!(port, "web ui listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// The operator API router, without a listener. Shared by the HTTP server
/// and the iroh operator channel (operator.rs executes requests against
/// this router in-process — one source of truth for the API).
pub fn build_router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/providers.json", get(providers_json))
        .route("/harnesses.json", get(harnesses_json))
        .route("/vendor/marked.js", get(vendor_marked))
        .route("/vendor/dompurify.js", get(vendor_dompurify))
        .route("/api/v1/endpoint", get(endpoint))
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/daemons", get(daemons))
        .route(
            "/api/v1/daemons/{id}",
            get(daemon_details).delete(daemon_remove),
        )
        .route("/api/v1/daemons/{id}/labels", post(daemon_labels))
        .route("/api/v1/agents", post(agent_create))
        .route("/api/v1/agents/{name}/{action}", post(agent_action))
        .route("/api/v1/agents", get(agents))
        .route(
            "/api/v1/agents/{name}",
            get(agent_details).patch(agent_update),
        )
        .route("/api/v1/agents/{name}/logs", get(agent_logs))
        .route(
            "/api/v1/agents/{name}/session/history",
            get(crate::web_session::session_history),
        )
        .route("/api/v1/providers", get(providers_annotated))
        .route("/api/v1/harnesses", get(harnesses_json))
        .route(
            "/api/v1/agents/{name}/session",
            get(crate::web_session::session_sse),
        )
        .route(
            "/api/v1/agents/{name}/prompt",
            post(crate::web_session::session_prompt),
        )
        .route(
            "/api/v1/agents/{name}/session_state",
            get(crate::web_session::session_state),
        )
        .route("/api/v1/agents/{name}/shell", get(agent_shell_ws))
        .route("/api/v1/secrets", get(secrets_inventory))
        .route("/api/v1/secrets/reveal", post(secret_reveal))
        .route(
            "/api/v1/secrets/providers/{id}",
            put(secret_set_provider).delete(secret_delete_provider),
        )
        .route(
            "/api/v1/secrets/git-deploy-key",
            put(secret_set_deploy_key).delete(secret_delete_deploy_key),
        )
        .route(
            "/api/v1/secrets/extra/{name}",
            put(secret_set_extra).delete(secret_delete_extra),
        )
        .route("/api/v1/daemons/approve", post(daemon_approve))
        .route("/api/v1/daemons/pending", get(pending_daemons))
        .route(
            "/api/v1/daemons/pending/{id}/approve",
            post(pending_approve),
        )
        .route(
            "/api/v1/daemons/pending/{id}/dismiss",
            post(pending_dismiss),
        )
        .route("/api/v1/audit", get(audit_tail))
        .route("/api/v1/events", get(fleet_events))
        .route("/api/v1/pi-packages", get(pi_packages))
        .with_state(state)
}

// ── pi.dev package catalog ───────────────────────────────────────────────

#[derive(Deserialize)]
struct PiPackagesQuery {
    /// Substring search over name/description/author.
    q: Option<String>,
    /// Badge filter: extension, skill, prompt, theme, …
    r#type: Option<String>,
    page: Option<usize>,
    per_page: Option<usize>,
}

/// Proxied pi.dev package index (crawled + cached server-side; the site is
/// server-rendered HTML with no JSON API and CORS blocks browser fetches).
async fn pi_packages(
    State(_s): State<WebState>,
    Query(q): Query<PiPackagesQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    match crate::pi_packages::Catalog::shared()
        .query(
            q.q.as_deref(),
            q.r#type.as_deref(),
            q.page.unwrap_or(1),
            q.per_page.unwrap_or(50),
        )
        .await
    {
        Ok(page) => Ok(Json(serde_json::to_value(page).unwrap_or_default())),
        Err(e) => Err(err(
            StatusCode::BAD_GATEWAY,
            format!("pi.dev catalog unavailable: {e:#}"),
        )),
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}

async fn app_js() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../../../web/app.js"),
    )
        .into_response()
}

async fn style_css() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/css")],
        include_str!("../../../web/style.css"),
    )
        .into_response()
}

/// Provider/model catalog (pi's supported providers + models), generated
/// by tools/gen-providers.mjs and checked in at web/providers.json.
async fn providers_json() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        include_str!("../../../web/providers.json"),
    )
        .into_response()
}

// Vendored front-end libraries for markdown rendering in the chat.
async fn vendor_marked() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../../../web/vendor/marked.js"),
    )
        .into_response()
}

async fn vendor_dompurify() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../../../web/vendor/dompurify.js"),
    )
        .into_response()
}

// ── helpers ────────────────────────────────────────────────────────────────

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message.into()})))
}

fn internal(e: anyhow::Error) -> (StatusCode, Json<Value>) {
    err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn daemon_json(d: &DaemonRow) -> Value {
    json!({
        "endpoint_id": d.endpoint_id,
        "approved": d.approved,
        "online": d.online,
        "hostname": d.hostname,
        "os": d.os,
        "arch": d.arch,
        "labels": d.effective_labels(),
        "reported_labels": serde_json::from_str::<Value>(&d.labels).unwrap_or_default(),
        "label_overrides": serde_json::from_str::<Value>(&d.label_overrides).unwrap_or_default(),
        "max_agents": d.max_agents,
        "last_seen": d.last_seen,
        "capacity": serde_json::from_str::<Value>(&d.capacity_json).unwrap_or_default(),
        "usage": serde_json::from_str::<Value>(&d.usage_json).unwrap_or_default(),
    })
}

fn agent_json(a: &AgentRow) -> Value {
    json!({
        "id": a.id,
        "name": a.name,
        "daemon_endpoint_id": a.daemon_endpoint_id,
        "manifest": a.manifest,
        "state": crate::store::state_str(a.state),
        // Public vocabulary: running | idle | sleeping | waking | failed.
        "status": suzerain_protocol::state::public_status(a.state, a.busy == Some(true)),
        "busy": a.busy,
        "idle_secs": crate::lifecycle::extrapolated_idle_secs(a),
        "needs_attention": a.needs_attention,
        "auto_suspend_override": a.auto_suspend_override,
        "created_at": a.created_at,
        "session_file": a.session_file,
    })
}

// ── global fleet events (Suzy G6) ────────────────────────────────────────

/// `GET /api/v1/events` — global fleet event stream: lightweight change
/// hints (agent_state, agent_activity, agent, daemon, pending_daemon,
/// audit) as named SSE events. Advisory only: clients refetch the affected
/// lists on receipt; a `resync` event means the receiver lagged and should
/// refetch everything.
async fn fleet_events() -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use futures_util::StreamExt;
    let stream = tokio_stream::wrappers::BroadcastStream::new(crate::events::subscribe())
        .filter_map(|r| async move {
            let ev = match r {
                Ok(v) => axum::response::sse::Event::default()
                    .event(v["kind"].as_str().unwrap_or("event"))
                    .data(v.to_string()),
                Err(_) => axum::response::sse::Event::default()
                    .event("resync")
                    .data("{}"),
            };
            Some(Ok(ev))
        });
    axum::response::Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new().interval(std::time::Duration::from_secs(15)),
    )
}

// ── agent shell (M4: pty into the guest VM) ──────────────────────────────

/// `GET …/shell` (WebSocket upgrade) — interactive pty shell into the
/// agent's VM. Frames are JSON text carrying `ShellMessage`
/// (base64 byte payloads); binary client frames are treated as raw input
/// bytes. Sleeping agents are woken transparently first (same pattern as
/// the prompt endpoint), with progress sent as notice frames.
async fn agent_shell_ws(
    State(s): State<WebState>,
    Path(name): Path<String>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let Some(agent) = s.store.get_agent_by_name(&name).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, "agent not found"));
    };
    Ok(ws.on_upgrade(move |socket| shell_relay(s.cp.clone(), s.store.clone(), agent, socket)))
}

type WsSink =
    futures_util::stream::SplitSink<axum::extract::ws::WebSocket, axum::extract::ws::Message>;

async fn ws_notice(ws_send: &mut WsSink, message: String) {
    use futures_util::SinkExt;
    let msg = serde_json::to_string(&suzerain_protocol::control::ShellMessage::Notice { message })
        .unwrap_or_default();
    let _ = ws_send
        .send(axum::extract::ws::Message::Text(msg.into()))
        .await;
}

/// Wake-if-sleeping (with narration via `notice`), reload, and open the
/// daemon shell stream for an agent. Shared by the WebSocket relay (web
/// UI) and the iroh operator channel (Suzy).
pub(crate) async fn dial_agent_shell(
    cp: &Arc<ControlPlane>,
    store: &Store,
    agent: AgentRow,
    notice: &mut (dyn FnMut(String) + Send),
) -> Option<(
    iroh::endpoint::SendStream,
    tokio::io::BufReader<iroh::endpoint::RecvStream>,
)> {
    use suzerain_protocol::control::StreamHello;

    if !crate::wake::is_awake(cp, &agent).await {
        notice(format!("agent '{}' is sleeping — waking…", agent.name));
        if let Err(e) = crate::wake::ensure_awake(cp, &agent).await {
            notice(format!("wake failed: {e:#}"));
            return None;
        }
        notice("agent is awake".to_string());
    }
    // Reload: a wake may have moved the agent to another daemon.
    let agent = match store.get_agent_by_name(&agent.name).await {
        Ok(Some(a)) => a,
        _ => {
            notice("agent disappeared from the registry".to_string());
            return None;
        }
    };
    let daemon: iroh::EndpointId = agent.daemon_endpoint_id.parse().ok()?;
    match cp
        .open_stream_retry(&daemon, &StreamHello::Shell { agent_id: agent.id })
        .await
    {
        Ok(streams) => Some(streams),
        Err(_) => {
            notice("daemon unreachable".to_string());
            None
        }
    }
}

async fn shell_relay(
    cp: Arc<ControlPlane>,
    store: Store,
    agent: AgentRow,
    socket: axum::extract::ws::WebSocket,
) {
    use axum::extract::ws::Message;
    use futures_util::{SinkExt, StreamExt};
    use suzerain_protocol::control::ShellMessage;
    use suzerain_protocol::framing::{read_jsonl, write_jsonl};

    let (mut ws_send, mut ws_recv) = socket.split();

    // Wake (narrated) + reload + dial the daemon shell stream.
    let (dialed, pending) = {
        let mut pending: Vec<String> = Vec::new();
        let result = dial_agent_shell(&cp, &store, agent, &mut |m| pending.push(m)).await;
        (result, pending)
    };
    for m in pending {
        ws_notice(&mut ws_send, m).await;
    }
    let Some((mut send, mut recv)) = dialed else {
        return;
    };

    loop {
        tokio::select! {
            frame = ws_recv.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(msg @ (ShellMessage::Data { .. } | ShellMessage::Resize { .. })) =
                            serde_json::from_str::<ShellMessage>(&text)
                        {
                            if write_jsonl(&mut send, &msg).await.is_err() { break; }
                        }
                    }
                    // Binary frames: raw input bytes → base64 Data.
                    Some(Ok(Message::Binary(bytes))) => {
                        let msg = ShellMessage::Data {
                            data: crate::bundle::base64_encode(&bytes),
                        };
                        if write_jsonl(&mut send, &msg).await.is_err() { break; }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            msg = read_jsonl::<_, ShellMessage>(&mut recv) => {
                match msg {
                    Ok(shell_msg) => {
                        let done = matches!(shell_msg, ShellMessage::Exit { .. });
                        let text = serde_json::to_string(&shell_msg).unwrap_or_default();
                        if ws_send.send(Message::Text(text.into())).await.is_err() { break; }
                        if done { break; }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

// ── endpoints ─────────────────────────────────────────────────────────────

async fn endpoint(State(s): State<WebState>) -> Json<Value> {
    Json(json!({
        "endpoint_id": s.cp.endpoint_id().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn overview(State(s): State<WebState>) -> ApiResult {
    let daemons = s.store.list_daemons().await.map_err(internal)?;
    let agents = s.store.list_agents().await.map_err(internal)?;
    let online = daemons.iter().filter(|d| d.online && d.approved).count();
    let mut states: std::collections::BTreeMap<String, usize> = Default::default();
    for a in &agents {
        *states
            .entry(crate::store::state_str(a.state).to_string())
            .or_default() += 1;
    }
    Ok(Json(json!({
        "endpoint_id": s.cp.endpoint_id().to_string(),
        "daemons_total": daemons.len(),
        "daemons_online": online,
        "agents_total": agents.len(),
        "agents_by_state": states,
    })))
}

async fn daemons(State(s): State<WebState>) -> ApiResult {
    let daemons = s.store.list_daemons().await.map_err(internal)?;
    Ok(Json(
        json!({"daemons": daemons.iter().map(daemon_json).collect::<Vec<_>>()}),
    ))
}

async fn daemon_details(State(s): State<WebState>, Path(id): Path<String>) -> ApiResult {
    let daemons = s.store.list_daemons().await.map_err(internal)?;
    let Some(d) = daemons
        .iter()
        .find(|d| d.endpoint_id.starts_with(&id) || d.hostname == id)
    else {
        return Err(err(StatusCode::NOT_FOUND, "daemon not found"));
    };
    let agents: Vec<Value> = s
        .store
        .list_agents()
        .await
        .map_err(internal)?
        .iter()
        .filter(|a| a.daemon_endpoint_id == d.endpoint_id)
        .map(agent_json)
        .collect();
    let audit = audit::read_tail(200)
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|e| e.to_string().contains(&d.endpoint_id))
        .collect::<Vec<_>>();
    let mut out = daemon_json(d);
    out["agents"] = json!(agents);
    out["audit"] = json!(audit);
    Ok(Json(out))
}

async fn agents(State(s): State<WebState>) -> ApiResult {
    let agents = s.store.list_agents().await.map_err(internal)?;
    let daemons = s.store.list_daemons().await.map_err(internal)?;
    let host = |eid: &str| {
        daemons
            .iter()
            .find(|d| d.endpoint_id == eid)
            .map(|d| d.hostname.clone())
            .unwrap_or_default()
    };
    let rows: Vec<Value> = agents
        .iter()
        .map(|a| {
            let mut v = agent_json(a);
            v["daemon_hostname"] = json!(host(&a.daemon_endpoint_id));
            v
        })
        .collect();
    Ok(Json(json!({"agents": rows})))
}

async fn agent_details(State(s): State<WebState>, Path(name): Path<String>) -> ApiResult {
    let Some(agent) = s.store.get_agent_by_name(&name).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, "agent not found"));
    };
    let daemons = s.store.list_daemons().await.map_err(internal)?;
    let mut out = agent_json(&agent);
    if let Some(d) = daemons
        .iter()
        .find(|d| d.endpoint_id == agent.daemon_endpoint_id)
    {
        out["daemon"] = daemon_json(d);
    }
    let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
    let events: Vec<Value> = tokio::fs::read_to_string(&log)
        .await
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    out["last_event_at"] = json!(events.last().map(|e| e["at"].clone()));
    out["event_count"] = json!(events.len());
    out["sessions"] = json!(s
        .store
        .list_agent_sessions(&agent.id)
        .await
        .map_err(internal)?);
    out["manifest_toml"] = json!(toml::to_string_pretty(&agent.manifest).unwrap_or_default());
    Ok(Json(out))
}

#[derive(Deserialize)]
struct LogsQuery {
    tail: Option<usize>,
    kind: Option<String>,
    q: Option<String>,
}

async fn agent_logs(
    State(s): State<WebState>,
    Path(name): Path<String>,
    Query(q): Query<LogsQuery>,
) -> ApiResult {
    let Some(agent) = s.store.get_agent_by_name(&name).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, "agent not found"));
    };
    let tail = q.tail.unwrap_or(200).min(2000);
    let log = data_dir().join("logs").join(format!("{}.jsonl", agent.id));
    let content = tokio::fs::read_to_string(&log).await.unwrap_or_default();
    let mut events: Vec<Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if let Some(kind) = &q.kind {
        events.retain(|e| e["kind"].as_str() == Some(kind.as_str()));
    }
    if let Some(needle) = &q.q {
        events.retain(|e| e.to_string().contains(needle.as_str()));
    }
    let total = events.len();
    let start = total.saturating_sub(tail);
    Ok(Json(
        json!({"events": &events[start..], "total_matching": total}),
    ))
}

async fn secrets_inventory(State(s): State<WebState>) -> ApiResult {
    let entries = crate::secrets::status();
    let agents = s.store.list_agents().await.map_err(internal)?;
    let used_by = |provider: &str| {
        agents
            .iter()
            .filter(|a| a.manifest.secrets.providers.iter().any(|p| p == provider))
            .count()
    };
    let rows: Vec<Value> = entries
        .iter()
        .map(|e| {
            let (kind, name) = match e.split_once(':') {
                Some((k, n)) => (k.to_string(), n.to_string()),
                None => ("provider".to_string(), e.clone()),
            };
            json!({
                "kind": kind,
                "name": name,
                "used_by": if kind == "provider" { used_by(&name) } else { 0 },
            })
        })
        .collect();
    Ok(Json(
        json!({"entries": rows, "store_present": crate::secrets::secrets_path().exists()}),
    ))
}

// ── M2: actions ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateBody {
    manifest_toml: Option<String>,
    manifest: Option<Value>,
    require: Option<std::collections::BTreeMap<String, String>>,
    daemon: Option<String>,
}

async fn agent_create(
    State(s): State<WebState>,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let manifest: suzerain_protocol::manifest::AgentManifest = if let Some(t) = &body.manifest_toml
    {
        toml::from_str(t).map_err(|e| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid manifest TOML: {e}"),
            )
        })?
    } else if let Some(v) = body.manifest {
        serde_json::from_value(v).map_err(|e| {
            err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid manifest: {e}"),
            )
        })?
    } else {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "manifest_toml or manifest required",
        ));
    };
    if let Err(e) = crate::catalog::validate_model(&manifest.model.provider, &manifest.model.id) {
        return Err(err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")));
    }
    match crate::actions::create_agent_background(
        &s.cp,
        manifest,
        body.require.unwrap_or_default(),
        body.daemon,
    )
    .await
    {
        Ok((agent, daemon_hostname)) => {
            let mut out = agent_json(&agent);
            out["daemon_hostname"] = json!(daemon_hostname);
            Ok(Json(out))
        }
        Err(e) => Err(err(StatusCode::CONFLICT, format!("{e:#}"))),
    }
}

#[derive(Default, Deserialize)]
struct ActionBody {
    /// Update the registry even if the daemon is unreachable (the VM may
    /// keep running orphaned; audit-logged).
    force: Option<bool>,
}

/// The only remaining lifecycle verb: destroy. Start/stop/suspend/restore
/// are handled automatically by the control plane (auto-suspend +
/// transparent wake) and are not user-invokable.
async fn agent_action(
    State(s): State<WebState>,
    Path((name, action)): Path<(String, String)>,
    body: Option<Json<ActionBody>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let force = body.and_then(|b| b.force).unwrap_or(false);
    match action.as_str() {
        "destroy" => {}
        other => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!(
                    "unknown action '{other}' — agents suspend and wake automatically; \
                     the only lifecycle action is 'destroy'"
                ),
            ));
        }
    };
    crate::actions::destroy_agent(&s.cp, &name, force)
        .await
        .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct AgentUpdateBody {
    /// Runtime auto-suspend override: a duration ("10m"), "never", or
    /// "default" to clear the override and inherit the global policy.
    auto_suspend: Option<String>,
}

/// `PATCH /api/v1/agents/{name}` — per-agent policy overrides.
async fn agent_update(
    State(s): State<WebState>,
    Path(name): Path<String>,
    Json(body): Json<AgentUpdateBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(agent) = s.store.get_agent_by_name(&name).await.map_err(internal)? else {
        return Err(err(StatusCode::NOT_FOUND, "agent not found"));
    };
    let Some(value) = body.auto_suspend.as_deref() else {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "auto_suspend required",
        ));
    };
    let policy = suzerain_protocol::manifest::Lifecycle {
        auto_suspend: Some(value.to_string()),
    }
    .auto_suspend_policy()
    .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let stored = match policy {
        suzerain_protocol::manifest::AutoSuspendPolicy::Inherit => None,
        _ => Some(value),
    };
    s.store
        .set_auto_suspend_override(&agent.id, stored)
        .await
        .map_err(internal)?;
    crate::audit::record(
        "agent_config",
        json!({"name": name, "id": agent.id, "auto_suspend": value, "via": "web"}),
    )
    .await;
    Ok(Json(json!({"ok": true, "auto_suspend": stored})))
}

/// Harness catalog (provisionable harness kinds + versions), checked in at
/// web/harnesses.json — the single source of truth for the web UI and the
/// MCP server's create-time validation.
async fn harnesses_json() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        include_str!("../../../web/harnesses.json"),
    )
        .into_response()
}

/// Provider catalog annotated for programmatic agent creation: the static
/// provider/model list plus, per provider, whether a key can be injected
/// into the guest at all (`key_injectable`) and whether the store holds
/// one (`key_configured`). Names only — never values.
async fn providers_annotated(State(_s): State<WebState>) -> Response {
    let raw = crate::catalog::providers_json();
    let configured: std::collections::BTreeSet<String> = crate::secrets::inventory()
        .into_iter()
        .filter(|(kind, _)| kind == "provider")
        .map(|(_, name)| name)
        .collect();
    let mut out = serde_json::Map::new();
    if let Some(providers) = raw["providers"].as_object() {
        for (id, entry) in providers {
            out.insert(
                id.clone(),
                json!({
                    "models": entry["models"],
                    "key_injectable": suzerain_protocol::secrets::provider_env_and_host(id).is_some(),
                    "key_configured": configured.contains(id),
                }),
            );
        }
    }
    Json(json!({"providers": out})).into_response()
}

#[derive(Deserialize)]
struct DaemonRemoveQuery {
    force: Option<bool>,
}

/// `DELETE /api/v1/daemons/{id}?force=` — remove an (approved) daemon.
/// Refuses while agents are assigned unless force: then each agent gets a
/// best-effort destroy order (same tolerance rules as lifecycle) and its
/// registry row is deleted alongside the daemon.
async fn daemon_remove(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Query(q): Query<DaemonRemoveQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = s.store.clone();
    let daemon = store
        .list_daemons()
        .await
        .map_err(internal)?
        .into_iter()
        .find(|d| d.endpoint_id == id || d.endpoint_id.starts_with(&id) || d.hostname == id)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, format!("no daemon matching '{id}'")))?;
    let agents: Vec<String> = store
        .list_agents()
        .await
        .map_err(internal)?
        .into_iter()
        .filter(|a| a.daemon_endpoint_id == daemon.endpoint_id)
        .map(|a| a.name)
        .collect();
    let force = q.force.unwrap_or(false);
    if !agents.is_empty() && !force {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "daemon '{}' still has agents: {} — destroy them first or retry with force=true",
                daemon.hostname,
                agents.join(", ")
            ),
        ));
    }
    for name in &agents {
        // Best-effort: destroy already tolerates unreachable daemons and
        // missing agents.
        if let Err(e) = crate::actions::destroy_agent(&s.cp, name, true).await {
            tracing::warn!(agent = %name, "force destroy during daemon remove failed: {e:#}");
            // destroy may not reach the delete when the order transport
            // fails outright; make sure the row goes away regardless.
            if let Ok(Some(agent)) = store.get_agent_by_name(name).await {
                let _ = store.delete_agent(&agent.id).await;
            }
        }
    }
    store
        .delete_daemon(&daemon.endpoint_id)
        .await
        .map_err(internal)?;
    crate::audit::record(
        "daemon_remove",
        json!({"endpoint_id": daemon.endpoint_id, "hostname": daemon.hostname, "force": force, "agents_removed": agents}),
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct LabelsBody {
    set: Option<std::collections::BTreeMap<String, String>>,
    remove: Option<Vec<String>>,
}

async fn daemon_labels(
    State(s): State<WebState>,
    Path(id): Path<String>,
    Json(body): Json<LabelsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let mut daemons = s.store.list_daemons().await.map_err(internal)?;
    let Some(d) = daemons
        .iter_mut()
        .find(|d| d.endpoint_id.starts_with(&id) || d.hostname == id)
    else {
        return Err(err(StatusCode::NOT_FOUND, "daemon not found"));
    };
    let mut overrides: std::collections::BTreeMap<String, String> =
        serde_json::from_str(&d.label_overrides).unwrap_or_default();
    if let Some(set) = body.set {
        overrides.extend(set);
    }
    if let Some(remove) = body.remove {
        for k in remove {
            overrides.remove(&k);
        }
    }
    let overrides_json = serde_json::to_string(&overrides).unwrap();
    s.store
        .set_label_overrides(&d.endpoint_id, &overrides_json)
        .await
        .map_err(internal)?;
    crate::audit::record(
        "daemon_label",
        json!({"endpoint_id": d.endpoint_id, "overrides": overrides}),
    )
    .await;
    let mut effective = d.effective_labels();
    effective.extend(overrides);
    Ok(Json(json!({"effective_labels": effective})))
}

// ── M4: secrets CRUD ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ValueBody {
    value: Option<String>,
}

async fn secret_set_provider(
    Path(id): Path<String>,
    Json(body): Json<ValueBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let value = body.value.unwrap_or_default();
    crate::secrets::set_provider(&id, &value)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    crate::audit::record("secret_set", json!({"kind": "provider", "name": id})).await;
    Ok(Json(json!({"ok": true})))
}

async fn secret_delete_provider(
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    crate::secrets::delete_provider(&id)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    crate::audit::record("secret_delete", json!({"kind": "provider", "name": id})).await;
    Ok(Json(json!({"ok": true})))
}

async fn secret_set_deploy_key(
    Json(body): Json<ValueBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let value = body.value.unwrap_or_default();
    crate::secrets::set_deploy_key(&value)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    crate::audit::record("secret_set", json!({"kind": "git", "name": "deploy_key"})).await;
    Ok(Json(json!({"ok": true})))
}

async fn secret_delete_deploy_key() -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    crate::secrets::delete_deploy_key()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    crate::audit::record(
        "secret_delete",
        json!({"kind": "git", "name": "deploy_key"}),
    )
    .await;
    Ok(Json(json!({"ok": true})))
}

async fn secret_set_extra(
    Path(name): Path<String>,
    Json(body): Json<ValueBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let value = body.value.unwrap_or_default();
    crate::secrets::set_extra(&name, &value)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    crate::audit::record("secret_set", json!({"kind": "extra", "name": name})).await;
    Ok(Json(json!({"ok": true})))
}

async fn secret_delete_extra(
    Path(name): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    crate::secrets::delete_extra(&name)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    crate::audit::record("secret_delete", json!({"kind": "extra", "name": name})).await;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct RevealBody {
    kind: String,
    name: String,
}

/// Audited reveal-once (spec decision #4): the value is returned once, the
/// audit entry records kind/name/actor but never the value.
async fn secret_reveal(
    Json(body): Json<RevealBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let value = crate::secrets::reveal(&body.kind, &body.name)
        .map_err(|e| err(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    crate::audit::record(
        "secret_reveal",
        json!({"kind": body.kind, "name": body.name, "actor": "web"}),
    )
    .await;
    Ok(Json(json!({"value": value})))
}

#[derive(Deserialize)]
struct ApproveBody {
    endpoint_id: String,
}

async fn daemon_approve(
    State(s): State<WebState>,
    Json(body): Json<ApproveBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let id: iroh::EndpointId = body
        .endpoint_id
        .parse()
        .map_err(|_| err(StatusCode::UNPROCESSABLE_ENTITY, "invalid endpoint id"))?;
    s.store
        .approve_daemon(&id.to_string())
        .await
        .map_err(internal)?;
    crate::audit::record("daemon_approve", json!({"endpoint_id": id.to_string()})).await;
    Ok(Json(json!({"approved": id.to_string()})))
}

// ── M4: pending enrollments ───────────────────────────────────────────────

async fn pending_daemons(State(s): State<WebState>) -> ApiResult {
    Ok(Json(
        json!({"pending": s.store.list_pending_daemons().await.map_err(internal)?}),
    ))
}

async fn pending_approve(
    State(s): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = s.store.clone();
    let pending = store.list_pending_daemons().await.map_err(internal)?;
    let Some(p) = pending
        .iter()
        .find(|p| p["endpoint_id"].as_str().unwrap_or("").starts_with(&id))
    else {
        return Err(err(StatusCode::NOT_FOUND, "pending daemon not found"));
    };
    let endpoint_id = p["endpoint_id"].as_str().unwrap().to_string();
    store.approve_daemon(&endpoint_id).await.map_err(internal)?;
    crate::audit::record("daemon_approve", json!({"endpoint_id": endpoint_id})).await;
    Ok(Json(json!({"approved": endpoint_id})))
}

async fn pending_dismiss(
    State(s): State<WebState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let store = s.store.clone();
    store.delete_pending_daemon(&id).await.map_err(internal)?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
struct AuditQuery {
    tail: Option<usize>,
    action: Option<String>,
}

async fn audit_tail(State(_s): State<WebState>, Query(q): Query<AuditQuery>) -> ApiResult {
    let mut entries = audit::read_tail(q.tail.unwrap_or(100).min(500))
        .await
        .map_err(internal)?;
    if let Some(action) = &q.action {
        entries.retain(|e| e["action"].as_str() == Some(action.as_str()));
    }
    Ok(Json(json!({"entries": entries})))
}
