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
use crate::domain::memory::{FeedbackSummary, MemoryRecord, MemoryVersionLink};
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
            .column("status", "'rejected'")
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
        // Walk the supersedes chain backwards: start from `memory_id`,
        // follow `supersedes_memory_id` to predecessors, and append each
        // record. Output ordered newest → oldest by version DESC.
        let mut chain: Vec<MemoryRecord> = Vec::new();
        let mut cursor = Some(memory_id.to_string());
        // Cap depth — the chain should be short (typically 1–3) and a
        // cycle would loop forever otherwise.
        for _ in 0..32 {
            let Some(id) = cursor.take() else { break };
            let rec = self
                .query_memories(
                    format!(
                        "tenant = {} AND memory_id = {}",
                        sql_quote(tenant),
                        sql_quote(&id)
                    ),
                    Some(1),
                )
                .await?
                .into_iter()
                .next();
            let Some(r) = rec else { break };
            cursor = r.supersedes_memory_id.clone();
            chain.push(r);
        }
        chain.sort_by(|a, b| b.version.cmp(&a.version));
        Ok(chain
            .into_iter()
            .map(|r| MemoryVersionLink {
                memory_id: r.memory_id,
                version: r.version,
                status: r.status,
                updated_at: r.updated_at,
                supersedes_memory_id: r.supersedes_memory_id,
            })
            .collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryStatus, MemoryType, Scope, Visibility};
    use crate::storage::types::EmbeddingJobInsert;
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
            summary: "round-trip test".into(),
            content: "use bun for fast installs".into(),
            evidence: vec!["src/main.rs:42".into(), "Cargo.toml:11".into()],
            code_refs: vec!["foo::bar()".into()],
            project: Some("mem".into()),
            repo: Some("mem".into()),
            module: None,
            task_type: None,
            tags: vec!["tooling".into()],
            topics: vec![],
            confidence: 0.7,
            decay_score: 0.0,
            content_hash: "h".repeat(64),
            idempotency_key: Some("idemp-1".into()),
            session_id: None,
            supersedes_memory_id: None,
            source_agent: "test".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
            last_validated_at: None,
        }
    }

    #[tokio::test]
    pub async fn lancedb_insert_and_get_memory_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.expect("open lancedb store");

        let memory = fixture("mem_lance_001", "tenant-a");
        repo.insert_memory(memory.clone())
            .await
            .expect("insert_memory");

        let got = repo
            .get_memory_for_tenant("tenant-a", "mem_lance_001")
            .await
            .expect("get_memory_for_tenant")
            .expect("memory should exist");

        assert_eq!(got.memory_id, memory.memory_id);
        assert_eq!(got.tenant, memory.tenant);
        assert_eq!(got.memory_type, memory.memory_type);
        assert_eq!(got.status, memory.status);
        assert_eq!(got.summary, memory.summary);
        assert_eq!(got.content, memory.content);
        assert_eq!(got.evidence, memory.evidence);
        assert_eq!(got.code_refs, memory.code_refs);
        assert_eq!(got.project, memory.project);
        assert_eq!(got.module, memory.module);
        assert_eq!(got.tags, memory.tags);
        assert_eq!(got.topics, memory.topics);
        assert_eq!(got.confidence, memory.confidence);
        assert_eq!(got.content_hash, memory.content_hash);
        assert_eq!(got.idempotency_key, memory.idempotency_key);
        assert_eq!(got.created_at, memory.created_at);
        assert_eq!(got.updated_at, memory.updated_at);
        assert_eq!(got.last_validated_at, memory.last_validated_at);

        let missing = repo
            .get_memory_for_tenant("tenant-a", "does-not-exist")
            .await
            .expect("missing query");
        assert!(missing.is_none());

        // Cross-tenant filter must not leak.
        let wrong_tenant = repo
            .get_memory_for_tenant("tenant-b", "mem_lance_001")
            .await
            .expect("cross-tenant query");
        assert!(wrong_tenant.is_none());
    }

    /// Exercises the batch-impl filter methods (`list_memories_for_tenant`,
    /// `list_memory_ids_for_tenant`, `find_by_idempotency_or_hash`,
    /// `search_candidates`, `recent_active_memories`,
    /// `fetch_memories_by_ids`, `list_pending_review`, `get_pending`,
    /// `get_memory`).
    #[tokio::test]
    pub async fn lancedb_filter_methods_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut a1 = fixture("mem_a_001", "tenant-a");
        a1.idempotency_key = Some("idem-a-1".into());
        let mut a2 = fixture("mem_a_002", "tenant-a");
        a2.status = MemoryStatus::PendingConfirmation;
        a2.content_hash = "h2".repeat(32);
        let mut a3 = fixture("mem_a_003", "tenant-a");
        a3.status = MemoryStatus::Archived;
        let b1 = fixture("mem_b_001", "tenant-b");

        for m in [&a1, &a2, &a3, &b1] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // list_memories_for_tenant
        let a_all = repo.list_memories_for_tenant("tenant-a").await.unwrap();
        assert_eq!(a_all.len(), 3);
        let b_all = repo.list_memories_for_tenant("tenant-b").await.unwrap();
        assert_eq!(b_all.len(), 1);

        // list_memory_ids_for_tenant
        let mut ids_a = repo.list_memory_ids_for_tenant("tenant-a").await.unwrap();
        ids_a.sort();
        assert_eq!(ids_a, vec!["mem_a_001", "mem_a_002", "mem_a_003"]);

        // find_by_idempotency_or_hash — match via idempotency_key
        let by_idem = repo
            .find_by_idempotency_or_hash("tenant-a", &Some("idem-a-1".into()), "no-such-hash")
            .await
            .unwrap();
        assert_eq!(by_idem.unwrap().memory_id, "mem_a_001");

        // ... match via content_hash when no idempotency_key supplied
        let by_hash = repo
            .find_by_idempotency_or_hash("tenant-a", &None, &a2.content_hash)
            .await
            .unwrap();
        assert_eq!(by_hash.unwrap().memory_id, "mem_a_002");

        // search_candidates — drops `archived`
        let cands = repo.search_candidates("tenant-a").await.unwrap();
        let mut cand_ids: Vec<_> = cands.iter().map(|m| m.memory_id.clone()).collect();
        cand_ids.sort();
        assert_eq!(cand_ids, vec!["mem_a_001", "mem_a_002"]);

        // recent_active_memories — same filter, with limit
        let recent = repo.recent_active_memories("tenant-a", 1).await.unwrap();
        assert_eq!(recent.len(), 1);

        // fetch_memories_by_ids — IN clause
        let by_ids = repo
            .fetch_memories_by_ids("tenant-a", &["mem_a_001", "mem_a_002"])
            .await
            .unwrap();
        assert_eq!(by_ids.len(), 2);
        // Empty input — short-circuit, no query.
        assert!(repo
            .fetch_memories_by_ids("tenant-a", &[])
            .await
            .unwrap()
            .is_empty());

        // list_pending_review
        let pending = repo.list_pending_review("tenant-a").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].memory_id, "mem_a_002");

        // get_pending — exact one
        let p = repo.get_pending("tenant-a", "mem_a_002").await.unwrap();
        assert_eq!(p.unwrap().memory_id, "mem_a_002");
        // get_pending — wrong status returns None
        let np = repo.get_pending("tenant-a", "mem_a_001").await.unwrap();
        assert!(np.is_none());

        // get_memory — cross-tenant (no tenant filter)
        let cross = repo.get_memory("mem_b_001".into()).await.unwrap();
        assert_eq!(cross.unwrap().tenant, "tenant-b");
    }

    /// Mutating-method round-trip: accept_pending, reject_pending,
    /// replace_pending_with_successor, delete_memory_hard.
    #[tokio::test]
    pub async fn lancedb_mutating_methods_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut p = fixture("mem_p", "tenant");
        p.status = MemoryStatus::PendingConfirmation;
        let mut q = fixture("mem_q", "tenant");
        q.status = MemoryStatus::PendingConfirmation;
        let r = fixture("mem_r", "tenant");
        let s = fixture("mem_s", "tenant");
        for m in [&p, &q, &r, &s] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // accept_pending → status active
        let accepted = repo.accept_pending("tenant", "mem_p").await.unwrap();
        assert_eq!(accepted.status, MemoryStatus::Active);
        assert_eq!(accepted.memory_id, "mem_p");

        // reject_pending → status rejected
        let rejected = repo.reject_pending("tenant", "mem_q").await.unwrap();
        assert_eq!(rejected.status, MemoryStatus::Rejected);

        // After accept/reject, list_pending_review is empty
        let pending = repo.list_pending_review("tenant").await.unwrap();
        assert!(pending.is_empty());

        // replace_pending_with_successor: archive r, insert successor
        let mut succ = fixture("mem_r_v2", "tenant");
        succ.supersedes_memory_id = Some("mem_r".into());
        succ.version = 2;
        let returned = repo
            .replace_pending_with_successor("tenant", "mem_r", succ.clone())
            .await
            .unwrap();
        assert_eq!(returned.memory_id, "mem_r_v2");
        let archived = repo
            .get_memory_for_tenant("tenant", "mem_r")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.status, MemoryStatus::Rejected);
        let successor_row = repo
            .get_memory_for_tenant("tenant", "mem_r_v2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(successor_row.supersedes_memory_id, Some("mem_r".into()));
        assert_eq!(successor_row.version, 2);

        // delete_memory_hard
        repo.delete_memory_hard("tenant", "mem_s").await.unwrap();
        let gone = repo.get_memory_for_tenant("tenant", "mem_s").await.unwrap();
        assert!(gone.is_none());

        // delete on non-existent → NotFound-equivalent error
        let err = repo
            .delete_memory_hard("tenant", "does-not-exist")
            .await
            .unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidData("memory not found")),
            "expected NotFound-equivalent, got {err:?}",
        );
    }

    #[tokio::test]
    pub async fn lancedb_feedback_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let memory = fixture("mem_fb", "tenant");
        repo.insert_memory(memory.clone()).await.unwrap();

        // Apply 3 feedbacks of different kinds
        let make = |kind: &str, ts: &str, suffix: &str| FeedbackEvent {
            feedback_id: format!("fb_{suffix}"),
            memory_id: memory.memory_id.clone(),
            feedback_kind: kind.into(),
            created_at: ts.into(),
        };
        let _ = repo
            .apply_feedback(&memory, make("useful", "2026-05-08T01:00:00Z", "1"))
            .await
            .unwrap();
        let after_useful = repo
            .get_memory_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert!(
            after_useful.confidence > memory.confidence,
            "useful must increase confidence: {} vs {}",
            after_useful.confidence,
            memory.confidence,
        );
        assert!(after_useful.last_validated_at.is_some());

        let _ = repo
            .apply_feedback(&after_useful, make("outdated", "2026-05-08T02:00:00Z", "2"))
            .await
            .unwrap();
        let after_outdated = repo
            .get_memory_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert!(
            after_outdated.decay_score > after_useful.decay_score,
            "outdated must increase decay",
        );

        let _ = repo
            .apply_feedback(
                &after_outdated,
                make("incorrect", "2026-05-08T03:00:00Z", "3"),
            )
            .await
            .unwrap();
        let after_incorrect = repo
            .get_memory_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_incorrect.status,
            MemoryStatus::Archived,
            "incorrect must archive",
        );

        // list_feedback_for_memory — sorted ASC by created_at
        let events = repo.list_feedback_for_memory("mem_fb").await.unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.feedback_kind.as_str()).collect();
        assert_eq!(kinds, vec!["useful", "outdated", "incorrect"]);

        // feedback_summary — counts per kind
        let summary = repo.feedback_summary("mem_fb").await.unwrap();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.useful, 1);
        assert_eq!(summary.outdated, 1);
        assert_eq!(summary.incorrect, 1);
        assert_eq!(summary.applies_here, 0);
        assert_eq!(summary.does_not_apply_here, 0);

        // Empty feedback for a memory with none
        let summary_none = repo.feedback_summary("never-feedback'd").await.unwrap();
        assert_eq!(summary_none.total, 0);
    }

    /// `upsert_memory_embedding` + `semantic_search_memories` round-trip:
    /// insert two memories, write their embeddings, search by a query
    /// vector, expect both back in cosine-distance order with the closer
    /// vector ranked first. Also exercises tenant prefilter and
    /// `delete_memory_embedding`.
    #[tokio::test(flavor = "multi_thread")]
    pub async fn lancedb_embedding_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        // Two memories under "tenant-a", one under "tenant-b" (cross-tenant
        // leak test).
        let a1 = fixture("mem_emb_1", "tenant-a");
        let a2 = fixture("mem_emb_2", "tenant-a");
        let b1 = fixture("mem_emb_3", "tenant-b");
        for m in [&a1, &a2, &b1] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // Hand-rolled 4-d unit vectors. q ≈ v1 (close), v2 different,
        // v3 belongs to tenant-b and must not appear in tenant-a search.
        fn to_blob(v: &[f32]) -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 4);
            for f in v {
                out.extend_from_slice(&f.to_ne_bytes());
            }
            out
        }
        let v1 = vec![1.0_f32, 0.0, 0.0, 0.0];
        let v2 = vec![0.0_f32, 1.0, 0.0, 0.0];
        let v3 = vec![0.0_f32, 0.0, 1.0, 0.0];
        repo.upsert_memory_embedding(
            "mem_emb_1",
            "tenant-a",
            "fake-test",
            4,
            &to_blob(&v1),
            "h1",
            "00000001778000000000",
            "00000001778000000000",
        )
        .await
        .unwrap();
        repo.upsert_memory_embedding(
            "mem_emb_2",
            "tenant-a",
            "fake-test",
            4,
            &to_blob(&v2),
            "h2",
            "00000001778000000000",
            "00000001778000000000",
        )
        .await
        .unwrap();
        repo.upsert_memory_embedding(
            "mem_emb_3",
            "tenant-b",
            "fake-test",
            4,
            &to_blob(&v3),
            "h3",
            "00000001778000000000",
            "00000001778000000000",
        )
        .await
        .unwrap();

        // Query close to v1 → mem_emb_1 should rank first; mem_emb_3
        // (tenant-b) must be filtered out.
        let q = vec![0.99_f32, 0.14, 0.0, 0.0]; // ≈ unit, close to v1
        let hits = repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "tenant-a should have 2 hits, got {hits:?}");
        assert_eq!(hits[0].0.memory_id, "mem_emb_1", "v1 should rank first");
        assert_eq!(hits[1].0.memory_id, "mem_emb_2");
        // similarity ∈ (0, 1] for close-but-not-identical normalized vecs;
        // strictly greater than the v2 score.
        assert!(hits[0].1 > hits[1].1);

        // Upsert overwrite: re-write mem_emb_1 with v2 — now query close
        // to v1 should rank mem_emb_2 first (because both rows now have
        // v2-like vectors, but mem_emb_1 will be slightly off due to
        // float roundtrip, so we just check the row count stays at 2).
        repo.upsert_memory_embedding(
            "mem_emb_1",
            "tenant-a",
            "fake-test",
            4,
            &to_blob(&v2),
            "h1b",
            "00000001778000000001",
            "00000001778000000001",
        )
        .await
        .unwrap();
        let after_overwrite = repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(after_overwrite.len(), 2);

        // delete_memory_embedding removes the row from the search corpus.
        repo.delete_memory_embedding("mem_emb_2").await.unwrap();
        let after_delete = repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(after_delete.len(), 1);
        assert_eq!(after_delete[0].0.memory_id, "mem_emb_1");

        // delete on no-row is a no-op (table exists but no matching row).
        repo.delete_memory_embedding("does-not-exist")
            .await
            .unwrap();

        // Search before any upsert (fresh repo, no memory_embeddings
        // table) returns empty without error.
        let dir2 = tempdir().unwrap();
        let path2 = dir2.path().join("empty.store");
        let empty_repo = LanceStore::open(&path2).await.unwrap();
        let empty_hits = empty_repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert!(empty_hits.is_empty());
        // And delete on a missing table is a no-op.
        empty_repo
            .delete_memory_embedding("anything")
            .await
            .unwrap();
    }

    /// embedding_jobs queue end-to-end:
    /// enqueue (idempotent) → claim → complete; reschedule → re-claim;
    /// permanently_fail; mark_stale; list/filter; stale_live;
    /// delete_by_memory_id; latest_status_for_hash.
    #[tokio::test]
    pub async fn lancedb_embedding_jobs_queue_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let m1 = fixture("mem_q1", "tenant-a");
        let m2 = fixture("mem_q2", "tenant-a");
        for m in [&m1, &m2] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // Enqueue: first call creates, second is idempotent (dup detected).
        let insert1 = EmbeddingJobInsert {
            job_id: "job_1".into(),
            tenant: "tenant-a".into(),
            memory_id: "mem_q1".into(),
            target_content_hash: "hash_q1".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000000000".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
        };
        let enq1 = repo
            .try_enqueue_embedding_job(insert1.clone())
            .await
            .unwrap();
        assert!(enq1, "first enqueue should create");
        let enq1b = repo.try_enqueue_embedding_job(insert1).await.unwrap();
        assert!(!enq1b, "duplicate enqueue must return false");

        let first = repo
            .first_embedding_job_id_for_memory("mem_q1")
            .await
            .unwrap();
        assert_eq!(first, Some("job_1".into()));
        let none = repo
            .first_embedding_job_id_for_memory("does-not-exist")
            .await
            .unwrap();
        assert!(none.is_none());

        let status = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(status.as_deref(), Some("pending"));

        // Add a second job (different memory) so claim ordering is testable.
        let insert2 = EmbeddingJobInsert {
            job_id: "job_2".into(),
            tenant: "tenant-a".into(),
            memory_id: "mem_q2".into(),
            target_content_hash: "hash_q2".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000000001".into(),
            created_at: "00000001778000000001".into(),
            updated_at: "00000001778000000001".into(),
        };
        repo.try_enqueue_embedding_job(insert2).await.unwrap();

        // Claim 5: only 2 available; ordered by available_at ASC then
        // created_at ASC (job_1 first because earlier available_at).
        let now = "00000001778000010000";
        let claimed = repo.claim_next_n_embedding_jobs(now, 5, 5).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].job_id, "job_1");
        assert_eq!(claimed[1].job_id, "job_2");
        assert_eq!(claimed[0].attempt_count, 0);

        // After claim, both rows are 'processing'.
        let status_after = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(status_after.as_deref(), Some("processing"));

        // Re-claim returns nothing.
        let recl = repo.claim_next_n_embedding_jobs(now, 5, 5).await.unwrap();
        assert!(recl.is_empty());

        repo.complete_embedding_job("job_1", "00000001778000020000")
            .await
            .unwrap();
        let s1 = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(s1.as_deref(), Some("completed"));

        repo.reschedule_embedding_job_failure(
            "job_2",
            1,
            "transient",
            "00000001778000040000",
            "00000001778000030000",
        )
        .await
        .unwrap();
        let s2 = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q2", "hash_q2")
            .await
            .unwrap();
        assert_eq!(s2.as_deref(), Some("failed"));

        // Re-claim with budget=2 should pick job_2 again (failed,
        // attempt_count < max_retries, available_at <= now).
        let now2 = "00000001778000050000";
        let recl2 = repo.claim_next_n_embedding_jobs(now2, 2, 5).await.unwrap();
        assert_eq!(recl2.len(), 1);
        assert_eq!(recl2[0].job_id, "job_2");
        assert_eq!(recl2[0].attempt_count, 1);

        // Permanently fail it (attempt_count beyond budget).
        repo.permanently_fail_embedding_job("job_2", 5, "boom", "00000001778000060000")
            .await
            .unwrap();
        let recl3 = repo.claim_next_n_embedding_jobs(now2, 2, 5).await.unwrap();
        // Failed but attempt_count (5) >= max_retries (2) → not eligible.
        assert!(recl3.is_empty());

        repo.mark_embedding_job_stale("job_1", "00000001778000070000")
            .await
            .unwrap();
        let s_stale = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(s_stale.as_deref(), Some("stale"));

        // list_embedding_jobs: tenant filter.
        let all = repo
            .list_embedding_jobs("tenant-a", None, None, 50)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // status filter.
        let only_failed = repo
            .list_embedding_jobs("tenant-a", Some("failed"), None, 50)
            .await
            .unwrap();
        assert_eq!(only_failed.len(), 1);
        assert_eq!(only_failed[0].job_id, "job_2");
        assert_eq!(only_failed[0].attempt_count, 5);

        // memory_id filter.
        let only_q1 = repo
            .list_embedding_jobs("tenant-a", None, Some("mem_q1"), 50)
            .await
            .unwrap();
        assert_eq!(only_q1.len(), 1);
        assert_eq!(only_q1[0].memory_id, "mem_q1");

        // stale_live: enqueue a fresh pending row, then sweep it stale.
        let insert3 = EmbeddingJobInsert {
            job_id: "job_3".into(),
            tenant: "tenant-a".into(),
            memory_id: "mem_q1".into(),
            target_content_hash: "hash_q1_v2".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000080000".into(),
            created_at: "00000001778000080000".into(),
            updated_at: "00000001778000080000".into(),
        };
        repo.try_enqueue_embedding_job(insert3).await.unwrap();
        let staled = repo
            .stale_live_embedding_jobs_for_memory(
                "tenant-a",
                "mem_q1",
                "fake-test",
                "00000001778000090000",
            )
            .await
            .unwrap();
        assert_eq!(staled, 1);

        let deleted = repo
            .delete_embedding_jobs_by_memory_id("mem_q1")
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        let remaining = repo
            .list_embedding_jobs("tenant-a", None, None, 50)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].memory_id, "mem_q2");

        // delete on no-row → 0.
        let zero = repo
            .delete_embedding_jobs_by_memory_id("nope")
            .await
            .unwrap();
        assert_eq!(zero, 0);
    }

    /// `bm25_candidates` lazy-creates the FTS index on `memories.content`
    /// the first time it's called, then BM25-ranks rows matching the
    /// query — distinct from semantic_search_memories (vector ANN).
    /// Tenant filter must be honored; empty query / k == 0 returns [].
    #[tokio::test]
    pub async fn lancedb_bm25_candidates_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut a = fixture("mem_b1", "tenant-a");
        a.content = "DuckDB single mutex serializes all writes".into();
        let mut b = fixture("mem_b2", "tenant-a");
        b.content = "LanceDB native vector search uses ANN".into();
        let mut c = fixture("mem_b3", "tenant-a");
        c.content = "Tantivy provides BM25 in DuckDB build".into();
        let mut d = fixture("mem_b4", "tenant-b");
        d.content = "DuckDB connection pool tenant-b".into();
        for m in [&a, &b, &c, &d] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // Empty query → []; k=0 → [].
        let none1 = repo.bm25_candidates("tenant-a", "", 10).await.unwrap();
        assert!(none1.is_empty());
        let none2 = repo.bm25_candidates("tenant-a", "DuckDB", 0).await.unwrap();
        assert!(none2.is_empty());

        // Real query: 'DuckDB' should match mem_b1 + mem_b3 (tenant-a)
        // but NOT mem_b4 (tenant-b filter).
        let hits = repo
            .bm25_candidates("tenant-a", "DuckDB", 10)
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(ids.contains(&"mem_b1"), "got {ids:?}");
        assert!(ids.contains(&"mem_b3"), "got {ids:?}");
        assert!(!ids.contains(&"mem_b2"));
        assert!(
            !ids.contains(&"mem_b4"),
            "tenant filter must exclude tenant-b"
        );

        // Index now exists — second call should reuse, not rebuild.
        let table = repo.conn.open_table("memories").execute().await.unwrap();
        let indices = table.list_indices().await.unwrap();
        assert!(
            indices
                .iter()
                .any(|c| c.columns.iter().any(|col| col == "content")),
            "FTS index should exist on content column after first call",
        );

        // Different query, same tenant.
        let lance_hits = repo
            .bm25_candidates("tenant-a", "LanceDB", 10)
            .await
            .unwrap();
        let lance_ids: Vec<&str> = lance_hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(lance_ids.contains(&"mem_b2"));
    }
}
