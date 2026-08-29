//! `--debug <view>` boot mode: opens Suzy straight into one named view,
//! pre-populated with fixture data instead of a real workspace connection.
//!
//! This exists so a screenshot (by a human or an agent) can confirm a
//! view's styling against `crates/suzy/design-system/` without needing a
//! live suzerain control plane, real daemons, or real agents. It never
//! touches `~/.config/suzy` — the identity is ephemeral and nothing is
//! persisted.
//!
//! Every view/tab reads from the same fixture workspace (one "debug"
//! workspace, agent `atlas`), so clicking around after landing on the
//! requested view keeps working without triggering real network fetches —
//! each cache map (`logs`, `details`, `activity`, `secrets`, `chats`,
//! `shells`) is pre-seeded, which is what the app's own `!contains_key(..)`
//! guards check before dispatching a fetch.

use std::collections::HashMap;
use std::sync::mpsc::channel;
use std::sync::Arc;

use serde_json::json;
use suzerain_client::{Agent, Client, Daemon, EndpointInfo, Overview};

use crate::chat::{Chat, ChatItem, Part};
use crate::config::{Config, WorkspaceCfg};
use crate::create::CreateForm;
use crate::terminal::Terminal;
use crate::views::{ActivityState, DetailsState, LogsState, SecretsState};
use crate::{ShellState, SuzyApp, View, Workspace};

/// Names accepted by `suzy --debug <name>`, matching the app's top-level
/// views plus each agent tab (the fixture agent is `atlas`).
pub const VIEW_NAMES: &[&str] = &[
    "dashboard",
    "castellans",
    "activity",
    "secrets",
    "chat",
    "shell",
    "logs",
    "details",
    "add-workspace",
    "create-agent",
];

const FIXTURE_AGENT: &str = "atlas";

/// Builds a `SuzyApp` pre-loaded with fixture data and pointed at `view`.
/// `Err` names the bad input back to the caller (unknown view name).
pub fn build_app(
    cc: &eframe::CreationContext,
    rt: tokio::runtime::Runtime,
    view: &str,
) -> Result<SuzyApp, String> {
    if !VIEW_NAMES.contains(&view) {
        return Err(format!(
            "unknown --debug view {view:?}; expected one of: {}",
            VIEW_NAMES.join(", ")
        ));
    }

    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());
    crate::theme::install_fonts(&cc.egui_ctx);
    crate::theme::apply(&cc.egui_ctx);

    let iroh_key = suzerain_client::iroh::SecretKey::generate();
    let client = Arc::new(Client::new("debug-fixture-not-dialed", iroh_key.clone()));
    let ws_id: crate::net::WsId = 0;

    let (tx, rx) = channel();
    let mut app = SuzyApp {
        rt,
        stored_ctx: Some(cc.egui_ctx.clone()),
        iroh_key,
        cfg: Config::default(),
        config_path: std::env::temp_dir().join("suzy-debug-config.toml"),
        workspaces: vec![Workspace {
            cfg: WorkspaceCfg {
                name: "debug".to_string(),
                endpoint_id: "debug-fixture-endpoint-id".to_string(),
                test_addr: None,
            },
            client,
            endpoint: Some(endpoint_info()),
            error: None,
            overview: Some(overview()),
            agents: agents(),
            daemons: daemons(),
            pending: pending(),
            providers: Some(providers_catalog()),
            harnesses: None,
            loop_handle: None,
        }],
        active_ws: Some(ws_id),
        view: View::Dashboard,
        chats: HashMap::new(),
        logs: HashMap::new(),
        details: HashMap::new(),
        activity: HashMap::new(),
        secrets: HashMap::new(),
        session_handles: HashMap::new(),
        shells: HashMap::new(),
        shell_handles: HashMap::new(),
        tx,
        rx,
        add_ws_open: view == "add-workspace",
        add_ws_name: String::new(),
        add_ws_eid: String::new(),
        create_open: view == "create-agent",
        create_form: CreateForm::default(),
        destroy_confirm: None,
        remove_ws_confirm: None,
        labels_editing: None,
        labels_draft: String::new(),
        status_msg: Some(format!(
            "debug mode — view {view:?}, fixture data (no live control plane)"
        )),
    };

    // Pre-seed every per-agent/per-workspace cache so the app's
    // `!contains_key(..)` fetch guards stay quiet — no background task
    // will ever try to dial `client`, whatever the operator clicks next.
    app.activity.insert(ws_id, activity_state());
    app.secrets.insert(ws_id, secrets_state());
    let agent_key = (ws_id, FIXTURE_AGENT.to_string());
    app.logs.insert(agent_key.clone(), logs_state());
    app.details.insert(agent_key.clone(), details_state());
    app.chats.insert(agent_key.clone(), chat_fixture());
    app.shells.insert(agent_key.clone(), shell_state());
    // A live-looking (not "connection closed") shell needs a handle in the
    // map too — a trivial task that just parks forever, aborted on drop.
    app.shell_handles.insert(
        agent_key,
        app.rt.spawn(std::future::pending::<()>()).abort_handle(),
    );

    app.view = match view {
        "dashboard" => View::Dashboard,
        "castellans" => View::Castellans,
        "activity" => View::Activity,
        "secrets" => View::Secrets,
        "chat" => View::Agent {
            ws: ws_id,
            agent: FIXTURE_AGENT.to_string(),
            tab: crate::AgentTab::Chat,
        },
        "shell" => View::Agent {
            ws: ws_id,
            agent: FIXTURE_AGENT.to_string(),
            tab: crate::AgentTab::Shell,
        },
        "logs" => View::Agent {
            ws: ws_id,
            agent: FIXTURE_AGENT.to_string(),
            tab: crate::AgentTab::Logs,
        },
        "details" => View::Agent {
            ws: ws_id,
            agent: FIXTURE_AGENT.to_string(),
            tab: crate::AgentTab::Details,
        },
        // These two are dialogs overlaid on the dashboard, not distinct
        // views — the `add_ws_open`/`create_open` flags set above are what
        // actually opens them.
        "add-workspace" | "create-agent" => View::Dashboard,
        _ => unreachable!("validated above"),
    };

    Ok(app)
}

// ── fixture data ─────────────────────────────────────────────────────────

fn endpoint_info() -> EndpointInfo {
    serde_json::from_value(json!({
        "endpoint_id": "debug-fixture-endpoint-id",
        "version": "debug",
    }))
    .expect("fixture EndpointInfo")
}

fn overview() -> Overview {
    serde_json::from_value(json!({
        "endpoint_id": "debug-fixture-endpoint-id",
        "daemons_total": 2,
        "daemons_online": 1,
        "agents_total": 5,
        "agents_by_state": {
            "running": 1, "idle": 1, "waking": 1, "sleeping": 1, "failed": 1
        },
    }))
    .expect("fixture Overview")
}

fn daemons() -> Vec<Daemon> {
    vec![
        serde_json::from_value(json!({
            "endpoint_id": "debug-daemon-workshop-01",
            "approved": true, "online": true,
            "hostname": "vm-workshop-01", "os": "linux", "arch": "x86_64",
            "labels": {"zone": "prod", "pool": "canary"},
            "reported_labels": {"zone": "prod"},
            "label_overrides": {"pool": "canary"},
            "max_agents": 4, "last_seen": "2026-08-29T13:58:00Z",
            "capacity": {"vcpu_total": 8, "memory_mib_total": 16384, "disk_mib_total": 102400, "gpus": []},
            "usage": {"memory_mib_free": 8192, "cpu_load1": 0.42, "disk_mib_free": 51200, "gpus": []},
        }))
        .expect("fixture Daemon"),
        serde_json::from_value(json!({
            "endpoint_id": "debug-daemon-workshop-02",
            "approved": true, "online": false,
            "hostname": "vm-workshop-02", "os": "linux", "arch": "aarch64",
            "labels": {}, "reported_labels": {}, "label_overrides": {},
            "max_agents": 2, "last_seen": "2026-08-29T09:12:00Z",
            "capacity": {"vcpu_total": 4, "memory_mib_total": 8192, "disk_mib_total": 51200, "gpus": []},
            "usage": {"memory_mib_free": 0, "cpu_load1": 0.0, "disk_mib_free": 0, "gpus": []},
        }))
        .expect("fixture Daemon"),
    ]
}

fn pending() -> Vec<serde_json::Value> {
    vec![json!({
        "endpoint_id": "debug-pending-workshop-03",
        "hostname": "vm-workshop-03",
        "os": "linux", "arch": "x86_64",
        "capacity": {}, "first_seen": "2026-08-29T14:01:00Z", "last_seen": "2026-08-29T14:01:00Z",
    })]
}

fn manifest_json(name: &str, provider: &str, model: &str) -> serde_json::Value {
    json!({
        "name": name,
        "harness": {"type": "pi", "version": "0.84.1"},
        "model": {"provider": provider, "id": model},
    })
}

fn agents() -> Vec<Agent> {
    let rows: [(&str, &str, &str, &str, bool); 5] = [
        (
            FIXTURE_AGENT,
            "debug-daemon-workshop-01",
            "vm-workshop-01",
            "running",
            false,
        ),
        (
            "hollis",
            "debug-daemon-workshop-01",
            "vm-workshop-01",
            "idle",
            false,
        ),
        (
            "periwinkle",
            "debug-daemon-workshop-01",
            "vm-workshop-01",
            "waking",
            false,
        ),
        (
            "brackish",
            "debug-daemon-workshop-02",
            "vm-workshop-02",
            "sleeping",
            false,
        ),
        (
            "driftwood",
            "debug-daemon-workshop-01",
            "vm-workshop-01",
            "failed",
            true,
        ),
    ];
    rows.into_iter()
        .enumerate()
        .map(
            |(i, (name, daemon_eid, daemon_host, status, needs_attention))| {
                let idle_secs = if status == "idle" {
                    serde_json::Value::from(240)
                } else {
                    serde_json::Value::Null
                };
                serde_json::from_value(json!({
                    "id": format!("00000000-0000-0000-0000-0000000000{i:02}"),
                    "name": name,
                    "daemon_endpoint_id": daemon_eid,
                    "daemon_hostname": daemon_host,
                    "manifest": manifest_json(name, "kimi-coding", "kimi-for-coding"),
                    "state": "active",
                    "status": status,
                    "busy": status == "running",
                    "idle_secs": idle_secs,
                    "needs_attention": needs_attention,
                    "auto_suspend_override": serde_json::Value::Null,
                    "created_at": "2026-08-20T14:03:00Z",
                    "session_file": "/agent/sessions/s1.jsonl",
                }))
                .expect("fixture Agent")
            },
        )
        .collect()
}

fn providers_catalog() -> serde_json::Value {
    json!({
        "providers": {
            "kimi-coding": {}, "anthropic": {}, "openai": {},
        }
    })
}

fn activity_state() -> ActivityState {
    ActivityState {
        entries: vec![
            json!({"at": "2026-08-29T14:01:05Z", "actor": "operator", "action": "daemon_approve",
                   "detail": {"endpoint_id": "debug-daemon-workshop-01"}}),
            json!({"at": "2026-08-29T14:02:10Z", "actor": "operator", "action": "agent_create",
                   "detail": {"name": FIXTURE_AGENT}}),
            json!({"at": "2026-08-29T14:03:00Z", "actor": "operator", "action": "secret_set_provider",
                   "detail": {"name": "kimi-coding"}}),
            json!({"at": "2026-08-29T14:10:44Z", "actor": "system", "action": "agent_wake",
                   "detail": {"name": "periwinkle"}}),
            json!({"at": "2026-08-29T14:12:31Z", "actor": "system", "action": "agent_crash",
                   "detail": {"name": "driftwood", "reason": "pi_exit(1)"}}),
            json!({"at": "2026-08-29T14:15:02Z", "actor": "operator", "action": "daemon_remove",
                   "detail": {"endpoint_id": "debug-daemon-workshop-99"}}),
        ],
        action: String::new(),
        q: String::new(),
        tail: 300,
        loaded: true,
        error: None,
    }
}

fn secrets_state() -> SecretsState {
    SecretsState {
        value: Some(json!({
            "store_present": true,
            "entries": [
                {"kind": "provider", "name": "kimi-coding", "used_by": 3},
                {"kind": "provider", "name": "anthropic", "used_by": 0},
                {"kind": "git", "name": "deploy-key"},
                {"kind": "extra", "name": "SLACK_WEBHOOK@vm-workshop-01", "used_by": 1},
            ]
        })),
        loaded: true,
        ..SecretsState::default()
    }
}

fn logs_state() -> LogsState {
    let events = vec![
        json!({"kind": "session_started", "at": "2026-08-29T14:03:00Z", "payload": {}}),
        json!({"kind": "turn_start", "at": "2026-08-29T14:03:02Z", "payload": {}}),
        json!({"kind": "message_end", "at": "2026-08-29T14:03:05Z",
               "payload": {"message": {"role": "user", "content": "check the deploy queue"}}}),
        json!({"kind": "message_end", "at": "2026-08-29T14:03:11Z",
               "payload": {"message": {"role": "assistant", "content": "queue is clear, nothing pending"}}}),
        json!({"kind": "turn_end", "at": "2026-08-29T14:03:11Z", "payload": {}}),
        json!({"kind": "pi_stderr", "at": "2026-08-29T14:04:00Z",
               "payload": {"line": "warn: provider latency 1200ms"}}),
    ];
    LogsState {
        kind: String::new(),
        q: String::new(),
        tail: 200,
        total: events.len(),
        events,
        error: None,
        loaded: true,
    }
}

fn details_state() -> DetailsState {
    DetailsState {
        value: Some(json!({
            "state": "active",
            "created_at": "2026-08-20T14:03:00Z",
            "auto_suspend_override": serde_json::Value::Null,
            "sessions": [
                {"started_at": "2026-08-20T14:03:00Z", "session_file": "/agent/sessions/s1.jsonl", "ended_at": "2026-08-25T09:00:00Z"},
                {"started_at": "2026-08-25T09:05:00Z", "session_file": "/agent/sessions/s2.jsonl", "ended_at": serde_json::Value::Null},
            ],
            "manifest_toml": "name = \"atlas\"\n\n[harness]\ntype = \"pi\"\nversion = \"0.84.1\"\n\n[model]\nprovider = \"kimi-coding\"\nid = \"kimi-for-coding\"\n",
        })),
        error: None,
        auto_suspend_input: String::new(),
        loaded: true,
    }
}

fn chat_fixture() -> Chat {
    let mut chat = Chat::new(FIXTURE_AGENT.to_string());
    chat.items = vec![
        ChatItem::System("session started".to_string()),
        ChatItem::User("check the deploy queue".to_string()),
        ChatItem::Assistant(vec![
            Part::Thinking("scanning the queue for stuck jobs".to_string()),
            Part::Text("queue is clear, nothing pending".to_string()),
        ]),
        ChatItem::ToolResult {
            name: "run_shell".to_string(),
            text: "deploy-queue: 0 pending, 0 failed".to_string(),
            is_error: false,
        },
        ChatItem::User("thanks — flag anything odd".to_string()),
        ChatItem::Error("provider timeout, retrying…".to_string()),
    ];
    chat.status_line = "idle".to_string();
    chat
}

fn shell_state() -> ShellState {
    let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut term = Terminal::default();
    term.write_system("connected — debug fixture shell");
    term.feed(b"$ ls\r\nCargo.toml  src/  design-system/\r\n$ ");
    ShellState {
        term,
        input: input_tx,
        exited: None,
        error: None,
    }
}
