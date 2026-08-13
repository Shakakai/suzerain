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
///
/// Covers every API-key provider in `@earendil-works/pi-ai`'s catalog
/// (env var names from pi-ai's `env-api-keys.js`, hosts from each
/// provider's `baseUrl`; host patterns support `*` wildcards, matched by
/// the Gondolin egress hook). Intentionally unsupported:
/// - `openai-codex`, `github-copilot` OAuth flows (no plain API key env var)
///   — though github-copilot does accept COPILOT_GITHUB_TOKEN, so it is mapped.
/// - `amazon-bedrock` (AWS credential chain, not a single key)
/// - `azure-openai-responses` (per-deployment host; no stable allowlist host)
pub fn provider_env_and_host(provider: &str) -> Option<(&'static str, &'static str)> {
    Some(match provider {
        "anthropic" => ("ANTHROPIC_API_KEY", "api.anthropic.com"),
        "openai" => ("OPENAI_API_KEY", "api.openai.com"),
        "google" | "gemini" => ("GEMINI_API_KEY", "generativelanguage.googleapis.com"),
        "google-vertex" => ("GOOGLE_CLOUD_API_KEY", "*-aiplatform.googleapis.com"),
        "kimi" | "kimi-coding" => ("KIMI_API_KEY", "api.kimi.com"),
        "openrouter" => ("OPENROUTER_API_KEY", "openrouter.ai"),
        "groq" => ("GROQ_API_KEY", "api.groq.com"),
        "mistral" => ("MISTRAL_API_KEY", "api.mistral.ai"),
        "xai" => ("XAI_API_KEY", "api.x.ai"),
        "deepseek" => ("DEEPSEEK_API_KEY", "api.deepseek.com"),
        "cerebras" => ("CEREBRAS_API_KEY", "api.cerebras.ai"),
        "ant-ling" => ("ANT_LING_API_KEY", "api.ant-ling.com"),
        "baseten" => ("BASETEN_API_KEY", "inference.baseten.co"),
        "cloudflare-ai-gateway" => ("CLOUDFLARE_API_KEY", "gateway.ai.cloudflare.com"),
        "cloudflare-workers-ai" => ("CLOUDFLARE_API_KEY", "api.cloudflare.com"),
        "fireworks" => ("FIREWORKS_API_KEY", "api.fireworks.ai"),
        "github-copilot" => ("COPILOT_GITHUB_TOKEN", "api.individual.githubcopilot.com"),
        "huggingface" => ("HF_TOKEN", "router.huggingface.co"),
        "minimax" => ("MINIMAX_API_KEY", "api.minimax.io"),
        "minimax-cn" => ("MINIMAX_CN_API_KEY", "api.minimaxi.com"),
        "moonshotai" => ("MOONSHOT_API_KEY", "api.moonshot.ai"),
        "moonshotai-cn" => ("MOONSHOT_API_KEY", "api.moonshot.cn"),
        "nvidia" => ("NVIDIA_API_KEY", "integrate.api.nvidia.com"),
        "opencode" | "opencode-go" => ("OPENCODE_API_KEY", "opencode.ai"),
        "qwen-token-plan" => (
            "QWEN_TOKEN_PLAN_API_KEY",
            "token-plan.ap-southeast-1.maas.aliyuncs.com",
        ),
        "qwen-token-plan-cn" => (
            "QWEN_TOKEN_PLAN_CN_API_KEY",
            "token-plan.cn-beijing.maas.aliyuncs.com",
        ),
        "qwen-token-plan-individual" => (
            "QWEN_TOKEN_PLAN_API_KEY",
            "token-plan.ap-southeast-1.maas.aliyuncs.com",
        ),
        "together" => ("TOGETHER_API_KEY", "api.together.ai"),
        "vercel-ai-gateway" => ("AI_GATEWAY_API_KEY", "ai-gateway.vercel.sh"),
        "xiaomi" => ("XIAOMI_API_KEY", "api.xiaomimimo.com"),
        "xiaomi-token-plan-cn" => (
            "XIAOMI_TOKEN_PLAN_CN_API_KEY",
            "token-plan-cn.xiaomimimo.com",
        ),
        "xiaomi-token-plan-ams" => (
            "XIAOMI_TOKEN_PLAN_AMS_API_KEY",
            "token-plan-ams.xiaomimimo.com",
        ),
        "xiaomi-token-plan-sgp" => (
            "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
            "token-plan-sgp.xiaomimimo.com",
        ),
        "zai" => ("ZAI_API_KEY", "api.z.ai"),
        "zai-coding-cn" => ("ZAI_CODING_CN_API_KEY", "open.bigmodel.cn"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::provider_env_and_host;

    #[test]
    fn covers_pi_catalog_providers() {
        // Spot-check the mapping against pi-ai's catalog.
        assert_eq!(
            provider_env_and_host("anthropic"),
            Some(("ANTHROPIC_API_KEY", "api.anthropic.com"))
        );
        assert_eq!(
            provider_env_and_host("kimi-coding"),
            Some(("KIMI_API_KEY", "api.kimi.com"))
        );
        assert_eq!(
            provider_env_and_host("vercel-ai-gateway"),
            Some(("AI_GATEWAY_API_KEY", "ai-gateway.vercel.sh"))
        );
        assert_eq!(
            provider_env_and_host("google-vertex"),
            Some(("GOOGLE_CLOUD_API_KEY", "*-aiplatform.googleapis.com"))
        );
        assert_eq!(
            provider_env_and_host("huggingface"),
            Some(("HF_TOKEN", "router.huggingface.co"))
        );
        // Aliases kept for older manifests.
        assert_eq!(
            provider_env_and_host("kimi"),
            Some(("KIMI_API_KEY", "api.kimi.com"))
        );
        assert_eq!(
            provider_env_and_host("gemini"),
            Some(("GEMINI_API_KEY", "generativelanguage.googleapis.com"))
        );
        // Unsupported: no stable API-key env var / host.
        assert_eq!(provider_env_and_host("openai-codex"), None);
        assert_eq!(provider_env_and_host("amazon-bedrock"), None);
        assert_eq!(provider_env_and_host("azure-openai-responses"), None);
        assert_eq!(provider_env_and_host("nonsense"), None);
    }
}
