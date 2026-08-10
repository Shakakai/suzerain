//! Placement: pick which daemon runs a new agent. Phase 2 keeps this simple:
//! first online daemon with spare capacity; labels/constraints come later.

use anyhow::{Context, Result};

use crate::control::ControlPlane;
use crate::store::DaemonRow;

pub struct Placement {
    pub endpoint_id: iroh::EndpointId,
}

/// Choose a daemon for a new agent.
pub async fn place(cp: &ControlPlane, requested: Option<&str>) -> Result<Placement> {
    let daemons: Vec<DaemonRow> = cp
        .store()
        .list_daemons()
        .await?
        .into_iter()
        .filter(|d| d.approved && d.online)
        .collect();

    if let Some(want) = requested {
        let d = daemons
            .iter()
            .find(|d| d.endpoint_id.starts_with(want) || d.hostname == want)
            .with_context(|| format!("no online approved daemon matching '{want}'"))?;
        return Ok(Placement {
            endpoint_id: d.endpoint_id.parse()?,
        });
    }

    // Least-loaded: fewest agents relative to capacity.
    let agents = cp.store().list_agents().await?;
    let mut best: Option<(&DaemonRow, f64)> = None;
    for d in &daemons {
        let count = agents
            .iter()
            .filter(|a| a.daemon_endpoint_id == d.endpoint_id)
            .count() as f64;
        let load = count / d.max_agents.max(1) as f64;
        if load >= 1.0 {
            continue;
        }
        if best.map(|(_, l)| load < l).unwrap_or(true) {
            best = Some((d, load));
        }
    }
    let (daemon, _) = best.context("no online approved daemon has spare capacity")?;
    Ok(Placement {
        endpoint_id: daemon.endpoint_id.parse()?,
    })
}
