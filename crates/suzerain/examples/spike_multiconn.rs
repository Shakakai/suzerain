//! G4 repro: dialing a SECOND connection to the same peer on a different ALPN
//! while the first connection is open. The P0 spike observed this hang in
//! QUIC session resumption (iroh 1.0.3 / noq 1.1.1). This example isolates it
//! and tests candidate mitigations.
//!
//!   cargo run -p suzerain --example spike_multiconn -- <variant>
//!
//! Variants:
//!   baseline        A-then-B on one endpoint (expected: hang — the bug)
//!   no-tickets      client endpoint with max_tls_tickets(0) (no resumption)
//!   wait-long       baseline with a 90s wait (does it eventually connect?)
//!   close-first     close A before dialing B (expected: works)
//!   new-endpoint    dial B from a fresh endpoint (expected: works)

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use iroh::{endpoint::presets, protocol::Router, Endpoint, EndpointId};
use tokio::time::timeout;

const ALPN_A: &[u8] = b"repro/a/0";
const ALPN_B: &[u8] = b"repro/b/0";
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug)]
struct NullHandler;

impl iroh::protocol::ProtocolHandler for NullHandler {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        connection.closed().await;
        Ok(())
    }
}

async fn bind(no_tickets: bool) -> Result<Endpoint> {
    let mut builder = Endpoint::builder(presets::N0);
    if no_tickets {
        builder = builder.max_tls_tickets(0);
    }
    Ok(builder.bind().await?)
}

async fn server() -> Result<(EndpointId, Router)> {
    let endpoint = bind(false).await?;
    let id = endpoint.id();
    let router = Router::builder(endpoint)
        .accept(ALPN_A, NullHandler)
        .accept(ALPN_B, NullHandler)
        .spawn();
    Ok((id, router))
}

async fn try_dial(
    endpoint: &Endpoint,
    id: EndpointId,
    alpn: &'static [u8],
    wait: Duration,
) -> String {
    let start = Instant::now();
    match timeout(wait, endpoint.connect(id, alpn)).await {
        Ok(Ok(conn)) => {
            let alpn_str = String::from_utf8_lossy(alpn);
            // Prove the connection: open a bi stream.
            let opened = timeout(Duration::from_secs(5), conn.open_bi()).await;
            format!(
                "CONNECTED {alpn_str} in {:?} (stream: {})",
                start.elapsed(),
                if opened.is_ok() { "ok" } else { "FAILED" }
            )
        }
        Ok(Err(err)) => format!("FAILED {}: {err}", String::from_utf8_lossy(alpn)),
        Err(_) => format!("TIMEOUT {} after {:?}", String::from_utf8_lossy(alpn), wait),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let variant = std::env::args().nth(1).unwrap_or_else(|| "baseline".into());
    let (server_id, _router) = server().await?;
    println!("server: {server_id}");
    tokio::time::sleep(Duration::from_secs(1)).await;

    match variant.as_str() {
        "server" => {
            // Standalone cross-process server: ONE endpoint hosting gossip +
            // both ALPNs. Prints SERVER_ID and stays alive.
            let endpoint = bind(false).await?;
            let gossip = iroh_gossip::Gossip::builder().spawn(endpoint.clone());
            let _router = Router::builder(endpoint.clone())
                .accept(iroh_gossip::ALPN, gossip.clone())
                .accept(ALPN_A, NullHandler)
                .accept(ALPN_B, NullHandler)
                .spawn();
            let topic = iroh_gossip::TopicId::from_bytes([7u8; 32]);
            let (_tx, mut rx) = gossip.subscribe(topic, vec![]).await?.split();
            tokio::spawn(async move {
                while let Some(event) = n0_future::StreamExt::next(&mut rx).await {
                    if let Ok(iroh_gossip::api::Event::Received(msg)) = event {
                        println!("[gossip] {}", String::from_utf8_lossy(&msg.content));
                    }
                }
            });
            println!("SERVER_ID={}", endpoint.id());
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
        "client" => {
            // Cross-process client: join gossip on the given server, keep
            // traffic flowing, then dial ALPN_B (the spike's failing shape).
            let server_id: EndpointId = std::env::args()
                .nth(2)
                .context("client needs SERVER_ID")?
                .parse()?;
            let client = bind(false).await?;
            let gossip = iroh_gossip::Gossip::builder().spawn(client.clone());
            let _router = Router::builder(client.clone())
                .accept(iroh_gossip::ALPN, gossip.clone())
                .spawn();
            let topic = iroh_gossip::TopicId::from_bytes([7u8; 32]);
            let (tx, _rx) = gossip
                .subscribe_and_join(topic, vec![server_id])
                .await?
                .split();
            println!("gossip joined");
            for i in 0..3 {
                tx.broadcast(format!("hello-{i}").into_bytes().into())
                    .await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            println!(
                "dial B (gossip up + traffic): {}",
                try_dial(&client, server_id, ALPN_B, Duration::from_secs(30)).await
            );
        }
        "gossip-first" => {
            // The exact spike B failure shape: ONE server endpoint hosts
            // gossip AND the second ALPN; the client joins gossip first
            // (long-lived connection), then dials the other ALPN.
            let server_ep2 = bind(false).await?;
            let server_gossip = iroh_gossip::Gossip::builder().spawn(server_ep2.clone());
            let _gossip_router = Router::builder(server_ep2.clone())
                .accept(iroh_gossip::ALPN, server_gossip.clone())
                .accept(ALPN_B, NullHandler)
                .spawn();
            let topic = iroh_gossip::TopicId::from_bytes([7u8; 32]);
            let _server_sub = server_gossip.subscribe(topic, vec![]).await?;
            println!("server(gossip+B): {}", server_ep2.id());

            let client = bind(false).await?;
            let gossip = iroh_gossip::Gossip::builder().spawn(client.clone());
            let _router = Router::builder(client.clone())
                .accept(iroh_gossip::ALPN, gossip.clone())
                .spawn();
            let (_tx, rx) = gossip
                .subscribe_and_join(topic, vec![server_ep2.id()])
                .await?
                .split();
            println!("gossip joined (connection up)");
            tokio::time::sleep(Duration::from_secs(3)).await;
            println!(
                "dial B (gossip up): {}",
                try_dial(&client, server_ep2.id(), ALPN_B, DIAL_TIMEOUT).await
            );
            let _ = rx;
        }
        "baseline" | "wait-long" => {
            let wait = if variant == "wait-long" {
                Duration::from_secs(90)
            } else {
                DIAL_TIMEOUT
            };
            let client = bind(false).await?;
            println!(
                "dial A: {}",
                try_dial(&client, server_id, ALPN_A, DIAL_TIMEOUT).await
            );
            println!(
                "dial B (A held open): {}",
                try_dial(&client, server_id, ALPN_B, wait).await
            );
        }
        "no-tickets" => {
            let client = bind(true).await?;
            println!(
                "dial A: {}",
                try_dial(&client, server_id, ALPN_A, DIAL_TIMEOUT).await
            );
            println!(
                "dial B (A held open, tickets off): {}",
                try_dial(&client, server_id, ALPN_B, DIAL_TIMEOUT).await
            );
        }
        "close-first" => {
            let client = bind(false).await?;
            println!(
                "dial A: {}",
                try_dial(&client, server_id, ALPN_A, DIAL_TIMEOUT).await
            );
            let conn_a = client.connect(server_id, ALPN_A).await.ok();
            if let Some(c) = conn_a {
                c.close(0u32.into(), b"done");
                println!("closed A");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            println!(
                "dial B: {}",
                try_dial(&client, server_id, ALPN_B, DIAL_TIMEOUT).await
            );
        }
        "new-endpoint" => {
            let client = bind(false).await?;
            println!(
                "dial A: {}",
                try_dial(&client, server_id, ALPN_A, DIAL_TIMEOUT).await
            );
            let client2 = bind(false).await?;
            println!(
                "dial B (from a different endpoint): {}",
                try_dial(&client2, server_id, ALPN_B, DIAL_TIMEOUT).await
            );
        }
        other => bail!("unknown variant '{other}'"),
    }
    Ok(())
}
