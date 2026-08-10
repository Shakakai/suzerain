//! Phase 1 local control API: a unix-socket JSONL server so agents outlive
//! CLI invocations. Phase 2 replaces/augments this with the iroh control
//! protocol; the command set deliberately mirrors the future Order surface.
//!
//! Protocol: one JSON object per line, both directions.
//!   → {"id":N,"cmd":"create","manifest":{...}}   (manifest inline object)
//!   ← {"id":N,"ok":true,"result":{...}}
//!   → {"id":N,"cmd":"attach","name":"x"}         (then: stream of events down,
//!     and {"cmd":"prompt","message":"..."} lines up on the same connection)

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tracing::{info, warn};
use uuid::Uuid;

use suzerain_protocol::manifest::AgentManifest;
use suzerain_protocol::state::AgentState;

use crate::journal::Journal;
use crate::state::{self, AgentPaths};
use crate::supervisor::Supervisor;

pub fn socket_path() -> PathBuf {
    state::data_dir().join("castellan.sock")
}

pub async fn serve(supervisor: Arc<Supervisor>) -> Result<()> {
    let path = socket_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::remove_file(&path).await.ok();
    let listener =
        UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    info!(socket = %path.display(), "castellan daemon listening");

    loop {
        let (stream, _) = listener.accept().await?;
        let sup = Arc::clone(&supervisor);
        tokio::spawn(async move {
            if let Err(err) = handle_conn(stream, sup).await {
                warn!("connection error: {err:#}");
            }
        });
    }
}

async fn reply(
    w: &mut tokio::net::unix::OwnedWriteHalf,
    id: Value,
    result: Result<Value>,
) -> Result<()> {
    let msg = match result {
        Ok(value) => json!({"id": id, "ok": true, "result": value}),
        Err(err) => json!({"id": id, "ok": false, "error": format!("{err:#}")}),
    };
    let mut line = serde_json::to_vec(&msg)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

async fn handle_conn(stream: UnixStream, sup: Arc<Supervisor>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let mut attach_rx: Option<broadcast::Receiver<Value>> = None;
    let mut attach_name = String::new();

    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Some(line) = line? else { break };
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                // Prompts on an attached connection are not request/reply.
                if attach_rx.is_some() && msg["cmd"] == "prompt" {
                    if let Some(message) = msg["message"].as_str() {
                        sup.prompt(&attach_name, message).await.ok();
                    }
                    continue;
                }
                let id = msg["id"].clone();
                let result = dispatch(&msg, &sup).await;
                // Attach switches this connection into streaming mode.
                let attaching = matches!(&result, Ok(v) if v["__attach"].as_bool() == Some(true));
                reply(&mut writer, id, result).await?;
                if attaching {
                    attach_rx = Some(sup.subscribe(msg["name"].as_str().unwrap_or("")).await?);
                    attach_name = msg["name"].as_str().unwrap_or("").to_string();
                }
            }
            event = async { attach_rx.as_mut().unwrap().recv().await }, if attach_rx.is_some() => {
                match event {
                    Ok(ev) => {
                        let mut line = serde_json::to_vec(&json!({"event": ev}))?;
                        line.push(b'\n');
                        if writer.write_all(&line).await.is_err() { break; }
                        writer.flush().await.ok();
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    Ok(())
}

async fn dispatch(msg: &Value, sup: &Arc<Supervisor>) -> Result<Value> {
    let cmd = msg["cmd"].as_str().unwrap_or("");
    match cmd {
        "create" => {
            let manifest: AgentManifest =
                serde_json::from_value(msg["manifest"].clone()).context("invalid manifest")?;
            // Standalone path: secrets come from the daemon's own env.
            let id = Uuid::new_v4();
            let bundle = crate::provision::bundle_from_env(&manifest);
            state::save_bundle(&id, &bundle).await?;
            let record = sup.create(Some(id), manifest).await?;
            Ok(serde_json::to_value(record)?)
        }
        "start" => {
            let record = sup.start(msg["name"].as_str().unwrap_or("")).await?;
            Ok(serde_json::to_value(record)?)
        }
        "stop" => {
            sup.stop(msg["name"].as_str().unwrap_or("")).await?;
            Ok(json!({"stopped": true}))
        }
        "destroy" => {
            sup.destroy(msg["name"].as_str().unwrap_or("")).await?;
            Ok(json!({"destroyed": true}))
        }
        "list" => {
            let mut records = state::list().await?;
            for r in &mut records {
                if sup.running(&r.id).await.is_some() {
                    r.state = AgentState::Active;
                }
            }
            Ok(serde_json::to_value(records)?)
        }
        "logs" => {
            let name = msg["name"].as_str().unwrap_or("");
            let tail = msg["tail"].as_u64().unwrap_or(50) as usize;
            let record = state::find_by_name(name).await?;
            let paths = AgentPaths::for_agent(&record.id);
            let events = Journal::read_all(&paths.root).await?;
            let start = events.len().saturating_sub(tail);
            Ok(json!({"events": &events[start..]}))
        }
        "prompt" => {
            sup.prompt(
                msg["name"].as_str().unwrap_or(""),
                msg["message"].as_str().unwrap_or(""),
            )
            .await?;
            Ok(json!({"sent": true}))
        }
        "attach" => {
            // Validated here; the stream switch happens in handle_conn.
            let name = msg["name"].as_str().unwrap_or("");
            state::find_by_name(name).await?;
            Ok(json!({"__attach": true}))
        }
        "ask" => {
            // One-shot question: prompt, wait for the agent to settle, return
            // the final assistant text.
            let name = msg["name"].as_str().unwrap_or("");
            let message = msg["message"].as_str().unwrap_or("");
            let record = state::find_by_name(name).await?;
            let running = sup
                .running(&record.id)
                .await
                .with_context(|| format!("agent '{name}' is not running"))?;
            let mut rx = running.pi.subscribe();
            running.pi.prompt(message).await?;
            let settled = tokio::time::timeout(std::time::Duration::from_secs(300), async {
                while let Ok(ev) = rx.recv().await {
                    let t = ev["type"].as_str().unwrap_or("");
                    if t == "agent_end" || t == "agent_settled" {
                        break;
                    }
                }
            })
            .await;
            if settled.is_err() {
                bail!("agent did not settle within 300s");
            }
            let text = running.pi.get_last_assistant_text().await?;
            Ok(json!({"text": text}))
        }
        "exec" => {
            let name = msg["name"].as_str().unwrap_or("");
            let argv: Vec<String> = serde_json::from_value(msg["argv"].clone())?;
            let record = state::find_by_name(name).await?;
            let running = sup
                .running(&record.id)
                .await
                .with_context(|| format!("agent '{name}' is not running"))?;
            let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
            let (code, stdout, stderr) = running.pi.driver().exec(&argv_refs, None, &[]).await?;
            Ok(json!({"exitCode": code, "stdout": stdout, "stderr": stderr}))
        }
        _ => bail!("unknown command '{cmd}'"),
    }
}
