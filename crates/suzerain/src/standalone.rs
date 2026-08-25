//! Standalone mode (docs/UNIFIED-AGENT-API-DESIGN.md §4.1): this binary's
//! own control-plane process spawns a re-exec'd child of itself running in
//! `agent` mode, wiring the child's identity/approval/config so it connects
//! back automatically — no manual `suz daemon approve` step needed for the
//! co-located agent.
//!
//! Transport note: the parent and child talk over an ordinary iroh
//! connection (the same `Order`/`OrderAck`/`StreamHello` code path a real
//! cross-host `control`↔`agent` pair uses), not a socket pair — see §4.1's
//! transport-revision note for why. That means this module's job is purely
//! orchestration (identity, approval, config, spawn); no new wire protocol
//! lives here.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

use crate::control::ControlPlane;

/// Ensure the co-located agent-worker child is approved and pointed at this
/// control plane, then spawn it. Returns the child process handle — the
/// caller is responsible for awaiting/reaping it. `kill_on_drop` is set, so
/// dropping the handle (including this process exiting) tears the child
/// down too — no orphaned agent-worker process left holding VMs.
///
/// `log_dir`: `None` inherits this process's stdout/stderr, so the child's
/// own tracing output lands on the same terminal as the parent's — the
/// default. `Some(dir)` redirects the child's stdout/stderr to
/// `<dir>/agent.log` instead (appended); the child's own `telemetry::init`
/// call is unaffected either way, since this redirects the OS-level file
/// descriptors it writes to, not its tracing setup.
pub async fn spawn_agent_worker(cp: &ControlPlane, log_dir: Option<&Path>) -> Result<Child> {
    // The child's iroh identity is generated/loaded here, in the parent,
    // before the child ever runs: `castellan::control::identity()` reads or
    // creates `castellan.key` in the same shared data dir either way, so
    // calling it here just means the parent learns the child's EndpointId
    // synchronously instead of waiting for it to register — which is what
    // lets the parent pre-approve it below instead of requiring a manual
    // `suz daemon approve` step for a co-located agent.
    let child_key = castellan::control::identity()
        .context("loading/creating the co-located agent's identity")?;
    let child_id = child_key.public();

    cp.store()
        .approve_daemon(&child_id.to_string())
        .await
        .context("auto-approving the co-located agent daemon")?;

    // Point the child at this control plane so it connects back instead of
    // falling into its own "standalone, no control plane" fallback mode.
    let endpoint_id = cp.endpoint_id().to_string();
    let mut cfg = castellan::control::load_config()?;
    if cfg.suzerain_endpoint_id.as_deref() != Some(endpoint_id.as_str()) {
        cfg.suzerain_endpoint_id = Some(endpoint_id);
        castellan::control::save_config(&cfg)?;
    }

    let exe = std::env::current_exe().context("resolving this binary's own path to re-exec")?;
    let mut cmd = Command::new(exe);
    cmd.args(["run", "--mode", "agent"]).kill_on_drop(true);
    if let Some(dir) = log_dir {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating log directory {}", dir.display()))?;
        let log_path = dir.join("agent.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("opening log file {}", log_path.display()))?;
        cmd.stdout(Stdio::from(
            log_file
                .try_clone()
                .context("cloning agent-worker log file handle for stdout")?,
        ));
        cmd.stderr(Stdio::from(log_file));
    }
    let child = cmd
        .spawn()
        .context("spawning the co-located agent-worker process")?;
    tracing::info!(
        child_id = %child_id,
        pid = ?child.id(),
        "spawned co-located agent-worker (standalone mode)"
    );
    Ok(child)
}
