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

use secrecy::ExposeSecret;

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
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            // No keypair yet: generate one so first-time users can write
            // secrets without manual setup (iroh identity does the same).
            let identity = age::x25519::Identity::generate();
            let secret = identity.to_string();
            let public = identity.to_public().to_string();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &path,
                format!(
                    "# created by suzerain\n# public key: {public}\n{}\n",
                    secret.expose_secret()
                ),
            )?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            tracing::info!(path = %path.display(), "generated age identity for secrets store");
            secret.expose_secret().to_string()
        }
    };
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

    if !manifest.repos.is_empty() || !manifest.extensions.is_empty() {
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

/// Fail-fast check that every secret a manifest names actually exists in
/// the store. Runs BEFORE the create flow inserts the registry row and
/// schedules (actions::prepare_create): a missing secret otherwise only
/// surfaced in the background create task, leaving the agent stuck in
/// `provisioning` with nothing but a log warning.
///
/// Checks, collected into one error so the operator can fix everything in
/// a single pass:
/// - each secrets.providers entry has an API-key env mapping AND a key in
///   the store
/// - each secrets.extra entry exists in the store
/// - SSH-form repo/extension clones get a git deploy key
pub fn preflight(manifest: &AgentManifest) -> Result<()> {
    let mut missing: Vec<String> = Vec::new();

    // Providers with no API-key mapping can never be injected into the VM,
    // regardless of what the store holds (catalog::validate_model already
    // rejects such providers as the *model* provider; this catches them in
    // the secrets list).
    for provider in &manifest.secrets.providers {
        if provider_env_and_host(provider).is_none() {
            missing.push(format!(
                "provider '{provider}' has no API-key env mapping (OAuth-only or unknown) — \
                 remove it from secrets.providers or choose a key-based provider"
            ));
        }
    }

    let declares_secrets = !manifest.secrets.providers.is_empty()
        || !manifest.secrets.extra.is_empty()
        || needs_deploy_key(manifest);
    let guard = STORE.read().unwrap();
    match guard.as_ref() {
        None if declares_secrets => {
            bail!(
                "manifest declares secrets but the secrets store is not loaded ({}). \
                 Start the control plane (`suzerain run`), then add the keys, e.g.:\n  \
                 suz secrets set provider <PROVIDER_ID> --value <API_KEY>",
                secrets_path().display()
            );
        }
        None => {}
        Some(store) => {
            for provider in &manifest.secrets.providers {
                if provider_env_and_host(provider).is_some()
                    && !store.providers.contains_key(provider)
                {
                    missing.push(format!(
                        "no API key for provider '{provider}'\n    \
                         fix: suz secrets set provider {provider} --value <API_KEY>"
                    ));
                }
            }
            for name in &manifest.secrets.extra {
                if !store.extra.contains_key(name) {
                    missing.push(format!(
                        "no extra secret '{name}'\n    \
                         fix: suz secrets set extra {name} --value <VALUE>"
                    ));
                }
            }
            if needs_deploy_key(manifest) && !store.git.contains_key("deploy_key") {
                missing.push(
                    "no git deploy key (manifest clones over SSH)\n    \
                     fix: suz secrets set deploy-key < /path/to/id_ed25519"
                        .to_string(),
                );
            }
        }
    }
    if !missing.is_empty() {
        bail!(
            "manifest references missing secrets — a human must add them, then retry \
             the create:\n  - {}",
            missing.join("\n  - ")
        );
    }
    Ok(())
}

/// SSH-form repo/extension clones need the daemon's git deploy key; https
/// clones of public repos (and npm: package sources) don't.
fn needs_deploy_key(manifest: &AgentManifest) -> bool {
    fn is_ssh(url: &str) -> bool {
        url.starts_with("git@") || url.starts_with("ssh://")
    }
    manifest.repos.iter().any(|r| is_ssh(&r.url))
        || manifest.extensions.iter().any(|e| {
            e.url.as_deref().is_some_and(is_ssh)
                || e.source
                    .as_deref()
                    .is_some_and(|s| is_ssh(s) || s.strip_prefix("git:").is_some_and(is_ssh))
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use suzerain_protocol::manifest::AgentManifest;

    /// STORE is process-global: serialize preflight tests and restore the
    /// previous store afterwards.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_store<R>(store: Option<StoreFile>, f: impl FnOnce() -> R) -> R {
        let _g = TEST_LOCK.lock().unwrap();
        let prev = std::mem::replace(&mut *STORE.write().unwrap(), store);
        let result = f();
        *STORE.write().unwrap() = prev;
        result
    }

    fn store_with(providers: &[&str], extra: &[&str], deploy_key: bool) -> StoreFile {
        let mut s = StoreFile::default();
        for p in providers {
            s.providers.insert(p.to_string(), "sk-test".to_string());
        }
        for e in extra {
            s.extra.insert(e.to_string(), "value".to_string());
        }
        if deploy_key {
            s.git
                .insert("deploy_key".to_string(), "-----BEGIN".to_string());
        }
        s
    }

    fn manifest(extra_toml: &str) -> AgentManifest {
        let text = format!(
            r#"
name = "x"
harness = {{ type = "pi", version = "0.84.1" }}
model = {{ provider = "anthropic", id = "claude-sonnet-4-5" }}
{extra_toml}
"#
        );
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn ok_when_no_secrets_declared_and_no_store() {
        with_store(None, || preflight(&manifest("")).unwrap());
    }

    #[test]
    fn ok_when_all_secrets_present() {
        let m = manifest(
            r#"
[secrets]
providers = ["anthropic"]
extra = ["MY_TOKEN@api.example.com"]
"#,
        );
        with_store(
            Some(store_with(
                &["anthropic"],
                &["MY_TOKEN@api.example.com"],
                false,
            )),
            || preflight(&m).unwrap(),
        );
    }

    #[test]
    fn rejects_declared_secrets_without_loaded_store() {
        let m = manifest(
            r#"
[secrets]
providers = ["anthropic"]
"#,
        );
        with_store(None, || {
            let err = preflight(&m).unwrap_err().to_string();
            assert!(err.contains("secrets store is not loaded"), "{err}");
        });
    }

    #[test]
    fn lists_every_missing_secret_in_one_error() {
        let m = manifest(
            r#"
[[repos]]
url = "git@github.com:org/repo.git"

[secrets]
providers = ["anthropic", "openai"]
extra = ["MISSING_TOKEN"]
"#,
        );
        // anthropic present, openai missing; extra missing; deploy key missing.
        with_store(Some(store_with(&["anthropic"], &[], false)), || {
            let err = preflight(&m).unwrap_err().to_string();
            assert!(err.contains("no API key for provider 'openai'"), "{err}");
            assert!(err.contains("no extra secret 'MISSING_TOKEN'"), "{err}");
            assert!(err.contains("no git deploy key"), "{err}");
            assert!(!err.contains("anthropic"), "{err}");
            // Every miss carries the exact remediation command for a human.
            assert!(
                err.contains("fix: suz secrets set provider openai --value <API_KEY>"),
                "{err}"
            );
            assert!(
                err.contains("fix: suz secrets set extra MISSING_TOKEN --value <VALUE>"),
                "{err}"
            );
            assert!(
                err.contains("fix: suz secrets set deploy-key < /path/to/id_ed25519"),
                "{err}"
            );
        });
    }

    #[test]
    fn rejects_unmappable_provider_even_if_store_has_it() {
        let m = manifest(
            r#"
[secrets]
providers = ["openai-codex"]
"#,
        );
        with_store(Some(store_with(&["openai-codex"], &[], false)), || {
            let err = preflight(&m).unwrap_err().to_string();
            assert!(err.contains("no API-key env mapping"), "{err}");
        });
    }

    #[test]
    fn https_repos_need_no_deploy_key() {
        let m = manifest(
            r#"
[[repos]]
url = "https://github.com/octocat/Hello-World.git"
"#,
        );
        with_store(Some(StoreFile::default()), || preflight(&m).unwrap());
    }

    #[test]
    fn ssh_extension_source_needs_deploy_key() {
        let m = manifest(
            r#"
[[extensions]]
source = "git:git@github.com:me/ext.git"
"#,
        );
        with_store(Some(StoreFile::default()), || {
            let err = preflight(&m).unwrap_err().to_string();
            assert!(err.contains("no git deploy key"), "{err}");
        });
        with_store(Some(store_with(&[], &[], true)), || preflight(&m).unwrap());
    }
}
