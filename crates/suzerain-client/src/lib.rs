//! Async Rust client for a suzerain control plane over the **iroh operator
//! channel** (`suz/operator/0`, see `crates/suzerain/src/operator.rs`).
//!
//! Connects by EndpointId from anywhere iroh reaches (N0 relays + NAT
//! holepunching); authorization is the client's public key against the
//! control plane's `[operator] allow` list. The API surface mirrors the
//! `/api/v1` HTTP operator API (which continues to serve the web UI and
//! MCP): unary calls become `Rest` ops executed against the same router
//! in-process; SSE endpoints become `Stream` ops; the agent shell is a
//! native `Shell` op.

use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use suzerain_protocol::control::{OperatorFrame, OperatorHello};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::{NodeCapacity, NodeUsage};
use tokio::io::BufReader;
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

pub use iroh;
use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Non-2xx from the control plane; message is the API's {error} body.
    #[error("http {0}: {1}")]
    Http(u16, String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Transport-level failure on the operator channel.
    #[error("operator channel: {0}")]
    Channel(String),
    /// The server returned an OperatorFrame::Error.
    #[error("operator error: {0}")]
    Op(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn channel_err(e: impl std::fmt::Display) -> Error {
    Error::Channel(format!("{e:#}"))
}

// ── models ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct EndpointInfo {
    pub endpoint_id: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Overview {
    pub endpoint_id: String,
    pub daemons_total: usize,
    pub daemons_online: usize,
    pub agents_total: usize,
    #[serde(default)]
    pub agents_by_state: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Daemon {
    pub endpoint_id: String,
    pub approved: bool,
    pub online: bool,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Labels as reported by the daemon itself.
    #[serde(default)]
    pub reported_labels: BTreeMap<String, String>,
    /// Operator-side overrides (win over reported).
    #[serde(default)]
    pub label_overrides: BTreeMap<String, String>,
    #[serde(default)]
    pub max_agents: u32,
    #[serde(default)]
    pub last_seen: String,
    #[serde(default)]
    pub capacity: NodeCapacity,
    #[serde(default)]
    pub usage: NodeUsage,
}

impl Daemon {
    pub fn short_id(&self) -> &str {
        &self.endpoint_id[..self.endpoint_id.len().min(8)]
    }
}

/// An agent row as served by `GET /api/v1/agents`. `status` is the public
/// vocabulary: running | idle | sleeping | waking | failed | decommissioned.
#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub id: Uuid,
    pub name: String,
    pub daemon_endpoint_id: String,
    pub manifest: AgentManifest,
    pub state: String,
    pub status: String,
    #[serde(default)]
    pub busy: Option<bool>,
    #[serde(default)]
    pub idle_secs: Option<i64>,
    #[serde(default)]
    pub needs_attention: bool,
    #[serde(default)]
    pub auto_suspend_override: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub session_file: Option<String>,
    #[serde(default)]
    pub daemon_hostname: Option<String>,
}

/// One message on an SSE-style stream: the `event:` name and the `data:`
/// payload (raw JSON text; parse per stream semantics).
#[derive(Debug, Clone)]
pub struct SseMessage {
    pub event: String,
    pub data: String,
}

impl SseMessage {
    pub fn json(&self) -> Result<Value> {
        Ok(serde_json::from_str(&self.data)?)
    }
}

/// High-level demux of the per-agent session stream: replayed `history`
/// items, then `history_end`, then live pi `event`s (plus synthetic
/// `status`/`notice` system lines and a terminal `error`).
#[derive(Debug, Clone)]
pub enum SessionEvent {
    History(Value),
    HistoryEnd,
    Live(Value),
    ServerError(String),
}

/// Prompt delivery mode for [`Client::prompt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Prompt,
    Steer,
    FollowUp,
    Abort,
}

impl PromptMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Steer => "steer",
            Self::FollowUp => "follow_up",
            Self::Abort => "abort",
        }
    }
}

// ── client ───────────────────────────────────────────────────────────────

/// A connection to one suzerain control plane over iroh. Cheap to clone;
/// the underlying endpoint and connection are shared.
#[derive(Clone)]
pub struct Client {
    inner: std::sync::Arc<Inner>,
}

struct Inner {
    key: SecretKey,
    /// Connect target: a bare EndpointId string (parsed at dial time, so
    /// invalid input surfaces as a connection error, not a build failure)
    /// or a full address with direct socket addrs (tests/LAN).
    remote: RemoteTarget,
    /// Full N0 discovery (DNS + relays) for real deployments; minimal for
    /// direct-address (test) connections.
    full_discovery: bool,
    endpoint: OnceCell<Endpoint>,
    conn: Mutex<Option<iroh::endpoint::Connection>>,
}

enum RemoteTarget {
    Id(String),
    Addr(EndpointAddr),
}

impl RemoteTarget {
    fn resolve(&self) -> Result<EndpointAddr> {
        match self {
            Self::Id(s) => {
                let id: EndpointId = s
                    .trim()
                    .parse()
                    .map_err(|_| Error::Channel(format!("invalid endpoint id '{s}'")))?;
                Ok(EndpointAddr::new(id))
            }
            Self::Addr(a) => Ok(a.clone()),
        }
    }
}

impl Client {
    /// Connect by EndpointId (production path: N0 discovery + relays —
    /// works anywhere iroh reaches). `key` is the operator identity Suzy
    /// persists; its public half must be in the control plane's
    /// `[operator] allow` list. The id is validated at dial time.
    pub fn new(remote_endpoint_id: &str, key: SecretKey) -> Self {
        Self {
            inner: std::sync::Arc::new(Inner {
                key,
                remote: RemoteTarget::Id(remote_endpoint_id.to_string()),
                full_discovery: true,
                endpoint: OnceCell::new(),
                conn: Mutex::new(None),
            }),
        }
    }

    /// Connect with a full address (direct socket addresses — used by
    /// tests and LAN setups; no discovery needed).
    pub fn with_addr(addr: EndpointAddr, key: SecretKey) -> Self {
        Self {
            inner: std::sync::Arc::new(Inner {
                key,
                remote: RemoteTarget::Addr(addr),
                full_discovery: false,
                endpoint: OnceCell::new(),
                conn: Mutex::new(None),
            }),
        }
    }

    /// Suzy's public identity — the id to add to `[operator] allow`.
    pub fn local_id(&self) -> String {
        self.inner.key.public().to_string()
    }

    async fn iroh_endpoint(&self) -> Result<&Endpoint> {
        self.inner
            .endpoint
            .get_or_try_init(|| async {
                let mut builder = if self.inner.full_discovery {
                    Endpoint::builder(presets::N0)
                } else {
                    Endpoint::builder(presets::Empty)
                };
                builder = builder
                    .secret_key(self.inner.key.clone())
                    // Explicit provider: presets::Empty sets none, and the
                    // process default is ambiguous when several are linked.
                    .crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()));
                builder.bind().await.map_err(channel_err)
            })
            .await
    }

    async fn connection(&self) -> Result<iroh::endpoint::Connection> {
        {
            let guard = self.inner.conn.lock().await;
            if let Some(conn) = guard.as_ref() {
                if conn.close_reason().is_none() {
                    return Ok(conn.clone());
                }
            }
        }
        let endpoint = self.iroh_endpoint().await?;
        let remote = self.inner.remote.resolve()?;
        let conn = endpoint
            .connect(remote, suzerain_protocol::alpn::OPERATOR)
            .await
            .map_err(channel_err)?;
        *self.inner.conn.lock().await = Some(conn.clone());
        Ok(conn)
    }

    /// Drop the cached connection (after a transport failure).
    async fn disconnect(&self) {
        *self.inner.conn.lock().await = None;
    }

    /// Unary op: one bi-stream, one Reply frame. Redials once on transport
    /// failure. Non-2xx statuses become `Error::Http` with the API's
    /// {error} body — same semantics as the HTTP API.
    async fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let mut last_err: Option<Error> = None;
        for attempt in 0..2 {
            match self.rest_once(method, path, body.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    self.disconnect().await;
                    if attempt == 0 {
                        continue;
                    }
                }
            }
        }
        Err(last_err.expect("one attempt always runs"))
    }

    async fn rest_once(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let conn = self.connection().await?;
        let (mut send, recv) = conn.open_bi().await.map_err(channel_err)?;
        let hello = OperatorHello::Rest {
            method: method.to_string(),
            path: path.to_string(),
            body,
        };
        write_jsonl(&mut send, &hello).await.map_err(channel_err)?;
        let mut recv = BufReader::new(recv);
        let frame: OperatorFrame = read_jsonl(&mut recv).await.map_err(channel_err)?;
        match frame {
            OperatorFrame::Reply { status, body } => {
                if (200..300).contains(&status) {
                    Ok(body)
                } else {
                    let msg = body["error"]
                        .as_str()
                        .unwrap_or("request failed")
                        .to_string();
                    Err(Error::Http(status, msg))
                }
            }
            OperatorFrame::Error { message } => Err(Error::Op(message)),
            other => Err(Error::Channel(format!("unexpected frame: {other:?}"))),
        }
    }

    /// Streaming op: body chunks of the SSE response as decoded bytes.
    async fn stream_bytes(
        &self,
        path: &str,
    ) -> Result<std::pin::Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>> {
        let conn = self.connection().await?;
        let (mut send, recv) = conn.open_bi().await.map_err(channel_err)?;
        write_jsonl(
            &mut send,
            &OperatorHello::Stream {
                path: path.to_string(),
            },
        )
        .await
        .map_err(channel_err)?;
        // Keep `send` alive: dropping it signals EOF to the server.
        let stream = futures_util::stream::unfold(
            (BufReader::new(recv), send, false),
            |(mut recv, send, done)| async move {
                if done {
                    return None;
                }
                let frame: std::result::Result<OperatorFrame, _> = read_jsonl(&mut recv).await;
                match frame {
                    Ok(OperatorFrame::Chunk { data }) => {
                        Some((b64_decode(&data), (recv, send, false)))
                    }
                    Ok(OperatorFrame::End) => Some((Ok(Vec::new()), (recv, send, true))),
                    Ok(OperatorFrame::Error { message }) => {
                        Some((Err(Error::Op(message)), (recv, send, true)))
                    }
                    Ok(other) => Some((
                        Err(Error::Channel(format!("unexpected frame: {other:?}"))),
                        (recv, send, true),
                    )),
                    Err(e) => {
                        // EOF on a long-lived stream: clean end.
                        if matches!(e, suzerain_protocol::framing::FramingError::Eof) {
                            Some((Ok(Vec::new()), (recv, send, true)))
                        } else {
                            Some((Err(channel_err(e)), (recv, send, true)))
                        }
                    }
                }
            },
        );
        Ok(Box::pin(stream))
    }

    /// SSE over a stream op: reassembles chunks into SseMessages using the
    /// same block parser as any SSE transport.
    async fn sse(
        &self,
        path: &str,
    ) -> Result<std::pin::Pin<Box<dyn Stream<Item = Result<SseMessage>> + Send>>> {
        let chunks = self.stream_bytes(path).await?;
        Ok(Box::pin(chunks_to_sse(chunks)))
    }

    // ── fleet ────────────────────────────────────────────────────────

    pub async fn endpoint(&self) -> Result<EndpointInfo> {
        Ok(serde_json::from_value(
            self.rest("GET", "/api/v1/endpoint", None).await?,
        )?)
    }

    pub async fn overview(&self) -> Result<Overview> {
        Ok(serde_json::from_value(
            self.rest("GET", "/api/v1/overview", None).await?,
        )?)
    }

    pub async fn daemons(&self) -> Result<Vec<Daemon>> {
        let v = self.rest("GET", "/api/v1/daemons", None).await?;
        Ok(serde_json::from_value(v["daemons"].clone())?)
    }

    pub async fn pending_daemons(&self) -> Result<Vec<Value>> {
        let v = self.rest("GET", "/api/v1/daemons/pending", None).await?;
        Ok(serde_json::from_value(v["pending"].clone())?)
    }

    pub async fn approve_daemon(&self, endpoint_id: &str) -> Result<()> {
        self.rest(
            "POST",
            "/api/v1/daemons/approve",
            Some(json!({"endpoint_id": endpoint_id})),
        )
        .await?;
        Ok(())
    }

    pub async fn approve_pending(&self, endpoint_id: &str) -> Result<()> {
        self.rest(
            "POST",
            &format!("/api/v1/daemons/pending/{endpoint_id}/approve"),
            Some(json!({})),
        )
        .await?;
        Ok(())
    }

    pub async fn dismiss_pending(&self, endpoint_id: &str) -> Result<()> {
        self.rest(
            "POST",
            &format!("/api/v1/daemons/pending/{endpoint_id}/dismiss"),
            Some(json!({})),
        )
        .await?;
        Ok(())
    }

    pub async fn remove_daemon(&self, endpoint_id: &str) -> Result<()> {
        self.rest("DELETE", &format!("/api/v1/daemons/{endpoint_id}"), None)
            .await?;
        Ok(())
    }

    pub async fn set_daemon_labels(
        &self,
        id: &str,
        set: &BTreeMap<String, String>,
        remove: &[String],
    ) -> Result<()> {
        self.rest(
            "POST",
            &format!("/api/v1/daemons/{id}/labels"),
            Some(json!({"set": set, "remove": remove})),
        )
        .await?;
        Ok(())
    }

    pub async fn audit(&self, tail: usize) -> Result<Vec<Value>> {
        let v = self
            .rest("GET", &format!("/api/v1/audit?tail={tail}"), None)
            .await?;
        Ok(serde_json::from_value(v["entries"].clone())?)
    }

    // ── agents ───────────────────────────────────────────────────────

    pub async fn agents(&self) -> Result<Vec<Agent>> {
        let v = self.rest("GET", "/api/v1/agents", None).await?;
        Ok(serde_json::from_value(v["agents"].clone())?)
    }

    pub async fn agent(&self, name: &str) -> Result<Value> {
        self.rest("GET", &format!("/api/v1/agents/{name}"), None)
            .await
    }

    pub async fn create_agent(&self, manifest_toml: &str) -> Result<Value> {
        self.rest(
            "POST",
            "/api/v1/agents",
            Some(json!({"manifest_toml": manifest_toml})),
        )
        .await
    }

    pub async fn destroy_agent(&self, name: &str, force: bool) -> Result<()> {
        self.rest(
            "POST",
            &format!("/api/v1/agents/{name}/destroy"),
            Some(json!({"force": force})),
        )
        .await?;
        Ok(())
    }

    pub async fn set_auto_suspend(&self, name: &str, value: &str) -> Result<()> {
        self.rest(
            "PATCH",
            &format!("/api/v1/agents/{name}"),
            Some(json!({"auto_suspend": value})),
        )
        .await?;
        Ok(())
    }

    pub async fn agent_logs(&self, name: &str, tail: usize) -> Result<Value> {
        self.rest(
            "GET",
            &format!("/api/v1/agents/{name}/logs?tail={tail}"),
            None,
        )
        .await
    }

    pub async fn agent_logs_query(&self, name: &str, query: &str) -> Result<Value> {
        self.rest("GET", &format!("/api/v1/agents/{name}/logs?{query}"), None)
            .await
    }

    // ── session ──────────────────────────────────────────────────────

    pub async fn session_history(&self, name: &str, tail: Option<usize>) -> Result<Value> {
        let q = tail.map(|t| format!("?tail={t}")).unwrap_or_default();
        self.rest(
            "GET",
            &format!("/api/v1/agents/{name}/session/history{q}"),
            None,
        )
        .await
    }

    pub async fn session_state(&self, name: &str) -> Result<Value> {
        self.rest("GET", &format!("/api/v1/agents/{name}/session_state"), None)
            .await
    }

    pub async fn prompt(&self, name: &str, message: &str, mode: PromptMode) -> Result<Value> {
        self.rest(
            "POST",
            &format!("/api/v1/agents/{name}/prompt"),
            Some(json!({"message": message, "mode": mode.as_str()})),
        )
        .await
    }

    pub async fn session_stream(
        &self,
        name: &str,
    ) -> Result<std::pin::Pin<Box<dyn Stream<Item = Result<SessionEvent>> + Send>>> {
        let sse = self.sse(&format!("/api/v1/agents/{name}/session")).await?;
        use futures_util::StreamExt;
        Ok(Box::pin(sse.filter_map(|m| async move {
            match m {
                Ok(msg) => Some(match msg.event.as_str() {
                    "history" => msg.json().map(SessionEvent::History),
                    "history_end" => Ok(SessionEvent::HistoryEnd),
                    "error" => Ok(SessionEvent::ServerError(msg.data)),
                    _ => msg.json().map(SessionEvent::Live),
                }),
                Err(e) => Some(Err(e)),
            }
        })))
    }

    /// Global fleet event stream (G6): named events (`agent_state`,
    /// `agent_activity`, `agent`, `daemon`, `pending_daemon`, `audit`,
    /// `resync`). Advisory hints; refetch affected lists on receipt.
    pub async fn events_stream(
        &self,
    ) -> Result<std::pin::Pin<Box<dyn Stream<Item = Result<SseMessage>> + Send>>> {
        self.sse("/api/v1/events").await
    }

    // ── shell ────────────────────────────────────────────────────────

    /// Open a pty shell into an agent's guest VM (M4). Sleeping agents
    /// wake transparently server-side; progress arrives as
    /// `ShellMessage::Notice` frames.
    pub async fn shell_connect(&self, name: &str) -> Result<ShellConn> {
        let conn = self.connection().await?;
        let (mut send, recv) = conn.open_bi().await.map_err(channel_err)?;
        write_jsonl(
            &mut send,
            &OperatorHello::Shell {
                name: name.to_string(),
            },
        )
        .await
        .map_err(channel_err)?;
        Ok(ShellConn {
            send,
            recv: BufReader::new(recv),
        })
    }

    // ── catalogs & secrets ───────────────────────────────────────────

    pub async fn providers(&self) -> Result<Value> {
        self.rest("GET", "/api/v1/providers", None).await
    }

    pub async fn harnesses(&self) -> Result<Value> {
        self.rest("GET", "/api/v1/harnesses", None).await
    }

    pub async fn pi_packages(&self, q: Option<&str>, page: usize) -> Result<Value> {
        let mut url = format!("/api/v1/pi-packages?page={page}");
        if let Some(q) = q {
            url.push_str(&format!("&q={}", urlencoding_simple(q)));
        }
        self.rest("GET", &url, None).await
    }

    pub async fn secrets(&self) -> Result<Value> {
        self.rest("GET", "/api/v1/secrets", None).await
    }

    pub async fn set_secret_provider(&self, id: &str, value: &str) -> Result<()> {
        self.rest(
            "PUT",
            &format!("/api/v1/secrets/providers/{id}"),
            Some(json!({"value": value})),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_secret_provider(&self, id: &str) -> Result<()> {
        self.rest("DELETE", &format!("/api/v1/secrets/providers/{id}"), None)
            .await?;
        Ok(())
    }

    pub async fn set_secret_extra(&self, name: &str, value: &str) -> Result<()> {
        self.rest(
            "PUT",
            &format!("/api/v1/secrets/extra/{name}"),
            Some(json!({"value": value})),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_secret_extra(&self, name: &str) -> Result<()> {
        self.rest("DELETE", &format!("/api/v1/secrets/extra/{name}"), None)
            .await?;
        Ok(())
    }

    pub async fn set_deploy_key(&self, value: &str) -> Result<()> {
        self.rest(
            "PUT",
            "/api/v1/secrets/git-deploy-key",
            Some(json!({"value": value})),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_deploy_key(&self) -> Result<()> {
        self.rest("DELETE", "/api/v1/secrets/git-deploy-key", None)
            .await?;
        Ok(())
    }

    /// Audited reveal-once: the value is returned in this response only and
    /// a `secret_reveal` audit entry is written (name + actor, never value).
    pub async fn reveal_secret(&self, kind: &str, name: &str) -> Result<Value> {
        self.rest(
            "POST",
            "/api/v1/secrets/reveal",
            Some(json!({"kind": kind, "name": name})),
        )
        .await
    }
}

// ── shell connection ─────────────────────────────────────────────────────

pub use suzerain_protocol::control::ShellMessage;

/// A live shell connection: pty into the agent's guest VM over the
/// operator channel (ShellMessage JSONL frames).
pub struct ShellConn {
    send: iroh::endpoint::SendStream,
    recv: BufReader<iroh::endpoint::RecvStream>,
}

impl ShellConn {
    pub async fn send(&mut self, msg: &ShellMessage) -> Result<()> {
        write_jsonl(&mut self.send, msg).await.map_err(channel_err)
    }

    /// Raw input bytes → base64 Data frame.
    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<()> {
        self.send(&ShellMessage::Data {
            data: b64_encode(bytes),
        })
        .await
    }

    pub async fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.send(&ShellMessage::Resize { cols, rows }).await
    }

    pub async fn next(&mut self) -> Option<Result<ShellMessage>> {
        match read_jsonl(&mut self.recv).await {
            Ok(msg) => Some(Ok(msg)),
            Err(suzerain_protocol::framing::FramingError::Eof) => None,
            Err(e) => Some(Err(channel_err(e))),
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn urlencoding_simple(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

pub fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let n = chunk
            .iter()
            .fold(0u32, |acc, &c| (acc << 6) | val(c).unwrap_or(0));
        let len = chunk.iter().filter(|&&c| c != b'=').count();
        if len >= 2 {
            out.push((n >> 16) as u8);
        }
        if len >= 3 {
            out.push((n >> 8) as u8);
        }
        if len >= 4 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Incrementally parse a byte-chunk stream into SSE messages (multi-line
/// data, event names, `:` keep-alives, blank-line block separators).
fn chunks_to_sse(
    chunks: std::pin::Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>,
) -> impl Stream<Item = Result<SseMessage>> + Send {
    use futures_util::StreamExt;
    futures_util::stream::unfold(
        (
            chunks,
            Vec::<u8>::new(),
            std::collections::VecDeque::<SseMessage>::new(),
            false,
        ),
        |(mut chunks, mut buf, mut ready, mut eof)| async move {
            loop {
                if let Some(msg) = ready.pop_front() {
                    return Some((Ok(msg), (chunks, buf, ready, eof)));
                }
                while let Some((len, end)) = find_block_end(&buf) {
                    let block: Vec<u8> = buf.drain(..end).collect();
                    if let Some(msg) = parse_block(&block[..len]) {
                        ready.push_back(msg);
                    }
                }
                if let Some(msg) = ready.pop_front() {
                    return Some((Ok(msg), (chunks, buf, ready, eof)));
                }
                if eof {
                    if let Some(msg) = parse_block(&buf) {
                        return Some((Ok(msg), (chunks, Vec::new(), ready, true)));
                    }
                    return None;
                }
                match chunks.next().await {
                    Some(Ok(chunk)) if chunk.is_empty() => eof = true, // End marker
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    Some(Err(e)) => return Some((Err(e), (chunks, buf, ready, true))),
                    None => eof = true,
                }
            }
        },
    )
}

fn find_block_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, i + 2));
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some((i, i + 4));
        }
        i += 1;
    }
    None
}

fn parse_block(block: &[u8]) -> Option<SseMessage> {
    let text = String::from_utf8_lossy(block);
    let mut event = String::new();
    let mut data: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with(':') {
            continue;
        }
        if let Some(v) = line.strip_prefix("event:") {
            event = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v).to_string());
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseMessage {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        for data in [
            b"".as_slice(),
            b"a",
            b"ab",
            b"abc",
            b"hello, terminal \x1b[31mred\x1b[0m",
            &[0u8, 1, 2, 253, 254, 255][..],
        ] {
            assert_eq!(b64_decode(&b64_encode(data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64_decode("aGVsbG8=").unwrap(), b"hello");
    }

    #[test]
    fn parses_sse_blocks() {
        let raw = b"event: history\ndata: {\"a\":1}\n\n: keep-alive\n\nevent: event\ndata: line1\ndata: line2\n\n";
        let mut buf: Vec<u8> = raw.to_vec();
        let mut msgs = Vec::new();
        while let Some((len, end)) = find_block_end(&buf) {
            let block: Vec<u8> = buf.drain(..end).collect();
            if let Some(m) = parse_block(&block[..len]) {
                msgs.push(m);
            }
        }
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].event, "history");
        assert_eq!(msgs[0].data, "{\"a\":1}");
        assert_eq!(msgs[1].event, "event");
        assert_eq!(msgs[1].data, "line1\nline2");
    }
}
