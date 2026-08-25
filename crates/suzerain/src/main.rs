//! suzerain: the unified binary (docs/UNIFIED-AGENT-API-DESIGN.md). One
//! binary, one way to run a fleet node — there is no separate `castellan`
//! binary anymore (the crate is now a library `suzerain` depends on).
//!
//!   suzerain run                       — foreground, standalone mode
//!                                        (default): control plane + a
//!                                        co-located agent-hosting child
//!                                        process, one binary
//!   suzerain run --operator <id>       — same, and approve a Suzy/operator
//!                                        EndpointId at startup (repeatable)
//!   suzerain run --mode control        — control plane only (no local
//!                                        agent hosting)
//!   suzerain run --mode agent          — agent-hosting only; reports to a
//!                                        control/standalone node elsewhere
//!   suzerain init --suzerain <id>      — configure this host's agent-role
//!                                        identity/labels and point it at a
//!                                        remote control plane, ahead of
//!                                        `suzerain run --mode agent`
//!   suzerain id                        — print this node's EndpointId

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use suzerain::retention::RoleMode;

#[derive(Parser)]
#[command(
    name = "suzerain",
    version,
    about = "Suzerain: fleet manager for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run in the foreground
    Run {
        /// Which half of the binary to run as. Defaults to the
        /// `[role].mode` config value, which itself defaults to
        /// `standalone`.
        #[arg(long, value_enum)]
        mode: Option<RoleMode>,
        /// Approve a Suzy/operator EndpointId at startup (repeatable) —
        /// live immediately and persisted to `[operator] allow`, same as
        /// running `suz operator approve <id>` separately. Lets a
        /// single-machine quickstart skip that extra step: pass Suzy's
        /// EndpointId here and it's ready to connect the moment this
        /// process comes up.
        #[arg(long = "operator")]
        operators: Vec<String>,
    },
    /// Configure this host's agent-hosting identity/config ahead of
    /// `suzerain run --mode agent` — a real, dedicated compute host
    /// reporting to a control plane on a different machine. Not needed for
    /// `standalone`/`control` modes, which never read this config.
    Init {
        /// The remote control plane's EndpointId to report to
        #[arg(long)]
        suzerain: Option<String>,
        /// Scheduling label (repeatable), e.g. --label gpu=true
        #[arg(long)]
        label: Vec<String>,
    },
    /// Print this node's iroh EndpointId (control-plane identity)
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
        Commands::Init { suzerain, label } => init_agent_role(suzerain, label),
        Commands::Run { mode, operators } => {
            let mode = match mode {
                Some(m) => m,
                None => suzerain::retention::load_config()?.role.mode,
            };
            match mode {
                RoleMode::Agent => castellan::run_foreground().await,
                RoleMode::Control => run_control_plane_foreground(&operators).await,
                RoleMode::Standalone => run_standalone(&operators).await,
            }
        }
    }
}

/// Generate/load this host's agent-role identity (the same iroh key
/// `mode = agent` and standalone mode's co-located child both use — see
/// `castellan::control::identity()`), and optionally point it at a remote
/// control plane and set scheduling labels. Mirrors the old `castellan
/// init` command exactly, just under the merged binary.
fn init_agent_role(suzerain_endpoint: Option<String>, labels: Vec<String>) -> Result<()> {
    let key = castellan::control::identity()?;
    // Persist BEFORE printing: pipelines that close stdout early (head -1)
    // must not lose the config write.
    if let Some(id) = suzerain_endpoint {
        id.parse::<iroh::EndpointId>()
            .context("invalid suzerain endpoint id")?;
        let mut cfg = castellan::control::load_config()?;
        cfg.suzerain_endpoint_id = Some(id);
        castellan::control::save_config(&cfg)?;
    }
    if !labels.is_empty() {
        let mut cfg = castellan::control::load_config()?;
        for kv in &labels {
            let Some((k, v)) = kv.split_once('=') else {
                anyhow::bail!("label must be k=v, got '{kv}'");
            };
            cfg.labels
                .insert(k.trim().to_string(), v.trim().to_string());
        }
        castellan::control::save_config(&cfg)?;
        println!("labels: {:?}", cfg.labels);
    }
    println!("agent-hosting endpoint id: {}", key.public());
    println!(
        "approve it on the control plane: suz daemon approve {}",
        key.public()
    );
    Ok(())
}

/// Start the control plane and every background task it owns. Returns the
/// live `ControlPlane` handle plus the retention-loaded config, so
/// standalone mode can layer the co-located agent-worker on top without
/// duplicating any of this startup sequence.
///
/// `cli_operators` are EndpointIds passed via `suzerain run --operator
/// <id>` — approved both live (added to the in-memory allow set this
/// process starts with) and persisted to `[operator] allow`, exactly like
/// `suz operator approve` would do against an already-running process.
/// This just collapses "start, then separately approve Suzy" into one
/// step for a quickstart.
async fn run_control_plane(
    cli_operators: &[String],
) -> Result<Arc<suzerain::control::ControlPlane>> {
    suzerain::secrets::load()?;
    let store = suzerain::store::Store::open().await?;
    tokio::spawn(suzerain::retention::run());
    let config = suzerain::retention::load_config()?;
    let mut operator_allow: Vec<iroh::EndpointId> = config
        .operator
        .allow
        .iter()
        .filter_map(|s| match s.parse() {
            Ok(id) => Some(id),
            Err(_) => {
                tracing::warn!("[operator] allow entry '{s}' is not a valid EndpointId — ignored");
                None
            }
        })
        .collect();
    for raw in cli_operators {
        let id: iroh::EndpointId = raw
            .parse()
            .with_context(|| format!("--operator '{raw}' is not a valid EndpointId"))?;
        if !operator_allow.contains(&id) {
            operator_allow.push(id);
        }
        // Persist too, so a restart without --operator still trusts it —
        // same durability `suz operator approve` gives you.
        suzerain::retention::add_operator_allow(raw)
            .with_context(|| format!("persisting --operator '{raw}' to [operator] allow"))?;
        println!("approved operator: {id}");
    }
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
    } else {
        // The REST API (`/api/v1/...`) is now the only local client-facing
        // transport `suz` and `suzerain-mcp` speak (docs/UNIFIED-AGENT-API-
        // DESIGN.md §6 step 3 retired the old Unix-socket protocol) — with
        // the web server disabled, neither can reach this control plane at
        // all. Suzy is unaffected (its iroh operator channel dials the
        // same router in-process, independent of this listener).
        tracing::warn!(
            "[web].enabled = false — `suz` and `suzerain-mcp` cannot reach this control plane \
             (only the iroh operator channel, e.g. Suzy, still works)"
        );
    }
    Ok(cp)
}

/// `mode = control`'s foreground loop. There's no longer a dedicated
/// "serve forever" future to await (the old Unix-socket operator API is
/// retired — see the warning above) — every real server here is a
/// background task, so the process just runs until interrupted.
async fn run_control_plane_foreground(cli_operators: &[String]) -> Result<()> {
    let _cp = run_control_plane(cli_operators).await?;
    wait_for_shutdown().await
}

/// Wait for a shutdown signal, then return — letting `main()`'s normal
/// return path run every pending `Drop` (including the standalone-mode
/// child's `kill_on_drop`, see `standalone::spawn_agent_worker`).
///
/// This distinction matters: `kill_on_drop` only fires on a *graceful*
/// Rust-level exit (a scope ending, a spawned task being dropped when the
/// tokio runtime shuts down at the end of `main()`) — an external `kill`/
/// `systemctl stop`/`launchctl unload` sends SIGTERM, which by default
/// terminates the process immediately without running any Rust destructors
/// at all. Catching SIGTERM here and returning normally is what turns "the
/// OS killed us" into "we exited, so Drop ran" — without it, standalone
/// mode's agent-worker child (and its qemu VM) would be orphaned on every
/// ordinary stop/restart, not just a crash.
async fn wait_for_shutdown() -> Result<()> {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
    Ok(())
}

/// `mode = standalone` (default): the control plane, plus a re-exec'd
/// `mode = agent` child process pre-approved and pointed at this control
/// plane — see `suzerain::standalone` for the identity/approval wiring.
/// The child is spawned with `kill_on_drop`, so this process exiting tears
/// it down too.
///
/// The child's lifecycle is monitored in a background task purely for
/// logging — it must never race against (and potentially cancel) the
/// actual client-facing API server below via `tokio::select!`: if the
/// child exits, the control plane keeps serving exactly like today's
/// behavior when a real, remote castellan disconnects.
async fn run_standalone(cli_operators: &[String]) -> Result<()> {
    let cp = run_control_plane(cli_operators).await?;
    let mut child = suzerain::standalone::spawn_agent_worker(&cp).await?;
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => tracing::warn!(%status, "co-located agent-worker exited"),
            Err(err) => tracing::warn!("co-located agent-worker wait failed: {err:#}"),
        }
    });

    wait_for_shutdown().await
}
