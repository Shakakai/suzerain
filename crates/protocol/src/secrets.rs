//! Secret bundles: the per-agent slice of the secrets store, delivered by
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
    /// Daemon-scoped git SSH key (one per daemon, Q-E), OpenSSH format.
    /// Held host-side only: gondolin's ssh proxy uses it for upstream auth;
    /// the guest never sees the private key.
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

/// Errors validating an SSH private key (see [`validate_ssh_private_key`]).
#[derive(Debug, thiserror::Error)]
pub enum SshKeyError {
    #[error(
        "not a valid SSH private key ({0}). Accepted: any ssh-keygen private key \
         (ed25519, ecdsa, RSA) in OpenSSH format. Legacy PEM? Convert with: \
         ssh-keygen -p -f <keyfile>"
    )]
    Parse(String),
    #[error(
        "passphrase-protected SSH keys can't be used by agents (git runs \
         non-interactively). Remove the passphrase: ssh-keygen -p -f <keyfile>"
    )]
    Encrypted,
}

/// Validate an SSH private key, returning its algorithm name
/// (`ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-nistp256`, …).
///
/// Accepts any key `ssh-keygen` produces by default — OpenSSH format, all
/// algorithms — so any key an operator already uses for git works. Legacy
/// PEM keys get a conversion hint (`ssh-keygen -p -f <keyfile>`).
pub fn validate_ssh_private_key(key: &str) -> Result<String, SshKeyError> {
    let trimmed = key.trim();
    let parsed = ssh_key::PrivateKey::from_openssh(trimmed)
        .map_err(|e| SshKeyError::Parse(e.to_string()))?;
    if parsed.is_encrypted() {
        return Err(SshKeyError::Encrypted);
    }
    Ok(parsed.algorithm().to_string())
}

#[cfg(test)]
mod tests {
    use super::{provider_env_and_host, validate_ssh_private_key};

    // ── ssh key validation ─────────────────────────────────────────────
    // Throwaway keys generated with ssh-keygen for these tests only.

    const ED25519: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACB49yJ8Ln7nQJi4D7Omph/V/3RCA68U8Gh6Q1xtFRQtigAAAIjL0g8jy9IP\n\
IwAAAAtzc2gtZWQyNTUxOQAAACB49yJ8Ln7nQJi4D7Omph/V/3RCA68U8Gh6Q1xtFRQtig\n\
AAAED9MGIiyezj+L34ruyd5jHTrYSBgVuXlKEVFmPTbUapn3j3InwufudAmLgPs6amH9X/\n\
dEIDrxTwaHpDXG0VFC2KAAAABHRlc3QB\n\
-----END OPENSSH PRIVATE KEY-----\n";

    const ED25519_ENCRYPTED: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBgQIsZNA\n\
JXblwhJwn2J1lBAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIHNQfm17JhmbnlvC\n\
NGJSvW1IsnNVO5fqT0vrnlXPwkkrAAAAkARlEgBD0+/wpTuYRFzm6yVt9BJoBQwiCaHK/H\n\
LQ46b8Y4jQKNNVpsdT4O5U3MDFiY5GfYNrSLVNpBvyCIAFoqkcjOp5JEvziz5rp4xZQvXo\n\
UhGrHtT47TC2wtSSZlQLrz+pW/Nz9uEODJJ2yrvCenpyAbmbDLjj6wygoVAoSpC0e0czfY\n\
jwE6CcCvJIkyomMg==\n\
-----END OPENSSH PRIVATE KEY-----\n";

    const ECDSA: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS\n\
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQQFMgLAVAA5SYY50vXtKnNtVRjlwJb/\n\
0kxK74IdbfqXr02BlXMujt5XlCjkwN2Z+VcED1EIzFFIOQPBn7pVQmd+AAAAsBCuxcUQrs\n\
XFAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBAUyAsBUADlJhjnS\n\
9e0qc21VGOXAlv/STErvgh1t+pevTYGVcy6O3leUKOTA3Zn5VwQPUQjMUUg5A8GfulVCZ3\n\
4AAAAhANh1GyNfxy3HM0FWp+sc4UrFXXxPzYj+EPKVfnJBxzX3AAAAF3RvZGQuY3VsbGVu\n\
QG1hY21pbmkubGFu\n\
-----END OPENSSH PRIVATE KEY-----\n";

    const RSA_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----\n\
MIIEowIBAAKCAQEAoAdj/u/6a+PdiEEnerPEwH8QHeJ1UzafqhJq4nu4cY79MLBS\n\
al1F1SZFGKU4NwuZhZAT9IHsbqgZ1xLAHp1ZRj9n87oPAfSUUOjLmHUMxrHPodEa\n\
-----END RSA PRIVATE KEY-----\n"; // truncated for the negative/PEM case

    #[test]
    fn ssh_key_ed25519_accepted() {
        assert_eq!(validate_ssh_private_key(ED25519).unwrap(), "ssh-ed25519");
    }

    #[test]
    fn ssh_key_ecdsa_accepted() {
        assert_eq!(
            validate_ssh_private_key(ECDSA).unwrap(),
            "ecdsa-sha2-nistp256"
        );
    }

    #[test]
    fn ssh_key_encrypted_rejected_with_guidance() {
        let err = validate_ssh_private_key(ED25519_ENCRYPTED).unwrap_err();
        assert!(
            err.to_string().contains("passphrase"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn ssh_key_garbage_rejected() {
        assert!(validate_ssh_private_key("not a key").is_err());
        assert!(validate_ssh_private_key("").is_err());
        assert!(validate_ssh_private_key(RSA_PEM).is_err()); // truncated
    }

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
