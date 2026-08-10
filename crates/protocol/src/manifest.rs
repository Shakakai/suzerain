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
    #[serde(default)]
    pub toolchain: Toolchain,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub extensions: Vec<Extension>,
    #[serde(default)]
    pub secrets: SecretScopes,
    #[serde(default)]
    pub egress: Egress,
    #[serde(default)]
    pub observability: Observability,
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

/// A pi extension, distributed as its own git repo and checked out at `ref_`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extension {
    pub url: String,
    #[serde(rename = "ref")]
    pub ref_: String,
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
        assert_eq!(m.extensions[0].ref_, "v1.2.0");
        assert_eq!(m.secrets.providers, vec!["openai"]);
        assert!(m.observability.otel.is_some());
    }
}
