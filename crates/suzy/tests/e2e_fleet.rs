//! Full-fleet end-to-end test, driven entirely through the real Suzy UI:
//!
//!   suzerain (in-process, isolated $SUZERAIN_HOME)
//!     ←→ castellan (subprocess, isolated $CASTELLAN_HOME, real Gondolin VM)
//!     ←→ Suzy (headless via egui_kittest, isolated $SUZY_HOME)
//!
//! Steps: stand up a fresh suzerain (alongside any others on this machine —
//! all state/ports are per-$SUZERAIN_HOME and the web UI is disabled),
//! enroll + approve a castellan through Suzy, add an LLM provider key and a
//! git SSH key through Suzy's secrets view, create an agent through Suzy's
//! create form, ask "What's 2+2?" in Suzy's chat and expect a correct
//! answer, then destroy the agent and clean up every file and process.
//!
//! The agent talks to a REAL LLM (no mock): set KIMI_API_KEY.
//!
//! Prerequisites (the test SKIPS with a message when one is missing; set
//! SUZ_E2E_REQUIRED=1 to turn skips into failures):
//!   - KIMI_API_KEY in the environment
//!   - `cargo build -p castellan` (target/debug/castellan)
//!   - node on PATH (or mise shims) + `npm ci --prefix tools/gondolin-driver`
//!   - ssh-keygen; on Linux also qemu-system + KVM (or GONDOLIN_ACCEL=tcg)
//!   - first run downloads Gondolin guest assets (~600MB) and provisions the
//!     VM over the network (apk/npm/pi install) — expect several minutes
//!
//! Run:
//!   cargo test -p suzy --test e2e_fleet -- --nocapture --test-threads=1

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use suzy::config::{Config, WorkspaceCfg};
use suzy::SuzyApp;

const AGENT: &str = "e2e-math";
const PROVIDER: &str = "kimi-coding";

const MANIFEST: &str = r#"name = "e2e-math"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "kimi-coding", id = "kimi-for-coding" }

[secrets]
providers = ["kimi-coding"]
"#;

// ── skip / prerequisite checks ───────────────────────────────────────────

fn skip_or_fail(reason: &str) -> bool {
    if std::env::var("SUZ_E2E_REQUIRED").as_deref() == Ok("1") {
        panic!("SUZ_E2E_REQUIRED=1 but prerequisite is missing: {reason}");
    }
    eprintln!("skipping e2e_fleet: {reason}");
    true
}

fn have_cmd(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn workspace_root() -> PathBuf {
    // crates/suzy → crates → repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

/// Returns None when a prerequisite is missing (after logging a skip).
fn check_prerequisites() -> Option<PathBuf> {
    if std::env::var("KIMI_API_KEY")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        skip_or_fail("KIMI_API_KEY not set (the test agent talks to a real LLM)");
        return None;
    }
    let root = workspace_root();
    let castellan = root.join("target/debug/castellan");
    if !castellan.exists()
        && skip_or_fail("target/debug/castellan missing — run `cargo build -p castellan`")
    {
        return None;
    }
    if !root.join("tools/gondolin-driver/src/index.mjs").exists()
        && skip_or_fail("gondolin-driver script missing")
    {
        return None;
    }
    if !root.join("tools/gondolin-driver/node_modules").exists()
        && skip_or_fail(
            "gondolin-driver deps missing — run `npm ci --prefix tools/gondolin-driver`",
        )
    {
        return None;
    }
    if !have_cmd("node") && skip_or_fail("node not on PATH") {
        return None;
    }
    if !have_cmd("ssh-keygen") && skip_or_fail("ssh-keygen not on PATH") {
        return None;
    }
    if cfg!(target_os = "linux")
        && !have_cmd("qemu-system-x86_64")
        && !have_cmd("qemu-system-aarch64")
        && skip_or_fail("qemu-system missing (needed for Gondolin VMs on Linux)")
    {
        return None;
    }
    Some(castellan)
}

// ── cleanup guard ────────────────────────────────────────────────────────

/// Kills the castellan process group (castellan → gondolin-driver → VM) and
/// removes the temp state dirs. On test failure the temp root is PRESERVED
/// (logs!) and its path printed.
struct Guard {
    root: PathBuf,
    castellan: Option<std::process::Child>,
    ok: bool,
}

impl Guard {
    fn kill_castellan(&mut self) {
        let Some(mut child) = self.castellan.take() else {
            return;
        };
        let pgid = child.id() as libc::pid_t; // setsid'd at spawn: pgid == pid
        unsafe {
            libc::killpg(pgid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    unsafe {
                        libc::killpg(pgid, libc::SIGKILL);
                    }
                    let _ = child.wait();
                    break;
                }
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.kill_castellan();
        if self.ok {
            let _ = std::fs::remove_dir_all(&self.root);
        } else {
            eprintln!(
                "e2e_fleet FAILED — keeping state dir for inspection: {}",
                self.root.display()
            );
        }
    }
}

// ── pump helpers (same pattern as tests/ui.rs) ───────────────────────────

fn pump_until(
    h: &mut Harness<SuzyApp>,
    timeout: Duration,
    what: &str,
    pred: impl Fn(&Harness<SuzyApp>) -> bool,
) {
    let start = Instant::now();
    loop {
        h.step();
        if pred(h) {
            return;
        }
        if start.elapsed() > timeout {
            let ws = h.state().workspaces.first();
            let status = h.state().status_msg.clone();
            panic!(
                "timed out waiting for: {what} \
                 (ws error: {:?}, endpoint: {:?}, status: {status:?})",
                ws.map(|w| w.error.clone()),
                ws.map(|w| w.endpoint.clone()),
            );
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn has_label(h: &Harness<SuzyApp>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}

fn has_label_containing(h: &Harness<SuzyApp>, label: &str) -> bool {
    h.query_all_by_label_contains(label).next().is_some()
}

// ── the test ─────────────────────────────────────────────────────────────

#[test]
fn fleet_e2e() {
    let Some(castellan_bin) = check_prerequisites() else {
        return;
    };
    let kimi_key = std::env::var("KIMI_API_KEY").unwrap();

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "suzerain=info,suzy=info,suzerain_client=info".into()),
        )
        .try_init();
    let _ =
        rustls::crypto::CryptoProvider::install_default(rustls::crypto::ring::default_provider());

    // ── 1. Fresh, fully isolated suzerain (temp $SUZERAIN_HOME, web UI off,
    //       ephemeral iroh ports) — never touches other instances. ─────────
    // NB: keep the root path SHORT — $CASTELLAN_HOME/castellan.sock must fit
    // in SUN_LEN (104 bytes); macOS's per-user temp dir is far too long.
    let tmp = PathBuf::from("/tmp").join(format!(
        "suzy-e2e-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 0x1_0000_0000
    ));
    let suzerain_home = tmp.join("suzerain");
    let castellan_home = tmp.join("castellan");
    let suzy_home = tmp.join("suzy");
    std::fs::create_dir_all(&suzerain_home).unwrap();
    std::fs::create_dir_all(&castellan_home).unwrap();
    std::fs::create_dir_all(&suzy_home).unwrap();

    let mut guard = Guard {
        root: tmp.clone(),
        castellan: None,
        ok: false,
    };

    std::env::set_var("SUZERAIN_HOME", &suzerain_home);
    std::env::set_var("SOPS_AGE_KEY_FILE", tmp.join("age-keys.txt"));
    std::env::set_var("SUZY_HOME", &suzy_home);

    // Suzy's operator identity: pre-generated so the control plane can
    // allowlist it before Suzy starts.
    let suzy_key = suzerain_client::iroh::SecretKey::generate();
    std::fs::write(suzy_home.join("iroh.key"), suzy_key.to_bytes()).unwrap();

    // Web UI disabled: no fixed port, so nothing can clash with a real
    // suzerain running on this machine.
    std::fs::write(
        suzerain_home.join("suzerain.toml"),
        "[web]\nenabled = false\n",
    )
    .unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let cp = rt
        .block_on(async {
            suzerain::secrets::load()?;
            let store = suzerain::store::Store::open().await?;
            suzerain::control::start(store, vec![suzy_key.public()])
                .await
                .map(std::sync::Arc::new)
        })
        .expect("start control plane");

    // The secrets store file is created on first write; create it now (empty)
    // so Suzy's secrets view leaves its "store not set up" state.
    suzerain::secrets::set_provider("__e2e_bootstrap", "x").unwrap();
    suzerain::secrets::delete_provider("__e2e_bootstrap").unwrap();

    // Wait until the endpoint has at least one dialable address.
    let addr = rt.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let addr = cp.addr();
            if !addr.addrs.is_empty() {
                break addr;
            }
            if Instant::now() > deadline {
                panic!("suzerain endpoint never got a dialable address");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    let sid = cp.endpoint_id().to_string();
    eprintln!("suzerain up: {sid}");

    // ── 2. Castellan: init against this suzerain, then run (unapproved). ──
    let mise_shims = format!(
        "{}/.local/share/mise/shims",
        std::env::var("HOME").unwrap_or_default()
    );
    let cast_env = [
        (
            "CASTELLAN_HOME".to_string(),
            castellan_home.to_string_lossy().into(),
        ),
        (
            "PATH".to_string(),
            format!("{mise_shims}:{}", std::env::var("PATH").unwrap_or_default()),
        ),
    ];
    let init = std::process::Command::new(&castellan_bin)
        .args(["init", "--suzerain", &sid])
        .envs(cast_env.iter().cloned())
        .output()
        .expect("castellan init");
    assert!(
        init.status.success(),
        "castellan init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // Own process group: cleanup kills castellan + driver + VM together,
    // never other fleets on this machine.
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&castellan_bin);
    cmd.arg("run")
        .envs(cast_env.iter().cloned())
        .stdout(std::fs::File::create(tmp.join("castellan.log")).unwrap())
        .stderr(std::fs::File::create(tmp.join("castellan.err")).unwrap());
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    guard.castellan = Some(cmd.spawn().expect("castellan run"));

    // ── 3. Suzy: headless app connected to this suzerain. ─────────────────
    let mut cfg = Config::default();
    cfg.workspaces.push(WorkspaceCfg {
        name: "e2e".into(),
        endpoint_id: sid.clone(),
        test_addr: Some(addr),
    });
    let config_path = tmp.join("suzy-config.toml");
    let app_path = config_path.clone();
    let mut harness: Harness<'static, SuzyApp> = Harness::new_eframe(move |cc| {
        let rt2 = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        SuzyApp::with_config(cc, rt2, cfg.clone(), app_path)
    });

    pump_until(
        &mut harness,
        Duration::from_secs(60),
        "suzy connected",
        |h| {
            h.state()
                .workspaces
                .first()
                .is_some_and(|w| w.endpoint.is_some())
        },
    );

    // Approve the castellan through Suzy's Castellans view (pending flow).
    harness.get_by_label("🖥 Castellans").click();
    pump_until(
        &mut harness,
        Duration::from_secs(120),
        "pending castellan enrollment",
        |h| has_label(h, "Approve"),
    );
    harness.get_by_label("Approve").click();
    pump_until(
        &mut harness,
        Duration::from_secs(180),
        "castellan online",
        |h| has_label_containing(h, "🟢"),
    );
    eprintln!("castellan approved + online");

    // ── 4. Secrets: LLM provider key + git SSH key, via Suzy. ─────────────
    harness.get_by_label("🔑 Secrets").click();
    pump_until(
        &mut harness,
        Duration::from_secs(30),
        "secrets view loaded",
        |h| h.state().secrets.get(&0).is_some_and(|s| s.value.is_some()),
    );

    // Provider key (values injected via field state; the submit button and
    // the whole network/store path are real).
    {
        let st = harness.state_mut();
        let s = st.secrets.entry(0).or_default();
        s.new_provider_id = PROVIDER.into();
        s.new_provider_value = kimi_key.clone();
    }
    harness.step();
    harness
        .get_all_by_label("set")
        .next()
        .expect("provider set button")
        .click();
    pump_until(
        &mut harness,
        Duration::from_secs(30),
        "provider key stored",
        |h| {
            h.state()
                .status_msg
                .as_ref()
                .is_some_and(|m| m.contains("set provider: ok"))
        },
    );

    // Throwaway git SSH key.
    let key_path = tmp.join("id_ed25519");
    let out = std::process::Command::new("ssh-keygen")
        .args([
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "suzy-e2e",
            "-f",
            key_path.to_str().unwrap(),
        ])
        .output()
        .expect("ssh-keygen");
    assert!(
        out.status.success(),
        "ssh-keygen failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ssh_key = std::fs::read_to_string(&key_path).unwrap();
    harness
        .state_mut()
        .secrets
        .entry(0)
        .or_default()
        .deploy_key_value = ssh_key;
    harness.step();
    harness.get_by_label("upload").click();
    pump_until(
        &mut harness,
        Duration::from_secs(30),
        "ssh key stored",
        |h| {
            h.state()
                .status_msg
                .as_ref()
                .is_some_and(|m| m.contains("upload ssh key: ok"))
        },
    );

    // Verify both landed in the inventory (refresh, then read the rows).
    harness.get_by_label("↻").click();
    pump_until(
        &mut harness,
        Duration::from_secs(30),
        "secrets inventory",
        |h| {
            let Some(v) = h.state().secrets.get(&0).and_then(|s| s.value.clone()) else {
                return false;
            };
            let entries = v["entries"].as_array().cloned().unwrap_or_default();
            entries
                .iter()
                .any(|e| e["kind"] == "provider" && e["name"] == PROVIDER)
                && entries.iter().any(|e| e["kind"] == "git")
        },
    );
    eprintln!("secrets stored: provider key + ssh key");

    // ── 5. Create the agent through Suzy. ──────────────────────────────────
    harness.get_by_label("✚ Create agent").click();
    harness.step();
    {
        let form = &mut harness.state_mut().create_form;
        form.toml_text = MANIFEST.to_string();
        form.toml_edited = true; // keep the form from regenerating over it
    }
    harness.step();
    harness
        .get_all_by_label("✚ Create agent")
        .last()
        .expect("dialog submit button")
        .click();
    pump_until(
        &mut harness,
        Duration::from_secs(60),
        "agent create accepted",
        |h| {
            h.state()
                .status_msg
                .as_ref()
                .is_some_and(|m| m.contains("provisioning"))
        },
    );

    // Provisioning boots a real VM and installs tooling over the network —
    // allow generous time, but fail fast if the agent dies.
    let provision_timeout = std::env::var("SUZ_E2E_PROVISION_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(20 * 60));
    let start = Instant::now();
    loop {
        harness.step();
        if has_label_containing(&harness, AGENT) {
            // Open the agent tab: the header shows its live status.
            if harness.get_all_by_label("● e2e-math").next().is_some() {
                harness.get_by_label("● e2e-math").click();
            }
        }
        if has_label(&harness, "● idle") {
            break;
        }
        if has_label(&harness, "● failed") {
            panic!(
                "agent provisioning failed — see {}",
                tmp.join("castellan.log").display()
            );
        }
        assert!(
            start.elapsed() <= provision_timeout,
            "timed out waiting for agent provisioning ({}s)",
            provision_timeout.as_secs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("agent provisioned and idle");

    // ── 6. Ask the agent a question through Suzy's chat. ───────────────────
    if !has_label(&harness, "● idle") {
        harness.get_by_label("● e2e-math").click();
        pump_until(&mut harness, Duration::from_secs(120), "agent idle", |h| {
            has_label(h, "● idle")
        });
    }
    let input = harness.get_by_role(egui::accesskit::Role::MultilineTextInput);
    input.focus();
    input.type_text("What's 2+2? Reply with just the number.");
    input.key_press(egui_kittest::kittest::Key::Enter);

    let answered = |h: &Harness<SuzyApp>| {
        h.state()
            .chats
            .get(&(0, AGENT.to_string()))
            .is_some_and(|c| {
                c.items.iter().any(|item| {
                    matches!(item, suzy::chat::ChatItem::Assistant(parts)
                        if parts.iter().any(|p| matches!(p, suzy::chat::Part::Text(t) if t.contains('4'))))
                })
            })
    };
    pump_until(
        &mut harness,
        Duration::from_secs(240),
        "assistant answer containing \"4\"",
        answered,
    );
    let reply = harness
        .state()
        .chats
        .get(&(0, AGENT.to_string()))
        .map(|c| format!("{:?}", c.items));
    eprintln!("agent answered: {reply:?}");

    // ── 7. Destroy the agent through Suzy; clean everything up. ────────────
    harness.get_by_label("⚙ Details").click();
    pump_until(&mut harness, Duration::from_secs(60), "details tab", |h| {
        has_label_containing(h, "manifest (read-only")
    });
    harness.get_by_label("🗑 destroy").click();
    harness.step();
    harness.get_by_label("Destroy").click();
    pump_until(
        &mut harness,
        Duration::from_secs(180),
        "agent destroyed",
        |h| !has_label_containing(h, AGENT),
    );

    drop(harness);
    guard.kill_castellan();
    guard.ok = true; // Drop removes the temp dirs
    eprintln!("E2E PASSED");
}
