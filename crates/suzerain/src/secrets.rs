//! SOPS-backed secrets store. The encrypted store lives at
//! `$SUZERAIN_HOME/secrets.sops.yaml` (age-encrypted; decryption via the
//! `sops` CLI, which honors SOPS_AGE_KEY_FILE / ~/.config/sops/age/keys.txt).
//! Plaintext exists only in process memory.
//!
//! Store format:
//! ```yaml
//! providers:
//!   kimi-coding: "sk-…"        # keyed by pi provider id
//!   anthropic: "sk-ant-…"
//! git:
//!   deploy_key: |              # one deploy key per daemon fleet (Q-E)
//!     -----BEGIN OPENSSH PRIVATE KEY-----
//!     …
//! extra:
//!   some-name: "…"             # arbitrary named secrets (manifest secrets.extra)
//! ```

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::{bail, Context, Result};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::secrets::{provider_env_and_host, SecretBundle, SecretEntry};

use crate::identity::data_dir;

#[derive(Debug, Default, serde::Deserialize)]
struct StoreFile {
    #[serde(default)]
    providers: BTreeMap<String, String>,
    #[serde(default)]
    git: BTreeMap<String, String>,
    #[serde(default)]
    extra: BTreeMap<String, String>,
}

static STORE: RwLock<Option<StoreFile>> = RwLock::new(None);

pub fn secrets_path() -> PathBuf {
    std::env::var("SUZERAIN_SECRETS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join("secrets.sops.yaml"))
}

/// Load (or reload) the store: `sops -d` into memory. Missing file = empty
/// store (agents then get no keys and fail loudly at spawn).
pub fn load() -> Result<()> {
    let path = secrets_path();
    if !path.exists() {
        *STORE.write().unwrap() = Some(StoreFile::default());
        tracing::warn!(path = %path.display(), "no secrets store found; agents will get no provider keys");
        return Ok(());
    }
    let output = std::process::Command::new("sops")
        .args(["-d", "--output-type", "json"])
        .arg(&path)
        .output()
        .with_context(|| format!("running sops on {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "sops decryption failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let store: StoreFile = serde_json::from_slice(&output.stdout)?;
    *STORE.write().unwrap() = Some(store);
    tracing::info!(path = %path.display(), "secrets store loaded");
    Ok(())
}

/// Which provider ids have keys in the store (no values exposed).
pub fn status() -> Vec<String> {
    let guard = STORE.read().unwrap();
    match guard.as_ref() {
        Some(s) => {
            let mut v: Vec<String> = s.providers.keys().cloned().collect();
            if s.git.contains_key("deploy_key") {
                v.push("git:deploy_key".into());
            }
            v.extend(s.extra.keys().map(|k| format!("extra:{k}")));
            v
        }
        None => vec![],
    }
}

/// Slice exactly what this agent needs (Q7): declared providers' keys mapped
/// to their env var + API host, the daemon git key if the agent clones repos,
/// and any named extras.
pub fn slice_for(manifest: &AgentManifest) -> Result<SecretBundle> {
    let guard = STORE.read().unwrap();
    let store = guard.as_ref().context("secrets store not loaded")?;
    let mut bundle = SecretBundle::default();

    for provider in &manifest.secrets.providers {
        let Some((env_var, host)) = provider_env_and_host(provider) else {
            tracing::warn!(provider, "unknown provider mapping; skipping");
            continue;
        };
        let value = store
            .providers
            .get(provider)
            .with_context(|| format!("secrets store has no key for provider '{provider}'"))?;
        bundle.env.insert(
            env_var.to_string(),
            SecretEntry {
                value: value.clone(),
                hosts: vec![host.to_string()],
            },
        );
    }

    if !manifest.repos.is_empty() {
        bundle.git_ssh_key = store.git.get("deploy_key").cloned();
    }

    for name in &manifest.secrets.extra {
        let value = store
            .extra
            .get(name)
            .with_context(|| format!("secrets store has no extra secret '{name}'"))?;
        // Extras are env-injected without placeholder hosts unless suffixed
        // with host patterns, e.g. "MY_KEY@api.example.com".
        let (env_var, hosts) = match name.split_once('@') {
            Some((var, host)) => (var.to_string(), vec![host.to_string()]),
            None => (name.clone(), vec![]),
        };
        bundle.env.insert(
            env_var,
            SecretEntry {
                value: value.clone(),
                hosts,
            },
        );
    }

    Ok(bundle)
}
