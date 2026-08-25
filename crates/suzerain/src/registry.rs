//! `Registry` — the pluggable storage seam for daemons, agents, sessions,
//! and the pending-message queue.
//!
//! Per docs/UNIFIED-AGENT-API-DESIGN.md §4.2: this trait is a faithful
//! mirror of [`crate::store::Store`]'s existing public methods, not a new
//! design — the goal of this module is to make the *existing* concrete
//! implementation reachable through a trait object (`Arc<dyn Registry>`)
//! with zero behavior change, so the SQL `Store` stops being the only thing
//! that can ever sit behind [`crate::control::ControlPlane`].
//!
//! `Store` keeps every method as an inherent impl (in `store.rs`, unchanged)
//! and additionally implements this trait by delegating to those inherent
//! methods one-for-one (see the `impl Registry for Store` block below).
//! Rust resolves `self.foo()` against a type's *inherent* methods before its
//! trait methods, so the delegating calls below reach `Store`'s real
//! implementations rather than recursing into this trait — this is the
//! standard "wrap without moving the code" pattern for exactly this kind of
//! extraction.

use anyhow::Result;
use async_trait::async_trait;
use suzerain_protocol::state::{AgentState, DaemonInfo};
use uuid::Uuid;

use crate::store::{AgentRow, AgentSessionRow, DaemonRow, PendingMessage, Store};

/// The single source of truth for daemons, agents, sessions, and the
/// pending-message queue — today always backed by [`Store`] (sqlite or
/// postgres), reachable as `Arc<dyn Registry>` so an alternate backend can
/// be substituted later without touching any call site.
#[async_trait]
pub trait Registry: Send + Sync {
    // -- daemons --
    async fn approve_daemon(&self, endpoint_id: &str) -> Result<()>;
    async fn daemon_approved(&self, endpoint_id: &str) -> Result<bool>;
    async fn upsert_daemon(&self, info: &DaemonInfo, online: bool) -> Result<()>;
    async fn set_daemon_usage(&self, endpoint_id: &str, usage_json: &str) -> Result<()>;
    async fn set_label_overrides(&self, endpoint_id: &str, overrides_json: &str) -> Result<()>;
    async fn set_daemon_online(&self, endpoint_id: &str, online: bool) -> Result<()>;
    async fn set_all_daemons_offline(&self) -> Result<()>;
    async fn list_daemons(&self) -> Result<Vec<DaemonRow>>;
    async fn delete_daemon(&self, endpoint_id: &str) -> Result<()>;

    // -- pending daemon enrollment --
    async fn upsert_pending_daemon(&self, info: &DaemonInfo) -> Result<()>;
    async fn delete_pending_daemon(&self, endpoint_id: &str) -> Result<()>;
    async fn list_pending_daemons(&self) -> Result<Vec<serde_json::Value>>;

    // -- agents --
    async fn create_agent(&self, row: &AgentRow) -> Result<()>;
    async fn update_agent_state(&self, id: &Uuid, state: AgentState) -> Result<()>;
    async fn set_agent_session_file(&self, id: &Uuid, session_file: &str) -> Result<()>;
    async fn set_agent_daemon(&self, id: &Uuid, daemon_endpoint_id: &str) -> Result<()>;
    async fn delete_agent(&self, id: &Uuid) -> Result<()>;
    async fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentRow>>;
    async fn get_agent(&self, id: &Uuid) -> Result<Option<AgentRow>>;
    async fn list_agents(&self) -> Result<Vec<AgentRow>>;
    async fn set_agent_activity(
        &self,
        id: &Uuid,
        idle_secs: Option<u64>,
        busy: Option<bool>,
    ) -> Result<()>;
    async fn set_needs_attention(&self, id: &Uuid, needs: bool) -> Result<()>;
    async fn set_auto_suspend_override(&self, id: &Uuid, value: Option<&str>) -> Result<()>;
    async fn set_agent_woke_at(&self, id: &Uuid) -> Result<()>;

    // -- sessions --
    async fn start_agent_session(&self, agent_id: &Uuid, session_file: &str) -> Result<()>;
    async fn ensure_open_session(
        &self,
        agent_id: &Uuid,
        session_file: &str,
        fallback_start: &str,
    ) -> Result<()>;
    async fn list_agent_sessions(&self, agent_id: &Uuid) -> Result<Vec<AgentSessionRow>>;

    // -- pending message queue (wake/deliver) --
    async fn enqueue_message(&self, agent_id: &Uuid, body: &str) -> Result<i64>;
    async fn pending_messages(&self, agent_id: &Uuid) -> Result<Vec<PendingMessage>>;
    async fn agents_with_pending_messages(&self) -> Result<Vec<Uuid>>;
    async fn set_message_status(
        &self,
        ids: &[i64],
        status: &str,
        error: Option<&str>,
    ) -> Result<()>;
    async fn prune_messages(&self, days: u32) -> Result<()>;

    // -- log-shipping ack watermark --
    async fn acked_through(&self, agent_id: &Uuid) -> Result<u64>;
    async fn set_acked_through(&self, agent_id: &Uuid, seq: u64) -> Result<()>;
}

#[async_trait]
impl Registry for Store {
    async fn approve_daemon(&self, endpoint_id: &str) -> Result<()> {
        self.approve_daemon(endpoint_id).await
    }
    async fn daemon_approved(&self, endpoint_id: &str) -> Result<bool> {
        self.daemon_approved(endpoint_id).await
    }
    async fn upsert_daemon(&self, info: &DaemonInfo, online: bool) -> Result<()> {
        self.upsert_daemon(info, online).await
    }
    async fn set_daemon_usage(&self, endpoint_id: &str, usage_json: &str) -> Result<()> {
        self.set_daemon_usage(endpoint_id, usage_json).await
    }
    async fn set_label_overrides(&self, endpoint_id: &str, overrides_json: &str) -> Result<()> {
        self.set_label_overrides(endpoint_id, overrides_json).await
    }
    async fn set_daemon_online(&self, endpoint_id: &str, online: bool) -> Result<()> {
        self.set_daemon_online(endpoint_id, online).await
    }
    async fn set_all_daemons_offline(&self) -> Result<()> {
        self.set_all_daemons_offline().await
    }
    async fn list_daemons(&self) -> Result<Vec<DaemonRow>> {
        self.list_daemons().await
    }
    async fn delete_daemon(&self, endpoint_id: &str) -> Result<()> {
        self.delete_daemon(endpoint_id).await
    }
    async fn upsert_pending_daemon(&self, info: &DaemonInfo) -> Result<()> {
        self.upsert_pending_daemon(info).await
    }
    async fn delete_pending_daemon(&self, endpoint_id: &str) -> Result<()> {
        self.delete_pending_daemon(endpoint_id).await
    }
    async fn list_pending_daemons(&self) -> Result<Vec<serde_json::Value>> {
        self.list_pending_daemons().await
    }
    async fn create_agent(&self, row: &AgentRow) -> Result<()> {
        self.create_agent(row).await
    }
    async fn update_agent_state(&self, id: &Uuid, state: AgentState) -> Result<()> {
        self.update_agent_state(id, state).await
    }
    async fn set_agent_session_file(&self, id: &Uuid, session_file: &str) -> Result<()> {
        self.set_agent_session_file(id, session_file).await
    }
    async fn set_agent_daemon(&self, id: &Uuid, daemon_endpoint_id: &str) -> Result<()> {
        self.set_agent_daemon(id, daemon_endpoint_id).await
    }
    async fn delete_agent(&self, id: &Uuid) -> Result<()> {
        self.delete_agent(id).await
    }
    async fn get_agent_by_name(&self, name: &str) -> Result<Option<AgentRow>> {
        self.get_agent_by_name(name).await
    }
    async fn get_agent(&self, id: &Uuid) -> Result<Option<AgentRow>> {
        self.get_agent(id).await
    }
    async fn list_agents(&self) -> Result<Vec<AgentRow>> {
        self.list_agents().await
    }
    async fn set_agent_activity(
        &self,
        id: &Uuid,
        idle_secs: Option<u64>,
        busy: Option<bool>,
    ) -> Result<()> {
        self.set_agent_activity(id, idle_secs, busy).await
    }
    async fn set_needs_attention(&self, id: &Uuid, needs: bool) -> Result<()> {
        self.set_needs_attention(id, needs).await
    }
    async fn set_auto_suspend_override(&self, id: &Uuid, value: Option<&str>) -> Result<()> {
        self.set_auto_suspend_override(id, value).await
    }
    async fn set_agent_woke_at(&self, id: &Uuid) -> Result<()> {
        self.set_agent_woke_at(id).await
    }
    async fn start_agent_session(&self, agent_id: &Uuid, session_file: &str) -> Result<()> {
        self.start_agent_session(agent_id, session_file).await
    }
    async fn ensure_open_session(
        &self,
        agent_id: &Uuid,
        session_file: &str,
        fallback_start: &str,
    ) -> Result<()> {
        self.ensure_open_session(agent_id, session_file, fallback_start)
            .await
    }
    async fn list_agent_sessions(&self, agent_id: &Uuid) -> Result<Vec<AgentSessionRow>> {
        self.list_agent_sessions(agent_id).await
    }
    async fn enqueue_message(&self, agent_id: &Uuid, body: &str) -> Result<i64> {
        self.enqueue_message(agent_id, body).await
    }
    async fn pending_messages(&self, agent_id: &Uuid) -> Result<Vec<PendingMessage>> {
        self.pending_messages(agent_id).await
    }
    async fn agents_with_pending_messages(&self) -> Result<Vec<Uuid>> {
        self.agents_with_pending_messages().await
    }
    async fn set_message_status(
        &self,
        ids: &[i64],
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        self.set_message_status(ids, status, error).await
    }
    async fn prune_messages(&self, days: u32) -> Result<()> {
        self.prune_messages(days).await
    }
    async fn acked_through(&self, agent_id: &Uuid) -> Result<u64> {
        self.acked_through(agent_id).await
    }
    async fn set_acked_through(&self, agent_id: &Uuid, seq: u64) -> Result<()> {
        self.set_acked_through(agent_id, seq).await
    }
}
