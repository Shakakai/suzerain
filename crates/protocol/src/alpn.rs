//! ALPN identifiers for the iroh protocols spoken between suzerain and
//! castellan. One iroh `Router` per node multiplexes these.

/// Targeted orders suzerain → castellan, with acks (request/response bi-streams).
pub const CONTROL: &[u8] = b"suz/control/0";

/// Reliable event-log shipping castellan → suzerain (acked, resumable).
pub const LOGS: &[u8] = b"suz/logs/0";

/// Session attach relay: cli ↔ suzerain ↔ castellan.
pub const ATTACH: &[u8] = b"suz/attach/0";

/// Operator channel: desktop clients (Suzy) ↔ suzerain. Public-key
/// authorized (the `[operator] allow` list); works anywhere iroh reaches.
pub const OPERATOR: &[u8] = b"suz/operator/0";

/// Agent bundle streaming suzerain → castellan for restore-on-any-server.
pub const RESTORE: &[u8] = b"suz/restore/0";

/// Well-known iroh-gossip topic for fleet presence/announcements.
/// (Best-effort signals only — never event logs.)
pub const FLEET_TOPIC: [u8; 32] = *b"suzerain-fleet-v1_______________";
