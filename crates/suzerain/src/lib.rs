//! suzerain library core: control-plane subsystems live here; the binary is
//! a thin shell. See docs/PLAN.md §7.

pub mod registry;
pub mod relay;
pub mod scheduler;
pub mod store;
