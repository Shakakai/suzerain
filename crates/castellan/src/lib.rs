//! castellan library core: the daemon's subsystems live here; the binary is
//! a thin shell. See docs/PLAN.md §6.

pub mod daemon;
pub mod driver;
pub mod journal;
pub mod provision;
pub mod rpc;
pub mod state;
pub mod supervisor;
