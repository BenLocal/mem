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
//
// postgres-backend P4 — hybrid retrieval (pgvector ANN + tsvector BM25 +
// RRF fusion), behaviour-aligned with the Lance/DuckDB backend in
// `duckdb_query/capability_capsules.rs` and `pipeline/retrieve.rs`.
//
// Lance-semantic parity preserved:
//   - `search_candidates`: live status (NOT rejected/archived), exclude
//     `diary` type, exclude rows superseded by another *active* row
//     (version-chain dedup), ordered `updated_at DESC, version DESC, id ASC`;
//     optional `MEM_RECALL_POOL_LIMIT` lifecycle-pool cap with
//     `preference`/`workflow` guidance floor-exempt.
//   - `bm25_candidate_ids`: tsvector @@ plainto_tsquery('simple', q), live
//     status + non-diary filter, 1-based rank by ts_rank DESC then id ASC.
//   - `ann_candidate_ids`: pgvector `<=>` cosine distance, DISTINCT-ON dedup
//     over chunk rows (min distance per capsule), 1-based rank; missing
//     embeddings table short-circuits to empty (lazy-create parity).
//   - `hybrid_candidates`: RRF over the two channels with the EXACT formula
//     from `retrieve::sql_rrf` — `1/(60+rank)` per source, summed.
//
// per-source cap is a `pipeline::retrieve::finalize` concern (downstream of
// this layer), not applied in the candidate SQL — same as Lance, whose
// `hybrid_candidates` likewise does not apply the per-source cap.

use super::CapsuleStore;

/// RRF reciprocal-rank constant — mirrors `retrieve::sql_rrf` / the Lance
/// `hybrid_candidates` SQL (`1.0 / (60.0 + rank)`). Kept as a named const so
/// the two backends can't silently drift.
const RRF_K: f32 = 60.0;

/// Read the optional `MEM_RECALL_POOL_LIMIT` lifecycle-pool cap. Unset / 0 /
/// invalid → `None` (unbounded full pool — default). Mirrors the Lance
/// `search_candidates` env read.
fn recall_pool_limit() -> Option<usize> {
    std::env::var("MEM_RECALL_POOL_LIMIT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
}

#[async_trait]
impl CapsuleSearchStore for PostgresCapsuleStore {
    async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Live (non-rejected, non-archived), non-diary, version-chain
        // deduped pool. Optional MEM_RECALL_POOL_LIMIT cap keeps all
        // preference/workflow guidance plus the N most-recently-written
        // other rows. `n` is a parsed usize, safe to interpolate; tenant
        // is a bound param.
        let pool_limit = recall_pool_limit();
        let bound_clause = match pool_limit {
            Some(n) => format!(
                "AND (c.capability_capsule_type IN ('preference', 'workflow') \
                      OR c.capability_capsule_id IN ( \
                          SELECT capability_capsule_id FROM capability_capsules \
                          WHERE tenant = $1 AND status NOT IN ('rejected', 'archived') \
                            AND capability_capsule_type != 'diary' \
                          ORDER BY updated_at DESC LIMIT {n} \
                      )) "
            ),
            None => String::new(),
        };
        let cols = SELECT_COLUMNS
            .split(',')
            .map(|c| format!("c.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols} FROM capability_capsules c \
             WHERE c.tenant = $1 AND c.status NOT IN ('rejected', 'archived') \
               AND c.capability_capsule_type != 'diary' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM capability_capsules s \
                   WHERE s.supersedes_capability_capsule_id = c.capability_capsule_id \
                     AND s.tenant = c.tenant AND s.status = 'active' \
               ) \
               {bound_clause}\
             ORDER BY c.updated_at DESC, c.version DESC, c.capability_capsule_id ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_record).collect()
    }

    async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Same live filter as the Lance fast path: non-rejected,
        // non-archived, non-diary, ordered updated_at/version/id, bounded
        // limit clamped to [1, 1024].
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 AND status NOT IN ('rejected', 'archived') \
               AND capability_capsule_type != 'diary' \
             ORDER BY updated_at DESC, version DESC, capability_capsule_id ASC \
             LIMIT $2"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .bind(lim)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_record).collect()
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Identical contract to CapsuleStore::fetch_capability_capsules_by_ids
        // (same columns, same `tenant + id = ANY($2)` shape, empty
        // short-circuit, no order guarantee) — reuse it directly.
        CapsuleStore::fetch_capability_capsules_by_ids(self, tenant, ids).await
    }

    async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        // Project just the id column, ordered updated_at DESC. Admin reads
        // are NOT status/diary/supersede filtered (Lance does the same).
        let rows = sqlx::query(
            "SELECT capability_capsule_id FROM capability_capsules \
             WHERE tenant = $1 ORDER BY updated_at DESC",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                r.try_get::<String, _>("capability_capsule_id")
                    .map_err(sqlx_err)
            })
            .collect()
    }

    async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        // Recursive walk of the supersedes link, both directions
        // (predecessors + successors), tenant-filtered at every step,
        // ordered version DESC, updated_at DESC — mirrors the Lance
        // recursive CTE.
        let rows = sqlx::query(
            "WITH RECURSIVE chain AS ( \
                SELECT capability_capsule_id, version, status, updated_at, \
                       supersedes_capability_capsule_id \
                FROM capability_capsules \
                WHERE tenant = $1 AND capability_capsule_id = $2 \
              UNION \
                SELECT c.capability_capsule_id, c.version, c.status, c.updated_at, \
                       c.supersedes_capability_capsule_id \
                FROM capability_capsules c \
                JOIN chain ch \
                  ON c.capability_capsule_id = ch.supersedes_capability_capsule_id \
                  OR c.supersedes_capability_capsule_id = ch.capability_capsule_id \
                WHERE c.tenant = $1 \
            ) \
            SELECT capability_capsule_id, version, status, updated_at, \
                   supersedes_capability_capsule_id \
            FROM chain \
            ORDER BY version DESC, updated_at DESC",
        )
        .bind(tenant)
        .bind(capability_capsule_id)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                Ok(CapabilityCapsuleVersionLink {
                    capability_capsule_id: r.try_get("capability_capsule_id").map_err(sqlx_err)?,
                    version: r.try_get("version").map_err(sqlx_err)?,
                    status: parse_status_pub(&r.try_get::<String, _>("status").map_err(sqlx_err)?)?,
                    updated_at: r.try_get("updated_at").map_err(sqlx_err)?,
                    supersedes_capability_capsule_id: r
                        .try_get("supersedes_capability_capsule_id")
                        .map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    async fn hybrid_candidates(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        // Postgres routes the fused path through the portable compose form
        // (the Lance fused-SQL fast path is lance-extension specific). Same
        // outputs within f32 rounding per the trait doc.
        self.hybrid_candidates_compose(tenant, query_text, query_embedding, k)
            .await
    }

    async fn hybrid_candidates_compose(
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
        // Oversample mirrors the Lance fused query: FTS over the one-row-per-
        // capsule table gets k*2; the ANN channel gets k*4 because its
        // embeddings table holds N chunk-rows per capsule that collapse to
        // fewer distinct capsules after the per-capsule dedup.
        let lex = self
            .bm25_candidate_ids(tenant, query_text, k.saturating_mul(2))
            .await?;
        let sem = self
            .ann_candidate_ids(tenant, query_embedding, k.saturating_mul(4))
            .await?;

        // Rust-side RRF, byte-identical to retrieve::sql_rrf: per id, sum
        // 1/(RRF_K + rank) over whichever channels it appears in. `rank`
        // is the 1-based rank from each channel.
        use std::collections::HashMap;
        let mut rrf: HashMap<String, f32> = HashMap::new();
        for (id, rank) in &lex {
            *rrf.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + *rank as f32);
        }
        for (id, rank) in &sem {
            *rrf.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + *rank as f32);
        }
        if rrf.is_empty() {
            return Ok(Vec::new());
        }

        // Hydrate the fused ids, then apply the SAME outer filter the Lance
        // hybrid query applies post-fusion: live status, non-diary, and
        // version-chain dedup (drop rows superseded by an active row). The
        // ANN channel carries no status/type columns, so vec-only hits that
        // point at archived/rejected/diary/superseded rows are dropped here.
        let owned_ids: Vec<String> = rrf.keys().cloned().collect();
        let hydrate_sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules m \
             WHERE m.tenant = $1 AND m.capability_capsule_id = ANY($2) \
               AND m.status NOT IN ('rejected', 'archived') \
               AND m.capability_capsule_type != 'diary' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM capability_capsules s \
                   WHERE s.supersedes_capability_capsule_id = m.capability_capsule_id \
                     AND s.tenant = m.tenant AND s.status = 'active' \
               )"
        );
        let hydrated_rows = sqlx::query(&hydrate_sql)
            .bind(tenant)
            .bind(&owned_ids)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        let hydrated: Vec<CapabilityCapsuleRecord> = hydrated_rows
            .iter()
            .map(pg_row_to_record)
            .collect::<Result<_, _>>()?;

        let mut scored: Vec<(CapabilityCapsuleRecord, f32)> = hydrated
            .into_iter()
            .filter_map(|rec| rrf.get(&rec.capability_capsule_id).map(|s| (rec, *s)))
            .collect();
        // Order: rrf_score DESC, updated_at DESC, id ASC — matches the Lance
        // ORDER BY. f32 total order via partial_cmp (scores are finite).
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.0.updated_at.cmp(&a.0.updated_at))
                .then_with(|| a.0.capability_capsule_id.cmp(&b.0.capability_capsule_id))
        });
        scored.truncate(k);
        Ok(scored)
    }

    async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_text.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        // tsvector @@ plainto_tsquery('simple', q) — same 'simple' config as
        // the generated column. Live status + non-diary filter mirrors the
        // Lance bm25 CTE. 1-based rank by ts_rank DESC, id ASC (the Lance
        // `_score DESC, id ASC` tiebreak). NULL plainto_tsquery (all-stopword
        // / empty after tokenize) yields no matches — fine, returns empty.
        let rows = sqlx::query(
            "SELECT capability_capsule_id, \
                    ROW_NUMBER() OVER ( \
                        ORDER BY ts_rank(content_tsv, plainto_tsquery('simple', $2)) DESC, \
                                 capability_capsule_id ASC \
                    ) AS rank_lex \
             FROM capability_capsules \
             WHERE tenant = $1 \
               AND status NOT IN ('rejected', 'archived') \
               AND capability_capsule_type != 'diary' \
               AND content_tsv @@ plainto_tsquery('simple', $2) \
             ORDER BY rank_lex \
             LIMIT $3",
        )
        .bind(tenant)
        .bind(query_text)
        .bind(k_i)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("capability_capsule_id")
                        .map_err(sqlx_err)?,
                    r.try_get::<i64, _>("rank_lex").map_err(sqlx_err)?,
                ))
            })
            .collect()
    }

    async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // Lazy-create parity: if no upsert has created the embeddings table
        // yet, there are no candidates — return empty rather than error
        // (mirrors the Lance "embeddings dataset missing" short-circuit).
        if !embeddings_table_exists(self, "capability_capsule_embeddings").await? {
            return Ok(Vec::new());
        }
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        // pgvector cosine distance `<=>`. The embeddings table holds N chunk
        // rows per capsule; dedup to one row per capsule taking the nearest
        // (min distance) chunk — the analog of the Lance `GROUP BY
        // capability_capsule_id, MIN(_distance)` collapse. DISTINCT ON keeps
        // the closest chunk per id (ORDER BY id, distance), then the outer
        // query ranks those by distance ASC, id ASC (1-based).
        let qv = pgvector::Vector::from(query_embedding.to_vec());
        let rows = sqlx::query(
            "SELECT capability_capsule_id, \
                    ROW_NUMBER() OVER (ORDER BY best_distance ASC, capability_capsule_id ASC) \
                        AS rank_sem \
             FROM ( \
                 SELECT DISTINCT ON (capability_capsule_id) \
                        capability_capsule_id, (embedding <=> $1) AS best_distance \
                 FROM capability_capsule_embeddings \
                 WHERE tenant = $2 \
                 ORDER BY capability_capsule_id, embedding <=> $1 \
             ) d \
             ORDER BY rank_sem \
             LIMIT $3",
        )
        .bind(qv)
        .bind(tenant)
        .bind(k_i)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("capability_capsule_id")
                        .map_err(sqlx_err)?,
                    r.try_get::<i64, _>("rank_sem").map_err(sqlx_err)?,
                ))
            })
            .collect()
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
//
// pgvector-backed implementation (postgres-backend P3). Two tables —
// `capability_capsule_embeddings` (keyed `capability_capsule_id`) and
// `conversation_message_embeddings` (keyed `message_block_id`) — are
// **lazy-created on first upsert** with a `vector(<dim>)` column, the
// dim spliced in from the upsert call (the dim is provider-dependent
// and unknown at migrate time, exactly like the Lance backend). The
// migration `0002_embeddings.sql` only installs the `vector` extension.
//
// Chunked semantics mirror Lance: one DELETE of the id's rows, then one
// INSERT per chunk vector, all sharing the id (chunk_index 0..N) — search
// dedups via GROUP BY. The single-vector upsert is the chunk_index=0 case.
// `get_capability_capsule_embedding_vector` / `_row` read the chunk_index
// = 0 row, matching Lance's "first row" read.
//
// Dim drift (re-upserting at a different dim into an existing table) is
// NOT handled — `CREATE TABLE IF NOT EXISTS` won't alter the column.
// Same limitation as Lance; P3 tests use one fixed dim.

use sqlx::Row as _;

use super::postgres_capsule_store::{
    enum_to_str as enum_to_str_pub, parse_status as parse_status_pub,
    row_to_record as pg_row_to_record, sqlx_err, SELECT_COLUMNS,
};
use crate::embedding::wire::decode_f32_blob;

/// Lazy-create the `capability_capsule_embeddings` table at the given
/// vector dim. `dim` is a trusted i64 (the embedding provider's
/// dimension), never user input, so splicing it into the DDL is safe.
async fn ensure_capability_capsule_embeddings_table(
    store: &PostgresCapsuleStore,
    dim: i64,
) -> Result<(), StorageError> {
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS capability_capsule_embeddings (\
            capability_capsule_id TEXT NOT NULL, \
            tenant TEXT NOT NULL, \
            chunk_index INT NOT NULL DEFAULT 0, \
            embedding vector({dim}) NOT NULL, \
            embedding_model TEXT, \
            embedding_dim BIGINT, \
            content_hash TEXT, \
            source_updated_at TEXT, \
            created_at TEXT, \
            PRIMARY KEY (capability_capsule_id, chunk_index))"
    );
    sqlx::raw_sql(&ddl)
        .execute(store.pool())
        .await
        .map_err(sqlx_err)?;
    sqlx::raw_sql(
        "CREATE INDEX IF NOT EXISTS idx_capability_capsule_embeddings_hnsw \
         ON capability_capsule_embeddings USING hnsw (embedding vector_cosine_ops)",
    )
    .execute(store.pool())
    .await
    .map_err(sqlx_err)?;
    Ok(())
}

/// Lazy-create the `conversation_message_embeddings` table at the given
/// vector dim. Transcript analog of the capsule table.
async fn ensure_conversation_message_embeddings_table(
    store: &PostgresCapsuleStore,
    dim: i64,
) -> Result<(), StorageError> {
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS conversation_message_embeddings (\
            message_block_id TEXT NOT NULL, \
            tenant TEXT NOT NULL, \
            chunk_index INT NOT NULL DEFAULT 0, \
            embedding vector({dim}) NOT NULL, \
            embedding_model TEXT, \
            embedding_dim BIGINT, \
            content_hash TEXT, \
            source_updated_at TEXT, \
            created_at TEXT, \
            PRIMARY KEY (message_block_id, chunk_index))"
    );
    sqlx::raw_sql(&ddl)
        .execute(store.pool())
        .await
        .map_err(sqlx_err)?;
    sqlx::raw_sql(
        "CREATE INDEX IF NOT EXISTS idx_conversation_message_embeddings_hnsw \
         ON conversation_message_embeddings USING hnsw (embedding vector_cosine_ops)",
    )
    .execute(store.pool())
    .await
    .map_err(sqlx_err)?;
    Ok(())
}

/// Does table `name` exist in the current search_path? Used so the
/// `get_*` / `delete_*` methods stay no-op when no upsert has lazily
/// created the table yet (Lance returns `None` / does nothing there).
async fn embeddings_table_exists(
    store: &PostgresCapsuleStore,
    name: &str,
) -> Result<bool, StorageError> {
    let row = sqlx::query("SELECT to_regclass($1) IS NOT NULL AS present")
        .bind(name)
        .fetch_one(store.pool())
        .await
        .map_err(sqlx_err)?;
    row.try_get::<bool, _>("present").map_err(sqlx_err)
}

#[async_trait]
impl EmbeddingVectorStore for PostgresCapsuleStore {
    #[allow(clippy::too_many_arguments)]
    async fn upsert_capability_capsule_embedding(
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
        let dim = usize::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim negative"))?;
        let vector = decode_f32_blob(embedding_blob, dim).map_err(StorageError::InvalidData)?;
        // Single-vector upsert == the one-chunk case.
        self.upsert_capability_capsule_embedding_chunks(
            capability_capsule_id,
            tenant,
            embedding_model,
            embedding_dim,
            std::slice::from_ref(&vector),
            content_hash,
            source_updated_at,
            now,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_capability_capsule_embedding_chunks(
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
        // Empty vectors == no-op: leave the capsule with no embedding
        // rows (Lance contract). Don't even create the table.
        if vectors.is_empty() {
            return Ok(());
        }
        ensure_capability_capsule_embeddings_table(self, embedding_dim).await?;
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        // Delete the id's existing rows ONCE, then insert one row per
        // chunk vector (chunk_index 0..N).
        sqlx::query("DELETE FROM capability_capsule_embeddings WHERE capability_capsule_id = $1")
            .bind(capability_capsule_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        for (idx, v) in vectors.iter().enumerate() {
            let chunk_index = i32::try_from(idx)
                .map_err(|_| StorageError::InvalidData("chunk_index does not fit in i32"))?;
            sqlx::query(
                "INSERT INTO capability_capsule_embeddings (\
                    capability_capsule_id, tenant, chunk_index, embedding, embedding_model, \
                    embedding_dim, content_hash, source_updated_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(capability_capsule_id)
            .bind(tenant)
            .bind(chunk_index)
            .bind(pgvector::Vector::from(v.clone()))
            .bind(embedding_model)
            .bind(embedding_dim)
            .bind(content_hash)
            .bind(source_updated_at)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    async fn delete_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        // Idempotent; no-op if the table was never lazy-created.
        if !embeddings_table_exists(self, "capability_capsule_embeddings").await? {
            return Ok(());
        }
        sqlx::query("DELETE FROM capability_capsule_embeddings WHERE capability_capsule_id = $1")
            .bind(capability_capsule_id)
            .execute(self.pool())
            .await
            .map_err(sqlx_err)?;
        Ok(())
    }

    async fn get_capability_capsule_embedding_row(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        // Returns `(model, content_hash, created_at)` for the chunk_index
        // = 0 row. Mirrors Lance's metadata triple (Lance's `updated_at`
        // == `now` at upsert, which is `created_at` here).
        if !embeddings_table_exists(self, "capability_capsule_embeddings").await? {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT embedding_model, content_hash, created_at \
             FROM capability_capsule_embeddings \
             WHERE capability_capsule_id = $1 AND chunk_index = 0 LIMIT 1",
        )
        .bind(capability_capsule_id)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some((
                r.try_get::<String, _>("embedding_model")
                    .map_err(sqlx_err)?,
                r.try_get::<String, _>("content_hash").map_err(sqlx_err)?,
                r.try_get::<String, _>("created_at").map_err(sqlx_err)?,
            ))),
        }
    }

    async fn get_capability_capsule_embedding_vector(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        // Chunk_index = 0 row's vector (Lance reads the first row).
        if !embeddings_table_exists(self, "capability_capsule_embeddings").await? {
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT embedding FROM capability_capsule_embeddings \
             WHERE capability_capsule_id = $1 AND chunk_index = 0 LIMIT 1",
        )
        .bind(capability_capsule_id)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)?;
        match row {
            None => Ok(None),
            Some(r) => {
                let v = r
                    .try_get::<pgvector::Vector, _>("embedding")
                    .map_err(sqlx_err)?;
                Ok(Some(v.to_vec()))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_conversation_message_embedding(
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
        let dim = usize::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim negative"))?;
        let vector = decode_f32_blob(embedding_blob, dim).map_err(StorageError::InvalidData)?;
        self.upsert_conversation_message_embedding_chunks(
            message_block_id,
            tenant,
            embedding_model,
            embedding_dim,
            std::slice::from_ref(&vector),
            content_hash,
            source_updated_at,
            now,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn upsert_conversation_message_embedding_chunks(
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
        if vectors.is_empty() {
            return Ok(());
        }
        ensure_conversation_message_embeddings_table(self, embedding_dim).await?;
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        sqlx::query("DELETE FROM conversation_message_embeddings WHERE message_block_id = $1")
            .bind(message_block_id)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        for (idx, v) in vectors.iter().enumerate() {
            let chunk_index = i32::try_from(idx)
                .map_err(|_| StorageError::InvalidData("chunk_index does not fit in i32"))?;
            sqlx::query(
                "INSERT INTO conversation_message_embeddings (\
                    message_block_id, tenant, chunk_index, embedding, embedding_model, \
                    embedding_dim, content_hash, source_updated_at, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            )
            .bind(message_block_id)
            .bind(tenant)
            .bind(chunk_index)
            .bind(pgvector::Vector::from(v.clone()))
            .bind(embedding_model)
            .bind(embedding_dim)
            .bind(content_hash)
            .bind(source_updated_at)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError> {
        if !embeddings_table_exists(self, "conversation_message_embeddings").await? {
            return Ok(());
        }
        sqlx::query("DELETE FROM conversation_message_embeddings WHERE message_block_id = $1")
            .bind(message_block_id)
            .execute(self.pool())
            .await
            .map_err(sqlx_err)?;
        Ok(())
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
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);

        // Lookup first — same precondition the Lance writer uses.
        if let Some(id) = self.lookup_alias(tenant, alias).await? {
            return Ok(id);
        }

        // Auto-promote: insert entity + first alias in one transaction.
        // entity_id is UUIDv7 (matches the Lance backend's id source).
        // canonical_name preserves the caller-verbatim alias; the alias
        // row stores the normalized form (the entity_aliases PK).
        let entity_id = uuid::Uuid::now_v7().to_string();
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        sqlx::query(
            "INSERT INTO entities (entity_id, tenant, canonical_name, kind, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&entity_id)
        .bind(tenant)
        .bind(alias)
        .bind(kind.as_db_str())
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        sqlx::query(
            "INSERT INTO entity_aliases (tenant, alias_text, entity_id, created_at) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(tenant)
        .bind(&normalized)
        .bind(&entity_id)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        tx.commit().await.map_err(sqlx_err)?;
        Ok(entity_id)
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);

        // Existing-owner check: who currently owns the normalized form?
        // Mirrors the Lance backend's three-way outcome.
        match self.lookup_alias(tenant, alias).await? {
            None => {
                sqlx::query(
                    "INSERT INTO entity_aliases (tenant, alias_text, entity_id, created_at) \
                     VALUES ($1, $2, $3, $4)",
                )
                .bind(tenant)
                .bind(&normalized)
                .bind(entity_id)
                .bind(now)
                .execute(self.pool())
                .await
                .map_err(sqlx_err)?;
                Ok(AddAliasOutcome::Inserted)
            }
            Some(owner) if owner == entity_id => Ok(AddAliasOutcome::AlreadyOnSameEntity),
            Some(other) => Ok(AddAliasOutcome::ConflictWithDifferentEntity(other)),
        }
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        // Two SELECTs — entity row, then its aliases ordered
        // `created_at ASC, alias_text ASC` (matches DuckDB read).
        let row = sqlx::query(
            "SELECT entity_id, tenant, canonical_name, kind, created_at \
             FROM entities WHERE tenant = $1 AND entity_id = $2",
        )
        .bind(tenant)
        .bind(entity_id)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let entity = pg_row_to_entity(&row)?;

        let alias_rows = sqlx::query(
            "SELECT alias_text FROM entity_aliases \
             WHERE tenant = $1 AND entity_id = $2 \
             ORDER BY created_at ASC, alias_text ASC",
        )
        .bind(tenant)
        .bind(entity_id)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        let aliases = alias_rows
            .iter()
            .map(|r| r.try_get::<String, _>("alias_text").map_err(sqlx_err))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(EntityWithAliases { entity, aliases }))
    }

    async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);
        let row = sqlx::query(
            "SELECT entity_id FROM entity_aliases \
             WHERE tenant = $1 AND alias_text = $2 LIMIT 1",
        )
        .bind(tenant)
        .bind(&normalized)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(r.try_get::<String, _>("entity_id").map_err(sqlx_err)?)),
        }
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        // Static SQL with optional filters via `($N IS NULL OR ...)`,
        // mirroring the DuckDB read: tenant + optional kind + optional
        // LIKE substring on canonical_name, ordered created_at DESC,
        // limit clamped to [1, 1024]. `query` is wrapped in `%...%` so
        // the caller doesn't deal with wildcards (LIKE, case-sensitive).
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let kind = kind_filter.map(|k| k.as_db_str().to_string());
        let like = query.map(|q| format!("%{q}%"));
        let rows = sqlx::query(
            "SELECT entity_id, tenant, canonical_name, kind, created_at \
             FROM entities \
             WHERE tenant = $1 \
               AND ($2::TEXT IS NULL OR kind = $2) \
               AND ($3::TEXT IS NULL OR canonical_name LIKE $3) \
             ORDER BY created_at DESC \
             LIMIT $4",
        )
        .bind(tenant)
        .bind(kind)
        .bind(like)
        .bind(lim)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_entity).collect()
    }
}

/// Project an `entities` row into an [`Entity`]. `kind` round-trips
/// through `EntityKind::from_db_str` (lowercase db form).
fn pg_row_to_entity(row: &sqlx::postgres::PgRow) -> Result<Entity, StorageError> {
    let kind_str: String = row.try_get("kind").map_err(sqlx_err)?;
    Ok(Entity {
        entity_id: row.try_get("entity_id").map_err(sqlx_err)?,
        tenant: row.try_get("tenant").map_err(sqlx_err)?,
        canonical_name: row.try_get("canonical_name").map_err(sqlx_err)?,
        kind: EntityKind::from_db_str(&kind_str)
            .ok_or(StorageError::InvalidData("unknown entity kind"))?,
        created_at: row.try_get("created_at").map_err(sqlx_err)?,
    })
}

// ──────────────────────────────── SessionStore ────────────────────────────

#[async_trait]
impl SessionStore for PostgresCapsuleStore {
    async fn touch_session(
        &self,
        session_id: &str,
        last_active_at: &str,
    ) -> Result<(), StorageError> {
        // Bump memory_count + stamp last_seen_at on the active row
        // (Lance only_if `session_id =`). Silent no-op if the row is
        // missing — `rows_affected == 0` is not an error.
        sqlx::query(
            "UPDATE sessions SET last_seen_at = $1, memory_count = memory_count + 1 \
             WHERE session_id = $2",
        )
        .bind(last_active_at)
        .bind(session_id)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        let session = Session {
            session_id: session_id.to_string(),
            tenant: tenant.to_string(),
            caller_agent: caller_agent.to_string(),
            started_at: now.to_string(),
            last_seen_at: now.to_string(),
            ended_at: None,
            goal: None,
            memory_count: 0,
        };
        sqlx::query(
            "INSERT INTO sessions (session_id, tenant, caller_agent, started_at, \
                last_seen_at, ended_at, goal, memory_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&session.session_id)
        .bind(&session.tenant)
        .bind(&session.caller_agent)
        .bind(&session.started_at)
        .bind(&session.last_seen_at)
        .bind(&session.ended_at)
        .bind(&session.goal)
        .bind(i64::from(session.memory_count))
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(session)
    }

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError> {
        sqlx::query("UPDATE sessions SET ended_at = $1 WHERE session_id = $2")
            .bind(ended_at)
            .bind(session_id)
            .execute(self.pool())
            .await
            .map_err(sqlx_err)?;
        Ok(())
    }

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        // Most-recent non-closed session for the identity. The Lance
        // backend sorts collected rows by `last_seen_at DESC`; the
        // trait doc says "most-recent". Postgres pushes the ORDER down.
        let row = sqlx::query(
            "SELECT session_id, tenant, caller_agent, started_at, last_seen_at, \
                    ended_at, goal, memory_count \
             FROM sessions \
             WHERE tenant = $1 AND caller_agent = $2 AND ended_at IS NULL \
             ORDER BY last_seen_at DESC \
             LIMIT 1",
        )
        .bind(tenant)
        .bind(caller_agent)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)?;
        row.as_ref().map(pg_row_to_session).transpose()
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        // `workflow_candidate` is JSON-encoded into the nullable text
        // column (same as the Lance backend). scope / visibility serialize
        // to their snake_case wire form.
        let workflow_json: Option<String> = match &episode.workflow_candidate {
            Some(c) => Some(
                serde_json::to_string(c)
                    .map_err(|e| StorageError::InvalidInput(format!("workflow_candidate: {e}")))?,
            ),
            None => None,
        };
        sqlx::query(
            "INSERT INTO episodes (episode_id, tenant, goal, steps, outcome, evidence, \
                scope, visibility, project, repo, module, tags, source_agent, \
                idempotency_key, created_at, updated_at, workflow_candidate) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&episode.episode_id)
        .bind(&episode.tenant)
        .bind(&episode.goal)
        .bind(&episode.steps)
        .bind(&episode.outcome)
        .bind(&episode.evidence)
        .bind(enum_to_str_pub(&episode.scope)?)
        .bind(enum_to_str_pub(&episode.visibility)?)
        .bind(&episode.project)
        .bind(&episode.repo)
        .bind(&episode.module)
        .bind(&episode.tags)
        .bind(&episode.source_agent)
        .bind(&episode.idempotency_key)
        .bind(&episode.created_at)
        .bind(&episode.updated_at)
        .bind(&workflow_json)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(episode)
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        // Trait contract: episodes whose `outcome = 'success'`. Ordered
        // created_at DESC (the Lance backend's in-memory sort).
        let rows = sqlx::query(
            "SELECT episode_id, tenant, goal, steps, outcome, evidence, scope, \
                    visibility, project, repo, module, tags, source_agent, \
                    idempotency_key, created_at, updated_at, workflow_candidate \
             FROM episodes \
             WHERE tenant = $1 AND outcome = 'success' \
             ORDER BY created_at DESC",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_episode).collect()
    }
}

/// Project a `sessions` row into a [`Session`]. `memory_count` is
/// BIGINT on disk (no unsigned PG type) → clamp back into `u32`.
fn pg_row_to_session(row: &sqlx::postgres::PgRow) -> Result<Session, StorageError> {
    let memory_count: i64 = row.try_get("memory_count").map_err(sqlx_err)?;
    Ok(Session {
        session_id: row.try_get("session_id").map_err(sqlx_err)?,
        tenant: row.try_get("tenant").map_err(sqlx_err)?,
        caller_agent: row.try_get("caller_agent").map_err(sqlx_err)?,
        started_at: row.try_get("started_at").map_err(sqlx_err)?,
        last_seen_at: row.try_get("last_seen_at").map_err(sqlx_err)?,
        ended_at: row.try_get("ended_at").map_err(sqlx_err)?,
        goal: row.try_get("goal").map_err(sqlx_err)?,
        memory_count: u32::try_from(memory_count).unwrap_or(u32::MAX),
    })
}

/// Project an `episodes` row into an [`EpisodeRecord`]. scope /
/// visibility parse from their snake_case text; workflow_candidate
/// JSON-decodes when present.
fn pg_row_to_episode(row: &sqlx::postgres::PgRow) -> Result<EpisodeRecord, StorageError> {
    use crate::domain::capability_capsule::{Scope, Visibility};
    use crate::domain::workflow::WorkflowCandidate;

    let scope_s: String = row.try_get("scope").map_err(sqlx_err)?;
    let visibility_s: String = row.try_get("visibility").map_err(sqlx_err)?;
    let scope: Scope = serde_json::from_value(serde_json::Value::String(scope_s))
        .map_err(|_| StorageError::InvalidData("unknown episode scope"))?;
    let visibility: Visibility = serde_json::from_value(serde_json::Value::String(visibility_s))
        .map_err(|_| StorageError::InvalidData("unknown episode visibility"))?;
    let workflow_raw: Option<String> = row.try_get("workflow_candidate").map_err(sqlx_err)?;
    let workflow_candidate = match workflow_raw {
        None => None,
        Some(raw) => Some(
            serde_json::from_str::<WorkflowCandidate>(&raw)
                .map_err(|e| StorageError::InvalidInput(format!("workflow_candidate: {e}")))?,
        ),
    };
    Ok(EpisodeRecord {
        episode_id: row.try_get("episode_id").map_err(sqlx_err)?,
        tenant: row.try_get("tenant").map_err(sqlx_err)?,
        goal: row.try_get("goal").map_err(sqlx_err)?,
        steps: row.try_get("steps").map_err(sqlx_err)?,
        outcome: row.try_get("outcome").map_err(sqlx_err)?,
        evidence: row.try_get("evidence").map_err(sqlx_err)?,
        scope,
        visibility,
        project: row.try_get("project").map_err(sqlx_err)?,
        repo: row.try_get("repo").map_err(sqlx_err)?,
        module: row.try_get("module").map_err(sqlx_err)?,
        tags: row.try_get("tags").map_err(sqlx_err)?,
        source_agent: row.try_get("source_agent").map_err(sqlx_err)?,
        idempotency_key: row.try_get("idempotency_key").map_err(sqlx_err)?,
        created_at: row.try_get("created_at").map_err(sqlx_err)?,
        updated_at: row.try_get("updated_at").map_err(sqlx_err)?,
        workflow_candidate,
    })
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
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        // Mirror the DuckDB decay sweep (duckdb_query/decay.rs) in PG
        // dialect, in one transaction:
        //   1. hard-expiry archive (expires_at deadline passed),
        //   2. decay the active set, anchoring the clock on
        //      COALESCE(last_used_at, updated_at) and advancing
        //      last_used_at to now. Postgres supports COALESCE inside the
        //      SET expression (the lance extension didn't, which is why
        //      the DuckDB impl split into two NULL-disjoint passes); a
        //      single statement is equivalent. Timestamps are 20-digit
        //      zero-padded ms strings → cast to double precision for the
        //      arithmetic. decay_score stays an additive accumulator,
        //      capped at 1.0 via LEAST.
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        sqlx::query(
            "UPDATE capability_capsules SET status = 'archived' \
             WHERE status = 'active' AND expires_at IS NOT NULL AND expires_at <= $1",
        )
        .bind(now_ms_str)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        sqlx::query(
            "UPDATE capability_capsules \
             SET decay_score = LEAST(1.0, decay_score + $1 * \
                     (($2 - COALESCE(last_used_at, updated_at)::double precision) / $3)), \
                 last_used_at = $4 \
             WHERE status = 'active' AND decay_score < 1.0",
        )
        .bind(decay_rate_per_day)
        .bind(now_ms)
        .bind(ms_per_day)
        .bind(now_ms_str)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    async fn vacuum_old_versions_with(
        &self,
        _older_than_days: i64,
        _aggressive: bool,
    ) -> Result<VacuumStats, StorageError> {
        // Lance manifest pruning has no Postgres analog (autovacuum
        // handles dead tuples). No-op zero-stats, per the trait doc for
        // non-Lance backends.
        Ok(VacuumStats::default())
    }

    async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Empty `types` short-circuits (trait contract). Otherwise: the
        // pending-confirmation rows past the cutoff with low decay and a
        // type in the allow-list, ordered created_at ASC (matches the
        // DuckDB read). `decay_score < $4` uses f64 like the DuckDB side.
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let type_strs: Vec<String> = types
            .iter()
            .map(enum_to_str_pub)
            .collect::<Result<Vec<_>, _>>()?;
        let max_decay = max_decay_score as f64;
        let sql = format!(
            "SELECT {SELECT_COLUMNS} FROM capability_capsules \
             WHERE tenant = $1 AND status = 'pending_confirmation' \
               AND updated_at < $2 \
               AND decay_score < $3 \
               AND capability_capsule_type = ANY($4) \
             ORDER BY created_at ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .bind(cutoff_updated_at)
            .bind(max_decay)
            .bind(&type_strs)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_record).collect()
    }
}

// ────────────────────────────── MineCursorStore ───────────────────────────

#[async_trait]
impl MineCursorStore for PostgresCapsuleStore {
    async fn get_mine_cursor(
        &self,
        transcript_path: &str,
    ) -> Result<Option<MineCursor>, StorageError> {
        let row = sqlx::query(
            "SELECT transcript_path, last_line_number, updated_at \
             FROM mine_cursors WHERE transcript_path = $1 LIMIT 1",
        )
        .bind(transcript_path)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)?;
        match row {
            None => Ok(None),
            Some(r) => Ok(Some(MineCursor {
                transcript_path: r.try_get("transcript_path").map_err(sqlx_err)?,
                last_line_number: r.try_get("last_line_number").map_err(sqlx_err)?,
                updated_at: r.try_get("updated_at").map_err(sqlx_err)?,
            })),
        }
    }

    async fn upsert_mine_cursor(
        &self,
        transcript_path: &str,
        last_line_number: i64,
        updated_at: &str,
    ) -> Result<(), StorageError> {
        // INSERT ON CONFLICT(transcript_path) DO UPDATE — the real-PK
        // analog of the Lance delete-then-add upsert. Monotonicity of
        // last_line_number is a caller invariant, not enforced here
        // (same as Lance).
        sqlx::query(
            "INSERT INTO mine_cursors (transcript_path, last_line_number, updated_at) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (transcript_path) DO UPDATE \
             SET last_line_number = EXCLUDED.last_line_number, \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(transcript_path)
        .bind(last_line_number)
        .bind(updated_at)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }
}

// ─────────────────────────── EvolutionCandidateStore ──────────────────────

#[async_trait]
impl EvolutionCandidateStore for PostgresCapsuleStore {
    async fn upsert_evolution_candidate(
        &self,
        candidate: EvolutionCandidate,
    ) -> Result<(), StorageError> {
        // Full-row upsert keyed on candidate_id. `member_ids` /
        // `result_capsule_ids` (Vec<String>) JSON-encode into text
        // columns — same on-disk shape as the Lance backend (which also
        // stored them as JSON strings, not Arrow lists). `params` is
        // already a JSON string; stored verbatim.
        let member_ids = encode_id_list(&candidate.member_ids);
        let result_ids = encode_id_list(&candidate.result_capsule_ids);
        sqlx::query(
            "INSERT INTO evolution_candidates (candidate_id, tenant, op_kind, member_ids, \
                params, evidence, consecutive_cycles, status, first_proposed_at, \
                last_signal_at, executed_at, result_capsule_ids) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT (candidate_id) DO UPDATE SET \
                tenant = EXCLUDED.tenant, \
                op_kind = EXCLUDED.op_kind, \
                member_ids = EXCLUDED.member_ids, \
                params = EXCLUDED.params, \
                evidence = EXCLUDED.evidence, \
                consecutive_cycles = EXCLUDED.consecutive_cycles, \
                status = EXCLUDED.status, \
                first_proposed_at = EXCLUDED.first_proposed_at, \
                last_signal_at = EXCLUDED.last_signal_at, \
                executed_at = EXCLUDED.executed_at, \
                result_capsule_ids = EXCLUDED.result_capsule_ids",
        )
        .bind(&candidate.candidate_id)
        .bind(&candidate.tenant)
        .bind(&candidate.op_kind)
        .bind(&member_ids)
        .bind(&candidate.params)
        .bind(candidate.evidence)
        .bind(candidate.consecutive_cycles)
        .bind(&candidate.status)
        .bind(&candidate.first_proposed_at)
        .bind(&candidate.last_signal_at)
        .bind(&candidate.executed_at)
        .bind(&result_ids)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn list_evolution_candidates(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError> {
        // tenant + optional status, no pagination (sweep-time read of a
        // small table). Optional status via `($2 IS NULL OR status = $2)`.
        let rows = sqlx::query(
            "SELECT candidate_id, tenant, op_kind, member_ids, params, evidence, \
                    consecutive_cycles, status, first_proposed_at, last_signal_at, \
                    executed_at, result_capsule_ids \
             FROM evolution_candidates \
             WHERE tenant = $1 AND ($2::TEXT IS NULL OR status = $2)",
        )
        .bind(tenant)
        .bind(status)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_evolution_candidate).collect()
    }
}

/// Encode a `Vec<String>` id list as a JSON text array (the Lance
/// backend's `encode_ids`). Lossless round-trip with
/// [`decode_id_list`].
fn encode_id_list(ids: &[String]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_string())
}

/// Decode a JSON text array back into a `Vec<String>` (Lance's
/// `decode_ids`). Malformed / NULL-equivalent text → empty vec.
fn decode_id_list(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Project an `evolution_candidates` row into an [`EvolutionCandidate`].
fn pg_row_to_evolution_candidate(
    row: &sqlx::postgres::PgRow,
) -> Result<EvolutionCandidate, StorageError> {
    let member_ids: String = row.try_get("member_ids").map_err(sqlx_err)?;
    let result_ids: String = row.try_get("result_capsule_ids").map_err(sqlx_err)?;
    Ok(EvolutionCandidate {
        candidate_id: row.try_get("candidate_id").map_err(sqlx_err)?,
        tenant: row.try_get("tenant").map_err(sqlx_err)?,
        op_kind: row.try_get("op_kind").map_err(sqlx_err)?,
        member_ids: decode_id_list(&member_ids),
        params: row.try_get("params").map_err(sqlx_err)?,
        evidence: row.try_get("evidence").map_err(sqlx_err)?,
        consecutive_cycles: row.try_get("consecutive_cycles").map_err(sqlx_err)?,
        status: row.try_get("status").map_err(sqlx_err)?,
        first_proposed_at: row.try_get("first_proposed_at").map_err(sqlx_err)?,
        last_signal_at: row.try_get("last_signal_at").map_err(sqlx_err)?,
        executed_at: row.try_get("executed_at").map_err(sqlx_err)?,
        result_capsule_ids: decode_id_list(&result_ids),
    })
}
