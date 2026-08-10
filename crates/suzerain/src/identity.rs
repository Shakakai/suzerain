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

/// Load the node's secret key, generating and persisting one on first run.
pub fn load_or_create_secret_key() -> Result<SecretKey> {
    let path = data_dir().join("identity.key");
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
