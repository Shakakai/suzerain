//! Event-log envelopes. Every event emitted by an agent's pi process (plus
//! castellan lifecycle events) is journaled locally and shipped to suzerain
//! over `suz/logs/0` in this envelope. Suzerain dedupes on `(agent_id, seq)`
//! and acks contiguous ranges so castellan can prune.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEvent {
    pub agent_id: Uuid,
    /// Monotonic per-agent sequence number assigned by the owning castellan.
    pub seq: u64,
    /// RFC 3339 timestamp of local journaling.
    pub at: String,
    /// Event kind: a pi RPC event type (e.g. "message_update") or a castellan
    /// lifecycle event (e.g. "spawned", "crashed", "order_received").
    pub kind: String,
    /// Raw event payload (the pi RPC event JSON, or a lifecycle object).
    pub payload: serde_json::Value,
}

/// A batch of events shipped castellan → suzerain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogBatch {
    pub agent_id: Uuid,
    pub events: Vec<LogEvent>,
}

/// Suzerain's acknowledgement: highest contiguous seq durably stored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogAck {
    pub agent_id: Uuid,
    pub acked_through: u64,
}
