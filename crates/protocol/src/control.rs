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
    /// Daemon → control plane: agent bundle upload (session files + pi-home)
    /// for centralized restore.
    BundleUpload { agent_id: Uuid },
    /// Control plane → daemon: restore bundle for an agent.
    Restore { agent_id: Uuid },
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
    },
    File {
        /// Path relative to the agent's guest dir (e.g. "sessions/x.jsonl").
        path: String,
        data: String,
        last: bool,
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

/// Messages on the attach stream (both directions after the hello).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachMessage {
    /// Operator/cli → agent: a prompt.
    Prompt { message: String },
    /// Agent → operator: a raw pi RPC event.
    Event { event: serde_json::Value },
}
