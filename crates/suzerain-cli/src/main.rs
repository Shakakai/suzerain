use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "suz",
    version,
    about = "Operator CLI for the suzerain control plane"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage castellan daemons
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
    /// Manage agents
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Approve a daemon's EndpointId (enrollment)
    Approve { endpoint_id: String },
    /// List known daemons
    List,
}

#[derive(Subcommand)]
enum AgentCommands {
    /// Create an agent from a manifest file
    Create {
        /// Path to the agent manifest TOML
        #[arg(long)]
        manifest: String,
    },
    /// List agents and their states
    List,
    /// Attach to an agent session (history + live stream)
    Attach { name: String },
    /// Start a suspended/stopped agent
    Start { name: String },
    /// Gracefully stop an agent
    Stop { name: String },
    /// Suspend an agent (snapshot for later boot)
    Suspend { name: String },
    /// Permanently destroy an agent
    Destroy { name: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let cli = Cli::parse();
    // TODO(phase-2): connect to suzerain over iroh and dispatch.
    println!("scaffold only — not yet implemented: {cli:?}");
    Ok(())
}

impl std::fmt::Debug for Cli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.command {
            Commands::Daemon { .. } => write!(f, "daemon …"),
            Commands::Agent { .. } => write!(f, "agent …"),
        }
    }
}
