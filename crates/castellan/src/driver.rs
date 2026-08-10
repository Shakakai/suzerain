//! Client for the gondolin-driver sidecar (Node). One driver process per
//! agent VM. Speaks the JSONL command/event protocol from
//! tools/gondolin-driver/src/index.mjs over the child's stdio.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
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
}

pub struct DriverClient {
    child: Mutex<Child>,
    stdin: Mutex<tokio::process::ChildStdin>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<DriverEvent>,
}

impl DriverClient {
    /// Spawn the driver sidecar. Resolution order for the driver script:
    /// `CASTELLAN_DRIVER` env, then `<cwd>/tools/gondolin-driver/src/index.mjs`.
    pub async fn spawn() -> Result<Arc<Self>> {
        let script = std::env::var("CASTELLAN_DRIVER")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("tools/gondolin-driver/src/index.mjs"));
        let mut child = Command::new("node")
            .arg(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
            // Driver exited: fail every in-flight request instead of hanging.
            pending_task.lock().await.clear();
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

    /// Send a command and await its reply. Errors on driver-side failure.
    pub async fn request(&self, mut cmd: Value) -> Result<Value> {
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
        let reply = tokio::time::timeout(std::time::Duration::from_secs(120), rx)
            .await
            .map_err(|_| anyhow!("driver command timed out"))?
            .map_err(|_| anyhow!("driver exited or dropped the reply channel"))?;
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
    /// instead of a fresh boot.
    pub async fn boot(
        &self,
        mounts: &[(String, String)],
        env: &[(String, String)],
        session_label: &str,
        resume_from: Option<&str>,
    ) -> Result<()> {
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
        let mut options = json!({
            "mounts": mounts_obj,
            "env": env_obj,
            "sessionLabel": session_label,
            "memory": "2G",
            "cpus": 2,
        });
        if let Some(path) = resume_from {
            options["resumeFrom"] = json!(path);
        }
        self.request(json!({ "cmd": "boot", "options": options }))
            .await?;
        Ok(())
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
        self.request(json!({
            "cmd": "spawn_agent", "argv": argv, "cwd": cwd, "env": env_obj,
        }))
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

    /// Shut the VM down and reap the driver process.
    pub async fn close(&self) -> Result<()> {
        let _ = self.request(json!({"cmd": "close"})).await;
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
        Ok(())
    }
}
