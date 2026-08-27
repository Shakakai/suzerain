//! Integration test: `ControlPlane::order` no longer serializes all orders
//! to a daemon behind one persistent stream. A fake "castellan" (a bare
//! iroh endpoint speaking just enough of the control protocol — Register,
//! then per-order streams) lets this run without a real VM/Gondolin.
//!
//! Regression target: before the per-order-stream change, `order()` wrote
//! and read every order on one shared register stream while holding both
//! its send and recv locks for the full round trip, so a slow order for
//! one agent (e.g. a long provision) blocked the ack for any other order
//! to the same daemon — including unrelated Stop/Suspend/Destroy calls —
//! behind it. This test proves a fast order completes promptly even while
//! a slow one to the same daemon is still in flight.

use std::sync::Arc;
use std::time::{Duration, Instant};

use iroh::endpoint::presets;
use iroh::{Endpoint, SecretKey};
use suzerain_protocol::alpn;
use suzerain_protocol::control::{Register, RegisterResponse, StreamHello};
use suzerain_protocol::framing::{read_jsonl, write_jsonl};
use suzerain_protocol::order::{Order, OrderAck};
use tokio::io::BufReader;

/// A nonce marking the order the fake daemon should sit on for a while
/// before acking, to stand in for a slow real-world operation (e.g. a
/// multi-minute provision).
const SLOW_NONCE: u64 = 999;
const SLOW_DELAY: Duration = Duration::from_secs(3);

#[tokio::test]
async fn fast_order_is_not_blocked_by_a_slow_order_on_the_same_daemon() {
    let tmp = std::env::temp_dir().join(format!("suz-orderdispatch-{}", uuid::Uuid::new_v4()));
    // SAFETY: this is the only test in this binary; nothing else reads the
    // env concurrently.
    unsafe { std::env::set_var("SUZERAIN_HOME", &tmp) };

    let store = suzerain::store::Store::open().await.expect("store");
    let cp = suzerain::control::start(store.clone(), vec![])
        .await
        .expect("control plane");
    let cp_addr = cp.addr();

    // ── fake daemon: connect, register, then answer orders on their own
    // streams (per-order, matching what a real castellan now does). ──
    let daemon_key = SecretKey::generate();
    let daemon_id = daemon_key.public();
    store
        .approve_daemon(&daemon_id.to_string())
        .await
        .expect("approve daemon");

    let daemon_endpoint = Endpoint::builder(presets::N0)
        .secret_key(daemon_key)
        .bind()
        .await
        .expect("daemon endpoint");
    let conn = daemon_endpoint
        .connect(cp_addr, alpn::CONTROL)
        .await
        .expect("daemon connect");

    let (mut reg_tx, reg_rx) = conn.open_bi().await.expect("register stream");
    let info = suzerain_protocol::state::DaemonInfo {
        endpoint_id: daemon_id.to_string(),
        hostname: "fake-daemon".into(),
        os: "test".into(),
        arch: "test".into(),
        labels: Default::default(),
        max_agents: 4,
        agents: vec![],
        capacity: Default::default(),
        usage: Default::default(),
    };
    write_jsonl(
        &mut reg_tx,
        &Register {
            info,
            protocol_version: suzerain_protocol::control::PROTOCOL_VERSION,
        },
    )
    .await
    .expect("write register");
    let mut reg_rx = BufReader::new(reg_rx);
    let response: RegisterResponse = read_jsonl(&mut reg_rx).await.expect("register response");
    assert!(response.accepted, "daemon registration was rejected");
    let _ = reg_tx.finish();
    drop(reg_rx);

    // Give suzerain a moment to install the session before we send orders.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let accept_conn = conn.clone();
    tokio::spawn(async move {
        while let Ok((mut send, recv)) = accept_conn.accept_bi().await {
            tokio::spawn(async move {
                let mut recv = BufReader::new(recv);
                let hello: StreamHello = match read_jsonl(&mut recv).await {
                    Ok(h) => h,
                    Err(_) => return,
                };
                assert!(
                    matches!(hello, StreamHello::Order),
                    "expected an Order stream, got {hello:?}"
                );
                let order: Order = read_jsonl(&mut recv).await.expect("read order");
                if let Order::Ping { nonce } = order {
                    if nonce == SLOW_NONCE {
                        tokio::time::sleep(SLOW_DELAY).await;
                    }
                }
                write_jsonl(
                    &mut send,
                    &OrderAck {
                        success: true,
                        message: None,
                        data: None,
                    },
                )
                .await
                .expect("write ack");
                let _ = send.finish();
            });
        }
    });

    // ── the actual regression check ──
    let start = Instant::now();
    let cp_slow = Arc::new(cp);
    let cp_for_slow = cp_slow.clone();
    let slow = tokio::spawn(async move {
        cp_for_slow
            .order(&daemon_id, &Order::Ping { nonce: SLOW_NONCE })
            .await
    });
    // Let the slow order actually start before firing the fast one.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let fast_ack = tokio::time::timeout(
        Duration::from_secs(2),
        cp_slow.order(&daemon_id, &Order::Ping { nonce: 1 }),
    )
    .await
    .expect("fast order timed out — it was blocked behind the slow one")
    .expect("fast order failed");
    assert!(fast_ack.success);
    assert!(
        start.elapsed() < SLOW_DELAY,
        "fast order took {:?}, as long as the slow order — they were serialized",
        start.elapsed()
    );

    let slow_ack = slow
        .await
        .expect("slow order task panicked")
        .expect("slow order failed");
    assert!(slow_ack.success);
    assert!(
        start.elapsed() >= SLOW_DELAY,
        "slow order returned suspiciously fast: {:?}",
        start.elapsed()
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
