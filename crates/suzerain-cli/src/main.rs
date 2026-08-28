//! `suz`: operator CLI for the suzerain control plane.
//!
//! Talks directly to the control plane's REST API (`/api/v1/...`, served by
//! `suzerain run`'s embedded web server) via `suzerain_client::Client`'s
//! HTTP transport — the same shared client Suzy uses over iroh and
//! suzerain-mcp uses as a thin wrapper (docs/UNIFIED-AGENT-API-DESIGN.md
//! §6 step 3). This replaces the old ad-hoc Unix-socket JSONL protocol
//! entirely.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use serde_json::Value;
use suzerain_client::{Client, PromptMode, SessionEvent};

#[derive(Parser)]
#[command(
    name = "suz",
    version,
    about = "Operator CLI for the suzerain control plane"
)]
struct Cli {
    /// Base URL of the control plane REST API. Defaults to reading
    /// `[web].port` from suzerain.toml (falling back to 8484) if unset.
    #[arg(long, env = "SUZERAIN_API_URL")]
    api_url: Option<String>,
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
    /// Manage suzy operator clients (authorize their EndpointIds on the
    /// iroh operator channel)
    Operator {
        #[command(subcommand)]
        command: OperatorCommands,
    },
    /// Manage agents
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Print the control plane's EndpointId
    Id,
    /// Manage secrets (provider keys, extras, git SSH key).
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
        /// Secret kind: provider | extra | ssh-key
        kind: String,
        /// Provider id (e.g. anthropic) or extra secret name (e.g.
        /// MY_TOKEN@api.example.com); not used for ssh-key
        name: Option<String>,
        /// Secret value; omit to read from stdin
        #[arg(long)]
        value: Option<String>,
    },
    /// Remove a secret
    Remove {
        /// Secret kind: provider | extra | ssh-key
        kind: String,
        /// Provider id or extra secret name; not used for ssh-key
        name: Option<String>,
    },
}

/// Normalize the CLI kind spelling to the wire kind.
fn secret_kind(kind: &str) -> Result<&'static str> {
    match kind {
        "provider" => Ok("provider"),
        "extra" => Ok("extra"),
        "ssh-key" | "ssh_key" | "ssh" => Ok("ssh_key"),
        // pre-rename aliases
        "deploy-key" | "deploy_key" | "git" => Ok("ssh_key"),
        other => bail!("unknown secret kind '{other}' (provider|extra|ssh-key)"),
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
enum OperatorCommands {
    /// Approve a suzy EndpointId. On a running control plane this takes
    /// effect immediately (no restart); if suzerain is down the id is
    /// written to config.toml and applies on next start.
    Approve { endpoint_id: String },
    /// List approved suzy EndpointIds
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
    /// List agents and their statuses
    List,
    /// Send a prompt and print the final answer (sleeping agents wake
    /// automatically; the first answer can take a few minutes)
    Ask {
        name: String,
        message: Vec<String>,
        /// How long to wait for a reply before giving up (seconds)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Attach interactively: history, then live stream; type prompts.
    /// Sleeping agents wake automatically on attach.
    Attach { name: String },
    /// Set per-agent policy overrides
    Config {
        name: String,
        /// Auto-suspend after this much inactivity ("10m", "2h"), "never",
        /// or "default" to inherit the global policy
        #[arg(long)]
        auto_suspend: String,
    },
    /// Permanently destroy an agent
    Destroy { name: String },
    /// Show an agent's centrally stored event log
    Logs {
        name: String,
        #[arg(long, default_value = "50")]
        tail: usize,
    },
}

fn data_dir() -> std::path::PathBuf {
    std::env::var("SUZERAIN_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            std::path::PathBuf::from(home).join(".local/share/suzerain")
        })
}

/// `suzerain.toml` in the data dir (castellan.toml sits beside it in the
/// shared fleet home); a legacy `config.toml` is renamed on first access.
fn config_path() -> std::path::PathBuf {
    let dir = data_dir();
    let new = dir.join("suzerain.toml");
    let legacy = dir.join("config.toml");
    if !new.exists() && legacy.exists() && std::fs::rename(&legacy, &new).is_err() {
        return legacy;
    }
    new
}

/// The control plane's REST API base URL: `--api-url`/`SUZERAIN_API_URL` if
/// given, else `[web].port` from suzerain.toml (default 8484 — matching
/// `suzerain::retention::Web`'s default), on 127.0.0.1 (the web server is
/// localhost-only).
fn api_base_url(explicit: Option<String>) -> String {
    if let Some(url) = explicit {
        return url;
    }
    let port = std::fs::read_to_string(config_path())
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|doc| doc.get("web")?.get("port")?.as_integer())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(8484);
    format!("http://127.0.0.1:{port}")
}

/// Offline fallback for `operator approve`: add the id to `[operator]
/// allow` in suzerain.toml directly (used when the control plane is down;
/// it reads the file at startup). Returns true when newly added. Uses
/// toml::Value so unrelated sections are preserved; comments are not.
fn add_operator_allow_to_file(path: &std::path::Path, endpoint_id: &str) -> Result<bool> {
    let mut doc: toml::Value = if path.exists() {
        toml::from_str(&std::fs::read_to_string(path)?)?
    } else {
        toml::Value::Table(Default::default())
    };
    let root = doc.as_table_mut().context("config file: not a table")?;
    let operator = operator_table(root)?;
    let allow = operator
        .entry("allow")
        .or_insert_with(|| toml::Value::Array(vec![]));
    let allow = allow
        .as_array_mut()
        .context("[operator] allow is not an array")?;
    if allow.iter().any(|e| e.as_str() == Some(endpoint_id)) {
        return Ok(false);
    }
    allow.push(toml::Value::String(endpoint_id.to_string()));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&doc)?)?;
    Ok(true)
}

fn operator_table(
    root: &mut toml::map::Map<String, toml::Value>,
) -> Result<&mut toml::map::Map<String, toml::Value>> {
    let operator = root
        .entry("operator")
        .or_insert_with(|| toml::Value::Table(Default::default()));
    operator.as_table_mut().context("[operator] is not a table")
}

/// Render one reconstructed history item (`{"role", "parts": [...]}` — see
/// `web_session::history_items`) as plain text.
fn render_history_item(item: &Value) {
    let role = item["role"].as_str().unwrap_or("?");
    let text: String = item["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["text"].as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if !text.trim().is_empty() {
        println!("\x1b[2m[{role}] {text}\x1b[0m");
    }
}

/// Render one live pi event (the raw event shape under `SessionEvent::Live`).
fn render_live_event(ev: &Value) {
    match ev["type"].as_str().unwrap_or("") {
        "status" => {
            let msg = ev["message"].as_str().unwrap_or("");
            println!("\x1b[2m[status] {msg}\x1b[0m");
        }
        "notice" => {
            println!(
                "\x1b[2m[notice] {}\x1b[0m",
                ev["message"].as_str().unwrap_or("")
            );
        }
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

/// Interactive attach: replay history, then the live stream; stdin lines
/// become prompts. Composed from `session_stream` (SSE) + `prompt` (POST)
/// — the same primitives `Client::ask` and the web UI use, per
/// docs/UNIFIED-AGENT-API-DESIGN.md §4.3.5.
async fn attach(client: &Client, name: &str) -> Result<()> {
    let mut stream = client.session_stream(name).await?;
    println!("attached to '{name}' — type a prompt and hit enter; ctrl-c detaches");

    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<String>(16);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if stdin_tx.send(line).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            ev = stream.next() => {
                match ev {
                    Some(Ok(SessionEvent::History(item))) => render_history_item(&item),
                    Some(Ok(SessionEvent::HistoryEnd)) => {
                        println!("\x1b[2m—— history above; live below ——\x1b[0m");
                    }
                    Some(Ok(SessionEvent::Live(ev))) => render_live_event(&ev),
                    Some(Ok(SessionEvent::ServerError(msg))) => {
                        println!("\x1b[31m[error] {msg}\x1b[0m");
                    }
                    Some(Err(e)) => {
                        println!("\x1b[31m[stream error] {e}\x1b[0m");
                        break;
                    }
                    None => break,
                }
            }
            input = stdin_rx.recv() => {
                let Some(input) = input else { break };
                if input.trim().is_empty() { continue }
                if let Err(e) = client.prompt(name, &input, PromptMode::Prompt).await {
                    println!("\x1b[31m[send failed] {e}\x1b[0m");
                }
            }
        }
    }
    Ok(())
}

fn short(id: &str) -> &str {
    &id[..8.min(id.len())]
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = Client::http(&api_base_url(cli.api_url));

    match cli.command {
        Commands::Id => {
            let r = client.endpoint().await?;
            println!("{}", r.endpoint_id);
        }
        Commands::Secrets { command } => match command {
            None => {
                let r = client.secrets().await?;
                for e in r["entries"].as_array().into_iter().flatten() {
                    println!(
                        "{}:{}",
                        e["kind"].as_str().unwrap_or("?"),
                        e["name"].as_str().unwrap_or("?")
                    );
                }
            }
            Some(SecretsCommands::Set { kind, name, value }) => {
                let kind = secret_kind(&kind)?;
                let value = match value {
                    Some(v) => v,
                    None => read_secret_stdin()?,
                };
                match kind {
                    "provider" => {
                        let name = name.context("provider name required")?;
                        client.set_secret_provider(&name, &value).await?;
                        println!("set provider {name}");
                    }
                    "extra" => {
                        let name = name.context("extra secret name required")?;
                        client.set_secret_extra(&name, &value).await?;
                        println!("set extra {name}");
                    }
                    "ssh_key" => {
                        client.set_ssh_key(&value).await?;
                        println!("set ssh_key");
                    }
                    _ => unreachable!(),
                }
            }
            Some(SecretsCommands::Remove { kind, name }) => {
                let kind = secret_kind(&kind)?;
                match kind {
                    "provider" => {
                        let name = name.context("provider name required")?;
                        client.delete_secret_provider(&name).await?;
                        println!("removed provider {name}");
                    }
                    "extra" => {
                        let name = name.context("extra secret name required")?;
                        client.delete_secret_extra(&name).await?;
                        println!("removed extra {name}");
                    }
                    "ssh_key" => {
                        client.delete_ssh_key().await?;
                        println!("removed ssh_key");
                    }
                    _ => unreachable!(),
                }
            }
        },
        Commands::Audit { tail } => {
            for e in client.audit(tail).await? {
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
                client.approve_daemon(&endpoint_id).await?;
                println!("approved {endpoint_id}");
            }
            DaemonCommands::Label {
                daemon,
                set,
                remove,
            } => {
                let set_map: std::collections::BTreeMap<String, String> = set
                    .iter()
                    .map(|kv| parse_label_kv(kv))
                    .collect::<Result<_>>()?;
                client.set_daemon_labels(&daemon, &set_map, &remove).await?;
                println!("labels updated for {daemon}");
            }
            DaemonCommands::List => {
                for d in client.daemons().await? {
                    println!(
                        "{:<10} {:<8} {:<8} {:<20} {}/{}",
                        short(&d.endpoint_id),
                        if d.approved { "approved" } else { "pending" },
                        if d.online { "online" } else { "offline" },
                        d.hostname,
                        d.os,
                        d.arch,
                    );
                }
            }
        },
        Commands::Operator { command } => match command {
            OperatorCommands::Approve { endpoint_id } => {
                endpoint_id
                    .parse::<iroh::EndpointId>()
                    .context("invalid endpoint id")?;
                match client.operator_approve(&endpoint_id).await {
                    Ok(()) => println!("approved {endpoint_id} (live — no restart needed)"),
                    Err(_) => {
                        // Control plane unreachable: persist for next start.
                        let path = config_path();
                        if add_operator_allow_to_file(&path, &endpoint_id)? {
                            println!(
                                "suzerain not running — added {endpoint_id} to {} (applies on next start)",
                                path.display()
                            );
                        } else {
                            println!("{endpoint_id} already approved in {}", path.display());
                        }
                    }
                }
            }
            OperatorCommands::List => {
                for id in client.operators().await? {
                    println!("{id}");
                }
            }
        },
        Commands::Agent { command } => match command {
            AgentCommands::Create { manifest, daemon } => {
                let text = std::fs::read_to_string(&manifest)
                    .with_context(|| format!("reading {manifest}"))?;
                // Validate locally so a bad manifest fails with a clear
                // parse error before it ever reaches the network.
                let _: suzerain_protocol::AgentManifest =
                    toml::from_str(&text).with_context(|| format!("parsing {manifest}"))?;
                let r = client.create_agent_full(&text, daemon.as_deref()).await?;
                println!(
                    "created {} ({}) on daemon {}…",
                    r["name"].as_str().unwrap_or("?"),
                    r["id"].as_str().unwrap_or("?"),
                    short(r["daemon_endpoint_id"].as_str().unwrap_or("?")),
                );
            }
            AgentCommands::List => {
                for a in client.agents().await? {
                    let idle = a.idle_secs.unwrap_or(0).max(0) as u64;
                    let idle_str = if a.status == "idle" {
                        format!(" ({}m)", idle / 60)
                    } else {
                        String::new()
                    };
                    println!(
                        "{:<24} {:<14} {} on {}…{}{}",
                        a.name,
                        a.status,
                        a.id,
                        short(&a.daemon_endpoint_id),
                        idle_str,
                        if a.needs_attention {
                            " ⚠ needs attention"
                        } else {
                            ""
                        },
                    );
                }
            }
            AgentCommands::Ask {
                name,
                message,
                timeout,
            } => {
                let reply = client
                    .ask(&name, &message.join(" "), Duration::from_secs(timeout))
                    .await?;
                println!("{}", if reply.is_empty() { "<none>" } else { &reply });
            }
            AgentCommands::Attach { name } => attach(&client, &name).await?,
            AgentCommands::Config { name, auto_suspend } => {
                client.set_auto_suspend(&name, &auto_suspend).await?;
                println!("auto-suspend policy for {name}: {auto_suspend}");
            }
            AgentCommands::Destroy { name } => {
                client.destroy_agent(&name, false).await?;
                println!("destroyed {name}");
            }
            AgentCommands::Logs { name, tail } => {
                let r = client.agent_logs(&name, tail).await?;
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

/// Parse one `--set key=value` argument. Errors cleanly (instead of
/// panicking) on a malformed value with no `=`.
fn parse_label_kv(s: &str) -> Result<(String, String)> {
    let (k, v) = s
        .split_once('=')
        .with_context(|| format!("invalid --set value '{s}': expected key=value"))?;
    Ok((k.trim().to_string(), v.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_label_kv_valid() {
        assert_eq!(
            parse_label_kv("key=value").unwrap(),
            ("key".to_string(), "value".to_string())
        );
    }

    #[test]
    fn parse_label_kv_trims_whitespace() {
        assert_eq!(
            parse_label_kv(" key = value ").unwrap(),
            ("key".to_string(), "value".to_string())
        );
    }

    #[test]
    fn parse_label_kv_rejects_missing_equals() {
        assert!(parse_label_kv("bad").is_err());
    }

    // ── secret_kind() ────────────────────────────────────────────────────

    #[test]
    fn secret_kind_normalizes_known_spellings() {
        assert_eq!(secret_kind("provider").unwrap(), "provider");
        assert_eq!(secret_kind("extra").unwrap(), "extra");
        assert_eq!(secret_kind("ssh-key").unwrap(), "ssh_key");
        assert_eq!(secret_kind("ssh_key").unwrap(), "ssh_key");
        assert_eq!(secret_kind("ssh").unwrap(), "ssh_key");
    }

    #[test]
    fn secret_kind_accepts_legacy_aliases() {
        assert_eq!(secret_kind("deploy-key").unwrap(), "ssh_key");
        assert_eq!(secret_kind("deploy_key").unwrap(), "ssh_key");
        assert_eq!(secret_kind("git").unwrap(), "ssh_key");
    }

    #[test]
    fn secret_kind_rejects_unknown_kind() {
        let err = secret_kind("bogus").unwrap_err();
        assert!(err.to_string().contains("unknown secret kind"), "{err}");
    }

    // ── add_operator_allow_to_file() ─────────────────────────────────────

    fn scratch_config_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "suz-cli-test-{tag}-{}-{:?}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn add_operator_allow_creates_file_and_adds_id() {
        let path = scratch_config_path("create");
        assert!(!path.exists());
        let added = add_operator_allow_to_file(&path, "abc123").unwrap();
        assert!(added);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("abc123"), "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_operator_allow_is_idempotent() {
        let path = scratch_config_path("idempotent");
        assert!(add_operator_allow_to_file(&path, "abc123").unwrap());
        // Second add of the same id should report "not newly added".
        let added_again = add_operator_allow_to_file(&path, "abc123").unwrap();
        assert!(!added_again);
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("abc123").count(), 1, "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_operator_allow_preserves_unrelated_sections() {
        let path = scratch_config_path("preserve");
        std::fs::write(&path, "[web]\nport = 9999\n").unwrap();
        add_operator_allow_to_file(&path, "xyz789").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("9999"), "{text}");
        assert!(text.contains("xyz789"), "{text}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn add_operator_allow_appends_second_distinct_id() {
        let path = scratch_config_path("append");
        add_operator_allow_to_file(&path, "id-one").unwrap();
        let added = add_operator_allow_to_file(&path, "id-two").unwrap();
        assert!(added);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("id-one") && text.contains("id-two"), "{text}");
        std::fs::remove_file(&path).ok();
    }

    // ── api_base_url() ───────────────────────────────────────────────────

    #[test]
    fn api_base_url_prefers_explicit_arg() {
        assert_eq!(
            api_base_url(Some("http://example.test:1234".to_string())),
            "http://example.test:1234"
        );
    }

    // ── CLI parsing (clap derive) ────────────────────────────────────────

    #[test]
    fn cli_parses_agent_ask_with_multiword_message() {
        let cli = Cli::try_parse_from([
            "suz",
            "agent",
            "ask",
            "my-agent",
            "hello",
            "there",
            "--timeout",
            "42",
        ])
        .unwrap();
        match cli.command {
            Commands::Agent {
                command:
                    AgentCommands::Ask {
                        name,
                        message,
                        timeout,
                    },
            } => {
                assert_eq!(name, "my-agent");
                assert_eq!(message, vec!["hello".to_string(), "there".to_string()]);
                assert_eq!(timeout, 42);
            }
            _ => panic!("expected Agent::Ask"),
        }
    }

    #[test]
    fn cli_ask_timeout_defaults_to_300() {
        let cli = Cli::try_parse_from(["suz", "agent", "ask", "my-agent", "hi"]).unwrap();
        match cli.command {
            Commands::Agent {
                command: AgentCommands::Ask { timeout, .. },
            } => assert_eq!(timeout, 300),
            _ => panic!("expected Agent::Ask"),
        }
    }

    #[test]
    fn cli_parses_daemon_label_with_repeated_set_flags() {
        let cli = Cli::try_parse_from([
            "suz",
            "daemon",
            "label",
            "box1",
            "--set",
            "zone=office",
            "--set",
            "tier=gpu",
            "--remove",
            "old",
        ])
        .unwrap();
        match cli.command {
            Commands::Daemon {
                command:
                    DaemonCommands::Label {
                        daemon,
                        set,
                        remove,
                    },
            } => {
                assert_eq!(daemon, "box1");
                assert_eq!(set, vec!["zone=office".to_string(), "tier=gpu".to_string()]);
                assert_eq!(remove, vec!["old".to_string()]);
            }
            _ => panic!("expected Daemon::Label"),
        }
    }

    #[test]
    fn cli_parses_api_url_flag() {
        let cli = Cli::try_parse_from(["suz", "--api-url", "http://host:1", "id"]).unwrap();
        assert_eq!(cli.api_url.as_deref(), Some("http://host:1"));
    }

    #[test]
    fn cli_secrets_bare_command_is_none() {
        let cli = Cli::try_parse_from(["suz", "secrets"]).unwrap();
        match cli.command {
            Commands::Secrets { command } => assert!(command.is_none()),
            _ => panic!("expected Secrets"),
        }
    }

    #[test]
    fn cli_secrets_set_reads_optional_value() {
        let cli = Cli::try_parse_from(["suz", "secrets", "set", "provider", "anthropic"]).unwrap();
        match cli.command {
            Commands::Secrets {
                command:
                    Some(SecretsCommands::Set {
                        kind, name, value, ..
                    }),
            } => {
                assert_eq!(kind, "provider");
                assert_eq!(name.as_deref(), Some("anthropic"));
                assert!(value.is_none());
            }
            _ => panic!("expected Secrets::Set"),
        }
    }

    #[test]
    fn cli_audit_tail_defaults_to_50() {
        let cli = Cli::try_parse_from(["suz", "audit"]).unwrap();
        match cli.command {
            Commands::Audit { tail } => assert_eq!(tail, 50),
            _ => panic!("expected Audit"),
        }
    }

    #[test]
    fn cli_rejects_missing_required_subcommand() {
        assert!(Cli::try_parse_from(["suz"]).is_err());
    }

    #[test]
    fn cli_rejects_unknown_subcommand() {
        assert!(Cli::try_parse_from(["suz", "bogus-command"]).is_err());
    }

    #[test]
    fn cli_agent_create_requires_manifest_flag() {
        assert!(Cli::try_parse_from(["suz", "agent", "create"]).is_err());
    }

    #[test]
    fn cli_agent_create_parses_manifest_and_optional_daemon() {
        let cli = Cli::try_parse_from([
            "suz",
            "agent",
            "create",
            "--manifest",
            "path/to.toml",
            "--daemon",
            "box1",
        ])
        .unwrap();
        match cli.command {
            Commands::Agent {
                command: AgentCommands::Create { manifest, daemon },
            } => {
                assert_eq!(manifest, "path/to.toml");
                assert_eq!(daemon.as_deref(), Some("box1"));
            }
            _ => panic!("expected Agent::Create"),
        }
    }
}
