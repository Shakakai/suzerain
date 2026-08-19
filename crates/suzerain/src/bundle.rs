//! Central bundle store: agent restore bundles (manifest + pi session files +
//! pi-home config) uploaded by daemons on suspend, streamed back out on
//! restore-on-any-server. Files live under `<data>/bundles/<agent_id>/`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use suzerain_protocol::control::BundleMessage;
use suzerain_protocol::manifest::AgentManifest;
use uuid::Uuid;

use crate::identity::data_dir;

/// Bundle storage root: `[bundles] dir` in config.toml wins (e.g. an
/// external drive); default `<data>/bundles`.
pub fn bundle_root() -> PathBuf {
    if let Ok(cfg) = crate::retention::load_config() {
        if let Some(dir) = cfg.bundles.dir.filter(|d| !d.trim().is_empty()) {
            return PathBuf::from(dir);
        }
    }
    data_dir().join("bundles")
}

pub struct StoredBundle {
    pub manifest: AgentManifest,
    pub session_file: Option<String>,
    /// (relative path, absolute host path) pairs.
    pub files: Vec<(String, PathBuf)>,
    /// Upload-time sha256 per relative path (tamper evidence).
    pub hashes: std::collections::BTreeMap<String, String>,
}

pub fn bundle_dir(agent_id: &Uuid) -> PathBuf {
    bundle_root().join(agent_id.to_string())
}

/// Persist an incoming bundle message stream. `start` was already consumed.
/// The files dir is wiped first: periodic refreshes (G3) replace the whole
/// bundle, and stale session files from older uploads must not linger.
pub async fn write_start(
    agent_id: &Uuid,
    manifest: &AgentManifest,
    session_file: Option<&str>,
) -> Result<()> {
    let dir = bundle_dir(agent_id);
    let files = dir.join("files");
    if files.exists() {
        tokio::fs::remove_dir_all(&files).await?;
    }
    tokio::fs::create_dir_all(&files).await?;
    let meta = serde_json::json!({
        "manifest": manifest,
        "session_file": session_file,
        "files": {},
    });
    tokio::fs::write(dir.join("meta.json"), serde_json::to_vec_pretty(&meta)?).await?;
    Ok(())
}

/// Record a file's upload-time checksum in the bundle meta (tamper evidence).
async fn record_hash(agent_id: &Uuid, rel_path: &str, sha256: &str) -> Result<()> {
    let meta_path = bundle_dir(agent_id).join("meta.json");
    let text = tokio::fs::read_to_string(&meta_path).await?;
    let mut meta: serde_json::Value = serde_json::from_str(&text)?;
    meta["files"][rel_path] = serde_json::Value::String(sha256.to_string());
    tokio::fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?).await?;
    Ok(())
}

/// Write one bundle file chunk (whole-file `data` is base64). When a
/// checksum is provided it is verified before writing (G8 integrity).
pub async fn write_file(
    agent_id: &Uuid,
    rel_path: &str,
    data_base64: &str,
    sha256: Option<&str>,
) -> Result<()> {
    if rel_path.contains("..") {
        bail!("unsafe bundle path: {rel_path}");
    }
    let bytes = base64_decode(data_base64)?;
    if let Some(want) = sha256 {
        let got = suzerain_protocol::framing::sha256_hex(&bytes);
        if got != want {
            bail!("bundle checksum mismatch for {rel_path}");
        }
    }
    let dest = bundle_dir(agent_id).join("files").join(rel_path);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let got = suzerain_protocol::framing::sha256_hex(&bytes);
    tokio::fs::write(&dest, bytes).await?;
    record_hash(agent_id, rel_path, &got).await?;
    Ok(())
}

pub async fn load(agent_id: &Uuid) -> Result<StoredBundle> {
    let dir = bundle_dir(agent_id);
    let meta_text = tokio::fs::read_to_string(dir.join("meta.json"))
        .await
        .with_context(|| format!("no bundle stored for agent {agent_id}"))?;
    let meta: serde_json::Value = serde_json::from_str(&meta_text)?;
    let manifest: AgentManifest = serde_json::from_value(meta["manifest"].clone())?;
    let session_file = meta["session_file"].as_str().map(str::to_string);

    let mut files = Vec::new();
    let files_root = dir.join("files");
    collect(&files_root.clone(), &files_root, &mut files);
    let hashes = meta["files"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok(StoredBundle {
        manifest,
        session_file,
        files,
        hashes,
    })
}

fn collect(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push((rel.to_string_lossy().to_string(), path));
        }
    }
}

pub fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(text: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Result<u32> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => bail!("invalid base64 character"),
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            if *c != b'=' {
                n |= val(*c)? << (18 - 6 * i);
            }
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Silence unused warning for the symmetric helper used by restore.
#[allow(dead_code)]
fn _assert_message_type(_: BundleMessage) {}
