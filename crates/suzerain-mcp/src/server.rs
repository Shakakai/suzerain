//! The suzerain MCP server: 18 tools over the control plane REST API.
//! Tool catalog: docs/MCP-PLAN.md §2.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use suzerain_protocol::manifest::{
    AgentManifest, Egress, Extension, GpuResources, Harness, ModelSpec, Observability, Otel,
    Prompt, Repo, Resources, Schedule, SecretScopes,
};

use crate::client::ApiClient;

#[derive(Clone)]
pub struct SuzerainMcp {
    api: Arc<ApiClient>,
    tool_router: ToolRouter<Self>,
}

/// Render a tool outcome: success → pretty JSON; failure → MCP tool error
/// carrying the control plane's (operator-actionable) message verbatim.
fn outcome(result: Result<Value>) -> Result<CallToolResult, ErrorData> {
    match result {
        Ok(v) => Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&v).unwrap_or_default(),
        )])),
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "{e:#}"
        ))])),
    }
}

/// Closest candidate by substring containment (either direction), for
/// "did you mean …" hints.
fn suggest<'a>(needle: &str, candidates: impl Iterator<Item = &'a str>) -> Option<String> {
    let needle = needle.to_lowercase();
    let mut best: Option<&str> = None;
    for c in candidates {
        let cl = c.to_lowercase();
        if (cl.contains(&needle) || needle.contains(&cl)) && best.is_none_or(|b| c.len() < b.len())
        {
            best = Some(c);
        }
    }
    best.map(str::to_string)
}

// ── parameter structs ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LlmProvidersParams {
    /// Drill into a single provider id (e.g. "anthropic"). Omit for all.
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PiPackagesParams {
    /// Substring search over name/description/author.
    pub q: Option<String>,
    /// Badge filter: extension, skill, prompt, theme. Default: all.
    pub r#type: Option<String>,
    /// Page number (1-based).
    pub page: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AuditParams {
    /// Number of recent entries (default 50).
    pub tail: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CastellanAddParams {
    /// EndpointId of an enrolled daemon to approve. Omit to get enrollment
    /// instructions for a new machine.
    pub endpoint_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EndpointIdParams {
    /// Daemon EndpointId (or unambiguous prefix, or hostname).
    pub endpoint_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CastellanListParams {
    /// Also list pending (not yet approved) enrollments. Default true.
    pub include_pending: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CastellanRemoveParams {
    /// Daemon EndpointId (or unambiguous prefix, or hostname).
    pub endpoint_id: String,
    /// Safety latch: must exactly equal the daemon's full EndpointId.
    pub confirm: String,
    /// Remove even while agents are assigned (they are destroyed first,
    /// best-effort). Default false.
    pub force: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LabelsParams {
    /// Daemon EndpointId (or unambiguous prefix, or hostname).
    pub endpoint_id: String,
    /// Labels to set (operator overrides), e.g. {"zone": "office"}.
    pub set: Option<BTreeMap<String, String>>,
    /// Label keys to remove.
    pub remove: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentListParams {
    /// Filter by public status (running, idle, sleeping, waking, failed).
    pub state: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentNameParams {
    /// Agent name.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentDeleteParams {
    /// Agent name.
    pub name: String,
    /// Safety latch: must exactly equal the agent name.
    pub confirm: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentLogsParams {
    /// Agent name.
    pub name: String,
    /// Number of recent events (default 50).
    pub tail: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionEventsParams {
    /// Agent name.
    pub name: String,
    /// Keep only the last N transcript items.
    pub tail: Option<usize>,
    /// Roles to include (default all): user, assistant, toolResult, system.
    pub roles: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionSendParams {
    /// Agent name (sleeping agents wake automatically).
    pub name: String,
    /// The message to send to the agent's session.
    pub message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepoParam {
    /// Git URL (https or git@).
    pub url: String,
    /// Branch/tag/SHA (default "main").
    pub r#ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtensionParam {
    /// pi package install source from the pi.dev catalog, e.g.
    /// "npm:@scope/pkg" or "git:github.com/user/repo". Prefer this.
    pub source: Option<String>,
    /// Legacy form: git repo URL cloned into pi-home (requires ref too).
    pub url: Option<String>,
    /// Ref for the url form (default "main").
    pub r#ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentCreateParams {
    /// Complete agent manifest as TOML. Takes precedence over ALL
    /// structured fields when set.
    pub manifest_toml: Option<String>,
    /// Fleet-wide unique agent name (required for structured mode).
    pub name: Option<String>,
    /// LLM provider id — check llm_providers first: it must be
    /// key_injectable AND key_configured or the create is rejected.
    pub provider: Option<String>,
    /// Model id valid for the provider (see llm_providers).
    pub model: Option<String>,
    /// Thinking level (e.g. "high"); provider-dependent.
    pub thinking: Option<String>,
    /// Harness version — must be in harness_catalog. Default: latest known.
    pub harness_version: Option<String>,
    /// vCPUs (default 2).
    pub vcpu: Option<u32>,
    /// Memory MiB (default 2048).
    pub memory_mib: Option<u64>,
    /// Disk MiB (default 5120).
    pub disk_mib: Option<u64>,
    /// GPU count (0 = none).
    pub gpu_count: Option<u32>,
    /// Minimum free VRAM per GPU (MiB).
    pub gpu_vram_mib: Option<u64>,
    /// Git repos cloned into the agent workspace.
    pub repos: Option<Vec<RepoParam>>,
    /// pi extensions/packages deployed with the agent (see
    /// pi_packages_search for catalog sources).
    pub extensions: Option<Vec<ExtensionParam>>,
    /// Text appended to pi's system prompt (written to the agent's
    /// APPEND_SYSTEM.md).
    pub append_system_prompt: Option<String>,
    /// Hard pin: daemon EndpointId prefix or hostname.
    pub daemon: Option<String>,
    /// Scheduling labels: every k=v must match a daemon's effective labels.
    pub require: Option<BTreeMap<String, String>>,
    /// Extra egress allowlist hosts beyond the provisioning defaults.
    pub egress_allow: Option<Vec<String>>,
    /// OTEL endpoint for agent traces (http(s) URL).
    pub otel_endpoint: Option<String>,
}

// ── the server ────────────────────────────────────────────────────────────

impl SuzerainMcp {
    pub fn new(api: ApiClient) -> Self {
        Self {
            api: Arc::new(api),
            tool_router: Self::tool_router(),
        }
    }

    async fn create_from_structured(&self, p: AgentCreateParams) -> Result<Value> {
        let name = p.name.context("name is required (structured mode)")?;
        let provider = p
            .provider
            .context("provider is required (structured mode)")?;
        let model = p.model.context("model is required (structured mode)")?;

        // Discovery: harness version default + full client-side
        // pre-validation against the live catalogs.
        let (providers, harnesses, daemons) = tokio::try_join!(
            self.api.get("/api/v1/providers"),
            self.api.get("/api/v1/harnesses"),
            self.api.get("/api/v1/daemons"),
        )?;

        let harness_version = match p.harness_version {
            Some(v) => v,
            None => harnesses["harnesses"]["pi"]["versions"]
                .as_array()
                .and_then(|v| v.last())
                .and_then(|v| v.as_str())
                .context("harness catalog has no pi versions")?
                .to_string(),
        };

        let manifest = AgentManifest {
            name: name.clone(),
            harness: Harness {
                kind: "pi".to_string(),
                version: harness_version,
            },
            model: ModelSpec {
                provider: provider.clone(),
                id: model,
                thinking: p.thinking,
            },
            resources: Resources {
                vcpu: p.vcpu.unwrap_or(2),
                memory_mib: p.memory_mib.unwrap_or(2048),
                disk_mib: p.disk_mib.unwrap_or(5120),
                gpu: p.gpu_count.filter(|c| *c > 0).map(|count| GpuResources {
                    count,
                    vram_mib: p.gpu_vram_mib,
                }),
            },
            schedule: Schedule {
                require: p.require.unwrap_or_default(),
                daemon: p.daemon,
            },
            toolchain: Default::default(),
            repos: p
                .repos
                .unwrap_or_default()
                .into_iter()
                .map(|r| Repo {
                    url: r.url,
                    ref_: r.r#ref.unwrap_or_else(|| "main".to_string()),
                })
                .collect(),
            extensions: p
                .extensions
                .unwrap_or_default()
                .into_iter()
                .map(|e| Extension {
                    source: e.source,
                    ref_: e
                        .r#ref
                        .or_else(|| e.url.as_ref().map(|_| "main".to_string())),
                    url: e.url,
                })
                .collect(),
            prompt: Prompt {
                append_system: p.append_system_prompt,
            },
            // Secrets are never set through MCP: the manifest simply scopes
            // the model's provider key (see docs/MCP-PLAN.md §4).
            secrets: SecretScopes {
                providers: vec![provider.clone()],
                extra: vec![],
            },
            egress: Egress {
                allow: p.egress_allow.unwrap_or_default(),
            },
            observability: Observability {
                otel: p.otel_endpoint.map(|endpoint| Otel {
                    endpoint,
                    headers: BTreeMap::new(),
                }),
            },
            lifecycle: Default::default(),
            provision: None,
        };

        self.validate_create(&manifest, &providers, &harnesses, &daemons)?;

        let rendered = toml::to_string_pretty(&manifest).unwrap_or_default();
        let resp = self
            .api
            .post(
                "/api/v1/agents",
                json!({"manifest": serde_json::to_value(&manifest)?}),
            )
            .await?;
        Ok(json!({
            "agent": resp,
            "rendered_manifest_toml": rendered,
            "note": "provisioning runs in the background — poll agent_get / agent_logs for progress",
        }))
    }

    /// Client-side pre-validation so agent_create succeeds on the first
    /// call. The control plane re-checks everything server-side; a miss
    /// here degrades to a clear server error, never a wedged agent.
    fn validate_create(
        &self,
        m: &AgentManifest,
        providers: &Value,
        harnesses: &Value,
        daemons: &Value,
    ) -> Result<()> {
        let catalog = providers["providers"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let ids: Vec<&str> = catalog.keys().map(String::as_str).collect();

        let entry = catalog.get(&m.model.provider).ok_or_else(|| {
            let hint = suggest(&m.model.provider, ids.iter().copied())
                .map(|s| format!(" — did you mean '{s}'?"))
                .unwrap_or_default();
            anyhow::anyhow!(
                "unknown provider '{}'{hint}. Known providers: {}",
                m.model.provider,
                ids.join(", ")
            )
        })?;

        if entry["key_injectable"].as_bool() == Some(false) {
            bail!(
                "provider '{}' can't receive an API key inside the agent VM (OAuth-only). \
                 Choose a key-based provider from llm_providers.",
                m.model.provider
            );
        }
        if entry["key_configured"].as_bool() == Some(false) {
            bail!(
                "no API key configured for provider '{}' — a human must add it, then retry:\n  \
                 suz secrets set provider {} --value <API_KEY>",
                m.model.provider,
                m.model.provider
            );
        }
        let models: Vec<&str> = entry["models"]
            .as_array()
            .map(|ms| ms.iter().filter_map(|m| m["id"].as_str()).collect())
            .unwrap_or_default();
        if !models.is_empty() && !models.contains(&m.model.id.as_str()) {
            let hint = suggest(&m.model.id, models.iter().copied())
                .map(|s| format!("; did you mean '{s}'?"))
                .unwrap_or_default();
            bail!(
                "unknown model '{}' for provider '{}'{hint}",
                m.model.id,
                m.model.provider
            );
        }

        let kinds: Vec<&str> = harnesses["harnesses"]
            .as_object()
            .map(|h| h.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let versions: Vec<&str> = harnesses["harnesses"][&m.harness.kind]["versions"]
            .as_array()
            .map(|vs| vs.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if !kinds.contains(&m.harness.kind.as_str()) {
            bail!(
                "unknown harness '{}' — harness_catalog lists: {}",
                m.harness.kind,
                kinds.join(", ")
            );
        }
        if !versions.contains(&m.harness.version.as_str()) {
            bail!(
                "harness '{}' version '{}' is not provisionable — harness_catalog lists: {}",
                m.harness.kind,
                m.harness.version,
                versions.join(", ")
            );
        }

        let daemon_list: Vec<&Value> = daemons["daemons"]
            .as_array()
            .map(|d| d.iter().collect())
            .unwrap_or_default();
        if let Some(pin) = &m.schedule.daemon {
            let found = daemon_list.iter().any(|d| {
                d["endpoint_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with(pin.as_str()))
                    || d["hostname"].as_str() == Some(pin.as_str())
            });
            if !found {
                let known: Vec<String> = daemon_list
                    .iter()
                    .map(|d| {
                        format!(
                            "{} ({})",
                            d["hostname"].as_str().unwrap_or("?"),
                            &d["endpoint_id"].as_str().unwrap_or("?")
                                [..12.min(d["endpoint_id"].as_str().unwrap_or("?").len())]
                        )
                    })
                    .collect();
                bail!(
                    "no daemon matches pin '{}'. Known daemons: {} — see castellan_list",
                    pin,
                    known.join(", ")
                );
            }
        }
        for key in m.schedule.require.keys() {
            let found = daemon_list.iter().any(|d| {
                d["online"].as_bool() == Some(true)
                    && d["labels"].as_object().is_some_and(|l| l.contains_key(key))
            });
            if !found {
                bail!(
                    "no online daemon carries label '{key}' (schedule.require) — set it with \
                     castellan_labels_set or drop the constraint"
                );
            }
        }
        Ok(())
    }
}

#[tool_router]
impl SuzerainMcp {
    // ── discovery ─────────────────────────────────────────────────────────

    /// List LLM providers with their models, annotated with key_injectable
    /// (can receive an API key in-guest) and key_configured (store holds a
    /// key). Consult BEFORE agent_create: a provider failing either check
    /// will be rejected.
    #[tool(
        description = "List LLM providers + models, annotated with key_injectable/key_configured. Consult before agent_create."
    )]
    pub async fn llm_providers(
        &self,
        Parameters(p): Parameters<LlmProvidersParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.api.get("/api/v1/providers").await {
            Err(e) => outcome(Err(e)),
            Ok(v) => match &p.provider {
                None => outcome(Ok(v)),
                Some(id) => {
                    let all = &v["providers"];
                    if let Some(entry) = all.get(id) {
                        outcome(Ok(json!({id: entry})))
                    } else {
                        let ids: Vec<&str> = all
                            .as_object()
                            .map(|o| o.keys().map(String::as_str).collect())
                            .unwrap_or_default();
                        let hint = suggest(id, ids.iter().copied())
                            .map(|s| format!(" — did you mean '{s}'?"))
                            .unwrap_or_default();
                        outcome(Err(anyhow::anyhow!(
                            "unknown provider '{id}'{hint}. Available: {}",
                            ids.join(", ")
                        )))
                    }
                }
            },
        }
    }

    /// Harness kinds and exact versions castellan can provision (for
    /// agent_create's harness_version).
    #[tool(description = "List agent harnesses and provisionable versions (for agent_create).")]
    pub async fn harness_catalog(&self) -> Result<CallToolResult, ErrorData> {
        outcome(self.api.get("/api/v1/harnesses").await)
    }

    /// Search the pi.dev package catalog (extensions, skills, prompts,
    /// themes). Results include the install source for
    /// agent_create's extensions[].source.
    #[tool(
        description = "Search the pi.dev extension package catalog; results carry the install source for agent_create."
    )]
    pub async fn pi_packages_search(
        &self,
        Parameters(p): Parameters<PiPackagesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .get_query(
                    "/api/v1/pi-packages",
                    &[
                        ("q", p.q.unwrap_or_default()),
                        ("type", p.r#type.unwrap_or_default()),
                        ("page", p.page.unwrap_or(1).to_string()),
                    ],
                )
                .await,
        )
    }

    /// Fleet overview: daemon/agent counts, agents by state, per-daemon
    /// capacity and free resources. Use for sizing agent resources and
    /// checking placement headroom.
    #[tool(
        description = "Fleet overview: daemon/agent counts, capacity and free resources per daemon."
    )]
    pub async fn fleet_overview(&self) -> Result<CallToolResult, ErrorData> {
        outcome(self.api.get("/api/v1/overview").await)
    }

    /// Recent control-plane audit entries: who did what (agent creates,
    /// lifecycle, secret changes by kind/name, daemon approvals).
    #[tool(description = "Recent audit log entries (who did what on the control plane).")]
    pub async fn audit_tail(
        &self,
        Parameters(p): Parameters<AuditParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .get_query(
                    "/api/v1/audit",
                    &[("tail", p.tail.unwrap_or(50).to_string())],
                )
                .await,
        )
    }

    // ── castellans ────────────────────────────────────────────────────────

    /// Enroll a new castellan daemon. Without endpoint_id: returns the
    /// commands a human runs on the new machine. With endpoint_id: approves
    /// that daemon's enrollment.
    #[tool(
        description = "Add a castellan: no arg → enrollment instructions for a human; with endpoint_id → approve the daemon."
    )]
    pub async fn castellan_add(
        &self,
        Parameters(p): Parameters<CastellanAddParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match p.endpoint_id {
            Some(id) => outcome(
                self.api
                    .post("/api/v1/daemons/approve", json!({"endpoint_id": id}))
                    .await,
            ),
            None => match self.api.get("/api/v1/endpoint").await {
                Err(e) => outcome(Err(e)),
                Ok(v) => {
                    let eid = v["endpoint_id"].as_str().unwrap_or("?");
                    outcome(Ok(json!({
                        "instructions_for_a_human": [
                            "On the new machine, install prerequisites (qemu + mise) and the suzerain binaries",
                            format!("castellan init --suzerain {eid}   # prints the new daemon's EndpointId"),
                            "castellan run                        # registers and takes orders",
                            "Then approve it here: castellan_add with the printed EndpointId (or approve via castellan_list pending entries)",
                        ],
                        "suzerain_endpoint_id": eid,
                    })))
                }
            },
        }
    }

    /// Details for one castellan: capacity, usage, GPUs, labels, hosted
    /// agents, recent activity.
    #[tool(
        description = "Get castellan details: capacity, usage, labels, hosted agents, activity."
    )]
    pub async fn castellan_get(
        &self,
        Parameters(p): Parameters<EndpointIdParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .get(&format!("/api/v1/daemons/{}", p.endpoint_id))
                .await,
        )
    }

    /// List castellan daemons (online/offline, capacity, labels). Pending
    /// enrollments are included by default, flagged as pending.
    #[tool(description = "List castellan daemons, including pending enrollments (flagged).")]
    pub async fn castellan_list(
        &self,
        Parameters(p): Parameters<CastellanListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let daemons = self.api.get("/api/v1/daemons").await;
        if p.include_pending.unwrap_or(true) {
            match (daemons, self.api.get("/api/v1/daemons/pending").await) {
                (Ok(mut d), Ok(pend)) => {
                    d["pending"] = pend["pending"].clone();
                    outcome(Ok(d))
                }
                (Ok(d), Err(_)) => outcome(Ok(d)),
                (Err(e), _) => outcome(Err(e)),
            }
        } else {
            outcome(daemons)
        }
    }

    /// Remove a castellan from the fleet. Refuses while agents are assigned
    /// unless force=true (agents are then destroyed first, best-effort).
    /// `confirm` must exactly equal the daemon's FULL EndpointId.
    #[tool(
        description = "Remove a castellan (confirm must equal its full EndpointId; force destroys assigned agents first)."
    )]
    pub async fn castellan_remove(
        &self,
        Parameters(p): Parameters<CastellanRemoveParams>,
    ) -> Result<CallToolResult, ErrorData> {
        // Resolve the full EndpointId so confirm can require an exact match.
        let resolved = match self.api.get("/api/v1/daemons").await {
            Err(e) => return outcome(Err(e)),
            Ok(v) => v["daemons"]
                .as_array()
                .and_then(|ds| {
                    ds.iter().find(|d| {
                        d["endpoint_id"].as_str().is_some_and(|id| {
                            id == p.endpoint_id || id.starts_with(p.endpoint_id.as_str())
                        }) || d["hostname"].as_str() == Some(p.endpoint_id.as_str())
                    })
                })
                .and_then(|d| d["endpoint_id"].as_str().map(str::to_string)),
        };
        let Some(full_id) = resolved else {
            return outcome(Err(anyhow::anyhow!(
                "no daemon matching '{}' — see castellan_list",
                p.endpoint_id
            )));
        };
        if p.confirm != full_id {
            return outcome(Err(anyhow::anyhow!(
                "confirm must exactly equal the daemon's full EndpointId ('{full_id}') — got '{}'. \
                 This is a destructive operation; re-call with confirm set correctly if intended.",
                p.confirm
            )));
        }
        outcome(
            self.api
                .delete(
                    &format!("/api/v1/daemons/{full_id}"),
                    &[("force", p.force.unwrap_or(false).to_string())],
                )
                .await,
        )
    }

    /// Set/remove scheduling labels on a castellan (operator overrides).
    /// Labels drive schedule.require placement for new agents.
    #[tool(
        description = "Set/remove scheduling labels on a castellan (drives schedule.require placement)."
    )]
    pub async fn castellan_labels_set(
        &self,
        Parameters(p): Parameters<LabelsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .post(
                    &format!("/api/v1/daemons/{}/labels", p.endpoint_id),
                    json!({
                        "set": p.set.unwrap_or_default(),
                        "remove": p.remove.unwrap_or_default(),
                    }),
                )
                .await,
        )
    }

    // ── agents ────────────────────────────────────────────────────────────

    /// List agents with public status (running, idle, sleeping, waking,
    /// failed), daemon, model, and resources. Optionally filter by status.
    #[tool(description = "List agents (status, daemon, model, resources); optional status filter.")]
    pub async fn agent_list(
        &self,
        Parameters(p): Parameters<AgentListParams>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.api.get("/api/v1/agents").await {
            Err(e) => outcome(Err(e)),
            Ok(mut v) => {
                if let Some(state) = &p.state {
                    if let Some(agents) = v["agents"].as_array() {
                        let filtered: Vec<Value> = agents
                            .iter()
                            .filter(|a| {
                                a["status"].as_str() == Some(state.as_str())
                                    || a["state"].as_str() == Some(state.as_str())
                            })
                            .cloned()
                            .collect();
                        v["agents"] = json!(filtered);
                    }
                }
                outcome(Ok(v))
            }
        }
    }

    /// Create an agent. Provide EITHER manifest_toml OR structured fields
    /// (name/provider/model are then required). Structured input is
    /// pre-validated against llm_providers / harness_catalog / castellan_list
    /// so creation succeeds on the first call. Secrets are never set here:
    /// the manifest scopes the model provider's key only; anything missing
    /// fails with the exact `suz secrets set …` command a human must run.
    #[tool(
        description = "Create an agent (manifest_toml or structured fields; pre-validated against the discovery catalogs)."
    )]
    pub async fn agent_create(
        &self,
        Parameters(p): Parameters<AgentCreateParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if let Some(toml_text) = &p.manifest_toml {
            return outcome(
                self.api
                    .post("/api/v1/agents", json!({"manifest_toml": toml_text}))
                    .await,
            );
        }
        outcome(self.create_from_structured(p).await)
    }

    /// Agent details: public status, manifest TOML, daemon, session file,
    /// event counts. Poll this after agent_create to track provisioning.
    #[tool(description = "Get agent details (status, manifest, daemon, session file).")]
    pub async fn agent_get(
        &self,
        Parameters(p): Parameters<AgentNameParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(self.api.get(&format!("/api/v1/agents/{}", p.name)).await)
    }

    /// Permanently destroy an agent and delete its registry entry.
    /// `confirm` must exactly equal the agent name.
    #[tool(
        description = "Delete an agent permanently (confirm must exactly equal the agent name)."
    )]
    pub async fn agent_delete(
        &self,
        Parameters(p): Parameters<AgentDeleteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        if p.confirm != p.name {
            return outcome(Err(anyhow::anyhow!(
                "confirm must exactly equal the agent name ('{}') — got '{}'. \
                 This permanently destroys the agent; re-call with confirm set correctly if intended.",
                p.name,
                p.confirm
            )));
        }
        outcome(
            self.api
                .post(&format!("/api/v1/agents/{}/destroy", p.name), json!({}))
                .await,
        )
    }

    /// Operational event log for an agent: lifecycle, crashes, respawns,
    /// provisioning progress. Distinct from the session transcript
    /// (agent_session_events) — use this for debugging failed agents.
    #[tool(
        description = "Agent operational log (lifecycle, crashes, provisioning) — for debugging, not the chat transcript."
    )]
    pub async fn agent_logs(
        &self,
        Parameters(p): Parameters<AgentLogsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .get_query(
                    &format!("/api/v1/agents/{}/logs", p.name),
                    &[("tail", p.tail.unwrap_or(50).to_string())],
                )
                .await,
        )
    }

    /// The agent's session transcript (chat messages, tool calls, system
    /// notices) as a snapshot, newest-last. Filter by role and/or keep the
    /// last N items. Poll after agent_session_send to read the reply.
    #[tool(
        description = "Agent session transcript snapshot (roles filter + tail); poll after agent_session_send."
    )]
    pub async fn agent_session_events(
        &self,
        Parameters(p): Parameters<SessionEventsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .get_query(
                    &format!("/api/v1/agents/{}/session/history", p.name),
                    &[
                        ("tail", p.tail.map(|t| t.to_string()).unwrap_or_default()),
                        ("roles", p.roles.map(|rs| rs.join(",")).unwrap_or_default()),
                    ],
                )
                .await,
        )
    }

    /// Send a message to an agent's session. Sleeping agents wake
    /// automatically (the message is held and delivered once the agent is
    /// up — first replies can take a few minutes). Returns once the
    /// message is accepted; poll agent_session_events (role=assistant) for
    /// the reply.
    #[tool(
        description = "Send a message to an agent's session (sleeping agents wake automatically); poll agent_session_events for the reply."
    )]
    pub async fn agent_session_send(
        &self,
        Parameters(p): Parameters<SessionSendParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .post(
                    &format!("/api/v1/agents/{}/prompt", p.name),
                    json!({"message": p.message, "mode": "prompt"}),
                )
                .await,
        )
    }

    /// Abort the agent's current turn without stopping the agent.
    #[tool(description = "Abort the agent's current turn (the agent stays up).")]
    pub async fn agent_session_abort(
        &self,
        Parameters(p): Parameters<AgentNameParams>,
    ) -> Result<CallToolResult, ErrorData> {
        outcome(
            self.api
                .post(
                    &format!("/api/v1/agents/{}/prompt", p.name),
                    json!({"message": "abort", "mode": "abort"}),
                )
                .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SuzerainMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(
                "Suzerain fleet operator: manage castellan daemons and agents. Agent lifecycle is \
                 automatic: agents suspend themselves after a period of inactivity and wake \
                 transparently when a message arrives — there are NO start/stop/suspend/restore \
                 tools; just create, chat, and delete. Public agent statuses: running (turn in \
                 flight), idle (awake, waiting), sleeping (suspended; wakes on demand), waking, \
                 failed (needs_attention = human intervention required). Sending a message to a \
                 sleeping or failed agent queues it durably and triggers a wake; first replies \
                 can take a few minutes. \
                 Creating an agent: (1) llm_providers to pick a provider/model that is \
                 key_injectable AND key_configured, (2) harness_catalog for the version, \
                 (3) optionally pi_packages_search for extensions and castellan_list for \
                 placement labels/pins, (4) agent_create with structured fields. Provisioning \
                 is asynchronous — poll agent_get/agent_logs. Secrets are NEVER managed via \
                 this server: if agent_create reports a missing secret, relay the exact \
                 `suz secrets set …` command from the error to a human. Destructive tools \
                 (agent_delete, castellan_remove) require confirm to exactly match the \
                 resource name/id — never guess it; ask the user first.",
            );
        info.server_info =
            rmcp::model::Implementation::new("suzerain-mcp", env!("CARGO_PKG_VERSION"));
        info
    }
}
