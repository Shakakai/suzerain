//! suzerain-protocol: shared wire types for the suzerain control plane and
//! castellan daemons.
//!
//! Everything on the wire between suzerain and castellan is defined here so
//! the two sides can never drift: ALPN identifiers, agent manifests, orders,
//! event-log envelopes, and lifecycle states.

pub mod alpn;
pub mod event;
pub mod framing;
pub mod manifest;
pub mod order;
pub mod state;

pub use event::LogEvent;
pub use manifest::AgentManifest;
pub use order::{Order, OrderAck};
pub use state::{AgentState, DaemonInfo};
