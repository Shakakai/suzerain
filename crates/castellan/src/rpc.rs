//! pi RPC adapter: a typed client for a `pi --mode rpc` process running
//! inside the agent's Gondolin VM (via the gondolin-driver stdio bridge).
//!
//! Handles id-correlated command responses and fans the raw pi event stream
//! out to subscribers (journal, attach relays).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Mutex};

use crate::driver::{DriverClient, DriverEvent};

pub struct PiAgent {
    driver: Arc<DriverClient>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    events: broadcast::Sender<Value>,
}

impl PiAgent {
    /// Spawn pi in RPC mode inside the VM. `env` must include provider
    /// credentials and `PI_CODING_AGENT_DIR` (per-agent isolation).
    ///
    /// Note: gondolin's array-form exec does not search $PATH, so the pi
    /// binary is referenced by absolute path.
    pub async fn spawn(
        driver: Arc<DriverClient>,
        cwd: &str,
        env: &[(String, String)],
        provider: &str,
        model: &str,
        resume_session: Option<&str>,
    ) -> Result<Arc<Self>> {
        let mut argv = vec![
            "/agent/toolchain/global/bin/pi".to_string(),
            "--mode".to_string(),
            "rpc".to_string(),
            "--session-dir".to_string(),
            "/agent/sessions".to_string(),
            "--provider".to_string(),
            provider.to_string(),
            "--model".to_string(),
            model.to_string(),
        ];
        if let Some(path) = resume_session {
            argv.push("--session".to_string());
            argv.push(path.to_string());
        }
        driver
            .spawn_agent(
                &argv.iter().map(String::as_str).collect::<Vec<_>>(),
                cwd,
                env,
            )
            .await?;

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(4096);

        // Demux task: driver agent_stdout lines → responses (by id) + events.
        let mut rx = driver.subscribe();
        let pending_task = Arc::clone(&pending);
        let events_task = events.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(DriverEvent::AgentStdout(line)) => {
                        let msg: Value = match serde_json::from_str(&line) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if msg["type"] == "response" {
                            if let Some(id) = msg["id"].as_str() {
                                if let Some(tx) = pending_task.lock().await.remove(id) {
                                    let _ = tx.send(msg);
                                }
                            }
                        } else {
                            let _ = events_task.send(msg);
                        }
                    }
                    Ok(DriverEvent::AgentStderr(line)) => {
                        let _ = events_task.send(json!({"type": "pi_stderr", "line": line}));
                    }
                    Ok(DriverEvent::AgentExit(code)) => {
                        let _ = events_task.send(json!({"type": "pi_exit", "code": code}));
                        // Fail all pending commands: no response is coming.
                        pending_task.lock().await.clear();
                        break;
                    }
                    Ok(DriverEvent::DriverDied) => {
                        // VM/driver gone: surface as a crash-level event.
                        let _ = events_task.send(json!({"type": "driver_died"}));
                        pending_task.lock().await.clear();
                        break;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok(Arc::new(Self {
            driver,
            next_id: AtomicU64::new(0),
            pending,
            events,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Send an RPC command and await its response.
    pub async fn command(&self, mut cmd: Value) -> Result<Value> {
        let id = format!("c{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.driver
            .agent_stdin(&serde_json::to_string(&cmd)?)
            .await?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(60), rx)
            .await
            .map_err(|_| anyhow!("pi command timed out"))?
            .map_err(|_| anyhow!("pi exited or dropped the response channel"))?;
        if resp["success"].as_bool() == Some(true) {
            Ok(resp)
        } else {
            bail!(
                "pi command {} failed: {}",
                cmd["type"].as_str().unwrap_or("?"),
                resp["error"].as_str().unwrap_or("unknown")
            )
        }
    }

    pub async fn get_state(&self) -> Result<Value> {
        Ok(self.command(json!({"type": "get_state"})).await?["data"].clone())
    }

    pub async fn prompt(&self, message: &str) -> Result<()> {
        self.command(json!({"type": "prompt", "message": message}))
            .await?;
        Ok(())
    }

    pub async fn abort(&self) -> Result<()> {
        self.command(json!({"type": "abort"})).await?;
        Ok(())
    }

    pub async fn get_last_assistant_text(&self) -> Result<Option<String>> {
        let resp = self
            .command(json!({"type": "get_last_assistant_text"}))
            .await?;
        Ok(resp["data"]["text"].as_str().map(str::to_string))
    }

    pub fn driver(&self) -> &Arc<DriverClient> {
        &self.driver
    }
}
