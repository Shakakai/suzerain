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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(seq: u64) -> LogEvent {
        LogEvent {
            agent_id: Uuid::new_v4(),
            seq,
            at: "2026-08-27T00:00:00Z".to_string(),
            kind: "message_update".to_string(),
            payload: serde_json::json!({"text": "hello"}),
        }
    }

    #[test]
    fn log_event_round_trips() {
        let event = sample_event(42);
        let json = serde_json::to_string(&event).unwrap();
        let back: LogEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, event.agent_id);
        assert_eq!(back.seq, 42);
        assert_eq!(back.kind, "message_update");
        assert_eq!(back.payload["text"], "hello");
    }

    #[test]
    fn log_batch_round_trips_multiple_events() {
        let agent_id = Uuid::new_v4();
        let batch = LogBatch {
            agent_id,
            events: vec![sample_event(1), sample_event(2), sample_event(3)],
        };
        let json = serde_json::to_string(&batch).unwrap();
        let back: LogBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, agent_id);
        assert_eq!(back.events.len(), 3);
        assert_eq!(back.events[2].seq, 3);
    }

    #[test]
    fn log_batch_round_trips_empty_events() {
        let batch = LogBatch {
            agent_id: Uuid::new_v4(),
            events: vec![],
        };
        let json = serde_json::to_string(&batch).unwrap();
        let back: LogBatch = serde_json::from_str(&json).unwrap();
        assert!(back.events.is_empty());
    }

    #[test]
    fn log_ack_round_trips() {
        let agent_id = Uuid::new_v4();
        let ack = LogAck {
            agent_id,
            acked_through: 100,
        };
        let json = serde_json::to_string(&ack).unwrap();
        let back: LogAck = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, agent_id);
        assert_eq!(back.acked_through, 100);
    }

    #[test]
    fn log_event_missing_required_field_fails() {
        // `kind` is required (no #[serde(default)]); malformed input from a
        // peer must fail to deserialize rather than silently substituting.
        let json = serde_json::json!({
            "agent_id": Uuid::new_v4(),
            "seq": 1,
            "at": "2026-08-27T00:00:00Z",
            "payload": {}
        });
        assert!(serde_json::from_value::<LogEvent>(json).is_err());
    }
}
