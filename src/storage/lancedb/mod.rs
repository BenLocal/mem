//! LanceDB backend (skeleton).
//!
//! `LanceDbRepository` is the alternate backend to [`crate::storage::DuckDbRepository`].
//! It implements the same four traits — `MemoryRepository`,
//! `TranscriptRepository`, `EntityRegistry`, `GraphStore` — so all upper
//! layers (services, HTTP handlers) work against it interchangeably.
//!
//! **Status:** every method body is `unimplemented!()` today. The struct +
//! trait impl blocks are a scaffold proving the abstraction surface
//! actually allows pluggable backends. To turn this into a working
//! backend, implement the methods incrementally; tests in
//! `tests/lancedb_*.rs` (to be added) will validate parity with the
//! DuckDB happy path.
//!
//! **Schema mapping** (planned, not yet enforced):
//!
//! | mem table                          | LanceDB table                  |
//! |------------------------------------|--------------------------------|
//! | memories                           | `memories`                     |
//! | embedding_jobs                     | `embedding_jobs`               |
//! | memory_embeddings                  | `memory_embeddings` (vector col)|
//! | conversation_messages              | `conversation_messages`        |
//! | conversation_message_embeddings    | `conversation_message_embeddings` (vector col) |
//! | transcript_embedding_jobs          | `transcript_embedding_jobs`    |
//! | feedback_events                    | `feedback_events`              |
//! | episodes                           | `episodes`                     |
//! | sessions                           | `sessions`                     |
//! | entities                           | `entities`                     |
//! | entity_aliases                     | `entity_aliases`               |
//! | graph_edges                        | `graph_edges`                  |
//!
//! Vector columns use LanceDB's native vector type — no separate HNSW
//! sidecar; ANN is built-in.
//!
//! **Compile-time:** behind a `lancedb` Cargo feature. The default mem
//! build does not pull in lance/arrow.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use lancedb::Connection;

use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::memory::{FeedbackSummary, GraphEdge, MemoryRecord, MemoryVersionLink};
use crate::domain::session::Session;
use crate::domain::ConversationMessage;
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::storage::duckdb::{ClaimedEmbeddingJob, EmbeddingJobInsert, EntityRegistry};
use crate::storage::{
    ContextWindow, FeedbackEvent, GraphError, GraphStore, MemoryRepository, StorageError,
    TranscriptRepository, TranscriptSessionSummary,
};

/// LanceDB-backed implementation of the storage trait surface.
///
/// Holds an open `lancedb::Connection`. All async DB operations route
/// through it; there is no equivalent of the DuckDB single-Mutex write
/// connection because LanceDB is itself async-native and handles
/// concurrency internally.
#[derive(Clone)]
pub struct LanceDbRepository {
    /// LanceDB connection. Currently unused — every trait method is
    /// `unimplemented!()` placeholder. The first real method to write is
    /// `open()` (creates / opens the schema tables); afterwards method
    /// bodies will hit `self.conn.open_table(...)` etc.
    #[allow(dead_code)]
    conn: Arc<Connection>,
}

impl LanceDbRepository {
    /// Open (or create) a LanceDB store at the given path.
    ///
    /// **Not implemented:** creating the per-table schemas (memories,
    /// embedding_jobs, etc.) is the first real method that needs writing.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let _ = path;
        unimplemented!(
            "LanceDb::open — connect via lancedb::connect(path).execute().await, then \
             ensure all 12 schema tables exist (create_empty_table) per the schema \
             mapping in this module's doc comment"
        )
    }
}

#[async_trait]
#[allow(clippy::too_many_arguments)]
impl MemoryRepository for LanceDbRepository {
    async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
        let _ = memory;
        unimplemented!("LanceDb::insert_memory — see docs/repository.rs trait def")
    }

    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        let _ = insert;
        unimplemented!("LanceDb::try_enqueue_embedding_job — see docs/repository.rs trait def")
    }

    async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let _ = memory_id;
        unimplemented!(
            "LanceDb::first_embedding_job_id_for_memory — see docs/repository.rs trait def"
        )
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        let _ = (now, max_retries, n);
        unimplemented!("LanceDb::claim_next_n_embedding_jobs — see docs/repository.rs trait def")
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
        let _ = (
            memory_id,
            tenant,
            embedding_model,
            embedding_dim,
            embedding_blob,
            content_hash,
            source_updated_at,
            now,
        );
        unimplemented!("LanceDb::upsert_memory_embedding — see docs/repository.rs trait def")
    }

    async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::delete_memory_embedding — see docs/repository.rs trait def")
    }

    async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_memories_for_tenant — see docs/repository.rs trait def")
    }

    async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        let _ = (tenant, query_embedding, limit);
        unimplemented!(
            "LanceDb::semantic_search_memories — use Table::vector_search().column(\"embedding\").limit(limit).execute()"
        )
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        let _ = (job_id, now);
        unimplemented!("LanceDb::complete_embedding_job — see docs/repository.rs trait def")
    }

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        let _ = (job_id, now);
        unimplemented!("LanceDb::mark_embedding_job_stale — see docs/repository.rs trait def")
    }

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let _ = (job_id, new_attempt_count, last_error, available_at, now);
        unimplemented!(
            "LanceDb::reschedule_embedding_job_failure — see docs/repository.rs trait def"
        )
    }

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let _ = (job_id, new_attempt_count, last_error, now);
        unimplemented!("LanceDb::permanently_fail_embedding_job — see docs/repository.rs trait def")
    }

    async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError> {
        let _ = memory_id;
        unimplemented!(
            "LanceDb::delete_embedding_jobs_by_memory_id — see docs/repository.rs trait def"
        )
    }

    async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::get_memory_for_tenant — see docs/repository.rs trait def")
    }

    async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::get_pending — see docs/repository.rs trait def")
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = (tenant, idempotency_key, content_hash);
        unimplemented!("LanceDb::find_by_idempotency_or_hash — see docs/repository.rs trait def")
    }

    async fn list_pending_review(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_pending_review — see docs/repository.rs trait def")
    }

    async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::search_candidates — see docs/repository.rs trait def")
    }

    async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = (tenant, limit);
        unimplemented!("LanceDb::recent_active_memories — see docs/repository.rs trait def")
    }

    async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = (tenant, query, k);
        unimplemented!(
            "LanceDb::bm25_candidates — LanceDB has no native BM25; either run Tantivy \
             alongside (same as DuckDB) or use vector_search() as a substitute"
        )
    }

    async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = (tenant, ids);
        unimplemented!("LanceDb::fetch_memories_by_ids — see docs/repository.rs trait def")
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::accept_pending — see docs/repository.rs trait def")
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::reject_pending — see docs/repository.rs trait def")
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (tenant, original_memory_id, successor);
        unimplemented!("LanceDb::replace_pending_with_successor — see docs/repository.rs trait def")
    }

    async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (memory, feedback);
        unimplemented!("LanceDb::apply_feedback — see docs/repository.rs trait def")
    }

    async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::list_feedback_for_memory — see docs/repository.rs trait def")
    }

    async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!(
            "LanceDb::list_memory_versions_for_tenant — see docs/repository.rs trait def"
        )
    }

    async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::feedback_summary — see docs/repository.rs trait def")
    }

    async fn delete_memory_hard(&self, tenant: &str, memory_id: &str) -> Result<(), StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::delete_memory_hard — see docs/repository.rs trait def")
    }

    async fn get_memory(&self, memory_id: String) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::get_memory — see docs/repository.rs trait def")
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        let _ = episode;
        unimplemented!("LanceDb::insert_episode — see docs/repository.rs trait def")
    }

    async fn list_memory_ids_for_tenant(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_memory_ids_for_tenant — see docs/repository.rs trait def")
    }

    async fn touch_session(
        &self,
        session_id: &str,
        last_seen_at: &str,
    ) -> Result<(), StorageError> {
        let _ = (session_id, last_seen_at);
        unimplemented!("LanceDb::touch_session — see docs/repository.rs trait def")
    }

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        let _ = (tenant, caller_agent);
        unimplemented!("LanceDb::latest_active_session — see docs/repository.rs trait def")
    }

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        let _ = (session_id, tenant, caller_agent, now);
        unimplemented!("LanceDb::open_session — see docs/repository.rs trait def")
    }

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError> {
        let _ = (session_id, ended_at);
        unimplemented!("LanceDb::close_session — see docs/repository.rs trait def")
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        let _ = tenant;
        unimplemented!(
            "LanceDb::list_successful_episodes_for_tenant — see docs/repository.rs trait def"
        )
    }

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        let _ = (tenant, status_filter, memory_id_filter, limit);
        unimplemented!("LanceDb::list_embedding_jobs — see docs/repository.rs trait def")
    }

    async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        let _ = (tenant, memory_id, provider, now);
        unimplemented!(
            "LanceDb::stale_live_embedding_jobs_for_memory — see docs/repository.rs trait def"
        )
    }

    async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::get_memory_embedding_row — see docs/repository.rs trait def")
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let _ = (tenant, memory_id, target_content_hash);
        unimplemented!(
            "LanceDb::latest_embedding_job_status_for_hash — see docs/repository.rs trait def"
        )
    }
}

#[async_trait]
impl TranscriptRepository for LanceDbRepository {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        let _ = msg;
        unimplemented!("LanceDb::create_conversation_message — see docs/repository.rs trait def")
    }

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, session_id);
        unimplemented!(
            "LanceDb::get_conversation_messages_by_session — see docs/repository.rs trait def"
        )
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
        let _ = (tenant, session_id, since, until, cursor, limit);
        unimplemented!("LanceDb::get_conversation_messages_by_session_paged — see docs/repository.rs trait def")
    }

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_transcript_sessions — see docs/repository.rs trait def")
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, ids);
        unimplemented!(
            "LanceDb::fetch_conversation_messages_by_ids — see docs/repository.rs trait def"
        )
    }

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        let _ = (tenant, primary_id, k_before, k_after, include_tool_blocks);
        unimplemented!("LanceDb::context_window_for_block — see docs/repository.rs trait def")
    }

    async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        let _ = (tenant, session_id, k);
        unimplemented!("LanceDb::anchor_session_candidates — see docs/repository.rs trait def")
    }

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, limit);
        unimplemented!("LanceDb::recent_conversation_messages — see docs/repository.rs trait def")
    }

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, query, k);
        unimplemented!("LanceDb::bm25_transcript_candidates — see docs/repository.rs trait def")
    }
}

#[async_trait]
impl GraphStore for LanceDbRepository {
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let _ = node_id;
        unimplemented!("LanceDb::neighbors — query graph_edges table where from_node_id = ? OR to_node_id = ? AND valid_to is null")
    }

    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError> {
        let _ = (edges, now);
        unimplemented!("LanceDb::sync_memory_edges — idempotent insert into graph_edges table")
    }

    async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let _ = memory_id;
        unimplemented!(
            "LanceDb::close_edges_for_memory — set valid_to = now where from_node_id = memory:<id>"
        )
    }

    async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        let _ = node_ids;
        unimplemented!("LanceDb::related_memory_ids — find memory: prefixed nodes connected to any of node_ids")
    }
}

#[async_trait]
impl EntityRegistry for LanceDbRepository {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        let _ = (tenant, alias, kind, now);
        unimplemented!("LanceDb::resolve_or_create — see docs/repository.rs trait def")
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let _ = (tenant, entity_id);
        unimplemented!("LanceDb::get_entity — see docs/repository.rs trait def")
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        let _ = (tenant, entity_id, alias, now);
        unimplemented!("LanceDb::add_alias — see docs/repository.rs trait def")
    }

    async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        let _ = (tenant, alias);
        unimplemented!("LanceDb::lookup_alias — see docs/repository.rs trait def")
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let _ = (tenant, kind_filter, query, limit);
        unimplemented!("LanceDb::list_entities — see docs/repository.rs trait def")
    }
}
