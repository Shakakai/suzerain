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

/// Which entries of the SOPS store this agent may receive (sliced by suzerain).
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
}
