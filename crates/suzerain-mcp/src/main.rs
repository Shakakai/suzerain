//! suzerain-mcp: MCP server (stdio) exposing the suzerain control plane to
//! LLM operator assistants. See docs/MCP-PLAN.md.
//!
//! IMPORTANT: stdout is the MCP protocol channel — all logging goes to
//! stderr.

mod client;
mod server;

use anyhow::Result;
use clap::Parser;
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(
    name = "suzerain-mcp",
    version,
    about = "MCP server for the suzerain control plane"
)]
struct Cli {
    /// Base URL of the control plane REST API
    #[arg(
        long,
        env = "SUZERAIN_API_URL",
        default_value = "http://127.0.0.1:8484"
    )]
    api_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "suzerain_mcp=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let server = server::SuzerainMcp::new(client::ApiClient::new(cli.api_url.clone()));
    tracing::info!(api_url = %cli.api_url, "serving MCP over stdio");

    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
