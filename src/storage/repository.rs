//! Storage abstraction trait for swappable backend implementations.
//!
//! `DuckDbRepository` is the only concrete implementation today, but
//! callers (services, HTTP handlers, workers) should accept
//! `Arc<dyn MemoryRepository + Send + Sync>` (or a generic `R: MemoryRepository`)
//! so future backends — LanceDB, Milvus, sqlite, in-memory test fixture — can
//! be slotted in without touching the upper layers.
//!
//! Scope of this trait: the **core memories pipeline** — memory CRUD /
//! lifecycle, search, feedback events, embedding-job queue. Sessions,
//! transcripts, entities, graph edges live in their own narrower traits
//! (and `EntityRegistry` already exists), so backends can implement them
//! independently.
//!
//! Visibility: each trait method mirrors a public inherent method on
//! `DuckDbRepository` 1:1; the impl block at the bottom of this file is
//! pure trampoline. There's no behavior in the trait that's not in the
//! inherent surface — switching a service from `Arc<DuckDbRepository>`
//! to `Arc<dyn MemoryRepository>` is a no-op at the call site (just
//! requires `use crate::storage::MemoryRepository;` to bring the
//! methods into scope).

use async_trait::async_trait;

use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::memory::{FeedbackSummary, MemoryRecord, MemoryVersionLink};
use crate::domain::session::Session;
use crate::domain::ConversationMessage;
use crate::storage::{ContextWindow, FeedbackEvent, TranscriptSessionSummary};

use super::duckdb::{
    ClaimedEmbeddingJob, DuckDbRepository, EmbeddingJobInsert, EntityRegistry, StorageError,
};

/// Core memory + embedding-job repository surface. See module-level docs.
#[async_trait]
#[allow(clippy::too_many_arguments)]
pub trait MemoryRepository: Send + Sync {
    async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError>;

    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError>;

    async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError>;

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError>;

    async fn upsert_memory_embedding(
        &self,
        memory_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError>;

    async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError>;

    async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError>;

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError>;

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError>;

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError>;

    async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError>;

    async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError>;

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError>;

    async fn list_pending_review(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError>;

    async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError>;

    async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError>;

    async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError>;

    async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError>;

    async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError>;

    async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError>;

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError>;

    async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError>;

    async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError>;

    async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError>;

    async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError>;

    async fn delete_memory_hard(&self, tenant: &str, memory_id: &str) -> Result<(), StorageError>;

    async fn get_memory(&self, memory_id: String) -> Result<Option<MemoryRecord>, StorageError>;

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError>;

    async fn list_memory_ids_for_tenant(&self, tenant: &str) -> Result<Vec<String>, StorageError>;

    async fn touch_session(&self, session_id: &str, last_seen_at: &str)
        -> Result<(), StorageError>;

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError>;

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError>;

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError>;

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError>;

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError>;

    async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError>;

    async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError>;

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError>;
}

/// Transcript-archive surface — the parallel pipeline alongside `memories`
/// (one row per Claude Code transcript block). Used by `TranscriptService`
/// and the `POST /transcripts/*` HTTP routes.
#[async_trait]
pub trait TranscriptRepository: Send + Sync {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError>;

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    #[allow(clippy::too_many_arguments)]
    async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError>;

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError>;

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError>;

    async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError>;

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError>;

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError>;
}

/// Combined storage surface: `MemoryRepository` for the core memories +
/// embedding-jobs surface, plus `EntityRegistry` for the alias-canonicalization
/// path used by `MemoryService::ingest`. Bundling them here lets services
/// hold a single `Arc<dyn Repository + Send + Sync>` instead of two
/// separate trait objects.
///
/// `as_entity_registry` is the manual upcasting helper — without it (or
/// nightly trait-upcasting), callers can't pass `&dyn Repository` to a
/// function that wants `&dyn EntityRegistry`. With it, the call site is
/// `repo.as_entity_registry()`.
///
/// Backends that implement both traits get `Repository` for free via the
/// blanket impl below.
pub trait Repository:
    MemoryRepository + EntityRegistry + TranscriptRepository + Send + Sync
{
    fn as_entity_registry(&self) -> &dyn EntityRegistry;
    fn as_transcript_repository(&self) -> &dyn TranscriptRepository;
}

impl<T> Repository for T
where
    T: MemoryRepository + EntityRegistry + TranscriptRepository + Send + Sync + 'static,
{
    fn as_entity_registry(&self) -> &dyn EntityRegistry {
        self
    }
    fn as_transcript_repository(&self) -> &dyn TranscriptRepository {
        self
    }
}

#[async_trait]
impl MemoryRepository for DuckDbRepository {
    async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
        DuckDbRepository::insert_memory(self, memory).await
    }

    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        DuckDbRepository::try_enqueue_embedding_job(self, insert).await
    }

    async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        DuckDbRepository::first_embedding_job_id_for_memory(self, memory_id).await
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        DuckDbRepository::claim_next_n_embedding_jobs(self, now, max_retries, n).await
    }

    async fn upsert_memory_embedding(
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
        DuckDbRepository::upsert_memory_embedding(
            self,
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
    }

    async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        DuckDbRepository::delete_memory_embedding(self, memory_id).await
    }

    async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        DuckDbRepository::list_memories_for_tenant(self, tenant).await
    }

    async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        DuckDbRepository::semantic_search_memories(self, tenant, query_embedding, limit).await
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        DuckDbRepository::complete_embedding_job(self, job_id, now).await
    }

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        DuckDbRepository::mark_embedding_job_stale(self, job_id, now).await
    }

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        DuckDbRepository::reschedule_embedding_job_failure(
            self,
            job_id,
            new_attempt_count,
            last_error,
            available_at,
            now,
        )
        .await
    }

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        DuckDbRepository::permanently_fail_embedding_job(
            self,
            job_id,
            new_attempt_count,
            last_error,
            now,
        )
        .await
    }

    async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError> {
        DuckDbRepository::delete_embedding_jobs_by_memory_id(self, memory_id).await
    }

    async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        DuckDbRepository::get_memory_for_tenant(self, tenant, memory_id).await
    }

    async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        DuckDbRepository::get_pending(self, tenant, memory_id).await
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        DuckDbRepository::find_by_idempotency_or_hash(self, tenant, idempotency_key, content_hash)
            .await
    }

    async fn list_pending_review(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        DuckDbRepository::list_pending_review(self, tenant).await
    }

    async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        DuckDbRepository::search_candidates(self, tenant).await
    }

    async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        DuckDbRepository::recent_active_memories(self, tenant, limit).await
    }

    async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        DuckDbRepository::bm25_candidates(self, tenant, query, k).await
    }

    async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        DuckDbRepository::fetch_memories_by_ids(self, tenant, ids).await
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        DuckDbRepository::accept_pending(self, tenant, memory_id).await
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        DuckDbRepository::reject_pending(self, tenant, memory_id).await
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        DuckDbRepository::replace_pending_with_successor(
            self,
            tenant,
            original_memory_id,
            successor,
        )
        .await
    }

    async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        DuckDbRepository::apply_feedback(self, memory, feedback).await
    }

    async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        DuckDbRepository::list_feedback_for_memory(self, memory_id).await
    }

    async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        DuckDbRepository::list_memory_versions_for_tenant(self, tenant, memory_id).await
    }

    async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        DuckDbRepository::feedback_summary(self, memory_id).await
    }

    async fn delete_memory_hard(&self, tenant: &str, memory_id: &str) -> Result<(), StorageError> {
        DuckDbRepository::delete_memory_hard(self, tenant, memory_id).await
    }

    async fn get_memory(&self, memory_id: String) -> Result<Option<MemoryRecord>, StorageError> {
        DuckDbRepository::get_memory(self, memory_id).await
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        DuckDbRepository::insert_episode(self, episode).await
    }

    async fn list_memory_ids_for_tenant(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        DuckDbRepository::list_memory_ids_for_tenant(self, tenant).await
    }

    async fn touch_session(
        &self,
        session_id: &str,
        last_seen_at: &str,
    ) -> Result<(), StorageError> {
        DuckDbRepository::touch_session(self, session_id, last_seen_at).await
    }

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        DuckDbRepository::latest_active_session(self, tenant, caller_agent).await
    }

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        DuckDbRepository::open_session(self, session_id, tenant, caller_agent, now).await
    }

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError> {
        DuckDbRepository::close_session(self, session_id, ended_at).await
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        DuckDbRepository::list_successful_episodes_for_tenant(self, tenant).await
    }

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        DuckDbRepository::list_embedding_jobs(self, tenant, status_filter, memory_id_filter, limit)
            .await
    }

    async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        DuckDbRepository::stale_live_embedding_jobs_for_memory(
            self, tenant, memory_id, provider, now,
        )
        .await
    }

    async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        DuckDbRepository::get_memory_embedding_row(self, memory_id).await
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        DuckDbRepository::latest_embedding_job_status_for_hash(
            self,
            tenant,
            memory_id,
            target_content_hash,
        )
        .await
    }
}

#[async_trait]
impl TranscriptRepository for DuckDbRepository {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        DuckDbRepository::create_conversation_message(self, msg).await
    }

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        DuckDbRepository::get_conversation_messages_by_session(self, tenant, session_id).await
    }

    async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        DuckDbRepository::get_conversation_messages_by_session_paged(
            self, tenant, session_id, since, until, cursor, limit,
        )
        .await
    }

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        DuckDbRepository::list_transcript_sessions(self, tenant).await
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        DuckDbRepository::fetch_conversation_messages_by_ids(self, tenant, ids).await
    }

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        DuckDbRepository::context_window_for_block(
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
        DuckDbRepository::anchor_session_candidates(self, tenant, session_id, k).await
    }

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        DuckDbRepository::recent_conversation_messages(self, tenant, limit).await
    }

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        DuckDbRepository::bm25_transcript_candidates(self, tenant, query, k).await
    }
}
