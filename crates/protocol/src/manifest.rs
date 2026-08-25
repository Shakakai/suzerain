//! The agent manifest: the single TOML document describing everything a
//! castellan needs to provision and run an agent. Versioned by suzerain and
//! delivered with every spawn/restore.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    /// Long-lived pet name, unique fleet-wide.
    pub name: String,
    pub harness: Harness,
    pub model: ModelSpec,
    /// Resource requests: static VM allocation + scheduling reservations
    /// (Kubernetes-style: fit checks use requests, not live usage).
    #[serde(default)]
    pub resources: Resources,
    /// Placement constraints (labels + optional hard pin).
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub toolchain: Toolchain,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub extensions: Vec<Extension>,
    #[serde(default)]
    pub prompt: Prompt,
    #[serde(default)]
    pub secrets: SecretScopes,
    #[serde(default)]
    pub egress: Egress,
    #[serde(default)]
    pub observability: Observability,
    /// Lifecycle policy overrides (auto-suspend). Omitted = inherit the
    /// control plane's global `[auto_suspend]` config.
    #[serde(default)]
    pub lifecycle: Lifecycle,
    /// Declarative VM bootstrap (docs/UNIFIED-AGENT-API-DESIGN.md §4.8.2).
    /// When present, this **fully replaces** the hardcoded
    /// Alpine/npm/mise/pi provisioning sequence for `harness.type = "pi"`
    /// — no partial-override merging (see §4.8.2's rationale). When
    /// absent, provisioning is unchanged from today.
    #[serde(default)]
    pub provision: Option<ProvisionSpec>,
}

/// Per-agent lifecycle policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lifecycle {
    /// Auto-suspend after this much inactivity: a duration ("10m", "2h"),
    /// or "never" (explicit opt-out — the agent is also exempt from
    /// resource-pressure preemption). Omitted = inherit the global policy.
    #[serde(default)]
    pub auto_suspend: Option<String>,
}

/// Resolved auto-suspend policy for one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoSuspendPolicy {
    /// Follow the global default (no override set).
    Inherit,
    /// Never auto-suspend or preempt this agent.
    Never,
    /// Suspend after this many idle seconds.
    After(u64),
}

impl Lifecycle {
    /// Parse `auto_suspend` into a policy. "default"/"inherit" = Inherit.
    pub fn auto_suspend_policy(&self) -> Result<AutoSuspendPolicy, String> {
        match self.auto_suspend.as_deref().map(str::trim) {
            None | Some("") | Some("default") | Some("inherit") => Ok(AutoSuspendPolicy::Inherit),
            Some("never") => Ok(AutoSuspendPolicy::Never),
            Some(d) => crate::state::parse_duration_secs(d).map(AutoSuspendPolicy::After),
        }
    }
}

/// Resource requests for an agent. Omitted fields take defaults so
/// accounting always works.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default = "default_vcpu")]
    pub vcpu: u32,
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u64,
    #[serde(default = "default_disk_mib")]
    pub disk_mib: u64,
    #[serde(default)]
    pub gpu: Option<GpuResources>,
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            vcpu: default_vcpu(),
            memory_mib: default_memory_mib(),
            disk_mib: default_disk_mib(),
            gpu: None,
        }
    }
}

fn default_vcpu() -> u32 {
    2
}
fn default_memory_mib() -> u64 {
    2048
}
fn default_disk_mib() -> u64 {
    5120
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct GpuResources {
    pub count: u32,
    /// Minimum free VRAM per GPU; nvidia = measured, apple = unified free
    /// memory, other = request fails with a clear error.
    #[serde(default)]
    pub vram_mib: Option<u64>,
}

/// Placement constraints: arbitrary label subset match + optional hard pin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schedule {
    /// Every k=v must exactly match the daemon's effective labels.
    #[serde(default)]
    pub require: std::collections::BTreeMap<String, String>,
    /// Hard pin: endpoint-id prefix or hostname.
    #[serde(default)]
    pub daemon: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Harness {
    /// Harness kind; only "pi" is supported in v1.
    #[serde(rename = "type")]
    pub kind: String,
    /// Pinned harness version (e.g. pi version installed via npm in the guest).
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub thinking: Option<String>,
}

/// In-guest toolchain, rendered to a workspace `mise.toml` at provision time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Toolchain {
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
}

/// A git repository cloned fresh over SSH into the agent workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub url: String,
    #[serde(rename = "ref", default = "default_ref")]
    pub ref_: String,
}

fn default_ref() -> String {
    "main".to_string()
}

/// A pi extension/package deployed with the agent.
///
/// Two forms, exactly one required:
/// - `source`: a pi package install source, installed with `pi install`
///   (e.g. `npm:@scope/pkg`, `npm:@scope/pkg@1.2.3`,
///   `git:github.com/user/repo`, `git:github.com/user/repo@v1`) — this is
///   what the pi.dev package catalog yields.
/// - `url` + `ref`: a git repo cloned into the agent's pi-home extensions
///   dir (legacy/pinned form).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extension {
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(rename = "ref")]
    pub ref_: Option<String>,
}

/// Prompt customization, rendered into the agent's isolated pi-home.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prompt {
    /// Text written to `APPEND_SYSTEM.md` in the agent's pi-home; pi
    /// appends it to the system prompt on every run.
    #[serde(default)]
    pub append_system: Option<String>,
}

/// Which entries of the secrets store this agent may receive (sliced by suzerain).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretScopes {
    /// LLM providers whose keys the agent needs (e.g. ["openai"]).
    #[serde(default)]
    pub providers: Vec<String>,
    /// Extra named secrets from the store.
    #[serde(default)]
    pub extra: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Egress {
    /// Extra allowlisted hosts beyond provider/git/npm/otel endpoints.
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Observability {
    #[serde(default)]
    pub otel: Option<Otel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Otel {
    pub endpoint: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

// ── declarative provisioning (§4.8.2) ───────────────────────────────────

/// A harness-neutral bootstrap spec: packages, extra mounts, typed package
/// installs, an escape-hatch script list, isolation trust, and prompt
/// customization. Steps run in file order within each array (`packages`,
/// then `mounts`, then `install`, then `run`) — no implicit parallelism or
/// dependency graph (§4.8.3, a deliberate simplicity choice).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvisionSpec {
    /// Reserved for future use (a non-Alpine guest rootfs) — accepted and
    /// stored, not yet honored by any `Provisioner` implementation.
    #[serde(default)]
    pub base_image: Option<String>,
    /// OS packages installed before anything else (e.g. via `apk add`).
    #[serde(default)]
    pub packages: Vec<String>,
    /// Host→guest bind mounts beyond the standard `/agent` mount.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// Package installs, run in listed order via a named resolver —
    /// idempotency is the resolver's responsibility (§4.8.3).
    #[serde(default)]
    pub install: Vec<InstallEntry>,
    /// Arbitrary scripts — the escape hatch for anything the built-in
    /// resolvers don't cover, not the primary mechanism.
    #[serde(default)]
    pub run: Vec<RunEntry>,
    #[serde(default)]
    pub trust: TrustSpec,
    #[serde(default)]
    pub prompt: Prompt,
}

/// A host→guest bind mount, in addition to the always-mounted
/// workspace/pi-home/sessions/extensions dirs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Relative to the agent's host root dir (`AgentPaths::root`).
    pub host: String,
    /// Absolute path in the guest.
    pub guest: String,
    #[serde(default)]
    pub read_only: bool,
}

/// A typed package install. `resolver` selects the variant (internally
/// tagged, so `resolver = "npm"` plus that variant's fields is exactly one
/// TOML table — see the `[[provision.install]]` examples in
/// docs/UNIFIED-AGENT-API-DESIGN.md §4.8.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "resolver", rename_all = "lowercase")]
pub enum InstallEntry {
    Npm {
        package: String,
        #[serde(default)]
        version: Option<String>,
        /// Install prefix; default `/agent/toolchain/global`.
        #[serde(default)]
        prefix: Option<String>,
    },
    Git {
        url: String,
        #[serde(rename = "ref", default = "default_ref")]
        ref_: String,
        /// Absolute destination path in the guest.
        dest: String,
    },
    Mise {
        tools: BTreeMap<String, String>,
    },
}

/// An escape-hatch script, run at one of two points relative to the
/// harness process starting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunEntry {
    pub when: RunWhen,
    pub script: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunWhen {
    PreStart,
    /// Not yet implemented — `Provisioner::provision` runs before the
    /// harness process is spawned and has no hook for "after start" today;
    /// rejected at validation time rather than silently dropped.
    PostStart,
}

/// Isolated pi-home trust: which host-mounted paths the harness may treat
/// as trusted. Defaults to just the workspace when `[provision]` is
/// present but `[provision.trust]` is omitted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustSpec {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plan_example_manifest() {
        let text = r#"
name = "researcher-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "openai", id = "gpt-5", thinking = "high" }

[toolchain]
tools = { node = "22", python = "3.12" }

[[repos]]
url = "git@github.com:org/repo.git"

[[extensions]]
url = "git@github.com:me/deep-research-ext.git"
ref = "v1.2.0"

[secrets]
providers = ["openai"]

[observability.otel]
endpoint = "https://otel.example.com"
"#;
        let m: AgentManifest = toml::from_str(text).unwrap();
        assert_eq!(m.name, "researcher-1");
        assert_eq!(m.harness.kind, "pi");
        assert_eq!(m.repos[0].ref_, "main"); // default applied
        assert_eq!(m.extensions[0].ref_.as_deref(), Some("v1.2.0"));
        assert_eq!(m.secrets.providers, vec!["openai"]);
        assert!(m.observability.otel.is_some());
    }

    #[test]
    fn parses_prompt_and_source_extensions() {
        let text = r#"
name = "auditor-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "anthropic", id = "claude-sonnet-4-5" }

[[extensions]]
source = "npm:@vigolium/piolium"

[[extensions]]
source = "git:github.com/user/repo@v1"

[prompt]
append_system = """
You are a meticulous security auditor.
Always cite file paths.
"""
"#;
        let m: AgentManifest = toml::from_str(text).unwrap();
        assert_eq!(
            m.extensions[0].source.as_deref(),
            Some("npm:@vigolium/piolium")
        );
        assert!(m.extensions[0].url.is_none());
        assert_eq!(
            m.extensions[1].source.as_deref(),
            Some("git:github.com/user/repo@v1")
        );
        let append = m.prompt.append_system.as_deref().unwrap_or_default();
        assert!(append.contains("security auditor"), "{append}");
    }

    #[test]
    fn parses_declarative_provision_spec() {
        let text = r#"
name = "declarative-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "openai", id = "gpt-5" }

[provision]
base_image = "alpine:3.20"
packages = ["git", "curl", "bash", "ca-certificates"]

[[provision.mounts]]
host = "extra-data"
guest = "/agent/extra"
read_only = true

[[provision.install]]
resolver = "npm"
package = "@earendil-works/pi-coding-agent"
version = "0.84.1"
prefix = "/agent/toolchain/global"

[[provision.install]]
resolver = "git"
url = "https://github.com/octocat/Hello-World.git"
ref = "master"
dest = "/agent/workspace/Hello-World"

[[provision.install]]
resolver = "mise"
tools = { node = "20", python = "3.12" }

[[provision.run]]
when = "pre_start"
script = "echo hi > /agent/workspace/marker"
env = { FOO = "bar" }

[provision.trust]
paths = ["/agent/workspace"]

[provision.prompt]
append_system = "You are ..."
"#;
        let m: AgentManifest = toml::from_str(text).unwrap();
        let spec = m.provision.expect("provision spec present");
        assert_eq!(spec.base_image.as_deref(), Some("alpine:3.20"));
        assert_eq!(
            spec.packages,
            vec!["git", "curl", "bash", "ca-certificates"]
        );
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.mounts[0].guest, "/agent/extra");
        assert!(spec.mounts[0].read_only);
        assert_eq!(spec.install.len(), 3);
        match &spec.install[0] {
            InstallEntry::Npm {
                package, version, ..
            } => {
                assert_eq!(package, "@earendil-works/pi-coding-agent");
                assert_eq!(version.as_deref(), Some("0.84.1"));
            }
            other => panic!("expected Npm, got {other:?}"),
        }
        match &spec.install[1] {
            InstallEntry::Git { url, ref_, dest } => {
                assert_eq!(url, "https://github.com/octocat/Hello-World.git");
                assert_eq!(ref_, "master");
                assert_eq!(dest, "/agent/workspace/Hello-World");
            }
            other => panic!("expected Git, got {other:?}"),
        }
        match &spec.install[2] {
            InstallEntry::Mise { tools } => {
                assert_eq!(tools.get("node").map(String::as_str), Some("20"));
            }
            other => panic!("expected Mise, got {other:?}"),
        }
        assert_eq!(spec.run.len(), 1);
        assert_eq!(spec.run[0].when, RunWhen::PreStart);
        assert_eq!(spec.trust.paths, vec!["/agent/workspace"]);
        assert_eq!(spec.prompt.append_system.as_deref(), Some("You are ..."));
    }

    #[test]
    fn provision_absent_by_default() {
        let text = r#"
name = "plain-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "openai", id = "gpt-5" }
"#;
        let m: AgentManifest = toml::from_str(text).unwrap();
        assert!(m.provision.is_none());
    }
}
