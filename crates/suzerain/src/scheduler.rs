//! Placement: two-phase scheduling (research-informed, Kubernetes model).
//!
//! **Filter** — approved+online → hard pin → label subset match → resource
//! fit per dimension: `capacity − allocated − reserve ≥ request` (fit uses
//! *requests*, never live usage). GPU: at least `count` GPUs with enough
//! free VRAM (nvidia = measured, apple = unified free memory).
//!
//! **Score** — spread-only (LeastAllocated): normalized free fraction per
//! dimension, weighted cpu=1, mem=1, vram=1, disk=0.5; highest wins.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::{DaemonInfo, GpuKind};

use crate::control::ControlPlane;
use crate::store::DaemonRow;

pub struct Placement {
    pub endpoint_id: iroh::EndpointId,
    pub daemon_hostname: String,
    pub score: f64,
}

/// Everything the scheduler needs to decide.
pub struct Constraints {
    pub require: BTreeMap<String, String>,
    pub pin: Option<String>,
    pub manifest: AgentManifest,
    /// Daemons excluded from consideration (e.g. already failed this wake).
    pub exclude: Vec<String>,
}

/// Sum of resource requests of agents already on a daemon.
#[derive(Default, Clone, Copy)]
struct Allocated {
    vcpu: u32,
    memory_mib: u64,
    disk_mib: u64,
    /// Active-ish agent count (for the daemon's max_agents slot limit).
    agents: u32,
}

/// Host headroom reserved from fit checks (from castellan config).
#[derive(Default, Clone, Copy)]
struct Reserve {
    vcpu: u32,
    memory_mib: u64,
}

/// Choose a daemon for a new agent, or explain precisely why none fits.
pub async fn place(cp: &ControlPlane, constraints: &Constraints) -> Result<Placement> {
    let daemons: Vec<DaemonRow> = cp
        .store()
        .list_daemons()
        .await?
        .into_iter()
        .filter(|d| d.approved && d.online && !constraints.exclude.contains(&d.endpoint_id))
        .collect();
    if daemons.is_empty() {
        bail!("no online approved daemons");
    }

    // Hard pin short-circuits everything else.
    if let Some(want) = &constraints.pin {
        let d = daemons
            .iter()
            .find(|d| d.endpoint_id.starts_with(want.as_str()) || d.hostname == *want)
            .with_context(|| format!("no online approved daemon matching pin '{want}'"))?;
        return Ok(Placement {
            endpoint_id: d.endpoint_id.parse()?,
            daemon_hostname: d.hostname.clone(),
            score: 0.0,
        });
    }

    // Allocated requests per daemon. Only agents consuming live resources
    // count: Suspended agents are checkpointed to disk (slot + resources
    // freed) and Failed/Decommissioned consume nothing.
    let agents = cp.store().list_agents().await?;
    let allocated_of = |endpoint_id: &str| -> Allocated {
        agents
            .iter()
            .filter(|a| {
                a.daemon_endpoint_id == endpoint_id
                    && matches!(
                        a.state,
                        suzerain_protocol::AgentState::Provisioning
                            | suzerain_protocol::AgentState::Active
                            | suzerain_protocol::AgentState::Restoring
                    )
            })
            .fold(Allocated::default(), |mut acc, a| {
                acc.vcpu += a.manifest.resources.vcpu;
                acc.memory_mib += a.manifest.resources.memory_mib;
                acc.disk_mib += a.manifest.resources.disk_mib;
                acc.agents += 1;
                acc
            })
    };

    let reserve_of = |_d: &DaemonRow| -> Reserve {
        // Reserve travels in daemon config but is not yet reported; treat as
        // zero until a daemon advertises one (v1).
        Reserve::default()
    };

    let req = &constraints.manifest.resources;
    let mut scored: Vec<(&DaemonRow, f64)> = Vec::new();
    let mut rejections: Vec<String> = Vec::new();

    for d in &daemons {
        match fits(
            d,
            allocated_of(&d.endpoint_id),
            reserve_of(d),
            req,
            &constraints.require,
        ) {
            Ok(score) => scored.push((d, score)),
            Err(why) => rejections.push(format!(
                "{}…: {why}",
                &d.endpoint_id[..8.min(d.endpoint_id.len())]
            )),
        }
    }

    if scored.is_empty() {
        bail!(
            "no daemon can host '{}':\n  {}",
            constraints.manifest.name,
            rejections.join("\n  ")
        );
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (daemon, score) = scored[0];
    Ok(Placement {
        endpoint_id: daemon.endpoint_id.parse()?,
        daemon_hostname: daemon.hostname.clone(),
        score,
    })
}

/// Filter one candidate; returns its spread score when it fits.
fn fits(
    d: &DaemonRow,
    alloc: Allocated,
    reserve: Reserve,
    req: &suzerain_protocol::manifest::Resources,
    require: &BTreeMap<String, String>,
) -> Result<f64, String> {
    // Labels: subset match against effective labels.
    let labels = d.effective_labels();
    for (k, v) in require {
        match labels.get(k) {
            Some(have) if have == v => {}
            Some(have) => return Err(format!("label {k}={have} ≠ {v}")),
            None => return Err(format!("missing label {k}")),
        }
    }

    // Slot limit: a daemon at max_agents cannot take more, regardless of
    // spare resources.
    if alloc.agents >= d.max_agents {
        return Err(format!(
            "agent slots full ({}/{}) — an idle agent may be preempted to make room",
            alloc.agents, d.max_agents
        ));
    }

    let cap = d.capacity();
    let usage = d.usage();

    let vcpu_free = cap
        .vcpu_total
        .saturating_sub(alloc.vcpu)
        .saturating_sub(reserve.vcpu);
    if vcpu_free < req.vcpu {
        return Err(format!(
            "insufficient vcpu (need {}, have {})",
            req.vcpu, vcpu_free
        ));
    }
    let mem_free = cap
        .memory_mib_total
        .saturating_sub(alloc.memory_mib)
        .saturating_sub(reserve.memory_mib);
    if mem_free < req.memory_mib {
        return Err(format!(
            "insufficient memory (need {} MiB, have {} MiB)",
            req.memory_mib, mem_free
        ));
    }
    let disk_free = cap.disk_mib_total.saturating_sub(alloc.disk_mib);
    if disk_free < req.disk_mib {
        return Err(format!(
            "insufficient disk (need {} MiB, have {} MiB)",
            req.disk_mib, disk_free
        ));
    }

    // GPU fit + best per-GPU free VRAM (for scoring).
    let mut vram_best_free = 0u64;
    if let Some(gpu_req) = req.gpu {
        let usage_gpus = &usage.gpus;
        let mut matching = 0u32;
        for cap_gpu in &cap.gpus {
            let free = match cap_gpu.kind {
                GpuKind::Nvidia | GpuKind::Apple => usage_gpus
                    .iter()
                    .find(|g| g.index == cap_gpu.index)
                    .and_then(|g| g.vram_free_mib)
                    .or(cap_gpu.vram_free_mib)
                    .unwrap_or(0),
                GpuKind::Other => {
                    if gpu_req.vram_mib.is_some() {
                        continue;
                    }
                    u64::MAX // count-only request: any gpu kind works
                }
            };
            let vram_ok = gpu_req.vram_mib.map(|need| free >= need).unwrap_or(true);
            if vram_ok {
                matching += 1;
                vram_best_free =
                    vram_best_free.max(free.min(cap_gpu.vram_total_mib.unwrap_or(free)));
            }
        }
        if matching < gpu_req.count {
            return Err(format!(
                "no {} GPU(s) with ≥{} MiB free VRAM",
                gpu_req.count,
                gpu_req.vram_mib.unwrap_or(0)
            ));
        }
    }

    // Spread score: normalized free fraction per dimension.
    let frac = |free: u64, total: u64| {
        if total == 0 {
            0.0
        } else {
            free as f64 / total as f64
        }
    };
    let mut score = frac(vcpu_free as u64, cap.vcpu_total as u64)
        + frac(mem_free, cap.memory_mib_total)
        + 0.5 * frac(disk_free, cap.disk_mib_total);
    if let Some(gpu_req) = req.gpu {
        if let Some(need) = gpu_req.vram_mib {
            // Free VRAM beyond the requirement, normalized to the requirement.
            score += (vram_best_free as f64 - need as f64).max(0.0) / need.max(1) as f64;
        }
    }
    Ok(score)
}

/// DaemonInfo passthrough for future reserve reporting (kept for API shape).
#[allow(dead_code)]
fn _reserve_from_info(_info: &DaemonInfo) -> Reserve {
    Reserve::default()
}

/// Place, preempting idle agents when nothing fits: if no daemon can host
/// the request, suspend authoritatively-idle agents (longest-idle first,
/// policy-exempt and recently-woken agents excluded) on otherwise-feasible
/// daemons until one fits, then place again. The daemon re-validates
/// idleness at suspend time, so a candidate that went busy is skipped.
pub async fn place_or_preempt(cp: &ControlPlane, constraints: &Constraints) -> Result<Placement> {
    match place(cp, constraints).await {
        Ok(p) => Ok(p),
        Err(first_err) => {
            if preempt_idle(cp, constraints).await? {
                place(cp, constraints).await
            } else {
                Err(first_err)
            }
        }
    }
}

/// Try to free capacity on label/pin-feasible daemons by suspending idle
/// agents. Returns true if at least one agent was suspended.
async fn preempt_idle(cp: &ControlPlane, constraints: &Constraints) -> Result<bool> {
    let cfg = crate::retention::load_config().unwrap_or_default();
    let daemons: Vec<DaemonRow> = cp
        .store()
        .list_daemons()
        .await?
        .into_iter()
        .filter(|d| d.approved && d.online && !constraints.exclude.contains(&d.endpoint_id))
        .collect();
    let agents = cp.store().list_agents().await?;
    let mut suspended_any = false;

    for d in &daemons {
        // Label/pin feasibility (resource fit is what we're trying to fix).
        if let Some(want) = &constraints.pin {
            if !(d.endpoint_id.starts_with(want.as_str()) || d.hostname == *want) {
                continue;
            }
        }
        let labels = d.effective_labels();
        if !constraints
            .require
            .iter()
            .all(|(k, v)| labels.get(k) == Some(v))
        {
            continue;
        }

        // Candidates: longest-idle first.
        let mut candidates: Vec<&crate::store::AgentRow> = agents
            .iter()
            .filter(|a| {
                a.daemon_endpoint_id == d.endpoint_id && crate::lifecycle::is_preemptible(a, &cfg)
            })
            .collect();
        candidates.sort_by_key(|a| std::cmp::Reverse(crate::lifecycle::extrapolated_idle_secs(a)));

        let mut freed = Allocated::default();
        for candidate in candidates {
            if would_fit(d, &agents, constraints, freed) {
                break; // enough capacity projected
            }
            let Ok(daemon) = d.endpoint_id.parse() else {
                continue;
            };
            tracing::info!(
                agent = %candidate.name,
                daemon = %d.hostname,
                "preempting idle agent to free capacity"
            );
            let ack = cp
                .order(
                    &daemon,
                    &suzerain_protocol::order::Order::SuspendAgent {
                        agent_id: candidate.id,
                        only_if_idle: true,
                        not_since: candidate.activity_reported_at.clone(),
                    },
                )
                .await;
            match ack {
                Ok(a) if a.success => {
                    cp.store()
                        .update_agent_state(&candidate.id, suzerain_protocol::AgentState::Suspended)
                        .await?;
                    crate::audit::record(
                        "agent_preempt_suspend",
                        serde_json::json!({"name": candidate.name, "id": candidate.id, "for": constraints.manifest.name}),
                    )
                    .await;
                    freed.vcpu += candidate.manifest.resources.vcpu;
                    freed.memory_mib += candidate.manifest.resources.memory_mib;
                    freed.disk_mib += candidate.manifest.resources.disk_mib;
                    freed.agents += 1;
                    suspended_any = true;
                }
                Ok(a) => {
                    tracing::info!(
                        agent = %candidate.name,
                        "preemption refused (agent busy): {}",
                        a.message.unwrap_or_default()
                    );
                }
                Err(e) => {
                    tracing::warn!(agent = %candidate.name, "preemption order failed: {e:#}");
                }
            }
        }
    }
    Ok(suspended_any)
}

/// Would the request fit on `d` given the already-freed capacity?
fn would_fit(
    d: &DaemonRow,
    agents: &[crate::store::AgentRow],
    constraints: &Constraints,
    freed: Allocated,
) -> bool {
    let alloc = agents
        .iter()
        .filter(|a| {
            a.daemon_endpoint_id == d.endpoint_id
                && matches!(
                    a.state,
                    suzerain_protocol::AgentState::Provisioning
                        | suzerain_protocol::AgentState::Active
                        | suzerain_protocol::AgentState::Restoring
                )
        })
        .fold(Allocated::default(), |mut acc, a| {
            acc.vcpu += a.manifest.resources.vcpu;
            acc.memory_mib += a.manifest.resources.memory_mib;
            acc.disk_mib += a.manifest.resources.disk_mib;
            acc.agents += 1;
            acc
        });
    let effective = Allocated {
        vcpu: alloc.vcpu.saturating_sub(freed.vcpu),
        memory_mib: alloc.memory_mib.saturating_sub(freed.memory_mib),
        disk_mib: alloc.disk_mib.saturating_sub(freed.disk_mib),
        agents: alloc.agents.saturating_sub(freed.agents),
    };
    fits(
        d,
        effective,
        Reserve::default(),
        &constraints.manifest.resources,
        &constraints.require,
    )
    .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use suzerain_protocol::manifest::Resources;
    use suzerain_protocol::state::{GpuInfo, GpuKind, NodeCapacity, NodeUsage};

    fn daemon(labels: &[(&str, &str)], cap: NodeCapacity, usage: NodeUsage) -> DaemonRow {
        DaemonRow {
            endpoint_id: "aaaa1111".into(),
            approved: true,
            online: true,
            hostname: "host".into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            labels: serde_json::to_string(
                &labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<BTreeMap<_, _>>(),
            )
            .unwrap(),
            label_overrides: "{}".into(),
            max_agents: 8,
            last_seen: String::new(),
            capacity_json: serde_json::to_string(&cap).unwrap(),
            usage_json: serde_json::to_string(&usage).unwrap(),
        }
    }

    fn cap(vcpu: u32, mem: u64, disk: u64) -> NodeCapacity {
        NodeCapacity {
            vcpu_total: vcpu,
            memory_mib_total: mem,
            disk_mib_total: disk,
            gpus: vec![],
        }
    }

    fn req(vcpu: u32, mem: u64, disk: u64) -> Resources {
        Resources {
            vcpu,
            memory_mib: mem,
            disk_mib: disk,
            gpu: None,
        }
    }

    fn empty_usage() -> NodeUsage {
        NodeUsage::default()
    }

    #[test]
    fn label_subset_match() {
        let d = daemon(
            &[("zone", "office"), ("tier", "a")],
            cap(8, 16384, 100_000),
            empty_usage(),
        );
        let mut require = BTreeMap::new();
        require.insert("zone".to_string(), "office".to_string());
        assert!(fits(
            &d,
            Allocated::default(),
            Reserve::default(),
            &req(2, 2048, 5120),
            &require
        )
        .is_ok());
        require.insert("zone".to_string(), "home".to_string());
        assert!(fits(
            &d,
            Allocated::default(),
            Reserve::default(),
            &req(2, 2048, 5120),
            &require
        )
        .is_err());
    }

    #[test]
    fn fit_uses_allocated_requests() {
        let d = daemon(&[], cap(8, 16384, 100_000), empty_usage());
        let alloc = Allocated {
            vcpu: 6,
            memory_mib: 12_000,
            disk_mib: 90_000,
            agents: 3,
        };
        assert!(fits(
            &d,
            alloc,
            Reserve::default(),
            &req(2, 4096, 5120),
            &BTreeMap::new()
        )
        .is_ok());
        assert!(fits(
            &d,
            alloc,
            Reserve::default(),
            &req(3, 4096, 5120),
            &BTreeMap::new()
        )
        .is_err());
        assert!(fits(
            &d,
            alloc,
            Reserve::default(),
            &req(2, 4385, 5120),
            &BTreeMap::new()
        )
        .is_err());
        assert!(fits(
            &d,
            alloc,
            Reserve::default(),
            &req(2, 4096, 10_001),
            &BTreeMap::new()
        )
        .is_err());
    }

    #[test]
    fn gpu_vram_apple_unified() {
        let mut c = cap(8, 16384, 100_000);
        c.gpus = vec![GpuInfo {
            index: 0,
            kind: GpuKind::Apple,
            name: "Apple".into(),
            vram_total_mib: Some(16384),
            vram_free_mib: Some(16384),
        }];
        let mut usage = empty_usage();
        usage.gpus = vec![GpuInfo {
            index: 0,
            kind: GpuKind::Apple,
            name: "Apple".into(),
            vram_total_mib: None,
            vram_free_mib: Some(4000),
        }];
        let d = daemon(&[], c, usage);
        let mut r = req(2, 2048, 5120);
        r.gpu = Some(suzerain_protocol::manifest::GpuResources {
            count: 1,
            vram_mib: Some(3000),
        });
        assert!(fits(
            &d,
            Allocated::default(),
            Reserve::default(),
            &r,
            &BTreeMap::new()
        )
        .is_ok());
        r.gpu = Some(suzerain_protocol::manifest::GpuResources {
            count: 1,
            vram_mib: Some(5000),
        });
        let err = fits(
            &d,
            Allocated::default(),
            Reserve::default(),
            &r,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.contains("VRAM"), "{err}");
    }

    #[test]
    fn max_agents_slot_limit() {
        let d = daemon(&[], cap(8, 16384, 100_000), empty_usage()); // max_agents = 8
        let full = Allocated {
            agents: 8,
            ..Default::default()
        };
        assert!(fits(
            &d,
            full,
            Reserve::default(),
            &req(1, 128, 128),
            &BTreeMap::new()
        )
        .is_err());
        let room = Allocated {
            agents: 7,
            ..Default::default()
        };
        assert!(fits(
            &d,
            room,
            Reserve::default(),
            &req(1, 128, 128),
            &BTreeMap::new()
        )
        .is_ok());
    }

    #[test]
    fn spread_score_prefers_freer_node() {
        let free = daemon(&[], cap(8, 16384, 100_000), empty_usage());
        let busy = daemon(&[], cap(8, 16384, 100_000), empty_usage());
        let s_free = fits(
            &free,
            Allocated::default(),
            Reserve::default(),
            &req(2, 2048, 5120),
            &BTreeMap::new(),
        )
        .unwrap();
        let s_busy = fits(
            &busy,
            Allocated {
                vcpu: 4,
                memory_mib: 8000,
                disk_mib: 50_000,
                agents: 2,
            },
            Reserve::default(),
            &req(2, 2048, 5120),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(s_free > s_busy);
    }
}
