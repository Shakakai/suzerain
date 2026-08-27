//! Client for the gondolin-driver sidecar (Node). One driver process per
//! agent VM. Speaks the JSONL command/event protocol from
//! tools/gondolin-driver/src/index.mjs over the child's stdio.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot, Mutex};

/// Non-reply events emitted by the driver.
#[derive(Debug, Clone)]
pub enum DriverEvent {
    /// A stdout line from the agent process (pi RPC JSONL).
    AgentStdout(String),
    /// A stderr line from the agent process.
    AgentStderr(String),
    /// The agent process exited.
    AgentExit(i32),
    /// Pty output chunk from a shell (base64).
    ShellData { shell: u32, data: String },
    /// A shell process exited.
    ShellExit { shell: u32, code: i32 },
    /// The driver process itself died (stdout EOF): the VM and everything in
    /// it is gone.
    DriverDied,
}

/// Default timeout for commands that can legitimately run long: `boot`
/// (first boot downloads guest assets), guest `exec` (arbitrary provisioning
/// scripts).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(360);
/// Timeout for commands that should return quickly; a wedge here is a
/// signal to stop waiting and let the caller fall back to killing the
/// driver process rather than sit out the full [`DEFAULT_TIMEOUT`].
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);

pub struct DriverClient {
    child: Mutex<Child>,
    stdin: Mutex<tokio::process::ChildStdin>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<DriverEvent>,
}

impl DriverClient {
    /// Spawn the driver sidecar. The driver script is resolved from (first
    /// hit wins): $CASTELLAN_DRIVER → <data dir>/driver → exe-relative repo
    /// path → walking up from cwd for a repo checkout → cwd-relative path.
    pub async fn spawn() -> Result<Arc<Self>> {
        let script = driver_script()?;
        let mut child = Command::new(node_binary())
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Kill the driver if the daemon dies: a dropped Child is NOT
            // killed by default, orphaning the driver (and its VM, which
            // keeps holding guest memory — the pressure behind the
            // 2026-08-12 provisioning wedge).
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawning gondolin-driver: {}", script.display()))?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);

        // Reader task: demux replies (by id) from events.
        let pending_task = Arc::clone(&pending);
        let events_task = events.clone();
        let mut lines = stdout;
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line).await {
                    Ok(0) => break, // driver exited
                    Ok(_) => {
                        let msg: Value = match serde_json::from_str(line.trim_end()) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        match msg["event"].as_str() {
                            Some("reply") => {
                                if let Some(id) = msg["id"].as_u64() {
                                    if let Some(tx) = pending_task.lock().await.remove(&id) {
                                        let _ = tx.send(msg);
                                    }
                                }
                            }
                            Some("agent_stdout") => {
                                let l = msg["line"].as_str().unwrap_or("").to_string();
                                let _ = events_task.send(DriverEvent::AgentStdout(l));
                            }
                            Some("agent_stderr") => {
                                let l = msg["line"].as_str().unwrap_or("").to_string();
                                let _ = events_task.send(DriverEvent::AgentStderr(l));
                            }
                            Some("agent_exit") => {
                                let code = msg["exitCode"].as_i64().unwrap_or(-1) as i32;
                                let _ = events_task.send(DriverEvent::AgentExit(code));
                            }
                            Some("shell_data") => {
                                let shell = msg["shell"].as_u64().unwrap_or(0) as u32;
                                let data = msg["data"].as_str().unwrap_or("").to_string();
                                let _ = events_task.send(DriverEvent::ShellData { shell, data });
                            }
                            Some("shell_exit") => {
                                let shell = msg["shell"].as_u64().unwrap_or(0) as u32;
                                let code = msg["exitCode"].as_i64().unwrap_or(-1) as i32;
                                let _ = events_task.send(DriverEvent::ShellExit { shell, code });
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
            // Driver exited: fail every in-flight request instead of hanging.
            pending_task.lock().await.clear();
            let _ = events_task.send(DriverEvent::DriverDied);
        });

        Ok(Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(0),
            pending,
            events,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DriverEvent> {
        self.events.subscribe()
    }

    /// Send a command and await its reply, with the default (generous)
    /// timeout — for commands that can legitimately take a while (VM boot,
    /// arbitrary guest `exec`). See [`Self::request_timeout`] for commands
    /// that should fail fast instead of inheriting that ceiling.
    pub async fn request(&self, cmd: Value) -> Result<Value> {
        self.request_timeout(cmd, DEFAULT_TIMEOUT).await
    }

    /// Send a command and await its reply within `timeout`. Errors on
    /// driver-side failure or on timeout.
    ///
    /// Every call used to share one 360s ceiling regardless of what the
    /// command actually was: a wedged `boot` and a `close` (which should
    /// return almost immediately) waited the same up-to-6-minutes before
    /// giving up, so a single wedged request could make an unrelated cheap
    /// one look wedged too. Callers now pick a timeout that fits the
    /// command.
    pub async fn request_timeout(&self, mut cmd: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        {
            let mut line = serde_json::to_vec(&cmd)?;
            line.push(b'\n');
            let mut stdin = self.stdin.lock().await;
            stdin.write_all(&line).await?;
            stdin.flush().await?;
        }
        let reply = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => bail!("driver exited or dropped the reply channel"),
            Err(_) => {
                // Without this, a timed-out id lingers in `pending` forever
                // (or until the driver dies and clears the whole map) —
                // a slow leak of one oneshot::Sender per timeout over the
                // daemon's life.
                self.pending.lock().await.remove(&id);
                bail!("driver command timed out after {}s", timeout.as_secs());
            }
        };
        if reply["ok"].as_bool() == Some(true) {
            Ok(reply["result"].clone())
        } else {
            bail!(
                "driver command {} failed: {}",
                cmd["cmd"].as_str().unwrap_or("?"),
                reply["error"].as_str().unwrap_or("unknown")
            )
        }
    }

    /// Boot the agent's VM. `mounts`: guest path → host path. If
    /// `resume_from` is set, the VM resumes from that disk checkpoint
    /// instead of a fresh boot. `secrets` wires Gondolin HTTP-hook
    /// placeholder injection; the returned map is env var → placeholder value
    /// (what the guest process should see). `allowed_hosts` is the egress
    /// allowlist; `git_ssh_key` enables proxied SSH git egress with the key
    /// held host-side.
    #[allow(clippy::too_many_arguments)]
    pub async fn boot(
        &self,
        mounts: &[(String, String)],
        env: &[(String, String)],
        session_label: &str,
        resume_from: Option<&str>,
        secrets: &suzerain_protocol::secrets::SecretBundle,
        allowed_hosts: &[String],
        git_hosts: &[String],
        resources: &suzerain_protocol::manifest::Resources,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let mounts_obj: Value = mounts
            .iter()
            .map(|(g, h)| (g.clone(), json!({ "hostPath": h })))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let env_obj: Value = env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let secrets_obj: Value = secrets
            .env
            .iter()
            .map(|(k, e)| (k.clone(), json!({"value": e.value, "hosts": e.hosts})))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let mut options = json!({
            "mounts": mounts_obj,
            "env": env_obj,
            "sessionLabel": session_label,
            "memory": format!("{}M", resources.memory_mib),
            "cpus": resources.vcpu,
            "secrets": secrets_obj,
            "allowedHosts": allowed_hosts,
        });
        if let Some(path) = resume_from {
            options["resumeFrom"] = json!(path);
        }
        if let Some(key) = &secrets.git_ssh_key {
            options["ssh"] = json!({
                "allowedHosts": git_hosts,
                "credentials": git_hosts.iter().map(|h| (h.clone(), json!({"privateKey": key}))).collect::<serde_json::Map<_,_>>(),
            });
        }
        let result = self
            .request(json!({ "cmd": "boot", "options": options }))
            .await?;
        let placeholders = result["placeholders"].clone();
        Ok(serde_json::from_value(placeholders).unwrap_or_default())
    }

    /// Disk-checkpoint the VM (stops it). Returns the checkpoint path.
    pub async fn checkpoint(&self, path: &str) -> Result<String> {
        let r = self
            .request(json!({"cmd": "checkpoint", "path": path}))
            .await?;
        Ok(r["path"].as_str().unwrap_or(path).to_string())
    }

    /// Buffered exec in the guest. Returns (exit_code, stdout, stderr).
    pub async fn exec(
        &self,
        argv: &[&str],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Result<(i64, String, String)> {
        let env_obj: Value = env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let r = self
            .request(json!({"cmd": "exec", "argv": argv, "cwd": cwd, "env": env_obj}))
            .await?;
        Ok((
            r["exitCode"].as_i64().unwrap_or(-1),
            r["stdout"].as_str().unwrap_or("").to_string(),
            r["stderr"].as_str().unwrap_or("").to_string(),
        ))
    }

    /// Convenience: shell out inside the guest, requiring exit 0.
    pub async fn sh(&self, script: &str, env: &[(String, String)]) -> Result<String> {
        let (code, stdout, stderr) = self.exec(&["sh", "-lc", script], None, env).await?;
        if code != 0 {
            bail!("guest sh failed ({code}): {script}\n{stderr}");
        }
        Ok(stdout)
    }

    /// Spawn the long-running agent process (streaming stdio).
    pub async fn spawn_agent(
        &self,
        argv: &[&str],
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<()> {
        let env_obj: Value = env
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect::<serde_json::Map<_, _>>()
            .into();
        // Spawning is just kicking off a background process in the driver;
        // it doesn't wait for the agent to do anything, so it should never
        // legitimately take long.
        self.request_timeout(
            json!({
                "cmd": "spawn_agent", "argv": argv, "cwd": cwd, "env": env_obj,
            }),
            QUICK_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Write a line to the agent's stdin.
    pub async fn agent_stdin(&self, line: &str) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let msg = json!({"cmd": "agent_stdin", "data": format!("{line}\n")});
        let mut buf = serde_json::to_vec(&msg)?;
        buf.push(b'\n');
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Fire-and-forget command write (no reply expected).
    async fn send_cmd(&self, cmd: Value) -> Result<()> {
        let mut buf = serde_json::to_vec(&cmd)?;
        buf.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(&buf).await?;
        stdin.flush().await?;
        Ok(())
    }

    /// Spawn an interactive pty shell in the guest. Output arrives as
    /// DriverEvent::ShellData on the broadcast channel.
    pub async fn shell_spawn(
        &self,
        shell: u32,
        argv: &[&str],
        cwd: Option<&str>,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        self.request_timeout(
            json!({
                "cmd": "shell_spawn",
                "shell": shell,
                "argv": argv,
                "cwd": cwd,
                "cols": cols,
                "rows": rows,
                "pty": true,
            }),
            QUICK_TIMEOUT,
        )
        .await?;
        Ok(())
    }

    /// Write raw bytes (base64) to a shell's stdin.
    pub async fn shell_stdin(&self, shell: u32, data_b64: &str) -> Result<()> {
        self.send_cmd(json!({"cmd": "shell_stdin", "shell": shell, "data": data_b64}))
            .await
    }

    pub async fn shell_resize(&self, shell: u32, cols: u16, rows: u16) -> Result<()> {
        self.send_cmd(json!({"cmd": "shell_resize", "shell": shell, "cols": cols, "rows": rows}))
            .await
    }

    pub async fn shell_close(&self, shell: u32) -> Result<()> {
        self.send_cmd(json!({"cmd": "shell_close", "shell": shell}))
            .await
    }

    /// Shut the VM down and reap the driver process. Uses a short timeout:
    /// if the driver is wedged (the reason we're often calling this in the
    /// first place — see the provisioning-timeout and respawn cleanup
    /// paths), there's no point sitting out the full default timeout
    /// before falling through to killing the process outright.
    pub async fn close(&self) -> Result<()> {
        let _ = self
            .request_timeout(json!({"cmd": "close"}), QUICK_TIMEOUT)
            .await;
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        Ok(())
    }
}

/// Resolve the gondolin-driver script, trying install and dev layouts.
fn driver_script() -> Result<PathBuf> {
    const REL: &str = "tools/gondolin-driver/src/index.mjs";
    let mut tried: Vec<PathBuf> = Vec::new();

    if let Ok(explicit) = std::env::var("CASTELLAN_DRIVER") {
        let p = PathBuf::from(explicit);
        if p.exists() {
            return Ok(p);
        }
        tried.push(p);
    }

    // Installed copy in the daemon data dir (mise run package).
    let installed = crate::state::data_dir().join("driver/src/index.mjs");
    if installed.exists() {
        return Ok(installed);
    }
    tried.push(installed);

    // Dev layout: exe lives in target/{debug,release} → repo root is ../...
    if let Ok(exe) = std::env::current_exe() {
        if let Some(repo) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            let p = repo.join(REL);
            if p.exists() {
                return Ok(p);
            }
            tried.push(p);
        }
    }

    // Walk up from cwd looking for a repo checkout (marker: Cargo.toml + tools/).
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let p = d.join(REL);
        if p.exists() && d.join("Cargo.toml").exists() {
            return Ok(p);
        }
        dir = d.parent().map(|p| p.to_path_buf());
    }

    let fallback = PathBuf::from(REL);
    tried.push(fallback.clone());
    if fallback.exists() {
        return Ok(fallback);
    }

    anyhow::bail!(
        "gondolin-driver script not found; tried: {}. Set CASTELLAN_DRIVER or install the driver (mise run package).",
        tried.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
    )
}

/// Resolve node: PATH first, then the mise shims dir.
fn node_binary() -> PathBuf {
    let shim = format!(
        "{}/.local/share/mise/shims/node",
        std::env::var("HOME").unwrap_or_default()
    );
    let which = std::process::Command::new("which")
        .arg("node")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match which {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(shim),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `DriverClient` around a stub child process (not the real
    /// gondolin-driver) so `request_timeout` can be exercised without a
    /// Node/Gondolin environment. `cmd` runs under `sh -c`; whatever it
    /// writes to stdout is fed through the same reply-demux reader task
    /// `spawn()` installs.
    async fn spawn_stub(cmd: &str) -> Arc<DriverClient> {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn stub child");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(16);

        let pending_task = Arc::clone(&pending);
        let events_task = events.clone();
        let mut lines = stdout;
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(msg) = serde_json::from_str::<Value>(line.trim_end()) {
                            if msg["event"].as_str() == Some("reply") {
                                if let Some(id) = msg["id"].as_u64() {
                                    if let Some(tx) = pending_task.lock().await.remove(&id) {
                                        let _ = tx.send(msg);
                                    }
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            pending_task.lock().await.clear();
            let _ = events_task.send(DriverEvent::DriverDied);
        });

        Arc::new(DriverClient {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(0),
            pending,
            events,
        })
    }

    /// A wedged request (the stub never emits a `reply` event) must not
    /// leave its oneshot sender behind in `pending` once it times out —
    /// that would leak one entry per timeout over the daemon's life.
    #[tokio::test]
    async fn timed_out_request_does_not_leak_pending_entry() {
        let driver = spawn_stub("cat >/dev/null").await;
        let result = driver
            .request_timeout(json!({"cmd": "noop"}), Duration::from_millis(200))
            .await;
        assert!(result.is_err());
        assert!(
            driver.pending.lock().await.is_empty(),
            "timed-out request must not leak its pending entry"
        );
    }

    /// A cheap command given a short timeout must fail on ITS OWN timeout,
    /// not sit out anywhere near the long default ceiling that used to be
    /// shared by every command regardless of how quick it should be.
    #[tokio::test]
    async fn quick_timeout_does_not_wait_for_the_default_ceiling() {
        let driver = spawn_stub("cat >/dev/null").await;
        let start = std::time::Instant::now();
        let result = driver
            .request_timeout(json!({"cmd": "noop"}), Duration::from_millis(200))
            .await;
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "request_timeout should honor the short timeout, not DEFAULT_TIMEOUT"
        );
    }

    /// A reply that arrives in time is still delivered correctly (the
    /// per-call timeout plumbing doesn't break the happy path).
    #[tokio::test]
    async fn reply_within_timeout_succeeds() {
        // The stub is a tiny "driver": read one JSONL line, echo back a
        // matching ok reply.
        let driver = spawn_stub(
            "read line; id=$(echo \"$line\" | sed -n 's/.*\"id\":\\([0-9]*\\).*/\\1/p'); \
             printf '{\"event\":\"reply\",\"id\":%s,\"ok\":true,\"result\":{}}\\n' \"$id\"",
        )
        .await;
        let result = driver
            .request_timeout(json!({"cmd": "noop"}), Duration::from_secs(5))
            .await;
        assert!(result.is_ok(), "expected success, got {result:?}");
    }
}
