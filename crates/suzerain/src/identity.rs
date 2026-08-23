//! Node identity: a persistent iroh SecretKey per node. The public half
//! (EndpointId) is the node's identity fleet-wide — enrollment is pinning a
//! daemon's EndpointId in suzerain's allowlist.

use std::path::PathBuf;

use anyhow::{Context, Result};
use iroh::SecretKey;

pub fn data_dir() -> PathBuf {
    std::env::var("SUZERAIN_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            PathBuf::from(home).join(".local/share/suzerain")
        })
}

/// Rename a legacy file to its shared-home name (no-op when the new name
/// exists or the legacy file is gone). Falls back to the legacy path when
/// the rename fails, so callers never lose state.
pub fn migrate_name(dir: &std::path::Path, legacy: &str, current: &str) -> PathBuf {
    let new = dir.join(current);
    let old = dir.join(legacy);
    if !new.exists() && old.exists() {
        match std::fs::rename(&old, &new) {
            Ok(()) => tracing::info!("migrated {legacy} → {current}"),
            Err(err) => {
                tracing::warn!(
                    "renaming {legacy} to {current} failed ({err:#}); using legacy name"
                );
                return old;
            }
        }
    }
    new
}

/// This node's identity file within the shared fleet home.
pub fn key_path() -> PathBuf {
    migrate_name(&data_dir(), "identity.key", "suzerain.key")
}

/// Load the node's secret key, generating and persisting one on first run.
pub fn load_or_create_secret_key() -> Result<SecretKey> {
    let path = key_path();
    if path.exists() {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let bytes: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .with_context(|| format!("{} is not a 32-byte secret key", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    std::fs::create_dir_all(data_dir())?;
    let key = SecretKey::generate();
    std::fs::write(&path, key.to_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_name_renames_legacy_file() {
        let dir = std::env::temp_dir().join(format!("suz-idtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("identity.key"), [3u8; 32]).unwrap();
        let path = migrate_name(&dir, "identity.key", "suzerain.key");
        assert_eq!(path, dir.join("suzerain.key"));
        assert_eq!(std::fs::read(&path).unwrap(), vec![3u8; 32]);
        assert!(!dir.join("identity.key").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_name_prefers_current_name() {
        let dir = std::env::temp_dir().join(format!("suz-idtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("identity.key"), [1u8; 32]).unwrap();
        std::fs::write(dir.join("suzerain.key"), [2u8; 32]).unwrap();
        let path = migrate_name(&dir, "identity.key", "suzerain.key");
        assert_eq!(std::fs::read(&path).unwrap(), vec![2u8; 32]);
        assert!(dir.join("identity.key").exists()); // legacy left for manual reconcile
        std::fs::remove_dir_all(&dir).ok();
    }
}
