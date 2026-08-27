//! suzerain library core: control-plane subsystems. See docs/PLAN.md §7.

pub mod actions;
pub mod audit;
pub mod bundle;
pub mod catalog;
pub mod chat_store;
pub mod control;
pub mod events;
pub mod file_locks;
pub mod identity;
pub mod lifecycle;
pub mod operator;
pub mod pi_packages;
pub mod registry;
pub mod relay;
pub mod retention;
pub mod scheduler;
pub mod secrets;
pub mod standalone;
pub mod store;
pub mod wake;
pub mod web;
pub mod web_session;
