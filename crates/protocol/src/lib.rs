//! suzerain-protocol: shared wire types for the suzerain control plane and
//! castellan daemons.
//!
//! Everything on the wire between suzerain and castellan is defined here so
//! the two sides can never drift: ALPN identifiers, agent manifests, orders,
//! event-log envelopes, and lifecycle states.

pub mod alpn;
pub mod control;
pub mod event;
pub mod framing;
pub mod manifest;
pub mod order;
pub mod secrets;
pub mod state;
pub mod telemetry;

pub use control::{
    AgentStateEntry, AttachMessage, BundleAck, BundleMessage, Register, RegisterResponse,
    StateReport, StreamHello,
};
pub use event::LogEvent;
pub use manifest::AgentManifest;
pub use order::{Order, OrderAck};
pub use secrets::{SecretBundle, SecretEntry};
pub use state::{AgentState, DaemonInfo};
