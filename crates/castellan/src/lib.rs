//! castellan library core: the daemon's subsystems live here; the binary is
//! a thin shell. See docs/PLAN.md §6.

use std::sync::Arc;

use anyhow::Result;
use suzerain_protocol::AgentState;

pub mod control;
pub mod driver;
pub mod journal;
pub mod probe;
pub mod provision;
pub mod rpc;
pub mod secrets;
pub mod state;
pub mod supervisor;

/// Exclusive flock on the data dir; exits with a clear error if another
/// castellan already holds it. Returned guard keeps the lock held.
pub fn acquire_instance_lock() -> Result<std::fs::File> {
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

/// Run the daemon in the foreground: instance lock, crash-reconciliation,
/// and the control-plane client. All user-facing agent lifecycle
/// operations (create/ask/attach/destroy/...) go through the control
/// plane's unified API (`suzerain`'s operator socket/REST/iroh operator
/// channel) — castellan itself no longer has a local socket or CLI verbs
/// of its own (removed per docs/UNIFIED-AGENT-API-DESIGN.md §6 step 2: a
/// standalone deployment always has a control-role process now, so there
/// is no more "castellan with no control plane, talk to it directly"
/// scenario to serve).
///
/// Used both by `castellan run` (pointed at a control plane via `castellan
/// init --suzerain <id>`, for a real distributed `agent`-only host) and,
/// per §4.1, by the re-exec'd agent-worker child a `suzerain run`
/// standalone-mode parent spawns (`suzerain run --mode agent`) — the exact
/// same code path either way.
pub async fn run_foreground() -> Result<()> {
    // Instance fencing (G2): two daemons on the same data dir would fight
    // over one identity (registration flapping).
    let _lock = acquire_instance_lock()?;
    // Reconcile: VMs/drivers are children of the previous daemon process
    // and died with it — nothing can be running now.
    for mut record in state::list().await? {
        if matches!(record.state, AgentState::Active | AgentState::Restoring) {
            record.state = AgentState::Suspended;
            state::save(&record).await?;
            tracing::info!(agent = %record.name, "marked suspended on daemon startup");
        }
    }
    let supervisor = Arc::new(supervisor::Supervisor::new());
    supervisor.spawn_activity_flusher();
    control::run_control_client(supervisor).await
}
