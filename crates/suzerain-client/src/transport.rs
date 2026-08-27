//! Transport abstraction (docs/UNIFIED-AGENT-API-DESIGN.md §6 step 3): the
//! ~40 typed methods on [`crate::Client`] only ever call two primitives —
//! `rest` (one request, one JSON reply) and `sse` (a server-sent-event
//! stream) — so those two are the whole seam between "what a call means"
//! and "how it physically reaches the control plane". `Client` itself
//! never changes based on which `Transport` backs it.
//!
//! Two implementations:
//! - [`IrohTransport`]: today's only transport, unchanged in behavior —
//!   the iroh operator channel (`suz/operator/0`), used by Suzy.
//! - [`HttpTransport`]: direct HTTP against the control plane's REST API
//!   (`/api/v1/...`, same router the iroh transport tunnels to
//!   in-process) — used by `suz` and `suzerain-mcp`, both local-only
//!   callers with no need for iroh's remote reachability.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use serde_json::Value;
use tokio::io::BufReader;
use tokio::sync::{Mutex, OnceCell};

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};

use suzerain_protocol::control::{OperatorFrame, OperatorHello};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};

use crate::{b64_decode, channel_err, chunks_to_sse, Error, Result, ShellConn, SseMessage};

/// Request timeout for one-shot `rest()` calls over HTTP. SSE streams
/// (`sse()`) intentionally have no overall timeout — they're meant to live
/// indefinitely — see [`HttpTransport::new`].
const REST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How long to wait for the initial TCP/TLS connect before giving up. Applied
/// to both the `rest` and `sse` clients so a dead/unreachable server fails
/// fast even on the (otherwise timeout-less) streaming path.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Classify a `reqwest::Error` as a timeout (connect or read/write deadline
/// exceeded) vs. any other transport failure, so callers can tell a slow or
/// unreachable server apart from other kinds of channel errors.
fn map_reqwest_err(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Timeout(format!("{e:#}"))
    } else {
        channel_err(e)
    }
}

pub(crate) type SseStream = Pin<Box<dyn Stream<Item = Result<SseMessage>> + Send>>;
type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>;

/// The seam between `Client`'s typed methods and however bytes actually
/// move. `local_id`/`shell_connect` are iroh-specific extras with a
/// default "not supported" implementation for other transports — every
/// other method on `Client` is transport-agnostic.
#[async_trait]
pub(crate) trait Transport: Send + Sync {
    async fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value>;
    async fn sse(&self, path: &str) -> Result<SseStream>;

    fn local_id(&self) -> Option<String> {
        None
    }

    async fn shell_connect(&self, _name: &str) -> Result<ShellConn> {
        Err(Error::Channel(
            "shell is only available over the iroh operator channel".into(),
        ))
    }
}

// ── iroh operator channel (unchanged behavior, moved from lib.rs) ─────────

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

pub(crate) struct IrohTransport {
    key: SecretKey,
    remote: RemoteTarget,
    full_discovery: bool,
    endpoint: OnceCell<Endpoint>,
    conn: Mutex<Option<iroh::endpoint::Connection>>,
}

impl IrohTransport {
    pub(crate) fn new(remote_endpoint_id: &str, key: SecretKey) -> Self {
        Self {
            key,
            remote: RemoteTarget::Id(remote_endpoint_id.to_string()),
            full_discovery: true,
            endpoint: OnceCell::new(),
            conn: Mutex::new(None),
        }
    }

    pub(crate) fn with_addr(addr: EndpointAddr, key: SecretKey) -> Self {
        Self {
            key,
            remote: RemoteTarget::Addr(addr),
            full_discovery: false,
            endpoint: OnceCell::new(),
            conn: Mutex::new(None),
        }
    }

    async fn iroh_endpoint(&self) -> Result<&Endpoint> {
        self.endpoint
            .get_or_try_init(|| async {
                let mut builder = if self.full_discovery {
                    Endpoint::builder(presets::N0)
                } else {
                    Endpoint::builder(presets::Empty)
                };
                builder = builder
                    .secret_key(self.key.clone())
                    // Explicit provider: presets::Empty sets none, and the
                    // process default is ambiguous when several are linked.
                    .crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()));
                builder.bind().await.map_err(channel_err)
            })
            .await
    }

    async fn connection(&self) -> Result<iroh::endpoint::Connection> {
        {
            let guard = self.conn.lock().await;
            if let Some(conn) = guard.as_ref() {
                if conn.close_reason().is_none() {
                    return Ok(conn.clone());
                }
            }
        }
        let endpoint = self.iroh_endpoint().await?;
        let remote = self.remote.resolve()?;
        let conn = endpoint
            .connect(remote, suzerain_protocol::alpn::OPERATOR)
            .await
            .map_err(channel_err)?;
        *self.conn.lock().await = Some(conn.clone());
        Ok(conn)
    }

    async fn disconnect(&self) {
        *self.conn.lock().await = None;
    }

    /// Runs one request. The `bool` alongside an `Err` says whether the
    /// request bytes were already handed to the connection (`write_jsonl`
    /// returned `Ok`) before the failure happened — i.e. whether the server
    /// may have already acted on it, even though we never saw the reply.
    async fn rest_once(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> std::result::Result<Value, (Error, bool)> {
        let conn = self.connection().await.map_err(|e| (e, false))?;
        let (mut send, recv) = conn.open_bi().await.map_err(|e| (channel_err(e), false))?;
        let hello = OperatorHello::Rest {
            method: method.to_string(),
            path: path.to_string(),
            body,
        };
        write_jsonl(&mut send, &hello)
            .await
            .map_err(|e| (channel_err(e), false))?;
        // From here on the server may have already received and processed
        // the request, so any failure below must be reported as "sent".
        let mut recv = BufReader::new(recv);
        let frame: OperatorFrame = read_jsonl(&mut recv)
            .await
            .map_err(|e| (channel_err(e), true))?;
        match frame {
            OperatorFrame::Reply { status, body } => {
                if (200..300).contains(&status) {
                    Ok(body)
                } else {
                    let msg = body["error"]
                        .as_str()
                        .unwrap_or("request failed")
                        .to_string();
                    Err((Error::Http(status, msg), true))
                }
            }
            OperatorFrame::Error { message } => Err((Error::Op(message), true)),
            other => Err((Error::Channel(format!("unexpected frame: {other:?}")), true)),
        }
    }

    async fn stream_bytes(&self, path: &str) -> Result<ByteStream> {
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
}

#[async_trait]
impl Transport for IrohTransport {
    async fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        match self.rest_once(method, path, body.clone()).await {
            Ok(v) => Ok(v),
            Err((e, sent)) => {
                self.disconnect().await;
                // A retry is only safe when we know the request never
                // reached the server (the connection died before the bytes
                // went out), or when re-sending it can't cause a duplicate
                // effect (GET is read-only). Once the request has been sent
                // for a write like create_agent/prompt/set_secret_*, the
                // server may already have acted on it even though we never
                // saw the reply — retrying blind there risks doing it
                // twice, so we surface the original error instead.
                if !safe_to_retry(method, sent) {
                    return Err(e);
                }
                self.rest_once(method, path, body).await.map_err(|(e, _)| e)
            }
        }
    }

    async fn sse(&self, path: &str) -> Result<SseStream> {
        let chunks = self.stream_bytes(path).await?;
        Ok(Box::pin(chunks_to_sse(chunks)))
    }

    fn local_id(&self) -> Option<String> {
        Some(self.key.public().to_string())
    }

    async fn shell_connect(&self, name: &str) -> Result<ShellConn> {
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
        Ok(ShellConn::new(send, BufReader::new(recv)))
    }
}

// ── direct HTTP against the REST API ──────────────────────────────────────

/// Talks straight to `/api/v1/...` — the same router the iroh transport
/// reaches in-process via `OperatorHello::Rest`. Used by local-only callers
/// (`suz`, `suzerain-mcp`) that don't need iroh's remote reachability.
pub(crate) struct HttpTransport {
    base: String,
    /// Backs one-shot `rest()` calls: bounded by [`REST_TIMEOUT`] end to end.
    rest_http: reqwest::Client,
    /// Backs `sse()`: no overall timeout (streams live indefinitely), but
    /// still bounded on the initial connect via [`CONNECT_TIMEOUT`].
    sse_http: reqwest::Client,
}

impl HttpTransport {
    pub(crate) fn new(base_url: &str) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            rest_http: reqwest::Client::builder()
                .timeout(REST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .expect("reqwest client builds"),
            sse_http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .expect("reqwest client builds"),
        }
    }

    /// Runs one REST call. The `bool` alongside an `Err` says whether the
    /// request may have reached the server (a connect failure means it
    /// definitely didn't; anything else — timeout, body error, non-2xx —
    /// means it might have), mirroring `IrohTransport::rest_once`.
    async fn rest_once(
        &self,
        method: &reqwest::Method,
        path: &str,
        body: &Option<Value>,
    ) -> std::result::Result<Value, (Error, bool)> {
        let mut req = self
            .rest_http
            .request(method.clone(), format!("{}{path}", self.base));
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| {
            let sent = !e.is_connect();
            (map_reqwest_err(e), sent)
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| (map_reqwest_err(e), true))?;
        if status.is_success() {
            Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
        } else {
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{status}: {text}"));
            Err((Error::Http(status.as_u16(), msg), true))
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn rest(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        let method: reqwest::Method = method
            .parse()
            .map_err(|_| Error::Channel(format!("invalid HTTP method '{method}'")))?;
        match self.rest_once(&method, path, &body).await {
            Ok(v) => Ok(v),
            Err((e, sent)) => {
                if !safe_to_retry(method.as_str(), sent) {
                    return Err(e);
                }
                self.rest_once(&method, path, &body)
                    .await
                    .map_err(|(e, _)| e)
            }
        }
    }

    async fn sse(&self, path: &str) -> Result<SseStream> {
        use futures_util::TryStreamExt;
        let resp = self
            .sse_http
            .get(format!("{}{path}", self.base))
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(map_reqwest_err)?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Http(status.as_u16(), text));
        }
        let chunks: ByteStream = Box::pin(
            resp.bytes_stream()
                .map_ok(|b| b.to_vec())
                .map_err(map_reqwest_err),
        );
        Ok(Box::pin(chunks_to_sse(chunks)))
    }
}

pub(crate) type DynTransport = Arc<dyn Transport>;

/// Whether a `rest()` retry is safe: only when the request never reached
/// the server (`!sent`), or when re-sending can't cause a duplicate effect
/// (a `GET` is read-only). Once a write like create_agent/prompt/
/// set_secret_* has been sent, the server may have already acted on it even
/// though we never saw the reply, so blindly retrying risks doing it twice.
fn safe_to_retry(method: &str, sent: bool) -> bool {
    !sent || method.eq_ignore_ascii_case("GET")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_when_request_never_sent() {
        assert!(safe_to_retry("POST", false));
        assert!(safe_to_retry("PUT", false));
        assert!(safe_to_retry("DELETE", false));
        assert!(safe_to_retry("GET", false));
    }

    #[test]
    fn retries_get_even_if_sent() {
        assert!(safe_to_retry("GET", true));
        assert!(safe_to_retry("get", true));
    }

    #[test]
    fn refuses_to_retry_sent_writes() {
        assert!(!safe_to_retry("POST", true));
        assert!(!safe_to_retry("PUT", true));
        assert!(!safe_to_retry("DELETE", true));
        assert!(!safe_to_retry("PATCH", true));
    }

    /// A real read-timeout: bind a listener, accept the connection and hold
    /// it open without ever writing a response, then make a request with a
    /// short `.timeout()` against it. Reproduces the same `is_timeout()`
    /// condition a dead-server / hung-response SSE or REST call would hit,
    /// without waiting anywhere near the real 120s REST_TIMEOUT.
    #[tokio::test]
    async fn map_reqwest_err_classifies_read_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            // Accept and hold the connection open; never respond.
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            drop(stream);
        });

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(100))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("request should time out waiting for a response");

        let mapped = map_reqwest_err(err);
        assert!(
            matches!(mapped, Error::Timeout(_)),
            "expected Error::Timeout, got {mapped:?}"
        );

        server.abort();
    }

    /// `HttpTransport::rest()` retries a GET once when the first attempt
    /// fails after the request was already sent (here: the server accepts
    /// the connection and then closes it without responding) — same policy
    /// `IrohTransport::rest()` already has, now exercised end-to-end over a
    /// real TCP server.
    #[tokio::test]
    async fn http_transport_retries_get_after_dropped_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            // First connection: accept, then close without responding.
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            drop(stream);

            // Second connection: respond with a minimal valid JSON body.
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let body = b"{\"ok\":true}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.shutdown().await;
        });

        let transport = HttpTransport::new(&format!("http://{addr}"));
        let result = transport.rest("GET", "/anything", None).await;
        let value = result.expect("GET should succeed after one retry");
        assert_eq!(value["ok"], true);
    }
}
