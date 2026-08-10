//! Agent provisioning: boot the Gondolin VM, install base tooling, clone
//! repos and extension repos, set up the isolated pi-home, install the pinned
//! pi version, and apply the toolchain.
//!
//! Everything persistent lives under the host agent dir (`AgentPaths`),
//! mounted into the guest at `/agent` — the VM itself is disposable.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use suzerain_protocol::manifest::AgentManifest;

use crate::driver::DriverClient;
use crate::state::{AgentPaths, AgentRecord};

/// Map a pi provider id to the env var it authenticates with.
/// (Phase 4 replaces env passthrough with SOPS-sliced Gondolin placeholder
/// hooks; for now the daemon passes its own env through for declared
/// providers only.)
fn provider_env_var(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "google" | "gemini" => "GEMINI_API_KEY",
        "kimi" | "kimi-coding" => "KIMI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "groq" => "GROQ_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        "xai" => "XAI_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "cerebras" => "CEREBRAS_API_KEY",
        _ => return None,
    })
}

/// Collect the env passed into the guest for this agent: only the declared
/// providers' keys, plus OTEL if configured. Never the daemon's whole env.
pub fn agent_env(record: &AgentRecord) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for provider in &record.manifest.secrets.providers {
        if let Some(var) = provider_env_var(provider) {
            match std::env::var(var) {
                Ok(value) if !value.is_empty() => {
                    env.insert(var.to_string(), value);
                }
                _ => warn!(
                    provider,
                    var, "declared provider key not found in daemon env"
                ),
            }
        } else {
            warn!(provider, "no known env var mapping for provider");
        }
    }
    if let Some(otel) = &record.manifest.observability.otel {
        env.insert("OTEL_EXPORTER_OTLP_ENDPOINT".into(), otel.endpoint.clone());
        if !otel.headers.is_empty() {
            let headers = otel
                .headers
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            env.insert("OTEL_EXPORTER_OTLP_HEADERS".into(), headers);
        }
        env.insert("OTEL_SERVICE_NAME".into(), record.name.clone());
    }
    env.into_iter().collect()
}

/// Full provisioning of a fresh agent. Idempotent-ish: safe to re-run after
/// partial failure (steps that already completed are skipped or cheap).
pub async fn provision(driver: &DriverClient, record: &AgentRecord) -> Result<()> {
    let paths = AgentPaths::for_agent(&record.id);
    let manifest = &record.manifest;

    // 1. Host-side layout.
    for dir in [
        &paths.workspace,
        &paths.pi_home,
        &paths.sessions,
        &paths.extensions,
    ] {
        tokio::fs::create_dir_all(dir).await?;
    }
    tokio::fs::write(
        paths.root.join("manifest.toml"),
        toml::to_string_pretty(manifest)?,
    )
    .await?;

    // 2. Boot the VM with the agent dir mounted at /agent.
    info!(agent = %record.name, "booting VM");
    driver
        .boot(
            &[("/agent".into(), paths.guest.to_string_lossy().into())],
            &[],
            &format!("castellan-{}", record.name),
        )
        .await?;

    // 3. Base packages (small; the guest rootfs is only ~260MB — everything
    // big installs onto the host-mounted /agent volume instead).
    info!(agent = %record.name, "installing base packages in guest");
    driver
        .sh("apk add --no-cache git curl bash ca-certificates", &[])
        .await
        .context("installing base packages")?;

    // 4. Toolchain on the host mount: npm (run via the guest's baked-in node;
    // apk's npm is incompatible with it) then the pinned pi, globally
    // installed under /agent/toolchain/global.
    if !paths.guest.join("toolchain/npm/bin/npm-cli.js").exists() {
        info!(agent = %record.name, "installing npm toolchain");
        driver
            .sh(
                "mkdir -p /agent/toolchain && cd /tmp && \
                 wget -q https://registry.npmjs.org/npm/-/npm-11.11.0.tgz && \
                 tar xzf npm-11.11.0.tgz && mv package /agent/toolchain/npm && \
                 rm npm-11.11.0.tgz",
                &[],
            )
            .await
            .context("installing npm")?;
    }
    let version_marker = paths.guest.join("toolchain/pi-version");
    let want_version = manifest.harness.version.clone();
    let have_version = std::fs::read_to_string(&version_marker).unwrap_or_default();
    if have_version.trim() != want_version {
        let pi_pkg = format!(
            "@earendil-works/pi-coding-agent@{}",
            manifest.harness.version
        );
        info!(agent = %record.name, pkg = %pi_pkg, "installing pi in guest");
        driver
            .sh(
                &format!(
                    "node /agent/toolchain/npm/bin/npm-cli.js install -g \
                     --prefix /agent/toolchain/global '{pi_pkg}'"
                ),
                &[],
            )
            .await
            .context("installing pi")?;
        std::fs::write(&version_marker, &want_version)?;
    }

    // 5. Fresh repo clones into the workspace.
    for repo in &manifest.repos {
        let name = repo
            .url
            .rsplit('/')
            .next()
            .unwrap_or("repo")
            .trim_end_matches(".git");
        let dest = format!("/agent/workspace/{name}");
        info!(agent = %record.name, url = %repo.url, "cloning repo");
        // Try a shallow branch clone first; fall back to full clone for SHA refs.
        let shallow = driver
            .sh(
                &format!(
                    "git clone --quiet --depth 1 --branch '{}' '{}' '{dest}'",
                    repo.ref_, repo.url
                ),
                &[],
            )
            .await;
        if shallow.is_err() {
            driver
                .sh(&format!("git clone --quiet '{}' '{dest}'", repo.url), &[])
                .await
                .with_context(|| format!("cloning {}", repo.url))?;
            driver
                .sh(
                    &format!("git -C '{dest}' checkout --quiet '{}'", repo.ref_),
                    &[],
                )
                .await?;
        }
    }

    // 6. Extension repos → the agent's isolated pi-home extensions dir.
    for ext in &manifest.extensions {
        let name = ext
            .url
            .rsplit('/')
            .next()
            .unwrap_or("ext")
            .trim_end_matches(".git");
        let dest = format!("/agent/pi-home/extensions/{name}");
        info!(agent = %record.name, url = %ext.url, "cloning extension");
        driver
            .sh(
                &format!(
                    "git clone --quiet '{}' '{dest}' && git -C '{dest}' checkout --quiet '{}'",
                    ext.url, ext.ref_
                ),
                &[],
            )
            .await
            .with_context(|| format!("cloning extension {}", ext.url))?;
    }

    // 7. Toolchain via mise (only if the manifest declares tools). mise
    // also lives on the host mount.
    if !manifest.toolchain.tools.is_empty() {
        info!(agent = %record.name, "installing toolchain via mise");
        let tools_table = manifest
            .toolchain
            .tools
            .iter()
            .map(|(k, v)| format!("{k} = \"{v}\""))
            .collect::<Vec<_>>()
            .join("\n");
        driver
            .sh(
                &format!("printf '[tools]\\n{tools_table}\\n' > /agent/workspace/mise.toml"),
                &[],
            )
            .await?;
        driver
            .sh(
                "curl -fsSL https://mise.run | MISE_INSTALL_PATH=/agent/toolchain/mise sh >/dev/null 2>&1",
                &[],
            )
            .await
            .context("installing mise in guest")?;
        driver
            .sh(
                "cd /agent/workspace && MISE_DATA_DIR=/agent/toolchain/mise-data /agent/toolchain/mise install --yes",
                &[],
            )
            .await
            .context("mise install")?;
    }

    // 8. Isolated pi-home: trust the workspace, nothing else global (Q8).
    let trust = r#"{"/agent/workspace": true}"#;
    driver
        .sh(
            &format!(
                "mkdir -p /agent/pi-home && printf '%s' '{trust}' > /agent/pi-home/trust.json"
            ),
            &[],
        )
        .await?;

    info!(agent = %record.name, "provisioning complete");
    Ok(())
}

/// The env + argv pieces needed to spawn pi for this agent.
pub fn pi_spawn_env(record: &AgentRecord) -> Vec<(String, String)> {
    let mut env = agent_env(record);
    env.push(("PI_CODING_AGENT_DIR".into(), "/agent/pi-home".into()));
    env.push(("PI_SKIP_VERSION_CHECK".into(), "1".into()));
    env.push(("PI_OFFLINE".into(), "0".into()));
    // pi and its toolchain live on the host-mounted volume.
    env.push((
        "PATH".into(),
        "/agent/toolchain/global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            .into(),
    ));
    env
}

pub fn validate_manifest(m: &AgentManifest) -> Result<()> {
    if m.name.trim().is_empty() {
        bail!("manifest: name is required");
    }
    if m.harness.kind != "pi" {
        bail!("manifest: only harness type \"pi\" is supported in v1");
    }
    if !m.secrets.providers.contains(&m.model.provider) {
        warn!(
            provider = %m.model.provider,
            "model provider not in secrets.providers — agent may fail to authenticate"
        );
    }
    Ok(())
}
