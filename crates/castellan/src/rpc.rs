//! pi RPC adapter: a typed client for a `pi --mode rpc` process running
//! inside the agent's Gondolin VM (via the gondolin-driver stdio bridge).
//!
//! Handles id-correlated command responses and fans the raw pi event stream
//! out to subscribers (journal, attach relays).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
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
    /// Process liveness: -1 alive, >= 0 exit code, -2 driver died. Checked
    /// by `command` so calls against a dead pi fail fast with a useful
    /// error instead of hanging into the 60s RPC timeout.
    exit: Arc<AtomicI64>,
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
        let exit = Arc::new(AtomicI64::new(-1));

        // Demux task: driver agent_stdout lines → responses (by id) + events.
        let mut rx = driver.subscribe();
        let pending_task = Arc::clone(&pending);
        let events_task = events.clone();
        let exit_task = Arc::clone(&exit);
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
                        exit_task.store(code as i64, Ordering::SeqCst);
                        let _ = events_task.send(json!({"type": "pi_exit", "code": code}));
                        // Fail all pending commands: no response is coming.
                        pending_task.lock().await.clear();
                        break;
                    }
                    Ok(DriverEvent::DriverDied) => {
                        exit_task.store(-2, Ordering::SeqCst);
                        // VM/driver gone: surface as a crash-level event.
                        let _ = events_task.send(json!({"type": "driver_died"}));
                        pending_task.lock().await.clear();
                        break;
                    }
                    // Shell streams are relayed by control.rs, not the pi demux.
                    Ok(DriverEvent::ShellData { .. }) | Ok(DriverEvent::ShellExit { .. }) => {}
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
            exit,
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// Send an RPC command and await its response.
    pub async fn command(&self, mut cmd: Value) -> Result<Value> {
        match self.exit.load(Ordering::SeqCst) {
            -1 => {}
            -2 => bail!("agent VM driver is gone; pi is not running"),
            code => bail!(
                "pi exited (code {code}); the agent is crash-looping or misconfigured \
                 (check provider/model)"
            ),
        }
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

    pub async fn steer(&self, message: &str) -> Result<()> {
        self.command(json!({"type": "steer", "message": message}))
            .await?;
        Ok(())
    }

    pub async fn follow_up(&self, message: &str) -> Result<()> {
        self.command(json!({"type": "follow_up", "message": message}))
            .await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::process::Command;

    /// A `DriverClient` stub good enough to exercise `PiAgent::command`'s
    /// wire logic (id correlation, success/failure parsing) without a real
    /// gondolin-driver/node/pi process. `agent_stdin`/`request*` writes go to
    /// a harmless `cat` child that just discards them; nothing reads its
    /// stdout, since these tests fulfill `PiAgent`'s own pending map
    /// directly instead of round-tripping through the driver.
    async fn stub_driver() -> Arc<DriverClient> {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn stub driver child");
        let stdin = child.stdin.take().unwrap();
        Arc::new(DriverClient {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(0),
            pending: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(16).0,
        })
    }

    /// Build a `PiAgent` directly (bypassing `spawn`, which needs a real pi
    /// process inside a VM) around a stub driver.
    async fn stub_agent() -> Arc<PiAgent> {
        Arc::new(PiAgent {
            driver: stub_driver().await,
            next_id: AtomicU64::new(0),
            pending: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(16).0,
            exit: Arc::new(AtomicI64::new(-1)),
        })
    }

    /// Wait for `command()` to register its request, then hand back the id
    /// so the test can fulfill it as if pi had replied.
    async fn wait_for_pending_id(agent: &PiAgent) -> String {
        loop {
            if let Some(id) = agent.pending.lock().await.keys().next().cloned() {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn fulfill(agent: &PiAgent, reply: Value) {
        let id = wait_for_pending_id(agent).await;
        let tx = agent.pending.lock().await.remove(&id).unwrap();
        tx.send(reply).unwrap();
    }

    #[tokio::test]
    async fn command_fails_fast_when_driver_died() {
        let agent = stub_agent().await;
        agent.exit.store(-2, Ordering::SeqCst);
        let err = agent.command(json!({"type": "prompt"})).await.unwrap_err();
        assert!(
            err.to_string().contains("driver is gone"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn command_fails_fast_when_pi_already_exited() {
        let agent = stub_agent().await;
        agent.exit.store(3, Ordering::SeqCst);
        let err = agent.command(json!({"type": "prompt"})).await.unwrap_err();
        assert!(
            err.to_string().contains("pi exited (code 3)"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn command_success_returns_full_response() {
        let agent = stub_agent().await;
        let handle = tokio::spawn({
            let agent = Arc::clone(&agent);
            async move { agent.command(json!({"type": "get_state"})).await }
        });
        fulfill(&agent, json!({"success": true, "data": {"phase": "idle"}})).await;
        let resp = handle.await.unwrap().expect("command should succeed");
        assert_eq!(resp["data"]["phase"], "idle");
    }

    #[tokio::test]
    async fn command_failure_reports_pi_error_and_command_type() {
        let agent = stub_agent().await;
        let handle = tokio::spawn({
            let agent = Arc::clone(&agent);
            async move { agent.command(json!({"type": "prompt"})).await }
        });
        fulfill(&agent, json!({"success": false, "error": "boom"})).await;
        let err = handle.await.unwrap().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("boom"), "unexpected error: {msg}");
        assert!(msg.contains("prompt"), "unexpected error: {msg}");
    }

    #[tokio::test]
    async fn get_state_returns_the_data_field() {
        let agent = stub_agent().await;
        let handle = tokio::spawn({
            let agent = Arc::clone(&agent);
            async move { agent.get_state().await }
        });
        fulfill(
            &agent,
            json!({"success": true, "data": {"phase": "running"}}),
        )
        .await;
        let data = handle.await.unwrap().unwrap();
        assert_eq!(data["phase"], "running");
    }

    #[tokio::test]
    async fn get_last_assistant_text_extracts_text_field() {
        let agent = stub_agent().await;
        let handle = tokio::spawn({
            let agent = Arc::clone(&agent);
            async move { agent.get_last_assistant_text().await }
        });
        fulfill(
            &agent,
            json!({"success": true, "data": {"text": "hello world"}}),
        )
        .await;
        let text = handle.await.unwrap().unwrap();
        assert_eq!(text, Some("hello world".to_string()));
    }

    #[tokio::test]
    async fn get_last_assistant_text_is_none_when_field_absent() {
        let agent = stub_agent().await;
        let handle = tokio::spawn({
            let agent = Arc::clone(&agent);
            async move { agent.get_last_assistant_text().await }
        });
        fulfill(&agent, json!({"success": true, "data": {}})).await;
        let text = handle.await.unwrap().unwrap();
        assert_eq!(text, None);
    }

    #[tokio::test]
    async fn prompt_abort_steer_follow_up_surface_pi_errors() {
        // These are thin wrappers around `command`; confirm each actually
        // sends its own distinct command type and surfaces failure rather
        // than swallowing it.
        for (name, fut_kind) in [
            ("prompt", "prompt"),
            ("abort", "abort"),
            ("steer", "steer"),
            ("follow_up", "follow_up"),
        ] {
            let agent = stub_agent().await;
            let handle = tokio::spawn({
                let agent = Arc::clone(&agent);
                async move {
                    match fut_kind {
                        "prompt" => agent.prompt("hi").await,
                        "abort" => agent.abort().await,
                        "steer" => agent.steer("hi").await,
                        "follow_up" => agent.follow_up("hi").await,
                        _ => unreachable!(),
                    }
                }
            });
            fulfill(&agent, json!({"success": false, "error": "nope"})).await;
            let err = handle.await.unwrap().unwrap_err();
            assert!(
                err.to_string().contains(fut_kind),
                "{name}: expected error to mention its own command type, got: {err}"
            );
        }
    }
}
