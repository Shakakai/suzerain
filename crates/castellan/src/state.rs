//! Local agent registry: per-agent state persisted under the castellan data
//! dir. Phase 2 replaces the authoritative copy with suzerain's registry;
//! this stays as the daemon-local cache.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::AgentState;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: Uuid,
    pub name: String,
    pub manifest: AgentManifest,
    pub state: AgentState,
    pub created_at: String,
    /// pi session file path *inside the guest* (under /agent/sessions),
    /// recorded after first start for resume.
    #[serde(default)]
    pub session_file: Option<String>,
    /// Host path of the Gondolin disk checkpoint (same-host suspend/boot
    /// fast path).
    #[serde(default)]
    pub checkpoint: Option<String>,
    /// Wall-clock time of the agent's last meaningful activity (RFC3339).
    /// Flushed periodically by the supervisor so the inactivity clock
    /// survives a daemon restart.
    #[serde(default)]
    pub last_activity_at: Option<String>,
}

/// Root data dir for this daemon. Castellan shares the fleet home with
/// suzerain: $CASTELLAN_HOME, else $SUZERAIN_HOME, else the default
/// `~/.local/share/suzerain`. File names inside are disjoint
/// (castellan.toml / castellan.key / castellan.sock / agents/ vs
/// suzerain.toml / suzerain.key / suzerain.sock / suzerain.db / …).
pub fn data_dir() -> PathBuf {
    // Test-only, per-thread override: `std::env::set_var` is process-global,
    // so two tests pointing CASTELLAN_HOME at their own temp dirs (this
    // module's tests, and e.g. supervisor.rs's) race across OS threads under
    // the default parallel test harness. A thread-local avoids that
    // entirely — each test thread sees only its own override — without
    // changing behavior for any non-test caller.
    #[cfg(test)]
    {
        if let Some(dir) = tests::TEST_HOME_OVERRIDE.with(|c| c.borrow().clone()) {
            return dir;
        }
    }
    if let Ok(dir) = std::env::var("CASTELLAN_HOME") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("SUZERAIN_HOME") {
        return PathBuf::from(dir);
    }
    let dir = dirs_home().join(".local/share/suzerain");
    migrate_legacy_default_dir(&dir);
    dir
}
/// Before the shared fleet home, castellan's default was
/// `~/.local/share/castellan`. Move its contents into the fleet home once,
/// renaming the two files that would overlap with suzerain's. Runtime
/// residue (socket, lock) is left behind. Only runs for the pure default
/// layout — explicit $CASTELLAN_HOME/$SUZERAIN_HOME installs are untouched.
fn migrate_legacy_default_dir(new_home: &std::path::Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let old = dirs_home().join(".local/share/castellan");
        if !old.is_dir() || old == new_home {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&old) else {
            return;
        };
        let mut moved = 0usize;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let mapped = match name.as_str() {
                "config.toml" => "castellan.toml",
                "identity.key" => "castellan.key",
                "castellan.sock" | "castellan.lock" => continue, // runtime residue
                other => other,
            };
            let dest = new_home.join(mapped);
            if dest.exists() {
                tracing::warn!(
                    "legacy castellan dir: keeping {}, a {} already exists in the fleet home",
                    entry.path().display(),
                    mapped
                );
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            match std::fs::rename(entry.path(), &dest) {
                Ok(()) => moved += 1,
                Err(err) => tracing::warn!(
                    "legacy castellan dir: moving {} failed ({err:#})",
                    entry.path().display()
                ),
            }
        }
        if moved > 0 {
            tracing::info!(
                "migrated {moved} entr(y/ies) from {} into the shared fleet home {}",
                old.display(),
                new_home.display()
            );
        }
        // Best-effort: only succeeds once nothing (but skipped residue) remains.
        let _ = std::fs::remove_file(old.join("castellan.sock"));
        let _ = std::fs::remove_file(old.join("castellan.lock"));
        let _ = std::fs::remove_dir(&old);
    });
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

pub fn agent_dir(id: &Uuid) -> PathBuf {
    data_dir().join("agents").join(id.to_string())
}

/// Per-agent dir layout (host side; `guest/` is mounted at /agent in the VM).
pub struct AgentPaths {
    pub root: PathBuf,
    pub guest: PathBuf,
    pub workspace: PathBuf,
    pub pi_home: PathBuf,
    pub sessions: PathBuf,
    pub extensions: PathBuf,
}

impl AgentPaths {
    pub fn for_agent(id: &Uuid) -> Self {
        let root = agent_dir(id);
        let guest = root.join("guest");
        Self {
            workspace: guest.join("workspace"),
            pi_home: guest.join("pi-home"),
            sessions: guest.join("sessions"),
            extensions: guest.join("pi-home").join("extensions"),
            guest,
            root,
        }
    }

    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// Where the VM disk checkpoint lives for same-host suspend/boot.
    pub fn checkpoint_path(&self) -> PathBuf {
        self.root.join("checkpoint")
    }
}

pub async fn save(record: &AgentRecord) -> Result<()> {
    let paths = AgentPaths::for_agent(&record.id);
    let tmp = paths.root.join("state.json.tmp");
    tokio::fs::write(&tmp, serde_json::to_string_pretty(record)?).await?;
    tokio::fs::rename(&tmp, paths.state_file()).await?;
    Ok(())
}

pub async fn load(id: &Uuid) -> Result<AgentRecord> {
    let path = AgentPaths::for_agent(id).state_file();
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

pub async fn list() -> Result<Vec<AgentRecord>> {
    let mut out = Vec::new();
    let agents = data_dir().join("agents");
    let mut entries = match tokio::fs::read_dir(&agents).await {
        Ok(e) => e,
        Err(_) => return Ok(out),
    };
    while let Some(entry) = entries.next_entry().await? {
        let state = entry.path().join("state.json");
        if let Ok(text) = tokio::fs::read_to_string(&state).await {
            if let Ok(record) = serde_json::from_str::<AgentRecord>(&text) {
                out.push(record);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub async fn find_by_name(name: &str) -> Result<AgentRecord> {
    for record in list().await? {
        if record.name == name {
            return Ok(record);
        }
    }
    bail!("no agent named '{name}'")
}

/// Resolve by uuid, exact name, or unique id-prefix.
pub async fn find(id_or_name: &str) -> Result<AgentRecord> {
    if let Ok(id) = Uuid::parse_str(id_or_name) {
        return load(&id).await;
    }
    if let Ok(record) = find_by_name(id_or_name).await {
        return Ok(record);
    }
    let matches: Vec<AgentRecord> = list()
        .await?
        .into_iter()
        .filter(|r| r.id.to_string().starts_with(id_or_name))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no agent matching '{id_or_name}'"),
        _ => bail!("'{id_or_name}' matches multiple agents"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_manifest() -> AgentManifest {
        toml::from_str(
            r#"
name = "agent-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "anthropic", id = "claude-sonnet-4-5" }
"#,
        )
        .unwrap()
    }

    fn sample_record(id: Uuid, name: &str) -> AgentRecord {
        AgentRecord {
            id,
            name: name.to_string(),
            manifest: minimal_manifest(),
            state: AgentState::Active,
            created_at: "2026-08-27T00:00:00Z".to_string(),
            session_file: Some("/agent/sessions/abc".to_string()),
            checkpoint: None,
            last_activity_at: None,
        }
    }

    // Per-thread override consulted by `data_dir()` (see there for why: a
    // process-global `CASTELLAN_HOME` env var would race against any other
    // test in this binary that also points it at a temp dir, e.g.
    // supervisor.rs's `destroy_purges_the_secret_bundle`).
    thread_local! {
        pub(super) static TEST_HOME_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Point this thread's `data_dir()` at a fresh temp dir for the duration
    /// of `f`, cleaning up afterwards.
    async fn with_temp_home<F, Fut, T>(f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let dir = std::env::temp_dir().join(format!("castellan-state-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        TEST_HOME_OVERRIDE.with(|c| *c.borrow_mut() = Some(dir.clone()));
        let result = f().await;
        TEST_HOME_OVERRIDE.with(|c| *c.borrow_mut() = None);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    #[test]
    fn agent_paths_layout_is_relative_to_agent_root() {
        let id = Uuid::new_v4();
        let paths = AgentPaths::for_agent(&id);
        assert!(paths.root.ends_with(id.to_string()));
        assert_eq!(paths.guest, paths.root.join("guest"));
        assert_eq!(paths.workspace, paths.guest.join("workspace"));
        assert_eq!(paths.pi_home, paths.guest.join("pi-home"));
        assert_eq!(paths.sessions, paths.guest.join("sessions"));
        assert_eq!(paths.extensions, paths.pi_home.join("extensions"));
        assert_eq!(paths.state_file(), paths.root.join("state.json"));
        assert_eq!(paths.checkpoint_path(), paths.root.join("checkpoint"));
    }

    #[tokio::test]
    async fn save_then_load_round_trips_the_record() {
        with_temp_home(|| async {
            let id = Uuid::new_v4();
            let record = sample_record(id, "agent-1");
            tokio::fs::create_dir_all(&AgentPaths::for_agent(&id).root)
                .await
                .unwrap();
            save(&record).await.unwrap();

            let loaded = load(&id).await.unwrap();
            assert_eq!(loaded.id, record.id);
            assert_eq!(loaded.name, record.name);
            assert_eq!(loaded.session_file, record.session_file);
            assert!(matches!(loaded.state, AgentState::Active));
        })
        .await;
    }

    #[tokio::test]
    async fn load_missing_agent_errors_instead_of_panicking() {
        with_temp_home(|| async {
            let err = load(&Uuid::new_v4()).await.unwrap_err();
            // anyhow::Context wraps the io error with the path; just check
            // we got a real error rather than a default/empty record.
            assert!(!err.to_string().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn list_is_sorted_by_name_and_skips_corrupt_entries() {
        with_temp_home(|| async {
            let a = sample_record(Uuid::new_v4(), "zebra");
            let b = sample_record(Uuid::new_v4(), "apple");
            for r in [&a, &b] {
                tokio::fs::create_dir_all(&AgentPaths::for_agent(&r.id).root)
                    .await
                    .unwrap();
                save(r).await.unwrap();
            }
            // A corrupt/partial state.json (e.g. a torn write) must be
            // skipped rather than failing the whole listing.
            let corrupt_id = Uuid::new_v4();
            let corrupt_dir = AgentPaths::for_agent(&corrupt_id).root;
            tokio::fs::create_dir_all(&corrupt_dir).await.unwrap();
            tokio::fs::write(corrupt_dir.join("state.json"), b"{not valid json")
                .await
                .unwrap();

            let listed = list().await.unwrap();
            let names: Vec<&str> = listed.iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, vec!["apple", "zebra"]);
        })
        .await;
    }

    #[tokio::test]
    async fn list_on_missing_agents_dir_returns_empty() {
        with_temp_home(|| async {
            let listed = list().await.unwrap();
            assert!(listed.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn find_by_name_errors_when_absent() {
        with_temp_home(|| async {
            let err = find_by_name("nope").await.unwrap_err();
            assert!(err.to_string().contains("nope"));
        })
        .await;
    }

    #[tokio::test]
    async fn find_resolves_by_uuid_exact_name_and_unique_prefix() {
        with_temp_home(|| async {
            let id = Uuid::new_v4();
            let record = sample_record(id, "unique-agent");
            tokio::fs::create_dir_all(&AgentPaths::for_agent(&id).root)
                .await
                .unwrap();
            save(&record).await.unwrap();

            // By UUID.
            assert_eq!(find(&id.to_string()).await.unwrap().id, id);
            // By exact name.
            assert_eq!(find("unique-agent").await.unwrap().id, id);
            // By unique id-prefix.
            let prefix = &id.to_string()[..8];
            assert_eq!(find(prefix).await.unwrap().id, id);
        })
        .await;
    }

    #[tokio::test]
    async fn find_errors_on_ambiguous_prefix() {
        with_temp_home(|| async {
            // Craft two ids sharing their first 8 hex chars (the top 32
            // bits) so an id-prefix lookup is genuinely ambiguous.
            let high: u128 = 0x1234_5678_u128 << 96;
            let id_a = Uuid::from_u128(high | 1);
            let id_b = Uuid::from_u128(high | 2);
            let id_a_str = id_a.to_string();
            let id_b_str = id_b.to_string();
            let shared_prefix = &id_a_str[..8];
            assert_eq!(shared_prefix, &id_b_str[..8]);
            for (id, name) in [(id_a, "a"), (id_b, "b")] {
                let record = sample_record(id, name);
                tokio::fs::create_dir_all(&AgentPaths::for_agent(&id).root)
                    .await
                    .unwrap();
                save(&record).await.unwrap();
            }

            let err = find(shared_prefix).await.unwrap_err();
            assert!(err.to_string().contains("multiple"));
        })
        .await;
    }
}
