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
    /// Manage secrets (provider keys, extras, git deploy key).
    /// Bare `suz secrets` lists configured entries (names only, never values).
    Secrets {
        #[command(subcommand)]
        command: Option<SecretsCommands>,
    },
    /// Show the control-plane audit log
    Audit {
        #[arg(long, default_value = "50")]
        tail: usize,
    },
}

#[derive(Subcommand)]
enum SecretsCommands {
    /// Add or replace a secret. Reads the value from stdin when --value is
    /// omitted (preferred for multi-line keys and to keep secrets out of
    /// shell history).
    Set {
        /// Secret kind: provider | extra | deploy-key
        kind: String,
        /// Provider id (e.g. anthropic) or extra secret name (e.g.
        /// MY_TOKEN@api.example.com); not used for deploy-key
        name: Option<String>,
        /// Secret value; omit to read from stdin
        #[arg(long)]
        value: Option<String>,
    },
    /// Remove a secret
    Remove {
        /// Secret kind: provider | extra | deploy-key
        kind: String,
        /// Provider id or extra secret name; not used for deploy-key
        name: Option<String>,
    },
}

/// Normalize the CLI kind spelling to the wire kind.
fn secret_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "provider" => Ok("provider"),
        "extra" => Ok("extra"),
        "deploy-key" | "deploy_key" | "git" => Ok("deploy_key"),
        other => bail!("unknown secret kind '{other}' (provider|extra|deploy-key)"),
    }
}

/// Read a secret value from stdin (used when --value is omitted).
fn read_secret_stdin() -> Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading secret value from stdin")?;
    let value = buf.trim_end_matches('\n').to_string();
    if value.trim().is_empty() {
        bail!("empty secret value (pipe it via stdin or pass --value)");
    }
    Ok(value)
}

#[derive(Subcommand)]
enum DaemonCommands {
    /// Approve a daemon's EndpointId (enrollment)
    Approve { endpoint_id: String },
    /// List known daemons
    List,
    /// Set/remove scheduling labels on a daemon (operator overrides)
    Label {
        /// Daemon (endpoint-id prefix or hostname)
        daemon: String,
        /// k=v pairs to set (repeatable)
        #[arg(long)]
        set: Vec<String>,
        /// label keys to remove (repeatable)
        #[arg(long)]
        remove: Vec<String>,
    },
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
    /// Attach interactively: history, then live stream; type prompts
    Attach { name: String },
    /// Restore an agent (optionally onto a specific daemon)
    Restore {
        name: String,
        /// Target daemon (endpoint-id prefix or hostname)
        #[arg(long)]
        daemon: Option<String>,
    },
    /// Start a suspended agent
    Start {
        name: String,
        /// Force-restart: tear down the daemon's stale running entry first
        /// (recovery for an agent wedged but reported as running)
        #[arg(long)]
        force: bool,
    },
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

/// Interactive attach: history, then live events; stdin lines become prompts.
async fn attach(name: &str) -> Result<()> {
    let stream = UnixStream::connect(socket()).await?;
    let (reader, mut writer) = stream.into_split();
    let req = json!({"id": 1, "cmd": "agent_attach", "name": name});
    writer
        .write_all(format!("{}\n", serde_json::to_string(&req)?).as_bytes())
        .await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let first = lines.next_line().await?.context("no reply")?;
    let first: Value = serde_json::from_str(&first)?;
    if first["ok"].as_bool() != Some(true) {
        bail!("{}", first["error"].as_str().unwrap_or("attach failed"));
    }
    println!("attached to '{name}' — type a prompt and hit enter; ctrl-c detaches");

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
                let history = msg["history"].as_bool() == Some(true);
                render_event(&msg["event"], history);
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

fn render_event(ev: &Value, history: bool) {
    match ev["type"].as_str().unwrap_or("") {
        "message_end" if history => {
            let msg = &ev["message"];
            let role = msg["role"].as_str().unwrap_or("?");
            if let Some(parts) = msg["content"].as_array() {
                let text: String = parts
                    .iter()
                    .filter(|p| p["type"] == "text")
                    .filter_map(|p| p["text"].as_str())
                    .collect();
                if !text.trim().is_empty() {
                    println!("\x1b[2m[{role}] {text}\x1b[0m");
                }
            }
        }
        "history_end" => println!("\x1b[2m—— history above; live below ——\x1b[0m"),
        "message_update" => {
            let ame = &ev["assistantMessageEvent"];
            if ame["type"] == "text_delta" {
                if let Some(d) = ame["delta"].as_str() {
                    print!("{d}");
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
            }
        }
        "turn_end" => println!(),
        "tool_execution_start" => {
            println!("\n[tool: {}]", ev["toolName"].as_str().unwrap_or("?"));
        }
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Id => {
            let r = request(json!({"id": 1, "cmd": "endpoint_id"})).await?;
            println!("{}", r["endpoint_id"].as_str().unwrap_or("?"));
        }
        Commands::Secrets { command } => match command {
            None => {
                let r = request(json!({"id": 1, "cmd": "secrets_status"})).await?;
                for e in r["entries"].as_array().into_iter().flatten() {
                    println!("{}", e.as_str().unwrap_or("?"));
                }
            }
            Some(SecretsCommands::Set { kind, name, value }) => {
                let kind = secret_kind(&kind)?;
                let value = match value {
                    Some(v) => v,
                    None => read_secret_stdin()?,
                };
                let mut cmd = json!({"id": 1, "cmd": "secret_set", "kind": kind, "value": value});
                if let Some(n) = &name {
                    cmd["name"] = json!(n);
                }
                let r = request(cmd).await?;
                println!(
                    "set {} {}",
                    r["kind"].as_str().unwrap_or("?"),
                    r["name"].as_str().unwrap_or("?")
                );
            }
            Some(SecretsCommands::Remove { kind, name }) => {
                let kind = secret_kind(&kind)?;
                let mut cmd = json!({"id": 1, "cmd": "secret_delete", "kind": kind});
                if let Some(n) = &name {
                    cmd["name"] = json!(n);
                }
                let r = request(cmd).await?;
                println!(
                    "removed {} {}",
                    r["kind"].as_str().unwrap_or("?"),
                    r["name"].as_str().unwrap_or("?")
                );
            }
        },
        Commands::Audit { tail } => {
            let r = request(json!({"id": 1, "cmd": "audit_tail", "tail": tail})).await?;
            for e in r["entries"].as_array().into_iter().flatten() {
                println!(
                    "{} {:<16} {}",
                    e["at"].as_str().unwrap_or("?"),
                    e["action"].as_str().unwrap_or("?"),
                    serde_json::to_string(&e["detail"]).unwrap_or_default()
                );
            }
        }
        Commands::Daemon { command } => match command {
            DaemonCommands::Approve { endpoint_id } => {
                request(json!({"id": 1, "cmd": "daemon_approve", "endpoint_id": endpoint_id}))
                    .await?;
                println!("approved {endpoint_id}");
            }
            DaemonCommands::Label {
                daemon,
                set,
                remove,
            } => {
                let set_obj: serde_json::Map<String, Value> = set
                    .iter()
                    .map(|kv| {
                        let (k, v) = kv.split_once('=').expect("label must be k=v");
                        (k.trim().to_string(), json!(v.trim()))
                    })
                    .collect();
                let r = request(
                    json!({"id": 1, "cmd": "daemon_label", "endpoint_id": daemon, "set": set_obj, "remove": remove}),
                )
                .await?;
                println!(
                    "effective labels: {}",
                    serde_json::to_string(&r["effective_labels"])?
                );
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
            AgentCommands::Attach { name } => attach(&name).await?,
            AgentCommands::Restore { name, daemon } => {
                let r = request(
                    json!({"id": 1, "cmd": "agent_restore", "name": name, "daemon": daemon}),
                )
                .await?;
                println!(
                    "restored {} on daemon {}…",
                    r["restored"].as_str().unwrap_or("?"),
                    &r["daemon"].as_str().unwrap_or("?")
                        [..8.min(r["daemon"].as_str().unwrap_or("?").len())],
                );
            }
            AgentCommands::Start { name, force } => {
                request(json!({"id": 1, "cmd": "agent_start", "name": name, "force": force}))
                    .await?;
                println!("started {name}{}", if force { " (forced)" } else { "" });
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
