//! Phase 0 spike (a): prove a Rust process can drive pi in RPC mode.
//!
//!   cargo run -p castellan --example spike_pi_rpc -- "your prompt"
//!   cargo run -p castellan --example spike_pi_rpc -- --resume <session-file> "follow-up"
//!
//! Validates: spawn `pi --mode rpc`, LF-delimited JSONL framing, id-correlated
//! command responses (get_state), async event streaming (prompt → agent_end),
//! and cross-process session resume (restore path).

use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

const SPIKE_SESSION_DIR: &str = "/tmp/suz-spike-pi-sessions";
const OVERALL_TIMEOUT: Duration = Duration::from_secs(180);

struct PiRpc {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl PiRpc {
    async fn spawn(resume: Option<&str>) -> Result<Self> {
        std::fs::create_dir_all(SPIKE_SESSION_DIR)?;
        let mut cmd = Command::new("npx");
        cmd.args([
            "--no-install",
            "pi",
            "--mode",
            "rpc",
            "--session-dir",
            SPIKE_SESSION_DIR,
            "--provider",
            "kimi-coding",
            "--model",
            "kimi-for-coding",
        ]);
        if let Some(path) = resume {
            cmd.args(["--session", path]);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("spawning pi in rpc mode (is `npx pi` available?)")?;

        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        })
    }

    async fn send(&mut self, cmd: Value) -> Result<String> {
        self.next_id += 1;
        let id = format!("spike-{}", self.next_id);
        let mut cmd = cmd;
        cmd["id"] = json!(id);
        let mut line = serde_json::to_vec(&cmd)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await?;
        Ok(id)
    }

    /// Read one JSONL record (LF-delimited only, per rpc.md framing rules).
    async fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).await?;
        if n == 0 {
            bail!(
                "pi closed stdout (exit status: {:?})",
                self.child.try_wait()?
            );
        }
        let trimmed = line.trim_end_matches(['\n', '\r']);
        Ok(serde_json::from_str(trimmed)?)
    }

    /// Read until the response for `id` arrives, printing events seen en route.
    async fn await_response(&mut self, id: &str) -> Result<Value> {
        loop {
            let msg = self.read().await?;
            if msg["type"] == "response" && msg["id"] == id {
                return Ok(msg);
            }
            print_event(&msg);
        }
    }
}

fn print_event(msg: &Value) {
    let ty = msg["type"].as_str().unwrap_or("?");
    match ty {
        "message_update" => {} // too chatty for the spike
        "response" => {}
        _ => println!("  [event] {ty}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (resume, prompt) = match args.as_slice() {
        [flag, path, rest @ ..] if flag == "--resume" => (Some(path.clone()), rest.join(" ")),
        _ => (None, args.join(" ")),
    };
    if prompt.is_empty() {
        bail!("usage: spike_pi_rpc [--resume <session-file>] <prompt>");
    }

    timeout(OVERALL_TIMEOUT, async move {
        let mut pi = PiRpc::spawn(resume.as_deref()).await?;
        println!(" spawned pi (rpc mode)");

        // 1. state query — proves id-correlated request/response.
        let id = pi.send(json!({"type": "get_state"})).await?;
        let state = pi.await_response(&id).await?;
        anyhow::ensure!(state["success"] == true, "get_state failed: {state}");
        println!(
            " get_state ok: model={}",
            state["data"]["model"]["id"].as_str().unwrap_or("?")
        );

        // 2. prompt — proves event streaming until the agent settles.
        println!(" prompt: {prompt}");
        let id = pi
            .send(json!({"type": "prompt", "message": prompt}))
            .await?;
        pi.await_response(&id).await?;
        loop {
            let msg = pi.read().await?;
            print_event(&msg);
            if matches!(msg["type"].as_str(), Some("agent_end" | "agent_settled")) {
                break;
            }
        }

        // 3. final answer.
        let id = pi.send(json!({"type": "get_last_assistant_text"})).await?;
        let resp = pi.await_response(&id).await?;
        println!(
            "\n=== assistant ===\n{}",
            resp["data"]["text"].as_str().unwrap_or("<none>")
        );

        // 4. session file from get_state (for --resume testing = restore path).
        let id = pi.send(json!({"type": "get_state"})).await?;
        let state = pi.await_response(&id).await?;
        if let Some(file) = state["data"]["sessionFile"].as_str() {
            println!("\nsession file (resume with --resume): {file}");
        }

        // 5. shutdown: kill, then keep draining until exit so pi doesn't EPIPE.
        pi.child.kill().await.ok();
        let _ = pi.child.wait().await;
        println!("spike ok");
        Ok(())
    })
    .await
    .context("spike timed out")?
}
