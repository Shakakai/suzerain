//! Async bridge: all network work happens on the tokio runtime; results
//! arrive here as `NetMsg` values drained by the UI each frame.

use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use suzerain_client::{Agent, Client, Daemon, EndpointInfo, Overview, SessionEvent};

pub type WsId = usize;

#[derive(Debug)]
pub enum NetMsg {
    Connected {
        ws: WsId,
        info: EndpointInfo,
    },
    ConnectFailed {
        ws: WsId,
        error: String,
    },
    /// Full refetch of the workspace's fleet view.
    Snapshot {
        ws: WsId,
        overview: Box<Overview>,
        agents: Vec<Agent>,
        daemons: Vec<Daemon>,
        pending: Vec<Value>,
    },
    /// Provider + harness catalogs (fetched once per connection).
    Catalogs {
        ws: WsId,
        providers: Value,
        harnesses: Value,
    },
    SessionHistory {
        ws: WsId,
        agent: String,
        item: Value,
    },
    SessionHistoryEnd {
        ws: WsId,
        agent: String,
    },
    SessionLive {
        ws: WsId,
        agent: String,
        event: Value,
    },
    /// The session stream ended or errored; the UI may restart it.
    SessionClosed {
        ws: WsId,
        agent: String,
        error: Option<String>,
    },
    PromptDone {
        ws: WsId,
        agent: String,
        result: std::result::Result<Value, String>,
    },
    CreateDone {
        #[allow(dead_code)]
        ws: WsId,
        result: std::result::Result<Value, String>,
    },
    DestroyDone {
        #[allow(dead_code)]
        ws: WsId,
        result: std::result::Result<(), String>,
    },
    /// Agent details (manifest_toml, sessions, last_event_at, …).
    Details {
        ws: WsId,
        agent: String,
        result: std::result::Result<Value, String>,
    },
    /// Agent central-log events.
    Logs {
        ws: WsId,
        agent: String,
        result: std::result::Result<Value, String>,
    },
    /// Global audit feed (activity view).
    Activity {
        ws: WsId,
        result: std::result::Result<Vec<Value>, String>,
    },
    /// Masked secrets inventory.
    Secrets {
        ws: WsId,
        result: std::result::Result<Value, String>,
    },
    /// Audited reveal-once result (the only time a value crosses the wire).
    RevealDone {
        ws: WsId,
        result: std::result::Result<Value, String>,
    },
    /// Pty output bytes for the terminal tab (already base64-decoded).
    ShellData {
        ws: WsId,
        agent: String,
        bytes: Vec<u8>,
    },
    /// Shell-level notice (wake narration, spawn errors).
    ShellNotice {
        ws: WsId,
        agent: String,
        message: String,
    },
    /// The shell connection ended (exit, error, or drop).
    ShellClosed {
        ws: WsId,
        agent: String,
        exit: Option<i64>,
        error: Option<String>,
    },
    /// Labels / pending-enrollment / auto-suspend mutations.
    ActionDone {
        #[allow(dead_code)]
        ws: WsId,
        what: &'static str,
        result: std::result::Result<(), String>,
    },
}

fn send(tx: &Sender<NetMsg>, ctx: &egui::Context, msg: NetMsg) {
    if tx.send(msg).is_ok() {
        ctx.request_repaint();
    }
}

/// Fetch overview + agents + daemons + pending enrollments; send a Snapshot.
async fn push_snapshot(ws: WsId, client: &Client, tx: &Sender<NetMsg>, ctx: &egui::Context) {
    let res: anyhow::Result<NetMsg> = async {
        let overview = client.overview().await?;
        let agents = client.agents().await?;
        let daemons = client.daemons().await?;
        let pending = client.pending_daemons().await.unwrap_or_default();
        Ok(NetMsg::Snapshot {
            ws,
            overview: Box::new(overview),
            agents,
            daemons,
            pending,
        })
    }
    .await;
    match res {
        Ok(msg) => send(tx, ctx, msg),
        Err(e) => send(
            tx,
            ctx,
            NetMsg::ConnectFailed {
                ws,
                error: format!("{e:#}"),
            },
        ),
    }
}

/// Verify connectivity + identity, then run the workspace loop: initial
/// snapshot, fleet-event subscription with immediate refetch, 15s fallback
/// poll, automatic resubscribe on stream failure.
pub fn spawn_workspace_loop(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) -> tokio::task::AbortHandle {
    let task = rt.spawn(async move {
        match client.endpoint().await {
            Ok(info) => send(&tx, &ctx, NetMsg::Connected { ws, info }),
            Err(e) => {
                send(
                    &tx,
                    &ctx,
                    NetMsg::ConnectFailed {
                        ws,
                        error: format!("{e:#}"),
                    },
                );
                return;
            }
        }
        // Catalogs: static for the life of the connection.
        if let (Ok(providers), Ok(harnesses)) = (client.providers().await, client.harnesses().await)
        {
            send(
                &tx,
                &ctx,
                NetMsg::Catalogs {
                    ws,
                    providers,
                    harnesses,
                },
            );
        }
        push_snapshot(ws, &client, &tx, &ctx).await;
        loop {
            // (Re)subscribe to the fleet event stream.
            match client.events_stream().await {
                Ok(stream) => {
                    use futures_util::StreamExt;
                    tokio::pin!(stream);
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                                push_snapshot(ws, &client, &tx, &ctx).await;
                            }
                            item = stream.next() => {
                                match item {
                                    Some(Ok(_)) => {
                                        // Any hint: refetch. Bursts are
                                        // cheap (three small GETs).
                                        push_snapshot(ws, &client, &tx, &ctx).await;
                                    }
                                    Some(Err(_)) | None => break, // resubscribe
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    // Events endpoint unreachable (older control plane?):
                    // fall back to plain polling.
                    loop {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        push_snapshot(ws, &client, &tx, &ctx).await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
    task.abort_handle()
}

/// Stream one agent's session: history replay then live events. Ends only
/// on error or client drop (suspend/wake is handled server-side).
pub fn spawn_session_stream(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    agent: String,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) -> tokio::task::AbortHandle {
    let task = rt.spawn(async move {
        use futures_util::StreamExt;
        let stream = match client.session_stream(&agent).await {
            Ok(s) => s,
            Err(e) => {
                send(
                    &tx,
                    &ctx,
                    NetMsg::SessionClosed {
                        ws,
                        agent,
                        error: Some(format!("{e:#}")),
                    },
                );
                return;
            }
        };
        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            let msg = match item {
                Ok(SessionEvent::History(v)) => NetMsg::SessionHistory {
                    ws,
                    agent: agent.clone(),
                    item: v,
                },
                Ok(SessionEvent::HistoryEnd) => NetMsg::SessionHistoryEnd {
                    ws,
                    agent: agent.clone(),
                },
                Ok(SessionEvent::Live(v)) => NetMsg::SessionLive {
                    ws,
                    agent: agent.clone(),
                    event: v,
                },
                Ok(SessionEvent::ServerError(e)) => NetMsg::SessionClosed {
                    ws,
                    agent: agent.clone(),
                    error: Some(e),
                },
                Err(e) => NetMsg::SessionClosed {
                    ws,
                    agent: agent.clone(),
                    error: Some(format!("{e:#}")),
                },
            };
            let closed = matches!(msg, NetMsg::SessionClosed { .. });
            send(&tx, &ctx, msg);
            if closed {
                return;
            }
        }
        send(
            &tx,
            &ctx,
            NetMsg::SessionClosed {
                ws,
                agent,
                error: None,
            },
        );
    });
    task.abort_handle()
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_prompt(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    agent: String,
    message: String,
    mode: suzerain_client::PromptMode,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client
            .prompt(&agent, &message, mode)
            .await
            .map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::PromptDone { ws, agent, result });
    });
}

pub fn spawn_create(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    manifest_toml: String,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client
            .create_agent(&manifest_toml)
            .await
            .map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::CreateDone { ws, result });
    });
}

pub fn spawn_destroy(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    agent: String,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client
            .destroy_agent(&agent, false)
            .await
            .map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::DestroyDone { ws, result });
    });
}

pub fn spawn_details(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    agent: String,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client.agent(&agent).await.map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::Details { ws, agent, result });
    });
}

pub fn spawn_activity(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    tail: usize,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client.audit(tail).await.map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::Activity { ws, result });
    });
}

pub fn spawn_secrets(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client.secrets().await.map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::Secrets { ws, result });
    });
}

pub fn spawn_reveal(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    kind: String,
    name: String,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result = client
            .reveal_secret(&kind, &name)
            .await
            .map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::RevealDone { ws, result });
    });
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_logs(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    agent: String,
    kind: Option<String>,
    q: Option<String>,
    tail: usize,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let mut url = format!("tail={tail}");
        if let Some(k) = kind.filter(|k| !k.is_empty()) {
            url.push_str(&format!("&kind={k}"));
        }
        if let Some(q) = q.filter(|q| !q.is_empty()) {
            url.push_str(&format!("&q={}", q.replace(' ', "%20")));
        }
        let result = client
            .agent_logs_query(&agent, &url)
            .await
            .map_err(|e| format!("{e:#}"));
        send(&tx, &ctx, NetMsg::Logs { ws, agent, result });
    });
}

/// One-shot control-plane mutations (labels, pending approve/dismiss,
/// auto-suspend, daemon remove). The workspace loop's fleet events deliver
/// the resulting state; this only reports success/failure.
pub fn spawn_action(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    what: &'static str,
    action: Action,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) {
    rt.spawn(async move {
        let result: std::result::Result<(), String> = match action {
            Action::ApprovePending(id) => client
                .approve_pending(&id)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::DismissPending(id) => client
                .dismiss_pending(&id)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::RemoveDaemon(id) => client
                .remove_daemon(&id)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::SetLabels { id, set, remove } => client
                .set_daemon_labels(&id, &set, &remove)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::SetAutoSuspend { agent, value } => client
                .set_auto_suspend(&agent, &value)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::SetSecretProvider { id, value } => client
                .set_secret_provider(&id, &value)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::DeleteSecretProvider(id) => client
                .delete_secret_provider(&id)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::SetSecretExtra { name, value } => client
                .set_secret_extra(&name, &value)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::DeleteSecretExtra(name) => client
                .delete_secret_extra(&name)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::SetDeployKey(value) => client
                .set_deploy_key(&value)
                .await
                .map_err(|e| format!("{e:#}")),
            Action::DeleteDeployKey => client
                .delete_deploy_key()
                .await
                .map_err(|e| format!("{e:#}")),
        };
        send(&tx, &ctx, NetMsg::ActionDone { ws, what, result });
    });
}

/// Interactive shell into the agent's VM (M4): connects the WebSocket
/// relay, forwards widget input, demuxes pty output. `input_rx` is the
/// widget's channel; the task ends on shell exit, error, or abort.
pub fn spawn_shell(
    rt: tokio::runtime::Handle,
    ws: WsId,
    client: Arc<Client>,
    agent: String,
    mut input_rx: tokio::sync::mpsc::UnboundedReceiver<crate::terminal::TermInput>,
    tx: Sender<NetMsg>,
    ctx: egui::Context,
) -> tokio::task::AbortHandle {
    let task = rt.spawn(async move {
        use suzerain_client::ShellMessage;
        let mut conn = match client.shell_connect(&agent).await {
            Ok(c) => c,
            Err(e) => {
                send(
                    &tx,
                    &ctx,
                    NetMsg::ShellClosed {
                        ws,
                        agent,
                        exit: None,
                        error: Some(format!("{e:#}")),
                    },
                );
                return;
            }
        };
        loop {
            tokio::select! {
                input = input_rx.recv() => {
                    let Some(input) = input else { break };
                    let res = match input {
                        crate::terminal::TermInput::Data(bytes) => conn.send_input(&bytes).await,
                        crate::terminal::TermInput::Resize { cols, rows } => {
                            conn.resize(cols, rows).await
                        }
                    };
                    if let Err(e) = res {
                        send(&tx, &ctx, NetMsg::ShellClosed {
                            ws, agent, exit: None, error: Some(format!("{e:#}")),
                        });
                        return;
                    }
                }
                msg = conn.next() => {
                    match msg {
                        Some(Ok(ShellMessage::Data { data })) => {
                            if let Ok(bytes) = suzerain_client::b64_decode(&data) {
                                send(&tx, &ctx, NetMsg::ShellData {
                                    ws, agent: agent.clone(), bytes,
                                });
                            }
                        }
                        Some(Ok(ShellMessage::Notice { message })) => {
                            send(&tx, &ctx, NetMsg::ShellNotice {
                                ws, agent: agent.clone(), message,
                            });
                        }
                        Some(Ok(ShellMessage::Exit { code })) => {
                            send(&tx, &ctx, NetMsg::ShellClosed {
                                ws, agent, exit: Some(code), error: None,
                            });
                            return;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => {
                            send(&tx, &ctx, NetMsg::ShellClosed {
                                ws, agent, exit: None, error: Some(format!("{e:#}")),
                            });
                            return;
                        }
                        None => {
                            send(&tx, &ctx, NetMsg::ShellClosed {
                                ws, agent, exit: None, error: None,
                            });
                            return;
                        }
                    }
                }
            }
        }
    });
    task.abort_handle()
}

pub enum Action {
    ApprovePending(String),
    DismissPending(String),
    RemoveDaemon(String),
    SetLabels {
        id: String,
        set: std::collections::BTreeMap<String, String>,
        remove: Vec<String>,
    },
    SetAutoSuspend {
        agent: String,
        value: String,
    },
    SetSecretProvider {
        id: String,
        value: String,
    },
    DeleteSecretProvider(String),
    SetSecretExtra {
        name: String,
        value: String,
    },
    DeleteSecretExtra(String),
    SetDeployKey(String),
    DeleteDeployKey,
}
