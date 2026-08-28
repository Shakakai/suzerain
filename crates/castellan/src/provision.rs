//! Agent provisioning: boot the Gondolin VM, install base tooling, clone
//! repos and extension repos, set up the isolated pi-home, install the pinned
//! pi version, and apply the toolchain.
//!
//! Everything persistent lives under the host agent dir (`AgentPaths`),
//! mounted into the guest at `/agent` — the VM itself is disposable.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use suzerain_protocol::manifest::{AgentManifest, InstallEntry, RunWhen};

use crate::driver::DriverClient;
use crate::state::{AgentPaths, AgentRecord};

/// Single-quote `s` for safe embedding in a `sh -lc` script string.
///
/// `DriverClient::sh` runs its script through a shell, so any manifest-
/// derived value (repo URLs/refs, extension sources, tool versions, package
/// names, ...) that gets interpolated into one of these scripts must be
/// quoted through this first. Naively wrapping a value in `'...'` is *not*
/// enough: a value containing its own `'` breaks out of the quoting and lets
/// the rest of the string run as shell syntax. This escapes embedded single
/// quotes the standard POSIX way: close the quote, emit an escaped `'`,
/// reopen the quote.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

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
    for ext in &record.manifest.extensions {
        if let Some(host) = extension_host(ext) {
            hosts.push(host);
        }
    }
    hosts.extend(record.manifest.egress.allow.iter().cloned());
    hosts.sort();
    hosts.dedup();
    hosts
}

/// The host a `source`-form extension install needs to reach: the npm
/// registry for `npm:` sources, the git host for `git:`/URL sources.
fn extension_host(ext: &suzerain_protocol::manifest::Extension) -> Option<String> {
    let source = ext.source.as_deref()?;
    if source.starts_with("npm:") {
        return Some("registry.npmjs.org".to_string());
    }
    let rest = source.strip_prefix("git:").unwrap_or(source);
    // git:git@host:user/repo (ssh) or git:host/user/repo / git:https://…
    if let Some(ssh) = rest.strip_prefix("git@") {
        return ssh.split(':').next().map(str::to_string);
    }
    if rest.contains("://") {
        return url_host(rest);
    }
    rest.split('/')
        .next()
        .filter(|h| h.contains('.'))
        .map(str::to_string)
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

/// Env for running pi itself in the guest (package installs): per-agent
/// pi-home + the toolchain on PATH (pi's shebang needs `node`, the package
/// manager shells out to the npm shim).
fn pi_tool_env() -> Vec<(String, String)> {
    vec![
        ("PI_CODING_AGENT_DIR".into(), "/agent/pi-home".into()),
        ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
        (
            "PATH".into(),
            "/agent/toolchain/global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                .into(),
        ),
    ]
}

/// Configure git/ssh in the guest for an agent whose bundle carries the git
/// SSH key. Runs on every fresh boot before any git operation: the guest
/// rootfs is disposable — only `/agent` persists across suspend/restore.
///
/// **The private key never enters the guest.** Guest ssh/git traffic to the
/// manifest's git hosts is transparently proxied by gondolin's host-side ssh
/// proxy, which performs the upstream authentication with the key held on
/// the host (the proxy accepts any guest-side auth). All the guest needs is
/// relaxed host-key checking: its `known_hosts` is empty on every fresh
/// boot, and upstream host keys are verified host-side by the proxy against
/// the host's known_hosts, so this does not weaken MITM protection.
pub async fn configure_git_ssh(
    driver: &DriverClient,
    bundle: &suzerain_protocol::secrets::SecretBundle,
) -> Result<()> {
    if bundle.git_ssh_key.is_none() {
        return Ok(());
    }
    driver
        .sh(
            "mkdir -p /root/.ssh && chmod 700 /root/.ssh && \
             printf 'Host *\\n  StrictHostKeyChecking accept-new\\n' \
               > /root/.ssh/config && chmod 600 /root/.ssh/config && \
             if command -v git >/dev/null 2>&1; then \
               git config --global core.sshCommand \
                 'ssh -o StrictHostKeyChecking=accept-new'; \
             fi",
            &[],
        )
        .await
        .context("configuring git ssh in guest")?;
    Ok(())
}

/// Env for git clone commands in the guest: the guest's own known_hosts is
/// empty on first contact, so auto-accept the (host-side-verified) host key.
/// Upstream host keys are verified host-side by gondolin's ssh proxy against
/// the host's known_hosts, so this does not weaken MITM protection.
fn clone_env() -> Vec<(String, String)> {
    vec![(
        "GIT_SSH_COMMAND".into(),
        "ssh -o StrictHostKeyChecking=accept-new".into(),
    )]
}

/// Bootstrap a real npm (via the guest's baked-in node; apk's npm is
/// incompatible with it) onto the host mount at `/agent/toolchain/npm`.
/// Idempotent: skipped if the marker file already exists. Shared by the
/// hardcoded pi provisioning path and the declarative `npm` install
/// resolver — "how to get npm in a bare Alpine guest" isn't harness- or
/// resolver-specific.
async fn ensure_npm_toolchain(driver: &DriverClient, paths: &AgentPaths) -> Result<()> {
    if paths.guest.join("toolchain/npm/bin/npm-cli.js").exists() {
        return Ok(());
    }
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
    Ok(())
}

/// Write an npm shim at `<prefix>/bin/npm` pointing at the tarball npm
/// `ensure_npm_toolchain` installs — needed because apk's own npm is
/// incompatible with the guest's baked-in node. `prefix` must be under
/// `/agent` (anything else is guest-ephemeral and would vanish on restart).
async fn write_npm_shim(driver: &DriverClient, paths: &AgentPaths, prefix: &str) -> Result<()> {
    let rel = prefix
        .strip_prefix("/agent/")
        .with_context(|| format!("npm install prefix {prefix:?} must be under /agent/"))?;
    let bin_dir = paths.guest.join(rel).join("bin");
    std::fs::create_dir_all(&bin_dir)?;
    let shim = "#!/bin/sh\nexec node /agent/toolchain/npm/bin/npm-cli.js \"$@\"\n";
    std::fs::write(bin_dir.join("npm"), shim)?;
    driver
        .sh(&format!("chmod +x {}/bin/npm", shell_quote(prefix)), &[])
        .await?;
    Ok(())
}

/// Full provisioning of a fresh agent. Idempotent-ish: safe to re-run after
/// partial failure (steps that already completed are skipped or cheap).
/// Returns the placeholder env map for the agent's secrets.
///
/// Resolves `AgentPaths` itself (via `AgentPaths::for_agent`, which reads
/// `$CASTELLAN_HOME`/`$SUZERAIN_HOME`) for today's one caller
/// (`supervisor.rs`) — kept unchanged so nothing here needs to migrate. See
/// [`provision_with_paths`] and the [`Provisioner`] trait below for the
/// version that takes `AgentPaths` explicitly instead.
pub async fn provision(
    driver: &DriverClient,
    record: &AgentRecord,
    bundle: &suzerain_protocol::secrets::SecretBundle,
) -> Result<BTreeMap<String, String>> {
    let paths = AgentPaths::for_agent(&record.id);
    provision_with_paths(driver, record, &paths, bundle).await
}

/// Same as [`provision`], but with the host-side paths passed in explicitly
/// instead of resolved internally from env vars — per
/// docs/UNIFIED-AGENT-API-DESIGN.md §4.8.1, this is what makes a
/// [`Provisioner`] impl free of hidden global state: the trait method takes
/// `paths` as an argument, so every implementation (this one, and any future
/// `DeclarativeProvisioner`) gets it from its caller rather than reaching
/// into `$CASTELLAN_HOME`/`$SUZERAIN_HOME` on its own.
pub async fn provision_with_paths(
    driver: &DriverClient,
    record: &AgentRecord,
    paths: &AgentPaths,
    bundle: &suzerain_protocol::secrets::SecretBundle,
) -> Result<BTreeMap<String, String>> {
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
            &record.manifest.resources,
        )
        .await?;

    // 3. Base packages (small; the guest rootfs is only ~260MB — everything
    // big installs onto the host-mounted /agent volume instead).
    info!(agent = %record.name, "installing base packages in guest");
    driver
        .sh("apk add --no-cache git curl bash ca-certificates", &[])
        .await
        .context("installing base packages")?;

    // 3b. Point guest git/ssh at the host-side ssh proxy (which holds the
    // real key) — before any clone runs. The private key never enters the
    // guest.
    if bundle.git_ssh_key.is_some() {
        configure_git_ssh(driver, bundle).await?;
    }

    // 4. Toolchain on the host mount: npm (run via the guest's baked-in node;
    // apk's npm is incompatible with it) then the pinned pi, globally
    // installed under /agent/toolchain/global.
    ensure_npm_toolchain(driver, paths).await?;
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
                     --prefix /agent/toolchain/global {}",
                    shell_quote(&pi_pkg)
                ),
                &[],
            )
            .await
            .context("installing pi")?;
        std::fs::write(&version_marker, &want_version)?;
    }

    // 5b. npm shim so pi's package manager (`pi install`, which shells out
    // to `npm` for git packages) works in the guest: apk's npm is
    // incompatible with the baked-in node, so point a shim at the tarball
    // npm installed above.
    write_npm_shim(driver, paths, "/agent/toolchain/global").await?;

    // 6. Fresh repo clones into the workspace.
    for repo in &manifest.repos {
        let name = repo
            .url
            .rsplit('/')
            .next()
            .unwrap_or("repo")
            .trim_end_matches(".git");
        let dest = format!("/agent/workspace/{name}");
        let (q_ref, q_url, q_dest) = (
            shell_quote(&repo.ref_),
            shell_quote(&repo.url),
            shell_quote(&dest),
        );
        info!(agent = %record.name, url = %repo.url, "cloning repo");
        // Try a shallow branch clone first; fall back to full clone for SHA refs.
        let shallow = driver
            .sh(
                &format!("git clone --quiet --depth 1 --branch {q_ref} {q_url} {q_dest}"),
                &clone_env(),
            )
            .await;
        if shallow.is_err() {
            driver
                .sh(&format!("git clone --quiet {q_url} {q_dest}"), &clone_env())
                .await
                .with_context(|| format!("cloning {}", repo.url))?;
            driver
                .sh(&format!("git -C {q_dest} checkout --quiet {q_ref}"), &[])
                .await?;
        }
    }

    // 6b. git trusts the host-mounted workspace (host uid ≠ guest root).
    driver
        .sh("git config --global --add safe.directory '*'", &[])
        .await?;

    // 7. Extensions: `source` form installs via pi's package manager into
    // the agent's isolated pi-home (persists on the host mount); `url` form
    // clones the repo into the pi-home extensions dir.
    for ext in &manifest.extensions {
        if let Some(source) = &ext.source {
            info!(agent = %record.name, source = %source, "installing pi package");
            driver
                .sh(
                    &format!(
                        "/agent/toolchain/global/bin/pi install {}",
                        shell_quote(source)
                    ),
                    &pi_tool_env(),
                )
                .await
                .with_context(|| format!("installing pi package {source}"))?;
            continue;
        }
        let url = ext.url.as_deref().unwrap_or_default();
        let ref_ = ext.ref_.as_deref().unwrap_or("main");
        let name = url
            .rsplit('/')
            .next()
            .unwrap_or("ext")
            .trim_end_matches(".git");
        let dest = format!("/agent/pi-home/extensions/{name}");
        let (q_url, q_ref, q_dest) = (shell_quote(url), shell_quote(ref_), shell_quote(&dest));
        info!(agent = %record.name, url = %url, "cloning extension");
        driver
            .sh(
                &format!("git clone --quiet {q_url} {q_dest} && git -C {q_dest} checkout --quiet {q_ref}"),
                &clone_env(),
            )
            .await
            .with_context(|| format!("cloning extension {url}"))?;
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
        let mise_toml = format!("[tools]\n{tools_table}\n");
        driver
            .sh(
                &format!(
                    "printf '%s' {} > /agent/workspace/mise.toml",
                    shell_quote(&mise_toml)
                ),
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

    // 9. Isolated pi-home: trust the workspace, nothing else global (Q8).
    let trust = r#"{"/agent/workspace": true}"#;
    driver
        .sh(
            &format!(
                "mkdir -p /agent/pi-home && printf '%s' '{trust}' > /agent/pi-home/trust.json"
            ),
            &[],
        )
        .await?;

    // 10. Prompt customization: pi appends $PI_CODING_AGENT_DIR/APPEND_SYSTEM.md
    // to the system prompt on every run. Written host-side (pi-home lives on
    // the host mount) to avoid shell-escaping arbitrary prompt text.
    if let Some(append) = &manifest.prompt.append_system {
        if !append.trim().is_empty() {
            info!(agent = %record.name, "writing APPEND_SYSTEM.md");
            tokio::fs::write(paths.pi_home.join("APPEND_SYSTEM.md"), append).await?;
        }
    }

    info!(agent = %record.name, "provisioning complete");
    Ok(placeholders)
}

/// Length ceiling for manifest fields that become a single shell argument or
/// TOML scalar (URLs, refs, package names, hosts, ...).
const MAX_FIELD_LEN: usize = 4096;
/// Length ceiling for manifest fields that are themselves free-form text
/// blobs (provision scripts, the system-prompt append).
const MAX_TEXT_LEN: usize = 256 * 1024;

/// Bound the size and character set of a manifest field that ends up
/// interpolated into a guest shell command (or shell-adjacent config file
/// content built the same way, e.g. `mise.toml`).
///
/// `shell_quote` already neutralizes shell metacharacters, but not what
/// this checks: an unbounded value can blow up a single guest shell
/// invocation (or the JSONL message carrying it to the driver), and a NUL
/// byte truncates the argv the OS actually execs regardless of how
/// carefully the rest was quoted — silently dropping everything the
/// manifest author wrote after it. Embedded newlines/tabs are allowed:
/// `shell_quote`'s own tests cover them round-tripping safely through a
/// real shell, and multi-line values (scripts, prompts) are legitimate.
fn check_shell_field(what: &str, value: &str, max_len: usize) -> Result<()> {
    if value.len() > max_len {
        bail!(
            "manifest: {what} is {} bytes, exceeds the {max_len}-byte limit",
            value.len()
        );
    }
    if value.as_bytes().contains(&0) {
        bail!("manifest: {what} contains a NUL byte");
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
    {
        bail!("manifest: {what} contains a disallowed control character");
    }
    Ok(())
}

/// Same as [`check_shell_field`], plus a `"` ban: `name`/`version` land in
/// generated `mise.toml` content as `{name} = "{version}"` — unescaped, so
/// an embedded quote breaks out of the TOML string and lets the value
/// inject arbitrary extra TOML.
fn check_toml_scalar(what: &str, value: &str, max_len: usize) -> Result<()> {
    check_shell_field(what, value, max_len)?;
    if value.contains('"') {
        bail!("manifest: {what} must not contain a double quote");
    }
    Ok(())
}

pub fn validate_manifest(m: &AgentManifest) -> Result<()> {
    if m.name.trim().is_empty() {
        bail!("manifest: name is required");
    }
    if m.harness.kind != "pi" {
        bail!("manifest: only harness type \"pi\" is supported in v1");
    }
    for (i, repo) in m.repos.iter().enumerate() {
        check_shell_field(&format!("repos[{i}].url"), &repo.url, MAX_FIELD_LEN)?;
        check_shell_field(&format!("repos[{i}].ref"), &repo.ref_, MAX_FIELD_LEN)?;
    }
    for (i, ext) in m.extensions.iter().enumerate() {
        match (&ext.source, &ext.url) {
            (Some(source), None) => {
                let ok = source.starts_with("npm:")
                    || source.starts_with("git:")
                    || source.starts_with("https://")
                    || source.starts_with("ssh://")
                    || source.starts_with("git@");
                if !ok {
                    bail!(
                        "manifest: extensions[{i}].source '{source}' is not a pi install \
                         source (npm:<pkg>, git:<repo>, or a git URL)"
                    );
                }
                check_shell_field(&format!("extensions[{i}].source"), source, MAX_FIELD_LEN)?;
            }
            (None, Some(url)) => {
                check_shell_field(&format!("extensions[{i}].url"), url, MAX_FIELD_LEN)?;
            }
            (Some(_), Some(_)) => {
                bail!("manifest: extensions[{i}] sets both source and url — pick one")
            }
            (None, None) => {
                bail!("manifest: extensions[{i}] needs source or url")
            }
        }
        if ext.url.is_some() && ext.ref_.is_none() {
            bail!("manifest: extensions[{i}].ref is required with url");
        }
        if let Some(ref_) = &ext.ref_ {
            check_shell_field(&format!("extensions[{i}].ref"), ref_, MAX_FIELD_LEN)?;
        }
    }
    for (name, version) in &m.toolchain.tools {
        check_toml_scalar(&format!("toolchain.tools[{name}] name"), name, 256)?;
        check_toml_scalar(&format!("toolchain.tools[{name}] version"), version, 256)?;
    }
    if let Some(append) = &m.prompt.append_system {
        check_shell_field("prompt.append_system", append, MAX_TEXT_LEN)?;
    }
    if !m.secrets.providers.contains(&m.model.provider) {
        warn!(
            provider = %m.model.provider,
            "model provider not in secrets.providers — agent may fail to authenticate"
        );
    }
    if let Some(spec) = &m.provision {
        for (i, pkg) in spec.packages.iter().enumerate() {
            check_shell_field(&format!("provision.packages[{i}]"), pkg, MAX_FIELD_LEN)?;
        }
        for (i, mount) in spec.mounts.iter().enumerate() {
            if std::path::Path::new(&mount.host).is_absolute()
                || mount
                    .host
                    .split(['/', '\\'])
                    .any(|c| c == ".." || c.is_empty())
            {
                bail!(
                    "manifest: provision.mounts[{i}].host '{}' must be a relative path \
                     within the agent's root dir, with no '..' components",
                    mount.host
                );
            }
            check_shell_field(
                &format!("provision.mounts[{i}].host"),
                &mount.host,
                MAX_FIELD_LEN,
            )?;
            check_shell_field(
                &format!("provision.mounts[{i}].guest"),
                &mount.guest,
                MAX_FIELD_LEN,
            )?;
        }
        for (i, entry) in spec.install.iter().enumerate() {
            match entry {
                InstallEntry::Npm {
                    package,
                    version,
                    prefix,
                } => {
                    check_shell_field(
                        &format!("provision.install[{i}].package"),
                        package,
                        MAX_FIELD_LEN,
                    )?;
                    if let Some(v) = version {
                        check_shell_field(
                            &format!("provision.install[{i}].version"),
                            v,
                            MAX_FIELD_LEN,
                        )?;
                    }
                    if let Some(p) = prefix {
                        check_shell_field(
                            &format!("provision.install[{i}].prefix"),
                            p,
                            MAX_FIELD_LEN,
                        )?;
                    }
                }
                InstallEntry::Git { url, ref_, dest } => {
                    check_shell_field(&format!("provision.install[{i}].url"), url, MAX_FIELD_LEN)?;
                    check_shell_field(&format!("provision.install[{i}].ref"), ref_, MAX_FIELD_LEN)?;
                    check_shell_field(
                        &format!("provision.install[{i}].dest"),
                        dest,
                        MAX_FIELD_LEN,
                    )?;
                }
                InstallEntry::Mise { tools } => {
                    for (name, version) in tools {
                        check_toml_scalar(
                            &format!("provision.install[{i}].tools[{name}] name"),
                            name,
                            256,
                        )?;
                        check_toml_scalar(
                            &format!("provision.install[{i}].tools[{name}] version"),
                            version,
                            256,
                        )?;
                    }
                }
            }
        }
        for (i, entry) in spec.run.iter().enumerate() {
            if entry.when != RunWhen::PreStart {
                bail!(
                    "manifest: provision.run[{i}].when = post_start is not yet supported \
                     (only pre_start)"
                );
            }
            check_shell_field(
                &format!("provision.run[{i}].script"),
                &entry.script,
                MAX_TEXT_LEN,
            )?;
            for (k, v) in &entry.env {
                check_shell_field(&format!("provision.run[{i}].env[{k}] key"), k, 256)?;
                check_shell_field(
                    &format!("provision.run[{i}].env[{k}] value"),
                    v,
                    MAX_FIELD_LEN,
                )?;
            }
        }
        for (i, path) in spec.trust.paths.iter().enumerate() {
            check_shell_field(&format!("provision.trust.paths[{i}]"), path, MAX_FIELD_LEN)?;
        }
        if let Some(append) = &spec.prompt.append_system {
            check_shell_field("provision.prompt.append_system", append, MAX_TEXT_LEN)?;
        }
    }
    Ok(())
}

/// Pluggable VM/agent bootstrap (docs/UNIFIED-AGENT-API-DESIGN.md §4.8.1).
/// `paths` is an explicit argument rather than something an implementation
/// resolves for itself, so every impl — this one included — is free of the
/// hidden `$CASTELLAN_HOME`/`$SUZERAIN_HOME` env-var lookup `AgentPaths`
/// otherwise buries inside `provision()`.
#[async_trait::async_trait]
pub trait Provisioner: Send + Sync {
    async fn provision(
        &self,
        driver: &DriverClient,
        record: &AgentRecord,
        paths: &AgentPaths,
        bundle: &suzerain_protocol::secrets::SecretBundle,
    ) -> Result<BTreeMap<String, String>>;
}

/// Today's (and so far only) implementation: the hardcoded Alpine/npm/mise/
/// pi imperative sequence above, moved behind the trait unchanged. A
/// `DeclarativeProvisioner` reading a manifest's `[provision]` section
/// (§4.8.2) would be a second implementation of this same trait.
pub struct PiProvisioner;

#[async_trait::async_trait]
impl Provisioner for PiProvisioner {
    async fn provision(
        &self,
        driver: &DriverClient,
        record: &AgentRecord,
        paths: &AgentPaths,
        bundle: &suzerain_protocol::secrets::SecretBundle,
    ) -> Result<BTreeMap<String, String>> {
        provision_with_paths(driver, record, paths, bundle).await
    }
}

/// Reads `manifest.provision` (§4.8.2) and executes it: packages → mounts
/// (folded into the boot call, since mounts must exist at boot time) →
/// typed installs, in listed order → run scripts (`pre_start` only — see
/// `RunWhen::PostStart`'s doc comment) → trust → prompt. Harness-neutral by
/// construction: nothing here is pi-specific, unlike `PiProvisioner`'s
/// hardcoded sequence.
pub struct DeclarativeProvisioner;

#[async_trait::async_trait]
impl Provisioner for DeclarativeProvisioner {
    async fn provision(
        &self,
        driver: &DriverClient,
        record: &AgentRecord,
        paths: &AgentPaths,
        bundle: &suzerain_protocol::secrets::SecretBundle,
    ) -> Result<BTreeMap<String, String>> {
        let manifest = &record.manifest;
        let spec = manifest
            .provision
            .as_ref()
            .context("DeclarativeProvisioner requires manifest.provision")?;

        // 1. Host-side layout (same as PiProvisioner).
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

        // 2. Boot: base /agent mount plus any [[provision.mounts]] — these
        // must exist at boot time, so they can't be deferred to a later
        // step the way OS-package/install steps can.
        let mut mounts: Vec<(String, String)> =
            vec![("/agent".into(), paths.guest.to_string_lossy().into())];
        for m in &spec.mounts {
            mounts.push((
                m.guest.clone(),
                paths.root.join(&m.host).to_string_lossy().into(),
            ));
        }
        info!(agent = %record.name, "booting VM (declarative provisioner)");
        let placeholders = driver
            .boot(
                &mounts,
                &[],
                &format!("castellan-{}", record.name),
                None,
                bundle,
                &egress_hosts(record, bundle),
                &git_hosts(record),
                &manifest.resources,
            )
            .await?;

        // 3. OS packages, before anything else that might need them.
        if !spec.packages.is_empty() {
            let pkgs = spec.packages.join(" ");
            let q_pkgs = spec
                .packages
                .iter()
                .map(|p| shell_quote(p))
                .collect::<Vec<_>>()
                .join(" ");
            info!(agent = %record.name, packages = %pkgs, "installing packages (declarative)");
            driver
                .sh(&format!("apk add --no-cache {q_pkgs}"), &[])
                .await
                .context("installing packages")?;
        }

        // 3b. Point guest git/ssh at the host-side ssh proxy before any
        // clone runs — same as the hardcoded path.
        if bundle.git_ssh_key.is_some() {
            configure_git_ssh(driver, bundle).await?;
        }

        // 4. Typed installs, in listed order.
        for entry in &spec.install {
            match entry {
                InstallEntry::Npm {
                    package,
                    version,
                    prefix,
                } => {
                    ensure_npm_toolchain(driver, paths).await?;
                    let prefix = prefix.as_deref().unwrap_or("/agent/toolchain/global");
                    write_npm_shim(driver, paths, prefix).await?;
                    let pkg_spec = match version {
                        Some(v) => format!("{package}@{v}"),
                        None => package.clone(),
                    };
                    info!(agent = %record.name, pkg = %pkg_spec, "installing npm package (declarative)");
                    driver
                        .sh(
                            &format!(
                                "node /agent/toolchain/npm/bin/npm-cli.js install -g \
                                 --prefix {} {}",
                                shell_quote(prefix),
                                shell_quote(&pkg_spec)
                            ),
                            &[],
                        )
                        .await
                        .with_context(|| format!("installing npm package {pkg_spec}"))?;
                }
                InstallEntry::Git { url, ref_, dest } => {
                    let (q_url, q_ref, q_dest) =
                        (shell_quote(url), shell_quote(ref_), shell_quote(dest));
                    info!(agent = %record.name, url = %url, dest = %dest, "cloning repo (declarative)");
                    let shallow = driver
                        .sh(
                            &format!(
                                "git clone --quiet --depth 1 --branch {q_ref} {q_url} {q_dest}"
                            ),
                            &clone_env(),
                        )
                        .await;
                    if shallow.is_err() {
                        driver
                            .sh(&format!("git clone --quiet {q_url} {q_dest}"), &clone_env())
                            .await
                            .with_context(|| format!("cloning {url}"))?;
                        driver
                            .sh(&format!("git -C {q_dest} checkout --quiet {q_ref}"), &[])
                            .await?;
                    }
                }
                InstallEntry::Mise { tools } => {
                    info!(agent = %record.name, "installing toolchain via mise (declarative)");
                    let tools_table = tools
                        .iter()
                        .map(|(k, v)| format!("{k} = \"{v}\""))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mise_toml = format!("[tools]\n{tools_table}\n");
                    driver
                        .sh(
                            &format!(
                                "printf '%s' {} > /agent/workspace/mise.toml",
                                shell_quote(&mise_toml)
                            ),
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
            }
        }

        // 4b. git trusts the host-mounted workspace (host uid != guest
        // root) — harmless if no clone happened.
        driver
            .sh("git config --global --add safe.directory '*'", &[])
            .await?;

        // 5. Run scripts — pre_start only; `validate_manifest` rejects
        // `post_start` entries before this ever runs.
        for entry in &spec.run {
            if entry.when != RunWhen::PreStart {
                bail!("provision.run: post_start is not yet supported");
            }
            info!(agent = %record.name, "running provision script (declarative)");
            let env: Vec<(String, String)> = entry
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            driver
                .sh(&entry.script, &env)
                .await
                .context("running provision.run script")?;
        }

        // 6. Isolated pi-home trust — default to just the workspace when
        // [provision.trust] is omitted, matching the hardcoded path.
        let trust_paths: Vec<String> = if spec.trust.paths.is_empty() {
            vec!["/agent/workspace".to_string()]
        } else {
            spec.trust.paths.clone()
        };
        let trust_obj: BTreeMap<&str, bool> =
            trust_paths.iter().map(|p| (p.as_str(), true)).collect();
        let trust_json = serde_json::to_string(&trust_obj)?;
        driver
            .sh(
                &format!(
                    "mkdir -p /agent/pi-home && printf '%s' {} > /agent/pi-home/trust.json",
                    shell_quote(&trust_json)
                ),
                &[],
            )
            .await?;

        // 7. Prompt customization.
        if let Some(append) = &spec.prompt.append_system {
            if !append.trim().is_empty() {
                tokio::fs::write(paths.pi_home.join("APPEND_SYSTEM.md"), append).await?;
            }
        }

        info!(agent = %record.name, "declarative provisioning complete");
        Ok(placeholders)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uuid::Uuid;

    fn minimal_manifest() -> suzerain_protocol::manifest::AgentManifest {
        toml::from_str(
            r#"
name = "agent-1"
harness = { type = "pi", version = "0.84.1" }
model = { provider = "anthropic", id = "claude-sonnet-4-5" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn check_shell_field_rejects_oversized_values() {
        let huge = "a".repeat(100);
        assert!(check_shell_field("test", &huge, 10).is_err());
        assert!(check_shell_field("test", "short", 10).is_ok());
    }

    #[test]
    fn check_shell_field_rejects_nul_bytes() {
        let value = "before\0after";
        let err = check_shell_field("test", value, 4096).unwrap_err();
        assert!(err.to_string().contains("NUL"), "{err}");
    }

    #[test]
    fn check_shell_field_rejects_control_characters_but_allows_newlines() {
        assert!(check_shell_field("test", "line one\nline two\ttabbed", 4096).is_ok());
        assert!(check_shell_field("test", "bell\x07here", 4096).is_err());
    }

    #[test]
    fn check_toml_scalar_rejects_embedded_quote() {
        // Would otherwise break out of the generated `name = "value"` line
        // in mise.toml.
        assert!(check_toml_scalar("test", "1.0\"\nevil = \"x", 4096).is_err());
        assert!(check_toml_scalar("test", "1.0", 4096).is_ok());
    }

    #[test]
    fn validate_manifest_rejects_oversized_repo_url() {
        let mut m = minimal_manifest();
        m.repos.push(suzerain_protocol::manifest::Repo {
            url: "a".repeat(MAX_FIELD_LEN + 1),
            ref_: "main".to_string(),
        });
        let err = validate_manifest(&m).unwrap_err();
        assert!(err.to_string().contains("repos[0].url"), "{err}");
    }

    #[test]
    fn validate_manifest_rejects_nul_in_repo_ref() {
        let mut m = minimal_manifest();
        m.repos.push(suzerain_protocol::manifest::Repo {
            url: "https://github.com/org/repo.git".to_string(),
            ref_: "main\0evil".to_string(),
        });
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn validate_manifest_rejects_quote_in_toolchain_tool_version() {
        let mut m = minimal_manifest();
        m.toolchain
            .tools
            .insert("node".to_string(), "22\"\nevil = \"x".to_string());
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn validate_manifest_accepts_well_formed_fields() {
        let mut m = minimal_manifest();
        m.repos.push(suzerain_protocol::manifest::Repo {
            url: "git@github.com:org/repo.git".to_string(),
            ref_: "main".to_string(),
        });
        m.toolchain
            .tools
            .insert("node".to_string(), "22".to_string());
        assert!(validate_manifest(&m).is_ok());
    }

    fn manifest_with_mount_host(host: &str) -> suzerain_protocol::manifest::AgentManifest {
        let text = format!(
            r#"
name = "agent-1"
harness = {{ type = "pi", version = "0.84.1" }}
model = {{ provider = "anthropic", id = "claude-sonnet-4-5" }}

[[provision.mounts]]
host = "{host}"
guest = "/mnt/extra"
"#
        );
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn rejects_absolute_mount_host() {
        let m = manifest_with_mount_host("/etc");
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn rejects_parent_traversal_mount_host() {
        let m = manifest_with_mount_host("../../../etc");
        assert!(validate_manifest(&m).is_err());
    }

    #[test]
    fn accepts_relative_mount_host() {
        let m = manifest_with_mount_host("shared/data");
        assert!(validate_manifest(&m).is_ok());
    }

    /// `shell_quote`'s output must round-trip through a real shell: given
    /// back to `sh -c 'printf %s "$1"' -- <quoted>`, it must reproduce the
    /// original string byte-for-byte, including values crafted to break out
    /// of naive `'{value}'` interpolation (the shell-injection bug this
    /// helper closes).
    fn round_trips_through_shell(s: &str) -> bool {
        let quoted = shell_quote(s);
        let script = format!("printf %s {quoted}");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sh should run");
        assert!(output.status.success(), "script failed: {script}");
        String::from_utf8(output.stdout).unwrap() == s
    }

    #[test]
    fn quotes_plain_values() {
        assert!(round_trips_through_shell("main"));
        assert!(round_trips_through_shell("https://github.com/org/repo.git"));
    }

    #[test]
    fn escapes_embedded_single_quote() {
        assert!(round_trips_through_shell("main'; rm -rf / #"));
        assert!(round_trips_through_shell(
            "https://github.com/org/repo'; touch /tmp/pwned; echo '.git"
        ));
    }

    #[test]
    fn escapes_other_shell_metacharacters() {
        assert!(round_trips_through_shell("$(rm -rf /)"));
        assert!(round_trips_through_shell("`rm -rf /`"));
        assert!(round_trips_through_shell("a && b || c; d | e"));
        assert!(round_trips_through_shell("a\nb"));
    }

    // ── host-parsing helpers (egress allowlist derivation) ───────────────

    #[test]
    fn url_host_extracts_host_from_https_url() {
        assert_eq!(
            url_host("https://github.com/org/repo").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            url_host("https://example.com:8443/path").as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn url_host_returns_none_without_a_scheme() {
        assert_eq!(url_host("not-a-url"), None);
        // ssh shorthand has no "://" — url_host is not meant to parse it.
        assert_eq!(url_host("git@github.com:org/repo.git"), None);
    }

    #[test]
    fn repo_host_handles_ssh_and_https_forms() {
        assert_eq!(
            repo_host("git@github.com:org/repo.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            repo_host("https://gitlab.com/org/repo.git").as_deref(),
            Some("gitlab.com")
        );
    }

    fn ext_source(source: &str) -> suzerain_protocol::manifest::Extension {
        suzerain_protocol::manifest::Extension {
            source: Some(source.to_string()),
            url: None,
            ref_: None,
        }
    }

    #[test]
    fn extension_host_npm_source_is_the_npm_registry() {
        assert_eq!(
            extension_host(&ext_source("npm:@scope/pkg")).as_deref(),
            Some("registry.npmjs.org")
        );
    }

    #[test]
    fn extension_host_git_ssh_source() {
        assert_eq!(
            extension_host(&ext_source("git:git@github.com:org/ext.git")).as_deref(),
            Some("github.com")
        );
    }

    #[test]
    fn extension_host_git_https_source() {
        assert_eq!(
            extension_host(&ext_source("git:https://gitlab.com/org/ext.git")).as_deref(),
            Some("gitlab.com")
        );
    }

    #[test]
    fn extension_host_bare_host_path_source_requires_a_dot() {
        // "host/path" form — accepted only when the first segment looks
        // like a real host (contains a dot), so bare words don't get
        // mistaken for hosts and egress-allowlisted.
        assert_eq!(
            extension_host(&ext_source("git:example.com/org/ext.git")).as_deref(),
            Some("example.com")
        );
        assert_eq!(extension_host(&ext_source("git:localpath/ext")), None);
    }

    fn minimal_record() -> AgentRecord {
        AgentRecord {
            id: Uuid::new_v4(),
            name: "agent-1".to_string(),
            manifest: minimal_manifest(),
            state: suzerain_protocol::state::AgentState::Active,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            session_file: None,
            checkpoint: None,
            last_activity_at: None,
        }
    }

    #[test]
    fn egress_hosts_includes_secret_repo_otel_and_extra_hosts_deduped_and_sorted() {
        use suzerain_protocol::secrets::{SecretBundle, SecretEntry};

        let mut record = minimal_record();
        record
            .manifest
            .repos
            .push(suzerain_protocol::manifest::Repo {
                url: "https://github.com/org/repo.git".to_string(),
                ref_: "main".to_string(),
            });
        record.manifest.observability.otel = Some(suzerain_protocol::manifest::Otel {
            endpoint: "https://otel.example.com:4317".to_string(),
            headers: Default::default(),
        });
        record
            .manifest
            .egress
            .allow
            .push("extra.example.com".to_string());
        // Duplicate of a repo host, on purpose: egress_hosts must dedup.
        record.manifest.egress.allow.push("github.com".to_string());

        let mut bundle = SecretBundle::default();
        bundle.env.insert(
            "API_KEY".to_string(),
            SecretEntry {
                value: "secret".to_string(),
                hosts: vec!["api.example.com".to_string()],
            },
        );

        let hosts = egress_hosts(&record, &bundle);

        for expected in [
            "dl-cdn.alpinelinux.org",
            "registry.npmjs.org",
            "mise.run",
            "github.com",
            "objects.githubusercontent.com",
            "nodejs.org",
            "api.example.com",
            "otel.example.com",
            "extra.example.com",
        ] {
            assert!(hosts.iter().any(|h| h == expected), "missing {expected}");
        }
        // Sorted + deduped: no repeats, and ascending order.
        let mut sorted = hosts.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            hosts, sorted,
            "egress_hosts must return a sorted, deduped list"
        );
    }

    #[test]
    fn git_hosts_dedupes_and_sorts_repo_hosts() {
        let mut record = minimal_record();
        record
            .manifest
            .repos
            .push(suzerain_protocol::manifest::Repo {
                url: "git@github.com:org/repo-a.git".to_string(),
                ref_: "main".to_string(),
            });
        record
            .manifest
            .repos
            .push(suzerain_protocol::manifest::Repo {
                url: "https://github.com/org/repo-b.git".to_string(),
                ref_: "main".to_string(),
            });
        record
            .manifest
            .repos
            .push(suzerain_protocol::manifest::Repo {
                url: "git@bitbucket.org:org/repo-c.git".to_string(),
                ref_: "main".to_string(),
            });

        let hosts = git_hosts(&record);
        assert_eq!(
            hosts,
            vec!["bitbucket.org".to_string(), "github.com".to_string()]
        );
    }

    #[test]
    fn pi_spawn_env_includes_placeholders_and_toolchain_path() {
        let record = minimal_record();
        let mut placeholders = BTreeMap::new();
        placeholders.insert("ANTHROPIC_API_KEY".to_string(), "placeholder-1".to_string());

        let env = pi_spawn_env(&record, &placeholders);
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());

        assert_eq!(get("ANTHROPIC_API_KEY").as_deref(), Some("placeholder-1"));
        assert_eq!(
            get("PI_CODING_AGENT_DIR").as_deref(),
            Some("/agent/pi-home")
        );
        assert!(get("PATH")
            .unwrap()
            .starts_with("/agent/toolchain/global/bin:"));
        // No otel configured on the minimal manifest.
        assert_eq!(get("OTEL_EXPORTER_OTLP_ENDPOINT"), None);
    }

    #[test]
    fn pi_spawn_env_includes_otel_vars_when_configured() {
        let mut record = minimal_record();
        let mut headers = BTreeMap::new();
        headers.insert("x-api-key".to_string(), "k".to_string());
        record.manifest.observability.otel = Some(suzerain_protocol::manifest::Otel {
            endpoint: "https://otel.example.com:4317".to_string(),
            headers,
        });

        let env = pi_spawn_env(&record, &BTreeMap::new());
        let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());

        assert_eq!(
            get("OTEL_EXPORTER_OTLP_ENDPOINT").as_deref(),
            Some("https://otel.example.com:4317")
        );
        assert_eq!(
            get("OTEL_EXPORTER_OTLP_HEADERS").as_deref(),
            Some("x-api-key=k")
        );
        assert_eq!(
            get("OTEL_SERVICE_NAME").as_deref(),
            Some(record.name.as_str())
        );
    }

    // ── fake gondolin-driver, for exercising the (few) provisioning helpers
    // that shell out, without a real Gondolin VM ──────────────────────────
    //
    // `write_npm_shim` / `ensure_npm_toolchain` need a live `&DriverClient`,
    // which only ever talks to a real `gondolin-driver` (Node) sidecar over
    // stdio. This stands a tiny Node script in for that sidecar (wired via
    // $CASTELLAN_DRIVER) that acks every command instead of booting a VM, so
    // these tests can observe the *host-side* filesystem effects for real.

    // A tokio (not std) mutex: the guard is held across the `.await` in
    // `DriverClient::spawn` below, and clippy correctly flags a std
    // `MutexGuard` held over an await point as a footgun in general (it
    // isn't `Send`, and holding a std lock across a real suspension can
    // deadlock an executor). `DriverClient::spawn` itself never actually
    // suspends, but using the async-aware mutex here is free and keeps that
    // invariant from being load-bearing.
    static DRIVER_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn fake_driver_script_src() -> &'static str {
        r#"
process.stdin.setEncoding('utf8');
let buf = '';
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let idx;
  while ((idx = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, idx);
    buf = buf.slice(idx + 1);
    if (!line.trim()) continue;
    let msg;
    try { msg = JSON.parse(line); } catch (e) { continue; }
    handle(msg);
  }
});
process.stdin.on('end', () => process.exit(0));

function reply(id, result) {
  if (id === undefined) return;
  process.stdout.write(JSON.stringify({ event: 'reply', id, ok: true, result: result || {} }) + '\n');
}

function handle(msg) {
  switch (msg.cmd) {
    case 'boot': reply(msg.id, { placeholders: {} }); break;
    case 'checkpoint': reply(msg.id, { path: msg.path }); break;
    case 'exec': reply(msg.id, { exitCode: 0, stdout: '', stderr: '' }); break;
    default: reply(msg.id, {});
  }
}
"#
    }

    /// Spawn a `DriverClient` wired to the fake driver script above.
    /// Serializes on `DRIVER_ENV_LOCK` because `DriverClient::spawn` reads
    /// $CASTELLAN_DRIVER from process-wide env.
    async fn spawn_fake_driver() -> (Arc<DriverClient>, std::path::PathBuf) {
        let script_path =
            std::env::temp_dir().join(format!("castellan-fake-driver-{}.cjs", Uuid::new_v4()));
        std::fs::write(&script_path, fake_driver_script_src()).unwrap();
        let _guard = DRIVER_ENV_LOCK.lock().await;
        std::env::set_var("CASTELLAN_DRIVER", &script_path);
        let driver = DriverClient::spawn().await.expect("spawn fake driver");
        std::env::remove_var("CASTELLAN_DRIVER");
        (driver, script_path)
    }

    fn test_agent_paths(tag: &str) -> AgentPaths {
        let root = std::env::temp_dir().join(format!("castellan-test-{tag}-{}", Uuid::new_v4()));
        let guest = root.join("guest");
        AgentPaths {
            workspace: guest.join("workspace"),
            pi_home: guest.join("pi-home"),
            sessions: guest.join("sessions"),
            extensions: guest.join("pi-home").join("extensions"),
            guest,
            root,
        }
    }

    #[tokio::test]
    async fn write_npm_shim_confines_output_under_the_guest_dir_for_relative_prefix() {
        let paths = test_agent_paths("shim-ok");
        std::fs::create_dir_all(&paths.guest).unwrap();
        let (driver, script) = spawn_fake_driver().await;

        write_npm_shim(&driver, &paths, "/agent/toolchain/custom")
            .await
            .unwrap();

        let expected = paths.guest.join("toolchain/custom/bin/npm");
        assert!(expected.exists(), "shim should land under the guest dir");
        assert!(expected.starts_with(&paths.guest));

        std::fs::remove_dir_all(&paths.root).ok();
        std::fs::remove_file(&script).ok();
    }

    /// `write_npm_shim` takes `prefix` straight from `provision.install[].prefix`
    /// in the manifest (order-supplied). A `prefix` not rooted at `/agent/`
    /// must be rejected outright: previously `strip_prefix("/agent/")`
    /// silently fell back to the whole (still-absolute) `prefix`, and
    /// `Path::join`/`PathBuf::join` with an absolute argument **discards the
    /// base entirely** (documented std behavior) — so the shim would be
    /// written wherever `prefix` pointed on the HOST filesystem, ignoring the
    /// per-agent guest jail completely. This was a real path-traversal /
    /// arbitrary-file-write bug reachable from an order's manifest, now fixed
    /// by erroring instead of falling back.
    #[tokio::test]
    async fn write_npm_shim_prefix_outside_agent_must_not_escape_the_guest_dir() {
        let paths = test_agent_paths("shim-escape");
        std::fs::create_dir_all(&paths.guest).unwrap();
        let (driver, script) = spawn_fake_driver().await;

        let escape_dir = std::env::temp_dir().join(format!("castellan-escape-{}", Uuid::new_v4()));

        let result = write_npm_shim(&driver, &paths, escape_dir.to_str().unwrap()).await;
        assert!(
            result.is_err(),
            "write_npm_shim must reject a prefix outside /agent/, not silently escape the guest jail"
        );

        let escaped_shim = escape_dir.join("bin").join("npm");
        assert!(
            !escaped_shim.exists(),
            "write_npm_shim wrote outside the agent's guest jail, to {}",
            escaped_shim.display()
        );

        std::fs::remove_dir_all(&paths.root).ok();
        std::fs::remove_dir_all(&escape_dir).ok();
        std::fs::remove_file(&script).ok();
    }

    /// `ensure_npm_toolchain` documents itself as idempotent, skipping work
    /// when its marker file already exists. Use a driver that exits (and so
    /// closes its stdio) the instant it's spawned: if the marker check were
    /// ever bypassed and the code tried to shell out, that `driver.sh(..)`
    /// call would fail fast (driver already gone) and this test's `.unwrap()`
    /// would panic — instead of silently hanging on a driver that never
    /// replies.
    #[tokio::test]
    async fn ensure_npm_toolchain_skips_the_driver_when_marker_already_exists() {
        let paths = test_agent_paths("npm-marker");
        let marker_dir = paths.guest.join("toolchain/npm/bin");
        std::fs::create_dir_all(&marker_dir).unwrap();
        std::fs::write(marker_dir.join("npm-cli.js"), "// already installed").unwrap();

        let script_path =
            std::env::temp_dir().join(format!("castellan-dead-driver-{}.cjs", Uuid::new_v4()));
        std::fs::write(&script_path, "process.exit(0);\n").unwrap();
        let driver = {
            let _guard = DRIVER_ENV_LOCK.lock().await;
            std::env::set_var("CASTELLAN_DRIVER", &script_path);
            let d = DriverClient::spawn().await.expect("spawn dead driver");
            std::env::remove_var("CASTELLAN_DRIVER");
            d
        };

        ensure_npm_toolchain(&driver, &paths)
            .await
            .expect("marker present: must return without touching the driver");

        std::fs::remove_dir_all(&paths.root).ok();
        std::fs::remove_file(&script_path).ok();
    }
}
