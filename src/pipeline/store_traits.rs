//! Narrow read/write traits the pipeline layer needs from storage.
//!
//! Pipeline modules (`retrieve.rs`, `session.rs`, …) historically held
//! `&Store` / `Arc<Store>` directly. That made the pipeline a captive
//! of the concrete LanceBackend composition and prevented Phase 2 of
//! the backend-trait work from carving cleanly. This module defines
//! the **minimum** trait surface each pipeline file actually uses; the
//! pipeline now takes `&dyn GraphRead` / `&dyn SessionStore` instead
//! of `&Store`, and `Store` carries the blanket impls.
//!
//! See `docs/backend-coupling.md` §4.5 (QW-5) for the rationale. The
//! scope here is **strictly pipeline-internal** — these traits are
//! not the Phase 2 backend trait surface; that lives elsewhere. The
//! goal is to prove the mechanical-sweep pattern (replace `&Store`
//! with `&dyn TheNarrowTrait`) works at one safe point before
//! committing to it across services / workers / HTTP.

use async_trait::async_trait;

use crate::domain::session::Session;
use crate::storage::{GraphError, StorageError, Store};

/// Graph-side read the relevance ranker (`pipeline::retrieve`) needs.
/// Today: a single 1-hop neighbor expansion to discover memory ids
/// that the anchor set should boost. If retrieve grows multi-hop or
/// time-point graph reads later, those methods land here too — but
/// keep the trait narrow: only what pipeline actually invokes.
#[async_trait]
pub trait GraphRead: Send + Sync {
    /// Expand `anchors` to the set of capsule ids reachable via an
    /// active 1-hop edge. Used to apply a `graph_boost` to ranked
    /// candidates that fall in the anchor neighborhood.
    async fn related_capability_capsule_ids(
        &self,
        anchors: &[String],
    ) -> Result<Vec<String>, GraphError>;
}

/// Session lifecycle the ingest-time session resolver
/// (`pipeline::session`) needs. Mirrors the three Store methods
/// `resolve_session` reaches for.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Most-recent non-closed session for `(tenant, caller_agent)`,
    /// or `None` if the agent has never opened one (or every prior
    /// session has been explicitly closed).
    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError>;

    /// Open a new session row. `now` is the 20-digit ms timestamp
    /// that becomes both `started_at` and `last_seen_at`.
    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError>;

    /// Mark a session row as ended at `ended_at`. Idempotent — closing
    /// an already-closed session is a no-op at the storage layer.
    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError>;
}

#[async_trait]
impl GraphRead for Store {
    async fn related_capability_capsule_ids(
        &self,
        anchors: &[String],
    ) -> Result<Vec<String>, GraphError> {
        Store::related_capability_capsule_ids(self, anchors).await
    }
}

#[async_trait]
impl SessionStore for Store {
    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        Store::latest_active_session(self, tenant, caller_agent).await
    }

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        Store::open_session(self, session_id, tenant, caller_agent, now).await
    }

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError> {
        Store::close_session(self, session_id, ended_at).await
    }
}
