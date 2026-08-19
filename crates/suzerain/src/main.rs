//! suzerain: the control plane.
//!
//!   suzerain run    — foreground control plane (iroh + operator socket)
//!   suzerain id     — print this node's EndpointId

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "suzerain", version, about = "Suzerain control plane")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the control plane in the foreground
    Run,
    /// Print this node's iroh EndpointId
    Id,
}

#[tokio::main]
async fn main() -> Result<()> {
    suzerain_protocol::telemetry::init("suzerain=info", "suzerain")?;

    match Cli::parse().command {
        Commands::Id => {
            let key = suzerain::identity::load_or_create_secret_key()?;
            println!("{}", key.public());
            Ok(())
        }
        Commands::Run => {
            suzerain::secrets::load()?;
            let store = suzerain::store::Store::open().await?;
            tokio::spawn(suzerain::retention::run());
            let config = suzerain::retention::load_config()?;
            let operator_allow: Vec<iroh::EndpointId> = config
                .operator
                .allow
                .iter()
                .filter_map(|s| match s.parse() {
                    Ok(id) => Some(id),
                    Err(_) => {
                        tracing::warn!(
                            "[operator] allow entry '{s}' is not a valid EndpointId — ignored"
                        );
                        None
                    }
                })
                .collect();
            let cp = Arc::new(suzerain::control::start(store.clone(), operator_allow).await?);
            println!("suzerain endpoint id: {}", cp.endpoint_id());
            // Auto-suspend sweep (single authority for lifecycle decisions).
            tokio::spawn(suzerain::lifecycle::run(Arc::clone(&cp)));
            // Resume wakes interrupted by a restart (durable queue).
            let wake_cp = Arc::clone(&cp);
            tokio::spawn(async move {
                suzerain::wake::resume_pending(&wake_cp).await;
            });
            if config.web.enabled {
                let web_cp = Arc::clone(&cp);
                tokio::spawn(async move {
                    if let Err(err) = suzerain::web::serve(store, web_cp, config.web.port).await {
                        tracing::warn!("web ui exited: {err:#}");
                    }
                });
            }
            suzerain::api::serve(cp).await
        }
    }
}
