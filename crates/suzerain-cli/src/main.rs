use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

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
    /// Print the control plane's EndpointId
    Id,
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
        /// Pin to a specific daemon (endpoint-id prefix or hostname)
        #[arg(long)]
        daemon: Option<String>,
    },
    /// List agents and their states
    List,
    /// Send a prompt and print the final answer
    Ask { name: String, message: Vec<String> },
    /// Start a suspended agent
    Start { name: String },
    /// Gracefully stop an agent
    Stop { name: String },
    /// Suspend an agent (snapshot for later boot)
    Suspend { name: String },
    /// Permanently destroy an agent
    Destroy { name: String },
    /// Show an agent's centrally stored event log
    Logs {
        name: String,
        #[arg(long, default_value = "50")]
        tail: usize,
    },
}

fn socket() -> std::path::PathBuf {
    std::env::var("SUZERAIN_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            std::path::PathBuf::from(home).join(".local/share/suzerain")
        })
        .join("suzerain.sock")
}

async fn request(cmd: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket())
        .await
        .context("connecting to suzerain (is `suzerain run` up?)")?;
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(&cmd)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    let mut lines = BufReader::new(reader).lines();
    let reply = lines.next_line().await?.context("no reply")?;
    let reply: Value = serde_json::from_str(&reply)?;
    if reply["ok"].as_bool() == Some(true) {
        Ok(reply["result"].clone())
    } else {
        bail!("{}", reply["error"].as_str().unwrap_or("unknown error"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Id => {
            let r = request(json!({"id": 1, "cmd": "endpoint_id"})).await?;
            println!("{}", r["endpoint_id"].as_str().unwrap_or("?"));
        }
        Commands::Daemon { command } => match command {
            DaemonCommands::Approve { endpoint_id } => {
                request(json!({"id": 1, "cmd": "daemon_approve", "endpoint_id": endpoint_id}))
                    .await?;
                println!("approved {endpoint_id}");
            }
            DaemonCommands::List => {
                let r = request(json!({"id": 1, "cmd": "daemon_list"})).await?;
                for d in r.as_array().into_iter().flatten() {
                    println!(
                        "{:<10} {:<8} {:<8} {:<20} {}/{}",
                        &d["endpoint_id"].as_str().unwrap_or("?")
                            [..8.min(d["endpoint_id"].as_str().unwrap_or("?").len())],
                        if d["approved"].as_bool() == Some(true) {
                            "approved"
                        } else {
                            "pending"
                        },
                        if d["online"].as_bool() == Some(true) {
                            "online"
                        } else {
                            "offline"
                        },
                        d["hostname"].as_str().unwrap_or("?"),
                        d["os"].as_str().unwrap_or("?"),
                        d["arch"].as_str().unwrap_or("?"),
                    );
                }
            }
        },
        Commands::Agent { command } => match command {
            AgentCommands::Create { manifest, daemon } => {
                let text = std::fs::read_to_string(&manifest)
                    .with_context(|| format!("reading {manifest}"))?;
                let manifest: suzerain_protocol::AgentManifest = toml::from_str(&text)?;
                let r = request(
                    json!({"id": 1, "cmd": "agent_create", "manifest": manifest, "daemon": daemon}),
                )
                .await?;
                println!(
                    "created {} ({}) on daemon {}…",
                    r["name"].as_str().unwrap_or("?"),
                    r["id"].as_str().unwrap_or("?"),
                    &r["daemon_endpoint_id"].as_str().unwrap_or("?")
                        [..8.min(r["daemon_endpoint_id"].as_str().unwrap_or("?").len())],
                );
            }
            AgentCommands::List => {
                let r = request(json!({"id": 1, "cmd": "agent_list"})).await?;
                for a in r.as_array().into_iter().flatten() {
                    println!(
                        "{:<24} {:<14} {} on {}…",
                        a["name"].as_str().unwrap_or("?"),
                        a["state"].as_str().unwrap_or("?"),
                        a["id"].as_str().unwrap_or("?"),
                        &a["daemon_endpoint_id"].as_str().unwrap_or("?")
                            [..8.min(a["daemon_endpoint_id"].as_str().unwrap_or("?").len())],
                    );
                }
            }
            AgentCommands::Ask { name, message } => {
                let r = request(
                    json!({"id": 1, "cmd": "agent_ask", "name": name, "message": message.join(" ")}),
                )
                .await?;
                println!("{}", r["text"].as_str().unwrap_or("<none>"));
            }
            AgentCommands::Start { name } => {
                request(json!({"id": 1, "cmd": "agent_start", "name": name})).await?;
                println!("started {name}");
            }
            AgentCommands::Stop { name } => {
                request(json!({"id": 1, "cmd": "agent_stop", "name": name})).await?;
                println!("stopped {name}");
            }
            AgentCommands::Suspend { name } => {
                request(json!({"id": 1, "cmd": "agent_suspend", "name": name})).await?;
                println!("suspended {name}");
            }
            AgentCommands::Destroy { name } => {
                request(json!({"id": 1, "cmd": "agent_destroy", "name": name})).await?;
                println!("destroyed {name}");
            }
            AgentCommands::Logs { name, tail } => {
                let r = request(json!({"id": 1, "cmd": "agent_logs", "name": name, "tail": tail}))
                    .await?;
                for ev in r["events"].as_array().into_iter().flatten() {
                    println!(
                        "{} #{:<5} {}",
                        ev["at"].as_str().unwrap_or("?"),
                        ev["seq"],
                        ev["kind"].as_str().unwrap_or("?")
                    );
                }
            }
        },
    }
    Ok(())
}
