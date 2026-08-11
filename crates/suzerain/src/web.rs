//! Embedded web UI (local-only, docs/WEB-UI.md): axum server on
//! 127.0.0.1 serving a vanilla-JS SPA plus a REST/JSON API backed directly
//! by the Store and ControlPlane.

use std::sync::Arc;

use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::audit;
use crate::control::ControlPlane;
use crate::identity::data_dir;
use crate::store::{AgentRow, DaemonRow, Store};

#[derive(Clone)]
struct WebState {
    store: Store,
    cp: Arc<ControlPlane>,
}

/// Start the web server (blocks forever). Binds localhost only.
pub async fn serve(store: Store, cp: Arc<ControlPlane>, port: u16) -> Result<()> {
    let state = WebState { store, cp };
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/api/v1/endpoint", get(endpoint))
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/daemons", get(daemons))
        .route("/api/v1/daemons/{id}", get(daemon_details))
        .route("/api/v1/daemons/{id}/labels", post(daemon_labels))
        .route("/api/v1/agents", post(agent_create))
        .route("/api/v1/agents/{name}/{action}", post(agent_action))
        .route("/api/v1/agents", get(agents))
        .route("/api/v1/agents/{name}", get(agent_details))
        .route("/api/v1/agents/{name}/logs", get(agent_logs))
        .route("/api/v1/secrets", get(secrets_inventory))
        .route("/api/v1/audit", get(audit_tail))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    info!(port, "web ui listening on http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

// ── static assets ─────────────────────────────────────────────────────────

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
        "created_at": a.created_at,
        "session_file": a.session_file,
    })
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
    match crate::actions::create_agent(
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

async fn agent_action(
    State(s): State<WebState>,
    Path((name, action)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let action = match action.as_str() {
        "start" => crate::actions::Lifecycle::Start,
        "stop" => crate::actions::Lifecycle::Stop,
        "suspend" => crate::actions::Lifecycle::Suspend,
        "destroy" => crate::actions::Lifecycle::Destroy,
        other => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("unknown action '{other}'"),
            ));
        }
    };
    crate::actions::lifecycle(&s.cp, &name, action)
        .await
        .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
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
