//! Workspace connection profiles (`~/.config/suzy/config.toml`).
//!
//! A workspace is a connection to one suzerain control plane over the iroh
//! operator channel: the control plane's EndpointId is both its address
//! (reachable anywhere iroh reaches) and its identity. Authorization is
//! Suzy's own iroh public key, which the operator adds to the control
//! plane's `[operator] allow` list.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use suzerain_client::iroh::SecretKey;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceCfg {
    pub name: String,
    /// The control plane's iroh EndpointId (address + identity in one).
    pub endpoint_id: String,
    /// Test-only: dial a full address directly (skips discovery).
    #[serde(default, skip)]
    pub test_addr: Option<suzerain_client::iroh::EndpointAddr>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub workspaces: Vec<WorkspaceCfg>,
    /// "dark" | "light" (default dark).
    #[serde(default)]
    pub theme: String,
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("suzy")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn load() -> Config {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))
            .unwrap_or_else(|e| {
                tracing::warn!("{e:#}; starting with empty config");
                Config::default()
            }),
        Err(_) => Config::default(),
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    save_to(&config_path(), cfg)
}

pub fn save_to(path: &std::path::Path, cfg: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(cfg)?)?;
    Ok(())
}

/// Suzy's iroh identity, persisted at `~/.config/suzy/iroh.key` (raw 32
/// bytes, mode 0600). The public half is what operators allowlist.
pub fn load_or_create_key() -> Result<SecretKey> {
    let path = config_dir().join("iroh.key");
    if let Ok(bytes) = std::fs::read(&path) {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("{}: bad key length", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    let key = SecretKey::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, key.to_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(key)
}
