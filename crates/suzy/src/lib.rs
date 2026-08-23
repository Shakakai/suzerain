//! Suzy — desktop operator console for suzerain control planes.
//!
//! Layout (herdr-mapped, docs/SUZY.md):
//! - left sidebar: workspaces (connections) → daemons → agents with live
//!   status dots (running / idle / sleeping / waking / failed)
//! - main area: dashboard, castellans (pending enrollments, labels), or a
//!   per-agent tab (Chat / Logs / Details)

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use egui::{Color32, RichText, Ui};
use serde_json::Value;
use suzerain_client::{Agent, Client, Daemon, EndpointInfo, Overview, PromptMode};

pub mod chat;
pub mod config;
pub mod create;
pub mod net;
pub mod terminal;
pub mod views;

use chat::Chat;
use config::{Config, WorkspaceCfg};
use create::CreateForm;
use net::{NetMsg, WsId};
use views::{
    ActivityState, CastellanIntent, DetailsIntent, DetailsState, LogsState, SecretsIntent,
    SecretsState,
};

pub fn run() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "suzy=info".into()),
        )
        .init();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 820.0])
            .with_title("Suzy"),
        ..Default::default()
    };
    eframe::run_native(
        "Suzy",
        options,
        Box::new(move |cc| Ok(Box::new(SuzyApp::new(cc, rt)))),
    )
}

// ── state ────────────────────────────────────────────────────────────────

pub struct Workspace {
    pub cfg: WorkspaceCfg,
    pub client: Arc<Client>,
    pub endpoint: Option<EndpointInfo>,
    pub error: Option<String>,
    overview: Option<Overview>,
    agents: Vec<Agent>,
    daemons: Vec<Daemon>,
    pending: Vec<Value>,
    providers: Option<Value>,
    harnesses: Option<Value>,
    /// Keep-alive: aborting this stops the workspace's refresh loop
    /// (used when removing a workspace — removal UI is a later milestone).
    #[allow(dead_code)]
    loop_handle: Option<tokio::task::AbortHandle>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AgentTab {
    Chat,
    Shell,
    Logs,
    Details,
}

#[derive(Clone)]
pub enum View {
    Dashboard,
    Castellans,
    Activity,
    Secrets,
    Agent {
        ws: WsId,
        agent: String,
        tab: AgentTab,
    },
}

/// Live pty shell state for one agent (M4).
pub struct ShellState {
    pub term: terminal::Terminal,
    pub input: tokio::sync::mpsc::UnboundedSender<terminal::TermInput>,
    pub exited: Option<i64>,
    pub error: Option<String>,
}

pub struct SuzyApp {
    /// Keep-alive: background network tasks run on this runtime.
    #[allow(dead_code)]
    pub rt: tokio::runtime::Runtime,
    pub stored_ctx: Option<egui::Context>,
    /// Suzy's iroh identity (persisted at ~/.config/suzy/iroh.key); its
    /// public half is what operators add to `[operator] allow`.
    pub iroh_key: suzerain_client::iroh::SecretKey,
    pub cfg: Config,
    /// Where config.toml is persisted (injected for tests).
    pub config_path: std::path::PathBuf,
    pub workspaces: Vec<Workspace>,
    pub active_ws: Option<WsId>,
    pub view: View,
    pub chats: HashMap<(WsId, String), Chat>,
    pub logs: HashMap<(WsId, String), LogsState>,
    pub details: HashMap<(WsId, String), DetailsState>,
    pub activity: HashMap<WsId, ActivityState>,
    pub secrets: HashMap<WsId, SecretsState>,
    pub session_handles: HashMap<(WsId, String), tokio::task::AbortHandle>,
    pub shells: HashMap<(WsId, String), ShellState>,
    pub shell_handles: HashMap<(WsId, String), tokio::task::AbortHandle>,
    pub tx: Sender<NetMsg>,
    pub rx: Receiver<NetMsg>,
    // dialog state
    pub add_ws_open: bool,
    pub add_ws_name: String,
    pub add_ws_eid: String,
    pub create_open: bool,
    pub create_form: CreateForm,
    pub destroy_confirm: Option<(WsId, String)>,
    pub remove_ws_confirm: Option<WsId>,
    pub labels_editing: Option<(WsId, String)>, // daemon endpoint id
    pub labels_draft: String,
    pub status_msg: Option<String>,
}

impl SuzyApp {
    fn new(cc: &eframe::CreationContext, rt: tokio::runtime::Runtime) -> Self {
        Self::with_config(cc, rt, config::load(), config::config_path())
    }

    /// Test-friendly constructor: explicit config and persistence path.
    pub fn with_config(
        cc: &eframe::CreationContext,
        rt: tokio::runtime::Runtime,
        cfg: Config,
        config_path: std::path::PathBuf,
    ) -> Self {
        // Multiple rustls providers can be linked; pick ring explicitly
        // (iroh/QUIC needs a default CryptoProvider).
        let _ = rustls::crypto::CryptoProvider::install_default(
            rustls::crypto::ring::default_provider(),
        );
        cc.egui_ctx.set_visuals(if cfg.theme == "light" {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        });
        let (tx, rx) = channel();
        let iroh_key = config::load_or_create_key().unwrap_or_else(|e| {
            tracing::warn!("iroh key load failed ({e:#}); using an ephemeral identity");
            suzerain_client::iroh::SecretKey::generate()
        });
        let mut app = Self {
            rt,
            stored_ctx: Some(cc.egui_ctx.clone()),
            iroh_key,
            cfg,
            config_path,
            workspaces: Vec::new(),
            active_ws: None,
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
            add_ws_open: false,
            add_ws_name: String::new(),
            add_ws_eid: String::new(),
            create_open: false,
            create_form: CreateForm::default(),
            destroy_confirm: None,
            remove_ws_confirm: None,
            labels_editing: None,
            labels_draft: String::new(),
            status_msg: None,
        };
        for i in 0..app.cfg.workspaces.len() {
            let cfg = app.cfg.workspaces[i].clone();
            app.connect_workspace(cfg);
        }
        if !app.workspaces.is_empty() {
            app.active_ws = Some(0);
        }
        app
    }

    pub fn connect_workspace(&mut self, cfg: WorkspaceCfg) {
        let client = match &cfg.test_addr {
            Some(addr) => Arc::new(Client::with_addr(addr.clone(), self.iroh_key.clone())),
            None => Arc::new(Client::new(&cfg.endpoint_id, self.iroh_key.clone())),
        };
        let ws_id = self.workspaces.len();
        let handle = net::spawn_workspace_loop(
            self.rt.handle().clone(),
            ws_id,
            client.clone(),
            self.tx.clone(),
            self.ctx_handle(),
        );
        self.workspaces.push(Workspace {
            cfg,
            client,
            endpoint: None,
            error: None,
            overview: None,
            agents: Vec::new(),
            daemons: Vec::new(),
            pending: Vec::new(),
            providers: None,
            harnesses: None,
            loop_handle: Some(handle),
        });
    }

    /// egui context for background tasks to request repaints.
    fn ctx_handle(&self) -> egui::Context {
        self.stored_ctx.clone().expect("ctx set in update")
    }

    pub fn open_agent(&mut self, ws: WsId, agent: String, tab: AgentTab) {
        self.view = View::Agent {
            ws,
            agent: agent.clone(),
            tab,
        };
        match tab {
            AgentTab::Chat => self.ensure_chat_stream(ws, agent),
            AgentTab::Shell => self.ensure_shell(ws, agent),
            AgentTab::Logs => self.fetch_logs(ws, &agent),
            AgentTab::Details => self.fetch_details(ws, &agent),
        }
    }

    pub fn ensure_shell(&mut self, ws: WsId, agent: String) {
        let key = (ws, agent.clone());
        if self.shell_handles.contains_key(&key) {
            return;
        }
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = ShellState {
            term: terminal::Terminal::default(),
            input: input_tx,
            exited: None,
            error: None,
        };
        state
            .term
            .write_system("connecting — the agent wakes automatically if sleeping…");
        self.shells.insert(key.clone(), state);
        let handle = net::spawn_shell(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            agent,
            input_rx,
            self.tx.clone(),
            self.ctx_handle(),
        );
        self.shell_handles.insert(key, handle);
    }

    fn ensure_chat_stream(&mut self, ws: WsId, agent: String) {
        let key = (ws, agent.clone());
        if !self.session_handles.contains_key(&key) {
            self.chats
                .entry(key.clone())
                .or_insert_with(|| Chat::new(agent.clone()));
            let handle = net::spawn_session_stream(
                self.rt.handle().clone(),
                ws,
                self.workspaces[ws].client.clone(),
                agent,
                self.tx.clone(),
                self.ctx_handle(),
            );
            self.session_handles.insert(key, handle);
        }
    }

    fn fetch_logs(&mut self, ws: WsId, agent: &str) {
        let key = (ws, agent.to_string());
        let state = self.logs.entry(key).or_default();
        let (kind, q, tail) = (
            if state.kind.is_empty() {
                None
            } else {
                Some(state.kind.clone())
            },
            if state.q.is_empty() {
                None
            } else {
                Some(state.q.clone())
            },
            state.tail_or_default(),
        );
        net::spawn_logs(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            agent.to_string(),
            kind,
            q,
            tail,
            self.tx.clone(),
            self.ctx_handle(),
        );
    }

    fn fetch_details(&mut self, ws: WsId, agent: &str) {
        net::spawn_details(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            agent.to_string(),
            self.tx.clone(),
            self.ctx_handle(),
        );
    }

    fn fetch_activity(&mut self, ws: WsId) {
        let tail = match self.activity.get(&ws).map(|s| s.tail) {
            Some(0) | None => 300,
            Some(t) => t,
        };
        net::spawn_activity(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            tail,
            self.tx.clone(),
            self.ctx_handle(),
        );
    }

    fn fetch_secrets(&mut self, ws: WsId) {
        net::spawn_secrets(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            self.tx.clone(),
            self.ctx_handle(),
        );
    }

    /// Remove a workspace: tear down every connection and reconnect the
    /// rest (WsIds are positional, so all per-ws state is reset).
    pub fn remove_workspace(&mut self, ws: WsId) {
        if ws >= self.workspaces.len() {
            return;
        }
        self.cfg.workspaces.remove(ws);
        let _ = config::save_to(&self.config_path, &self.cfg);
        for w in self.workspaces.drain(..) {
            if let Some(h) = w.loop_handle {
                h.abort();
            }
        }
        for (_, h) in self.session_handles.drain() {
            h.abort();
        }
        for (_, h) in self.shell_handles.drain() {
            h.abort();
        }
        self.chats.clear();
        self.logs.clear();
        self.details.clear();
        self.shells.clear();
        self.activity.clear();
        self.secrets.clear();
        for i in 0..self.cfg.workspaces.len() {
            let cfg = self.cfg.workspaces[i].clone();
            self.connect_workspace(cfg);
        }
        self.active_ws = if self.workspaces.is_empty() {
            None
        } else {
            Some(0)
        };
        self.view = View::Dashboard;
    }

    /// Desktop notification (M3/G7): only when the window is unfocused — a
    /// focused operator already sees the sidebar flip. Runs on a thread:
    /// the OS notification call can block briefly.
    fn notify(&self, summary: &str, body: String) {
        let focused = self
            .stored_ctx
            .as_ref()
            .map(|c| c.input(|i| i.focused))
            .unwrap_or(true);
        if focused {
            return;
        }
        let summary = summary.to_string();
        std::thread::spawn(move || {
            if let Err(e) = notify_rust::Notification::new()
                .summary(&summary)
                .body(&body)
                .appname("Suzy")
                .show()
            {
                tracing::debug!("notification failed: {e}");
            }
        });
    }

    /// Diff the incoming agent list against the previous snapshot and fire
    /// notifications for the two transitions worth interrupting for
    /// (G7): an agent entering `failed`, and a wake completing.
    fn notify_transitions(&self, ws: WsId, agents: &[Agent]) {
        let Some(w) = self.workspaces.get(ws) else {
            return;
        };
        if w.agents.is_empty() {
            return; // first snapshot: baseline, no notifications
        }
        let old: HashMap<&str, (&str, bool)> = w
            .agents
            .iter()
            .map(|a| (a.name.as_str(), (a.status.as_str(), a.needs_attention)))
            .collect();
        for a in agents {
            let Some((old_status, old_attention)) = old.get(a.name.as_str()) else {
                continue;
            };
            if a.status == "failed" && *old_status != "failed" {
                self.notify(
                    "⚠ suzerain agent failed",
                    format!(
                        "{}: '{}' is now failed — check its logs",
                        w.cfg.name, a.name
                    ),
                );
            }
            if *old_status == "waking" && matches!(a.status.as_str(), "idle" | "running") {
                self.notify(
                    "suzerain agent awake",
                    format!("{}: '{}' finished waking", w.cfg.name, a.name),
                );
            }
            if a.needs_attention && !old_attention {
                self.notify(
                    "⚠ suzerain agent needs attention",
                    format!("{}: '{}' needs human intervention", w.cfg.name, a.name),
                );
            }
        }
    }

    fn dispatch_secret_intent(&mut self, ws: WsId, intent: SecretsIntent) {
        let client = self.workspaces[ws].client.clone();
        match intent {
            SecretsIntent::Refetch => self.fetch_secrets(ws),
            SecretsIntent::Reveal(kind, name) => {
                net::spawn_reveal(
                    self.rt.handle().clone(),
                    ws,
                    client,
                    kind,
                    name,
                    self.tx.clone(),
                    self.ctx_handle(),
                );
            }
            SecretsIntent::SetProvider(id, value) => {
                self.action_then_refresh_secrets(
                    ws,
                    "set provider",
                    net::Action::SetSecretProvider { id, value },
                );
            }
            SecretsIntent::DeleteProvider(id) => {
                self.action_then_refresh_secrets(
                    ws,
                    "delete provider",
                    net::Action::DeleteSecretProvider(id),
                );
            }
            SecretsIntent::SetExtra(name, value) => {
                self.action_then_refresh_secrets(
                    ws,
                    "set secret",
                    net::Action::SetSecretExtra { name, value },
                );
            }
            SecretsIntent::DeleteExtra(name) => {
                self.action_then_refresh_secrets(
                    ws,
                    "delete secret",
                    net::Action::DeleteSecretExtra(name),
                );
            }
            SecretsIntent::SetDeployKey(value) => {
                self.action_then_refresh_secrets(
                    ws,
                    "upload ssh key",
                    net::Action::SetDeployKey(value),
                );
            }
            SecretsIntent::DeleteDeployKey => {
                self.action_then_refresh_secrets(
                    ws,
                    "delete ssh key",
                    net::Action::DeleteDeployKey,
                );
            }
        }
        let _ = client;
    }

    fn action_then_refresh_secrets(&mut self, ws: WsId, what: &'static str, action: net::Action) {
        net::spawn_action(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            what,
            action,
            self.tx.clone(),
            self.ctx_handle(),
        );
        // The inventory changes behind the action; refetch immediately
        // (stale for a beat is fine, the fleet event loop also refreshes).
        self.fetch_secrets(ws);
    }

    fn send_prompt(&mut self, ws: WsId, agent: &str, mode: PromptMode) {
        let key = (ws, agent.to_string());
        let Some(chat) = self.chats.get_mut(&key) else {
            return;
        };
        let message = chat.input.trim().to_string();
        if message.is_empty() && mode != PromptMode::Abort {
            return;
        }
        if mode == PromptMode::Prompt {
            chat.items.push(chat::ChatItem::User(message.clone()));
            chat.input.clear();
            chat.streaming = true;
        }
        net::spawn_prompt(
            self.rt.handle().clone(),
            ws,
            self.workspaces[ws].client.clone(),
            agent.to_string(),
            message,
            mode,
            self.tx.clone(),
            self.ctx_handle(),
        );
    }

    pub fn drain_net(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle_net(msg);
        }
    }

    pub fn handle_net(&mut self, msg: NetMsg) {
        match msg {
            NetMsg::Connected { ws, info } => {
                if let Some(w) = self.workspaces.get_mut(ws) {
                    // Sanity: the server we reached should be the id we dialed
                    // (iroh guarantees this; a mismatch means a bug/proxy).
                    if w.cfg.endpoint_id != info.endpoint_id
                        && !w.cfg.endpoint_id.is_empty()
                        && w.cfg.test_addr.is_none()
                    {
                        tracing::warn!(
                            expected = %w.cfg.endpoint_id,
                            got = %info.endpoint_id,
                            "server-reported endpoint id differs from dialed id"
                        );
                    }
                    w.endpoint = Some(info);
                    w.error = None;
                }
            }
            NetMsg::ConnectFailed { ws, error } => {
                if let Some(w) = self.workspaces.get_mut(ws) {
                    w.error = Some(error);
                }
            }
            NetMsg::Catalogs {
                ws,
                providers,
                harnesses,
            } => {
                if let Some(w) = self.workspaces.get_mut(ws) {
                    w.providers = Some(providers);
                    w.harnesses = Some(harnesses);
                }
            }
            NetMsg::Snapshot {
                ws,
                overview,
                agents,
                daemons,
                pending,
            } => {
                self.notify_transitions(ws, &agents);
                if let Some(w) = self.workspaces.get_mut(ws) {
                    w.overview = Some(*overview);
                    w.agents = agents;
                    w.daemons = daemons;
                    w.pending = pending;
                    w.error = None;
                }
                // Keep open logs views fresh as fleet events arrive.
                if let View::Agent {
                    ws: vw,
                    agent,
                    tab: AgentTab::Logs,
                } = &self.view
                {
                    if *vw == ws {
                        let agent = agent.clone();
                        self.fetch_logs(ws, &agent);
                    }
                }
                // Same for the activity feed.
                if matches!(self.view, View::Activity) && self.active_ws == Some(ws) {
                    self.fetch_activity(ws);
                }
            }
            NetMsg::SessionHistory { ws, agent, item } => {
                if let Some(c) = self.chats.get_mut(&(ws, agent)) {
                    c.push_history(&item);
                }
            }
            NetMsg::SessionHistoryEnd { ws, agent } => {
                if let Some(c) = self.chats.get_mut(&(ws, agent)) {
                    c.history_done = true;
                }
            }
            NetMsg::SessionLive { ws, agent, event } => {
                if let Some(c) = self.chats.get_mut(&(ws, agent)) {
                    c.push_live(&event);
                }
            }
            NetMsg::SessionClosed { ws, agent, error } => {
                let key = (ws, agent.clone());
                self.session_handles.remove(&key);
                if let Some(c) = self.chats.get_mut(&key) {
                    c.closed = Some(error);
                    c.streaming = false;
                }
            }
            NetMsg::PromptDone { ws, agent, result } => {
                if let Err(e) = result {
                    if let Some(c) = self.chats.get_mut(&(ws, agent)) {
                        c.items
                            .push(chat::ChatItem::Error(format!("send failed: {e}")));
                        c.streaming = false;
                    }
                }
            }
            NetMsg::ShellData { ws, agent, bytes } => {
                if let Some(s) = self.shells.get_mut(&(ws, agent)) {
                    s.term.feed(&bytes);
                }
            }
            NetMsg::ShellNotice { ws, agent, message } => {
                if let Some(s) = self.shells.get_mut(&(ws, agent)) {
                    s.term.write_system(&message);
                }
            }
            NetMsg::ShellClosed {
                ws,
                agent,
                exit,
                error,
            } => {
                let key = (ws, agent);
                self.shell_handles.remove(&key);
                if let Some(s) = self.shells.get_mut(&key) {
                    s.exited = exit;
                    s.error = error.clone();
                    match (exit, error) {
                        (Some(code), _) => {
                            s.term.write_system(&format!("[shell exited, code {code}]"))
                        }
                        (_, Some(e)) => s.term.write_system(&format!("[connection closed: {e}]")),
                        _ => s.term.write_system("[connection closed]"),
                    }
                }
            }
            NetMsg::CreateDone { result, .. } => match result {
                Ok(v) => {
                    self.status_msg = Some(format!(
                        "agent '{}' provisioning",
                        v["name"].as_str().unwrap_or("?")
                    ));
                    self.create_open = false;
                    self.create_form = CreateForm::default();
                }
                Err(e) => self.status_msg = Some(format!("create failed: {e}")),
            },
            NetMsg::DestroyDone { result, .. } => match result {
                Ok(()) => self.status_msg = Some("agent destroyed".into()),
                Err(e) => self.status_msg = Some(format!("destroy failed: {e}")),
            },
            NetMsg::Details { ws, agent, result } => {
                let state = self.details.entry((ws, agent)).or_default();
                state.loaded = true;
                match result {
                    Ok(v) => {
                        state.error = None;
                        state.value = Some(v);
                    }
                    Err(e) => state.error = Some(e),
                }
            }
            NetMsg::Logs { ws, agent, result } => {
                let state = self.logs.entry((ws, agent)).or_default();
                state.loaded = true;
                match result {
                    Ok(v) => {
                        state.error = None;
                        state.total = v["total_matching"].as_u64().unwrap_or(0) as usize;
                        state.events = v["events"].as_array().cloned().unwrap_or_default();
                    }
                    Err(e) => state.error = Some(e),
                }
            }
            NetMsg::Activity { ws, result } => {
                let state = self.activity.entry(ws).or_default();
                state.loaded = true;
                match result {
                    Ok(entries) => {
                        state.error = None;
                        state.entries = entries;
                    }
                    Err(e) => state.error = Some(e),
                }
            }
            NetMsg::Secrets { ws, result } => {
                let state = self.secrets.entry(ws).or_default();
                state.loaded = true;
                match result {
                    Ok(v) => {
                        state.error = None;
                        state.value = Some(v);
                    }
                    Err(e) => state.error = Some(e),
                }
            }
            NetMsg::RevealDone { ws, result } => {
                let state = self.secrets.entry(ws).or_default();
                match result {
                    Ok(v) => {
                        state.revealed = Some((
                            String::new(),
                            String::new(),
                            v["value"].as_str().unwrap_or("").to_string(),
                        ));
                    }
                    Err(e) => state.error = Some(e),
                }
            }
            NetMsg::ActionDone { what, result, .. } => match result {
                Ok(()) => self.status_msg = Some(format!("{what}: ok")),
                Err(e) => self.status_msg = Some(format!("{what} failed: {e}")),
            },
        }
    }

    // ── ui ────────────────────────────────────────────────────────────

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("👑 Suzy").strong().size(16.0));
                ui.separator();
                for (i, w) in self.workspaces.iter().enumerate() {
                    let selected = self.active_ws == Some(i);
                    let label = if w.error.is_some() {
                        RichText::new(format!("⚠ {}", w.cfg.name)).color(Color32::LIGHT_RED)
                    } else {
                        RichText::new(&w.cfg.name)
                    };
                    if ui.selectable_label(selected, label).clicked() {
                        self.active_ws = Some(i);
                        self.view = View::Dashboard;
                    }
                }
                if ui.button("＋ workspace").clicked() {
                    self.add_ws_open = true;
                }
                ui.separator();
                if let Some(ws) = self.active_ws.and_then(|i| self.workspaces.get(i)) {
                    match &ws.endpoint {
                        Some(info) => {
                            ui.label(
                                RichText::new(format!(
                                    "● {} v{}",
                                    &info.endpoint_id[..info.endpoint_id.len().min(8)],
                                    info.version
                                ))
                                .color(Color32::from_rgb(0x5C, 0xC8, 0x7A))
                                .size(12.0),
                            );
                        }
                        None => {
                            ui.label(RichText::new("○ connecting…").color(Color32::GRAY));
                        }
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // theme toggle (persisted; cfg is the source of truth)
                    let dark = self.cfg.theme != "light";
                    if ui
                        .button(if dark { "🌙" } else { "☀" })
                        .on_hover_text("toggle light/dark")
                        .clicked()
                    {
                        let now_dark = !dark;
                        ui.ctx().set_visuals(if now_dark {
                            egui::Visuals::dark()
                        } else {
                            egui::Visuals::light()
                        });
                        self.cfg.theme = if now_dark {
                            "dark".into()
                        } else {
                            "light".into()
                        };
                        let _ = config::save_to(&self.config_path, &self.cfg);
                    }
                    if self.active_ws.is_some()
                        && ui
                            .button("➖ ws")
                            .on_hover_text("remove this workspace")
                            .clicked()
                    {
                        self.remove_ws_confirm = self.active_ws;
                    }
                    if let Some(msg) = self.status_msg.clone() {
                        if ui.button("✖").clicked() {
                            self.status_msg = None;
                        }
                        ui.label(RichText::new(msg).size(12.0).color(Color32::KHAKI));
                    }
                });
            });
        });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sidebar")
            .default_width(230.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                let Some(ws_id) = self.active_ws else {
                    ui.label("Add a workspace to connect to a suzerain.");
                    return;
                };
                if ui
                    .selectable_label(matches!(self.view, View::Dashboard), "📊 Dashboard")
                    .clicked()
                {
                    self.view = View::Dashboard;
                }
                if ui
                    .selectable_label(matches!(self.view, View::Castellans), "🖥 Castellans")
                    .clicked()
                {
                    self.view = View::Castellans;
                }
                if ui
                    .selectable_label(matches!(self.view, View::Activity), "≣ Activity")
                    .clicked()
                {
                    self.view = View::Activity;
                    self.fetch_activity(ws_id);
                }
                if ui
                    .selectable_label(matches!(self.view, View::Secrets), "🔑 Secrets")
                    .clicked()
                {
                    self.view = View::Secrets;
                    self.fetch_secrets(ws_id);
                }
                ui.add_space(6.0);
                ui.separator();

                let Some(ws) = self.workspaces.get(ws_id) else {
                    return;
                };
                if let Some(err) = ws.error.clone() {
                    ui.label(RichText::new(err).color(Color32::LIGHT_RED).size(12.0));
                    return;
                }

                // Group agents by daemon (herdr workspace→tab→pane mapped to
                // workspace→daemon→agent). Owned snapshot so the render
                // closure can call back into &mut self.
                struct AgentEntry {
                    name: String,
                    status: String,
                    hover: String,
                    needs_attention: bool,
                }
                let mut by_daemon: Vec<(String, Vec<AgentEntry>)> = Vec::new();
                let mut agent_count = 0;
                for agent in &ws.agents {
                    agent_count += 1;
                    let title = match &agent.daemon_hostname {
                        Some(h) if !h.is_empty() => format!("🖥 {h}"),
                        _ => format!(
                            "🖥 {}…",
                            &agent.daemon_endpoint_id[..agent.daemon_endpoint_id.len().min(8)]
                        ),
                    };
                    let entry = AgentEntry {
                        name: agent.name.clone(),
                        status: agent.status.clone(),
                        needs_attention: agent.needs_attention,
                        hover: format!(
                            "{} • {}:{}\n{}",
                            agent.status,
                            agent.manifest.model.provider,
                            agent.manifest.model.id,
                            agent.daemon_hostname.clone().unwrap_or_default()
                        ),
                    };
                    match by_daemon.iter_mut().find(|(t, _)| *t == title) {
                        Some((_, list)) => list.push(entry),
                        None => by_daemon.push((title, vec![entry])),
                    }
                }

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (title, agents) in by_daemon {
                        egui::CollapsingHeader::new(title)
                            .default_open(true)
                            .show(ui, |ui| {
                                for agent in agents {
                                    let selected = matches!(
                                        &self.view,
                                        View::Agent { ws: w, agent: a, .. }
                                            if *w == ws_id && *a == agent.name
                                    );
                                    let resp = ui.selectable_label(
                                        selected,
                                        RichText::new(format!("● {}", agent.name))
                                            .color(status_color(&agent.status)),
                                    );
                                    resp.clone().on_hover_text(agent.hover);
                                    if resp.clicked() {
                                        self.open_agent(ws_id, agent.name.clone(), AgentTab::Chat);
                                    }
                                    if agent.needs_attention {
                                        ui.label(
                                            RichText::new("  ⚠ needs attention")
                                                .color(Color32::LIGHT_RED)
                                                .size(11.0),
                                        );
                                    }
                                }
                            });
                    }
                    if agent_count == 0 {
                        ui.label(
                            RichText::new("no agents yet")
                                .italics()
                                .color(Color32::GRAY),
                        );
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    if ui.button("✚ Create agent").clicked() {
                        self.create_open = true;
                    }
                });
            });
    }

    fn dashboard(&self, ui: &mut Ui) {
        let Some(ws_id) = self.active_ws else {
            welcome(ui);
            return;
        };
        let Some(ws) = self.workspaces.get(ws_id) else {
            return;
        };
        ui.heading("Fleet");
        ui.add_space(8.0);
        if let Some(ov) = &ws.overview {
            ui.horizontal(|ui| {
                stat_card(
                    ui,
                    "daemons online",
                    &format!("{}/{}", ov.daemons_online, ov.daemons_total),
                );
                stat_card(ui, "agents", &ov.agents_total.to_string());
                for (state, n) in &ov.agents_by_state {
                    stat_card(ui, state, &n.to_string());
                }
            });
        }
        if !ws.pending.is_empty() {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "⚠ {} daemon(s) awaiting approval — see Castellans",
                    ws.pending.len()
                ))
                .color(Color32::KHAKI),
            );
        }
        ui.add_space(12.0);
        ui.heading("Castellans");
        egui::Grid::new("daemons_grid")
            .striped(true)
            .show(ui, |ui| {
                ui.label(RichText::new("host").strong());
                ui.label(RichText::new("endpoint").strong());
                ui.label(RichText::new("status").strong());
                ui.label(RichText::new("os/arch").strong());
                ui.label(RichText::new("cpu").strong());
                ui.label(RichText::new("memory free").strong());
                ui.label(RichText::new("agents").strong());
                ui.end_row();
                for d in &ws.daemons {
                    ui.label(&d.hostname);
                    ui.label(format!("{}…", d.short_id()));
                    let (txt, color) = if d.online && d.approved {
                        ("online", Color32::from_rgb(0x5C, 0xC8, 0x7A))
                    } else if !d.approved {
                        ("unapproved", Color32::KHAKI)
                    } else {
                        ("offline", Color32::GRAY)
                    };
                    ui.label(RichText::new(txt).color(color));
                    ui.label(format!("{}/{}", d.os, d.arch));
                    ui.label(format!("{} vcpu", d.capacity.vcpu_total));
                    ui.label(format!("{} MiB", d.usage.memory_mib_free));
                    let n = ws
                        .agents
                        .iter()
                        .filter(|a| a.daemon_endpoint_id == d.endpoint_id)
                        .count();
                    ui.label(format!("{n}/{}", d.max_agents));
                    ui.end_row();
                }
            });
    }

    fn castellans_view(&mut self, ui: &mut Ui, ws: WsId) {
        let Some(w) = self.workspaces.get(ws) else {
            return;
        };
        let daemons = w.daemons.clone();
        let pending = w.pending.clone();
        let endpoint = w.endpoint.clone();
        let intents = views::castellans_view(ui, &daemons, &pending, endpoint.as_ref());
        for intent in intents {
            match intent {
                CastellanIntent::ApprovePending(id) => net::spawn_action(
                    self.rt.handle().clone(),
                    ws,
                    self.workspaces[ws].client.clone(),
                    "approve daemon",
                    net::Action::ApprovePending(id),
                    self.tx.clone(),
                    self.ctx_handle(),
                ),
                CastellanIntent::DismissPending(id) => net::spawn_action(
                    self.rt.handle().clone(),
                    ws,
                    self.workspaces[ws].client.clone(),
                    "dismiss enrollment",
                    net::Action::DismissPending(id),
                    self.tx.clone(),
                    self.ctx_handle(),
                ),
                CastellanIntent::RemoveDaemon(id) => net::spawn_action(
                    self.rt.handle().clone(),
                    ws,
                    self.workspaces[ws].client.clone(),
                    "remove daemon",
                    net::Action::RemoveDaemon(id),
                    self.tx.clone(),
                    self.ctx_handle(),
                ),
                CastellanIntent::EditLabels(id) => {
                    self.labels_editing = Some((ws, id));
                }
            }
        }
    }

    fn agent_view(&mut self, ui: &mut Ui, ws: WsId, agent: &str, tab: AgentTab) {
        // tab bar
        ui.horizontal(|ui| {
            ui.heading(agent);
            let agent_status = self
                .workspaces
                .get(ws)
                .and_then(|w| w.agents.iter().find(|a| a.name == agent))
                .map(|a| a.status.clone())
                .unwrap_or_else(|| "?".into());
            ui.label(RichText::new(format!("● {agent_status}")).color(status_color(&agent_status)));
            ui.separator();
            for (t, label) in [
                (AgentTab::Chat, "💬 Chat"),
                (AgentTab::Shell, "⌨ Shell"),
                (AgentTab::Logs, "🧾 Logs"),
                (AgentTab::Details, "⚙ Details"),
            ] {
                if ui.selectable_label(tab == t, label).clicked() && tab != t {
                    self.open_agent(ws, agent.to_string(), t);
                    return;
                }
            }
        });
        ui.separator();
        match tab {
            AgentTab::Chat => self.chat_tab(ui, ws, agent),
            AgentTab::Shell => self.shell_tab(ui, ws, agent),
            AgentTab::Logs => {
                let key = (ws, agent.to_string());
                if !self.logs.contains_key(&key) {
                    self.fetch_logs(ws, agent);
                }
                let state = self.logs.entry(key).or_default();
                if views::logs_view(ui, state) {
                    self.fetch_logs(ws, agent);
                }
            }
            AgentTab::Details => {
                let key = (ws, agent.to_string());
                if !self.details.contains_key(&key) {
                    self.fetch_details(ws, agent);
                }
                let state = self.details.entry(key).or_default();
                let intents = views::details_view(ui, agent, state);
                for intent in intents {
                    match intent {
                        DetailsIntent::SetAutoSuspend(value) => net::spawn_action(
                            self.rt.handle().clone(),
                            ws,
                            self.workspaces[ws].client.clone(),
                            "auto-suspend",
                            net::Action::SetAutoSuspend {
                                agent: agent.to_string(),
                                value,
                            },
                            self.tx.clone(),
                            self.ctx_handle(),
                        ),
                        DetailsIntent::Destroy => {
                            self.destroy_confirm = Some((ws, agent.to_string()));
                        }
                        DetailsIntent::Refetch => self.fetch_details(ws, agent),
                    }
                }
            }
        }
    }

    fn shell_tab(&mut self, ui: &mut Ui, ws: WsId, agent: &str) {
        let key = (ws, agent.to_string());
        if !self.shells.contains_key(&key) {
            self.ensure_shell(ws, agent.to_string());
        }
        let closed = !self.shell_handles.contains_key(&key);
        if closed {
            ui.horizontal(|ui| {
                let msg = self
                    .shells
                    .get(&key)
                    .map(|s| match (s.exited, &s.error) {
                        (Some(code), _) => format!("shell exited (code {code})"),
                        (_, Some(e)) => format!("connection closed: {e}"),
                        _ => "connection closed".to_string(),
                    })
                    .unwrap_or_default();
                ui.label(RichText::new(msg).color(Color32::LIGHT_RED).size(12.0));
                if ui.button("reconnect").clicked() {
                    self.shells.remove(&key);
                    self.ensure_shell(ws, agent.to_string());
                }
            });
        }
        let Some(state) = self.shells.get_mut(&key) else {
            return;
        };
        let inputs = state.term.render(ui);
        for input in inputs {
            let _ = state.input.send(input);
        }
    }

    fn chat_tab(&mut self, ui: &mut Ui, ws: WsId, agent: &str) {
        let key = (ws, agent.to_string());
        let (streaming, status_line, closed) = {
            let Some(chat) = self.chats.get(&key) else {
                return;
            };
            (
                chat.streaming,
                chat.status_line.clone(),
                chat.closed.clone(),
            )
        };

        if streaming {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    RichText::new("turn in flight")
                        .size(11.5)
                        .color(Color32::GRAY),
                );
            });
        }

        let height_left = ui.available_height() - 120.0;
        egui::ScrollArea::vertical()
            .id_salt("chat_scroll")
            .max_height(height_left.max(120.0))
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if let Some(chat) = self.chats.get(&key) {
                    if chat.items.is_empty() && !chat.history_done {
                        ui.label(
                            RichText::new("loading history…")
                                .italics()
                                .color(Color32::GRAY),
                        );
                    } else {
                        chat::render_items(ui, &chat.items);
                    }
                }
            });

        ui.separator();
        if !status_line.is_empty() {
            ui.label(RichText::new(&status_line).size(11.5).color(Color32::KHAKI));
        }
        if let Some(err) = &closed {
            ui.horizontal(|ui| {
                let msg = match err {
                    Some(e) => format!("session stream closed: {e}"),
                    None => "session stream closed".to_string(),
                };
                ui.label(RichText::new(msg).color(Color32::LIGHT_RED).size(12.0));
                if ui.button("reconnect").clicked() {
                    let key = (ws, agent.to_string());
                    if !self.session_handles.contains_key(&key) {
                        if let Some(c) = self.chats.get_mut(&key) {
                            c.closed = None;
                        }
                        let handle = net::spawn_session_stream(
                            self.rt.handle().clone(),
                            ws,
                            self.workspaces[ws].client.clone(),
                            agent.to_string(),
                            self.tx.clone(),
                            self.ctx_handle(),
                        );
                        self.session_handles.insert(key, handle);
                    }
                }
            });
        }

        let mut send = false;
        let mut steer = false;
        let mut abort = false;
        ui.horizontal(|ui| {
            let Some(chat) = self.chats.get_mut(&key) else {
                return;
            };
            let resp = egui::TextEdit::multiline(&mut chat.input)
                .hint_text("message the agent… (Enter to send, Shift+Enter for newline)")
                .desired_width(ui.available_width() - 170.0)
                .desired_rows(2)
                .show(ui)
                .response;
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
            if resp.has_focus() && enter {
                send = true;
            }
            ui.vertical(|ui| {
                if ui.button("Send").clicked() {
                    send = true;
                }
                ui.horizontal(|ui| {
                    if ui
                        .button("Steer")
                        .on_hover_text("inject mid-turn")
                        .clicked()
                    {
                        steer = true;
                    }
                    if ui.button("Abort").clicked() {
                        abort = true;
                    }
                });
            });
        });
        if send {
            self.send_prompt(ws, agent, PromptMode::Prompt);
        } else if steer {
            self.send_prompt(ws, agent, PromptMode::Steer);
        } else if abort {
            self.send_prompt(ws, agent, PromptMode::Abort);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.add_ws_open {
            let mut open = true;
            egui::Window::new("Add workspace")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label("A workspace is a connection to one suzerain control plane,");
                    ui.label("over iroh — reachable anywhere by its EndpointId.");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("name:       ");
                        ui.text_edit_singleline(&mut self.add_ws_name);
                    });
                    ui.horizontal(|ui| {
                        ui.label("endpoint id:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.add_ws_eid)
                                .hint_text("the suzerain's iroh EndpointId")
                                .desired_width(360.0),
                        );
                    });
                    ui.add_space(6.0);
                    ui.label(RichText::new("first connection must be authorized on the control plane:").size(11.5));
                    egui::Frame::new()
                        .fill(Color32::from_rgb(0x18, 0x1C, 0x22))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(format!(
                                    "# in $SUZERAIN_HOME/suzerain.toml:\n[operator]\nallow = [\"{}\"]",
                                    self.iroh_key.public()
                                ))
                                .monospace()
                                .size(11.0),
                            );
                        });
                    if ui
                        .button("📋 copy my operator id")
                        .on_hover_text(self.iroh_key.public().to_string())
                        .clicked()
                    {
                        ui.ctx().copy_text(self.iroh_key.public().to_string());
                    }
                    ui.add_space(4.0);
                    let valid = !self.add_ws_eid.trim().is_empty();
                    if ui
                        .add_enabled(valid, egui::Button::new("Connect"))
                        .clicked()
                    {
                        let eid = self.add_ws_eid.trim().to_string();
                        let name = if self.add_ws_name.trim().is_empty() {
                            format!("{}…", &eid[..eid.len().min(8)])
                        } else {
                            self.add_ws_name.trim().to_string()
                        };
                        let cfg = WorkspaceCfg {
                            name,
                            endpoint_id: eid,
                            test_addr: None,
                        };
                        self.cfg.workspaces.push(cfg.clone());
                        let _ = config::save_to(&self.config_path, &self.cfg);
                        self.connect_workspace(cfg);
                        self.active_ws = Some(self.workspaces.len() - 1);
                        self.add_ws_open = false;
                        self.add_ws_name.clear();
                        self.add_ws_eid.clear();
                    }
                });
            if !open {
                self.add_ws_open = false;
            }
        }

        if self.create_open {
            let mut open = true;
            let mut submit: Option<String> = None;
            egui::Window::new("Create agent")
                .open(&mut open)
                .default_size([980.0, 620.0])
                .show(ctx, |ui| {
                    let cx = create::CreateCtx {
                        providers: self
                            .active_ws
                            .and_then(|i| self.workspaces.get(i))
                            .and_then(|w| w.providers.as_ref()),
                        harnesses: self
                            .active_ws
                            .and_then(|i| self.workspaces.get(i))
                            .and_then(|w| w.harnesses.as_ref()),
                        daemon_options: self
                            .active_ws
                            .and_then(|i| self.workspaces.get(i))
                            .map(|w| {
                                w.daemons
                                    .iter()
                                    .filter(|d| d.approved)
                                    .map(|d| {
                                        (
                                            format!("{} ({}…)", d.hostname, d.short_id()),
                                            d.hostname.clone(),
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    };
                    submit = create::show_create(ui, &mut self.create_form, &cx);
                });
            if let Some(toml) = submit {
                if let Some(ws) = self.active_ws {
                    net::spawn_create(
                        self.rt.handle().clone(),
                        ws,
                        self.workspaces[ws].client.clone(),
                        toml,
                        self.tx.clone(),
                        self.ctx_handle(),
                    );
                }
            }
            if !open {
                self.create_open = false;
            }
        }

        if let Some((ws, daemon_id)) = self.labels_editing.clone() {
            let mut open = true;
            let mut applied: Option<(std::collections::BTreeMap<String, String>, Vec<String>)> =
                None;
            egui::Window::new("Edit labels")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    if let Some(d) = self
                        .workspaces
                        .get(ws)
                        .and_then(|w| w.daemons.iter().find(|d| d.endpoint_id == daemon_id))
                        .cloned()
                    {
                        applied = views::labels_editor(ui, &d, &mut self.labels_draft);
                    } else {
                        ui.label("daemon not found");
                    }
                });
            let mut close = !open;
            if let Some((set, remove)) = applied {
                if !set.is_empty() || !remove.is_empty() {
                    net::spawn_action(
                        self.rt.handle().clone(),
                        ws,
                        self.workspaces[ws].client.clone(),
                        "labels",
                        net::Action::SetLabels {
                            id: daemon_id,
                            set,
                            remove,
                        },
                        self.tx.clone(),
                        self.ctx_handle(),
                    );
                }
                close = true;
            }
            if close {
                self.labels_editing = None;
                self.labels_draft.clear();
            }
        }

        if let Some(ws) = self.remove_ws_confirm {
            let mut open = true;
            let mut confirm = false;
            let name = self
                .workspaces
                .get(ws)
                .map(|w| w.cfg.name.clone())
                .unwrap_or_default();
            egui::Window::new("Remove workspace")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Remove workspace '{name}'? Only the connection is removed —\n\
                         agents and daemons on the control plane are untouched."
                    ));
                    if ui
                        .button(RichText::new("Remove").color(Color32::LIGHT_RED))
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            if confirm {
                self.remove_workspace(ws);
                self.remove_ws_confirm = None;
            } else if !open {
                self.remove_ws_confirm = None;
            }
        }

        // Reveal-once dialog (M4): value shown once, never stored beyond
        // the dialog, dedicated audit entry server-side.
        if let Some(ws) = self.active_ws {
            let revealed = self.secrets.get(&ws).and_then(|s| s.revealed.clone());
            if let Some((_, _, value)) = revealed {
                let mut open = true;
                egui::Window::new("Secret value (shown once)")
                    .open(&mut open)
                    .collapsible(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label(
                            RichText::new("audited reveal — this value is not shown again")
                                .size(11.0)
                                .color(Color32::KHAKI),
                        );
                        egui::Frame::new()
                            .fill(Color32::from_rgb(0x18, 0x1C, 0x22))
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.label(RichText::new(&value).monospace().size(12.0));
                            });
                        if ui.button("copy").clicked() {
                            ui.ctx().copy_text(value.clone());
                        }
                    });
                if !open {
                    if let Some(s) = self.secrets.get_mut(&ws) {
                        s.revealed = None;
                    }
                }
            }
        }

        if let Some((ws, agent)) = self.destroy_confirm.clone() {
            let mut open = true;
            let mut confirm = false;
            egui::Window::new("Destroy agent")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.label(format!(
                        "Destroy '{agent}'? The VM is stopped and the registry row removed."
                    ));
                    if ui
                        .button(RichText::new("Destroy").color(Color32::LIGHT_RED))
                        .clicked()
                    {
                        confirm = true;
                    }
                });
            if confirm {
                net::spawn_destroy(
                    self.rt.handle().clone(),
                    ws,
                    self.workspaces[ws].client.clone(),
                    agent.clone(),
                    self.tx.clone(),
                    self.ctx_handle(),
                );
                if let View::Agent {
                    ws: vw, agent: va, ..
                } = &self.view
                {
                    if *vw == ws && *va == agent {
                        self.view = View::Dashboard;
                    }
                }
                self.destroy_confirm = None;
            } else if !open {
                self.destroy_confirm = None;
            }
        }
    }
}

impl eframe::App for SuzyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.stored_ctx = Some(ctx.clone());
        self.drain_net();
        self.top_bar(ctx);
        self.sidebar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| match self.view.clone() {
            View::Dashboard => self.dashboard(ui),
            View::Castellans => {
                if let Some(ws) = self.active_ws {
                    self.castellans_view(ui, ws);
                } else {
                    welcome(ui);
                }
            }
            View::Activity => {
                if let Some(ws) = self.active_ws {
                    if !self.activity.contains_key(&ws) {
                        self.fetch_activity(ws);
                    }
                    let state = self.activity.entry(ws).or_default();
                    if views::activity_view(ui, state) {
                        self.fetch_activity(ws);
                    }
                } else {
                    welcome(ui);
                }
            }
            View::Secrets => {
                if let Some(ws) = self.active_ws {
                    if !self.secrets.contains_key(&ws) {
                        self.fetch_secrets(ws);
                    }
                    let providers = self.workspaces.get(ws).and_then(|w| w.providers.clone());
                    let state = self.secrets.entry(ws).or_default();
                    let intents = views::secrets_view(ui, state, providers.as_ref());
                    for intent in intents {
                        self.dispatch_secret_intent(ws, intent);
                    }
                } else {
                    welcome(ui);
                }
            }
            View::Agent { ws, agent, tab } => self.agent_view(ui, ws, &agent, tab),
        });
        self.dialogs(ctx);
    }
}

// ── small ui helpers ─────────────────────────────────────────────────────

fn welcome(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(120.0);
        ui.heading("👑 Suzy");
        ui.label("Add a workspace to connect to a suzerain control plane.");
        ui.label(
            RichText::new("you need its iroh EndpointId (printed by `suzerain run` / `suz id`)")
                .color(Color32::GRAY),
        );
    });
}

fn stat_card(ui: &mut Ui, label: &str, value: &str) {
    egui::Frame::new()
        .fill(Color32::from_rgb(0x20, 0x24, 0x2B))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(value).size(18.0).strong());
                ui.label(RichText::new(label).size(11.0).color(Color32::GRAY));
            });
        });
    ui.add_space(4.0);
}

pub(crate) fn status_color(status: &str) -> Color32 {
    match status {
        "running" => Color32::from_rgb(0xE6, 0xB3, 0x3C), // working: gold
        "idle" => Color32::from_rgb(0x5C, 0xC8, 0x7A),    // green
        "sleeping" => Color32::from_rgb(0x64, 0x8C, 0xC8), // blue
        "waking" => Color32::from_rgb(0xE8, 0x7D, 0x3E),  // orange
        "failed" => Color32::from_rgb(0xE0, 0x5C, 0x5C),  // red
        _ => Color32::GRAY,
    }
}
