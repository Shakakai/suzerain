//! Integration test: the real control plane's iroh operator channel
//! (`suz/operator/0`), no VM required. Exercises what the Suzy UI tests
//! can't (their mock reimplements the protocol): the real accept loop,
//! the `[operator] allow` authorization, ALPN registration, rest ops
//! against the real router, stream ops (fleet events), and the shell op's
//! early-error path.
//!
//! Single test function on purpose: it owns SUZERAIN_HOME (process-global
//! env) for the lifetime of the test binary.

use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use serde_json::{json, Value};
use suzerain_protocol::control::{OperatorFrame, OperatorHello, ShellMessage};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use tokio::io::BufReader;

async fn dial(endpoint: &Endpoint, addr: iroh::EndpointAddr) -> iroh::endpoint::Connection {
    endpoint
        .connect(addr, suzerain_protocol::alpn::OPERATOR)
        .await
        .expect("connect operator channel")
}

async fn rest(
    conn: &iroh::endpoint::Connection,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let (mut send, recv) = conn.open_bi().await.expect("open_bi");
    write_jsonl(
        &mut send,
        &OperatorHello::Rest {
            method: method.into(),
            path: path.into(),
            body,
        },
    )
    .await
    .expect("write hello");
    let mut recv = BufReader::new(recv);
    let frame: OperatorFrame = read_jsonl(&mut recv).await.expect("read reply");
    match frame {
        OperatorFrame::Reply { status, body } => (status, body),
        other => panic!("expected Reply, got {other:?}"),
    }
}

#[tokio::test]
async fn operator_channel_end_to_end() {
    let tmp = std::env::temp_dir().join(format!("suz-optest-{}", uuid::Uuid::new_v4()));
    // SAFETY: this is the only test in this binary; nothing else reads the
    // env concurrently.
    unsafe { std::env::set_var("SUZERAIN_HOME", &tmp) };

    let allowed_key = SecretKey::generate();
    let rogue_key = SecretKey::generate();

    let store = suzerain::store::Store::open().await.expect("store");
    let cp = suzerain::control::start(store, vec![allowed_key.public()])
        .await
        .expect("control plane");
    let addr = cp.addr();

    // ── unauthorized operator is rejected ──
    let rogue = Endpoint::builder(presets::N0)
        .secret_key(rogue_key.clone())
        .bind()
        .await
        .expect("rogue endpoint");
    let rogue_conn = dial(&rogue, addr.clone()).await;
    // The handshake completes (ALPN matches); the handler then closes the
    // connection, so using it must fail promptly.
    let rogue_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let stream = rogue_conn.open_bi().await;
        match stream {
            Err(e) => Err(format!("open_bi: {e}")),
            Ok((mut send, recv)) => {
                let w = write_jsonl(
                    &mut send,
                    &OperatorHello::Rest {
                        method: "GET".into(),
                        path: "/api/v1/endpoint".into(),
                        body: None,
                    },
                )
                .await;
                if let Err(e) = w {
                    return Err(format!("write: {e}"));
                }
                let mut recv = BufReader::new(recv);
                match read_jsonl::<_, OperatorFrame>(&mut recv).await {
                    Ok(frame) => Ok(frame),
                    Err(e) => Err(format!("read: {e}")),
                }
            }
        }
    })
    .await
    .expect("rogue attempt timed out");
    assert!(
        rogue_result.is_err(),
        "rogue operator got a reply: {rogue_result:?}"
    );

    // ── live approval: add_operator_allow takes effect without restart ──
    cp.add_operator_allow(rogue_key.public());
    assert!(
        cp.operator_allow().contains(&rogue_key.public()),
        "live allow set should include the newly approved id"
    );
    let rogue_upgrade = Endpoint::builder(presets::N0)
        .secret_key(rogue_key.clone())
        .bind()
        .await
        .expect("rogue endpoint re-bind");
    let rogue_conn = dial(&rogue_upgrade, addr.clone()).await;
    let (status, _) = rest(&rogue_conn, "GET", "/api/v1/endpoint", None).await;
    assert_eq!(status, 200, "freshly approved operator was rejected");

    // ── authorized operator: rest op against the real router ──
    let client = Endpoint::builder(presets::N0)
        .secret_key(allowed_key)
        .bind()
        .await
        .expect("client endpoint");
    let conn = dial(&client, addr).await;

    let (status, body) = rest(&conn, "GET", "/api/v1/endpoint", None).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["endpoint_id"].as_str().unwrap(),
        cp.endpoint_id().to_string()
    );

    // Validation errors propagate with their status (422 here).
    let (status, body) = rest(
        &conn,
        "POST",
        "/api/v1/agents",
        Some(json!({"manifest_toml": "name = 42"})), // invalid manifest
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(body["error"].as_str().unwrap_or("").contains("manifest"));

    // ── stream op: fleet events flow after a mutation ──
    let (mut ev_send, ev_recv) = conn.open_bi().await.expect("events stream");
    write_jsonl(
        &mut ev_send,
        &OperatorHello::Stream {
            path: "/api/v1/events".into(),
        },
    )
    .await
    .expect("events hello");
    let mut ev_recv = BufReader::new(ev_recv);

    // Cause an audited mutation: approving a (synthetic) daemon id.
    let fake_daemon = SecretKey::generate().public().to_string();
    let (status, _) = rest(
        &conn,
        "POST",
        "/api/v1/daemons/approve",
        Some(json!({"endpoint_id": fake_daemon})),
    )
    .await;
    assert_eq!(status, 200);

    let event_text = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let frame: OperatorFrame = read_jsonl(&mut ev_recv).await.expect("event frame");
            if let OperatorFrame::Chunk { data } = frame {
                let bytes = base64_decode(&data);
                let text = String::from_utf8_lossy(&bytes).to_string();
                if text.contains("\"kind\"") {
                    return text;
                }
            }
        }
    })
    .await
    .expect("no fleet event within 10s");
    assert!(
        event_text.contains("audit") || event_text.contains("daemon"),
        "unexpected event content: {event_text}"
    );

    // ── shell op: graceful error for a nonexistent agent ──
    let (mut sh_send, sh_recv) = conn.open_bi().await.expect("shell stream");
    write_jsonl(
        &mut sh_send,
        &OperatorHello::Shell {
            name: "nope".into(),
        },
    )
    .await
    .expect("shell hello");
    let mut sh_recv = BufReader::new(sh_recv);
    let msg: ShellMessage = read_jsonl(&mut sh_recv).await.expect("shell reply");
    match msg {
        ShellMessage::Notice { message } => {
            assert!(message.contains("no agent"), "unexpected notice: {message}");
        }
        other => panic!("expected Notice, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Local base64 decode (avoid pulling a dev-dep for one helper).
fn base64_decode(text: &str) -> Vec<u8> {
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
    let mut out = Vec::new();
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
    out
}
