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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "suzerain=info".into()),
        )
        .init();

    match Cli::parse().command {
        Commands::Id => {
            let key = suzerain::identity::load_or_create_secret_key()?;
            println!("{}", key.public());
            Ok(())
        }
        Commands::Run => {
            let store = suzerain::store::Store::open().await?;
            let cp = Arc::new(suzerain::control::start(store).await?);
            println!("suzerain endpoint id: {}", cp.endpoint_id());
            suzerain::api::serve(cp).await
        }
    }
}
