//! Backend-agnostic session + episode lifecycle — Phase 3 sub-trait.
//!
//! Sessions are per-(tenant, caller_agent) idle-timeout buckets;
//! episodes are workflow snapshots derived from session activity.
//! They share the same lifetime + tenant scope so they're grouped
//! into one sub-trait (per `docs/backend-coupling.md` §3.1 grouping).
//!
//! Note: a narrower [`crate::pipeline::store_traits::SessionStore`]
//! exists at the pipeline layer (3 methods, used by
//! `pipeline::session::resolve_session`). That trait is a subset of
//! this one; Phase 5 cleanup may unify them by having the pipeline
//! trait re-export from here.

use async_trait::async_trait;

use crate::domain::episode::EpisodeRecord;
use crate::domain::session::Session;
use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Bump `last_seen_at` on an existing active session. No-op if
    /// the session doesn't exist (silently).
    async fn touch_session(
        &self,
        session_id: &str,
        last_active_at: &str,
    ) -> Result<(), StorageError>;

    /// Open a new session row. `now` becomes both `started_at` and
    /// `last_seen_at`.
    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError>;

    /// Mark a session as ended at `ended_at`. Idempotent — closing
    /// an already-closed session is a no-op.
    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError>;

    /// Most-recent non-closed session for `(tenant, caller_agent)`,
    /// or `None` if every prior session has been closed.
    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError>;

    /// Append an episode row (workflow snapshot).
    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError>;

    /// Episodes for `tenant` whose `outcome = success`. Used by the
    /// workflow extraction pipeline.
    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError>;
}

#[async_trait]
impl SessionStore for Store {
    async fn touch_session(
        &self,
        session_id: &str,
        last_active_at: &str,
    ) -> Result<(), StorageError> {
        Store::touch_session(self, session_id, last_active_at).await
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

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        Store::latest_active_session(self, tenant, caller_agent).await
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        Store::insert_episode(self, episode).await
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        Store::list_successful_episodes_for_tenant(self, tenant).await
    }
}
