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

/// The env passed to the pi process: Gondolin *placeholder* values for each
/// secret (the real values never enter the guest), plus pi/toolchain config.
pub fn pi_spawn_env(
    record: &AgentRecord,
    placeholders: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = placeholders
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("PI_CODING_AGENT_DIR".into(), "/agent/pi-home".into()));
    env.push(("PI_SKIP_VERSION_CHECK".into(), "1".into()));
    // pi and its toolchain live on the host-mounted volume.
    env.push((
        "PATH".into(),
        "/agent/toolchain/global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
            .into(),
    ));
    if let Some(otel) = &record.manifest.observability.otel {
        env.push(("OTEL_EXPORTER_OTLP_ENDPOINT".into(), otel.endpoint.clone()));
        if !otel.headers.is_empty() {
            let headers = otel
                .headers
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            env.push(("OTEL_EXPORTER_OTLP_HEADERS".into(), headers));
        }
        env.push(("OTEL_SERVICE_NAME".into(), record.name.clone()));
    }
    env
}

/// Egress allowlist for the VM: provisioning hosts, each secret's allowed
/// hosts, git hosts from repo URLs, the OTEL endpoint, and manifest extras.
pub fn egress_hosts(
    record: &AgentRecord,
    bundle: &suzerain_protocol::secrets::SecretBundle,
) -> Vec<String> {
    let mut hosts: Vec<String> = [
        "dl-cdn.alpinelinux.org", // apk
        "registry.npmjs.org",     // npm tarballs
        "mise.run",               // toolchain installer
        "github.com",
        "objects.githubusercontent.com", // release downloads
        "nodejs.org",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for entry in bundle.env.values() {
        hosts.extend(entry.hosts.iter().cloned());
    }
    for repo in &record.manifest.repos {
        if let Some(host) = repo_host(&repo.url) {
            hosts.push(host);
        }
    }
    if let Some(otel) = &record.manifest.observability.otel {
        if let Some(host) = url_host(&otel.endpoint) {
            hosts.push(host);
        }
    }
    hosts.extend(record.manifest.egress.allow.iter().cloned());
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Git hosts for SSH egress (proxied, host-side key).
pub fn git_hosts(record: &AgentRecord) -> Vec<String> {
    let mut hosts: Vec<String> = record
        .manifest
        .repos
        .iter()
        .filter_map(|r| repo_host(&r.url))
        .collect();
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Extract a host from a git URL (ssh or https).
fn repo_host(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("git@") {
        return rest.split(':').next().map(str::to_string);
    }
    url_host(url)
}

fn url_host(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    let host = after_scheme.split(['/', ':']).next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Standalone fallback: build a bundle from the daemon's own environment
/// (used by the local CLI create path when no control plane is involved).
pub fn bundle_from_env(manifest: &AgentManifest) -> suzerain_protocol::secrets::SecretBundle {
    let mut bundle = suzerain_protocol::secrets::SecretBundle::default();
    for provider in &manifest.secrets.providers {
        if let Some((var, host)) = suzerain_protocol::secrets::provider_env_and_host(provider) {
            if let Ok(value) = std::env::var(var) {
                if !value.is_empty() {
                    bundle.env.insert(
                        var.to_string(),
                        suzerain_protocol::secrets::SecretEntry {
                            value,
                            hosts: vec![host.to_string()],
                        },
                    );
                }
            }
        }
    }
    bundle
}

/// Full provisioning of a fresh agent. Idempotent-ish: safe to re-run after
/// partial failure (steps that already completed are skipped or cheap).
/// Returns the placeholder env map for the agent's secrets.
pub async fn provision(
    driver: &DriverClient,
    record: &AgentRecord,
    bundle: &suzerain_protocol::secrets::SecretBundle,
) -> Result<BTreeMap<String, String>> {
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
    let placeholders = driver
        .boot(
            &[("/agent".into(), paths.guest.to_string_lossy().into())],
            &[],
            &format!("castellan-{}", record.name),
            None,
            bundle,
            &egress_hosts(record, bundle),
            &git_hosts(record),
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
    Ok(placeholders)
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
