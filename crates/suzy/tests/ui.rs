//! Headless UI tests: the real Suzy app (egui_kittest harness) against the
//! mock control plane (tests/common). Every major user-facing path is
//! driven through the actual UI — initial setup (welcome screen, adding a
//! workspace, switching between several), fleet sidebar/dashboard, chat
//! round trip, shell tab round trip, create-agent (default + explicit
//! provider selection), castellan approval + label editing, secrets
//! (reveal, add, delete), logs/details, activity, agent destroy, theme,
//! removing a workspace, needs-attention flagging, and multi-agent prompt
//! routing — plus the cancel path for every destructive/modal dialog.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::Mock;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use suzy::config::{Config, WorkspaceCfg};
use suzy::terminal::TermInput;
use suzy::SuzyApp;

const T: Duration = Duration::from_secs(20);

struct Fixture {
    harness: Harness<'static, SuzyApp>,
    mock_state: std::sync::Arc<std::sync::Mutex<common::MockState>>,
    _rt: tokio::runtime::Runtime,
    _config_path: PathBuf,
}

fn temp_config(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "suzy-uitest-{}-{}-{}.toml",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Start the mock control plane and build the app connected to it.
fn fixture(test: &str, with_workspace: bool) -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let mock = Mock::new();
    let mock_state = mock.state();
    let (endpoint_id, addr) = rt.block_on(mock.start());

    let mut cfg = Config::default();
    if with_workspace {
        cfg.workspaces.push(WorkspaceCfg {
            name: "mock".into(),
            endpoint_id,
            test_addr: Some(addr),
        });
    }
    let config_path = temp_config(test);
    let app_cfg = cfg.clone();
    let app_path = config_path.clone();
    let harness: Harness<'static, SuzyApp> = Harness::new_eframe(move |cc| {
        let rt2 = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        SuzyApp::with_config(cc, rt2, app_cfg, app_path)
    });
    Fixture {
        harness,
        mock_state,
        _rt: rt,
        _config_path: config_path,
    }
}

/// Step frames until `pred` holds (or fail with a tree dump on timeout).
fn pump_until(h: &mut Harness<SuzyApp>, what: &str, pred: impl Fn(&Harness<SuzyApp>) -> bool) {
    let start = Instant::now();
    loop {
        h.step();
        if pred(h) {
            return;
        }
        if start.elapsed() > T {
            let ws_err = h
                .state()
                .workspaces
                .first()
                .map(|w| format!("ws error: {:?} endpoint: {:?}", w.error, w.endpoint));
            panic!("timed out waiting for: {what} ({ws_err:?})");
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

/// Connect + wait for the fleet to appear in the sidebar.
fn connect_and_wait(fx: &mut Fixture) {
    pump_until(&mut fx.harness, "agent in sidebar", |h| {
        has_label_containing(h, "demo-1")
    });
}

// ── tests ────────────────────────────────────────────────────────────────

#[test]
fn welcome_screen_without_workspaces() {
    let mut fx = fixture("welcome", false);
    pump_until(&mut fx.harness, "welcome text", |h| {
        has_label_containing(h, "Add a workspace to connect to a suzerain")
    });
}

#[test]
fn fleet_sidebar_and_dashboard() {
    let mut fx = fixture("fleet", true);
    connect_and_wait(&mut fx);
    // Dashboard stats + castellan row.
    pump_until(&mut fx.harness, "dashboard", |h| {
        has_label(h, "Fleet") && has_label_containing(h, "mockbox")
    });
    // Sidebar daemon group + status-colored agent entry.
    assert!(has_label_containing(&fx.harness, "● demo-1"));
}

#[test]
fn chat_history_and_prompt_round_trip() {
    let mut fx = fixture("chat", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("● demo-1").click();
    // History replay renders.
    pump_until(&mut fx.harness, "history", |h| {
        has_label_containing(h, "mock reply: hi there")
    });

    // Type a prompt into the chat input and send with Enter.
    let input = fx
        .harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput);
    input.focus();
    input.type_text("ping");
    input.key_press(egui_kittest::kittest::Key::Enter);
    pump_until(&mut fx.harness, "mock reply", |h| {
        has_label_containing(h, "mock reply to: ping")
    });
    // The mock received exactly one prompt, in prompt mode.
    let prompts = fx.mock_state.lock().unwrap().prompts_received.clone();
    assert!(
        prompts
            .iter()
            .any(|(a, m)| a == "demo-1" && m == "prompt:ping"),
        "prompts: {prompts:?}"
    );
}

#[test]
fn shell_tab_round_trip_through_ws_to_process() {
    let mut fx = fixture("shell", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("● demo-1").click();
    fx.harness.step();
    fx.harness.get_by_label("⌨ Shell").click();
    fx.harness.step();

    // The shell connection spawns (notice written into the terminal).
    pump_until(&mut fx.harness, "shell connect", |h| {
        h.state()
            .shells
            .get(&(0, "demo-1".to_string()))
            .is_some_and(|s| {
                s.term.screen_text().contains("connecting")
                    || s.term.screen_text().contains("shell")
            })
    });

    // Type a command (inject through the widget's input channel — keyboard
    // focus on a painted canvas isn't accessible; key mapping itself is
    // unit-tested in terminal_key_mapping).
    fx.harness
        .state_mut()
        .shells
        .get(&(0, "demo-1".to_string()))
        .unwrap()
        .input
        .send(TermInput::Data(b"echo shell-marker-42\n".to_vec()))
        .unwrap();

    pump_until(&mut fx.harness, "shell output", |h| {
        h.state()
            .shells
            .get(&(0, "demo-1".to_string()))
            .is_some_and(|s| s.term.screen_text().contains("shell-marker-42"))
    });
}

#[test]
fn create_agent_form_submits_manifest() {
    let mut fx = fixture("create", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("✚ Create agent").click();
    fx.harness.step();
    // Catalog-driven defaults landed in the TOML preview.
    assert!(fx
        .harness
        .state()
        .create_form
        .toml_text
        .contains("kimi-coding"));
    // Submit (second button with that label is inside the window).
    let buttons: Vec<_> = fx.harness.get_all_by_label("✚ Create agent").collect();
    buttons.last().unwrap().click();
    pump_until(&mut fx.harness, "agent created", |h| {
        h.state()
            .status_msg
            .as_ref()
            .is_some_and(|m| m.contains("provisioning"))
    });
    let agents = fx.mock_state.lock().unwrap().agents.clone();
    assert!(
        agents.iter().any(|a| a["name"] == "my-agent"),
        "agents: {agents:?}"
    );
}

#[test]
fn create_agent_provider_dropdown_lists_configured_providers() {
    let mut fx = fixture("create-providers", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("✚ Create agent").click();
    fx.harness.step();

    // Default selection is the first configured+injectable provider
    // (alphabetical): kimi-coding.
    assert_eq!(fx.harness.state().create_form.provider, "kimi-coding");

    // Open the provider combo (labelled "provider" — see create.rs's
    // `ComboBox::from_label`) and pick openrouter, the mock's other
    // configured, key-injectable provider. (github-copilot: configured but
    // not key-injectable, and anthropic: injectable but not configured —
    // both excluded from this combo specifically; see create::tests for
    // that filtering logic in isolation.)
    use egui::accesskit::Role;
    fx.harness
        .get_by_role_and_label(Role::ComboBox, "provider")
        .click();
    fx.harness.step();
    fx.harness
        .get_by_role_and_label(Role::Button, "openrouter")
        .click();
    fx.harness.step();
    assert_eq!(fx.harness.state().create_form.provider, "openrouter");
    // One more frame: the model field is cleared on the same frame the
    // provider changes, then auto-filled with that provider's first model
    // on the next.
    fx.harness.step();
    assert_eq!(fx.harness.state().create_form.model, "stealth/ox-alpha");

    // Submit and confirm the created agent's manifest carries the
    // selected provider through.
    let buttons: Vec<_> = fx.harness.get_all_by_label("✚ Create agent").collect();
    buttons.last().unwrap().click();
    pump_until(&mut fx.harness, "agent created", |h| {
        h.state()
            .status_msg
            .as_ref()
            .is_some_and(|m| m.contains("provisioning"))
    });
    let agents = fx.mock_state.lock().unwrap().agents.clone();
    let created = agents
        .iter()
        .find(|a| a["name"] == "my-agent")
        .unwrap_or_else(|| panic!("agent not created: {agents:?}"));
    assert_eq!(created["manifest"]["model"]["provider"], "openrouter");
}

#[test]
fn logs_and_details_tabs() {
    let mut fx = fixture("logs", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("● demo-1").click();
    fx.harness.step();
    fx.harness.get_by_label("🧾 Logs").click();
    pump_until(&mut fx.harness, "log events", |h| {
        has_label_containing(h, "spawned")
    });

    fx.harness.get_by_label("⚙ Details").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "manifest", |h| {
        has_label_containing(h, "manifest (read-only")
    });
    // Auto-suspend editor: type a policy and apply.
    let input = fx.harness.get_by_role(egui::accesskit::Role::TextInput);
    input.focus();
    input.type_text("10m");
    fx.harness.get_by_label("Apply").click();
    pump_until(&mut fx.harness, "auto-suspend applied", |h| {
        h.state()
            .status_msg
            .as_ref()
            .is_some_and(|m| m.contains("auto-suspend: ok"))
    });
}

#[test]
fn castellans_pending_approval_flow() {
    let mut fx = fixture("castellans", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("🖥 Castellans").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "pending enrollment", |h| {
        has_label_containing(h, "pendingbox")
    });
    fx.harness.get_by_label("Approve").click();
    pump_until(&mut fx.harness, "pending cleared", |h| {
        !has_label_containing(h, "pendingbox")
    });
    let audit = fx.mock_state.lock().unwrap().audit.clone();
    assert!(audit.iter().any(|e| e["action"] == "daemon_approve"));
}

#[test]
fn secrets_view_and_audited_reveal() {
    let mut fx = fixture("secrets", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("🔑 Secrets").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "provider row", |h| {
        has_label_containing(h, "kimi-coding")
    });
    fx.harness.get_by_label("reveal once").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "revealed value", |h| {
        has_label_containing(h, "sk-mock-revealed-once")
    });
}

#[test]
fn activity_feed_lists_audit() {
    let mut fx = fixture("activity", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("≣ Activity").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "audit entry", |h| {
        has_label_containing(h, "daemon_approve")
    });
}

#[test]
fn destroy_agent_with_confirm() {
    let mut fx = fixture("destroy", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("● demo-1").click();
    fx.harness.step();
    fx.harness.get_by_label("⚙ Details").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "details", |h| {
        has_label_containing(h, "manifest (read-only")
    });
    fx.harness.get_by_label("🗑 destroy").click();
    fx.harness.step();
    fx.harness.get_by_label("Destroy").click();
    pump_until(&mut fx.harness, "destroyed", |h| {
        h.state()
            .status_msg
            .as_ref()
            .is_some_and(|m| m.contains("destroyed"))
    });
    let destroyed = fx.mock_state.lock().unwrap().destroyed.clone();
    assert!(destroyed.contains(&"demo-1".to_string()));
}

#[test]
fn destroy_agent_cancel_keeps_agent() {
    let mut fx = fixture("destroy-cancel", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("● demo-1").click();
    fx.harness.step();
    fx.harness.get_by_label("⚙ Details").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "details", |h| {
        has_label_containing(h, "manifest (read-only")
    });
    fx.harness.get_by_label("🗑 destroy").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "destroy-agent dialog", |h| {
        has_label_containing(h, "Destroy 'demo-1'")
    });
    fx.harness.get_by_label("Close window").click();
    pump_until(&mut fx.harness, "dialog closed", |h| {
        !has_label_containing(h, "Destroy 'demo-1'")
    });
    assert!(fx.mock_state.lock().unwrap().destroyed.is_empty());
    // Still there and selectable.
    assert!(has_label_containing(&fx.harness, "● demo-1"));
}

#[test]
fn daemon_labels_edit_applies_and_audits() {
    let mut fx = fixture("labels", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("🖥 Castellans").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "daemon row", |h| {
        has_label_containing(h, "mockbox")
    });
    // The daemon panel is a collapsed-by-default CollapsingHeader — expand
    // it before its "edit" button becomes reachable.
    fx.harness.get_by_label("🟢 mockbox").click();
    fx.harness.step();
    fx.harness.get_by_label("edit").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "edit-labels dialog", |h| {
        has_label_containing(h, "labels for mockbox")
    });
    let input = fx.harness.get_by_role(egui::accesskit::Role::TextInput);
    input.focus();
    input.type_text("env=prod");
    fx.harness.get_by_label("Apply").click();
    // The dialog closes optimistically the instant Apply is clicked (same
    // frame, before the network round-trip even starts) — wait for the
    // actual server round-trip to finish instead.
    pump_until(&mut fx.harness, "labels action completed", |h| {
        h.state()
            .status_msg
            .as_deref()
            .is_some_and(|m| m.starts_with("labels"))
    });
    let audit = fx.mock_state.lock().unwrap().audit.clone();
    assert!(
        audit.iter().any(|e| e["action"] == "daemon_label"
            && e["detail"]["endpoint_id"] == "mock-daemon-eid-0001"),
        "audit: {audit:?}"
    );
}

#[test]
fn daemon_labels_edit_cancel_sends_nothing() {
    let mut fx = fixture("labels-cancel", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("🖥 Castellans").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "daemon row", |h| {
        has_label_containing(h, "mockbox")
    });
    // The daemon panel is a collapsed-by-default CollapsingHeader — expand
    // it before its "edit" button becomes reachable.
    fx.harness.get_by_label("🟢 mockbox").click();
    fx.harness.step();
    fx.harness.get_by_label("edit").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "edit-labels dialog", |h| {
        has_label_containing(h, "labels for mockbox")
    });
    fx.harness.get_by_label("Cancel").click();
    pump_until(&mut fx.harness, "labels dialog closed", |h| {
        h.state().labels_editing.is_none()
    });
    let audit = fx.mock_state.lock().unwrap().audit.clone();
    assert!(!audit.iter().any(|e| e["action"] == "daemon_label"));
}

#[test]
fn secrets_add_new_provider_appears_in_inventory() {
    let mut fx = fixture("secrets-add", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("🔑 Secrets").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "provider row", |h| {
        has_label_containing(h, "kimi-coding")
    });

    // Selecting the provider id from the catalog dropdown is the same
    // ComboBox mechanism exercised end-to-end in
    // create_agent_provider_dropdown_lists_configured_providers; seed it
    // directly here so this test's real UI interaction is focused on the
    // security-relevant part — typing the write-only, masked secret value.
    let ws = fx.harness.state().active_ws.unwrap();
    fx.harness
        .state_mut()
        .secrets
        .get_mut(&ws)
        .unwrap()
        .new_provider_id = "anthropic".to_string();
    fx.harness.step();

    // Password-masked fields get accesskit's dedicated PasswordInput role,
    // not TextInput. The provider value field renders first — before the
    // extra-secrets section's own (also masked) value field further down.
    let value_input = fx
        .harness
        .get_all_by_role(egui::accesskit::Role::PasswordInput)
        .next()
        .expect("provider value field");
    value_input.focus();
    value_input.type_text("sk-anthropic-test-value");
    fx.harness.step();

    // Likewise the provider row's "set" button precedes the extra-secrets
    // section's own (identically labelled) "set" button.
    fx.harness
        .get_all_by_label("set")
        .next()
        .expect("provider set button")
        .click();
    // The inventory refetch fires concurrently with the write, not after
    // it (see action_then_refresh_secrets) — wait for the write itself to
    // finish, then force a fresh fetch so the render reflects it.
    pump_until(&mut fx.harness, "set-provider action completed", |h| {
        h.state()
            .status_msg
            .as_deref()
            .is_some_and(|m| m.starts_with("set provider"))
    });
    fx.harness.get_by_label("↻").click();
    pump_until(&mut fx.harness, "anthropic row appears", |h| {
        has_label_containing(h, "anthropic")
    });
    let secrets = fx.mock_state.lock().unwrap().secrets.clone();
    assert!(
        secrets
            .iter()
            .any(|e| e["kind"] == "provider" && e["name"] == "anthropic"),
        "secrets: {secrets:?}"
    );
}

#[test]
fn secrets_delete_provider_removes_it() {
    let mut fx = fixture("secrets-delete", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("🔑 Secrets").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "provider row", |h| {
        has_label_containing(h, "kimi-coding")
    });
    fx.harness.get_by_label("delete").click();
    pump_until(&mut fx.harness, "delete-provider action completed", |h| {
        h.state()
            .status_msg
            .as_deref()
            .is_some_and(|m| m.starts_with("delete provider"))
    });
    fx.harness.get_by_label("↻").click();
    pump_until(&mut fx.harness, "kimi-coding row gone", |h| {
        !has_label_containing(h, "kimi-coding")
    });
    let secrets = fx.mock_state.lock().unwrap().secrets.clone();
    assert!(!secrets.iter().any(|e| e["name"] == "kimi-coding"));
}

#[test]
fn needs_attention_badge_for_flagged_agent() {
    let mut fx = fixture("attention", true);
    fx.mock_state
        .lock()
        .unwrap()
        .agents
        .push(serde_json::json!({
            "id": "33333333-3333-3333-3333-333333333333",
            "name": "needs-help", "daemon_endpoint_id": "mock-daemon-eid-0001",
            "daemon_hostname": "mockbox",
            "manifest": {"name": "needs-help", "harness": {"type": "pi", "version": "0.84.1"},
                         "model": {"provider": "kimi-coding", "id": "kimi-for-coding"}},
            "state": "active", "status": "failed", "busy": false, "idle_secs": 0,
            "needs_attention": true, "auto_suspend_override": null,
            "created_at": "2026-08-12T00:00:02Z", "session_file": null,
        }));
    connect_and_wait(&mut fx);
    pump_until(&mut fx.harness, "flagged agent visible", |h| {
        has_label_containing(h, "needs-help")
    });
    assert!(has_label_containing(&fx.harness, "needs attention"));
    // Exactly one badge — the healthy demo-1 agent must not be flagged too.
    assert_eq!(
        fx.harness
            .query_all_by_label_contains("needs attention")
            .count(),
        1
    );
}

#[test]
fn multi_agent_prompt_routes_to_correct_agent() {
    let mut fx = fixture("multi-agent", true);
    fx.mock_state
        .lock()
        .unwrap()
        .agents
        .push(serde_json::json!({
            "id": "44444444-4444-4444-4444-444444444444",
            "name": "demo-2", "daemon_endpoint_id": "mock-daemon-eid-0001",
            "daemon_hostname": "mockbox",
            "manifest": {"name": "demo-2", "harness": {"type": "pi", "version": "0.84.1"},
                         "model": {"provider": "kimi-coding", "id": "kimi-for-coding"}},
            "state": "active", "status": "idle", "busy": false, "idle_secs": 0,
            "needs_attention": false, "auto_suspend_override": null,
            "created_at": "2026-08-12T00:00:02Z", "session_file": null,
        }));
    connect_and_wait(&mut fx);
    pump_until(&mut fx.harness, "both agents in sidebar", |h| {
        has_label_containing(h, "demo-1") && has_label_containing(h, "demo-2")
    });

    fx.harness.get_by_label("● demo-2").click();
    fx.harness.step();
    let input = fx
        .harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput);
    input.focus();
    input.type_text("ping-2");
    input.key_press(egui_kittest::kittest::Key::Enter);
    let mock_state = fx.mock_state.clone();
    pump_until(&mut fx.harness, "prompt recorded", move |_h| {
        mock_state
            .lock()
            .unwrap()
            .prompts_received
            .iter()
            .any(|(a, m)| a == "demo-2" && m == "prompt:ping-2")
    });
    let prompts = fx.mock_state.lock().unwrap().prompts_received.clone();
    assert!(
        prompts
            .iter()
            .any(|(a, m)| a == "demo-2" && m == "prompt:ping-2"),
        "prompts: {prompts:?}"
    );
    assert!(
        !prompts
            .iter()
            .any(|(a, m)| a == "demo-1" && m == "prompt:ping-2"),
        "prompt leaked to demo-1: {prompts:?}"
    );
}

#[test]
fn theme_toggle_persists() {
    let mut fx = fixture("theme", false);
    pump_until(&mut fx.harness, "welcome", |h| {
        has_label_containing(h, "Add a workspace")
    });
    assert_eq!(fx.harness.state().cfg.theme, "");
    fx.harness.get_by_label("🌙").click();
    fx.harness.step();
    assert_eq!(fx.harness.state().cfg.theme, "light");
    // Persisted to the injected path.
    let text = std::fs::read_to_string(&fx.harness.state().config_path).unwrap();
    assert!(text.contains("light"), "{text}");
}

#[test]
fn workspace_removal_returns_to_welcome() {
    let mut fx = fixture("wsremove", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("➖ ws").click();
    fx.harness.step();
    fx.harness.get_by_label("Remove").click();
    pump_until(&mut fx.harness, "welcome back", |h| {
        h.state().workspaces.is_empty()
            && has_label_containing(h, "Add a workspace to connect to a suzerain")
    });
}

#[test]
fn remove_workspace_cancel_keeps_it() {
    let mut fx = fixture("wsremove-cancel", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("➖ ws").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "remove-workspace dialog", |h| {
        has_label_containing(h, "Remove workspace")
    });
    // Cancel via the window's own close control, not the destructive button.
    fx.harness.get_by_label("Close window").click();
    pump_until(&mut fx.harness, "dialog closed", |h| {
        !has_label_containing(h, "Remove workspace")
    });
    assert_eq!(fx.harness.state().workspaces.len(), 1);
    // Still connected and usable — not a dead/disconnected shell.
    assert!(has_label_containing(&fx.harness, "● demo-1"));
}

#[test]
fn add_workspace_dialog_validates_and_persists() {
    let mut fx = fixture("wsadd", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("＋ workspace").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "add-workspace dialog", |h| {
        has_label_containing(h, "Add workspace")
    });

    // Connect is disabled until an EndpointId is entered.
    assert!(fx.harness.get_by_label("Connect").is_disabled());

    // name field, then endpoint-id field — the only two single-line text
    // inputs on screen while this modal-like dialog is open.
    let inputs: Vec<_> = fx
        .harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .collect();
    assert_eq!(inputs.len(), 2, "expected name + endpoint-id fields only");
    inputs[0].focus();
    inputs[0].type_text("second-suzerain");
    fx.harness.step();
    let inputs: Vec<_> = fx
        .harness
        .get_all_by_role(egui::accesskit::Role::TextInput)
        .collect();
    inputs[1].focus();
    inputs[1].type_text("f".repeat(64).as_str());
    fx.harness.step();

    fx.harness.get_by_label("Connect").click();
    fx.harness.step();

    // Added immediately (synchronously) — connecting happens in the
    // background and isn't what this test is checking.
    assert_eq!(fx.harness.state().workspaces.len(), 2);
    assert_eq!(fx.harness.state().workspaces[1].cfg.name, "second-suzerain");
    assert!(!fx.harness.state().add_ws_open);
    let text = std::fs::read_to_string(&fx.harness.state().config_path).unwrap();
    assert!(text.contains("second-suzerain"), "{text}");
}

#[test]
fn add_workspace_dialog_cancel_adds_nothing() {
    let mut fx = fixture("wsadd-cancel", true);
    connect_and_wait(&mut fx);
    fx.harness.get_by_label("＋ workspace").click();
    fx.harness.step();
    pump_until(&mut fx.harness, "add-workspace dialog", |h| {
        has_label_containing(h, "Add workspace")
    });
    fx.harness.get_by_label("Close window").click();
    pump_until(&mut fx.harness, "dialog closed", |h| {
        !has_label_containing(h, "Add workspace")
    });
    assert_eq!(fx.harness.state().workspaces.len(), 1);
    assert!(!fx.harness.state().add_ws_open);
}

#[test]
fn switch_between_two_workspaces() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (eid_a, addr_a) = rt.block_on(Mock::new().start());
    let (eid_b, addr_b) = rt.block_on(Mock::new().start());
    let mut cfg = Config::default();
    cfg.workspaces.push(WorkspaceCfg {
        name: "ws-a".into(),
        endpoint_id: eid_a,
        test_addr: Some(addr_a),
    });
    cfg.workspaces.push(WorkspaceCfg {
        name: "ws-b".into(),
        endpoint_id: eid_b,
        test_addr: Some(addr_b),
    });
    let config_path = temp_config("wsswitch");
    let app_cfg = cfg.clone();
    let app_path = config_path.clone();
    let mut harness: Harness<'static, SuzyApp> = Harness::new_eframe(move |cc| {
        let rt2 = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        SuzyApp::with_config(cc, rt2, app_cfg, app_path)
    });

    pump_until(&mut harness, "both tabs connected", |h| {
        has_label(h, "ws-a") && has_label(h, "ws-b") && has_label_containing(h, "demo-1")
    });
    assert_eq!(harness.state().active_ws, Some(0));

    harness.get_by_label("ws-b").click();
    harness.step();
    assert_eq!(harness.state().active_ws, Some(1));
    // Switching tabs resets the view back to the dashboard.
    pump_until(&mut harness, "dashboard on ws-b", |h| has_label(h, "Fleet"));

    harness.get_by_label("ws-a").click();
    harness.step();
    assert_eq!(harness.state().active_ws, Some(0));
}

// ── widget/conversion unit tests (via pub API) ───────────────────────────

#[test]
fn terminal_key_mapping() {
    use egui::{Key, Modifiers};
    use suzy::terminal::key_to_bytes;
    let none = Modifiers::default();
    assert_eq!(key_to_bytes(Key::Enter, &none), b"\r");
    assert_eq!(key_to_bytes(Key::Backspace, &none), vec![0x7F]);
    assert_eq!(key_to_bytes(Key::ArrowUp, &none), b"\x1b[A");
    assert_eq!(key_to_bytes(Key::Tab, &none), b"\t");
    let ctrl = Modifiers {
        ctrl: true,
        ..Default::default()
    };
    assert_eq!(key_to_bytes(Key::C, &ctrl), vec![0x03]);
    assert_eq!(key_to_bytes(Key::D, &ctrl), vec![0x04]);
}

#[test]
fn terminal_feeds_ansi_and_reports_screen() {
    let mut term = suzy::terminal::Terminal::default();
    term.feed(b"$ \x1b[32mgreen-ok\x1b[0m\r\n");
    assert!(term.screen_text().contains("green-ok"));
    // Scroll region / erase: a cleared line must not show stale content.
    term.feed(b"junk-line\r\n\x1b[2J\x1b[Hclean");
    let text = term.screen_text();
    assert!(text.contains("clean"));
    assert!(!text.contains("junk-line"));
}

#[test]
fn chat_converts_live_message_end() {
    let msg = serde_json::json!({
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "hmm"},
            {"type": "text", "text": "answer"},
            {"type": "toolCall", "name": "read", "arguments": {"path": "/tmp/x"}},
        ],
    });
    let item = suzy::chat::message_to_item(&msg).expect("item");
    match item {
        suzy::chat::ChatItem::Assistant(parts) => {
            assert_eq!(parts.len(), 3);
        }
        other => panic!("unexpected item: {other:?}"),
    }
}
