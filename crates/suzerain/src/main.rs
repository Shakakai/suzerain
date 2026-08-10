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
            let cp = Arc::new(suzerain::control::start(store).await?);
            println!("suzerain endpoint id: {}", cp.endpoint_id());
            suzerain::api::serve(cp).await
        }
    }
}
