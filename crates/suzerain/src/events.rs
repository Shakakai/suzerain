//! Global fleet event stream (Suzy / WEB-UI "global SSE channel" option):
//! a process-wide broadcast of lightweight change hints emitted at the
//! store/audit choke points, served as SSE at `GET /api/v1/events`.
//!
//! Events are advisory hints, not a sync protocol: payloads are small
//! (`kind` + a few ids), and a client that lags or reconnects simply
//! refetches the lists it cares about. Kinds emitted today:
//!
//! - `agent`      — agent added/removed/renamed or config changed {id?, name?}
//! - `agent_state`— lifecycle state transition {id, state}
//! - `agent_activity` — daemon-reported busy/idle flip {id, busy}
//! - `daemon`     — daemon registered/approved/online-offline/labels {}
//! - `pending_daemon` — an unapproved enrollment appeared/changed {}
//! - `audit`      — an audit entry was recorded {action}

use serde_json::{json, Value};
use tokio::sync::broadcast;

const CAPACITY: usize = 512;

fn channel() -> &'static broadcast::Sender<Value> {
    static TX: std::sync::OnceLock<broadcast::Sender<Value>> = std::sync::OnceLock::new();
    TX.get_or_init(|| broadcast::channel(CAPACITY).0)
}

/// Emit a fleet event. `kind` and `at` are merged into the payload.
/// Best-effort: zero receivers (or a full ring) is not an error.
pub fn emit(kind: &str, mut payload: Value) {
    let obj = payload.as_object_mut();
    if let Some(obj) = obj {
        obj.insert("kind".into(), json!(kind));
        obj.insert("at".into(), json!(crate::store::castellan_time_now()));
    } else {
        payload = json!({"kind": kind, "at": crate::store::castellan_time_now()});
    }
    let _ = channel().send(payload);
}

/// Subscribe to the fleet event stream.
pub fn subscribe() -> broadcast::Receiver<Value> {
    channel().subscribe()
}
