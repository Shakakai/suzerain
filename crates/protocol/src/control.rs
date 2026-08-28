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

/// Version of the suzerain<->castellan control protocol (Register handshake,
/// Order/OrderAck, StreamHello sub-streams). Bump when a change would break
/// an older peer talking to a newer one; `Register`/`RegisterResponse` carry
/// this so a version mismatch fails cleanly at the handshake instead of
/// deeper in the protocol. There is no deployed fleet to keep compatible
/// today — this exists purely so a future incompatible change has a
/// negotiation point to land on.
pub const PROTOCOL_VERSION: u32 = 1;

/// First message on the daemon's primary stream: registration handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Register {
    pub info: DaemonInfo,
    /// The daemon's `PROTOCOL_VERSION`. Defaults to 1 on deserialize so a
    /// peer that predates this field (none exist today) is still readable.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
}

/// Suzerain's registration response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub accepted: bool,
    #[serde(default)]
    pub message: Option<String>,
    /// The control plane's own `PROTOCOL_VERSION`, so a rejected/mismatched
    /// daemon can log what it was rejected against.
    #[serde(default = "default_protocol_version")]
    pub protocol_version: u32,
}

fn default_protocol_version() -> u32 {
    1
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
    /// Control plane → daemon: dispatch one order. Opened fresh per order
    /// (rather than reusing the register stream) so a slow order for one
    /// agent (e.g. a 15-minute provision) can't block the ack for an
    /// unrelated order on the same daemon connection — each order/ack pair
    /// gets its own stream instead of sharing one FIFO-ordered pipe.
    Order,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DaemonInfo;

    fn daemon_info() -> DaemonInfo {
        DaemonInfo {
            endpoint_id: "abc123".to_string(),
            hostname: "host-1".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            labels: Default::default(),
            max_agents: 4,
            agents: vec![],
            capacity: Default::default(),
            usage: Default::default(),
        }
    }

    #[test]
    fn register_round_trips_and_defaults_protocol_version() {
        let reg = Register {
            info: daemon_info(),
            protocol_version: 3,
        };
        let json = serde_json::to_string(&reg).unwrap();
        let back: Register = serde_json::from_str(&json).unwrap();
        assert_eq!(back.protocol_version, 3);
        assert_eq!(back.info.hostname, "host-1");

        // A peer that predates the field (no protocol_version key) still
        // deserializes, defaulting to 1.
        let legacy = serde_json::json!({ "info": {
            "endpoint_id": "abc123",
            "hostname": "host-1",
            "os": "linux",
            "arch": "x86_64",
            "max_agents": 4
        }});
        let back: Register = serde_json::from_value(legacy).unwrap();
        assert_eq!(back.protocol_version, 1);
    }

    #[test]
    fn register_response_defaults_message_and_version() {
        let json = serde_json::json!({ "accepted": false });
        let resp: RegisterResponse = serde_json::from_value(json).unwrap();
        assert!(!resp.accepted);
        assert_eq!(resp.message, None);
        assert_eq!(resp.protocol_version, 1);
    }

    #[test]
    fn stream_hello_tags_by_stream_field() {
        let agent_id = Uuid::new_v4();
        let hello = StreamHello::Logs { agent_id };
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["stream"], "logs");
        assert_eq!(json["agent_id"], agent_id.to_string());

        let hello = StreamHello::Order;
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["stream"], "order");

        let back: StreamHello = serde_json::from_value(json).unwrap();
        assert!(matches!(back, StreamHello::Order));
    }

    #[test]
    fn stream_hello_rejects_unknown_variant() {
        let json = serde_json::json!({ "stream": "not_a_real_stream" });
        assert!(serde_json::from_value::<StreamHello>(json).is_err());
    }

    #[test]
    fn shell_message_round_trips_all_variants() {
        let cases = vec![
            ShellMessage::Data {
                data: "aGVsbG8=".to_string(),
            },
            ShellMessage::Resize { cols: 80, rows: 24 },
            ShellMessage::Exit { code: 1 },
            ShellMessage::Notice {
                message: "shell".to_string(),
            },
        ];
        for msg in cases {
            let json = serde_json::to_string(&msg).unwrap();
            let back: ShellMessage = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{msg:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn bundle_message_start_defaults_secrets_to_none() {
        let json = serde_json::json!({
            "type": "start",
            "manifest": {
                "name": "a1",
                "harness": { "type": "pi", "version": "0.1.0" },
                "model": { "provider": "anthropic", "id": "x" }
            },
            "session_file": null
        });
        let msg: BundleMessage = serde_json::from_value(json).unwrap();
        match msg {
            BundleMessage::Start {
                session_file,
                secrets,
                ..
            } => {
                assert_eq!(session_file, None);
                assert!(secrets.is_none());
            }
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn bundle_message_file_defaults_sha256_to_none() {
        let json = serde_json::json!({
            "type": "file",
            "path": "sessions/x.jsonl",
            "data": "aGVsbG8=",
            "last": true
        });
        let msg: BundleMessage = serde_json::from_value(json).unwrap();
        match msg {
            BundleMessage::File { sha256, last, .. } => {
                assert_eq!(sha256, None);
                assert!(last);
            }
            other => panic!("expected File, got {other:?}"),
        }
    }

    #[test]
    fn bundle_ack_round_trips() {
        let ack = BundleAck {
            success: true,
            message: None,
        };
        let json = serde_json::to_string(&ack).unwrap();
        let back: BundleAck = serde_json::from_str(&json).unwrap();
        assert!(back.success);
        assert_eq!(back.message, None);
    }

    #[test]
    fn agent_state_entry_defaults_optional_fields() {
        let agent_id = Uuid::new_v4();
        let json = serde_json::json!({
            "agent_id": agent_id,
            "name": "agent-1",
            "state": "active"
        });
        let entry: AgentStateEntry = serde_json::from_value(json).unwrap();
        assert_eq!(entry.agent_id, agent_id);
        assert_eq!(entry.idle_secs, None);
        assert_eq!(entry.busy, None);
        assert_eq!(entry.session_file, None);
    }

    #[test]
    fn state_report_defaults_full_to_false() {
        let json = serde_json::json!({ "agents": [] });
        let report: StateReport = serde_json::from_value(json).unwrap();
        assert!(!report.full);
        assert!(report.agents.is_empty());
    }

    #[test]
    fn attach_message_round_trips_and_tags_correctly() {
        let msg = AttachMessage::Prompt {
            message: "hi".to_string(),
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "prompt");
        assert_eq!(json["message"], "hi");

        let msg = AttachMessage::Abort;
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "abort");
        let back: AttachMessage = serde_json::from_value(json).unwrap();
        assert!(matches!(back, AttachMessage::Abort));

        let msg = AttachMessage::Event {
            event: serde_json::json!({"kind": "message_update"}),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: AttachMessage = serde_json::from_str(&json).unwrap();
        match back {
            AttachMessage::Event { event } => assert_eq!(event["kind"], "message_update"),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn operator_hello_rest_defaults_body_to_none() {
        let json = serde_json::json!({
            "op": "rest",
            "method": "GET",
            "path": "/api/v1/agents"
        });
        let hello: OperatorHello = serde_json::from_value(json).unwrap();
        match hello {
            OperatorHello::Rest { method, path, body } => {
                assert_eq!(method, "GET");
                assert_eq!(path, "/api/v1/agents");
                assert_eq!(body, None);
            }
            other => panic!("expected Rest, got {other:?}"),
        }
    }

    #[test]
    fn operator_frame_round_trips_all_variants() {
        let frame = OperatorFrame::Reply {
            status: 200,
            body: serde_json::json!({"ok": true}),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: OperatorFrame = serde_json::from_str(&json).unwrap();
        match back {
            OperatorFrame::Reply { status, body } => {
                assert_eq!(status, 200);
                assert_eq!(body["ok"], true);
            }
            other => panic!("expected Reply, got {other:?}"),
        }

        let frame = OperatorFrame::Error {
            message: "boom".to_string(),
        };
        let json = serde_json::to_value(&frame).unwrap();
        assert_eq!(json["type"], "error");
        assert_eq!(json["message"], "boom");
    }
}
