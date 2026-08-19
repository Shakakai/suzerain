//! castellan: the per-server agent daemon.
//!
//!   castellan run                     — foreground daemon (unix socket control)
//!   castellan create --manifest m.toml
//!   castellan start|stop|destroy|list|logs|attach|exec …
//!
//! Phase 1: standalone, no control plane (see docs/PLAN.md §11).

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use castellan::daemon::{self, socket_path};
use castellan::state;
use castellan::supervisor::Supervisor;
use suzerain_protocol::AgentState;

#[derive(Parser)]
#[command(name = "castellan", version, about = "Per-server AI agent daemon")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize this daemon: generate identity, optionally set suzerain
    Init {
        /// Suzerain's EndpointId to report to
        #[arg(long)]
        suzerain: Option<String>,
        /// Scheduling label (repeatable), e.g. --label gpu=true
        #[arg(long)]
        label: Vec<String>,
    },
    /// Run the daemon in the foreground
    Run,
    /// Create and start an agent from a manifest file
    Create {
        #[arg(long)]
        manifest: String,
    },
    /// Start a suspended agent
    Start { name: String },
    /// Gracefully stop an agent
    Stop { name: String },
    /// Destroy an agent and its local state
    Destroy { name: String },
    /// List agents
    List,
    /// Show an agent's event journal
    Logs {
        name: String,
        #[arg(long, default_value = "50")]
        tail: usize,
    },
    /// Attach to a running agent: stream events, type prompts
    Attach { name: String },
    /// Send a prompt and print the final answer (one-shot)
    Ask { name: String, message: Vec<String> },
    /// Run a command inside the agent's VM (debugging)
    Exec {
        name: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        argv: Vec<String>,
    },
}

/// Send one request to the daemon socket and read its reply.
async fn request(cmd: Value) -> Result<Value> {
    let stream = UnixStream::connect(socket_path())
        .await
        .context("connecting to castellan daemon (is `castellan run` up?)")?;
    let (reader, mut writer) = stream.into_split();
    let mut line = serde_json::to_vec(&cmd)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    let mut lines = BufReader::new(reader).lines();
    let reply = lines
        .next_line()
        .await?
        .context("daemon closed connection")?;
    let reply: Value = serde_json::from_str(&reply)?;
    if reply["ok"].as_bool() == Some(true) {
        Ok(reply["result"].clone())
    } else {
        bail!("{}", reply["error"].as_str().unwrap_or("unknown error"))
    }
}

async fn attach(name: &str) -> Result<()> {
    let stream = UnixStream::connect(socket_path()).await?;
    let (reader, mut writer) = stream.into_split();
    let req = json!({"id": 1, "cmd": "attach", "name": name});
    writer
        .write_all(format!("{}\n", serde_json::to_string(&req)?).as_bytes())
        .await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    // First line is the attach reply.
    let first = lines.next_line().await?.context("no reply")?;
    let first: Value = serde_json::from_str(&first)?;
    if first["ok"].as_bool() != Some(true) {
        bail!("{}", first["error"].as_str().unwrap_or("attach failed"));
    }
    println!("attached to '{name}' — type a prompt and hit enter; ctrl-c detaches");

    // stdin → prompts.
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = stdin_lines.next_line().await {
            if stdin_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                let ev = &msg["event"];
                render_event(ev);
            }
            input = stdin_rx.recv() => {
                let Some(input) = input else { break };
                if input.trim().is_empty() { continue }
                let prompt = json!({"cmd": "prompt", "message": input});
                writer
                    .write_all(format!("{}\n", serde_json::to_string(&prompt)?).as_bytes())
                    .await?;
                writer.flush().await?;
            }
        }
    }
    Ok(())
}

/// Exclusive flock on the data dir; exits with a clear error if another
/// castellan already holds it. Returned guard keeps the lock held.
fn acquire_instance_lock() -> Result<std::fs::File> {
    use fs2::FileExt;
    std::fs::create_dir_all(state::data_dir())?;
    let path = state::data_dir().join("castellan.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    file.try_lock_exclusive().map_err(|_| {
        anyhow::anyhow!(
            "another castellan is already running with data dir {} (castellan.lock held)",
            state::data_dir().display()
        )
    })?;
    Ok(file)
}

fn render_event(ev: &Value) {
    match ev["type"].as_str().unwrap_or("") {
        // Streaming assistant text.
        "message_update" => {
            if let Some(delta) = ev["assistantMessageEvent"]["delta"].as_str() {
                if ev["assistantMessageEvent"]["type"] == "text_delta" {
                    print!("{delta}");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            }
        }
        "turn_end" => println!(),
        "tool_execution_start" => {
            let tool = ev["toolName"].as_str().unwrap_or("?");
            println!("\n[tool: {tool}]");
        }
        "pi_stderr" => {
            if let Some(l) = ev["line"].as_str() {
                eprintln!("[pi stderr] {l}");
            }
        }
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    suzerain_protocol::telemetry::init("castellan=info", "castellan")?;

    match Cli::parse().command {
        Commands::Init { suzerain, label } => {
            let key = castellan::control::identity()?;
            // Persist BEFORE printing: pipelines that close stdout early
            // (head -1) must not lose the config write.
            if let Some(id) = suzerain {
                id.parse::<iroh::EndpointId>()
                    .context("invalid suzerain endpoint id")?;
                let mut cfg = castellan::control::load_config()?;
                cfg.suzerain_endpoint_id = Some(id);
                castellan::control::save_config(&cfg)?;
            }
            if !label.is_empty() {
                let mut cfg = castellan::control::load_config()?;
                for kv in &label {
                    let Some((k, v)) = kv.split_once('=') else {
                        anyhow::bail!("label must be k=v, got '{kv}'");
                    };
                    cfg.labels
                        .insert(k.trim().to_string(), v.trim().to_string());
                }
                castellan::control::save_config(&cfg)?;
                println!("labels: {:?}", cfg.labels);
            }
            println!("castellan endpoint id: {}", key.public());
            println!(
                "approve it on the control plane: suz daemon approve {}",
                key.public()
            );
            Ok(())
        }
        Commands::Run => {
            // Instance fencing (G2): two daemons on the same data dir would
            // fight over one identity (registration flapping).
            let _lock = acquire_instance_lock()?;
            // Reconcile: VMs/drivers are children of the previous daemon
            // process and died with it — nothing can be running now.
            for mut record in state::list().await? {
                if matches!(record.state, AgentState::Active | AgentState::Restoring) {
                    record.state = AgentState::Suspended;
                    state::save(&record).await?;
                    tracing::info!(agent = %record.name, "marked suspended on daemon startup");
                }
            }
            let supervisor = Arc::new(Supervisor::new());
            supervisor.spawn_activity_flusher();
            let control_supervisor = supervisor.clone();
            let control = tokio::spawn(async move {
                if let Err(err) = castellan::control::run_control_client(control_supervisor).await {
                    tracing::warn!("control client exited: {err:#}");
                }
            });
            let served = daemon::serve(supervisor).await;
            control.abort();
            served
        }
        Commands::Create { manifest } => {
            let text = std::fs::read_to_string(&manifest)
                .with_context(|| format!("reading {manifest}"))?;
            let manifest: suzerain_protocol::manifest::AgentManifest =
                toml::from_str(&text).with_context(|| format!("parsing {manifest}"))?;
            let record = request(json!({"id": 1, "cmd": "create", "manifest": manifest})).await?;
            println!("created: {}", serde_json::to_string_pretty(&record)?);
            Ok(())
        }
        Commands::Start { name } => {
            let record = request(json!({"id": 1, "cmd": "start", "name": name})).await?;
            println!("started: {}", record["name"]);
            Ok(())
        }
        Commands::Stop { name } => {
            request(json!({"id": 1, "cmd": "stop", "name": name})).await?;
            println!("stopped: {name}");
            Ok(())
        }
        Commands::Destroy { name } => {
            request(json!({"id": 1, "cmd": "destroy", "name": name})).await?;
            println!("destroyed: {name}");
            Ok(())
        }
        Commands::List => {
            let records = request(json!({"id": 1, "cmd": "list"})).await?;
            for r in records.as_array().into_iter().flatten() {
                println!(
                    "{:<24} {:<14} {}",
                    r["name"].as_str().unwrap_or("?"),
                    r["state"].as_str().unwrap_or("?"),
                    r["id"].as_str().unwrap_or("?"),
                );
            }
            Ok(())
        }
        Commands::Logs { name, tail } => {
            let result =
                request(json!({"id": 1, "cmd": "logs", "name": name, "tail": tail})).await?;
            for ev in result["events"].as_array().into_iter().flatten() {
                println!(
                    "{} #{:<5} {}",
                    ev["at"].as_str().unwrap_or("?"),
                    ev["seq"],
                    ev["kind"].as_str().unwrap_or("?")
                );
            }
            Ok(())
        }
        Commands::Attach { name } => attach(&name).await,
        Commands::Ask { name, message } => {
            let result =
                request(json!({"id": 1, "cmd": "ask", "name": name, "message": message.join(" ")}))
                    .await?;
            println!("{}", result["text"].as_str().unwrap_or("<none>"));
            Ok(())
        }
        Commands::Exec { name, argv } => {
            let result =
                request(json!({"id": 1, "cmd": "exec", "name": name, "argv": argv})).await?;
            print!("{}", result["stdout"].as_str().unwrap_or(""));
            eprint!("{}", result["stderr"].as_str().unwrap_or(""));
            Ok(())
        }
    }
}
