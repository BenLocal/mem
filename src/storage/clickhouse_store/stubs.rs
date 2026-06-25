//! The 10 non-`CapsuleStore` sub-trait impls for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P2).** P2 only makes `ClickHouseBackend` a *complete*
//! [`Backend`] (every sub-trait impl'd → the blanket `impl<T> Backend for T`
//! applies → it can erase to `Arc<dyn Backend>`). The method bodies here are
//! `unimplemented!()` placeholders; the real implementations land per the
//! `docs/clickhouse-backend.md` §9 milestone table:
//!
//! ([`EmbeddingVectorStore`] is **done** in P3 — see `embedding.rs`.)
//!
//! - [`CapsuleSearchStore`] → **P4** (lexical + vector + Rust-side RRF)
//! - [`GraphStore`] / [`TranscriptStore`] / [`EmbeddingJobStore`] /
//!   [`EntityRegistry`] / [`SessionStore`] / [`MaintenanceStore`] /
//!   [`MineCursorStore`] / [`EvolutionCandidateStore`] → **P5**
//!
//! These panic at runtime if reached — `MEM_BACKEND=clickhouse mem serve`
//! starts (P2 wires assembly) but any read/write outside `CapsuleStore`
//! aborts until P3–P5 fill the bodies. The whole module is behind
//! `#[cfg(feature = "clickhouse")]`, so the default build never sees it.
//!
//! [`Backend`]: crate::storage::Backend

use async_trait::async_trait;

use super::backend::ClickHouseBackend;
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
use crate::storage::{
    CapsuleSearchStore, EmbeddingJobStore, EntityRegistry, EvolutionCandidate,
    EvolutionCandidateStore, GraphStore, MaintenanceStore, MineCursor, MineCursorStore,
    SessionStore, TranscriptStore,
};

// ─────────────────────────── CapsuleSearchStore (P4) ───────────────────────
#[async_trait]
impl CapsuleSearchStore for ClickHouseBackend {
    async fn search_candidates(
        &self,
        _tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!("clickhouse-backend P4: CapsuleSearchStore::search_candidates")
    }

    async fn recent_active_capability_capsules(
        &self,
        _tenant: &str,
        _limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!(
            "clickhouse-backend P4: CapsuleSearchStore::recent_active_capability_capsules"
        )
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        _tenant: &str,
        _ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!(
            "clickhouse-backend P4: CapsuleSearchStore::fetch_capability_capsules_by_ids"
        )
    }

    async fn list_capability_capsule_ids_for_tenant(
        &self,
        _tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        unimplemented!(
            "clickhouse-backend P4: CapsuleSearchStore::list_capability_capsule_ids_for_tenant"
        )
    }

    async fn list_capability_capsule_versions_for_tenant(
        &self,
        _tenant: &str,
        _capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        unimplemented!(
            "clickhouse-backend P4: CapsuleSearchStore::list_capability_capsule_versions_for_tenant"
        )
    }

    async fn hybrid_candidates(
        &self,
        _tenant: &str,
        _query_text: &str,
        _query_embedding: &[f32],
        _k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        unimplemented!("clickhouse-backend P4: CapsuleSearchStore::hybrid_candidates")
    }

    async fn hybrid_candidates_compose(
        &self,
        _tenant: &str,
        _query_text: &str,
        _query_embedding: &[f32],
        _k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        unimplemented!("clickhouse-backend P4: CapsuleSearchStore::hybrid_candidates_compose")
    }

    async fn bm25_candidate_ids(
        &self,
        _tenant: &str,
        _query_text: &str,
        _k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        unimplemented!("clickhouse-backend P4: CapsuleSearchStore::bm25_candidate_ids")
    }

    async fn ann_candidate_ids(
        &self,
        _tenant: &str,
        _query_embedding: &[f32],
        _k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        unimplemented!("clickhouse-backend P4: CapsuleSearchStore::ann_candidate_ids")
    }
}

// ─────────────────────────── EmbeddingJobStore (P5) ────────────────────────
#[async_trait]
impl EmbeddingJobStore for ClickHouseBackend {
    async fn try_enqueue_embedding_job(
        &self,
        _insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::try_enqueue_embedding_job")
    }

    async fn enqueue_embedding_jobs(
        &self,
        _inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::enqueue_embedding_jobs")
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        _now: &str,
        _max_retries: u32,
        _n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::claim_next_n_embedding_jobs")
    }

    async fn complete_embedding_job(&self, _job_id: &str, _now: &str) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::complete_embedding_job")
    }

    async fn mark_embedding_job_stale(
        &self,
        _job_id: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::mark_embedding_job_stale")
    }

    async fn reschedule_embedding_job_failure(
        &self,
        _job_id: &str,
        _new_attempt_count: i64,
        _last_error: &str,
        _available_at: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::reschedule_embedding_job_failure")
    }

    async fn permanently_fail_embedding_job(
        &self,
        _job_id: &str,
        _new_attempt_count: i64,
        _last_error: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::permanently_fail_embedding_job")
    }

    async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        unimplemented!(
            "clickhouse-backend P5: EmbeddingJobStore::delete_embedding_jobs_by_capability_capsule_id"
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
            "clickhouse-backend P5: EmbeddingJobStore::stale_live_embedding_jobs_for_capability_capsule"
        )
    }

    async fn get_embedding_job_status(
        &self,
        _job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::get_embedding_job_status")
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        _tenant: &str,
        _capability_capsule_id: &str,
        _target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!(
            "clickhouse-backend P5: EmbeddingJobStore::latest_embedding_job_status_for_hash"
        )
    }

    async fn list_embedding_jobs(
        &self,
        _tenant: &str,
        _status_filter: Option<&str>,
        _memory_id_filter: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        unimplemented!("clickhouse-backend P5: EmbeddingJobStore::list_embedding_jobs")
    }

    async fn claim_next_n_transcript_embedding_jobs(
        &self,
        _now: &str,
        _max_retries: u32,
        _n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        unimplemented!(
            "clickhouse-backend P5: EmbeddingJobStore::claim_next_n_transcript_embedding_jobs"
        )
    }

    async fn complete_transcript_embedding_job(
        &self,
        _job_id: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "clickhouse-backend P5: EmbeddingJobStore::complete_transcript_embedding_job"
        )
    }

    async fn mark_transcript_embedding_job_stale(
        &self,
        _job_id: &str,
        _now: &str,
    ) -> Result<(), StorageError> {
        unimplemented!(
            "clickhouse-backend P5: EmbeddingJobStore::mark_transcript_embedding_job_stale"
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
            "clickhouse-backend P5: EmbeddingJobStore::reschedule_transcript_embedding_job_failure"
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
            "clickhouse-backend P5: EmbeddingJobStore::permanently_fail_transcript_embedding_job"
        )
    }

    async fn get_transcript_embedding_job_status(
        &self,
        _job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!(
            "clickhouse-backend P5: EmbeddingJobStore::get_transcript_embedding_job_status"
        )
    }
}

// ─────────────────────────── GraphStore (P5) ───────────────────────────────
#[async_trait]
impl GraphStore for ClickHouseBackend {
    async fn neighbors(&self, _node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::neighbors")
    }

    async fn neighbors_within(
        &self,
        _node_id: &str,
        _max_hops: u32,
        _as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::neighbors_within")
    }

    async fn kg_timeline(&self, _node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::kg_timeline")
    }

    async fn query_predicate(
        &self,
        _predicate: &str,
        _as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::query_predicate")
    }

    async fn list_user_tunnels(&self, _limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::list_user_tunnels")
    }

    async fn find_tunnels(
        &self,
        _prefix_a: &str,
        _prefix_b: &str,
        _limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::find_tunnels")
    }

    async fn follow_tunnels(
        &self,
        _node_id: &str,
        _max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::follow_tunnels")
    }

    async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::graph_stats")
    }

    async fn related_capability_capsule_ids(
        &self,
        _node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::related_capability_capsule_ids")
    }

    async fn incident_edges_for_nodes(
        &self,
        _node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::incident_edges_for_nodes")
    }

    async fn sync_memory_edges(&self, _edges: &[GraphEdge], _now: &str) -> Result<(), GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::sync_memory_edges")
    }

    async fn add_edge_direct(&self, _edge: &GraphEdge) -> Result<bool, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::add_edge_direct")
    }

    async fn invalidate_edge(
        &self,
        _from_node_id: &str,
        _predicate: &str,
        _to_node_id: &str,
        _ended_at: &str,
    ) -> Result<usize, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::invalidate_edge")
    }

    async fn close_edges_for_capability_capsule(
        &self,
        _capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        unimplemented!("clickhouse-backend P5: GraphStore::close_edges_for_capability_capsule")
    }
}

// ─────────────────────────── TranscriptStore (P5) ──────────────────────────
#[async_trait]
impl TranscriptStore for ClickHouseBackend {
    async fn create_conversation_message(
        &self,
        _msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::create_conversation_message")
    }

    async fn create_conversation_messages(
        &self,
        _msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::create_conversation_messages")
    }

    async fn get_conversation_messages_by_session(
        &self,
        _tenant: &str,
        _session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!(
            "clickhouse-backend P5: TranscriptStore::get_conversation_messages_by_session"
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
            "clickhouse-backend P5: TranscriptStore::get_conversation_messages_by_session_paged"
        )
    }

    async fn list_transcript_sessions(
        &self,
        _tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::list_transcript_sessions")
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
            "clickhouse-backend P5: TranscriptStore::list_conversation_messages_in_range"
        )
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        _tenant: &str,
        _ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::fetch_conversation_messages_by_ids")
    }

    async fn context_window_for_block(
        &self,
        _tenant: &str,
        _primary_id: &str,
        _k_before: usize,
        _k_after: usize,
        _include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::context_window_for_block")
    }

    async fn anchor_session_candidates(
        &self,
        _tenant: &str,
        _session_id: &str,
        _k: usize,
    ) -> Result<Vec<String>, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::anchor_session_candidates")
    }

    async fn recent_conversation_messages(
        &self,
        _tenant: &str,
        _limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::recent_conversation_messages")
    }

    async fn bm25_transcript_candidates(
        &self,
        _tenant: &str,
        _query: &str,
        _k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::bm25_transcript_candidates")
    }

    async fn semantic_search_transcripts(
        &self,
        _tenant: &str,
        _query_embedding: &[f32],
        _limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        unimplemented!("clickhouse-backend P5: TranscriptStore::semantic_search_transcripts")
    }
}

// ─────────────────────────── EntityRegistry (P5) ───────────────────────────
#[async_trait]
impl EntityRegistry for ClickHouseBackend {
    async fn resolve_or_create(
        &self,
        _tenant: &str,
        _alias: &str,
        _kind: EntityKind,
        _now: &str,
    ) -> Result<String, StorageError> {
        unimplemented!("clickhouse-backend P5: EntityRegistry::resolve_or_create")
    }

    async fn add_alias(
        &self,
        _tenant: &str,
        _entity_id: &str,
        _alias: &str,
        _now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        unimplemented!("clickhouse-backend P5: EntityRegistry::add_alias")
    }

    async fn get_entity(
        &self,
        _tenant: &str,
        _entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        unimplemented!("clickhouse-backend P5: EntityRegistry::get_entity")
    }

    async fn lookup_alias(
        &self,
        _tenant: &str,
        _alias: &str,
    ) -> Result<Option<String>, StorageError> {
        unimplemented!("clickhouse-backend P5: EntityRegistry::lookup_alias")
    }

    async fn list_entities(
        &self,
        _tenant: &str,
        _kind_filter: Option<EntityKind>,
        _query: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        unimplemented!("clickhouse-backend P5: EntityRegistry::list_entities")
    }
}

// ─────────────────────────── SessionStore (P5) ─────────────────────────────
#[async_trait]
impl SessionStore for ClickHouseBackend {
    async fn touch_session(
        &self,
        _session_id: &str,
        _last_active_at: &str,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: SessionStore::touch_session")
    }

    async fn open_session(
        &self,
        _session_id: &str,
        _tenant: &str,
        _caller_agent: &str,
        _now: &str,
    ) -> Result<Session, StorageError> {
        unimplemented!("clickhouse-backend P5: SessionStore::open_session")
    }

    async fn close_session(&self, _session_id: &str, _ended_at: &str) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: SessionStore::close_session")
    }

    async fn latest_active_session(
        &self,
        _tenant: &str,
        _caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        unimplemented!("clickhouse-backend P5: SessionStore::latest_active_session")
    }

    async fn insert_episode(&self, _episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        unimplemented!("clickhouse-backend P5: SessionStore::insert_episode")
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        _tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        unimplemented!("clickhouse-backend P5: SessionStore::list_successful_episodes_for_tenant")
    }
}

// ─────────────────────────── MaintenanceStore (P5) ─────────────────────────
//
// Only the 3 *required* methods are stubbed; `vacuum_old_versions`,
// `ensure_query_indexes`, `rebuild_query_indexes` keep their trait defaults
// (zero-stats no-ops — the correct non-Lance behaviour, so they do NOT
// panic).
#[async_trait]
impl MaintenanceStore for ClickHouseBackend {
    async fn apply_time_decay(
        &self,
        _decay_rate_per_day: f64,
        _now_ms: f64,
        _ms_per_day: f64,
        _now_ms_str: &str,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: MaintenanceStore::apply_time_decay")
    }

    async fn vacuum_old_versions_with(
        &self,
        _older_than_days: i64,
        _aggressive: bool,
    ) -> Result<VacuumStats, StorageError> {
        unimplemented!("clickhouse-backend P5: MaintenanceStore::vacuum_old_versions_with")
    }

    async fn auto_promote_candidates(
        &self,
        _tenant: &str,
        _cutoff_updated_at: &str,
        _types: &[CapabilityCapsuleType],
        _max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        unimplemented!("clickhouse-backend P5: MaintenanceStore::auto_promote_candidates")
    }
}

// ─────────────────────────── MineCursorStore (P5) ──────────────────────────
#[async_trait]
impl MineCursorStore for ClickHouseBackend {
    async fn get_mine_cursor(
        &self,
        _transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError> {
        unimplemented!("clickhouse-backend P5: MineCursorStore::get_mine_cursor")
    }

    async fn upsert_mine_cursor(
        &self,
        _transcript_path: &str,
        _last_line_number: i64,
        _updated_at: &str,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: MineCursorStore::upsert_mine_cursor")
    }
}

// ─────────────────────────── EvolutionCandidateStore (P5) ──────────────────
#[async_trait]
impl EvolutionCandidateStore for ClickHouseBackend {
    async fn upsert_evolution_candidate(
        &self,
        _candidate: EvolutionCandidate,
    ) -> Result<(), StorageError> {
        unimplemented!("clickhouse-backend P5: EvolutionCandidateStore::upsert_evolution_candidate")
    }

    async fn list_evolution_candidates(
        &self,
        _tenant: &str,
        _status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError> {
        unimplemented!("clickhouse-backend P5: EvolutionCandidateStore::list_evolution_candidates")
    }
}
