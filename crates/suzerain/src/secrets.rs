//! Age-encrypted secrets store (pure Rust — the `age`/rage crate). The store
//! is `$SUZERAIN_HOME/secrets.age`: an armored age file wrapping a YAML
//! payload, encrypted to the operator's age recipient from `keys.txt`
//! (`SOPS_AGE_KEY_FILE` or `~/.config/sops/age/keys.txt`).
//!
//! Plaintext exists only in process memory; all mutations are atomic
//! (encrypt to temp → verify decrypt → rename). The legacy
//! `secrets.sops.yaml` is migrated once at startup via a final sops-CLI call.
//!
//! Payload format:
//! ```yaml
//! providers:
//!   kimi-coding: "sk-…"
//! git:
//!   deploy_key: "-----BEGIN OPENSSH PRIVATE KEY-----…"
//! extra:
//!   some-name: "…"
//! ```

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::iter;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::RwLock;

use age::armor::{ArmoredReader, ArmoredWriter, Format};
use anyhow::{bail, Context, Result};
use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::secrets::{provider_env_and_host, SecretBundle, SecretEntry};

use crate::identity::data_dir;

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct StoreFile {
    #[serde(default)]
    providers: BTreeMap<String, String>,
    #[serde(default)]
    git: BTreeMap<String, String>,
    #[serde(default)]
    extra: BTreeMap<String, String>,
}

static STORE: RwLock<Option<StoreFile>> = RwLock::new(None);
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn secrets_path() -> PathBuf {
    std::env::var("SUZERAIN_SECRETS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| data_dir().join("secrets.age"))
}

fn legacy_sops_path() -> PathBuf {
    data_dir().join("secrets.sops.yaml")
}

fn keys_file() -> PathBuf {
    std::env::var("SOPS_AGE_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config/sops/age/keys.txt")
        })
}

// ── age identity / recipient ───────────────────────────────────────────────

fn identities() -> Result<Vec<age::x25519::Identity>> {
    let path = keys_file();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading age identity from {}", path.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("AGE-SECRET-KEY-") {
            out.push(
                age::x25519::Identity::from_str(line)
                    .map_err(|e| anyhow::anyhow!("bad identity in keys.txt: {e}"))?,
            );
        }
    }
    if out.is_empty() {
        bail!("no AGE-SECRET-KEY identities found in {}", path.display());
    }
    Ok(out)
}

fn recipient() -> Result<age::x25519::Recipient> {
    Ok(identities()?[0].to_public())
}

// ── encrypt / decrypt ───────────────────────────────────────────────────────

fn decrypt_file(path: &PathBuf) -> Result<StoreFile> {
    let encrypted = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let armor = ArmoredReader::new(&encrypted[..]);
    let decryptor = age::Decryptor::new(armor).context("parsing age file")?;
    let ids = identities()?;
    let id_refs: Vec<&dyn age::Identity> = ids.iter().map(|i| i as &dyn age::Identity).collect();
    let mut reader = decryptor
        .decrypt(id_refs.iter().copied())
        .context("decrypting secrets.age (identity mismatch?)")?;
    let mut plain = String::new();
    reader.read_to_string(&mut plain)?;
    Ok(serde_yaml::from_str(&plain)?)
}

fn encrypt_payload(plain: &str) -> Result<Vec<u8>> {
    let recipient = recipient()?;
    let encryptor = age::Encryptor::with_recipients(iter::once(&recipient as _))
        .map_err(|e| anyhow::anyhow!("building encryptor: {e}"))?;
    let mut encrypted = Vec::new();
    {
        let armor = ArmoredWriter::wrap_output(&mut encrypted, Format::AsciiArmor)?;
        let mut writer = encryptor.wrap_output(armor)?;
        writer.write_all(plain.as_bytes())?;
        writer.finish()?.finish()?;
    }
    Ok(encrypted)
}

// ── load / migrate ─────────────────────────────────────────────────────────

/// Load (or reload) the store. Missing file = empty store (agents then get
/// no keys and fail loudly at spawn). Migrates a legacy sops store once.
pub fn load() -> Result<()> {
    migrate_legacy_once()?;
    let path = secrets_path();
    if !path.exists() {
        *STORE.write().unwrap() = Some(StoreFile::default());
        tracing::warn!(path = %path.display(), "no secrets store found; agents will get no provider keys");
        return Ok(());
    }
    let store = decrypt_file(&path)?;
    *STORE.write().unwrap() = Some(store);
    tracing::info!(path = %path.display(), "secrets store loaded");
    Ok(())
}

/// One-time migration from the legacy sops-CLI store (final sops call ever).
fn migrate_legacy_once() -> Result<()> {
    let age_path = secrets_path();
    let legacy = legacy_sops_path();
    if age_path.exists() || !legacy.exists() {
        return Ok(());
    }
    let output = std::process::Command::new("sops")
        .args(["-d", "--output-type", "yaml"])
        .arg(&legacy)
        .output()
        .context("decrypting legacy secrets.sops.yaml for migration")?;
    if !output.status.success() {
        bail!(
            "legacy sops decrypt failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let plain = String::from_utf8_lossy(&output.stdout);
    let store: StoreFile = serde_yaml::from_str(&plain)?;
    let encrypted = encrypt_payload(&plain)?;
    std::fs::write(&age_path, encrypted)?;
    std::fs::rename(&legacy, legacy.with_extension("migrated"))?;
    // Keep the parsed store for this boot without re-decrypting.
    *STORE.write().unwrap() = Some(store);
    tracing::info!("migrated secrets.sops.yaml → secrets.age");
    Ok(())
}

// ── slicing (unchanged semantics) ──────────────────────────────────────────

pub fn status() -> Vec<String> {
    inventory()
        .into_iter()
        .map(|(k, n)| format!("{k}:{n}"))
        .collect()
}

/// Slice exactly what this agent needs (Q7).
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

// ── inventory / mutations / reveal (M4) ────────────────────────────────────

pub fn inventory() -> Vec<(String, String)> {
    let guard = STORE.read().unwrap();
    match guard.as_ref() {
        Some(s) => {
            let mut out: Vec<(String, String)> = s
                .providers
                .keys()
                .map(|k| ("provider".into(), k.clone()))
                .collect();
            if s.git.contains_key("deploy_key") {
                out.push(("git".into(), "deploy_key".into()));
            }
            out.extend(s.extra.keys().map(|k| ("extra".into(), k.clone())));
            out
        }
        None => vec![],
    }
}

/// Apply a mutation to a CLONE of the store; only if the atomic persist
/// succeeds does the change take effect (disk + memory stay consistent).
fn mutate(f: impl FnOnce(&mut StoreFile)) -> Result<()> {
    let mut candidate = {
        let guard = STORE.read().unwrap();
        guard.as_ref().context("secrets store not loaded")?.clone()
    };
    f(&mut candidate);
    persist(&candidate)
}

fn persist(candidate: &StoreFile) -> Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap();
    let plain = serde_yaml::to_string(candidate)?;
    let encrypted = encrypt_payload(&plain)?;
    let path = secrets_path();
    let tmp = path.with_extension("age.tmp");
    std::fs::write(&tmp, &encrypted)?;
    // Verify the temp file decrypts cleanly before replacing.
    decrypt_file(&tmp).context("verification decrypt of new store failed")?;
    std::fs::rename(&tmp, &path)?;
    *STORE.write().unwrap() = Some(candidate.clone());
    Ok(())
}

pub fn set_provider(id: &str, value: &str) -> Result<()> {
    if id.trim().is_empty() || value.trim().is_empty() {
        bail!("provider id and value are required");
    }
    mutate(|s| {
        s.providers.insert(id.to_string(), value.to_string());
    })
}

pub fn delete_provider(id: &str) -> Result<()> {
    mutate(|s| {
        s.providers.remove(id);
    })
}

pub fn set_deploy_key(value: &str) -> Result<()> {
    if !value.contains("PRIVATE KEY") {
        bail!("value doesn't look like a private key");
    }
    mutate(|s| {
        s.git.insert("deploy_key".into(), value.to_string());
    })
}

pub fn delete_deploy_key() -> Result<()> {
    mutate(|s| {
        s.git.remove("deploy_key");
    })
}

pub fn set_extra(name: &str, value: &str) -> Result<()> {
    if name.trim().is_empty() || value.trim().is_empty() {
        bail!("name and value are required");
    }
    mutate(|s| {
        s.extra.insert(name.to_string(), value.to_string());
    })
}

pub fn delete_extra(name: &str) -> Result<()> {
    mutate(|s| {
        s.extra.remove(name);
    })
}

/// Reveal a value once (audited by the caller). kind: provider|extra|git.
pub fn reveal(kind: &str, name: &str) -> Result<String> {
    let guard = STORE.read().unwrap();
    let store = guard.as_ref().context("secrets store not loaded")?;
    let value = match kind {
        "provider" => store.providers.get(name),
        "extra" => store.extra.get(name),
        "git" => store.git.get(name),
        other => bail!("unknown secret kind '{other}'"),
    };
    value.cloned().context("secret not found")
}

pub fn secrets_display_path() -> PathBuf {
    secrets_path()
}
