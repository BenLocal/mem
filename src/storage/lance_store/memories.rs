//! Memory CRUD + filter + lookup + embedding-job + episode/session +
//! feedback methods. All inherent on LanceStore. Helpers
//! (`query_memories`, `update_status`, `query_embedding_jobs`) used
//! across these methods live with their domain rather than in
//! `mod.rs`.

use arrow_array::{Float32Array, RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;

use super::{
    decode_embedding_blob, embedding_job_row_to_record_batch, ensure_memory_embeddings_table,
    enum_to_str, feedback_adjustments, feedback_events_to_record_batch, lancedb_err,
    memories_to_record_batch, memory_embedding_to_record_batch, record_batch_to_embedding_job_rows,
    record_batch_to_feedback_events, record_batch_to_memories, sql_quote, EmbeddingJobRow,
    LanceStore,
};
use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::memory::{FeedbackSummary, MemoryRecord, MemoryVersionLink};
use crate::domain::session::Session;
use crate::storage::types::{ClaimedEmbeddingJob, EmbeddingJobInsert, FeedbackEvent, StorageError};

impl LanceStore {
    /// Apply a status transition to `(tenant, memory_id)` and return the
    /// updated row. Shared by `accept_pending` / `reject_pending` (and a
    /// future `archive_pending` if needed). Mirrors the DuckDB backend's
    /// `update_status` private helper.
    ///
    /// **Not yet implemented:** the embedding-references cleanup that the
    /// DuckDB version does (delete `embedding_jobs` + `memory_embeddings`
    /// rows for this memory) — those tables don't exist on the LanceDB
    /// side yet. Add when those tables land.
    pub async fn update_status(
        &self,
        tenant: &str,
        memory_id: &str,
        status_str: &str,
    ) -> Result<MemoryRecord, StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let now = crate::storage::current_timestamp();
        let result = table
            .update()
            .only_if(format!(
                "tenant = {} AND memory_id = {}",
                sql_quote(tenant),
                sql_quote(memory_id),
            ))
            .column("status", sql_quote(status_str))
            .column("updated_at", sql_quote(&now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        self.get_memory_for_tenant(tenant, memory_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after status update",
            ))
    }

    /// Run a filter query against the `memories` table and parse all
    /// returned batches into [`MemoryRecord`]s. Shared by every read
    /// method that just needs a `WHERE`-clause + optional `LIMIT`.
    pub async fn query_memories(
        &self,
        filter: String,
        limit: Option<usize>,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut q = table.query().only_if(filter);
        if let Some(l) = limit {
            q = q.limit(l);
        }
        let stream = q.execute().await.map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_memories(b)?);
        }
        Ok(out)
    }

    /// Read all `embedding_jobs` rows matching `filter`, parsed into
    /// [`EmbeddingJobRow`]s. Shared by every queue read path: the claim
    /// flow, `first_embedding_job_id_for_memory`, `list_embedding_jobs`,
    /// and the duplicate-detection in `try_enqueue_embedding_job`.
    pub(crate) async fn query_embedding_jobs(
        &self,
        filter: String,
    ) -> Result<Vec<EmbeddingJobRow>, StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_embedding_job_rows(b)?);
        }
        Ok(out)
    }
}

/// Memory CRUD + filter + embedding-job + feedback methods —
/// previously bound by the `MemoryRepository` trait, now plain
/// inherent. Methods kept as `pub async fn`; signatures unchanged.
#[allow(clippy::too_many_arguments)]
impl LanceStore {
    pub async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = memories_to_record_batch(std::slice::from_ref(&memory))?;
        // `RecordBatch` impls `Scannable` directly — no need to wrap in an
        // iterator. (Re-checking lancedb-0.27.2/src/data/scannable.rs L70.)
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(memory)
    }

    pub async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        // Idempotency check: if any live (pending/processing) row already
        // covers this (tenant, memory_id, target_content_hash, provider)
        // tuple, decline the enqueue. LanceDB has no transactions so the
        // count → insert window is racy under concurrent writers, but mem
        // serve runs one writer per DB so the race is single-instance safe.
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let live = table
            .count_rows(Some(format!(
                "tenant = {} AND memory_id = {} AND target_content_hash = {} \
                 AND provider = {} AND (status = 'pending' OR status = 'processing')",
                sql_quote(&insert.tenant),
                sql_quote(&insert.memory_id),
                sql_quote(&insert.target_content_hash),
                sql_quote(&insert.provider),
            )))
            .await
            .map_err(lancedb_err)?;
        if live > 0 {
            return Ok(false);
        }
        let row = EmbeddingJobRow {
            job_id: insert.job_id,
            tenant: insert.tenant,
            memory_id: insert.memory_id,
            target_content_hash: insert.target_content_hash,
            provider: insert.provider,
            status: "pending".to_string(),
            attempt_count: 0,
            last_error: None,
            available_at: insert.available_at,
            created_at: insert.created_at,
            updated_at: insert.updated_at,
        };
        let batch = embedding_job_row_to_record_batch(&row)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(true)
    }

    pub async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let mut rows = self
            .query_embedding_jobs(format!("memory_id = {}", sql_quote(memory_id)))
            .await?;
        // LanceDB has no ORDER BY — sort in memory by created_at ASC
        // (same shape as the DuckDB SQL).
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(rows.into_iter().next().map(|r| r.job_id))
    }

    pub async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        if n == 0 {
            return Ok(vec![]);
        }
        // Eligible = available_at <= now AND (pending OR (failed AND
        // attempt_count < max_retries)). LanceDB has no ORDER BY, so we
        // pull all eligible rows and sort by (available_at, created_at)
        // ASC in memory before slicing — queue depth is expected to be
        // small (worker drains continuously) so the in-memory cost is
        // negligible vs. the simpler code.
        //
        // Note: unlike DuckDB we don't sweep orphan jobs here. LanceDB
        // has no FK constraints, so the FK-loop pathology that motivated
        // the orphan sweep on DuckDB cannot occur here. If a memory is
        // deleted, its embedding_jobs rows simply stay until the worker
        // touches them; the FK-error retry loop is a DuckDB-only bug.
        let max_r = i64::from(max_retries);
        let filter = format!(
            "available_at <= {} AND (status = 'pending' OR (status = 'failed' AND attempt_count < {}))",
            sql_quote(now),
            max_r,
        );
        let mut rows = self.query_embedding_jobs(filter).await?;
        rows.sort_by(|a, b| {
            a.available_at
                .cmp(&b.available_at)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });
        rows.truncate(n);
        if rows.is_empty() {
            return Ok(vec![]);
        }

        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut claimed = Vec::with_capacity(rows.len());
        for r in rows {
            // Optimistic claim: only update if status is still eligible
            // (pending, or failed-with-budget). A second-instance race
            // would see rows_updated == 0 and we'd skip the row — same
            // shape as DuckDB's "updated == 0 → return None" branch.
            let result = table
                .update()
                .only_if(format!(
                    "job_id = {} AND (status = 'pending' OR (status = 'failed' AND attempt_count < {}))",
                    sql_quote(&r.job_id),
                    max_r,
                ))
                .column("status", "'processing'")
                .column("updated_at", sql_quote(now))
                .execute()
                .await
                .map_err(lancedb_err)?;
            if result.rows_updated == 0 {
                continue;
            }
            claimed.push(ClaimedEmbeddingJob {
                job_id: r.job_id,
                tenant: r.tenant,
                memory_id: r.memory_id,
                target_content_hash: r.target_content_hash,
                provider: r.provider,
                attempt_count: r.attempt_count,
            });
        }
        Ok(claimed)
    }

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
        let dim_i32 = i32::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim does not fit in i32"))?;
        let vector = decode_embedding_blob(embedding_blob, embedding_dim as usize)?;

        ensure_memory_embeddings_table(&self.conn, dim_i32).await?;

        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // upsert = delete-then-insert. LanceDB has no PK enforcement so
        // we sweep any existing row for this memory_id first.
        table
            .delete(&format!("memory_id = {}", sql_quote(memory_id)))
            .await
            .map_err(lancedb_err)?;
        let batch = memory_embedding_to_record_batch(
            memory_id,
            tenant,
            embedding_model,
            embedding_dim,
            &vector,
            content_hash,
            source_updated_at,
            now,
        )?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        // No-op if the table doesn't exist yet (semantic search hasn't
        // been used; nothing to delete).
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "memory_embeddings") {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!("memory_id = {}", sql_quote(memory_id)))
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query_memories(format!("tenant = {}", sql_quote(tenant)), None)
            .await
    }

    pub async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        // No embeddings written yet → empty result (matches DuckDB
        // legacy linear-scan behavior on an empty memory_embeddings).
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "memory_embeddings") {
            return Ok(vec![]);
        }
        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;

        // Vector search with tenant prefilter (default mode). LanceDB
        // filters before ANN, so tenant-scoping is correct even when an
        // ANN index is later attached.
        let stream = table
            .vector_search(query_embedding)
            .map_err(lancedb_err)?
            .distance_type(DistanceType::Cosine)
            .only_if(format!("tenant = {}", sql_quote(tenant)))
            .limit(limit)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;

        // Collect (memory_id, score) pairs in distance-ascending order.
        // LanceDB returns rows already sorted by `_distance`; preserve
        // that order across batches by extending sequentially.
        let mut hits: Vec<(String, f32)> = Vec::new();
        for b in &batches {
            let memory_ids = b
                .column_by_name("memory_id")
                .ok_or(StorageError::InvalidData("missing memory_id column"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or(StorageError::InvalidData("memory_id column type mismatch"))?;
            let distances = b
                .column_by_name("_distance")
                .ok_or(StorageError::InvalidData(
                    "missing _distance column from vector_search",
                ))?
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or(StorageError::InvalidData("_distance column type mismatch"))?;
            for i in 0..b.num_rows() {
                // Cosine distance ∈ [0, 2]; similarity = 1 - distance
                // matches DuckDB backend's cosine_similarity score
                // shape (higher = better, normalized vectors → [0, 1]).
                let score = 1.0 - distances.value(i);
                hits.push((memory_ids.value(i).to_string(), score));
            }
        }

        // Hydrate full MemoryRecord rows. fetch_memories_by_ids returns
        // out of input order, so we rebuild the score-ordered list
        // afterwards via a hashmap lookup.
        let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
        let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let records = self.fetch_memories_by_ids(tenant, &id_refs).await?;
        let by_id: std::collections::HashMap<String, MemoryRecord> = records
            .into_iter()
            .map(|m| (m.memory_id.clone(), m))
            .collect();
        let mut out = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            if let Some(rec) = by_id.get(&id) {
                out.push((rec.clone(), score));
            }
            // Else: embedding row exists but memory was archived/deleted
            // after embedding write — skip silently, matches DuckDB's
            // implicit-join semantics.
        }
        Ok(out)
    }

    pub async fn complete_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        // Mirror DuckDB: only complete a row that's currently 'processing'
        // (otherwise it's already completed/stale and we shouldn't bump it).
        // LanceDB doesn't have a NULL literal for last_error inside the
        // update column expression in a way the SQL parser tolerates as
        // an arbitrary expression — we encode "clear last_error" as
        // `CAST(NULL AS string)` so the column value is a SQL NULL.
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(format!(
                "job_id = {} AND status = 'processing'",
                sql_quote(job_id),
            ))
            .column("status", "'completed'")
            .column("last_error", "CAST(NULL AS string)")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn mark_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(format!("job_id = {}", sql_quote(job_id)))
            .column("status", "'stale'")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(format!("job_id = {}", sql_quote(job_id)))
            .column("status", "'failed'")
            .column("attempt_count", new_attempt_count.to_string())
            .column("last_error", sql_quote(last_error))
            .column("available_at", sql_quote(available_at))
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(format!("job_id = {}", sql_quote(job_id)))
            .column("status", "'failed'")
            .column("attempt_count", new_attempt_count.to_string())
            .column("last_error", sql_quote(last_error))
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError> {
        // Pre-count to return how many rows we delete (LanceDB's
        // DeleteResult only carries num_deleted_rows, but we want this
        // to match DuckDB's `Connection::execute(DELETE)` rowcount
        // contract regardless).
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let count = table
            .count_rows(Some(format!("memory_id = {}", sql_quote(memory_id))))
            .await
            .map_err(lancedb_err)?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .delete(&format!("memory_id = {}", sql_quote(memory_id)))
            .await
            .map_err(lancedb_err)?;
        // Lance servers older than this codebase may report 0 here even
        // when rows were deleted (the count_rows pre-flight is the
        // canonical source for the count we return).
        if result.num_deleted_rows == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.num_deleted_rows).unwrap_or(count))
        }
    }

    pub async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let filter = format!(
            "tenant = {} AND memory_id = {}",
            sql_quote(tenant),
            sql_quote(memory_id),
        );
        let stream = table
            .query()
            .only_if(filter)
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        for batch in &batches {
            let mems = record_batch_to_memories(batch)?;
            if let Some(m) = mems.into_iter().next() {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    pub async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let filter = format!(
            "tenant = {} AND memory_id = {} AND status = 'pending_confirmation'",
            sql_quote(tenant),
            sql_quote(memory_id),
        );
        Ok(self
            .query_memories(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        // Match either `idempotency_key` (when caller provided one) OR
        // `content_hash` — same precedence as DuckDB's variant.
        let filter = match idempotency_key.as_deref() {
            Some(k) => format!(
                "tenant = {} AND (idempotency_key = {} OR content_hash = {})",
                sql_quote(tenant),
                sql_quote(k),
                sql_quote(content_hash),
            ),
            None => format!(
                "tenant = {} AND content_hash = {}",
                sql_quote(tenant),
                sql_quote(content_hash),
            ),
        };
        Ok(self
            .query_memories(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let filter = format!(
            "tenant = {} AND status = 'pending_confirmation'",
            sql_quote(tenant),
        );
        self.query_memories(filter, None).await
    }

    pub async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        // Same live-status filter the DuckDB backend uses
        // (`pipeline::retrieve` post-filters this set anyway).
        let filter = format!(
            "tenant = {} AND status NOT IN ('rejected', 'archived')",
            sql_quote(tenant),
        );
        self.query_memories(filter, None).await
    }

    pub async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        // NOTE: LanceDB's `Query::limit` doesn't guarantee any ordering
        // without a `Table::create_index` on `updated_at`. For now this
        // returns _some_ N rows; switching to ordered results requires
        // an index + `Query::nearest_to` or a sort step. The DuckDB
        // backend uses `ORDER BY updated_at DESC`.
        let filter = format!(
            "tenant = {} AND status NOT IN ('rejected', 'archived')",
            sql_quote(tenant),
        );
        self.query_memories(filter, Some(limit)).await
    }

    pub async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(vec![]);
        }
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;

        // FTS index is built once at `LanceStore::open` time on the
        // `content` column (see `ensure_fts_index`); no per-call check.
        let fts_query = lancedb::index::scalar::FullTextSearchQuery::new(query.to_string());
        let stream = table
            .query()
            .full_text_search(fts_query)
            .only_if(format!("tenant = {}", sql_quote(tenant)))
            .limit(k)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_memories(b)?);
        }
        Ok(out)
    }

    pub async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let id_list: Vec<String> = ids.iter().map(|i| sql_quote(i)).collect();
        let filter = format!(
            "tenant = {} AND status NOT IN ('rejected', 'archived') AND memory_id IN ({})",
            sql_quote(tenant),
            id_list.join(", "),
        );
        self.query_memories(filter, None).await
    }

    pub async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, "active").await
    }

    pub async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, "rejected").await
    }

    pub async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        // Two-step supersede: archive the old row, then insert the new
        // one. LanceDB has no transaction semantics across these calls,
        // so a crash between them leaves the old archived without a
        // successor — same risk profile as the DuckDB backend's
        // non-tx'd version (see `replace_pending_with_successor` in
        // duckdb/mod.rs).
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let now = crate::storage::current_timestamp();
        table
            .update()
            .only_if(format!(
                "tenant = {} AND memory_id = {}",
                sql_quote(tenant),
                sql_quote(original_memory_id),
            ))
            .column("status", "'archived'")
            .column("updated_at", sql_quote(&now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = memories_to_record_batch(std::slice::from_ref(&successor))?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(successor)
    }

    pub async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        let (conf_delta, decay_delta, status_after, mark_validated) =
            feedback_adjustments(&feedback.feedback_kind)
                .ok_or(StorageError::InvalidData("invalid feedback kind"))?;
        let updated_at = feedback.created_at.clone();
        let mut updated = memory.clone();
        updated.updated_at = updated_at.clone();
        updated.confidence = (updated.confidence + conf_delta).clamp(0.0, 1.0);
        updated.decay_score = (updated.decay_score + decay_delta).clamp(0.0, 1.0);
        if let Some(ref s) = status_after {
            updated.status = s.clone();
        }
        if mark_validated {
            updated.last_validated_at = Some(updated_at.clone());
        }

        // Always log the event first — independent of the parent UPDATE
        // succeeding, the audit trail is preserved. (Mirrors the DuckDB
        // backend's ordering.)
        let fb_table = self
            .conn
            .open_table("feedback_events")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = feedback_events_to_record_batch(std::slice::from_ref(&feedback))?;
        fb_table.add(batch).execute().await.map_err(lancedb_err)?;

        // Update the parent memory row. Status / last_validated_at are
        // optionally set; confidence + decay + updated_at always.
        let mem_table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut update = mem_table
            .update()
            .only_if(format!("memory_id = {}", sql_quote(&updated.memory_id)))
            .column("confidence", format!("{}", updated.confidence))
            .column("decay_score", format!("{}", updated.decay_score))
            .column("updated_at", sql_quote(&updated.updated_at));
        if let Some(s) = status_after {
            update = update.column("status", sql_quote(&enum_to_str(&s)?));
        }
        if mark_validated {
            update = update.column("last_validated_at", sql_quote(&updated_at));
        }
        update.execute().await.map_err(lancedb_err)?;
        Ok(updated)
    }

    pub async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let table = self
            .conn
            .open_table("feedback_events")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!("memory_id = {}", sql_quote(memory_id)))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_feedback_events(b)?);
        }
        // DuckDB returns `created_at ASC` order. LanceDB doesn't sort
        // automatically — sort client-side since the row count per
        // memory is small (single-digits typically).
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
    }

    pub async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!(
            "LanceDb::list_memory_versions_for_tenant — see docs/repository.rs trait def"
        )
    }

    pub async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        // Fetch all events for this memory and aggregate client-side.
        // Counts are tiny (events per memory typically < 10), so the
        // network/parse cost is negligible compared to running a
        // GROUP BY query through LanceDB's filter API.
        let events = self.list_feedback_for_memory(memory_id).await?;
        let mut summary = FeedbackSummary::default();
        for e in events {
            summary.total += 1;
            match e.feedback_kind.as_str() {
                "useful" => summary.useful += 1,
                "outdated" => summary.outdated += 1,
                "incorrect" => summary.incorrect += 1,
                "applies_here" => summary.applies_here += 1,
                "does_not_apply_here" => summary.does_not_apply_here += 1,
                _ => {} // unknown kind — counted in `total` only
            }
        }
        Ok(summary)
    }

    pub async fn delete_memory_hard(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let result = table
            .delete(&format!(
                "tenant = {} AND memory_id = {}",
                sql_quote(tenant),
                sql_quote(memory_id),
            ))
            .await
            .map_err(lancedb_err)?;
        if result.num_deleted_rows == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        // TODO: cascade-delete from embedding_jobs / memory_embeddings /
        // feedback_events / graph_edges once those tables exist on the
        // LanceDB side. The DuckDB backend handles this in
        // `DuckDbRepository::delete_memory_hard` (see ./duckdb/mod.rs).
        Ok(())
    }

    pub async fn get_memory(
        &self,
        memory_id: String,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        // Cross-tenant lookup (admin / version-chain path). DuckDB does the
        // same — filters only on memory_id.
        let filter = format!("memory_id = {}", sql_quote(&memory_id));
        Ok(self
            .query_memories(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    pub async fn insert_episode(
        &self,
        episode: EpisodeRecord,
    ) -> Result<EpisodeRecord, StorageError> {
        let _ = episode;
        unimplemented!("LanceDb::insert_episode — see docs/repository.rs trait def")
    }

    pub async fn list_memory_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        Ok(self
            .query_memories(format!("tenant = {}", sql_quote(tenant)), None)
            .await?
            .into_iter()
            .map(|m| m.memory_id)
            .collect())
    }

    pub async fn touch_session(
        &self,
        session_id: &str,
        last_seen_at: &str,
    ) -> Result<(), StorageError> {
        let _ = (session_id, last_seen_at);
        unimplemented!("LanceDb::touch_session — see docs/repository.rs trait def")
    }

    pub async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        let _ = (tenant, caller_agent);
        unimplemented!("LanceDb::latest_active_session — see docs/repository.rs trait def")
    }

    pub async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        let _ = (session_id, tenant, caller_agent, now);
        unimplemented!("LanceDb::open_session — see docs/repository.rs trait def")
    }

    pub async fn close_session(
        &self,
        session_id: &str,
        ended_at: &str,
    ) -> Result<(), StorageError> {
        let _ = (session_id, ended_at);
        unimplemented!("LanceDb::close_session — see docs/repository.rs trait def")
    }

    pub async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        let _ = tenant;
        unimplemented!(
            "LanceDb::list_successful_episodes_for_tenant — see docs/repository.rs trait def"
        )
    }

    pub async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        let mut filter = format!("tenant = {}", sql_quote(tenant));
        if let Some(s) = status_filter {
            filter.push_str(&format!(" AND status = {}", sql_quote(s)));
        }
        if let Some(m) = memory_id_filter {
            filter.push_str(&format!(" AND memory_id = {}", sql_quote(m)));
        }
        let mut rows = self.query_embedding_jobs(filter).await?;
        // ORDER BY updated_at DESC LIMIT n — sort then truncate.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let lim = limit.min(10_000);
        rows.truncate(lim);
        let out = rows
            .into_iter()
            .map(|r| EmbeddingJobInfo {
                job_id: r.job_id,
                tenant: r.tenant,
                memory_id: r.memory_id,
                target_content_hash: r.target_content_hash,
                provider: r.provider,
                status: r.status,
                attempt_count: u32::try_from(r.attempt_count).unwrap_or(u32::MAX),
                last_error: r.last_error,
                available_at: r.available_at,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect();
        Ok(out)
    }

    pub async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        // Pre-count, then UPDATE all matching live rows to status 'stale'.
        // LanceDB's UpdateResult.rows_updated is the canonical rowcount,
        // but we count first so we can return the same shape as DuckDB
        // even if the LanceDB update reports 0 (legacy server quirk —
        // matches the same defensive shape we use in delete_*).
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let filter = format!(
            "tenant = {} AND memory_id = {} AND provider = {} \
             AND (status = 'pending' OR status = 'processing')",
            sql_quote(tenant),
            sql_quote(memory_id),
            sql_quote(provider),
        );
        let count = table
            .count_rows(Some(filter.clone()))
            .await
            .map_err(lancedb_err)?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .update()
            .only_if(filter)
            .column("status", "'stale'")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.rows_updated).unwrap_or(count))
        }
    }

    pub async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        // No memory_embeddings table yet → no row by definition.
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "memory_embeddings") {
            return Ok(None);
        }
        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!("memory_id = {}", sql_quote(memory_id)))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            fn col<'a, T: 'static>(
                batch: &'a RecordBatch,
                name: &'static str,
            ) -> Result<&'a T, StorageError> {
                batch
                    .column_by_name(name)
                    .ok_or(StorageError::InvalidData("missing column"))?
                    .as_any()
                    .downcast_ref::<T>()
                    .ok_or(StorageError::InvalidData("column type mismatch"))
            }
            let model = col::<StringArray>(b, "embedding_model")?;
            let hash = col::<StringArray>(b, "content_hash")?;
            let updated = col::<StringArray>(b, "updated_at")?;
            return Ok(Some((
                model.value(0).to_string(),
                hash.value(0).to_string(),
                updated.value(0).to_string(),
            )));
        }
        Ok(None)
    }

    pub async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let mut rows = self
            .query_embedding_jobs(format!(
                "tenant = {} AND memory_id = {} AND target_content_hash = {}",
                sql_quote(tenant),
                sql_quote(memory_id),
                sql_quote(target_content_hash),
            ))
            .await?;
        // ORDER BY updated_at DESC LIMIT 1.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(rows.into_iter().next().map(|r| r.status))
    }
}
