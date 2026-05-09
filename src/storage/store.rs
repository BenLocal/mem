//! Top-level storage handle. Composes [`LanceStore`] (writes) and
//! [`DuckDbQuery`] (reads) behind a single owner so the service layer
//! holds one `Arc<Store>` instead of two correlated handles.
//!
//! Architecture:
//!
//! ```text
//!   ┌─────────────────────── Store ───────────────────────┐
//!   │                                                     │
//!   │  writes ──► LanceStore ──► .lance/ on-disk datasets │
//!   │                                  ▲                  │
//!   │  reads  ──► DuckDbQuery ─────────┘ (ATTACHed)       │
//!   │                                                     │
//!   └─────────────────────────────────────────────────────┘
//! ```
//!
//! Both handles point at the **same** lance directory. Writes go
//! through LanceDB's Rust API (so the `EmbeddingFunction` adapter
//! can auto-embed at write time when a provider is configured); reads
//! go through DuckDB SQL via the `lance` core extension.
//!
//! ### Snapshot caching, and how `Store` works around it
//!
//! The lance DuckDB extension caches the dataset version at first
//! query post-ATTACH. Subsequent writes via the LanceDB Rust API
//! create a new version on disk, but the existing DuckDB connection
//! keeps reading the cached snapshot. DETACH + re-ATTACH on the same
//! connection does **not** clear that cache (verified empirically in
//! the `store_open_write_read_round_trip` test); only a fresh
//! `Connection::open_in_memory()` picks up the new version.
//!
//! `Store` resolves this by calling [`DuckDbQuery::refresh`] —
//! which swaps in a brand-new in-process DuckDB connection — after
//! every mutating method. The `lance_write_then_refresh!` macro
//! threads this through; reads are unaffected (they pay nothing).
//! Cost: about a connection-setup's worth of milliseconds per write
//! (extension load + ATTACH on the new conn). For mem's typical
//! write/read ratio this is negligible.
//!
//! Method surface mirrors the legacy `Repository` super-trait 1:1 so
//! the upcoming service-layer cutover is a method-call swap, not a
//! type swap. Every read maps to `DuckDbQuery`; every write maps to
//! `LanceStore`. The handful of reads that don't yet have a
//! `DuckDbQuery` SQL implementation route to `LanceStore`'s native
//! query path (these are flagged with a `// TODO: route to
//! DuckDbQuery once added` comment); they're equivalent in result
//! shape, just slower because LanceStore reads do client-side sort /
//! aggregate where SQL would push to the engine.

use std::path::Path;
use std::sync::Arc;

use super::duckdb_query::DuckDbQuery;
use super::lance_store::LanceStore;
use super::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    EntityRegistry, FeedbackEvent, GraphError, GraphStore as GraphStoreTrait, MemoryRepository,
    StorageError, TranscriptRepository, TranscriptSessionSummary,
};
use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::memory::{FeedbackSummary, GraphEdge, MemoryRecord, MemoryVersionLink};
use crate::domain::session::Session;
use crate::domain::{AddAliasOutcome, ConversationMessage, Entity, EntityKind, EntityWithAliases};

/// Handle carried by every service / worker / HTTP component. Cheap
/// to clone (just two `Arc`s).
#[derive(Clone)]
pub struct Store {
    /// Writes (and a handful of yet-to-be-migrated reads) flow here.
    pub(crate) lance: Arc<LanceStore>,
    /// Reads flow here.
    pub(crate) query: Arc<DuckDbQuery>,
}

impl Store {
    /// Open both halves at `path` (a directory holding lance datasets).
    /// Creates the directory + lance datasets via `LanceStore::open`,
    /// then opens an in-process DuckDB and ATTACHes the lance dir.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let lance = LanceStore::open(path).await?;
        let query = DuckDbQuery::open(path).await?;
        Ok(Self {
            lance: Arc::new(lance),
            query: Arc::new(query),
        })
    }

    /// Like [`Self::open`], but registers an [`EmbeddingProvider`] on
    /// the LanceStore side so vector columns can declare auto-embed
    /// against `<provider>-<model>` via `EmbeddingDefinition`. The
    /// DuckDB query side is unaffected — it reads whatever vectors
    /// are on disk regardless of who computed them.
    ///
    /// [`EmbeddingProvider`]: crate::embedding::EmbeddingProvider
    pub async fn open_with_provider(
        path: impl AsRef<Path>,
        provider: Arc<dyn crate::embedding::EmbeddingProvider>,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let lance = LanceStore::open_with_provider(path, provider).await?;
        let query = DuckDbQuery::open(path).await?;
        Ok(Self {
            lance: Arc::new(lance),
            query: Arc::new(query),
        })
    }
}

/// Internal helper: chain a `LanceStore` write and a `DuckDbQuery`
/// refresh in the order the `Store` contract requires. Writes go to
/// lance; reads after the call must see the new version, so the
/// in-process DuckDB connection is reset (see
/// [`DuckDbQuery::refresh`] for why).
///
/// Returns the underlying write's `T` on success. Refresh failures
/// surface as `StorageError`; the write has already committed at
/// that point, so the caller sees the value the write produced even
/// if a future read from the same `Store` happens to see a stale
/// version (it'll converge on the next mutation).
macro_rules! lance_write_then_refresh {
    ($self:ident, $expr:expr) => {{
        let result = $expr;
        // Refresh whether or not the write succeeded — partial
        // commits in lance still bump the manifest version, so we
        // want a clean view either way.
        if let Err(e) = $self.query.refresh().await {
            // If we *did* have a successful write but the refresh
            // failed, prefer to surface the refresh error (the
            // caller should know the read view is stale). If the
            // write itself failed, that's the more interesting
            // error.
            return match result {
                Ok(_) => Err(e),
                Err(orig) => Err(orig),
            };
        }
        result
    }};
}

// ── Memory writes (LanceStore + DuckDbQuery refresh) ────────────────
impl Store {
    pub async fn insert_memory(&self, m: MemoryRecord) -> Result<MemoryRecord, StorageError> {
        lance_write_then_refresh!(self, self.lance.insert_memory(m).await)
    }

    pub async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        lance_write_then_refresh!(self, self.lance.try_enqueue_embedding_job(insert).await)
    }

    pub async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .claim_next_n_embedding_jobs(now, max_retries, n)
                .await
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_memory_embedding(
        &self,
        memory_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .upsert_memory_embedding(
                    memory_id,
                    tenant,
                    embedding_model,
                    embedding_dim,
                    embedding_blob,
                    content_hash,
                    source_updated_at,
                    now,
                )
                .await
        )
    }

    pub async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.delete_memory_embedding(memory_id).await)
    }

    pub async fn complete_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.complete_embedding_job(job_id, now).await)
    }

    pub async fn mark_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.mark_embedding_job_stale(job_id, now).await)
    }

    pub async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .reschedule_embedding_job_failure(
                    job_id,
                    new_attempt_count,
                    last_error,
                    available_at,
                    now,
                )
                .await
        )
    }

    pub async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .permanently_fail_embedding_job(job_id, new_attempt_count, last_error, now)
                .await
        )
    }

    pub async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .delete_embedding_jobs_by_memory_id(memory_id)
                .await
        )
    }

    pub async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        lance_write_then_refresh!(self, self.lance.accept_pending(tenant, memory_id).await)
    }

    pub async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        lance_write_then_refresh!(self, self.lance.reject_pending(tenant, memory_id).await)
    }

    pub async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .replace_pending_with_successor(tenant, original_memory_id, successor)
                .await
        )
    }

    pub async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        lance_write_then_refresh!(self, self.lance.apply_feedback(memory, feedback).await)
    }

    pub async fn delete_memory_hard(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.delete_memory_hard(tenant, memory_id).await)
    }

    pub async fn insert_episode(
        &self,
        episode: EpisodeRecord,
    ) -> Result<EpisodeRecord, StorageError> {
        lance_write_then_refresh!(self, self.lance.insert_episode(episode).await)
    }

    pub async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .stale_live_embedding_jobs_for_memory(tenant, memory_id, provider, now)
                .await
        )
    }
}

// ── Memory reads (DuckDbQuery) ──────────────────────────────────────
impl Store {
    pub async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query.list_memories_for_tenant(tenant).await
    }

    pub async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        self.query.get_memory_for_tenant(tenant, memory_id).await
    }

    pub async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        self.query.get_pending(tenant, memory_id).await
    }

    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        self.query
            .find_by_idempotency_or_hash(tenant, idempotency_key, content_hash)
            .await
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query.list_pending_review(tenant).await
    }

    pub async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query.search_candidates(tenant).await
    }

    pub async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query.recent_active_memories(tenant, limit).await
    }

    pub async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query.bm25_candidates(tenant, query, k).await
    }

    pub async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query.fetch_memories_by_ids(tenant, ids).await
    }

    pub async fn list_memory_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        self.query.list_memory_ids_for_tenant(tenant).await
    }

    pub async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        self.query
            .list_memory_versions_for_tenant(tenant, memory_id)
            .await
    }

    pub async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        self.query
            .semantic_search_memories(tenant, query_embedding, limit)
            .await
    }
}

// ── Memory reads with no DuckDbQuery counterpart yet (LanceStore) ──
//
// These reads route to the LanceStore native-query path until a SQL
// counterpart lands in `duckdb_query`. They produce the same result
// shape; the only cost of going through LanceStore is that some do
// in-Rust sort / aggregate where DuckDB SQL would push to the engine.
// Marked individually with `// TODO: route to DuckDbQuery once added`.
impl Store {
    /// TODO: route to DuckDbQuery once added.
    pub async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        self.lance
            .first_embedding_job_id_for_memory(memory_id)
            .await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        self.lance.list_feedback_for_memory(memory_id).await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        self.lance.feedback_summary(memory_id).await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn get_memory(
        &self,
        memory_id: String,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        self.lance.get_memory(memory_id).await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        self.lance.latest_active_session(tenant, caller_agent).await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        self.lance.list_successful_episodes_for_tenant(tenant).await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        self.lance
            .list_embedding_jobs(tenant, status_filter, memory_id_filter, limit)
            .await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        self.lance.get_memory_embedding_row(memory_id).await
    }

    /// TODO: route to DuckDbQuery once added.
    pub async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        self.lance
            .latest_embedding_job_status_for_hash(tenant, memory_id, target_content_hash)
            .await
    }

    /// Read embedding-job status by id. Used by the embedding worker
    /// to skip mid-flight processing when a concurrent caller has
    /// already marked the job stale. Routes to DuckDbQuery (SQL).
    pub async fn get_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        self.query.get_embedding_job_status(job_id).await
    }

    /// Bulk decay sweep over `memories.decay_score`. Routes to
    /// DuckDbQuery — issued as a single SQL UPDATE via the lance
    /// extension (per-row Rust iteration is not viable for this
    /// shape). DuckDB-side writes invalidate the connection's own
    /// cache, so no `Store::refresh` is needed.
    pub async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        self.query
            .apply_time_decay(decay_rate_per_day, now_ms, ms_per_day, now_ms_str)
            .await
    }

    /// Session lifecycle (touch / open / close) — all mutations.
    /// Routed to LanceStore + DuckDbQuery refresh.
    pub async fn touch_session(
        &self,
        session_id: &str,
        last_active_at: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance.touch_session(session_id, last_active_at).await
        )
    }

    pub async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .open_session(session_id, tenant, caller_agent, now)
                .await
        )
    }

    pub async fn close_session(
        &self,
        session_id: &str,
        ended_at: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.close_session(session_id, ended_at).await)
    }
}

// ── Transcript writes (LanceStore + refresh) ────────────────────────
impl Store {
    /// Configure the embedding-provider id stamped on
    /// `transcript_embedding_jobs.provider` rows enqueued by
    /// [`Self::create_conversation_message`]. Called once during
    /// startup (typically from `app.rs` right after `Store::open*`),
    /// before any transcript writes. Until set, embed-eligible
    /// transcript writes return `StorageError::InvalidData`.
    pub fn set_transcript_job_provider(&self, provider: impl Into<String>) {
        self.lance.set_transcript_job_provider(provider);
    }

    pub async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.create_conversation_message(msg).await)
    }

    pub async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .claim_next_n_transcript_embedding_jobs(now, max_retries, n)
                .await
        )
    }

    pub async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .complete_transcript_embedding_job(job_id, now)
                .await
        )
    }

    pub async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .mark_transcript_embedding_job_stale(job_id, now)
                .await
        )
    }

    pub async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .reschedule_transcript_embedding_job_failure(
                    job_id,
                    new_attempt_count,
                    last_error,
                    available_at,
                    now,
                )
                .await
        )
    }

    pub async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .permanently_fail_transcript_embedding_job(
                    job_id,
                    new_attempt_count,
                    last_error,
                    now
                )
                .await
        )
    }

    /// Upsert a transcript-block embedding (transcript embedding
    /// worker's hot path). Routes to LanceStore + DuckDbQuery refresh.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_conversation_message_embedding(
        &self,
        message_block_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .upsert_conversation_message_embedding(
                    message_block_id,
                    tenant,
                    embedding_model,
                    embedding_dim,
                    embedding_blob,
                    content_hash,
                    source_updated_at,
                    now,
                )
                .await
        )
    }

    pub async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance
                .delete_conversation_message_embedding(message_block_id)
                .await
        )
    }

    /// Semantic recall over transcript blocks. Routes to DuckDbQuery
    /// (lance_vector_search SQL + JOIN conversation_messages, cosine
    /// similarity via `1 - L²/2` for normalized embeddings).
    pub async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        self.query
            .semantic_search_transcripts(tenant, query_embedding, limit)
            .await
    }
}

// ── Transcript reads (DuckDbQuery) ──────────────────────────────────
impl Store {
    pub async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.query
            .get_conversation_messages_by_session(tenant, session_id)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        self.query
            .get_conversation_messages_by_session_paged(
                tenant, session_id, since, until, cursor, limit,
            )
            .await
    }

    pub async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        self.query.list_transcript_sessions(tenant).await
    }

    pub async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.query
            .fetch_conversation_messages_by_ids(tenant, ids)
            .await
    }

    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        self.query
            .context_window_for_block(tenant, primary_id, k_before, k_after, include_tool_blocks)
            .await
    }

    pub async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        self.query
            .anchor_session_candidates(tenant, session_id, k)
            .await
    }

    pub async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.query.recent_conversation_messages(tenant, limit).await
    }

    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.query
            .bm25_transcript_candidates(tenant, query, k)
            .await
    }
}

// ── Graph (writes → LanceStore, reads → DuckDbQuery) ────────────────
impl Store {
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.neighbors(node_id).await
    }

    pub async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        self.query.related_memory_ids(node_ids).await
    }

    pub async fn sync_memory_edges(
        &self,
        edges: &[GraphEdge],
        now: &str,
    ) -> Result<(), GraphError> {
        let result = self.lance.sync_memory_edges(edges, now).await;
        if let Err(e) = self.query.refresh().await {
            return match result {
                Ok(_) => Err(GraphError::Backend(e.to_string())),
                Err(orig) => Err(orig),
            };
        }
        result
    }

    pub async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let result = self.lance.close_edges_for_memory(memory_id).await;
        if let Err(e) = self.query.refresh().await {
            return match result {
                Ok(_) => Err(GraphError::Backend(e.to_string())),
                Err(orig) => Err(orig),
            };
        }
        result
    }
}

// ── EntityRegistry (writes → LanceStore + refresh, reads → query) ───
impl Store {
    pub async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance.resolve_or_create(tenant, alias, kind, now).await
        )
    }

    pub async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        lance_write_then_refresh!(
            self,
            self.lance.add_alias(tenant, entity_id, alias, now).await
        )
    }

    pub async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        self.query.get_entity(tenant, entity_id).await
    }

    pub async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        self.query.lookup_alias(tenant, alias).await
    }

    pub async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        self.query
            .list_entities(tenant, kind_filter, query, limit)
            .await
    }
}

#[cfg(all(test, feature = "lancedb"))]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryStatus, MemoryType, Scope, Visibility};
    use tempfile::tempdir;

    fn fixture(memory_id: &str, tenant: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: memory_id.into(),
            tenant: tenant.into(),
            memory_type: MemoryType::Implementation,
            status: MemoryStatus::Active,
            scope: Scope::Project,
            visibility: Visibility::Shared,
            version: 1,
            summary: "round-trip".into(),
            content: "use bun for fast installs".into(),
            evidence: vec!["src/main.rs:42".into()],
            code_refs: vec![],
            project: Some("mem".into()),
            repo: None,
            module: None,
            task_type: None,
            tags: vec![],
            topics: vec![],
            confidence: 0.7,
            decay_score: 0.0,
            content_hash: "h".repeat(64),
            idempotency_key: None,
            session_id: None,
            supersedes_memory_id: None,
            source_agent: "test".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
            last_validated_at: None,
        }
    }

    /// Cross-stack round-trip: writes via the lance half are
    /// immediately visible to reads via the duckdb-query half,
    /// because every `Store` write triggers a `DuckDbQuery::refresh`
    /// (rebuild of the in-process DuckDB connection). Without that
    /// refresh the lance extension's snapshot cache would hide
    /// post-attach lance writes — see the
    /// `lance_write_then_refresh!` macro doc for the gory details.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_open_write_read_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();

        // First write → first read: m_a visible.
        let m = fixture("m_a", "tenant-a");
        store.insert_memory(m.clone()).await.unwrap();
        let got = store
            .get_memory_for_tenant("tenant-a", "m_a")
            .await
            .unwrap()
            .expect("m_a visible after insert");
        assert_eq!(got.memory_id, "m_a");
        assert_eq!(got.evidence, vec!["src/main.rs:42".to_string()]);
        // Cross-tenant scope.
        let none = store
            .list_memories_for_tenant("does-not-exist")
            .await
            .unwrap();
        assert!(none.is_empty());
        let all = store.list_memories_for_tenant("tenant-a").await.unwrap();
        assert_eq!(all.len(), 1);

        // Second write: previously hidden by the snapshot cache; now
        // refresh is wired so it shows up.
        let mut p = fixture("m_pending", "tenant-a");
        p.status = MemoryStatus::PendingConfirmation;
        store.insert_memory(p).await.unwrap();
        let after = store.list_memories_for_tenant("tenant-a").await.unwrap();
        assert_eq!(after.len(), 2, "second write must be visible");

        let pre = store
            .get_memory_for_tenant("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("pending row visible after second insert + refresh");
        assert_eq!(pre.status, MemoryStatus::PendingConfirmation);

        // UPDATE via accept_pending (lance Table::update) — the
        // hardest case: lance UPDATE wasn't visible at all without
        // refresh.
        let accepted = store.accept_pending("tenant-a", "m_pending").await.unwrap();
        assert_eq!(accepted.status, MemoryStatus::Active);
        let post = store
            .get_memory_for_tenant("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("row visible to SQL after lance UPDATE + refresh");
        assert_eq!(post.status, MemoryStatus::Active);
    }

    /// `get_embedding_job_status`: enqueue a job via the lance side,
    /// read its status through DuckDbQuery (SQL), confirm round-trip.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_get_embedding_job_status_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();

        store
            .insert_memory(fixture("m_e", "tenant-a"))
            .await
            .unwrap();

        let none = store
            .get_embedding_job_status("never-existed")
            .await
            .unwrap();
        assert!(none.is_none());

        store
            .try_enqueue_embedding_job(EmbeddingJobInsert {
                job_id: "job_e1".into(),
                tenant: "tenant-a".into(),
                memory_id: "m_e".into(),
                target_content_hash: "h".into(),
                provider: "fake-test".into(),
                available_at: "00000001778000000000".into(),
                created_at: "00000001778000000000".into(),
                updated_at: "00000001778000000000".into(),
            })
            .await
            .unwrap();

        let status = store.get_embedding_job_status("job_e1").await.unwrap();
        assert_eq!(status.as_deref(), Some("pending"));
    }

    /// `apply_time_decay`: insert an active memory with decay_score=0
    /// and an updated_at 10 days in the past, run decay, verify the
    /// score moved (~0.1 with the canonical 0.01/day rate).
    #[tokio::test(flavor = "multi_thread")]
    async fn store_apply_time_decay_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();

        const MS_PER_DAY: f64 = 86_400_000.0;
        const RATE: f64 = 0.01;
        let now_ms = 100_000_000_000_000_u128; // arbitrary, big enough.
        let ten_days_ago = now_ms - 10 * MS_PER_DAY as u128;
        let ten_days_ago_str = format!("{ten_days_ago:020}");
        let now_str = format!("{now_ms:020}");

        let mut active = fixture("m_decay", "tenant-a");
        active.created_at = ten_days_ago_str.clone();
        active.updated_at = ten_days_ago_str.clone();
        active.decay_score = 0.0;
        store.insert_memory(active).await.unwrap();

        // Saturated row should not move (`decay_score < 1.0` filter).
        let mut sat = fixture("m_sat", "tenant-a");
        sat.created_at = ten_days_ago_str.clone();
        sat.updated_at = ten_days_ago_str.clone();
        sat.decay_score = 1.0;
        store.insert_memory(sat).await.unwrap();

        // Non-active row should not move (status='active' filter).
        let mut prov = fixture("m_prov", "tenant-a");
        prov.status = MemoryStatus::PendingConfirmation;
        prov.created_at = ten_days_ago_str.clone();
        prov.updated_at = ten_days_ago_str.clone();
        prov.decay_score = 0.0;
        store.insert_memory(prov).await.unwrap();

        store
            .apply_time_decay(RATE, now_ms as f64, MS_PER_DAY, &now_str)
            .await
            .unwrap();

        // The bulk UPDATE goes through DuckDbQuery's SQL path, which
        // doesn't trigger the lance write→refresh dance (DuckDB-side
        // writes invalidate the cache automatically). But subsequent
        // reads through Store still go through DuckDbQuery, so they
        // see the new state.
        let active_after = store
            .get_memory_for_tenant("tenant-a", "m_decay")
            .await
            .unwrap()
            .unwrap();
        // ~10 days * 0.01/day = 0.1 (allow slop for f32→f64 coercion).
        assert!(
            (0.05..=0.15).contains(&(active_after.decay_score as f64)),
            "active row decay should be ~0.1 after 10 days, got {}",
            active_after.decay_score
        );

        let sat_after = store
            .get_memory_for_tenant("tenant-a", "m_sat")
            .await
            .unwrap()
            .unwrap();
        assert!((sat_after.decay_score - 1.0).abs() < 1e-6);

        let prov_after = store
            .get_memory_for_tenant("tenant-a", "m_prov")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(prov_after.decay_score, 0.0);
    }

    fn cm(
        id: &str,
        tenant: &str,
        line: u64,
        block_idx: u32,
        embed_eligible: bool,
        created_at: &str,
    ) -> ConversationMessage {
        use crate::domain::{BlockType, MessageRole};
        ConversationMessage {
            message_block_id: id.into(),
            session_id: Some("sess".into()),
            tenant: tenant.into(),
            caller_agent: "claude-code".into(),
            transcript_path: format!("/tmp/{id}.jsonl"),
            line_number: line,
            block_index: block_idx,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type: if embed_eligible {
                BlockType::Text
            } else {
                BlockType::ToolUse
            },
            content: "block content".into(),
            tool_name: None,
            tool_use_id: None,
            embed_eligible,
            created_at: created_at.into(),
        }
    }

    /// Transcript embedding queue end-to-end via `Store`:
    ///   - create_conversation_message with embed_eligible=true
    ///     enqueues a transcript_embedding_jobs row.
    ///   - tool_use blocks (embed_eligible=false) don't enqueue.
    ///   - claim → status='processing', returned job has the right
    ///     fields.
    ///   - complete clears it; reschedule + claim picks it back up
    ///     with bumped attempt_count.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_transcript_embedding_queue_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();
        store.set_transcript_job_provider("fake-test-model");

        // Eligible block → job enqueued.
        let m1 = cm("blk_e1", "tenant-a", 10, 0, true, "00000001778000000010");
        store.create_conversation_message(&m1).await.unwrap();

        // Tool-use block → no job enqueued.
        let m2 = cm("blk_e2", "tenant-a", 12, 0, false, "00000001778000000020");
        store.create_conversation_message(&m2).await.unwrap();

        // Idempotent re-create on the same (path, line, idx) does
        // not re-enqueue. The natural-key uniqueness check is on
        // (transcript_path, line_number, block_index), so we have
        // to override transcript_path on the dup explicitly (the
        // `cm` helper derives it from id).
        let mut m1_dup = cm(
            "blk_e1_dup",
            "tenant-a",
            10,
            0,
            true,
            "00000001778000000011",
        );
        m1_dup.transcript_path = m1.transcript_path.clone();
        store.create_conversation_message(&m1_dup).await.unwrap();

        // Claim 5 → only 1 should be there (blk_e1's job).
        let claimed = store
            .claim_next_n_transcript_embedding_jobs("99999999999999999999", 5, 5)
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "got {claimed:?}");
        assert_eq!(claimed[0].message_block_id, "blk_e1");
        assert_eq!(claimed[0].provider, "fake-test-model");
        assert_eq!(claimed[0].attempt_count, 0);

        // Re-claim returns nothing.
        let recl = store
            .claim_next_n_transcript_embedding_jobs("99999999999999999999", 5, 5)
            .await
            .unwrap();
        assert!(recl.is_empty());

        // Reschedule pushes it back to failed → re-claim with budget.
        store
            .reschedule_transcript_embedding_job_failure(
                &claimed[0].job_id,
                1,
                "transient",
                "00000001778000020000",
                "00000001778000020000",
            )
            .await
            .unwrap();
        let now2 = "99999999999999999999";
        let recl2 = store
            .claim_next_n_transcript_embedding_jobs(now2, 3, 5)
            .await
            .unwrap();
        assert_eq!(recl2.len(), 1);
        assert_eq!(recl2[0].attempt_count, 1);

        // Complete it.
        store
            .complete_transcript_embedding_job(&recl2[0].job_id, "00000001778000040000")
            .await
            .unwrap();
        let recl3 = store
            .claim_next_n_transcript_embedding_jobs(now2, 3, 5)
            .await
            .unwrap();
        assert!(recl3.is_empty(), "completed jobs are not re-claimable");

        // Permanently fail / mark stale exercised on a fresh seed
        // for symmetry with the memory-side test.
        let m3 = cm("blk_e3", "tenant-a", 14, 0, true, "00000001778000050000");
        store.create_conversation_message(&m3).await.unwrap();
        let claim3 = store
            .claim_next_n_transcript_embedding_jobs("99999999999999999999", 5, 5)
            .await
            .unwrap();
        assert_eq!(claim3.len(), 1);
        store
            .mark_transcript_embedding_job_stale(&claim3[0].job_id, "00000001778000070000")
            .await
            .unwrap();
        // Stale rows are not re-claimable.
        let claim4 = store
            .claim_next_n_transcript_embedding_jobs("99999999999999999999", 5, 5)
            .await
            .unwrap();
        assert!(claim4.is_empty());

        // permanently_fail bumps attempt_count past budget so future
        // claims with the same budget skip the row.
        let m4 = cm("blk_e4", "tenant-a", 16, 0, true, "00000001778000090000");
        store.create_conversation_message(&m4).await.unwrap();
        let claim5 = store
            .claim_next_n_transcript_embedding_jobs("99999999999999999999", 5, 5)
            .await
            .unwrap();
        assert_eq!(claim5.len(), 1);
        store
            .permanently_fail_transcript_embedding_job(
                &claim5[0].job_id,
                10,
                "boom",
                "00000001778000110000",
            )
            .await
            .unwrap();
        let claim6 = store
            .claim_next_n_transcript_embedding_jobs("99999999999999999999", 5, 5)
            .await
            .unwrap();
        assert!(claim6.is_empty());
    }

    /// Upsert + semantic_search round-trip on the transcript side.
    /// Mirrors the memory-side `store_apply_time_decay_round_trip` /
    /// duckdb_query semantic test: write 3 blocks (2 in tenant-a, 1
    /// in tenant-b) with hand-rolled 4-d unit vectors, query with a
    /// vector close to v1 in tenant-a, assert ordering, similarity
    /// shape, and tenant scope.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_transcript_embedding_search_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();
        store.set_transcript_job_provider("fake-test");

        // Seed 3 conversation_messages first (must exist for the JOIN
        // in semantic_search_transcripts to find them).
        let blocks = [
            ("blk_v1", "tenant-a", 10, [1.0_f32, 0.0, 0.0, 0.0]),
            ("blk_v2", "tenant-a", 12, [0.0, 1.0, 0.0, 0.0]),
            ("blk_v3", "tenant-b", 14, [0.0, 0.0, 1.0, 0.0]),
        ];
        for (id, tenant, line, _) in &blocks {
            let m = cm(id, tenant, *line, 0, true, "00000001778000000000");
            store.create_conversation_message(&m).await.unwrap();
        }

        // Upsert embeddings.
        fn to_blob(v: &[f32]) -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 4);
            for f in v {
                out.extend_from_slice(&f.to_ne_bytes());
            }
            out
        }
        let now = "00000001778000010000";
        for (id, tenant, _, vec) in &blocks {
            store
                .upsert_conversation_message_embedding(
                    id,
                    tenant,
                    "fake-test",
                    4,
                    &to_blob(vec),
                    "h",
                    now,
                    now,
                )
                .await
                .unwrap();
        }

        // Query close to v1 → blk_v1 first; blk_v2 second; blk_v3
        // (tenant-b) excluded.
        let q = vec![0.99_f32, 0.14, 0.0, 0.0];
        let hits = store
            .semantic_search_transcripts("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "tenant-a transcript hits → 2 (blk_v1, blk_v2); got {hits:?}"
        );
        assert_eq!(hits[0].0.message_block_id, "blk_v1");
        assert_eq!(hits[1].0.message_block_id, "blk_v2");
        assert!(hits[0].1 > hits[1].1, "similarity descending");
        assert!(
            hits[0].1 > 0.99,
            "v1 ≈ query → cos_sim > 0.99; got {}",
            hits[0].1
        );

        // Empty / 0-limit short-circuits.
        let empty1 = store
            .semantic_search_transcripts("tenant-a", &[], 10)
            .await
            .unwrap();
        assert!(empty1.is_empty());
        let empty2 = store
            .semantic_search_transcripts("tenant-a", &q, 0)
            .await
            .unwrap();
        assert!(empty2.is_empty());

        // tenant-b sees its own row.
        let b = store
            .semantic_search_transcripts("tenant-b", &q, 10)
            .await
            .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].0.message_block_id, "blk_v3");

        // delete then re-query: corpus shrinks.
        store
            .delete_conversation_message_embedding("blk_v2")
            .await
            .unwrap();
        let after_delete = store
            .semantic_search_transcripts("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(after_delete.len(), 1);
        assert_eq!(after_delete[0].0.message_block_id, "blk_v1");
    }

    /// Embed-eligible message inserted while no provider is
    /// configured → `InvalidData`. The `set_transcript_job_provider`
    /// call must precede the first eligible write.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_transcript_eligible_without_provider_errs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();
        // intentionally do NOT call set_transcript_job_provider

        let m = cm("blk", "tenant-a", 1, 0, true, "00000001778000000000");
        let err = store.create_conversation_message(&m).await.unwrap_err();
        assert!(matches!(err, StorageError::InvalidData(_)), "got {err:?}");
    }
}
