//! Backend-agnostic transcript archive — Phase 3 sub-trait.
//!
//! Covers the parallel-to-capsules pipeline for verbatim conversation
//! archival. Writes go to `conversation_messages`; reads cover
//! per-session listing, cursor-paged range queries, ANN/BM25 recall,
//! context-window stitching, and anchor lookups.
//!
//! **LANCE-SPECIFIC bits**:
//! - `bm25_transcript_candidates` uses `lance_fts(...)`.
//! - `semantic_search_transcripts` uses `lance_vector_search(...)`.
//! - `create_conversation_message(s)` also enqueues
//!   `transcript_embedding_jobs` inline when blocks are
//!   embed-eligible — the trait surface hides that fan-out.
//!
//! See `docs/backend-coupling.md` §3.1 + §6.4.

use async_trait::async_trait;

use crate::domain::ConversationMessage;
use crate::storage::types::{ContextWindow, StorageError, TranscriptSessionSummary};
use crate::storage::Store;

#[async_trait]
pub trait TranscriptStore: Send + Sync {
    // ── Writes ──────────────────────────────────────────────────────

    /// Append a single transcript block. Includes a dedup probe on
    /// `(message_block_id, ...)`. When the block is embed-eligible
    /// the backend also enqueues a transcript embedding job in the
    /// same call (fan-out hidden inside the impl).
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError>;

    /// Multi-row variant. Returns the number of rows that actually
    /// landed (input minus dedup-skipped). No-op when empty.
    async fn create_conversation_messages(
        &self,
        msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError>;

    // ── Per-session reads ───────────────────────────────────────────

    /// All messages in one session, chronologically ordered. Used
    /// by `GET /transcripts?session_id=…` HTTP path.
    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    /// Cursor-paged variant. Returns `(rows, has_more)`. Cursor is
    /// `(created_at, line_number, block_index)` — composite to handle
    /// ms-collisions on `created_at`. Supports time-range filters
    /// (`since` / `until`), role filter, block-type filter.
    #[allow(clippy::too_many_arguments)]
    async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError>;

    /// Aggregate per-session summary for the admin transcript-drawer
    /// view. One row per `(tenant, session_id)` with block-count,
    /// first/last timestamps, caller-agent.
    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError>;

    // ── Cross-session reads ─────────────────────────────────────────

    /// Time-range query across sessions for a tenant. Same cursor
    /// shape as the per-session paged variant.
    #[allow(clippy::too_many_arguments)]
    async fn list_conversation_messages_in_range(
        &self,
        tenant: &str,
        time_from: Option<&str>,
        time_to: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError>;

    /// Bulk fetch by `message_block_id` list, scoped to `tenant`.
    /// Empty `ids` short-circuits.
    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    // ── Recall / context ────────────────────────────────────────────

    /// Context-window stitching: `k_before` blocks before + the
    /// primary block + `k_after` blocks after, same-session. Filters
    /// tool blocks unless `include_tool_blocks=true`.
    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError>;

    /// Top-k anchor candidate block ids for `session_id` — used by
    /// transcript recall to pick high-signal anchor blocks for the
    /// session co-occurrence boost.
    async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError>;

    /// Most-recent conversation messages for `tenant`, ordered
    /// `created_at DESC`, bounded by `limit`. Empty-query fallback
    /// for transcript search.
    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    /// Top-k BM25 candidates over `conversation_messages.content`.
    /// **LANCE-SPECIFIC**: uses `lance_fts(...)`.
    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    /// Top-k semantic candidates over
    /// `conversation_message_embeddings.embedding`. Returns
    /// `(message, cosine_similarity)`.
    /// **LANCE-SPECIFIC**: uses `lance_vector_search(...)`.
    async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError>;
}

#[async_trait]
impl TranscriptStore for Store {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        Store::create_conversation_message(self, msg).await
    }

    async fn create_conversation_messages(
        &self,
        msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        Store::create_conversation_messages(self, msgs).await
    }

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        Store::get_conversation_messages_by_session(self, tenant, session_id).await
    }

    async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        Store::get_conversation_messages_by_session_paged(
            self, tenant, session_id, since, until, role, block_type, cursor, limit,
        )
        .await
    }

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        Store::list_transcript_sessions(self, tenant).await
    }

    async fn list_conversation_messages_in_range(
        &self,
        tenant: &str,
        time_from: Option<&str>,
        time_to: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        Store::list_conversation_messages_in_range(
            self, tenant, time_from, time_to, role, block_type, cursor, limit,
        )
        .await
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        Store::fetch_conversation_messages_by_ids(self, tenant, ids).await
    }

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        Store::context_window_for_block(
            self,
            tenant,
            primary_id,
            k_before,
            k_after,
            include_tool_blocks,
        )
        .await
    }

    async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        Store::anchor_session_candidates(self, tenant, session_id, k).await
    }

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        Store::recent_conversation_messages(self, tenant, limit).await
    }

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        Store::bm25_transcript_candidates(self, tenant, query, k).await
    }

    async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        Store::semantic_search_transcripts(self, tenant, query_embedding, limit).await
    }
}
