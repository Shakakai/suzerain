//! Create-agent dialog (M2): structured form synced with an editable TOML
//! manifest preview. Agents are declared, not detected — this form is the
//! defining difference from herdr's "open a pane and run a command".

use egui::{Color32, RichText, Ui};
use serde_json::Value;
use suzerain_protocol::manifest::{
    AgentManifest, Extension, Harness, Lifecycle, ModelSpec, Observability, Otel, Prompt, Repo,
    Resources, Schedule, SecretScopes, Toolchain,
};

pub struct CreateForm {
    pub name: String,
    pub harness: String,
    pub version: String,
    pub provider: String,
    pub model: String,
    /// "" = harness default.
    pub thinking: String,
    pub vcpu: String,
    pub memory_mib: String,
    pub disk_mib: String,
    /// (url, ref) rows.
    pub repos: Vec<(String, String)>,
    /// pi package sources (npm:@scope/pkg, git:github.com/user/repo@v1).
    pub extensions: Vec<String>,
    pub append_system: String,
    /// Selected provider ids (must be configured in the secrets store).
    pub secret_providers: Vec<String>,
    /// Placement: required k=v label rows.
    pub require: Vec<(String, String)>,
    /// Hard daemon pin (endpoint prefix or hostname); "" = scheduler picks.
    pub daemon_pin: String,
    /// "", "10m", "never", … ("" = inherit global policy).
    pub auto_suspend: String,
    /// Comma-separated extra egress hosts.
    pub egress_allow: String,
    pub otel_endpoint: String,

    pub toml_text: String,
    /// The user edited the TOML by hand; form edits stop clobbering it
    /// until "apply TOML → form" or "regenerate" is used.
    pub toml_edited: bool,
    pub parse_error: Option<String>,
}

impl Default for CreateForm {
    fn default() -> Self {
        let mut f = Self {
            name: "my-agent".into(),
            harness: "pi".into(),
            version: String::new(),
            provider: String::new(),
            model: String::new(),
            thinking: String::new(),
            vcpu: "2".into(),
            memory_mib: "2048".into(),
            disk_mib: "5120".into(),
            repos: Vec::new(),
            extensions: Vec::new(),
            append_system: String::new(),
            secret_providers: Vec::new(),
            require: Vec::new(),
            daemon_pin: String::new(),
            auto_suspend: String::new(),
            egress_allow: String::new(),
            otel_endpoint: String::new(),
            toml_text: String::new(),
            toml_edited: false,
            parse_error: None,
        };
        f.regenerate_toml();
        f
    }
}

impl CreateForm {
    /// Build the manifest from the structured fields.
    pub fn to_manifest(&self) -> Result<AgentManifest, String> {
        if self.name.trim().is_empty() {
            return Err("name is required".into());
        }
        if self.provider.is_empty() || self.model.is_empty() {
            return Err("provider and model are required".into());
        }
        let parse = |s: &str, what: &str, default: u64| -> Result<u64, String> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(default);
            }
            s.parse::<u64>()
                .map_err(|_| format!("{what}: '{s}' is not a number"))
        };
        let repos = self
            .repos
            .iter()
            .filter(|(u, _)| !u.trim().is_empty())
            .map(|(u, r)| Repo {
                url: u.trim().to_string(),
                ref_: if r.trim().is_empty() {
                    "main".into()
                } else {
                    r.trim().to_string()
                },
            })
            .collect();
        let extensions = self
            .extensions
            .iter()
            .filter(|s| !s.trim().is_empty())
            .map(|s| Extension {
                source: Some(s.trim().to_string()),
                url: None,
                ref_: None,
            })
            .collect();
        let require = self
            .require
            .iter()
            .filter(|(k, _)| !k.trim().is_empty())
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let egress: Vec<String> = self
            .egress_allow
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Ok(AgentManifest {
            name: self.name.trim().to_string(),
            harness: Harness {
                kind: self.harness.clone(),
                version: self.version.clone(),
            },
            model: ModelSpec {
                provider: self.provider.clone(),
                id: self.model.clone(),
                thinking: non_empty(&self.thinking),
            },
            resources: Resources {
                vcpu: parse(&self.vcpu, "vcpu", 2)? as u32,
                memory_mib: parse(&self.memory_mib, "memory_mib", 2048)?,
                disk_mib: parse(&self.disk_mib, "disk_mib", 5120)?,
                gpu: None,
            },
            schedule: Schedule {
                require,
                daemon: non_empty(&self.daemon_pin),
            },
            toolchain: Toolchain::default(),
            repos,
            extensions,
            prompt: Prompt {
                append_system: non_empty(&self.append_system),
            },
            secrets: SecretScopes {
                providers: self.secret_providers.clone(),
                extra: Vec::new(),
            },
            egress: suzerain_protocol::manifest::Egress { allow: egress },
            observability: Observability {
                otel: non_empty(&self.otel_endpoint).map(|endpoint| Otel {
                    endpoint,
                    headers: Default::default(),
                }),
            },
            lifecycle: Lifecycle {
                auto_suspend: non_empty(&self.auto_suspend),
            },
            provision: None,
        })
    }

    /// Refill the structured fields from a parsed manifest.
    pub fn load_manifest(&mut self, m: &AgentManifest) {
        self.name = m.name.clone();
        self.harness = m.harness.kind.clone();
        self.version = m.harness.version.clone();
        self.provider = m.model.provider.clone();
        self.model = m.model.id.clone();
        self.thinking = m.model.thinking.clone().unwrap_or_default();
        self.vcpu = m.resources.vcpu.to_string();
        self.memory_mib = m.resources.memory_mib.to_string();
        self.disk_mib = m.resources.disk_mib.to_string();
        self.repos = m
            .repos
            .iter()
            .map(|r| (r.url.clone(), r.ref_.clone()))
            .collect();
        self.extensions = m
            .extensions
            .iter()
            .filter_map(|e| e.source.clone().or_else(|| e.url.clone()))
            .collect();
        self.append_system = m.prompt.append_system.clone().unwrap_or_default();
        self.secret_providers = m.secrets.providers.clone();
        self.require = m
            .schedule
            .require
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.daemon_pin = m.schedule.daemon.clone().unwrap_or_default();
        self.auto_suspend = m.lifecycle.auto_suspend.clone().unwrap_or_default();
        self.egress_allow = m.egress.allow.join(", ");
        self.otel_endpoint = m
            .observability
            .otel
            .as_ref()
            .map(|o| o.endpoint.clone())
            .unwrap_or_default();
        self.toml_edited = false;
        self.parse_error = None;
    }

    /// Regenerate the TOML preview from the form (unless the user has
    /// diverged by editing the TOML directly).
    pub fn regenerate_toml(&mut self) {
        if self.toml_edited {
            return;
        }
        match self.to_manifest() {
            Ok(m) => {
                self.toml_text = toml::to_string_pretty(&m).unwrap_or_default();
                self.parse_error = None;
            }
            Err(e) => self.parse_error = Some(e),
        }
    }

    /// Parse the (possibly hand-edited) TOML back into the form.
    pub fn apply_toml(&mut self) {
        match toml::from_str::<AgentManifest>(&self.toml_text) {
            Ok(m) => self.load_manifest(&m),
            Err(e) => self.parse_error = Some(format!("invalid manifest TOML: {e}")),
        }
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

// ── catalog extraction ───────────────────────────────────────────────────

/// Providers usable for a new agent: key configured AND injectable
/// (WEB-UI.md §6). Returns sorted (id, models[(id, name)]).
pub fn usable_providers(catalog: &Value) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    if let Some(providers) = catalog["providers"].as_object() {
        for (id, entry) in providers {
            let ok = entry["key_configured"].as_bool().unwrap_or(false)
                && entry["key_injectable"].as_bool().unwrap_or(false);
            if !ok {
                continue;
            }
            let models = entry["models"]
                .as_array()
                .map(|ms| {
                    ms.iter()
                        .map(|m| {
                            (
                                m["id"].as_str().unwrap_or_default().to_string(),
                                m["name"].as_str().unwrap_or_default().to_string(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            out.push((id.clone(), models));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Providers with a configured key (for the secrets multi-select).
pub fn configured_providers(catalog: &Value) -> Vec<String> {
    let mut out: Vec<String> = catalog["providers"]
        .as_object()
        .map(|ps| {
            ps.iter()
                .filter(|(_, e)| e["key_configured"].as_bool().unwrap_or(false))
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

pub fn harness_versions(harnesses: &Value, kind: &str) -> Vec<String> {
    harnesses["harnesses"][kind]["versions"]
        .as_array()
        .map(|vs| {
            vs.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ── rendering ────────────────────────────────────────────────────────────

pub struct CreateCtx<'a> {
    pub providers: Option<&'a Value>,
    pub harnesses: Option<&'a Value>,
    pub daemon_options: Vec<(String, String)>, // (label, pin value)
}

/// Render the create window body. Returns `Some(toml)` when the user hits
/// Create with a valid manifest.
pub fn show_create(ui: &mut Ui, form: &mut CreateForm, cx: &CreateCtx) -> Option<String> {
    let mut submit: Option<String> = None;
    let usable = cx.providers.map(usable_providers).unwrap_or_default();
    let configured = cx.providers.map(configured_providers).unwrap_or_default();
    let versions = cx
        .harnesses
        .map(|h| harness_versions(h, &form.harness))
        .unwrap_or_default();

    // Defaults once catalogs are known.
    let mut defaults_applied = false;
    if form.provider.is_empty() && !usable.is_empty() {
        form.provider = usable[0].0.clone();
        defaults_applied = true;
    }
    if let Some((_, models)) = usable.iter().find(|(id, _)| *id == form.provider) {
        if form.model.is_empty() && !models.is_empty() {
            form.model = models[0].0.clone();
            defaults_applied = true;
        }
    }
    if form.version.is_empty() && !versions.is_empty() {
        form.version = versions[0].clone();
        defaults_applied = true;
    }
    if defaults_applied {
        form.regenerate_toml();
    }

    ui.columns(2, |cols| {
        // ── left: structured form ──
        cols[0].heading("manifest");
        egui::ScrollArea::vertical()
            .id_salt("create_form")
            .show(&mut cols[0], |ui| {
                let mut changed = false;
                changed |= ui
                    .horizontal(|ui| {
                        ui.label("name");
                        ui.text_edit_singleline(&mut form.name).changed()
                    })
                    .inner;

                ui.horizontal(|ui| {
                    ui.label("harness");
                    egui::ComboBox::from_id_salt("harness")
                        .selected_text(&form.harness)
                        .show_ui(ui, |ui| {
                            if let Some(h) = cx.harnesses {
                                if let Some(map) = h["harnesses"].as_object() {
                                    for kind in map.keys() {
                                        changed |= ui
                                            .selectable_value(&mut form.harness, kind.clone(), kind)
                                            .changed();
                                    }
                                }
                            }
                        });
                    if versions.is_empty() {
                        changed |= ui.text_edit_singleline(&mut form.version).changed();
                    } else {
                        egui::ComboBox::from_id_salt("version")
                            .selected_text(&form.version)
                            .show_ui(ui, |ui| {
                                for v in &versions {
                                    changed |= ui
                                        .selectable_value(&mut form.version, v.clone(), v)
                                        .changed();
                                }
                            });
                    }
                });

                ui.horizontal(|ui| {
                    // `from_label` (rather than `from_id_salt` plus a
                    // separate preceding `ui.label`) gives the combo a real
                    // accessible name — egui always reports SOME label for
                    // a ComboBox's widget info (empty string when none is
                    // given), which forecloses the usual label/labelled_by
                    // fallback accessibility tools rely on.
                    egui::ComboBox::from_label("provider")
                        .selected_text(&form.provider)
                        .show_ui(ui, |ui| {
                            for (id, _) in &usable {
                                if ui
                                    .selectable_value(&mut form.provider, id.clone(), id)
                                    .changed()
                                {
                                    form.model.clear();
                                    changed = true;
                                }
                            }
                        });
                });
                let models = usable
                    .iter()
                    .find(|(id, _)| *id == form.provider)
                    .map(|(_, ms)| ms.clone())
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label("model   ");
                    egui::ComboBox::from_id_salt("model")
                        .selected_text(&form.model)
                        .show_ui(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .max_height(300.0)
                                .show(ui, |ui| {
                                    for (id, name) in &models {
                                        changed |= ui
                                            .selectable_value(
                                                &mut form.model,
                                                id.clone(),
                                                format!("{id} — {name}"),
                                            )
                                            .changed();
                                    }
                                });
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("thinking");
                    egui::ComboBox::from_id_salt("thinking")
                        .selected_text(if form.thinking.is_empty() {
                            "default"
                        } else {
                            &form.thinking
                        })
                        .show_ui(ui, |ui| {
                            for level in ["", "off", "minimal", "low", "medium", "high", "xhigh"] {
                                changed |= ui
                                    .selectable_value(
                                        &mut form.thinking,
                                        level.to_string(),
                                        if level.is_empty() { "default" } else { level },
                                    )
                                    .changed();
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("resources");
                    ui.label(RichText::new("vcpu").size(11.0));
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut form.vcpu).desired_width(45.0))
                        .changed();
                    ui.label(RichText::new("mem MiB").size(11.0));
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut form.memory_mib).desired_width(60.0))
                        .changed();
                    ui.label(RichText::new("disk MiB").size(11.0));
                    changed |= ui
                        .add(egui::TextEdit::singleline(&mut form.disk_mib).desired_width(60.0))
                        .changed();
                });

                ui.separator();
                changed |= rows_editor(
                    ui,
                    "repos (url + ref)",
                    &mut form.repos,
                    "git@github.com:org/repo.git",
                    "main",
                );
                changed |= list_editor(
                    ui,
                    "extensions (pi package sources)",
                    &mut form.extensions,
                    "npm:@scope/pkg",
                );

                ui.separator();
                ui.label("secrets (providers with a configured key):");
                for p in &configured {
                    let mut selected = form.secret_providers.contains(p);
                    if ui.checkbox(&mut selected, p).changed() {
                        if selected {
                            form.secret_providers.push(p.clone());
                        } else {
                            form.secret_providers.retain(|x| x != p);
                        }
                        changed = true;
                    }
                }
                if form.secret_providers.is_empty() && !form.provider.is_empty() {
                    ui.label(
                        RichText::new(format!(
                            "hint: '{}' usually needs its key here",
                            form.provider
                        ))
                        .size(11.0)
                        .color(Color32::KHAKI),
                    );
                }

                ui.separator();
                ui.label("system prompt addition:");
                changed |= ui
                    .add(
                        egui::TextEdit::multiline(&mut form.append_system)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    )
                    .changed();

                ui.separator();
                changed |= rows_editor(
                    ui,
                    "placement require (k=v)",
                    &mut form.require,
                    "zone",
                    "office",
                );
                ui.horizontal(|ui| {
                    ui.label("pin to daemon");
                    egui::ComboBox::from_id_salt("pin")
                        .selected_text(if form.daemon_pin.is_empty() {
                            "any".to_string()
                        } else {
                            form.daemon_pin.clone()
                        })
                        .show_ui(ui, |ui| {
                            changed |= ui
                                .selectable_value(&mut form.daemon_pin, String::new(), "any")
                                .changed();
                            for (label, pin) in &cx.daemon_options {
                                changed |= ui
                                    .selectable_value(&mut form.daemon_pin, pin.clone(), label)
                                    .changed();
                            }
                        });
                });

                ui.horizontal(|ui| {
                    ui.label("auto-suspend");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut form.auto_suspend)
                                .hint_text("inherit (e.g. 10m, never)")
                                .desired_width(140.0),
                        )
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("egress allow");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut form.egress_allow)
                                .hint_text("host1, host2")
                                .desired_width(f32::INFINITY),
                        )
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("otel endpoint");
                    changed |= ui
                        .add(
                            egui::TextEdit::singleline(&mut form.otel_endpoint)
                                .hint_text("https://otel.example.com")
                                .desired_width(f32::INFINITY),
                        )
                        .changed();
                });

                if changed {
                    form.regenerate_toml();
                }
            });

        // ── right: TOML preview (editable) ──
        cols[1].heading("manifest.toml");
        egui::ScrollArea::vertical()
            .id_salt("create_toml")
            .show(&mut cols[1], |ui| {
                let resp = ui.add(
                    egui::TextEdit::multiline(&mut form.toml_text)
                        .font(egui::TextStyle::Monospace)
                        .desired_rows(28)
                        .desired_width(f32::INFINITY),
                );
                if resp.changed() {
                    form.toml_edited = true;
                }
                ui.horizontal(|ui| {
                    if ui
                        .button("apply TOML → form")
                        .on_hover_text("parse the edited TOML back into the form")
                        .clicked()
                    {
                        form.apply_toml();
                    }
                    if form.toml_edited
                        && ui
                            .button("regenerate from form")
                            .on_hover_text("discard hand edits")
                            .clicked()
                    {
                        form.toml_edited = false;
                        form.regenerate_toml();
                    }
                });
                if let Some(err) = &form.parse_error {
                    ui.label(RichText::new(err).color(Color32::LIGHT_RED).size(12.0));
                }
                if form.toml_edited {
                    ui.label(
                        RichText::new("hand-edited — form changes are paused")
                            .size(11.0)
                            .color(Color32::KHAKI),
                    );
                }
                ui.add_space(6.0);
                if ui
                    .button(RichText::new("✚ Create agent").strong())
                    .clicked()
                {
                    match toml::from_str::<AgentManifest>(&form.toml_text) {
                        Ok(_) => submit = Some(form.toml_text.clone()),
                        Err(e) => form.parse_error = Some(format!("invalid manifest TOML: {e}")),
                    }
                }
            });
    });
    submit
}

/// Repeatable (key, value) rows with add/remove. Returns true on change.
fn rows_editor(
    ui: &mut Ui,
    title: &str,
    rows: &mut Vec<(String, String)>,
    ph_a: &str,
    ph_b: &str,
) -> bool {
    let mut changed = false;
    ui.label(title);
    let mut remove_idx = None;
    for (i, (a, b)) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui
                .add(
                    egui::TextEdit::singleline(a)
                        .hint_text(ph_a)
                        .desired_width(200.0),
                )
                .changed();
            changed |= ui
                .add(
                    egui::TextEdit::singleline(b)
                        .hint_text(ph_b)
                        .desired_width(110.0),
                )
                .changed();
            if ui.button("✖").clicked() {
                remove_idx = Some(i);
            }
        });
    }
    if let Some(i) = remove_idx {
        rows.remove(i);
        changed = true;
    }
    if ui
        .button(format!(
            "＋ add {}",
            title.split('(').next().unwrap_or("row").trim()
        ))
        .clicked()
    {
        rows.push((String::new(), String::new()));
        changed = true;
    }
    changed
}

/// Repeatable single-value rows with add/remove. Returns true on change.
fn list_editor(ui: &mut Ui, title: &str, rows: &mut Vec<String>, placeholder: &str) -> bool {
    let mut changed = false;
    ui.label(title);
    let mut remove_idx = None;
    for (i, v) in rows.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui
                .add(
                    egui::TextEdit::singleline(v)
                        .hint_text(placeholder)
                        .desired_width(300.0),
                )
                .changed();
            if ui.button("✖").clicked() {
                remove_idx = Some(i);
            }
        });
    }
    if let Some(i) = remove_idx {
        rows.remove(i);
        changed = true;
    }
    if ui
        .button(format!(
            "＋ add {}",
            title.split('(').next().unwrap_or("row").trim()
        ))
        .clicked()
    {
        rows.push(String::new());
        changed = true;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::{configured_providers, usable_providers};
    use serde_json::json;

    fn fixture_catalog() -> serde_json::Value {
        json!({"providers": {
            "kimi-coding": {
                "models": [{"id": "kimi-for-coding", "name": "Kimi for Coding"}],
                "key_injectable": true, "key_configured": true,
            },
            "openrouter": {
                "models": [{"id": "stealth/ox-alpha", "name": "Stealth: Ox Alpha"}],
                "key_injectable": true, "key_configured": true,
            },
            // Configured but OAuth-only — can't receive an API key in the
            // guest VM, so it must never be offered for a new agent even
            // though a key exists for it.
            "github-copilot": {
                "models": [{"id": "gpt-5", "name": "GPT-5"}],
                "key_injectable": false, "key_configured": true,
            },
            // Key-injectable but no key configured yet.
            "anthropic": {
                "models": [{"id": "claude-sonnet-4-5", "name": "Claude Sonnet 4.5"}],
                "key_injectable": true, "key_configured": false,
            },
        }})
    }

    #[test]
    fn usable_providers_requires_configured_and_injectable() {
        let usable = usable_providers(&fixture_catalog());
        let ids: Vec<&str> = usable.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["kimi-coding", "openrouter"]);
    }

    #[test]
    fn usable_providers_carries_its_models() {
        let usable = usable_providers(&fixture_catalog());
        let (_, models) = usable
            .iter()
            .find(|(id, _)| id == "openrouter")
            .expect("openrouter listed");
        assert_eq!(
            models,
            &vec![(
                "stealth/ox-alpha".to_string(),
                "Stealth: Ox Alpha".to_string()
            )]
        );
    }

    #[test]
    fn configured_providers_ignores_injectability() {
        // The secrets multi-select (which providers to hand this agent)
        // only cares whether a key exists, not whether pi can inject it —
        // github-copilot belongs here even though it's excluded from
        // usable_providers above.
        let ids = configured_providers(&fixture_catalog());
        assert_eq!(ids, vec!["github-copilot", "kimi-coding", "openrouter"]);
    }
}
