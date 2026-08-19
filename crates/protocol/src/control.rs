//! Control-channel messages on the `suz/control/0` connection.
//!
//! One long-lived iroh connection per daemon (castellan dials suzerain after
//! approval). All channels are bi-streams on that single connection; every
//! stream starts with a labeled hello message. This deliberately avoids
//! multiple concurrent connections between the same peer pair (see
//! docs/PHASE0-FINDINGS.md, spike b).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::state::DaemonInfo;

/// First message on the daemon's primary stream: registration handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Register {
    pub info: DaemonInfo,
}

/// Suzerain's registration response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub accepted: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// Stream labels: the first message on every bi-stream opened on the control
/// connection (after the register stream).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "stream", rename_all = "snake_case")]
pub enum StreamHello {
    /// Daemon → control plane: event-log batch stream.
    Logs { agent_id: Uuid },
    /// Control plane → daemon: session attach relay for an agent.
    Attach { agent_id: Uuid },
    /// Daemon → control plane: agent state reports (snapshot + transitions).
    StateReport,
    /// Daemon → control plane: pull a freshly-sliced secret bundle for an
    /// agent (G7: bundles are re-pulled, never persisted on the daemon).
    Secrets { agent_id: Uuid },
    /// Daemon → control plane: agent bundle upload (session files + pi-home)
    /// for centralized restore.
    BundleUpload { agent_id: Uuid },
    /// Control plane → daemon: restore bundle for an agent.
    Restore { agent_id: Uuid },
    /// Control plane → daemon: interactive shell (pty) into an agent's VM.
    Shell { agent_id: Uuid },
}

/// Messages on the shell stream (both directions after the hello). Byte
/// payloads are base64 so the stream stays JSONL like every other channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ShellMessage {
    /// Terminal bytes, both directions (client input / pty output).
    Data { data: String },
    /// Client → daemon: pty resize.
    Resize { cols: u16, rows: u16 },
    /// Daemon → client: the shell process exited.
    Exit { code: i64 },
    /// Daemon → client: "shell" acknowledges the handshake (the agent is
    /// running and the pty is live); anything else is an error.
    Notice { message: String },
}

/// Bundle transfer messages (both upload and restore directions).
/// Files are base64-chunked; `last` marks the final chunk of a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BundleMessage {
    Start {
        manifest: Box<crate::manifest::AgentManifest>,
        /// Guest path of the pi session file to resume, if known.
        session_file: Option<String>,
        /// Secrets re-sliced at restore time (never persisted in bundles).
        #[serde(default)]
        secrets: Option<crate::secrets::SecretBundle>,
    },
    File {
        /// Path relative to the agent's guest dir (e.g. "sessions/x.jsonl").
        path: String,
        data: String,
        last: bool,
        /// SHA-256 (hex) of the decoded file content (G8 integrity).
        #[serde(default)]
        sha256: Option<String>,
    },
    End,
}

/// Receiver's reply after BundleMessage::End.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleAck {
    pub success: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// One agent's state as known by the reporting daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStateEntry {
    pub agent_id: Uuid,
    pub name: String,
    pub state: crate::state::AgentState,
    /// Seconds since the agent's last meaningful activity, computed
    /// daemon-side at report time (clock-skew-immune: the control plane
    /// extrapolates from its receipt time rather than comparing
    /// cross-machine timestamps).
    #[serde(default)]
    pub idle_secs: Option<u64>,
    /// Ground truth from the daemon: a turn is in flight. An agent that is
    /// busy is never an auto-suspend/preemption candidate.
    #[serde(default)]
    pub busy: Option<bool>,
    /// The agent's current pi session file (guest path). Sessions rotate
    /// on every suspend; the control plane tracks session eras from these
    /// reports.
    #[serde(default)]
    pub session_file: Option<String>,
}

/// Daemon → control plane: agent state report. The daemon sends a full
/// snapshot right after registration, then incremental reports as agents
/// transition. Suzerain applies entries only for agents whose registry row
/// belongs to the reporting daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateReport {
    pub agents: Vec<AgentStateEntry>,
    /// True for the post-registration snapshot: suzerain may treat owned
    /// agents MISSING from the report as lost (mark them Failed).
    #[serde(default)]
    pub full: bool,
}

/// Messages on the attach stream (both directions after the hello).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachMessage {
    /// Operator/cli → agent: a prompt.
    Prompt { message: String },
    /// Operator/cli → agent: steer mid-run.
    Steer { message: String },
    /// Operator/cli → agent: follow-up after the current run.
    FollowUp { message: String },
    /// Operator/cli → agent: abort the current turn.
    Abort,
    /// Agent → operator: a raw pi RPC event.
    Event { event: serde_json::Value },
    /// Daemon → operator: attach-level notice. "attached" acknowledges the
    /// attach handshake (the agent is running and accepting input); anything
    /// else is an error/explanation (e.g. "agent 'x' is not running"). Lets
    /// senders fail loudly instead of silently swallowing messages.
    Notice { message: String },
}

// ── operator channel (suz/operator/0): Suzy ↔ suzerain ───────────────────

/// First frame on every operator bi-stream. One connection multiplexes any
/// number of streams; each stream carries exactly one op.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OperatorHello {
    /// Unary call against the operator API: executed in-process against the
    /// same router the HTTP API serves (single source of truth). `path` is
    /// the /api/v1 path (query string allowed). Replies with one
    /// `OperatorFrame::Reply`.
    Rest {
        method: String,
        path: String,
        #[serde(default)]
        body: Option<serde_json::Value>,
    },
    /// Streaming GET (SSE endpoints): the response body is relayed as
    /// `OperatorFrame::Chunk` (base64) frames until `End`.
    Stream { path: String },
    /// Interactive pty shell into an agent's VM. After the hello, the
    /// stream carries `ShellMessage` frames in both directions.
    Shell { name: String },
}

/// Server → client frames after an `OperatorHello`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperatorFrame {
    /// Unary rest reply (`body` is the API's JSON, or a string if the
    /// response wasn't JSON).
    Reply {
        status: u16,
        body: serde_json::Value,
    },
    /// One chunk of a streaming response body (base64).
    Chunk { data: String },
    /// Stream finished cleanly.
    End,
    /// Op failed before/while running.
    Error { message: String },
}
