//! Node probing: static capacity + dynamic usage, reported at registration
//! and refreshed via heartbeat acks. macOS + Linux.

use suzerain_protocol::state::{GpuInfo, GpuKind, NodeCapacity, NodeUsage};

/// Static node capacity, probed once at startup.
pub fn capacity(data_dir: &std::path::Path) -> NodeCapacity {
    let (memory_mib_total, _) = memory();
    let (disk_mib_total, _) = disk(data_dir);
    NodeCapacity {
        vcpu_total: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        memory_mib_total,
        disk_mib_total,
        gpus: detect_gpus(memory_mib_total),
    }
}

/// Dynamic usage snapshot.
pub fn usage(data_dir: &std::path::Path, capacity: &NodeCapacity) -> NodeUsage {
    let (_, memory_mib_free) = memory();
    let (_, disk_mib_free) = disk(data_dir);
    NodeUsage {
        memory_mib_free,
        cpu_load1: load1(),
        disk_mib_free,
        gpus: refresh_gpu_free(&capacity.gpus, memory_mib_free),
    }
}

/// (total_mib, free_mib) of system memory.
fn memory() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            let mut total = 0u64;
            let mut avail = 0u64;
            for line in text.lines() {
                if let Some(v) = line.strip_prefix("MemTotal:") {
                    total = parse_kib(v);
                } else if let Some(v) = line.strip_prefix("MemAvailable:") {
                    avail = parse_kib(v);
                }
            }
            if total > 0 {
                return (total / 1024, avail / 1024);
            }
        }
        (0, 0)
    }
    #[cfg(target_os = "macos")]
    {
        let total = sysctl_u64("hw.memsize") / (1024 * 1024);
        // vm_stat: free + inactive pages approximate available memory.
        let page = sysctl_u64("hw.pagesize").max(4096);
        let mut free = 0u64;
        if let Ok(out) = std::process::Command::new("vm_stat").output() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let v: u64 = line
                    .split_whitespace()
                    .last()
                    .and_then(|s| s.trim_end_matches('.').parse().ok())
                    .unwrap_or(0);
                if line.starts_with("Pages free") || line.starts_with("Pages inactive") {
                    free += v;
                }
            }
        }
        (total, free * page / (1024 * 1024))
    }
}

#[cfg(target_os = "linux")]
fn parse_kib(s: &str) -> u64 {
    s.split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

#[cfg(target_os = "macos")]
fn sysctl_u64(name: &str) -> u64 {
    std::process::Command::new("sysctl")
        .args(["-n", name])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0)
}

fn load1() -> f64 {
    let mut avg = [0f64; 3];
    let rc = unsafe { libc::getloadavg(avg.as_mut_ptr(), 3) };
    if rc >= 1 {
        avg[0]
    } else {
        0.0
    }
}

/// (total_mib, free_mib) of the filesystem holding `dir`.
fn disk(dir: &std::path::Path) -> (u64, u64) {
    let stat = fs2::statvfs(dir).ok();
    let total = stat
        .as_ref()
        .map(|s| s.total_space() / (1024 * 1024))
        .unwrap_or(0);
    let free = stat.map(|s| s.free_space() / (1024 * 1024)).unwrap_or(0);
    (total, free)
}

fn detect_gpus(memory_mib_total: u64) -> Vec<GpuInfo> {
    // nvidia: real VRAM via nvidia-smi.
    if let Ok(out) = std::process::Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut gpus = Vec::new();
            for line in text.lines() {
                let parts: Vec<&str> = line.split(',').map(str::trim).collect();
                if parts.len() == 4 {
                    gpus.push(GpuInfo {
                        index: parts[0].parse().unwrap_or(0),
                        kind: GpuKind::Nvidia,
                        name: parts[1].to_string(),
                        vram_total_mib: parts[2].parse().ok(),
                        vram_free_mib: parts[3].parse().ok(),
                    });
                }
            }
            if !gpus.is_empty() {
                return gpus;
            }
        }
    }
    // Apple Silicon: unified memory (vram = system memory).
    if std::env::consts::OS == "macos" && std::env::consts::ARCH == "aarch64" {
        return vec![GpuInfo {
            index: 0,
            kind: GpuKind::Apple,
            name: "Apple Unified Memory GPU".into(),
            vram_total_mib: Some(memory_mib_total),
            vram_free_mib: Some(memory_mib_total), // refined at usage sampling
        }];
    }
    Vec::new()
}

fn refresh_gpu_free(gpus: &[GpuInfo], memory_mib_free: u64) -> Vec<GpuInfo> {
    if gpus.iter().any(|g| g.kind == GpuKind::Nvidia) {
        // nvidia-smi reports live free VRAM; total arg is unused for nvidia.
        return detect_gpus(0);
    }
    gpus.iter()
        .map(|g| match g.kind {
            GpuKind::Apple => GpuInfo {
                vram_free_mib: Some(memory_mib_free),
                ..g.clone()
            },
            _ => g.clone(),
        })
        .collect()
}
