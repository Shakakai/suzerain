//! Phase 0 spike (b): prove the iroh fabric between a "control plane" node and
//! a "daemon" node.
//!
//!   terminal 1: cargo run -p suzerain --example spike_iroh -- control
//!   terminal 2: cargo run -p suzerain --example spike_iroh -- daemon <CONTROL_ENDPOINT_ID>
//!
//! Validates: dialing by public key (EndpointId), mDNS LAN discovery
//! (+ n0 relay/pkarr fallback via presets::N0), ALPN protocol routing via
//! Router, request/response order-ack over a bi-stream using the shared
//! protocol types, and iroh-gossip pub/sub on the fleet topic.
//!
//! Findings baked into the design (docs/PLAN.md §2):
//! - The daemon must establish its control connection FIRST and keep it
//!   long-lived; joining gossip after works. The reverse order (gossip link
//!   up, then dialing a second connection with a different ALPN) was observed
//!   to hang in QUIC session resumption (iroh 1.0.3/noq). Order your connects.
//! - Accept handlers must `connection.closed().await` after `send.finish()`;
//!   returning early can discard unflushed stream data.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use iroh::{endpoint::presets, protocol::Router, Endpoint, EndpointId};
use iroh_gossip::{api::Event, Gossip, TopicId};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use n0_future::StreamExt;
use suzerain_protocol::alpn;
use suzerain_protocol::order::{Order, OrderAck};
use tokio::io::BufReader;
use tokio::time::timeout;

const SPIKE_TIMEOUT: Duration = Duration::from_secs(60);

fn topic() -> TopicId {
    TopicId::from_bytes(alpn::FLEET_TOPIC)
}

async fn bind_endpoint() -> Result<Endpoint> {
    let endpoint = Endpoint::bind(presets::N0).await.context("bind endpoint")?;
    // LAN discovery so two local machines find each other with zero config.
    let mdns = MdnsAddressLookup::builder().build(endpoint.id())?;
    endpoint.address_lookup()?.add(mdns);
    Ok(endpoint)
}

/// Control-plane side: accepts orders on `suz/control/0` and listens on the
/// fleet gossip topic.
#[derive(Debug)]
struct ControlHandler;

impl iroh::protocol::ProtocolHandler for ControlHandler {
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        let (mut send, recv) = connection.accept_bi().await?;
        let mut recv = BufReader::new(recv);
        let order: Order = suzerain_protocol::framing::read_jsonl(&mut recv)
            .await
            .map_err(iroh::protocol::AcceptError::from_err)?;
        println!("  [control] received order: {order:?}");
        let ack = OrderAck {
            success: true,
            message: Some("ack from control".into()),
        };
        suzerain_protocol::framing::write_jsonl(&mut send, &ack)
            .await
            .map_err(iroh::protocol::AcceptError::from_err)?;
        send.finish()?;
        // Wait for the peer to close so the ack is actually delivered before
        // this connection is dropped (returning early can discard it).
        connection.closed().await;
        Ok(())
    }
}

async fn run_control() -> Result<()> {
    let endpoint = bind_endpoint().await?;
    let id = endpoint.id();
    println!("CONTROL_ENDPOINT_ID={id}");

    let gossip = Gossip::builder().spawn(endpoint.clone());
    let router = Router::builder(endpoint.clone())
        .accept(alpn::CONTROL, ControlHandler)
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();

    // Join the fleet topic with no bootstrap peers; daemons join us.
    let (_sender, mut receiver) = gossip.subscribe_and_join(topic(), vec![]).await?.split();
    println!("listening for orders (alpn suz/control/0) and fleet gossip…");

    timeout(SPIKE_TIMEOUT, async {
        while let Some(event) = receiver.next().await {
            match event? {
                Event::Received(msg) => {
                    println!(
                        "  [gossip] received: {}",
                        String::from_utf8_lossy(&msg.content)
                    );
                    return anyhow::Ok(());
                }
                Event::NeighborUp(peer) => println!("  [gossip] neighbor up: {peer}"),
                _ => {}
            }
        }
        anyhow::Ok(())
    })
    .await
    .context("timed out waiting for gossip")??;

    // Give the control order a moment to arrive too, then shut down cleanly.
    tokio::time::sleep(Duration::from_secs(3)).await;
    router.shutdown().await?;
    println!("control spike ok");
    Ok(())
}

async fn run_daemon(control_id: &str) -> Result<()> {
    let control_id: EndpointId = control_id.parse().context("parsing control endpoint id")?;
    let endpoint = bind_endpoint().await?;
    let my_id = endpoint.id();
    println!("daemon endpoint id: {my_id}");

    let gossip = Gossip::builder().spawn(endpoint.clone());
    let _router = Router::builder(endpoint.clone())
        .accept(iroh_gossip::ALPN, gossip.clone())
        .spawn();

    timeout(SPIKE_TIMEOUT, async {
        // 1. control order first: direct connection, one order, one ack.
        println!("  [control] connecting to {control_id}…");
        let conn = timeout(
            Duration::from_secs(20),
            endpoint.connect(control_id, alpn::CONTROL),
        )
        .await
        .context("connect timed out (discovery didn't resolve control addr)")??;
        println!("  [control] connected, sending order…");
        let (mut send, recv) = conn.open_bi().await?;
        let mut recv = BufReader::new(recv);
        suzerain_protocol::framing::write_jsonl(&mut send, &Order::Ping { nonce: 42 }).await?;
        let ack: OrderAck = suzerain_protocol::framing::read_jsonl(&mut recv).await?;
        println!("  [control] ack: {ack:?}");
        anyhow::ensure!(ack.success, "order was nacked");
        // Deliberately left open: the control link is long-lived in the real
        // design, and this proves gossip works while it is held open.
        let _control_conn = conn;

        // 2. gossip: join the fleet topic via the control node and announce.
        let (sender, mut receiver) = gossip
            .subscribe_and_join(topic(), vec![control_id])
            .await?
            .split();
        sender
            .broadcast(format!("daemon-online:{my_id}").into_bytes().into())
            .await?;
        println!("  [gossip] announced on fleet topic");
        // Note any swarm event (neighbor up / echo) to confirm membership, but
        // don't block on it: our own broadcast is not delivered back to us.
        let _ = timeout(Duration::from_secs(10), async {
            while let Some(event) = receiver.next().await {
                match event? {
                    Event::NeighborUp(peer) => {
                        println!("  [gossip] neighbor up: {peer}");
                        return anyhow::Ok(());
                    }
                    Event::Received(msg) => {
                        println!(
                            "  [gossip] received: {}",
                            String::from_utf8_lossy(&msg.content)
                        );
                        return anyhow::Ok(());
                    }
                    _ => {}
                }
            }
            anyhow::Ok(())
        })
        .await;
        anyhow::Ok(())
    })
    .await
    .context("daemon spike timed out")??;

    endpoint.close().await;
    println!("daemon spike ok");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [mode] if mode == "control" => run_control().await,
        [mode, id] if mode == "daemon" => run_daemon(id).await,
        _ => {
            eprintln!("usage:\n  spike_iroh control\n  spike_iroh daemon <CONTROL_ENDPOINT_ID>");
            bail!("bad args");
        }
    }
}
