//! iroh operator channel (`suz/operator/0`): how desktop clients (Suzy)
//! talk to the control plane from anywhere iroh reaches. Identity and
//! authorization are the iroh public key: the `[operator] allow` list in
//! config.toml names the EndpointIds that may use this channel.
//!
//! Ops (one per bi-stream, see protocol OperatorHello):
//! - `rest`   — executed in-process against the same axum router the HTTP
//!   API serves (single source of truth for API logic)
//! - `stream` — streaming GET (SSE): body chunks relayed as base64 frames
//! - `shell`  — native pty relay into an agent's VM (ShellMessage frames)

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use iroh::endpoint::Connection;
use iroh::EndpointId;
use suzerain_protocol::control::{OperatorFrame, OperatorHello, ShellMessage};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use tracing::{info, warn};

use crate::control::ControlPlane;
use crate::store::Store;
use crate::web::{self, WebState};

#[derive(Clone)]
pub struct OperatorHandler {
    router: axum::Router,
    cp: Arc<ControlPlane>,
    store: Store,
    allow: Arc<BTreeSet<EndpointId>>,
}

impl std::fmt::Debug for OperatorHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorHandler").finish()
    }
}

impl OperatorHandler {
    pub fn new(cp: Arc<ControlPlane>, allow: BTreeSet<EndpointId>) -> Self {
        let state = WebState {
            store: cp.store().clone(),
            cp: cp.clone(),
        };
        Self {
            router: web::build_router(state),
            store: cp.store().clone(),
            cp,
            allow: Arc::new(allow),
        }
    }
}

impl iroh::protocol::ProtocolHandler for OperatorHandler {
    async fn accept(&self, connection: Connection) -> Result<(), iroh::protocol::AcceptError> {
        let remote = connection.remote_id();
        if !self.allow.contains(&remote) {
            warn!(
                %remote,
                "operator connection rejected (not in [operator] allow list)"
            );
            connection.close(1u32.into(), b"operator not allowed");
            return Ok(());
        }
        info!(%remote, "operator connected");
        let handler = self.clone();
        tokio::spawn(async move {
            while let Ok((send, recv)) = connection.accept_bi().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_stream(handler, send, recv).await {
                        tracing::debug!("operator stream ended: {err:#}");
                    }
                });
            }
        });
        Ok(())
    }
}

async fn handle_stream(
    handler: OperatorHandler,
    mut send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
) -> Result<()> {
    let mut recv = tokio::io::BufReader::new(recv);
    let hello: OperatorHello = read_jsonl(&mut recv).await?;
    match hello {
        OperatorHello::Rest { method, path, body } => {
            rest_op(&handler, &method, &path, body, &mut send).await
        }
        OperatorHello::Stream { path } => stream_op(&handler, &path, &mut send).await,
        OperatorHello::Shell { name } => shell_op(&handler, &name, send, recv).await,
    }
}

/// Build an axum request for the API router.
fn api_request(
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<axum::http::Request<axum::body::Body>> {
    let mut builder = axum::http::Request::builder().method(method).uri(path);
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            axum::body::Body::from(serde_json::to_vec(&v)?)
        }
        None => axum::body::Body::empty(),
    };
    Ok(builder.body(body)?)
}

async fn rest_op(
    handler: &OperatorHandler,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
    send: &mut iroh::endpoint::SendStream,
) -> Result<()> {
    use tower::ServiceExt;
    let frame = match api_request(method, path, body) {
        Ok(req) => match handler.router.clone().oneshot(req).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                    serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
                });
                OperatorFrame::Reply { status, body }
            }
            Err(err) => OperatorFrame::Error {
                message: format!("router error: {err}"),
            },
        },
        Err(err) => OperatorFrame::Error {
            message: format!("bad request: {err:#}"),
        },
    };
    write_jsonl(send, &frame).await?;
    Ok(())
}

async fn stream_op(
    handler: &OperatorHandler,
    path: &str,
    send: &mut iroh::endpoint::SendStream,
) -> Result<()> {
    use futures_util::StreamExt;
    use tower::ServiceExt;
    let req = api_request("GET", path, None)?;
    let resp = handler
        .router
        .clone()
        .oneshot(req)
        .await
        .map_err(|e| anyhow::anyhow!("router error: {e}"))?;
    let mut body = http_body_util::BodyStream::new(resp.into_body());
    while let Some(frame) = body.next().await {
        match frame {
            Ok(frame) => {
                if let Ok(data) = frame.into_data() {
                    let chunk = OperatorFrame::Chunk {
                        data: crate::bundle::base64_encode(&data),
                    };
                    if write_jsonl(send, &chunk).await.is_err() {
                        return Ok(()); // client gone
                    }
                }
            }
            Err(_) => break,
        }
    }
    let _ = write_jsonl(send, &OperatorFrame::End).await;
    Ok(())
}

/// Native shell relay: ShellMessage frames in both directions, wake
/// narration forwarded as notices.
async fn shell_op(
    handler: &OperatorHandler,
    name: &str,
    mut send: iroh::endpoint::SendStream,
    mut recv: tokio::io::BufReader<iroh::endpoint::RecvStream>,
) -> Result<()> {
    let Some(agent) = handler.store.get_agent_by_name(name).await? else {
        write_jsonl(
            &mut send,
            &ShellMessage::Notice {
                message: format!("no agent named '{name}'"),
            },
        )
        .await?;
        anyhow::bail!("no agent named '{name}'");
    };

    // Dial with narration; notices are collected then flushed in order.
    let (dialed, pending) = {
        let mut pending: Vec<String> = Vec::new();
        let result =
            web::dial_agent_shell(&handler.cp, &handler.store, agent, &mut |m| pending.push(m))
                .await;
        (result, pending)
    };
    for m in pending {
        write_jsonl(&mut send, &ShellMessage::Notice { message: m }).await?;
    }
    let Some((mut daemon_send, mut daemon_recv)) = dialed else {
        return Ok(());
    };

    loop {
        tokio::select! {
            msg = read_jsonl::<_, ShellMessage>(&mut recv) => {
                match msg {
                    Ok(msg @ (ShellMessage::Data { .. } | ShellMessage::Resize { .. })) => {
                        if write_jsonl(&mut daemon_send, &msg).await.is_err() { break; }
                    }
                    Ok(_) => {}
                    Err(suzerain_protocol::framing::FramingError::Eof) => break,
                    Err(err) => return Err(err.into()),
                }
            }
            msg = read_jsonl::<_, ShellMessage>(&mut daemon_recv) => {
                match msg {
                    Ok(shell_msg) => {
                        let done = matches!(shell_msg, ShellMessage::Exit { .. });
                        if write_jsonl(&mut send, &shell_msg).await.is_err() { break; }
                        if done { break; }
                    }
                    Err(_) => break,
                }
            }
        }
    }
    Ok(())
}
