//! Phase 5 — `Backend` umbrella placeholder impls for
//! [`PostgresCapsuleStore`].
//!
//! [`super::Backend`] requires 11 storage sub-traits. The Phase 4
//! spike (`postgres_capsule_store.rs`) only implements
//! [`super::CapsuleStore`] for real. This module supplies P2-skeleton
//! `unimplemented!()` placeholders for the other 10 so the concrete
//! type satisfies `Backend` and the blanket impl in `backend.rs`
//! applies. Every method body here is a deliberate stub — the real
//! Postgres implementations land in postgres-backend phases P3-P5.
//!
//! Behind the `postgres` cargo feature (this whole module is only
//! `mod`'d under `#[cfg(feature = "postgres")]`), so the default build
//! never sees these stubs.

use async_trait::async_trait;

use super::postgres_capsule_store::PostgresCapsuleStore;
use super::{
    CapsuleSearchStore, EmbeddingJobStore, EmbeddingVectorStore, EntityRegistry,
    EvolutionCandidate, EvolutionCandidateStore, GraphStore, MaintenanceStore, MineCursor,
    MineCursorStore, SessionStore, TranscriptStore,
};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleType, CapabilityCapsuleVersionLink, GraphEdge,
    GraphStats,
};
use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::session::Session;
use crate::domain::{AddAliasOutcome, ConversationMessage, Entity, EntityKind, EntityWithAliases};
use crate::storage::lance_store::VacuumStats;
use crate::storage::types::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    GraphError, StorageError, TranscriptSessionSummary,
};

// ─────────────────────────── CapsuleSearchStore ───────────────────────────

#[async_trait]
impl CapsuleSearchStore for PostgresCapsuleStore {
    async fn search_candidates(
        &self,
        _tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::search_candidates not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn recent_active_capability_capsules(
        &self,
        _tenant: &str,
        _limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::recent_active_capability_capsules not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        _tenant: &str,
        _ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::fetch_capability_capsules_by_ids not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_capability_capsule_ids_for_tenant(
        &self,
        _tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::list_capability_capsule_ids_for_tenant not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_capability_capsule_versions_for_tenant(
        &self,
        _tenant: &str,
        _capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::list_capability_capsule_versions_for_tenant not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn hybrid_candidates(
        &self,
        _tenant: &str,
        _query_text: &str,
        _query_embedding: &[f32],
        _k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::hybrid_candidates not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn hybrid_candidates_compose(
        &self,
        _tenant: &str,
        _query_text: &str,
        _query_embedding: &[f32],
        _k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::hybrid_candidates_compose not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn bm25_candidate_ids(
        &self,
        _tenant: &str,
        _query_text: &str,
        _k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::bm25_candidate_ids not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn ann_candidate_ids(
        &self,
        _tenant: &str,
        _query_embedding: &[f32],
        _k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        unimplemented!(
            "postgres backend: CapsuleSearchStore::ann_candidate_ids not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ─────────────────────────── EmbeddingJobStore ────────────────────────────

#[async_trait]
impl EmbeddingJobStore for PostgresCapsuleStore {
    async fn try_enqueue_embedding_job(
        &self,
        _insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::try_enqueue_embedding_job not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn enqueue_embedding_jobs(
        &self,
        _inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::enqueue_embedding_jobs not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        _now: &str,
        _max_retries: u32,
        _n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::claim_next_n_embedding_jobs not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn complete_embedding_job(&self, _job_id: &str, _now: &str) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::complete_embedding_job not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn mark_embedding_job_stale(
        &self,
        _job_id: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::mark_embedding_job_stale not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn reschedule_embedding_job_failure(
        &self,
        _job_id: &str,
        _new_attempt_count: i64,
        _last_error: &str,
        _available_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::reschedule_embedding_job_failure not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn permanently_fail_embedding_job(
        &self,
        _job_id: &str,
        _new_attempt_count: i64,
        _last_error: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::permanently_fail_embedding_job not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::delete_embedding_jobs_by_capability_capsule_id not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        _tenant: &str,
        _capability_capsule_id: &str,
        _provider: &str,
        _now: &str,
    ) -> Result<usize, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::stale_live_embedding_jobs_for_capability_capsule not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn get_embedding_job_status(
        &self,
        _job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::get_embedding_job_status not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        _tenant: &str,
        _capability_capsule_id: &str,
        _target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::latest_embedding_job_status_for_hash not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_embedding_jobs(
        &self,
        _tenant: &str,
        _status_filter: Option<&str>,
        _memory_id_filter: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::list_embedding_jobs not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn claim_next_n_transcript_embedding_jobs(
        &self,
        _now: &str,
        _max_retries: u32,
        _n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::claim_next_n_transcript_embedding_jobs not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn complete_transcript_embedding_job(
        &self,
        _job_id: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::complete_transcript_embedding_job not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn mark_transcript_embedding_job_stale(
        &self,
        _job_id: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::mark_transcript_embedding_job_stale not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn reschedule_transcript_embedding_job_failure(
        &self,
        _job_id: &str,
        _new_attempt_count: i64,
        _last_error: &str,
        _available_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::reschedule_transcript_embedding_job_failure not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn permanently_fail_transcript_embedding_job(
        &self,
        _job_id: &str,
        _new_attempt_count: i64,
        _last_error: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::permanently_fail_transcript_embedding_job not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn get_transcript_embedding_job_status(
        &self,
        _job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingJobStore::get_transcript_embedding_job_status not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ────────────────────────── EmbeddingVectorStore ──────────────────────────

#[async_trait]
impl EmbeddingVectorStore for PostgresCapsuleStore {
    #[allow(clippy::too_many_arguments)]
    async fn upsert_capability_capsule_embedding(
        &self,
        _capability_capsule_id: &str,
        _tenant: &str,
        _embedding_model: &str,
        _embedding_dim: i64,
        _embedding_blob: &[u8],
        _content_hash: &str,
        _source_updated_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::upsert_capability_capsule_embedding not yet implemented (postgres-backend P3-P5)"
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_capability_capsule_embedding_chunks(
        &self,
        _capability_capsule_id: &str,
        _tenant: &str,
        _embedding_model: &str,
        _embedding_dim: i64,
        _vectors: &[Vec<f32>],
        _content_hash: &str,
        _source_updated_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::upsert_capability_capsule_embedding_chunks not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn delete_capability_capsule_embedding(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::delete_capability_capsule_embedding not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn get_capability_capsule_embedding_row(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::get_capability_capsule_embedding_row not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn get_capability_capsule_embedding_vector(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::get_capability_capsule_embedding_vector not yet implemented (postgres-backend P3-P5)"
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_conversation_message_embedding(
        &self,
        _message_block_id: &str,
        _tenant: &str,
        _embedding_model: &str,
        _embedding_dim: i64,
        _embedding_blob: &[u8],
        _content_hash: &str,
        _source_updated_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::upsert_conversation_message_embedding not yet implemented (postgres-backend P3-P5)"
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_conversation_message_embedding_chunks(
        &self,
        _message_block_id: &str,
        _tenant: &str,
        _embedding_model: &str,
        _embedding_dim: i64,
        _vectors: &[Vec<f32>],
        _content_hash: &str,
        _source_updated_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::upsert_conversation_message_embedding_chunks not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn delete_conversation_message_embedding(
        &self,
        _message_block_id: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EmbeddingVectorStore::delete_conversation_message_embedding not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ───────────────────────────────── GraphStore ─────────────────────────────

#[async_trait]
impl GraphStore for PostgresCapsuleStore {
    async fn neighbors(&self, _node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::neighbors not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn neighbors_within(
        &self,
        _node_id: &str,
        _max_hops: u32,
        _as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::neighbors_within not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn kg_timeline(&self, _node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::kg_timeline not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn query_predicate(
        &self,
        _predicate: &str,
        _as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::query_predicate not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_user_tunnels(&self, _limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::list_user_tunnels not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn find_tunnels(
        &self,
        _prefix_a: &str,
        _prefix_b: &str,
        _limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::find_tunnels not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn follow_tunnels(
        &self,
        _node_id: &str,
        _max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::follow_tunnels not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::graph_stats not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn related_capability_capsule_ids(
        &self,
        _node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::related_capability_capsule_ids not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn incident_edges_for_nodes(
        &self,
        _node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::incident_edges_for_nodes not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn sync_memory_edges(&self, _edges: &[GraphEdge], _now: &str) -> Result<(), GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::sync_memory_edges not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn add_edge_direct(&self, _edge: &GraphEdge) -> Result<bool, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::add_edge_direct not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn invalidate_edge(
        &self,
        _from_node_id: &str,
        _predicate: &str,
        _to_node_id: &str,
        _ended_at: &str,
    ) -> Result<usize, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::invalidate_edge not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn close_edges_for_capability_capsule(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        unimplemented!(
            "postgres backend: GraphStore::close_edges_for_capability_capsule not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ─────────────────────────────── TranscriptStore ──────────────────────────

#[async_trait]
impl TranscriptStore for PostgresCapsuleStore {
    async fn create_conversation_message(
        &self,
        _msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::create_conversation_message not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn create_conversation_messages(
        &self,
        _msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::create_conversation_messages not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn get_conversation_messages_by_session(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::get_conversation_messages_by_session not yet implemented (postgres-backend P3-P5)"
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn get_conversation_messages_by_session_paged(
        &self,
        _tenant: &str,
        _session_id: &str,
        _since: Option<&str>,
        _until: Option<&str>,
        _role: Option<&str>,
        _block_type: Option<&str>,
        _cursor: Option<(&str, i64, i64)>,
        _limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::get_conversation_messages_by_session_paged not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_transcript_sessions(
        &self,
        _tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::list_transcript_sessions not yet implemented (postgres-backend P3-P5)"
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn list_conversation_messages_in_range(
        &self,
        _tenant: &str,
        _time_from: Option<&str>,
        _time_to: Option<&str>,
        _role: Option<&str>,
        _block_type: Option<&str>,
        _cursor: Option<(&str, i64, i64)>,
        _limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::list_conversation_messages_in_range not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        _tenant: &str,
        _ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::fetch_conversation_messages_by_ids not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn context_window_for_block(
        &self,
        _tenant: &str,
        _primary_id: &str,
        _k_before: usize,
        _k_after: usize,
        _include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::context_window_for_block not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn anchor_session_candidates(
        &self,
        _tenant: &str,
        _session_id: &str,
        _k: usize,
    ) -> Result<Vec<String>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::anchor_session_candidates not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn recent_conversation_messages(
        &self,
        _tenant: &str,
        _limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::recent_conversation_messages not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn bm25_transcript_candidates(
        &self,
        _tenant: &str,
        _query: &str,
        _k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::bm25_transcript_candidates not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn semantic_search_transcripts(
        &self,
        _tenant: &str,
        _query_embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        unimplemented!(
            "postgres backend: TranscriptStore::semantic_search_transcripts not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ─────────────────────────────── EntityRegistry ───────────────────────────

#[async_trait]
impl EntityRegistry for PostgresCapsuleStore {
    async fn resolve_or_create(
        &self,
        _tenant: &str,
        _alias: &str,
        _kind: EntityKind,
        _now: &str,
    ) -> Result<String, StorageError> {
        unimplemented!(
            "postgres backend: EntityRegistry::resolve_or_create not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn add_alias(
        &self,
        _tenant: &str,
        _entity_id: &str,
        _alias: &str,
        _now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        unimplemented!(
            "postgres backend: EntityRegistry::add_alias not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn get_entity(
        &self,
        _tenant: &str,
        _entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        unimplemented!(
            "postgres backend: EntityRegistry::get_entity not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn lookup_alias(
        &self,
        _tenant: &str,
        _alias: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!(
            "postgres backend: EntityRegistry::lookup_alias not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_entities(
        &self,
        _tenant: &str,
        _kind_filter: Option<EntityKind>,
        _query: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        unimplemented!(
            "postgres backend: EntityRegistry::list_entities not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ──────────────────────────────── SessionStore ────────────────────────────

#[async_trait]
impl SessionStore for PostgresCapsuleStore {
    async fn touch_session(
        &self,
        _session_id: &str,
        _last_active_at: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: SessionStore::touch_session not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn open_session(
        &self,
        _session_id: &str,
        _tenant: &str,
        _caller_agent: &str,
        _now: &str,
    ) -> Result<Session, StorageError> {
        unimplemented!(
            "postgres backend: SessionStore::open_session not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn close_session(&self, _session_id: &str, _ended_at: &str) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: SessionStore::close_session not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn latest_active_session(
        &self,
        _tenant: &str,
        _caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        unimplemented!(
            "postgres backend: SessionStore::latest_active_session not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn insert_episode(&self, _episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        unimplemented!(
            "postgres backend: SessionStore::insert_episode not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        _tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        unimplemented!(
            "postgres backend: SessionStore::list_successful_episodes_for_tenant not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ────────────────────────────── MaintenanceStore ──────────────────────────
//
// `vacuum_old_versions` + `ensure_query_indexes` have trait default
// bodies (Lance-specific no-ops for non-Lance backends) — left
// unimplemented here so the defaults apply. Only the three
// no-default methods get stubs.

#[async_trait]
impl MaintenanceStore for PostgresCapsuleStore {
    async fn apply_time_decay(
        &self,
        _decay_rate_per_day: f64,
        _now_ms: f64,
        _ms_per_day: f64,
        _now_ms_str: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: MaintenanceStore::apply_time_decay not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn vacuum_old_versions_with(
        &self,
        _older_than_days: i64,
        _aggressive: bool,
    ) -> Result<VacuumStats, StorageError> {
        unimplemented!(
            "postgres backend: MaintenanceStore::vacuum_old_versions_with not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn auto_promote_candidates(
        &self,
        _tenant: &str,
        _cutoff_updated_at: &str,
        _types: &[CapabilityCapsuleType],
        _max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!(
            "postgres backend: MaintenanceStore::auto_promote_candidates not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ────────────────────────────── MineCursorStore ───────────────────────────

#[async_trait]
impl MineCursorStore for PostgresCapsuleStore {
    async fn get_mine_cursor(
        &self,
        _transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError> {
        unimplemented!(
            "postgres backend: MineCursorStore::get_mine_cursor not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn upsert_mine_cursor(
        &self,
        _transcript_path: &str,
        _last_line_number: i64,
        _updated_at: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: MineCursorStore::upsert_mine_cursor not yet implemented (postgres-backend P3-P5)"
        )
    }
}

// ─────────────────────────── EvolutionCandidateStore ──────────────────────

#[async_trait]
impl EvolutionCandidateStore for PostgresCapsuleStore {
    async fn upsert_evolution_candidate(
        &self,
        _candidate: EvolutionCandidate,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "postgres backend: EvolutionCandidateStore::upsert_evolution_candidate not yet implemented (postgres-backend P3-P5)"
        )
    }

    async fn list_evolution_candidates(
        &self,
        _tenant: &str,
        _status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError> {
        unimplemented!(
            "postgres backend: EvolutionCandidateStore::list_evolution_candidates not yet implemented (postgres-backend P3-P5)"
        )
    }
}
