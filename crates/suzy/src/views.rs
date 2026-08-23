//! M2 views: castellans (pending enrollments, labels, add-daemon guide),
//! agent logs, agent details (manifest, auto-suspend policy, sessions).

use egui::{Color32, RichText, Ui};
use serde_json::Value;
use suzerain_client::{Daemon, EndpointInfo};

// ── castellans ───────────────────────────────────────────────────────────

/// User intents produced by the castellans view; the app dispatches them.
#[derive(Debug, Clone)]
pub enum CastellanIntent {
    ApprovePending(String),
    DismissPending(String),
    EditLabels(String),
    RemoveDaemon(String),
}

pub fn castellans_view(
    ui: &mut Ui,
    daemons: &[Daemon],
    pending: &[Value],
    endpoint: Option<&EndpointInfo>,
) -> Vec<CastellanIntent> {
    let mut intents = Vec::new();

    // ── pending enrollments ──
    if !pending.is_empty() {
        ui.heading("Pending enrollments");
        ui.label(
            RichText::new("these daemons registered but are not approved yet")
                .size(11.5)
                .color(Color32::KHAKI),
        );
        for p in pending {
            let eid = p["endpoint_id"].as_str().unwrap_or_default();
            let host = p["hostname"].as_str().unwrap_or_default();
            let os = p["os"].as_str().unwrap_or_default();
            let arch = p["arch"].as_str().unwrap_or_default();
            egui::Frame::new()
                .fill(Color32::from_rgb(0x2A, 0x24, 0x18))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(host).strong());
                            ui.label(RichText::new(eid.to_string()).monospace().size(10.5));
                            ui.label(
                                RichText::new(format!("{os}/{arch}"))
                                    .size(11.0)
                                    .color(Color32::GRAY),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("Dismiss").clicked() {
                                intents.push(CastellanIntent::DismissPending(eid.to_string()));
                            }
                            if ui
                                .button(
                                    RichText::new("Approve")
                                        .color(Color32::from_rgb(0x5C, 0xC8, 0x7A)),
                                )
                                .clicked()
                            {
                                intents.push(CastellanIntent::ApprovePending(eid.to_string()));
                            }
                        });
                    });
                });
            ui.add_space(4.0);
        }
        ui.separator();
    }

    // ── enrolled daemons ──
    ui.heading("Castellans");
    if daemons.is_empty() {
        ui.label(
            RichText::new("no daemons enrolled yet")
                .italics()
                .color(Color32::GRAY),
        );
    }
    for d in daemons {
        egui::CollapsingHeader::new(
            RichText::new(format!(
                "{} {}",
                if d.online { "🟢" } else { "⚫" },
                d.hostname
            ))
            .strong(),
        )
        .id_salt(format!("daemon_{}", d.endpoint_id))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&d.endpoint_id).monospace().size(10.5));
                if ui.button("📋").on_hover_text("copy endpoint id").clicked() {
                    ui.ctx().copy_text(d.endpoint_id.clone());
                }
            });
            ui.label(format!(
                "{}/{} • {} vcpu • {} MiB mem free • {} MiB disk free",
                d.os, d.arch, d.capacity.vcpu_total, d.usage.memory_mib_free, d.usage.disk_mib_free
            ));
            if !d.approved {
                ui.label(RichText::new("unapproved").color(Color32::KHAKI));
            }

            // labels: effective chips, overrides distinguished
            ui.horizontal_wrapped(|ui| {
                ui.label("labels:");
                for (k, v) in &d.labels {
                    let is_override = d.label_overrides.contains_key(k);
                    let text = if is_override {
                        RichText::new(format!("{k}={v} ✎")).color(Color32::KHAKI)
                    } else {
                        RichText::new(format!("{k}={v}"))
                    };
                    ui.label(text);
                }
                if d.labels.is_empty() {
                    ui.label(RichText::new("none").italics().color(Color32::GRAY));
                }
                if ui.button("edit").clicked() {
                    intents.push(CastellanIntent::EditLabels(d.endpoint_id.clone()));
                }
            });

            // its agents are listed in the sidebar; offer removal only when empty-ish
            ui.horizontal(|ui| {
                if ui
                    .button(
                        RichText::new("remove daemon")
                            .color(Color32::LIGHT_RED)
                            .size(11.5),
                    )
                    .clicked()
                {
                    intents.push(CastellanIntent::RemoveDaemon(d.endpoint_id.clone()));
                }
            });
        });
    }

    ui.separator();
    // ── add a castellan ──
    ui.heading("Add a castellan");
    if let Some(info) = endpoint {
        ui.label("On the new machine:");
        let cmds = format!(
            "curl -fsSL https://raw.githubusercontent.com/Shakakai/suzerain/main/ops/install.sh | bash -s -- castellan\n\
             castellan init --suzerain {}\n\
             # then approve it above (pending enrollments), and:\n\
             castellan run",
            info.endpoint_id
        );
        egui::Frame::new()
            .fill(Color32::from_rgb(0x18, 0x1C, 0x22))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&cmds).monospace().size(11.5));
                    if ui.button("📋 copy").clicked() {
                        ui.ctx().copy_text(cmds.clone());
                    }
                });
            });
        ui.label(
            RichText::new(
                "same machine works too — the daemon registers and shows up under pending enrollments",
            )
            .size(11.0)
            .color(Color32::GRAY),
        );
    } else {
        ui.label("connect to a workspace first");
    }

    intents
}

/// Labels editor body for a daemon. Returns `(set, remove)` when the user
/// applies. `draft` persists between frames.
pub fn labels_editor(
    ui: &mut Ui,
    daemon: &Daemon,
    draft: &mut String,
) -> Option<(std::collections::BTreeMap<String, String>, Vec<String>)> {
    ui.label(format!(
        "labels for {} ({}…)",
        daemon.hostname,
        daemon.short_id()
    ));
    ui.add_space(4.0);
    ui.label(
        RichText::new("reported by daemon:")
            .size(11.5)
            .color(Color32::GRAY),
    );
    ui.horizontal_wrapped(|ui| {
        for (k, v) in &daemon.reported_labels {
            ui.label(format!("{k}={v}"));
        }
        if daemon.reported_labels.is_empty() {
            ui.label(RichText::new("none").italics());
        }
    });
    ui.add_space(4.0);
    ui.label(RichText::new("operator overrides (win over reported):").size(11.5));
    let mut to_remove = Vec::new();
    ui.horizontal_wrapped(|ui| {
        for k in daemon.label_overrides.keys() {
            let v = &daemon.label_overrides[k];
            if ui.button(format!("{k}={v} ✖")).clicked() {
                to_remove.push(k.clone());
            }
        }
        if daemon.label_overrides.is_empty() {
            ui.label(RichText::new("none").italics());
        }
    });
    ui.add_space(4.0);
    ui.label("add / set override (k=v):");
    ui.text_edit_singleline(draft);
    let mut out = None;
    ui.horizontal(|ui| {
        if ui.button("Apply").clicked() {
            let mut set = std::collections::BTreeMap::new();
            let mut remove = to_remove.clone();
            for part in draft.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(k) = part.strip_prefix('-') {
                    remove.push(k.trim().to_string());
                } else if let Some((k, v)) = part.split_once('=') {
                    set.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            out = Some((set, remove));
            draft.clear();
        }
        if ui.button("Cancel").clicked() {
            draft.clear();
            out = Some((Default::default(), Vec::new())); // empty = close, no-op
        }
    });
    // Removals via chip buttons also apply on next Apply; if only removals
    // happened and the user clicks Apply with empty draft, they go through.
    if out.is_none() && !to_remove.is_empty() {
        out = Some((Default::default(), to_remove));
    }
    out
}

// ── agent logs ───────────────────────────────────────────────────────────

#[derive(Default)]
pub struct LogsState {
    pub kind: String,
    pub q: String,
    pub tail: usize,
    pub events: Vec<Value>,
    pub total: usize,
    pub error: Option<String>,
    pub loaded: bool,
}

impl LogsState {
    pub fn tail_or_default(&self) -> usize {
        if self.tail == 0 {
            200
        } else {
            self.tail
        }
    }
}

/// Returns true when a (re)fetch is requested.
pub fn logs_view(ui: &mut Ui, state: &mut LogsState) -> bool {
    let mut refetch = false;
    ui.horizontal(|ui| {
        ui.label("kind");
        if ui
            .add(
                egui::TextEdit::singleline(&mut state.kind)
                    .hint_text("message_end")
                    .desired_width(110.0),
            )
            .changed()
        {
            // filter applied on Refresh
        }
        ui.label("search");
        ui.add(
            egui::TextEdit::singleline(&mut state.q)
                .hint_text("substring")
                .desired_width(160.0),
        );
        ui.label("tail");
        let mut tail_str = state.tail_or_default().to_string();
        if ui
            .add(egui::TextEdit::singleline(&mut tail_str).desired_width(50.0))
            .changed()
        {
            state.tail = tail_str.parse().unwrap_or(200);
        }
        if ui.button("Refresh").clicked() {
            refetch = true;
        }
        ui.label(
            RichText::new(format!("{} / {} events", state.events.len(), state.total))
                .size(11.0)
                .color(Color32::GRAY),
        );
    });
    if let Some(err) = &state.error {
        ui.label(RichText::new(err).color(Color32::LIGHT_RED).size(12.0));
    }
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("logs_scroll")
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for ev in &state.events {
                let kind = ev["kind"].as_str().unwrap_or("?");
                let at = ev["at"].as_str().unwrap_or("");
                let short_at = at.get(11..19).unwrap_or(at);
                let payload = summarize_payload(ev);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(short_at)
                            .monospace()
                            .size(10.5)
                            .color(Color32::GRAY),
                    );
                    ui.label(
                        RichText::new(kind)
                            .monospace()
                            .size(10.5)
                            .color(kind_color(kind)),
                    );
                    ui.label(RichText::new(payload).monospace().size(10.5));
                });
            }
            if state.events.is_empty() && state.loaded {
                ui.label(
                    RichText::new("no matching events")
                        .italics()
                        .color(Color32::GRAY),
                );
            }
        });
    refetch
}

fn kind_color(kind: &str) -> Color32 {
    match kind {
        "message_end" => Color32::LIGHT_GRAY,
        "turn_start" | "turn_end" => Color32::KHAKI,
        "spawned" | "session_started" => Color32::from_rgb(0x5C, 0xC8, 0x7A),
        "crashed" | "pi_exit" | "driver_died" | "pi_stderr" => Color32::LIGHT_RED,
        "order_received" => Color32::from_rgb(0x64, 0x8C, 0xC8),
        _ => Color32::GRAY,
    }
}

fn summarize_payload(ev: &Value) -> String {
    let p = &ev["payload"];
    match ev["kind"].as_str().unwrap_or("") {
        "message_end" => {
            let role = p["message"]["role"].as_str().unwrap_or("?");
            let text: String = match &p["message"]["content"] {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .filter(|c| c["type"] == "text")
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => String::new(),
            };
            let one = text.replace('\n', " ");
            format!("{role}: {}", truncate(&one, 120))
        }
        "pi_stderr" => truncate(p["line"].as_str().unwrap_or(""), 140).to_string(),
        _ => truncate(&p.to_string(), 140).to_string().replace('\n', " "),
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.chars().count() > max {
        let byte_idx = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        &s[..byte_idx]
    } else {
        s
    }
}

// ── activity (global audit feed) ─────────────────────────────────────────

#[derive(Default)]
pub struct ActivityState {
    pub entries: Vec<Value>,
    /// Filter by action substring (e.g. "create", "daemon").
    pub action: String,
    /// Free-text filter over the whole entry.
    pub q: String,
    /// How many entries to fetch (0 = default 300).
    pub tail: usize,
    pub loaded: bool,
    pub error: Option<String>,
}

/// The machine-readable narrative of everything the control plane did.
/// Returns true when a (re)fetch is requested.
pub fn activity_view(ui: &mut Ui, state: &mut ActivityState) -> bool {
    let mut refetch = false;
    ui.horizontal(|ui| {
        ui.label("action");
        ui.add(
            egui::TextEdit::singleline(&mut state.action)
                .hint_text("agent_create")
                .desired_width(110.0),
        );
        ui.label("search");
        ui.add(
            egui::TextEdit::singleline(&mut state.q)
                .hint_text("substring")
                .desired_width(160.0),
        );
        if ui.button("Refresh").clicked() {
            refetch = true;
        }
        ui.label(
            RichText::new(format!("{} entries", state.entries.len()))
                .size(11.0)
                .color(Color32::GRAY),
        );
    });
    if let Some(err) = &state.error {
        ui.label(RichText::new(err).color(Color32::LIGHT_RED).size(12.0));
    }
    ui.separator();

    let action_filter = state.action.trim().to_string();
    let q = state.q.trim().to_string();
    egui::ScrollArea::vertical()
        .id_salt("activity_scroll")
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in &state.entries {
                let action = entry["action"].as_str().unwrap_or("?");
                if !action_filter.is_empty() && !action.contains(&action_filter) {
                    continue;
                }
                if !q.is_empty() && !entry.to_string().contains(&q) {
                    continue;
                }
                let at = entry["at"].as_str().unwrap_or("");
                let when = at.get(5..19).unwrap_or(at).replace('T', " ");
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(when)
                            .monospace()
                            .size(10.5)
                            .color(Color32::GRAY),
                    );
                    ui.label(
                        RichText::new(action)
                            .monospace()
                            .size(10.5)
                            .color(action_color(action)),
                    );
                    ui.label(
                        RichText::new(summarize_audit_detail(&entry["detail"]))
                            .monospace()
                            .size(10.5),
                    );
                });
            }
            if state.entries.is_empty() && state.loaded {
                ui.label(
                    RichText::new("no audit entries yet")
                        .italics()
                        .color(Color32::GRAY),
                );
            }
        });
    refetch
}

fn action_color(action: &str) -> Color32 {
    if action.contains("destroy") || action.contains("remove") || action.contains("delete") {
        Color32::LIGHT_RED
    } else if action.contains("create") || action.contains("approve") {
        Color32::from_rgb(0x5C, 0xC8, 0x7A)
    } else if action.contains("secret") {
        Color32::from_rgb(0xC8, 0x8C, 0xE0)
    } else {
        Color32::KHAKI
    }
}

fn summarize_audit_detail(detail: &Value) -> String {
    let obj = match detail.as_object() {
        Some(o) => o,
        None => return truncate(&detail.to_string(), 140).to_string(),
    };
    let parts: Vec<String> = obj
        .iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k}={}", truncate(&val, 60))
        })
        .collect();
    parts.join(" ")
}

// ── secrets (M4) ─────────────────────────────────────────────────────────

/// Write-only secrets editor. Values are never read back — the inventory is
/// masked; reveal-once is an audited, explicit action.
#[derive(Default)]
pub struct SecretsState {
    pub value: Option<Value>,
    pub loaded: bool,
    pub error: Option<String>,
    pub new_provider_id: String,
    pub new_provider_value: String,
    pub new_extra_name: String,
    pub new_extra_value: String,
    pub deploy_key_value: String,
    /// Reveal-once dialog content: (kind, name, value).
    pub revealed: Option<(String, String, String)>,
}

#[derive(Debug, Clone)]
pub enum SecretsIntent {
    SetProvider(String, String),
    DeleteProvider(String),
    SetExtra(String, String),
    DeleteExtra(String),
    SetDeployKey(String),
    DeleteDeployKey,
    Reveal(String, String),
    Refetch,
}

pub fn secrets_view(
    ui: &mut Ui,
    state: &mut SecretsState,
    providers_catalog: Option<&Value>,
) -> Vec<SecretsIntent> {
    let mut intents = Vec::new();
    if let Some(err) = &state.error {
        ui.label(RichText::new(err).color(Color32::LIGHT_RED));
    }
    let Some(v) = state.value.clone() else {
        ui.label(RichText::new("loading…").italics().color(Color32::GRAY));
        return intents;
    };

    if !v["store_present"].as_bool().unwrap_or(false) {
        ui.heading("Secrets store not set up");
        ui.label(
            "The control plane keeps an age-encrypted store in the fleet home. It is created \
             automatically on first write — set the first key from the CLI:",
        );
        egui::Frame::new()
            .fill(Color32::from_rgb(0x18, 0x1C, 0x22))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(
                        "suz secrets set provider <provider-id>\n\
                         # creates ~/.local/share/suzerain/secrets.age\n\
                         # (age identity: ~/.local/share/suzerain/age-keys.txt)",
                    )
                    .monospace()
                    .size(11.5),
                );
            });
        return intents;
    }

    ui.horizontal(|ui| {
        ui.heading("Secrets");
        ui.label(
            RichText::new("write-only — values are never read back; reveal is audited")
                .size(11.0)
                .color(Color32::GRAY),
        );
        if ui.button("↻").clicked() {
            intents.push(SecretsIntent::Refetch);
        }
    });
    ui.add_space(6.0);

    let entries = v["entries"].as_array().cloned().unwrap_or_default();
    let providers: Vec<&Value> = entries
        .iter()
        .filter(|e| e["kind"].as_str() == Some("provider"))
        .collect();
    let extras: Vec<&Value> = entries
        .iter()
        .filter(|e| e["kind"].as_str() == Some("extra"))
        .collect();
    let deploy_key = entries.iter().find(|e| e["kind"].as_str() == Some("git"));

    // ── providers ──
    ui.label(RichText::new("LLM provider keys").strong());
    egui::Grid::new("secrets_providers")
        .striped(true)
        .show(ui, |ui| {
            for e in &providers {
                let name = e["name"].as_str().unwrap_or("?");
                let used_by = e["used_by"].as_u64().unwrap_or(0);
                ui.label(RichText::new(name).monospace());
                ui.label(
                    RichText::new(format!("used by {used_by} agent(s)"))
                        .size(11.0)
                        .color(Color32::GRAY),
                );
                if ui.button("reveal once").clicked() {
                    intents.push(SecretsIntent::Reveal("provider".into(), name.to_string()));
                }
                if ui
                    .button(RichText::new("delete").color(Color32::LIGHT_RED).size(11.5))
                    .clicked()
                {
                    intents.push(SecretsIntent::DeleteProvider(name.to_string()));
                }
                ui.end_row();
            }
        });
    ui.horizontal(|ui| {
        ui.label("add:");
        // Dropdown from the pi provider catalog when available.
        if let Some(cat) = providers_catalog {
            let mut ids: Vec<String> = cat["providers"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            ids.sort();
            egui::ComboBox::from_id_salt("new_provider")
                .selected_text(if state.new_provider_id.is_empty() {
                    "provider…"
                } else {
                    &state.new_provider_id
                })
                .show_ui(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(240.0)
                        .show(ui, |ui| {
                            for id in ids {
                                ui.selectable_value(&mut state.new_provider_id, id.clone(), id);
                            }
                        });
                });
        } else {
            ui.text_edit_singleline(&mut state.new_provider_id);
        }
        ui.add(
            egui::TextEdit::singleline(&mut state.new_provider_value)
                .hint_text("sk-…")
                .password(true)
                .desired_width(200.0),
        );
        if ui.button("set").clicked()
            && !state.new_provider_id.is_empty()
            && !state.new_provider_value.is_empty()
        {
            intents.push(SecretsIntent::SetProvider(
                state.new_provider_id.clone(),
                state.new_provider_value.clone(),
            ));
            state.new_provider_value.clear();
        }
    });
    ui.add_space(8.0);

    // ── git SSH key ──
    ui.label(RichText::new("git SSH key (one per fleet — pull & push over SSH)").strong());
    ui.horizontal(|ui| {
        if deploy_key.is_some() {
            ui.label(RichText::new("● configured").color(Color32::from_rgb(0x5C, 0xC8, 0x7A)));
            if ui
                .button(RichText::new("delete").color(Color32::LIGHT_RED).size(11.5))
                .clicked()
            {
                intents.push(SecretsIntent::DeleteDeployKey);
            }
        } else {
            ui.label(RichText::new("○ not set").color(Color32::GRAY));
        }
    });
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::multiline(&mut state.deploy_key_value)
                .hint_text("any ssh-keygen private key — ed25519/ecdsa/RSA…")
                .desired_rows(2)
                .desired_width(360.0),
        );
        if ui.button("upload").clicked() && !state.deploy_key_value.trim().is_empty() {
            intents.push(SecretsIntent::SetDeployKey(state.deploy_key_value.clone()));
            state.deploy_key_value.clear();
        }
    });
    ui.add_space(8.0);

    // ── extra named secrets ──
    ui.label(RichText::new("extra named secrets").strong());
    egui::Grid::new("secrets_extra")
        .striped(true)
        .show(ui, |ui| {
            for e in &extras {
                let name = e["name"].as_str().unwrap_or("?");
                ui.label(RichText::new(name).monospace());
                if ui.button("reveal once").clicked() {
                    intents.push(SecretsIntent::Reveal("extra".into(), name.to_string()));
                }
                if ui
                    .button(RichText::new("delete").color(Color32::LIGHT_RED).size(11.5))
                    .clicked()
                {
                    intents.push(SecretsIntent::DeleteExtra(name.to_string()));
                }
                ui.end_row();
            }
        });
    ui.horizontal(|ui| {
        ui.label("add:");
        ui.add(
            egui::TextEdit::singleline(&mut state.new_extra_name)
                .hint_text("NAME or NAME@host")
                .desired_width(140.0),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.new_extra_value)
                .hint_text("value")
                .password(true)
                .desired_width(200.0),
        );
        if ui.button("set").clicked()
            && !state.new_extra_name.is_empty()
            && !state.new_extra_value.is_empty()
        {
            intents.push(SecretsIntent::SetExtra(
                state.new_extra_name.clone(),
                state.new_extra_value.clone(),
            ));
            state.new_extra_value.clear();
        }
    });

    intents
}

// ── agent details ────────────────────────────────────────────────────────

#[derive(Default)]
pub struct DetailsState {
    pub value: Option<Value>,
    pub error: Option<String>,
    pub auto_suspend_input: String,
    pub loaded: bool,
}

/// User intents from the details view.
#[derive(Debug, Clone)]
pub enum DetailsIntent {
    SetAutoSuspend(String),
    Destroy,
    Refetch,
}

pub fn details_view(ui: &mut Ui, agent: &str, state: &mut DetailsState) -> Vec<DetailsIntent> {
    let mut intents = Vec::new();
    if let Some(err) = &state.error {
        ui.label(RichText::new(err).color(Color32::LIGHT_RED));
    }
    let Some(v) = state.value.clone() else {
        ui.label(RichText::new("loading…").italics().color(Color32::GRAY));
        return intents;
    };

    ui.horizontal(|ui| {
        ui.label(RichText::new(agent).strong().size(15.0));
        ui.label(
            RichText::new(format!(
                "state: {} • created: {}",
                v["state"].as_str().unwrap_or("?"),
                v["created_at"].as_str().unwrap_or("?")
            ))
            .size(11.5)
            .color(Color32::GRAY),
        );
        if ui.button("↻").on_hover_text("refetch").clicked() {
            intents.push(DetailsIntent::Refetch);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(RichText::new("🗑 destroy").color(Color32::LIGHT_RED))
                .clicked()
            {
                intents.push(DetailsIntent::Destroy);
            }
        });
    });
    ui.add_space(6.0);

    // auto-suspend policy
    ui.horizontal(|ui| {
        ui.label("auto-suspend override:");
        if state.auto_suspend_input.is_empty() {
            state.auto_suspend_input = v["auto_suspend_override"]
                .as_str()
                .unwrap_or("")
                .to_string();
        }
        ui.add(
            egui::TextEdit::singleline(&mut state.auto_suspend_input)
                .hint_text("inherit (e.g. 10m, never, default)")
                .desired_width(180.0),
        );
        if ui.button("Apply").clicked() {
            let value = if state.auto_suspend_input.trim().is_empty() {
                "default".to_string()
            } else {
                state.auto_suspend_input.trim().to_string()
            };
            intents.push(DetailsIntent::SetAutoSuspend(value));
        }
        ui.label(
            RichText::new("\"never\" also exempts from resource-pressure preemption")
                .size(10.5)
                .color(Color32::GRAY),
        );
    });
    ui.add_space(6.0);

    // sessions (session eras)
    if let Some(sessions) = v["sessions"].as_array() {
        ui.label(RichText::new(format!("sessions ({}):", sessions.len())).strong());
        for s in sessions.iter().rev().take(5) {
            let file = s["session_file"].as_str().unwrap_or("");
            let short = file.rsplit('/').next().unwrap_or(file);
            let open = s["ended_at"].is_null();
            ui.label(
                RichText::new(format!(
                    "  {} {} — {}{}",
                    if open { "●" } else { "○" },
                    s["started_at"].as_str().unwrap_or("?"),
                    short,
                    if open { " (current)" } else { "" }
                ))
                .size(11.0)
                .color(if open {
                    Color32::from_rgb(0x5C, 0xC8, 0x7A)
                } else {
                    Color32::GRAY
                }),
            );
        }
        ui.add_space(6.0);
    }

    // manifest
    ui.label(RichText::new("manifest (read-only — recreate to change):").strong());
    egui::Frame::new()
        .fill(Color32::from_rgb(0x18, 0x1C, 0x22))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("manifest_scroll")
                .max_height(320.0)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(v["manifest_toml"].as_str().unwrap_or(""))
                            .monospace()
                            .size(11.5),
                    );
                });
        });

    intents
}
