//! Phase 5 — `Backend` umbrella sub-trait impls for
//! [`PostgresCapsuleStore`].
//!
//! [`super::super::Backend`] requires 11 storage sub-traits. The Phase 4
//! spike (`postgres_capsule_store.rs`) implements [`super::super::CapsuleStore`];
//! this module supplies the other 10 (CapsuleSearchStore,
//! EmbeddingVectorStore, EmbeddingJobStore, GraphStore, TranscriptStore,
//! EntityRegistry, SessionStore, MaintenanceStore, MineCursorStore,
//! EvolutionCandidateStore) so the concrete type satisfies `Backend` and
//! the blanket impl in `backend.rs` applies. Every method here is a real
//! Postgres implementation behaviour-aligned with the Lance
//! backend; the `MaintenanceStore::vacuum_old_versions_with` /
//! `ensure_query_indexes` no-ops are deliberate (no Lance-manifest analog
//! on Postgres).
//!
//! Compiled into every build (a default dependency — no cargo feature
//! gates it). The backend is selected at runtime via `MEM_BACKEND=postgres`
//! + `MEM_POSTGRES_URL`.

use async_trait::async_trait;

use super::super::{
    CapsuleSearchStore, EmbeddingJobStore, EmbeddingVectorStore, EntityRegistry,
    EvolutionCandidate, EvolutionCandidateStore, GraphStore, MaintenanceStore, MineCursor,
    MineCursorStore, SessionStore, TranscriptStore,
};
use super::capsule_store::PostgresCapsuleStore;
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
// RRF fusion), behaviour-aligned with the Lance backend in
// `lance_store/capability_capsules.rs` and `pipeline/retrieve.rs`.
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

use super::super::CapsuleStore;

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
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        // Idempotency probe: decline if any live (pending/processing)
        // row already covers the (tenant, capsule, hash, provider) tuple.
        // Mirrors the Lance count→insert window; PG gives us a real
        // transaction so the probe + insert are atomic.
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM embedding_jobs \
             WHERE tenant = $1 AND capability_capsule_id = $2 \
               AND target_content_hash = $3 AND provider = $4 \
               AND status IN ('pending', 'processing')",
        )
        .bind(&insert.tenant)
        .bind(&insert.capability_capsule_id)
        .bind(&insert.target_content_hash)
        .bind(&insert.provider)
        .fetch_one(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        if live > 0 {
            tx.rollback().await.map_err(sqlx_err)?;
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO embedding_jobs (job_id, tenant, capability_capsule_id, \
                target_content_hash, provider, status, attempt_count, last_error, \
                available_at, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, 'pending', 0, NULL, $6, $7, $8)",
        )
        .bind(&insert.job_id)
        .bind(&insert.tenant)
        .bind(&insert.capability_capsule_id)
        .bind(&insert.target_content_hash)
        .bind(&insert.provider)
        .bind(&insert.available_at)
        .bind(&insert.created_at)
        .bind(&insert.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;
        tx.commit().await.map_err(sqlx_err)?;
        Ok(true)
    }

    async fn enqueue_embedding_jobs(
        &self,
        inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        if inserts.is_empty() {
            return Ok(());
        }
        // Batch insert (caller guarantees no live duplicate, per the
        // Lance contract — no per-row probe). One transaction.
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        for insert in inserts {
            sqlx::query(
                "INSERT INTO embedding_jobs (job_id, tenant, capability_capsule_id, \
                    target_content_hash, provider, status, attempt_count, last_error, \
                    available_at, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, 'pending', 0, NULL, $6, $7, $8)",
            )
            .bind(&insert.job_id)
            .bind(&insert.tenant)
            .bind(&insert.capability_capsule_id)
            .bind(&insert.target_content_hash)
            .bind(&insert.provider)
            .bind(&insert.available_at)
            .bind(&insert.created_at)
            .bind(&insert.updated_at)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        // SELECT ... FOR UPDATE SKIP LOCKED then flip to 'processing' in
        // one transaction. Eligible = pending OR (failed with retry
        // budget AND available_at <= now). Ordered created_at ASC.
        let max_r = i64::from(max_retries);
        let lim = i64::try_from(n).unwrap_or(i64::MAX);
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        let rows = sqlx::query(
            "SELECT job_id, tenant, capability_capsule_id, target_content_hash, \
                    provider, attempt_count \
             FROM embedding_jobs \
             WHERE status = 'pending' \
                OR (status = 'failed' AND attempt_count < $1 AND available_at <= $2) \
             ORDER BY created_at ASC \
             LIMIT $3 \
             FOR UPDATE SKIP LOCKED",
        )
        .bind(max_r)
        .bind(now)
        .bind(lim)
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for r in &rows {
            let job_id: String = r.try_get("job_id").map_err(sqlx_err)?;
            sqlx::query(
                "UPDATE embedding_jobs SET status = 'processing', updated_at = $2 \
                 WHERE job_id = $1",
            )
            .bind(&job_id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
            claimed.push(ClaimedEmbeddingJob {
                job_id,
                tenant: r.try_get("tenant").map_err(sqlx_err)?,
                capability_capsule_id: r.try_get("capability_capsule_id").map_err(sqlx_err)?,
                target_content_hash: r.try_get("target_content_hash").map_err(sqlx_err)?,
                provider: r.try_get("provider").map_err(sqlx_err)?,
                attempt_count: r.try_get("attempt_count").map_err(sqlx_err)?,
            });
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(claimed)
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        // Only complete a row still 'processing' (mirror Lance / DuckDB).
        sqlx::query(
            "UPDATE embedding_jobs \
             SET status = 'completed', last_error = NULL, updated_at = $2 \
             WHERE job_id = $1 AND status = 'processing'",
        )
        .bind(job_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE embedding_jobs SET status = 'stale', updated_at = $2 WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE embedding_jobs \
             SET status = 'failed', attempt_count = $2, last_error = $3, \
                 available_at = $4, updated_at = $5 \
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(new_attempt_count)
        .bind(last_error)
        .bind(available_at)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE embedding_jobs \
             SET status = 'failed', attempt_count = $2, last_error = $3, updated_at = $4 \
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(new_attempt_count)
        .bind(last_error)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        let res = sqlx::query("DELETE FROM embedding_jobs WHERE capability_capsule_id = $1")
            .bind(capability_capsule_id)
            .execute(self.pool())
            .await
            .map_err(sqlx_err)?;
        Ok(res.rows_affected() as usize)
    }

    async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        // Mark every live (pending|processing) job for the triple stale.
        let res = sqlx::query(
            "UPDATE embedding_jobs SET status = 'stale', updated_at = $4 \
             WHERE tenant = $1 AND capability_capsule_id = $2 AND provider = $3 \
               AND status IN ('pending', 'processing')",
        )
        .bind(tenant)
        .bind(capability_capsule_id)
        .bind(provider)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(res.rows_affected() as usize)
    }

    async fn get_embedding_job_status(&self, job_id: &str) -> Result<Option<String>, StorageError> {
        sqlx::query_scalar::<_, String>("SELECT status FROM embedding_jobs WHERE job_id = $1")
            .bind(job_id)
            .fetch_optional(self.pool())
            .await
            .map_err(sqlx_err)
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        // Most-recent row's status — Lance sorts by updated_at DESC.
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM embedding_jobs \
             WHERE tenant = $1 AND capability_capsule_id = $2 AND target_content_hash = $3 \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(tenant)
        .bind(capability_capsule_id)
        .bind(target_content_hash)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)
    }

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        // tenant + optional status + optional capsule id, ordered
        // updated_at DESC, bounded (Lance clamps at 10_000).
        let lim = i64::try_from(limit.min(10_000)).unwrap_or(10_000);
        let rows = sqlx::query(
            "SELECT job_id, tenant, capability_capsule_id, target_content_hash, provider, \
                    status, attempt_count, last_error, available_at, created_at, updated_at \
             FROM embedding_jobs \
             WHERE tenant = $1 \
               AND ($2::TEXT IS NULL OR status = $2) \
               AND ($3::TEXT IS NULL OR capability_capsule_id = $3) \
             ORDER BY updated_at DESC \
             LIMIT $4",
        )
        .bind(tenant)
        .bind(status_filter)
        .bind(memory_id_filter)
        .bind(lim)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                let attempt_count: i64 = r.try_get("attempt_count").map_err(sqlx_err)?;
                Ok(EmbeddingJobInfo {
                    job_id: r.try_get("job_id").map_err(sqlx_err)?,
                    tenant: r.try_get("tenant").map_err(sqlx_err)?,
                    capability_capsule_id: r.try_get("capability_capsule_id").map_err(sqlx_err)?,
                    target_content_hash: r.try_get("target_content_hash").map_err(sqlx_err)?,
                    provider: r.try_get("provider").map_err(sqlx_err)?,
                    status: r.try_get("status").map_err(sqlx_err)?,
                    attempt_count: u32::try_from(attempt_count).unwrap_or(u32::MAX),
                    last_error: r.try_get("last_error").map_err(sqlx_err)?,
                    available_at: r.try_get("available_at").map_err(sqlx_err)?,
                    created_at: r.try_get("created_at").map_err(sqlx_err)?,
                    updated_at: r.try_get("updated_at").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

    async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        let max_r = i64::from(max_retries);
        let lim = i64::try_from(n).unwrap_or(i64::MAX);
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        let rows = sqlx::query(
            "SELECT job_id, tenant, message_block_id, provider, attempt_count \
             FROM transcript_embedding_jobs \
             WHERE status = 'pending' \
                OR (status = 'failed' AND attempt_count < $1 AND available_at <= $2) \
             ORDER BY created_at ASC \
             LIMIT $3 \
             FOR UPDATE SKIP LOCKED",
        )
        .bind(max_r)
        .bind(now)
        .bind(lim)
        .fetch_all(&mut *tx)
        .await
        .map_err(sqlx_err)?;

        let mut claimed = Vec::with_capacity(rows.len());
        for r in &rows {
            let job_id: String = r.try_get("job_id").map_err(sqlx_err)?;
            sqlx::query(
                "UPDATE transcript_embedding_jobs SET status = 'processing', updated_at = $2 \
                 WHERE job_id = $1",
            )
            .bind(&job_id)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
            claimed.push(ClaimedTranscriptEmbeddingJob {
                job_id,
                tenant: r.try_get("tenant").map_err(sqlx_err)?,
                message_block_id: r.try_get("message_block_id").map_err(sqlx_err)?,
                provider: r.try_get("provider").map_err(sqlx_err)?,
                attempt_count: r.try_get("attempt_count").map_err(sqlx_err)?,
            });
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(claimed)
    }

    async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE transcript_embedding_jobs \
             SET status = 'completed', last_error = NULL, updated_at = $2 \
             WHERE job_id = $1 AND status = 'processing'",
        )
        .bind(job_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE transcript_embedding_jobs SET status = 'stale', updated_at = $2 \
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE transcript_embedding_jobs \
             SET status = 'failed', attempt_count = $2, last_error = $3, \
                 available_at = $4, updated_at = $5 \
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(new_attempt_count)
        .bind(last_error)
        .bind(available_at)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        sqlx::query(
            "UPDATE transcript_embedding_jobs \
             SET status = 'failed', attempt_count = $2, last_error = $3, updated_at = $4 \
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(new_attempt_count)
        .bind(last_error)
        .bind(now)
        .execute(self.pool())
        .await
        .map_err(sqlx_err)?;
        Ok(())
    }

    async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        sqlx::query_scalar::<_, String>(
            "SELECT status FROM transcript_embedding_jobs WHERE job_id = $1",
        )
        .bind(job_id)
        .fetch_optional(self.pool())
        .await
        .map_err(sqlx_err)
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

use super::capsule_store::{
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

/// Map a sqlx error into a `GraphError::Backend` (the graph trait's
/// error type — distinct from `StorageError`, see `types.rs`).
fn graph_err(e: sqlx::Error) -> GraphError {
    GraphError::Backend(format!("postgres: {e}"))
}

/// `graph_edges` SELECT column order shared by every graph read. Keep
/// in sync with [`pg_row_to_graph_edge`].
const GRAPH_EDGE_COLS: &str = "from_node_id, to_node_id, relation, valid_from, valid_to, \
    confidence, extractor, strength, stability, last_activated, access_count";

/// Upper cap on `neighbors_within` / `follow_tunnels` hops (mirrors the
/// DuckDB `MAX_HOPS_CAP`).
const PG_MAX_HOPS_CAP: u32 = 3;

/// Project a `graph_edges` row into a [`GraphEdge`].
fn pg_row_to_graph_edge(row: &sqlx::postgres::PgRow) -> Result<GraphEdge, GraphError> {
    Ok(GraphEdge {
        from_node_id: row.try_get("from_node_id").map_err(graph_err)?,
        to_node_id: row.try_get("to_node_id").map_err(graph_err)?,
        relation: row.try_get("relation").map_err(graph_err)?,
        valid_from: row.try_get("valid_from").map_err(graph_err)?,
        valid_to: row.try_get("valid_to").map_err(graph_err)?,
        confidence: row.try_get("confidence").map_err(graph_err)?,
        extractor: row.try_get("extractor").map_err(graph_err)?,
        strength: row.try_get("strength").map_err(graph_err)?,
        stability: row.try_get("stability").map_err(graph_err)?,
        last_activated: row.try_get("last_activated").map_err(graph_err)?,
        access_count: row.try_get("access_count").map_err(graph_err)?,
    })
}

#[async_trait]
impl GraphStore for PostgresCapsuleStore {
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        // 1-hop active edges incident on node_id, ordered
        // (relation, from, to) — matches the DuckDB `neighbors`.
        let sql = format!(
            "SELECT {GRAPH_EDGE_COLS} FROM graph_edges \
             WHERE (from_node_id = $1 OR to_node_id = $1) AND valid_to IS NULL \
             ORDER BY relation, from_node_id, to_node_id"
        );
        let rows = sqlx::query(&sql)
            .bind(node_id)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        rows.iter().map(pg_row_to_graph_edge).collect()
    }

    async fn neighbors_within(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        // Recursive CTE BFS. `valid_to IS NULL` (active now) when as_of
        // is None; bitemporal window when supplied. The CTE walks node
        // ids up to `hops` levels; the final SELECT projects the
        // DISTINCT incident edge set, ordered (relation, from, to,
        // valid_from) for determinism (matches the DuckDB read).
        let hops = i32::try_from(max_hops.clamp(1, PG_MAX_HOPS_CAP)).unwrap_or(1);
        let validity = if as_of.is_some() {
            "valid_from <= $3 AND (valid_to IS NULL OR valid_to > $3)"
        } else {
            "valid_to IS NULL"
        };
        // `walk` accumulates the reachable node set with its depth;
        // `edges` is the DISTINCT incident-edge projection over those
        // nodes. Depth bound `< $2` (root is depth 0; a `hops`-level
        // walk visits depths 0..hops, expanding while depth < hops).
        let sql = format!(
            "WITH RECURSIVE walk(node, depth) AS ( \
                 SELECT $1::TEXT, 0 \
               UNION \
                 SELECT CASE WHEN e.from_node_id = w.node THEN e.to_node_id \
                             ELSE e.from_node_id END, \
                        w.depth + 1 \
                 FROM walk w \
                 JOIN graph_edges e \
                   ON (e.from_node_id = w.node OR e.to_node_id = w.node) \
                  AND {validity} \
                 WHERE w.depth < $2 \
             ) \
             SELECT DISTINCT {GRAPH_EDGE_COLS} \
             FROM graph_edges e \
             WHERE (e.from_node_id IN (SELECT node FROM walk) \
                 OR e.to_node_id IN (SELECT node FROM walk)) \
               AND {validity} \
             ORDER BY relation, from_node_id, to_node_id, valid_from"
        );
        let mut q = sqlx::query(&sql).bind(node_id).bind(hops);
        if let Some(ts) = as_of {
            q = q.bind(ts);
        }
        let rows = q.fetch_all(self.pool()).await.map_err(graph_err)?;
        // The final projection keys edges by their incidence on ANY
        // visited node — but a `hops`-deep walk's frontier nodes pull in
        // edges that are one hop beyond the bound. Trim to the edges
        // whose endpoints are both within the visited set so the result
        // matches the DuckDB BFS edge set exactly.
        let visited = self
            .neighbors_within_visited(node_id, max_hops, as_of, validity)
            .await?;
        let out: Vec<GraphEdge> = rows
            .iter()
            .map(pg_row_to_graph_edge)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|e| {
                visited.contains(e.from_node_id.as_str()) && visited.contains(e.to_node_id.as_str())
            })
            .collect();
        Ok(out)
    }

    async fn kg_timeline(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        // All edges (active + closed) incident on node_id, chronological.
        let sql = format!(
            "SELECT {GRAPH_EDGE_COLS} FROM graph_edges \
             WHERE from_node_id = $1 OR to_node_id = $1 \
             ORDER BY valid_from ASC, relation ASC, from_node_id ASC, to_node_id ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(node_id)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        rows.iter().map(pg_row_to_graph_edge).collect()
    }

    async fn query_predicate(
        &self,
        predicate: &str,
        as_of: Option<&str>,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        // relation = predicate, time-scoped when as_of supplied (else
        // active + closed). Ordered (valid_from, from, to).
        let sql = format!(
            "SELECT {GRAPH_EDGE_COLS} FROM graph_edges \
             WHERE relation = $1 \
               AND ($2::TEXT IS NULL \
                    OR (valid_from <= $2 AND (valid_to IS NULL OR valid_to > $2))) \
             ORDER BY valid_from ASC, from_node_id ASC, to_node_id ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(predicate)
            .bind(as_of)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        rows.iter().map(pg_row_to_graph_edge).collect()
    }

    async fn list_user_tunnels(&self, limit: usize) -> Result<Vec<GraphEdge>, GraphError> {
        let lim = i64::try_from(limit.clamp(1, 200)).unwrap_or(50);
        let sql = format!(
            "SELECT {GRAPH_EDGE_COLS} FROM graph_edges \
             WHERE relation LIKE 'user_tunnel:%' AND valid_to IS NULL \
             ORDER BY relation, from_node_id, to_node_id \
             LIMIT $1"
        );
        let rows = sqlx::query(&sql)
            .bind(lim)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        rows.iter().map(pg_row_to_graph_edge).collect()
    }

    async fn find_tunnels(
        &self,
        prefix_a: &str,
        prefix_b: &str,
        limit: usize,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        // Active user-tunnel edges bridging the two prefixes, both
        // directions. LIKE escaping is not applied (prefixes are
        // caller-trusted node-id stems, same as the DuckDB read).
        let like_a = format!("{prefix_a}%");
        let like_b = format!("{prefix_b}%");
        let lim = i64::try_from(limit.clamp(1, 200)).unwrap_or(50);
        let sql = format!(
            "SELECT {GRAPH_EDGE_COLS} FROM graph_edges \
             WHERE relation LIKE 'user_tunnel:%' AND valid_to IS NULL \
               AND ((from_node_id LIKE $1 AND to_node_id LIKE $2) \
                 OR (from_node_id LIKE $2 AND to_node_id LIKE $1)) \
             ORDER BY relation, from_node_id, to_node_id \
             LIMIT $3"
        );
        let rows = sqlx::query(&sql)
            .bind(&like_a)
            .bind(&like_b)
            .bind(lim)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        rows.iter().map(pg_row_to_graph_edge).collect()
    }

    async fn follow_tunnels(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<Vec<GraphEdge>, GraphError> {
        // BFS over active user-tunnel edges only, up to max_hops.
        let hops = i32::try_from(max_hops.clamp(1, PG_MAX_HOPS_CAP)).unwrap_or(1);
        let sql = format!(
            "WITH RECURSIVE walk(node, depth) AS ( \
                 SELECT $1::TEXT, 0 \
               UNION \
                 SELECT CASE WHEN e.from_node_id = w.node THEN e.to_node_id \
                             ELSE e.from_node_id END, \
                        w.depth + 1 \
                 FROM walk w \
                 JOIN graph_edges e \
                   ON (e.from_node_id = w.node OR e.to_node_id = w.node) \
                  AND e.relation LIKE 'user_tunnel:%' AND e.valid_to IS NULL \
                 WHERE w.depth < $2 \
             ) \
             SELECT DISTINCT {GRAPH_EDGE_COLS} \
             FROM graph_edges e \
             WHERE (e.from_node_id IN (SELECT node FROM walk) \
                 OR e.to_node_id IN (SELECT node FROM walk)) \
               AND e.relation LIKE 'user_tunnel:%' AND e.valid_to IS NULL \
             ORDER BY relation, from_node_id, to_node_id"
        );
        let rows = sqlx::query(&sql)
            .bind(node_id)
            .bind(hops)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        let visited = self.follow_tunnels_visited(node_id, max_hops).await?;
        let out: Vec<GraphEdge> = rows
            .iter()
            .map(pg_row_to_graph_edge)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|e| {
                visited.contains(e.from_node_id.as_str()) && visited.contains(e.to_node_id.as_str())
            })
            .collect();
        Ok(out)
    }

    async fn graph_stats(&self) -> Result<GraphStats, GraphError> {
        let total_edges: i64 = sqlx::query_scalar("SELECT count(*) FROM graph_edges")
            .fetch_one(self.pool())
            .await
            .map_err(graph_err)?;
        let active_edges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM graph_edges WHERE valid_to IS NULL")
                .fetch_one(self.pool())
                .await
                .map_err(graph_err)?;
        let node_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ( \
                 SELECT from_node_id AS n FROM graph_edges \
                 UNION SELECT to_node_id FROM graph_edges \
             ) u",
        )
        .fetch_one(self.pool())
        .await
        .map_err(graph_err)?;
        let rel_rows = sqlx::query(
            "SELECT relation, count(*) AS c FROM graph_edges \
             GROUP BY relation ORDER BY c DESC, relation ASC LIMIT 16",
        )
        .fetch_all(self.pool())
        .await
        .map_err(graph_err)?;
        let mut top_relations = Vec::with_capacity(rel_rows.len());
        for r in &rel_rows {
            top_relations.push((
                r.try_get::<String, _>("relation").map_err(graph_err)?,
                r.try_get::<i64, _>("c").map_err(graph_err)?,
            ));
        }
        Ok(GraphStats {
            node_count,
            total_edges,
            active_edges,
            closed_edges: total_edges - active_edges,
            top_relations,
        })
    }

    async fn related_capability_capsule_ids(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        // Pull active edges incident on the input set, keep the opposite
        // endpoint, strip the `capability_capsule:` prefix, dedup + sort.
        let owned: Vec<String> = node_ids.to_vec();
        let rows = sqlx::query(
            "SELECT from_node_id, to_node_id FROM graph_edges \
             WHERE (from_node_id = ANY($1) OR to_node_id = ANY($1)) AND valid_to IS NULL",
        )
        .bind(&owned)
        .fetch_all(self.pool())
        .await
        .map_err(graph_err)?;
        let node_set: std::collections::HashSet<&str> =
            node_ids.iter().map(|s| s.as_str()).collect();
        let mut ids = std::collections::HashSet::new();
        for r in &rows {
            let from: String = r.try_get("from_node_id").map_err(graph_err)?;
            let to: String = r.try_get("to_node_id").map_err(graph_err)?;
            for endpoint in [&from, &to] {
                if !node_set.contains(endpoint.as_str()) {
                    if let Some(mid) = endpoint.strip_prefix("capability_capsule:") {
                        ids.insert(mid.to_string());
                    }
                }
            }
        }
        let mut out: Vec<String> = ids.into_iter().collect();
        out.sort();
        Ok(out)
    }

    async fn incident_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = node_ids.to_vec();
        let rows = sqlx::query(
            "SELECT from_node_id, to_node_id FROM graph_edges \
             WHERE (from_node_id = ANY($1) OR to_node_id = ANY($1)) AND valid_to IS NULL",
        )
        .bind(&owned)
        .fetch_all(self.pool())
        .await
        .map_err(graph_err)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("from_node_id").map_err(graph_err)?,
                    r.try_get::<String, _>("to_node_id").map_err(graph_err)?,
                ))
            })
            .collect()
    }

    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError> {
        if edges.is_empty() {
            return Ok(());
        }
        // Idempotent: skip rows whose active (from, to, relation) exists.
        // Server forces valid_from = now, valid_to = NULL (active);
        // confidence/extractor/K9 dynamics preserved. One transaction.
        let mut tx = self.pool().begin().await.map_err(graph_err)?;
        for edge in edges {
            let exists: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM graph_edges \
                 WHERE from_node_id = $1 AND to_node_id = $2 AND relation = $3 \
                   AND valid_to IS NULL",
            )
            .bind(&edge.from_node_id)
            .bind(&edge.to_node_id)
            .bind(&edge.relation)
            .fetch_one(&mut *tx)
            .await
            .map_err(graph_err)?;
            if exists > 0 {
                continue;
            }
            sqlx::query(
                "INSERT INTO graph_edges (from_node_id, to_node_id, relation, valid_from, \
                    valid_to, confidence, extractor, strength, stability, last_activated, \
                    access_count) \
                 VALUES ($1, $2, $3, $4, NULL, $5, $6, $7, $8, $9, $10)",
            )
            .bind(&edge.from_node_id)
            .bind(&edge.to_node_id)
            .bind(&edge.relation)
            .bind(now)
            .bind(edge.confidence)
            .bind(&edge.extractor)
            .bind(edge.strength)
            .bind(edge.stability)
            .bind(&edge.last_activated)
            .bind(edge.access_count)
            .execute(&mut *tx)
            .await
            .map_err(graph_err)?;
        }
        tx.commit().await.map_err(graph_err)?;
        Ok(())
    }

    async fn add_edge_direct(&self, edge: &GraphEdge) -> Result<bool, GraphError> {
        // K12: reject an inverted bitemporal interval (valid_to <
        // valid_from) — it would be stored-but-permanently-invisible.
        if let Some(valid_to) = &edge.valid_to {
            if valid_to.as_str() < edge.valid_from.as_str() {
                return Err(GraphError::InvalidInput(format!(
                    "edge valid_to ({}) precedes valid_from ({}); a recall query would never match it",
                    valid_to, edge.valid_from
                )));
            }
        }
        // Idempotent: skip when an active (from, to, relation) exists.
        // valid_from / valid_to preserved verbatim (caller can backdate
        // or insert a pre-closed edge).
        let mut tx = self.pool().begin().await.map_err(graph_err)?;
        let exists: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM graph_edges \
             WHERE from_node_id = $1 AND to_node_id = $2 AND relation = $3 \
               AND valid_to IS NULL",
        )
        .bind(&edge.from_node_id)
        .bind(&edge.to_node_id)
        .bind(&edge.relation)
        .fetch_one(&mut *tx)
        .await
        .map_err(graph_err)?;
        if exists > 0 {
            tx.rollback().await.map_err(graph_err)?;
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO graph_edges (from_node_id, to_node_id, relation, valid_from, \
                valid_to, confidence, extractor, strength, stability, last_activated, \
                access_count) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(&edge.from_node_id)
        .bind(&edge.to_node_id)
        .bind(&edge.relation)
        .bind(&edge.valid_from)
        .bind(&edge.valid_to)
        .bind(edge.confidence)
        .bind(&edge.extractor)
        .bind(edge.strength)
        .bind(edge.stability)
        .bind(&edge.last_activated)
        .bind(edge.access_count)
        .execute(&mut *tx)
        .await
        .map_err(graph_err)?;
        tx.commit().await.map_err(graph_err)?;
        Ok(true)
    }

    async fn invalidate_edge(
        &self,
        from_node_id: &str,
        predicate: &str,
        to_node_id: &str,
        ended_at: &str,
    ) -> Result<usize, GraphError> {
        // Stamp valid_to on the active (from, predicate, to) edge.
        // Idempotent — returns 0 when no active edge matches.
        let res = sqlx::query(
            "UPDATE graph_edges SET valid_to = $4 \
             WHERE from_node_id = $1 AND to_node_id = $2 AND relation = $3 \
               AND valid_to IS NULL",
        )
        .bind(from_node_id)
        .bind(to_node_id)
        .bind(predicate)
        .bind(ended_at)
        .execute(self.pool())
        .await
        .map_err(graph_err)?;
        Ok(res.rows_affected() as usize)
    }

    async fn close_edges_for_capability_capsule(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, GraphError> {
        // Close every active edge INCIDENT to `capability_capsule:<id>` —
        // outgoing AND incoming (the node-id format the Lance writer uses). A
        // capsule can be the `to_node` of an edge (e.g. a `suspected_supersede`
        // new→canonical edge, or a `contradicts` edge), so a FROM-only close
        // would leave a dangling edge pointing at the capsule. Matches the
        // lance/clickhouse helper.
        let node = format!("capability_capsule:{capability_capsule_id}");
        let now = crate::storage::current_timestamp();
        let res = sqlx::query(
            "UPDATE graph_edges SET valid_to = $2 \
             WHERE (from_node_id = $1 OR to_node_id = $1) AND valid_to IS NULL",
        )
        .bind(&node)
        .bind(&now)
        .execute(self.pool())
        .await
        .map_err(graph_err)?;
        Ok(res.rows_affected() as usize)
    }
}

impl PostgresCapsuleStore {
    /// BFS-visited node set for `neighbors_within`, mirroring the DuckDB
    /// walk's visited set so the edge projection can be trimmed to edges
    /// whose *both* endpoints were reached. `validity` is the pre-built
    /// active/as_of predicate fragment (referencing `$3` when as_of set).
    async fn neighbors_within_visited(
        &self,
        node_id: &str,
        max_hops: u32,
        as_of: Option<&str>,
        validity: &str,
    ) -> Result<std::collections::HashSet<String>, GraphError> {
        let hops = i32::try_from(max_hops.clamp(1, PG_MAX_HOPS_CAP)).unwrap_or(1);
        let sql = format!(
            "WITH RECURSIVE walk(node, depth) AS ( \
                 SELECT $1::TEXT, 0 \
               UNION \
                 SELECT CASE WHEN e.from_node_id = w.node THEN e.to_node_id \
                             ELSE e.from_node_id END, \
                        w.depth + 1 \
                 FROM walk w \
                 JOIN graph_edges e \
                   ON (e.from_node_id = w.node OR e.to_node_id = w.node) \
                  AND {validity} \
                 WHERE w.depth < $2 \
             ) \
             SELECT DISTINCT node FROM walk"
        );
        let mut q = sqlx::query_scalar::<_, String>(&sql)
            .bind(node_id)
            .bind(hops);
        if let Some(ts) = as_of {
            q = q.bind(ts);
        }
        let nodes = q.fetch_all(self.pool()).await.map_err(graph_err)?;
        Ok(nodes.into_iter().collect())
    }

    /// BFS-visited node set for `follow_tunnels` (active user-tunnel
    /// edges only).
    async fn follow_tunnels_visited(
        &self,
        node_id: &str,
        max_hops: u32,
    ) -> Result<std::collections::HashSet<String>, GraphError> {
        let hops = i32::try_from(max_hops.clamp(1, PG_MAX_HOPS_CAP)).unwrap_or(1);
        let sql = "WITH RECURSIVE walk(node, depth) AS ( \
                 SELECT $1::TEXT, 0 \
               UNION \
                 SELECT CASE WHEN e.from_node_id = w.node THEN e.to_node_id \
                             ELSE e.from_node_id END, \
                        w.depth + 1 \
                 FROM walk w \
                 JOIN graph_edges e \
                   ON (e.from_node_id = w.node OR e.to_node_id = w.node) \
                  AND e.relation LIKE 'user_tunnel:%' AND e.valid_to IS NULL \
                 WHERE w.depth < $2 \
             ) \
             SELECT DISTINCT node FROM walk";
        let nodes = sqlx::query_scalar::<_, String>(sql)
            .bind(node_id)
            .bind(hops)
            .fetch_all(self.pool())
            .await
            .map_err(graph_err)?;
        Ok(nodes.into_iter().collect())
    }
}

// ─────────────────────────────── TranscriptStore ──────────────────────────

/// `conversation_messages` SELECT column order shared by every
/// transcript read. Keep in sync with [`pg_row_to_conversation_message`].
const CONVERSATION_COLS: &str = "message_block_id, session_id, tenant, caller_agent, \
    transcript_path, line_number, block_index, message_uuid, role, block_type, content, \
    tool_name, tool_use_id, embed_eligible, created_at, meta_json";

/// Project a `conversation_messages` row into a [`ConversationMessage`].
/// `line_number` / `block_index` are BIGINT on disk → narrow to u64/u32.
fn pg_row_to_conversation_message(
    row: &sqlx::postgres::PgRow,
) -> Result<ConversationMessage, StorageError> {
    use crate::domain::{BlockType, MessageRole};
    let line_number: i64 = row.try_get("line_number").map_err(sqlx_err)?;
    let block_index: i64 = row.try_get("block_index").map_err(sqlx_err)?;
    let role: String = row.try_get("role").map_err(sqlx_err)?;
    let block_type: String = row.try_get("block_type").map_err(sqlx_err)?;
    Ok(ConversationMessage {
        message_block_id: row.try_get("message_block_id").map_err(sqlx_err)?,
        session_id: row.try_get("session_id").map_err(sqlx_err)?,
        tenant: row.try_get("tenant").map_err(sqlx_err)?,
        caller_agent: row.try_get("caller_agent").map_err(sqlx_err)?,
        transcript_path: row.try_get("transcript_path").map_err(sqlx_err)?,
        line_number: u64::try_from(line_number)
            .map_err(|_| StorageError::InvalidData("negative line_number"))?,
        block_index: u32::try_from(block_index)
            .map_err(|_| StorageError::InvalidData("block_index out of range"))?,
        message_uuid: row.try_get("message_uuid").map_err(sqlx_err)?,
        role: MessageRole::from_db_str(&role)
            .ok_or(StorageError::InvalidData("unknown message role"))?,
        block_type: BlockType::from_db_str(&block_type)
            .ok_or(StorageError::InvalidData("unknown block type"))?,
        content: row.try_get("content").map_err(sqlx_err)?,
        tool_name: row.try_get("tool_name").map_err(sqlx_err)?,
        tool_use_id: row.try_get("tool_use_id").map_err(sqlx_err)?,
        embed_eligible: row.try_get("embed_eligible").map_err(sqlx_err)?,
        created_at: row.try_get("created_at").map_err(sqlx_err)?,
        meta_json: row.try_get("meta_json").map_err(sqlx_err)?,
    })
}

#[async_trait]
impl TranscriptStore for PostgresCapsuleStore {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        // Idempotent on (transcript_path, line_number, block_index) via
        // INSERT ... WHERE NOT EXISTS. On a fresh insert of an
        // embed-eligible block, also enqueue a transcript_embedding_job
        // (the fan-out the trait surface hides — mirrors Lance).
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        let res = sqlx::query(
            "INSERT INTO conversation_messages (message_block_id, session_id, tenant, \
                caller_agent, transcript_path, line_number, block_index, message_uuid, \
                role, block_type, content, tool_name, tool_use_id, embed_eligible, \
                created_at, meta_json) \
             SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16 \
             WHERE NOT EXISTS ( \
                 SELECT 1 FROM conversation_messages \
                 WHERE transcript_path = $5 AND line_number = $6 AND block_index = $7 \
             )",
        )
        .bind(&msg.message_block_id)
        .bind(&msg.session_id)
        .bind(&msg.tenant)
        .bind(&msg.caller_agent)
        .bind(&msg.transcript_path)
        .bind(msg.line_number as i64)
        .bind(i64::from(msg.block_index))
        .bind(&msg.message_uuid)
        .bind(msg.role.as_db_str())
        .bind(msg.block_type.as_db_str())
        .bind(&msg.content)
        .bind(&msg.tool_name)
        .bind(&msg.tool_use_id)
        .bind(msg.embed_eligible)
        .bind(&msg.created_at)
        .bind(&msg.meta_json)
        .execute(&mut *tx)
        .await
        .map_err(sqlx_err)?;

        // Only enqueue when the row actually landed AND is embed-eligible.
        if res.rows_affected() > 0 && msg.embed_eligible {
            let provider = self
                .transcript_job_provider()
                .ok_or(StorageError::InvalidData(
                    "transcript embedding job provider not configured; \
                 call set_transcript_job_provider during startup",
                ))?;
            let job_id = uuid::Uuid::now_v7().to_string();
            let now = crate::storage::current_timestamp();
            sqlx::query(
                "INSERT INTO transcript_embedding_jobs (job_id, tenant, message_block_id, \
                    provider, status, attempt_count, last_error, available_at, created_at, \
                    updated_at) \
                 VALUES ($1, $2, $3, $4, 'pending', 0, NULL, $5, $5, $5)",
            )
            .bind(&job_id)
            .bind(&msg.tenant)
            .bind(&msg.message_block_id)
            .bind(&provider)
            .bind(&now)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(())
    }

    async fn create_conversation_messages(
        &self,
        msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        if msgs.is_empty() {
            return Ok(0);
        }
        // Per-row dedup + enqueue inside one transaction. A HashSet
        // tracks intra-batch dup keys so two rows with the same
        // (path, line, block) in the input don't both land.
        let mut seen: std::collections::HashSet<(String, u64, u32)> =
            std::collections::HashSet::new();
        let mut landed = 0usize;
        let mut tx = self.pool().begin().await.map_err(sqlx_err)?;
        for msg in msgs {
            let key = (
                msg.transcript_path.clone(),
                msg.line_number,
                msg.block_index,
            );
            if !seen.insert(key) {
                continue;
            }
            let res = sqlx::query(
                "INSERT INTO conversation_messages (message_block_id, session_id, tenant, \
                    caller_agent, transcript_path, line_number, block_index, message_uuid, \
                    role, block_type, content, tool_name, tool_use_id, embed_eligible, \
                    created_at, meta_json) \
                 SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16 \
                 WHERE NOT EXISTS ( \
                     SELECT 1 FROM conversation_messages \
                     WHERE transcript_path = $5 AND line_number = $6 AND block_index = $7 \
                 )",
            )
            .bind(&msg.message_block_id)
            .bind(&msg.session_id)
            .bind(&msg.tenant)
            .bind(&msg.caller_agent)
            .bind(&msg.transcript_path)
            .bind(msg.line_number as i64)
            .bind(i64::from(msg.block_index))
            .bind(&msg.message_uuid)
            .bind(msg.role.as_db_str())
            .bind(msg.block_type.as_db_str())
            .bind(&msg.content)
            .bind(&msg.tool_name)
            .bind(&msg.tool_use_id)
            .bind(msg.embed_eligible)
            .bind(&msg.created_at)
            .bind(&msg.meta_json)
            .execute(&mut *tx)
            .await
            .map_err(sqlx_err)?;
            if res.rows_affected() == 0 {
                continue;
            }
            landed += 1;
            if msg.embed_eligible {
                let provider = self
                    .transcript_job_provider()
                    .ok_or(StorageError::InvalidData(
                        "transcript embedding job provider not configured; \
                         call set_transcript_job_provider during startup",
                    ))?;
                let job_id = uuid::Uuid::now_v7().to_string();
                let now = crate::storage::current_timestamp();
                sqlx::query(
                    "INSERT INTO transcript_embedding_jobs (job_id, tenant, message_block_id, \
                        provider, status, attempt_count, last_error, available_at, created_at, \
                        updated_at) \
                     VALUES ($1, $2, $3, $4, 'pending', 0, NULL, $5, $5, $5)",
                )
                .bind(&job_id)
                .bind(&msg.tenant)
                .bind(&msg.message_block_id)
                .bind(&provider)
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(sqlx_err)?;
            }
        }
        tx.commit().await.map_err(sqlx_err)?;
        Ok(landed)
    }

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND session_id = $2 \
             ORDER BY created_at ASC, line_number ASC, block_index ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .bind(session_id)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_conversation_message).collect()
    }

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
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        // Static SQL with optional filters via `($n IS NULL OR ...)` and
        // an explicit composite-cursor tuple comparison. Fetch limit+1
        // to detect has_more. Cursor bound as three separate params.
        let lim = i64::try_from(limit).unwrap_or(64);
        let fetch = lim.saturating_add(1);
        let (cur_at, cur_line, cur_idx) = match cursor {
            Some((s, l, b)) => (Some(s), Some(l), Some(b)),
            None => (None, None, None),
        };
        let sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND session_id = $2 \
               AND ($3::TEXT IS NULL OR created_at >= $3) \
               AND ($4::TEXT IS NULL OR created_at < $4) \
               AND ($5::TEXT IS NULL OR role = $5) \
               AND ($6::TEXT IS NULL OR block_type = $6) \
               AND ($7::TEXT IS NULL OR ( \
                    created_at > $7 \
                 OR (created_at = $7 AND line_number > $8) \
                 OR (created_at = $7 AND line_number = $8 AND block_index > $9))) \
             ORDER BY created_at ASC, line_number ASC, block_index ASC \
             LIMIT $10"
        );
        let mut out = sqlx::query(&sql)
            .bind(tenant)
            .bind(session_id)
            .bind(since)
            .bind(until)
            .bind(role)
            .bind(block_type)
            .bind(cur_at)
            .bind(cur_line)
            .bind(cur_idx)
            .bind(fetch)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?
            .iter()
            .map(pg_row_to_conversation_message)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = out.len() as i64 == fetch;
        if has_more {
            out.pop();
        }
        Ok((out, has_more))
    }

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let rows = sqlx::query(
            "SELECT session_id, count(*) AS block_count, min(created_at) AS first_at, \
                    max(created_at) AS last_at, max(caller_agent) AS caller_agent \
             FROM conversation_messages \
             WHERE tenant = $1 AND session_id IS NOT NULL \
             GROUP BY session_id \
             ORDER BY last_at DESC",
        )
        .bind(tenant)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                Ok(TranscriptSessionSummary {
                    session_id: r.try_get("session_id").map_err(sqlx_err)?,
                    block_count: r.try_get("block_count").map_err(sqlx_err)?,
                    first_at: r.try_get("first_at").map_err(sqlx_err)?,
                    last_at: r.try_get("last_at").map_err(sqlx_err)?,
                    caller_agent: r.try_get("caller_agent").map_err(sqlx_err)?,
                })
            })
            .collect()
    }

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
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        // Cross-session range scan: session_id IS NOT NULL, half-open
        // [time_from, time_to), same composite cursor + filters as the
        // per-session paged read.
        let lim = i64::try_from(limit).unwrap_or(64);
        let fetch = lim.saturating_add(1);
        let (cur_at, cur_line, cur_idx) = match cursor {
            Some((s, l, b)) => (Some(s), Some(l), Some(b)),
            None => (None, None, None),
        };
        let sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND session_id IS NOT NULL \
               AND ($2::TEXT IS NULL OR created_at >= $2) \
               AND ($3::TEXT IS NULL OR created_at < $3) \
               AND ($4::TEXT IS NULL OR role = $4) \
               AND ($5::TEXT IS NULL OR block_type = $5) \
               AND ($6::TEXT IS NULL OR ( \
                    created_at > $6 \
                 OR (created_at = $6 AND line_number > $7) \
                 OR (created_at = $6 AND line_number = $7 AND block_index > $8))) \
             ORDER BY created_at ASC, line_number ASC, block_index ASC \
             LIMIT $9"
        );
        let mut out = sqlx::query(&sql)
            .bind(tenant)
            .bind(time_from)
            .bind(time_to)
            .bind(role)
            .bind(block_type)
            .bind(cur_at)
            .bind(cur_line)
            .bind(cur_idx)
            .bind(fetch)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?
            .iter()
            .map(pg_row_to_conversation_message)
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = out.len() as i64 == fetch;
        if has_more {
            out.pop();
        }
        Ok((out, has_more))
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        // WHERE message_block_id = ANY($2); preserve input-slice order
        // with missing ids dropped (the DuckDB contract).
        let owned: Vec<String> = ids.to_vec();
        let sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND message_block_id = ANY($2)"
        );
        let fetched = sqlx::query(&sql)
            .bind(tenant)
            .bind(&owned)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?
            .iter()
            .map(pg_row_to_conversation_message)
            .collect::<Result<Vec<_>, _>>()?;
        let mut by_id: std::collections::HashMap<String, ConversationMessage> = fetched
            .into_iter()
            .map(|m| (m.message_block_id.clone(), m))
            .collect();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(m) = by_id.remove(id) {
                out.push(m);
            }
        }
        Ok(out)
    }

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        // 1. Primary fetch (NotFound when absent).
        let primary_sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND message_block_id = $2"
        );
        let primary_row = sqlx::query(&primary_sql)
            .bind(tenant)
            .bind(primary_id)
            .fetch_optional(self.pool())
            .await
            .map_err(sqlx_err)?;
        let primary = match primary_row {
            Some(r) => pg_row_to_conversation_message(&r)?,
            None => return Err(StorageError::NotFound("transcript primary block")),
        };

        // 2. No session → no neighbors.
        let session_id = match primary.session_id.clone() {
            Some(s) => s,
            None => {
                return Ok(ContextWindow {
                    primary,
                    before: Vec::new(),
                    after: Vec::new(),
                })
            }
        };

        let type_filter = if include_tool_blocks {
            ""
        } else {
            "AND block_type IN ('text', 'thinking') "
        };
        let k_before_i = i64::try_from(k_before).unwrap_or(0);
        let k_after_i = i64::try_from(k_after).unwrap_or(0);
        let p_line = primary.line_number as i64;
        let p_idx = i64::from(primary.block_index);

        // 3. Predecessors (strict tuple <), DESC then reversed to ASC.
        let before_sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND session_id = $2 \
               AND (created_at < $3 \
                 OR (created_at = $3 AND line_number < $4) \
                 OR (created_at = $3 AND line_number = $4 AND block_index < $5)) \
               {type_filter} \
             ORDER BY created_at DESC, line_number DESC, block_index DESC \
             LIMIT $6"
        );
        let mut before = sqlx::query(&before_sql)
            .bind(tenant)
            .bind(&session_id)
            .bind(&primary.created_at)
            .bind(p_line)
            .bind(p_idx)
            .bind(k_before_i)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?
            .iter()
            .map(pg_row_to_conversation_message)
            .collect::<Result<Vec<_>, _>>()?;
        before.reverse();

        // 4. Successors (strict tuple >), ASC.
        let after_sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND session_id = $2 \
               AND (created_at > $3 \
                 OR (created_at = $3 AND line_number > $4) \
                 OR (created_at = $3 AND line_number = $4 AND block_index > $5)) \
               {type_filter} \
             ORDER BY created_at ASC, line_number ASC, block_index ASC \
             LIMIT $6"
        );
        let after = sqlx::query(&after_sql)
            .bind(tenant)
            .bind(&session_id)
            .bind(&primary.created_at)
            .bind(p_line)
            .bind(p_idx)
            .bind(k_after_i)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?
            .iter()
            .map(pg_row_to_conversation_message)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ContextWindow {
            primary,
            before,
            after,
        })
    }

    async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let k_i = i64::try_from(k).unwrap_or(64);
        sqlx::query_scalar::<_, String>(
            "SELECT message_block_id FROM conversation_messages \
             WHERE tenant = $1 AND session_id = $2 AND embed_eligible = true \
             ORDER BY created_at DESC \
             LIMIT $3",
        )
        .bind(tenant)
        .bind(session_id)
        .bind(k_i)
        .fetch_all(self.pool())
        .await
        .map_err(sqlx_err)
    }

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let sql = format!(
            "SELECT {CONVERSATION_COLS} FROM conversation_messages \
             WHERE tenant = $1 AND embed_eligible = true \
             ORDER BY created_at DESC, line_number DESC, block_index DESC \
             LIMIT $2"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .bind(lim)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_conversation_message).collect()
    }

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // tsvector @@ plainto_tsquery('simple', q) over content_tsv (the
        // GIN-indexed generated column from 0004). embed_eligible scope,
        // ranked ts_rank DESC then id ASC, LIMIT k. Parity with the
        // capsule-side bm25 channel.
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        let cols = CONVERSATION_COLS
            .split(',')
            .map(|c| format!("m.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols} FROM conversation_messages m \
             WHERE m.tenant = $1 AND m.embed_eligible = true \
               AND m.content_tsv @@ plainto_tsquery('simple', $2) \
             ORDER BY ts_rank(m.content_tsv, plainto_tsquery('simple', $2)) DESC, \
                      m.message_block_id ASC \
             LIMIT $3"
        );
        let rows = sqlx::query(&sql)
            .bind(tenant)
            .bind(query)
            .bind(k_i)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter().map(pg_row_to_conversation_message).collect()
    }

    async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Lazy-create parity: no embeddings table yet → empty.
        if !embeddings_table_exists(self, "conversation_message_embeddings").await? {
            return Ok(Vec::new());
        }
        // pgvector `<=>` cosine distance, DISTINCT-ON dedup to the nearest
        // chunk per message, joined back to conversation_messages, scoped
        // embed_eligible. Score = cosine similarity = 1 - distance (pgvector
        // cosine distance is 1 - cosine_similarity). Ordered nearest first.
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let qv = pgvector::Vector::from(query_embedding.to_vec());
        let cols = CONVERSATION_COLS
            .split(',')
            .map(|c| format!("m.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {cols}, d.best_distance \
             FROM ( \
                 SELECT DISTINCT ON (message_block_id) \
                        message_block_id, (embedding <=> $1) AS best_distance \
                 FROM conversation_message_embeddings \
                 WHERE tenant = $2 \
                 ORDER BY message_block_id, embedding <=> $1 \
             ) d \
             JOIN conversation_messages m ON m.message_block_id = d.message_block_id \
             WHERE m.tenant = $2 AND m.embed_eligible = true \
             ORDER BY d.best_distance ASC \
             LIMIT $3"
        );
        let rows = sqlx::query(&sql)
            .bind(qv)
            .bind(tenant)
            .bind(lim)
            .fetch_all(self.pool())
            .await
            .map_err(sqlx_err)?;
        rows.iter()
            .map(|r| {
                let msg = pg_row_to_conversation_message(r)?;
                let distance: f64 = r.try_get("best_distance").map_err(sqlx_err)?;
                Ok((msg, 1.0_f32 - distance as f32))
            })
            .collect()
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
// bodies (Lance-specific no-ops for non-Lance backends) — left to the
// defaults here so they apply unchanged. Only the three
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
        // Mirror the lance decay sweep (lance_store/decay.rs) in PG
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
