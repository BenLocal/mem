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
    ClaimedEmbeddingJob, ContextWindow, EmbeddingJobInsert, EntityRegistry, FeedbackEvent,
    GraphError, GraphStore as GraphStoreTrait, MemoryRepository, StorageError,
    TranscriptRepository, TranscriptSessionSummary,
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
    pub async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        lance_write_then_refresh!(self, self.lance.create_conversation_message(msg).await)
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
}
