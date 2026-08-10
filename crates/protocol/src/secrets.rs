//! Secret bundles: the per-agent slice of the SOPS store, delivered by
//! suzerain inside create/restore flows. Real values travel only over the
//! encrypted iroh channel; the guest VM sees Gondolin placeholder values,
//! with host-side injection restricted to each secret's allowed hosts.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One injectable secret: an env var name, its real value, and the hosts the
/// host-side HTTP hook may inject it for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretEntry {
    pub value: String,
    /// Host patterns the value may be sent to (e.g. ["api.openai.com"]).
    pub hosts: Vec<String>,
}

/// Everything secret an agent needs, sliced from the store by suzerain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretBundle {
    /// Env var name → entry (e.g. OPENAI_API_KEY → {value, [api.openai.com]}).
    #[serde(default)]
    pub env: BTreeMap<String, SecretEntry>,
    /// Daemon-scoped git deploy key (one per daemon, Q-E), PEM/OpenSSH.
    #[serde(default)]
    pub git_ssh_key: Option<String>,
}

impl SecretBundle {
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.git_ssh_key.is_none()
    }

    /// All plaintext values, for redaction registration.
    pub fn values(&self) -> impl Iterator<Item = &str> {
        self.env
            .values()
            .map(|e| e.value.as_str())
            .chain(self.git_ssh_key.as_deref())
    }
}

/// pi provider id → (env var, API host). Used to slice provider keys.
pub fn provider_env_and_host(provider: &str) -> Option<(&'static str, &'static str)> {
    Some(match provider {
        "anthropic" => ("ANTHROPIC_API_KEY", "api.anthropic.com"),
        "openai" => ("OPENAI_API_KEY", "api.openai.com"),
        "google" | "gemini" => ("GEMINI_API_KEY", "generativelanguage.googleapis.com"),
        "kimi" | "kimi-coding" => ("KIMI_API_KEY", "api.kimi.com"),
        "openrouter" => ("OPENROUTER_API_KEY", "openrouter.ai"),
        "groq" => ("GROQ_API_KEY", "api.groq.com"),
        "mistral" => ("MISTRAL_API_KEY", "api.mistral.ai"),
        "xai" => ("XAI_API_KEY", "api.x.ai"),
        "deepseek" => ("DEEPSEEK_API_KEY", "api.deepseek.com"),
        "cerebras" => ("CEREBRAS_API_KEY", "api.cerebras.ai"),
        _ => return None,
    })
}
