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
//! every mutating method. The [`Store::commit_lance_write`] helper
//! threads this through (was the `lance_write_then_refresh!` macro
//! pre-QW-6); reads are unaffected (they pay nothing).
//! Cost: about a connection-setup's worth of milliseconds per write
//! (extension load + ATTACH on the new conn). For mem's typical
//! write/read ratio this is negligible.
//!
//! ### Portability annotation
//!
//! The method surface is **mostly portable across backends** — pure
//! CRUD over typed records that any reasonable storage engine could
//! re-implement. The handful of methods that bind to LanceDB-specific
//! behavior (Lance manifest pruning, lazy-create embedding tables,
//! `update().only_if()` optimistic-claim semantics, fused
//! `lance_fts` + `lance_vector_search` SQL, non-atomic two-op
//! writes that exploit Lance's no-transactions stance) are marked
//! **LANCE-SPECIFIC** in their doc comments. The unmarked default is
//! portable. This labelling is the input to the
//! `docs/backend-coupling.md` §6 Phase 2+ trait extraction —
//! anything marked LANCE-SPECIFIC has to be re-shaped before it can
//! land on a trait that a Postgres / SQLite / in-memory backend can
//! implement.
//!
//! Phase 5 (2026-05-18) made `Store` an implementation detail behind
//! the `Backend` umbrella trait — services / workers hold
//! `Arc<dyn Backend>`. The 9 sub-traits in `src/storage/*.rs`
//! delegate to either `self.lance.xxx` (writes; a handful of reads
//! kept here for write hot-paths) or `self.query.xxx` (reads,
//! canonical). Both halves are `pub(crate)` since Phase 5+ — the
//! concrete types only appear inside this file and `app.rs`.

use std::path::Path;
use std::sync::Arc;

use super::duckdb_query::DuckDbQuery;
use super::lance_store::LanceStore;
use super::{
    ClaimedEmbeddingJob, ClaimedTranscriptEmbeddingJob, ContextWindow, EmbeddingJobInsert,
    FeedbackEvent, GraphError, StorageError, TranscriptSessionSummary,
};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleVersionLink, GraphEdge,
};
use crate::domain::episode::EpisodeRecord;
use crate::domain::session::Session;
use crate::domain::{AddAliasOutcome, ConversationMessage, Entity, EntityKind, EntityWithAliases};

/// Handle carried by every service / worker / HTTP component. Cheap
/// to clone (just three `Arc`s).
#[derive(Clone)]
pub struct Store {
    /// Writes (and a handful of yet-to-be-migrated reads) flow here.
    pub(crate) lance: Arc<LanceStore>,
    /// Reads flow here.
    pub(crate) query: Arc<DuckDbQuery>,
    /// Per-bucket read-engine switch (`MEM_READ_ENGINE`) for the route-B
    /// migration. Each read method routes on this; default `DuckDb`. See
    /// `docs/remove-duckdb-keep-lance.md`.
    pub(crate) read_engine: crate::config::ReadEngine,
    /// Open-time advisory lock — held for the full lifetime of every
    /// `Store` clone (`Arc` keeps it alive until the last clone drops).
    /// `None` when `MEM_OPEN_LOCK_DISABLED=1` skipped acquisition. See
    /// `storage::open_lock` for the design rationale (incident TODO #3
    /// — multi-process write detection).
    _open_lock: Arc<Option<crate::storage::open_lock::OpenLock>>,
}

impl Store {
    /// Open both halves at `path` (a directory holding lance datasets).
    /// Creates the directory + lance datasets via `LanceStore::open`,
    /// then opens an in-process DuckDB and ATTACHes the lance dir.
    ///
    /// **Advisory lock**: refuses to open if another `mem` process
    /// already holds a lock on `<path>.lock`. Opt out with
    /// `MEM_OPEN_LOCK_DISABLED=1` (see `storage::open_lock`).
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let lock = crate::storage::open_lock::acquire(path)?;
        let lance = LanceStore::open(path).await?;
        let query = DuckDbQuery::open(path).await?;
        Ok(Self {
            lance: Arc::new(lance),
            query: Arc::new(query),
            read_engine: crate::config::parse_read_engine(|k| std::env::var(k).ok()),
            _open_lock: Arc::new(lock),
        })
    }

    /// Like [`Self::open`] but forces the read engine, bypassing
    /// `MEM_READ_ENGINE`. Used by the parity double-run
    /// (`tests/parity_golden.rs`) to read the same seeded dataset under each
    /// engine without racing a process-global env var.
    pub async fn open_with_read_engine(
        path: impl AsRef<Path>,
        read_engine: crate::config::ReadEngine,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let lock = crate::storage::open_lock::acquire(path)?;
        let lance = LanceStore::open(path).await?;
        let query = DuckDbQuery::open(path).await?;
        Ok(Self {
            lance: Arc::new(lance),
            query: Arc::new(query),
            read_engine,
            _open_lock: Arc::new(lock),
        })
    }

    /// Like [`Self::open`], but registers an [`EmbeddingProvider`] on
    /// the LanceStore side so vector columns can declare auto-embed
    /// against `<provider>-<model>` via `EmbeddingDefinition`. The
    /// DuckDB query side is unaffected — it reads whatever vectors
    /// are on disk regardless of who computed them.
    ///
    /// Acquires the same multi-process write guard as [`Self::open`].
    ///
    /// [`EmbeddingProvider`]: crate::embedding::EmbeddingProvider
    pub async fn open_with_provider(
        path: impl AsRef<Path>,
        provider: Arc<dyn crate::embedding::EmbeddingProvider>,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let lock = crate::storage::open_lock::acquire(path)?;
        let lance = LanceStore::open_with_provider(path, provider).await?;
        let query = DuckDbQuery::open(path).await?;
        Ok(Self {
            lance: Arc::new(lance),
            query: Arc::new(query),
            read_engine: crate::config::parse_read_engine(|k| std::env::var(k).ok()),
            _open_lock: Arc::new(lock),
        })
    }
}

impl Store {
    /// Chain a completed `LanceStore` write with the `DuckDbQuery`
    /// refresh ceremony the `Store` contract requires. Writes go
    /// to lance; reads after the call must see the new version, so
    /// the in-process DuckDB connection is reset (see
    /// [`DuckDbQuery::refresh`] for why).
    ///
    /// `result` is the **already-awaited** outcome of the lance
    /// write — callers compute the value first, then hand it to
    /// this method. The pre-computed-result shape avoids the
    /// closure-of-borrowed-future lifetime gymnastics a
    /// `commit_write(write_fn)` form would need, at the cost of
    /// repeating `self.lance.foo(...).await` at the callsite (which
    /// was already there inside the legacy `lance_write_then_refresh!`
    /// macro this method replaced).
    ///
    /// Refresh failures surface as `StorageError`; the write has
    /// already committed at that point, so the caller sees the
    /// value the write produced even if a future read from the
    /// same `Store` happens to see a stale version (it'll converge
    /// on the next mutation).
    ///
    /// Closes `backend-coupling.md` §6 Phase 1 QW-6. In Phase 2,
    /// this ceremony will live on the LanceBackend impl behind the
    /// portable trait surface; other backends won't need it.
    pub(crate) async fn commit_lance_write<T>(
        &self,
        result: Result<T, StorageError>,
    ) -> Result<T, StorageError> {
        // **D2 (2026-05-22)**: deferred refresh — flip the dirty flag
        // here (cheap atomic store) instead of eagerly calling
        // `refresh()` (which is ~100ms each: `Connection::open_in_memory`
        // + `INSTALL lance; LOAD lance;` + `ATTACH 12 tables`). The
        // next read on this `DuckDbQuery` instance calls
        // `ensure_fresh()` which sees the dirty flag and pays the
        // refresh cost once for N intervening writes.
        //
        // Trade-off: a caller that issues 10 writes back-to-back used
        // to pay 10×100ms; now pays 0×100ms + one refresh on whatever
        // read comes next. For worker tick loops (which write often
        // but read rarely on the same connection), this is the dominant
        // CPU win. For interactive flows (write → immediately read on
        // a different connection), the refresh shifts from write-time
        // to read-time but the user-visible latency stays the same.
        //
        // Mark-dirty + ensure_fresh ordering is documented on
        // `DuckDbQuery::ensure_fresh` — concurrent writers can't lose
        // updates because dirty=true is monotonic until the next
        // reader's atomic swap.
        self.query.mark_dirty();
        result
    }
}

// ── Memory writes (LanceStore + DuckDbQuery refresh) ────────────────
impl Store {
    pub async fn insert_capability_capsule(
        &self,
        m: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.commit_lance_write(self.lance.insert_capability_capsule(m).await)
            .await
    }

    /// Multi-row insert. Single Lance write + single DuckDB refresh,
    /// regardless of `memories.len()`. No-op (and no refresh) when empty.
    pub async fn insert_capability_capsules(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError> {
        if memories.is_empty() {
            return Ok(());
        }
        self.commit_lance_write(self.lance.insert_capability_capsules_batch(memories).await)
            .await
    }

    pub async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        self.commit_lance_write(self.lance.try_enqueue_embedding_job(insert).await)
            .await
    }

    /// Multi-row variant of [`Self::try_enqueue_embedding_job`]. Skips the
    /// per-row `(tenant, capability_capsule_id, target_content_hash,
    /// provider)` idempotency probe that the single-row form runs — the
    /// caller (service-level batch ingest) only invokes this immediately
    /// after a fresh `insert_capability_capsules`, so by construction no
    /// live job can already exist for those tuples. No-op when empty.
    pub async fn enqueue_embedding_jobs(
        &self,
        inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        if inserts.is_empty() {
            return Ok(());
        }
        self.commit_lance_write(self.lance.enqueue_embedding_jobs_batch(inserts).await)
            .await
    }

    /// **LANCE-SPECIFIC**: claim is an `update().only_if(...)` whose
    /// `rows_updated == 0` branch is what we read as "another worker
    /// got there first." Portable equivalent is Postgres `SELECT FOR
    /// UPDATE SKIP LOCKED` or Redis `BLPOP` — different shape, same
    /// outcome. Trait extraction has to abstract the claim primitive,
    /// not lift this signature.
    pub async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        self.commit_lance_write(
            self.lance
                .claim_next_n_embedding_jobs(now, max_retries, n)
                .await,
        )
        .await
    }

    /// **LANCE-SPECIFIC**: `capability_capsule_embeddings` is
    /// lazy-created on first call because the vector dim is
    /// provider-dependent and unknown at `Store::open` time. Portable
    /// backends would either ALTER on dim change (pgvector) or build
    /// a separate vector store; the lazy-table-create dance must move
    /// into backend-specific bootstrap, not stay on the trait.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        embedding_blob: &[u8],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .upsert_capability_capsule_embedding(
                    capability_capsule_id,
                    tenant,
                    embedding_model,
                    embedding_dim,
                    embedding_blob,
                    content_hash,
                    source_updated_at,
                    now,
                )
                .await,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_capability_capsule_embedding_chunks(
        &self,
        capability_capsule_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        vectors: &[Vec<f32>],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .upsert_capability_capsule_embedding_chunks(
                    capability_capsule_id,
                    tenant,
                    embedding_model,
                    embedding_dim,
                    vectors,
                    content_hash,
                    source_updated_at,
                    now,
                )
                .await,
        )
        .await
    }

    pub async fn delete_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .delete_capability_capsule_embedding(capability_capsule_id)
                .await,
        )
        .await
    }

    pub async fn complete_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(self.lance.complete_embedding_job(job_id, now).await)
            .await
    }

    pub async fn mark_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(self.lance.mark_embedding_job_stale(job_id, now).await)
            .await
    }

    pub async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .reschedule_embedding_job_failure(
                    job_id,
                    new_attempt_count,
                    last_error,
                    available_at,
                    now,
                )
                .await,
        )
        .await
    }

    pub async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .permanently_fail_embedding_job(job_id, new_attempt_count, last_error, now)
                .await,
        )
        .await
    }

    pub async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        self.commit_lance_write(
            self.lance
                .delete_embedding_jobs_by_capability_capsule_id(capability_capsule_id)
                .await,
        )
        .await
    }

    /// The single status-transition primitive (LanceStore +
    /// DuckDbQuery dirty-mark). `accept_pending` / `reject_pending` /
    /// O2 review-flagging are thin callers.
    pub async fn set_capsule_status(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        status: crate::domain::capability_capsule::CapabilityCapsuleStatus,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.commit_lance_write(
            self.lance
                .set_capsule_status(tenant, capability_capsule_id, status)
                .await,
        )
        .await
    }

    /// **LANCE-SPECIFIC**: two-op sequence (archive old + insert
    /// successor) with no transactional boundary — Lance doesn't
    /// have multi-row transactions, so a crash between the two ops
    /// leaves the old row archived without a successor. Portable
    /// backends could wrap in BEGIN/COMMIT; the trait should expose
    /// a single `supersede` primitive rather than this 2-step shape.
    pub async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.commit_lance_write(
            self.lance
                .replace_pending_with_successor(tenant, original_memory_id, successor)
                .await,
        )
        .await
    }

    /// **LANCE-SPECIFIC**: writes the `feedback_events` row first,
    /// then updates the parent capsule's
    /// `confidence` / `decay_score` / `status` / `last_validated_at`
    /// in a separate Lance call. No transactional boundary — partial
    /// commits are possible (audit row exists but parent didn't move).
    /// Portable backends should expose `apply_feedback` as a single
    /// atomic operation; the current contract is leaking Lance's
    /// no-transactions stance.
    pub async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        self.commit_lance_write(self.lance.apply_feedback(memory, feedback).await)
            .await
    }

    pub async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .delete_capability_capsule_hard(tenant, capability_capsule_id)
                .await,
        )
        .await
    }

    pub async fn insert_episode(
        &self,
        episode: EpisodeRecord,
    ) -> Result<EpisodeRecord, StorageError> {
        self.commit_lance_write(self.lance.insert_episode(episode).await)
            .await
    }

    pub async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        self.commit_lance_write(
            self.lance
                .stale_live_embedding_jobs_for_capability_capsule(
                    tenant,
                    capability_capsule_id,
                    provider,
                    now,
                )
                .await,
        )
        .await
    }
}

// ── Memory reads (DuckDbQuery) ──────────────────────────────────────
impl Store {
    pub async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        match self.read_engine {
            crate::config::ReadEngine::DuckDb => {
                self.query.list_capability_capsules_for_tenant(tenant).await
            }
            crate::config::ReadEngine::Lance => {
                self.lance.list_capability_capsules_for_tenant(tenant).await
            }
        }
    }

    pub async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        self.query.list_wings(tenant).await
    }

    pub async fn capsule_stats(
        &self,
        tenant: &str,
    ) -> Result<crate::domain::capability_capsule::CapsuleStats, StorageError> {
        match self.read_engine {
            crate::config::ReadEngine::DuckDb => self.query.capsule_stats(tenant).await,
            crate::config::ReadEngine::Lance => self.lance.capsule_stats(tenant).await,
        }
    }

    pub async fn get_taxonomy(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        match self.read_engine {
            crate::config::ReadEngine::DuckDb => self.query.get_taxonomy(tenant).await,
            crate::config::ReadEngine::Lance => self.lance.get_taxonomy(tenant).await,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_capability_capsules_in_scope(
        &self,
        tenant: &str,
        project: Option<&str>,
        repo: Option<&str>,
        module: Option<&str>,
        capsule_type: Option<&str>,
        status: Option<&str>,
        source_agent: Option<&str>,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), StorageError> {
        self.query
            .list_capability_capsules_in_scope(
                tenant,
                project,
                repo,
                module,
                capsule_type,
                status,
                source_agent,
                cursor,
                limit,
            )
            .await
    }

    pub async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        self.query
            .get_capability_capsule_for_tenant(tenant, capability_capsule_id)
            .await
    }

    pub async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        self.query.get_pending(tenant, capability_capsule_id).await
    }

    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        self.query
            .find_by_idempotency_or_hash(tenant, idempotency_key, content_hash)
            .await
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        self.query.list_pending_review(tenant).await
    }

    /// Auto-promote candidate set. Returns rows that match the
    /// `(status=PendingConfirmation, type∈types, updated_at<cutoff,
    /// decay_score<max_decay_score)` filter — see
    /// `DuckDbQuery::auto_promote_candidates` for full semantics.
    /// Sweep itself is in `crate::worker::auto_promote_worker`.
    pub async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[crate::domain::capability_capsule::CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        self.query
            .auto_promote_candidates(tenant, cutoff_updated_at, types, max_decay_score)
            .await
    }

    pub async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        self.query.search_candidates(tenant).await
    }

    pub async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        self.query
            .recent_active_capability_capsules(tenant, limit)
            .await
    }

    pub async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        self.query
            .fetch_capability_capsules_by_ids(tenant, ids)
            .await
    }

    pub async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        self.query
            .list_capability_capsule_ids_for_tenant(tenant)
            .await
    }

    pub async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        self.query
            .list_capability_capsule_versions_for_tenant(tenant, capability_capsule_id)
            .await
    }

    /// Cross-table hybrid recall: BM25 + vector + RRF fused inline
    /// in DuckDB SQL. Returns `(record, rrf_score)` ordered by
    /// `(rrf_score DESC, updated_at DESC, capability_capsule_id ASC)`.
    /// See `DuckDbQuery::hybrid_candidates` for the full contract.
    ///
    /// **LANCE-SPECIFIC** (kept as a LanceBackend optimization): the
    /// implementation fuses `lance_fts(...)` and `lance_vector_search(...)`
    /// in a single statement, which the QW-1 bench
    /// (`examples/hybrid_compose_vs_fused_bench.rs`) measured at
    /// **14–29% faster** than the equivalent Rust-compose form at
    /// `k ∈ {10, 50, 100}` over N=500 capsules.
    ///
    /// **Portable equivalent**: [`Self::hybrid_candidates_compose`]
    /// produces the same scores and orderings via
    /// [`Self::bm25_candidate_ids`] + [`Self::ann_candidate_ids`] +
    /// `pipeline::ranking::rrf_merge` + the existing
    /// `fetch_capability_capsules_by_ids`. That's the path future
    /// backends (Postgres, SQLite, in-memory) will compose; their
    /// `hybrid_candidates` impl can either route to the portable
    /// compose path or wire up a backend-specific fusion.
    pub async fn hybrid_candidates(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        self.query
            .hybrid_candidates(tenant, query_text, query_embedding, k)
            .await
    }

    /// Backend-portable compose form of [`Self::hybrid_candidates`]:
    /// `bm25_candidate_ids` + `ann_candidate_ids` + `rrf_merge` +
    /// `fetch_capability_capsules_by_ids` + final sort. Produces the
    /// same scores and orderings as the fused-SQL default to within
    /// f32 rounding noise.
    ///
    /// Used by `examples/hybrid_compose_vs_fused_bench.rs` and serves
    /// as the reference shape for the Phase 2 trait — backends that
    /// can't fuse BM25 + ANN in a single SQL statement should route
    /// their `hybrid_candidates` impl through compose. On LanceBackend
    /// this path is currently slower (see the fused-SQL doc above);
    /// don't use it in hot paths.
    pub async fn hybrid_candidates_compose(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        let has_text = !query_text.trim().is_empty();
        let has_vec = !query_embedding.is_empty();
        if (!has_text && !has_vec) || k == 0 {
            return Ok(Vec::new());
        }

        // Oversample each candidate set so the post-filter (status /
        // capsule_type) doesn't truncate the merged result below k.
        // Same `k * 2` posture the fused-SQL query carries inside its
        // CTE definitions.
        let oversample = k.saturating_mul(2);
        let bm25 = if has_text {
            self.bm25_candidate_ids(tenant, query_text, oversample)
                .await?
        } else {
            Vec::new()
        };
        let ann = if has_vec {
            self.ann_candidate_ids(tenant, query_embedding, oversample)
                .await?
        } else {
            Vec::new()
        };

        let merged = crate::pipeline::ranking::rrf_merge(&bm25, &ann);
        if merged.is_empty() {
            return Ok(Vec::new());
        }

        // Hydrate full capsule rows. Oversample again (3x k, bounded
        // by merged length) so post-fetch status/diary filtering
        // doesn't drop us under k. `fetch_capability_capsules_by_ids`
        // doesn't filter status/type, so we re-check in Rust below.
        let fetch_n = (k.saturating_mul(3)).min(merged.len());
        let top_ids: Vec<&str> = merged
            .iter()
            .take(fetch_n)
            .map(|(id, _)| id.as_str())
            .collect();
        let records = self
            .fetch_capability_capsules_by_ids(tenant, &top_ids)
            .await?;

        // Rebuild (record, score) pairs, dropping archived/rejected/diary.
        let score_by_id: std::collections::HashMap<&str, f32> =
            merged.iter().map(|(id, s)| (id.as_str(), *s)).collect();
        let mut out: Vec<(CapabilityCapsuleRecord, f32)> = records
            .into_iter()
            .filter(|r| {
                !matches!(
                    r.status,
                    crate::domain::capability_capsule::CapabilityCapsuleStatus::Archived
                        | crate::domain::capability_capsule::CapabilityCapsuleStatus::Rejected,
                ) && !matches!(
                    r.capability_capsule_type,
                    crate::domain::capability_capsule::CapabilityCapsuleType::Diary,
                )
            })
            .map(|r| {
                let s = *score_by_id
                    .get(r.capability_capsule_id.as_str())
                    .unwrap_or(&0.0);
                (r, s)
            })
            .collect();

        // Final ordering: rrf_score DESC, updated_at DESC, id ASC —
        // matches the fused-SQL outer ORDER BY.
        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.0.updated_at.cmp(&a.0.updated_at))
                .then_with(|| a.0.capability_capsule_id.cmp(&b.0.capability_capsule_id))
        });
        out.truncate(k);
        Ok(out)
    }

    /// **LANCE-SPECIFIC** (see `DuckDbQuery::bm25_candidate_ids`).
    /// Backend-portable callers shouldn't reach for this directly —
    /// they should use [`Self::hybrid_candidates`] which composes
    /// BM25 + ANN behind a backend-agnostic shell.
    pub async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        self.query.bm25_candidate_ids(tenant, query_text, k).await
    }

    /// **LANCE-SPECIFIC** (see `DuckDbQuery::ann_candidate_ids`).
    /// Returns an empty Vec when the lazy-created embeddings table
    /// doesn't exist yet. Backend-portable callers should reach for
    /// [`Self::hybrid_candidates`] instead.
    pub async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        match self.read_engine {
            crate::config::ReadEngine::DuckDb => {
                self.query
                    .ann_candidate_ids(tenant, query_embedding, k)
                    .await
            }
            crate::config::ReadEngine::Lance => {
                self.lance
                    .ann_candidate_ids(tenant, query_embedding, k)
                    .await
            }
        }
    }
}

// The 7 lance-side reads previously routed through inherent Store
// methods (with stale "TODO: route to DuckDbQuery once added" markers
// inviting a future SQL-port that never came and isn't needed now)
// got inlined into the trait impls directly:
//
//   CapsuleStore::feedback_summary           → self.lance.feedback_summary
//   CapsuleStore::get_capability_capsule     → self.lance.get_capability_capsule
//   SessionStore::latest_active_session      → self.lance.latest_active_session
//   SessionStore::list_successful_episodes_for_tenant
//                                            → self.lance.list_successful_episodes_for_tenant
//   EmbeddingJobStore::list_embedding_jobs   → self.lance.list_embedding_jobs
//   EmbeddingJobStore::latest_embedding_job_status_for_hash
//                                            → self.lance.latest_embedding_job_status_for_hash
//   EmbeddingVectorStore::get_capability_capsule_embedding_row
//                                            → self.lance.get_capability_capsule_embedding_row
//
// Service / worker callers use `Arc<dyn Backend>` (Phase 5) so the
// trait method is the only reachable entry point — the inherent
// middleman was scaffolding from before Phase 5.

impl Store {
    /// Read embedding-job status by id. Used by the embedding worker
    /// to skip mid-flight processing when a concurrent caller has
    /// already marked the job stale. Routes to DuckDbQuery (SQL).
    pub async fn get_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        self.query.get_embedding_job_status(job_id).await
    }

    /// Same shape as [`Self::get_embedding_job_status`] for the
    /// transcript-side queue.
    pub async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        self.query.get_transcript_embedding_job_status(job_id).await
    }

    /// Prune Lance version manifests older than `older_than_days`
    /// across every managed table. Issued through LanceStore's Rust
    /// API (not DuckDB SQL), so a DuckDB-side `refresh()` is needed
    /// afterwards to invalidate the snapshot cache — see
    /// [`Self::commit_lance_write`]. Driven by
    /// `crate::worker::vacuum_worker` on a daily cadence and
    /// exposed on-demand via `POST /admin/vacuum`.
    ///
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

    /// Stamp `last_used_at = now` on a batch of capsules (roadmap O1
    /// retrieval reinforcement). Routes to DuckDbQuery — a single SQL
    /// UPDATE via the lance extension, which invalidates the
    /// connection's own cache, so no `Store::refresh` is needed (same
    /// model as [`Self::apply_time_decay`]). Driven off the read path by
    /// `crate::worker::last_used_worker`. Best-effort — see
    /// `DuckDbQuery::bump_last_used_at` for why no rowcount is returned.
    pub async fn bump_last_used_at(
        &self,
        tenant: &str,
        capability_capsule_ids: &[String],
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        self.query
            .bump_last_used_at(tenant, capability_capsule_ids, now_ms_str)
            .await
    }

    /// Session lifecycle (touch / open / close) — all mutations.
    /// Routed to LanceStore + DuckDbQuery refresh.
    pub async fn touch_session(
        &self,
        session_id: &str,
        last_active_at: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(self.lance.touch_session(session_id, last_active_at).await)
            .await
    }

    pub async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        self.commit_lance_write(
            self.lance
                .open_session(session_id, tenant, caller_agent, now)
                .await,
        )
        .await
    }

    pub async fn close_session(
        &self,
        session_id: &str,
        ended_at: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(self.lance.close_session(session_id, ended_at).await)
            .await
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
        self.commit_lance_write(self.lance.create_conversation_message(msg).await)
            .await
    }

    /// Multi-row variant of [`Self::create_conversation_message`]. One
    /// bulk dedup probe + one Lance write for the messages table + one
    /// Lance write for the embedding-jobs table + one DuckDB refresh,
    /// regardless of `msgs.len()`. Returns the number of rows that
    /// actually landed (input minus dedup-skipped rows). No-op (and no
    /// refresh) when empty.
    pub async fn create_conversation_messages(
        &self,
        msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        if msgs.is_empty() {
            return Ok(0);
        }
        self.commit_lance_write(self.lance.create_conversation_messages_batch(msgs).await)
            .await
    }

    /// **LANCE-SPECIFIC**: same shape as
    /// [`Self::claim_next_n_embedding_jobs`] — Lance
    /// `update().only_if()` + `rows_updated` optimistic claim. Same
    /// portability caveat: the trait should abstract the claim
    /// primitive, not lift this signature.
    pub async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        self.commit_lance_write(
            self.lance
                .claim_next_n_transcript_embedding_jobs(now, max_retries, n)
                .await,
        )
        .await
    }

    pub async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .complete_transcript_embedding_job(job_id, now)
                .await,
        )
        .await
    }

    pub async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .mark_transcript_embedding_job_stale(job_id, now)
                .await,
        )
        .await
    }

    pub async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .reschedule_transcript_embedding_job_failure(
                    job_id,
                    new_attempt_count,
                    last_error,
                    available_at,
                    now,
                )
                .await,
        )
        .await
    }

    pub async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .permanently_fail_transcript_embedding_job(
                    job_id,
                    new_attempt_count,
                    last_error,
                    now,
                )
                .await,
        )
        .await
    }

    /// Upsert a transcript-block embedding (transcript embedding
    /// worker's hot path). Routes to LanceStore + DuckDbQuery refresh.
    ///
    /// **LANCE-SPECIFIC**: `conversation_message_embeddings` is
    /// lazy-created on first call (provider-dependent dim) — same
    /// caveat as [`Self::upsert_capability_capsule_embedding`].
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
        self.commit_lance_write(
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
                .await,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_conversation_message_embedding_chunks(
        &self,
        message_block_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        vectors: &[Vec<f32>],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .upsert_conversation_message_embedding_chunks(
                    message_block_id,
                    tenant,
                    embedding_model,
                    embedding_dim,
                    vectors,
                    content_hash,
                    source_updated_at,
                    now,
                )
                .await,
        )
        .await
    }

    pub async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError> {
        self.commit_lance_write(
            self.lance
                .delete_conversation_message_embedding(message_block_id)
                .await,
        )
        .await
    }

    /// Semantic recall over transcript blocks. Routes to DuckDbQuery
    /// (lance_vector_search SQL + JOIN conversation_messages, cosine
    /// similarity via `1 - L²/2` for normalized embeddings).
    ///
    /// **LANCE-SPECIFIC**: depends on the lance DuckDB extension's
    /// `lance_vector_search(...)` SQL table function. Trait
    /// extraction should expose a portable `top_k_vector_candidates`
    /// primitive that each backend implements with its own ANN path.
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
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        self.query
            .get_conversation_messages_by_session_paged(
                tenant, session_id, since, until, role, block_type, cursor, limit,
            )
            .await
    }

    pub async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        self.query.list_transcript_sessions(tenant).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn list_conversation_messages_in_range(
        &self,
        tenant: &str,
        time_from: Option<&str>,
        time_to: Option<&str>,
        role: Option<&str>,
        block_type: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        self.query
            .list_conversation_messages_in_range(
                tenant, time_from, time_to, role, block_type, cursor, limit,
            )
            .await
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

    /// **LANCE-SPECIFIC**: uses `lance_fts(...)` SQL table function
    /// from the lance DuckDB extension. Trait extraction should
    /// expose a `top_k_bm25_candidates` primitive — each backend can
    /// implement via its native FTS (Postgres tsvector + GIN, SQLite
    /// FTS5, Tantivy, etc).
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

    pub async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.neighbors_within(node_id, max_hops, as_of).await
    }

    pub async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.kg_timeline(node_id).await
    }

    pub async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.query_predicate(predicate, as_of).await
    }

    pub async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.list_user_tunnels(limit).await
    }

    pub async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.find_tunnels(prefix_a, prefix_b, limit).await
    }

    pub async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        self.query.follow_tunnels(node_id, max_hops).await
    }

    pub async fn graph_stats(
        &self,
    ) -> Result<crate::domain::capability_capsule::GraphStats, GraphError> {
        self.query.graph_stats().await
    }

    pub async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        self.query.related_capability_capsule_ids(node_ids).await
    }

    pub async fn incident_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError> {
        self.query.incident_edges_for_nodes(node_ids).await
    }

    pub async fn sync_memory_edges(
        &self,
        edges: &[GraphEdge],
        now: &str,
    ) -> Result<(), GraphError> {
        let result = self.lance.sync_memory_edges(edges, now).await;
        // D2: defer refresh — flip dirty flag, let next read pay it.
        self.query.mark_dirty();
        result
    }

    /// Caller-supplied direct edge write. Goes through the same Lance
    /// table as `sync_memory_edges` but preserves the caller's
    /// `valid_from` / `valid_to` verbatim (no server-side `now`
    /// override). Idempotent on active `(from, to, relation)`.
    pub async fn add_edge_direct(&self, edge: &GraphEdge) -> Result<bool, GraphError> {
        let result = self.lance.add_edge_direct(edge).await;
        self.query.mark_dirty();
        result
    }

    /// K9: apply one Hebbian potentiation to the active
    /// `(from, to, relation)` edge — read its current dynamics, run
    /// [`crate::domain::edge_dynamics::potentiate`], write the four K9
    /// columns back. Returns `false` (a dropped no-op) when the edge is
    /// no longer active. Called by the potentiation worker, off the read
    /// path.
    pub async fn potentiate_edge(
        &self,
        from_node_id: &str,
        to_node_id: &str,
        relation: &str,
        now: &str,
    ) -> Result<bool, GraphError> {
        let Some(mut edge) = self
            .query
            .get_active_edge(from_node_id, to_node_id, relation)
            .await?
        else {
            return Ok(false);
        };
        crate::domain::edge_dynamics::potentiate(&mut edge, now);
        let written = self.lance.update_edge_dynamics(&edge).await?;
        self.query.mark_dirty();
        Ok(written)
    }

    /// Invalidate one specific `(from, predicate, to)` active edge by
    /// stamping `valid_to = ended_at`. Idempotent — returns 0 when
    /// the triple has no active edge.
    pub async fn invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: &str,
    ) -> Result<usize, GraphError> {
        let result = self
            .lance
            .invalidate_edge(from_node_id, predicate, to_node_id, ended_at)
            .await;
        self.query.mark_dirty();
        result
    }

    pub async fn close_edges_for_capability_capsule(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        let result = self
            .lance
            .close_edges_for_capability_capsule(capability_capsule_id)
            .await;
        self.query.mark_dirty();
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
        self.commit_lance_write(self.lance.resolve_or_create(tenant, alias, kind, now).await)
            .await
    }

    pub async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        self.commit_lance_write(self.lance.add_alias(tenant, entity_id, alias, now).await)
            .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use tempfile::tempdir;

    fn fixture(capability_capsule_id: &str, tenant: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: capability_capsule_id.into(),
            tenant: tenant.into(),
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            status: CapabilityCapsuleStatus::Active,
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
            supersedes_capability_capsule_id: None,
            source_agent: "test".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
            last_validated_at: None,
            last_used_at: None,
            last_recalled_at: None,
            expires_at: None,
        }
    }

    /// Cross-stack round-trip: writes via the lance half are
    /// immediately visible to reads via the duckdb-query half,
    /// because every `Store` write triggers a `DuckDbQuery::refresh`
    /// (rebuild of the in-process DuckDB connection). Without that
    /// refresh the lance extension's snapshot cache would hide
    /// post-attach lance writes — see [`Store::commit_lance_write`]
    /// for the gory details.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_open_write_read_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();

        // First write → first read: m_a visible.
        let m = fixture("m_a", "tenant-a");
        store.insert_capability_capsule(m.clone()).await.unwrap();
        let got = store
            .get_capability_capsule_for_tenant("tenant-a", "m_a")
            .await
            .unwrap()
            .expect("m_a visible after insert");
        assert_eq!(got.capability_capsule_id, "m_a");
        assert_eq!(got.evidence, vec!["src/main.rs:42".to_string()]);
        // Cross-tenant scope.
        let none = store
            .list_capability_capsules_for_tenant("does-not-exist")
            .await
            .unwrap();
        assert!(none.is_empty());
        let all = store
            .list_capability_capsules_for_tenant("tenant-a")
            .await
            .unwrap();
        assert_eq!(all.len(), 1);

        // Second write: previously hidden by the snapshot cache; now
        // refresh is wired so it shows up.
        let mut p = fixture("m_pending", "tenant-a");
        p.status = CapabilityCapsuleStatus::PendingConfirmation;
        store.insert_capability_capsule(p).await.unwrap();
        let after = store
            .list_capability_capsules_for_tenant("tenant-a")
            .await
            .unwrap();
        assert_eq!(after.len(), 2, "second write must be visible");

        let pre = store
            .get_capability_capsule_for_tenant("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("pending row visible after second insert + refresh");
        assert_eq!(pre.status, CapabilityCapsuleStatus::PendingConfirmation);

        // UPDATE via set_capsule_status (lance Table::update) — the
        // hardest case: lance UPDATE wasn't visible at all without
        // refresh.
        let accepted = store
            .set_capsule_status("tenant-a", "m_pending", CapabilityCapsuleStatus::Active)
            .await
            .unwrap();
        assert_eq!(accepted.status, CapabilityCapsuleStatus::Active);
        let post = store
            .get_capability_capsule_for_tenant("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("row visible to SQL after lance UPDATE + refresh");
        assert_eq!(post.status, CapabilityCapsuleStatus::Active);
    }

    /// `get_embedding_job_status`: enqueue a job via the lance side,
    /// read its status through DuckDbQuery (SQL), confirm round-trip.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_get_embedding_job_status_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();

        store
            .insert_capability_capsule(fixture("m_e", "tenant-a"))
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
                capability_capsule_id: "m_e".into(),
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
        store.insert_capability_capsule(active).await.unwrap();

        // Saturated row should not move (`decay_score < 1.0` filter).
        let mut sat = fixture("m_sat", "tenant-a");
        sat.created_at = ten_days_ago_str.clone();
        sat.updated_at = ten_days_ago_str.clone();
        sat.decay_score = 1.0;
        store.insert_capability_capsule(sat).await.unwrap();

        // Non-active row should not move (status='active' filter).
        let mut prov = fixture("m_prov", "tenant-a");
        prov.status = CapabilityCapsuleStatus::PendingConfirmation;
        prov.created_at = ten_days_ago_str.clone();
        prov.updated_at = ten_days_ago_str.clone();
        prov.decay_score = 0.0;
        store.insert_capability_capsule(prov).await.unwrap();

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
            .get_capability_capsule_for_tenant("tenant-a", "m_decay")
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
            .get_capability_capsule_for_tenant("tenant-a", "m_sat")
            .await
            .unwrap()
            .unwrap();
        assert!((sat_after.decay_score - 1.0).abs() < 1e-6);

        let prov_after = store
            .get_capability_capsule_for_tenant("tenant-a", "m_prov")
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
            meta_json: None,
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

    /// Cross-stack batch round-trip: a multi-row insert reaches DuckDB
    /// after a single refresh, and the rows survive intra-batch dedup
    /// of identical (transcript_path, line_number, block_index) keys.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_create_conversation_messages_batch_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();
        store.set_transcript_job_provider("fake-test");

        let a = cm("blk_a", "tenant-a", 1, 0, true, "00000001778000000010");
        let b = cm("blk_b", "tenant-a", 2, 0, false, "00000001778000000020");
        let inserted = store
            .create_conversation_messages(&[a.clone(), b.clone()])
            .await
            .unwrap();
        assert_eq!(inserted, 2);

        let rows = store
            .get_conversation_messages_by_session("tenant-a", "sess")
            .await
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|m| m.message_block_id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"blk_a"));
        assert!(ids.contains(&"blk_b"));
    }

    /// Cross-stack batch capsule insert: multiple capsules land via a
    /// single refresh and are visible to the DuckDB read side.
    #[tokio::test(flavor = "multi_thread")]
    async fn store_insert_capability_capsules_batch_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let store = Store::open(&path).await.unwrap();
        let m1 = fixture("m_b1", "tenant-a");
        let mut m2 = fixture("m_b2", "tenant-a");
        m2.content_hash = "j".repeat(64);
        store
            .insert_capability_capsules(std::slice::from_ref(&m1))
            .await
            .unwrap();
        // Second batch — verifies the refresh runs every call, not
        // just on the first write.
        store
            .insert_capability_capsules(std::slice::from_ref(&m2))
            .await
            .unwrap();
        let all = store
            .list_capability_capsules_for_tenant("tenant-a")
            .await
            .unwrap();
        let ids: Vec<&str> = all
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert!(ids.contains(&"m_b1"));
        assert!(ids.contains(&"m_b2"));

        // Empty batch is a no-op (does not panic, does not refresh
        // when there is nothing to write).
        store.insert_capability_capsules(&[]).await.unwrap();
    }
}
