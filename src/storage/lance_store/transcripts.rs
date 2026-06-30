//! Transcript pipeline (parallel to memories): conversation_messages
//! reads/writes, transcript_embedding_jobs queue, and
//! conversation_message_embeddings upsert/delete. All inherent on
//! LanceStore.

use arrow_array::{Float32Array, RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    conversation_message_embedding_to_record_batch, conversation_messages_to_record_batch,
    ensure_conversation_message_embeddings_table, lancedb_err, parse_col,
    record_batch_to_conversation_messages, record_batch_to_transcript_embedding_job_rows,
    sql_quote, transcript_embedding_job_row_to_record_batch,
    transcript_embedding_job_rows_to_record_batch, LanceStore, TranscriptEmbeddingJobRow,
};
use crate::domain::ConversationMessage;
use crate::embedding::wire::decode_f32_blob;
use crate::storage::types::{
    ClaimedTranscriptEmbeddingJob, ContextWindow, StorageError, TranscriptSessionSummary,
};
use crate::storage::{timestamp_sub_ms, EMBEDDING_JOB_LEASE_MS};

/// `query_transcript_embedding_jobs` was a helper inside the
/// `update_status / query_capability_capsules / query_embedding_jobs` impl block
/// in mod.rs; pulled here next to its callers.
impl LanceStore {
    pub(crate) async fn query_transcript_embedding_jobs(
        &self,
        filter: String,
    ) -> Result<Vec<TranscriptEmbeddingJobRow>, StorageError> {
        let table = self
            .conn
            .open_table("transcript_embedding_jobs")
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
            out.extend(record_batch_to_transcript_embedding_job_rows(b)?);
        }
        Ok(out)
    }

    /// Route-B native equivalent of
    /// `DuckDbQuery::get_transcript_embedding_job_status`: read the
    /// `status` column of a `transcript_embedding_jobs` row by `job_id`,
    /// or `None` when the row is gone. Same shape as the memories-side
    /// `LanceStore::get_embedding_job_status`.
    pub async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let rows = self
            .query_transcript_embedding_jobs(format!("job_id = {}", sql_quote(job_id)))
            .await?;
        Ok(rows.into_iter().next().map(|r| r.status))
    }
}

/// Transcript embedding queue methods. Mirror the memories-side
/// queue (`try_enqueue_embedding_job` etc.) with `capability_capsule_id` →
/// `message_block_id` and `target_content_hash` dropped (transcript
/// blocks are immutable). All inherent on `LanceStore` — they're
/// not part of the trait surface (which never abstracted the
/// transcript queue).
impl LanceStore {
    /// Enqueue a `pending` row in `transcript_embedding_jobs`.
    /// Internal: `create_conversation_message` calls this when
    /// `embed_eligible == true`. No idempotency check — the
    /// underlying `conversation_messages` insert is itself
    /// idempotent on (transcript_path, line_number, block_index)
    /// and only enqueues on a fresh insert, so duplicate jobs can't
    /// be produced from this code path.
    pub async fn try_enqueue_transcript_embedding_job(
        &self,
        job_id: String,
        tenant: String,
        message_block_id: String,
        provider: String,
        now: String,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("transcript_embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let row = TranscriptEmbeddingJobRow {
            job_id,
            tenant,
            message_block_id,
            provider,
            status: "pending".to_string(),
            attempt_count: 0,
            last_error: None,
            available_at: now.clone(),
            created_at: now.clone(),
            updated_at: now,
        };
        let batch = transcript_embedding_job_row_to_record_batch(&row)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(())
    }

    /// Block-ids in `block_ids` that already have a `transcript_embedding_jobs`
    /// row, in ANY state. "Has a job row" ⟺ "was enqueued at least once":
    /// completed jobs are kept (the worker flips status to `completed`, the row
    /// is never deleted or vacuum-pruned), so this is the idempotency signal that
    /// lets the write path repair an enqueue that failed AFTER the message row
    /// committed — without ever re-enqueuing a block that was already queued
    /// (even one whose embedding has since completed).
    async fn transcript_jobs_present_for_blocks(
        &self,
        block_ids: &[&str],
    ) -> Result<std::collections::HashSet<String>, StorageError> {
        if block_ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let in_list = block_ids
            .iter()
            .map(|b| sql_quote(b))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = self
            .query_transcript_embedding_jobs(format!("message_block_id IN ({in_list})"))
            .await?;
        Ok(rows.into_iter().map(|r| r.message_block_id).collect())
    }

    /// Whether a `conversation_messages` row exists with this exact
    /// `message_block_id`. The insert-dedup key is `(transcript_path,
    /// line_number, block_index)`, which is NOT the block id — so an exists
    /// probe by key can match a *different* block. The orphan-repair path uses
    /// this to enqueue only for blocks that truly own a row, never for a message
    /// that was deduplicated away by key.
    async fn conversation_message_block_exists(
        &self,
        block_id: &str,
    ) -> Result<bool, StorageError> {
        let table = self
            .conn
            .open_table("conversation_messages")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let n = table
            .count_rows(Some(format!("message_block_id = {}", sql_quote(block_id))))
            .await
            .map_err(lancedb_err)?;
        Ok(n > 0)
    }

    /// Mirror of `claim_next_n_embedding_jobs` for the transcript
    /// queue. Eligible rows are `pending` or `failed` with
    /// `attempt_count < max_retries`, ordered `(available_at,
    /// created_at) ASC`. Each successful claim flips status to
    /// `processing` via optimistic UPDATE (skip if a racer beat us).
    pub async fn claim_next_n_transcript_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedTranscriptEmbeddingJob>, StorageError> {
        if n == 0 {
            return Ok(vec![]);
        }
        // The `processing AND updated_at <= now - lease` disjunct reclaims
        // orphaned in-flight jobs (worker crash / restart mid-embed /
        // mid-batch error) — mirrors the capsule queue's lease-reclaim. See
        // `claim_next_n_embedding_jobs` for the rationale.
        let max_r = i64::from(max_retries);
        let lease_cutoff = timestamp_sub_ms(now, EMBEDDING_JOB_LEASE_MS);
        let filter = format!(
            "available_at <= {now} AND (status = 'pending' \
             OR (status = 'failed' AND attempt_count < {max_r}) \
             OR (status = 'processing' AND updated_at <= {cutoff}))",
            now = sql_quote(now),
            cutoff = sql_quote(&lease_cutoff),
        );
        let mut rows = self.query_transcript_embedding_jobs(filter).await?;
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
            .open_table("transcript_embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut claimed = Vec::with_capacity(rows.len());
        for r in rows {
            let result = table
                .update()
                .only_if(format!(
                    "job_id = {job} AND (status = 'pending' \
                     OR (status = 'failed' AND attempt_count < {max_r}) \
                     OR (status = 'processing' AND updated_at <= {cutoff}))",
                    job = sql_quote(&r.job_id),
                    cutoff = sql_quote(&lease_cutoff),
                ))
                .column("status", "'processing'")
                .column("updated_at", sql_quote(now))
                .execute()
                .await
                .map_err(lancedb_err)?;
            if result.rows_updated == 0 {
                continue;
            }
            claimed.push(ClaimedTranscriptEmbeddingJob {
                job_id: r.job_id,
                tenant: r.tenant,
                message_block_id: r.message_block_id,
                provider: r.provider,
                attempt_count: r.attempt_count,
            });
        }
        Ok(claimed)
    }

    pub async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("transcript_embedding_jobs")
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

    pub async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("transcript_embedding_jobs")
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

    pub async fn reschedule_transcript_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("transcript_embedding_jobs")
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

    pub async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("transcript_embedding_jobs")
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

    /// Upsert a transcript-block embedding into
    /// `conversation_message_embeddings`. Mirrors
    /// `MemoryRepository::upsert_capability_capsule_embedding` 1:1 with
    /// `capability_capsule_id` → `message_block_id`. Lazy-creates the table on
    /// first call (dim is provider-dependent and not known at
    /// `LanceStore::open` time without a provider).
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
        let dim_i32 = i32::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim does not fit in i32"))?;
        let vector = decode_f32_blob(embedding_blob, embedding_dim as usize)
            .map_err(StorageError::InvalidData)?;

        ensure_conversation_message_embeddings_table(&self.conn, dim_i32).await?;
        let table = self
            .conn
            .open_table("conversation_message_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // upsert = delete-then-insert (Lance has no PK).
        table
            .delete(&format!(
                "message_block_id = {}",
                sql_quote(message_block_id),
            ))
            .await
            .map_err(lancedb_err)?;
        let batch = conversation_message_embedding_to_record_batch(
            message_block_id,
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

    /// ③ chunked: delete all existing embedding rows for the message
    /// once, then insert one row per chunk vector. Vectors share
    /// `message_block_id`; `semantic_search_transcripts` dedups them
    /// via GROUP BY. Takes raw `Vec<f32>` (no blob decode — the worker
    /// has the vectors already). Empty `vectors` is a no-op (leaves the
    /// message with no embedding rows). Mirrors
    /// `MemoryRepository::upsert_capability_capsule_embedding_chunks`.
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
        if vectors.is_empty() {
            return Ok(());
        }
        let dim_i32 = i32::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim does not fit in i32"))?;
        ensure_conversation_message_embeddings_table(&self.conn, dim_i32).await?;
        let table = self
            .conn
            .open_table("conversation_message_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // upsert = delete-then-insert once (Lance has no PK), then ONE add.
        table
            .delete(&format!(
                "message_block_id = {}",
                sql_quote(message_block_id),
            ))
            .await
            .map_err(lancedb_err)?;
        // Build every chunk row up front and add them in a SINGLE commit.
        // Per-chunk `table.add` wrote one Lance fragment per chunk — for a
        // chunked message that's N fragments per upsert, feeding the fragment
        // explosion the vacuum worker then has to compact back. One add =
        // one fragment regardless of chunk count.
        let mut batches = Vec::with_capacity(vectors.len());
        for vector in vectors {
            batches.push(conversation_message_embedding_to_record_batch(
                message_block_id,
                tenant,
                embedding_model,
                embedding_dim,
                vector,
                content_hash,
                source_updated_at,
                now,
            )?);
        }
        if !batches.is_empty() {
            // lancedb's `Scannable` is implemented for `Vec<RecordBatch>`, so
            // one `add` over all chunk batches commits a single fragment.
            table.add(batches).execute().await.map_err(lancedb_err)?;
        }
        Ok(())
    }

    /// Delete a transcript-block embedding by `message_block_id`.
    /// No-op if the lazy-created table doesn't exist yet.
    pub async fn delete_conversation_message_embedding(
        &self,
        message_block_id: &str,
    ) -> Result<(), StorageError> {
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "conversation_message_embeddings") {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("conversation_message_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!(
                "message_block_id = {}",
                sql_quote(message_block_id),
            ))
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }
}

/// Read all `conversation_messages` rows matching `filter`, parsed into
/// [`ConversationMessage`]s. Shared by every transcript read path.
impl LanceStore {
    pub async fn query_conversation_messages(
        &self,
        filter: String,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let table = self
            .conn
            .open_table("conversation_messages")
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
            out.extend(record_batch_to_conversation_messages(b)?);
        }
        Ok(out)
    }

    /// Route-B bucket "transcript fetch-by-ids": native lancedb-Rust
    /// equivalent of `DuckDbQuery::fetch_conversation_messages_by_ids` — bulk
    /// fetch by `message_block_id` list, scoped to `tenant`.
    ///
    /// **Order parity**: the DuckDB version returns rows in **input-slice
    /// order**, with missing ids silently dropped (post-search hydration
    /// tolerates rows that disappeared between search and fetch). A lance
    /// `only_if … IN (…)` scan returns table order, so we re-order the scanned
    /// rows back to the input-id order in Rust via a HashMap (exactly as the
    /// DuckDB impl does), then drop any id that didn't come back. Empty `ids`
    /// short-circuits to `Ok(vec![])` (mirrors DuckDB).
    ///
    /// Parity-exercised by `tests/storage_fetch_by_ids.rs`.
    pub async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_list = ids
            .iter()
            .map(|id| sql_quote(id))
            .collect::<Vec<_>>()
            .join(", ");
        let rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND message_block_id IN ({id_list})",
                sql_quote(tenant),
            ))
            .await?;
        // Re-order the table-order scan back to input-id order, dropping any
        // id that didn't come back (wrong tenant / vanished) — mirrors the
        // DuckDB impl's HashMap reshape so callers see input-slice order.
        let mut by_id: std::collections::HashMap<String, ConversationMessage> =
            std::collections::HashMap::with_capacity(rows.len());
        for m in rows {
            by_id.insert(m.message_block_id.clone(), m);
        }
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(m) = by_id.remove(id) {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// Route-B native equivalent of `DuckDbQuery::list_transcript_sessions`:
    /// per-session aggregate over `conversation_messages` for `tenant`.
    ///
    /// DuckDB does this with one `GROUP BY session_id` —
    /// `count(*)`, `min(created_at)`, `max(created_at)`,
    /// `max(caller_agent)`, filtered `tenant = ? AND session_id IS NOT
    /// NULL`, ordered `last_at DESC`. LanceDB has no GROUP BY, so we scan
    /// the rows and aggregate in Rust (the `capsule_stats` / `graph_stats`
    /// pattern). `max(caller_agent)` is the lexicographic max of the
    /// string column (DuckDB semantics). The `ORDER BY last_at DESC` is
    /// reproduced with a stable secondary `session_id ASC` tie-break so
    /// the output is deterministic (DuckDB's bare ORDER BY would be
    /// arbitrary on a `last_at` tie).
    pub async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id IS NOT NULL",
                sql_quote(tenant),
            ))
            .await?;

        // session_id → (block_count, first_at, last_at, max_caller_agent)
        let mut groups: std::collections::HashMap<String, (i64, String, String, String)> =
            std::collections::HashMap::new();
        for m in rows {
            let Some(session_id) = m.session_id else {
                continue; // belt-and-braces: the filter already drops these
            };
            let caller = m.caller_agent;
            let created = m.created_at;
            groups
                .entry(session_id)
                .and_modify(|(count, first_at, last_at, caller_max)| {
                    *count += 1;
                    if created < *first_at {
                        *first_at = created.clone();
                    }
                    if created > *last_at {
                        *last_at = created.clone();
                    }
                    if caller > *caller_max {
                        *caller_max = caller.clone();
                    }
                })
                .or_insert_with(|| (1, created.clone(), created.clone(), caller.clone()));
        }

        let mut out: Vec<TranscriptSessionSummary> = groups
            .into_iter()
            .map(
                |(session_id, (block_count, first_at, last_at, caller_agent))| {
                    TranscriptSessionSummary {
                        session_id,
                        block_count,
                        first_at,
                        last_at,
                        // DuckDB stores caller_agent NOT NULL, so `max()` is
                        // always Some here; the type is Option for the legacy
                        // `row.get(4).ok()` shape.
                        caller_agent: Some(caller_agent),
                    }
                },
            )
            .collect();
        // last_at DESC, then session_id ASC (deterministic tie-break).
        out.sort_by(|a, b| {
            b.last_at
                .cmp(&a.last_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        Ok(out)
    }

    /// Route-B native equivalent of
    /// `DuckDbQuery::recent_conversation_messages`: most-recent
    /// embed_eligible blocks for `tenant`, newest first
    /// (`created_at DESC, line_number DESC, block_index DESC`), capped at
    /// `limit` (clamped to `1..=1024`, matching the DuckDB impl). LanceDB
    /// has no ORDER BY / LIMIT-with-sort, so we scan then sort + slice in
    /// Rust to reproduce the DuckDB tie-break exactly.
    pub async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024) as usize;
        let mut rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND embed_eligible = true",
                sql_quote(tenant),
            ))
            .await?;
        // created_at DESC, line_number DESC, block_index DESC.
        rows.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.line_number.cmp(&a.line_number))
                .then_with(|| b.block_index.cmp(&a.block_index))
        });
        rows.truncate(lim);
        Ok(rows)
    }

    /// Route-B native equivalent of
    /// `DuckDbQuery::get_conversation_messages_by_session`: all conversation
    /// blocks for `(tenant, session_id)`, ordered chronologically
    /// `(created_at ASC, line_number ASC, block_index ASC)`. LanceDB has no
    /// `ORDER BY`, so the sort runs in Rust to reproduce the DuckDB tie-break.
    pub async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let mut rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {}",
                sql_quote(tenant),
                sql_quote(session_id),
            ))
            .await?;
        rows.sort_by(chrono_asc);
        Ok(rows)
    }

    /// Route-B native equivalent of
    /// `DuckDbQuery::get_conversation_messages_by_session_paged`: paginated
    /// per-session scroll. `since` / `until` apply to `created_at` (inclusive
    /// lower, exclusive upper); `role` / `block_type` are optional equality
    /// filters; the composite cursor `(created_at, line_number, block_index)`
    /// resumes strictly after the last row seen, all under the chronological
    /// `(created_at, line_number, block_index)` ASC ordering. `has_more` via
    /// the N+1 trick. LanceDB has no `ORDER BY` / `LIMIT`-with-sort, so the
    /// cursor + ordering + slice run in Rust.
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
        let lim = i64::try_from(limit).unwrap_or(64);
        let mut clauses = vec![
            format!("tenant = {}", sql_quote(tenant)),
            format!("session_id = {}", sql_quote(session_id)),
        ];
        if let Some(s) = since {
            clauses.push(format!("created_at >= {}", sql_quote(s)));
        }
        if let Some(u) = until {
            clauses.push(format!("created_at < {}", sql_quote(u)));
        }
        if let Some(r) = role {
            clauses.push(format!("role = {}", sql_quote(r)));
        }
        if let Some(b) = block_type {
            clauses.push(format!("block_type = {}", sql_quote(b)));
        }
        let mut rows = self
            .query_conversation_messages(clauses.join(" AND "))
            .await?;
        if let Some((cur_at, cur_line, cur_idx)) = cursor {
            rows.retain(|m| cursor_after(m, cur_at, cur_line, cur_idx));
        }
        rows.sort_by(chrono_asc);
        let fetch = lim.saturating_add(1) as usize;
        let has_more = rows.len() >= fetch;
        rows.truncate(lim.max(0) as usize);
        Ok((rows, has_more))
    }

    /// Route-B native equivalent of
    /// `DuckDbQuery::list_conversation_messages_in_range`: cross-session range
    /// scan over the half-open `[time_from, time_to)` window (each bound
    /// optional), optionally narrowed by `role` / `block_type`, ordered
    /// chronologically and paginated by the same composite cursor as
    /// [`Self::get_conversation_messages_by_session_paged`]. Null-session
    /// blocks are excluded (anchored to a conversation). LanceDB has no
    /// `ORDER BY`, so cursor + ordering + N+1 slice run in Rust.
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
        let lim = i64::try_from(limit).unwrap_or(64);
        let mut clauses = vec![
            format!("tenant = {}", sql_quote(tenant)),
            "session_id IS NOT NULL".to_string(),
        ];
        if let Some(s) = time_from {
            clauses.push(format!("created_at >= {}", sql_quote(s)));
        }
        if let Some(u) = time_to {
            clauses.push(format!("created_at < {}", sql_quote(u)));
        }
        if let Some(r) = role {
            clauses.push(format!("role = {}", sql_quote(r)));
        }
        if let Some(b) = block_type {
            clauses.push(format!("block_type = {}", sql_quote(b)));
        }
        let mut rows = self
            .query_conversation_messages(clauses.join(" AND "))
            .await?;
        if let Some((cur_at, cur_line, cur_idx)) = cursor {
            rows.retain(|m| cursor_after(m, cur_at, cur_line, cur_idx));
        }
        rows.sort_by(chrono_asc);
        let fetch = lim.saturating_add(1) as usize;
        let has_more = rows.len() >= fetch;
        rows.truncate(lim.max(0) as usize);
        Ok((rows, has_more))
    }

    /// Route-B native equivalent of `DuckDbQuery::context_window_for_block`:
    /// the primary block + `k_before` predecessors + `k_after` successors in
    /// the same session, neighbors ordered chronologically ASC. The primary is
    /// always returned (even when `include_tool_blocks=false` and its own
    /// block_type is tool_use/tool_result); the filter applies to neighbors
    /// only. Returns `Err(StorageError::NotFound("transcript primary block"))`
    /// when no row matches the primary id under this tenant, and empty
    /// `before`/`after` when the primary has no session_id.
    ///
    /// Mirrors the DuckDB three-scan shape: primary fetch, then predecessors
    /// (strict tuple `<`, take nearest `k_before`, return ASC) and successors
    /// (strict tuple `>`, take nearest `k_after`). LanceDB has no `ORDER BY` /
    /// `LIMIT`, so the tuple comparison + ordering + cap run in Rust.
    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        // 1. Primary fetch.
        let primary_rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND message_block_id = {}",
                sql_quote(tenant),
                sql_quote(primary_id),
            ))
            .await?;
        let Some(primary) = primary_rows.into_iter().next() else {
            return Err(StorageError::NotFound("transcript primary block"));
        };

        // 2. No session → no neighbors.
        let Some(session_id) = primary.session_id.clone() else {
            return Ok(ContextWindow {
                primary,
                before: Vec::new(),
                after: Vec::new(),
            });
        };

        // 3. Scan the session once, then split into before/after in Rust.
        let mut session_rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {}",
                sql_quote(tenant),
                sql_quote(&session_id),
            ))
            .await?;
        // Optional block_type filter applies to NEIGHBORS only (text/thinking).
        if !include_tool_blocks {
            use crate::domain::BlockType;
            session_rows.retain(|m| matches!(m.block_type, BlockType::Text | BlockType::Thinking));
        }

        let p_at = primary.created_at.as_str();
        let p_line = primary.line_number as i64;
        let p_idx = primary.block_index as i64;

        // Predecessors: strict tuple `<` primary; nearest `k_before` (sort DESC,
        // take k, reverse to ASC for the caller).
        let mut before: Vec<ConversationMessage> = session_rows
            .iter()
            .filter(|m| tuple_lt(m, p_at, p_line, p_idx))
            .cloned()
            .collect();
        before.sort_by(chrono_desc);
        before.truncate(k_before);
        before.reverse();

        // Successors: strict tuple `>` primary; nearest `k_after` (sort ASC,
        // take k).
        let mut after: Vec<ConversationMessage> = session_rows
            .iter()
            .filter(|m| tuple_gt(m, p_at, p_line, p_idx))
            .cloned()
            .collect();
        after.sort_by(chrono_asc);
        after.truncate(k_after);

        Ok(ContextWindow {
            primary,
            before,
            after,
        })
    }

    /// Route-B native equivalent of `DuckDbQuery::anchor_session_candidates`:
    /// the most-recent `embed_eligible` blocks in `(tenant, session_id)`,
    /// capped at `k`, ordered `created_at DESC`, returning `message_block_id`s
    /// only. `k == 0` short-circuits to `Ok(vec![])` (matches DuckDB). LanceDB
    /// has no `ORDER BY` / `LIMIT`, so the sort + cap run in Rust.
    ///
    /// The DuckDB query orders by `created_at DESC` with no secondary
    /// tie-break; to stay deterministic on a `created_at` tie we add a stable
    /// `(line_number DESC, block_index DESC)` secondary key (consistent with
    /// `recent_conversation_messages`).
    pub async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let k_cap = i64::try_from(k).unwrap_or(64) as usize;
        let mut rows = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {} AND embed_eligible = true",
                sql_quote(tenant),
                sql_quote(session_id),
            ))
            .await?;
        rows.sort_by(chrono_desc);
        rows.truncate(k_cap);
        Ok(rows.into_iter().map(|m| m.message_block_id).collect())
    }
}

/// Chronological ASC comparator `(created_at, line_number, block_index)` —
/// the DuckDB `ORDER BY created_at ASC, line_number ASC, block_index ASC`.
fn chrono_asc(a: &ConversationMessage, b: &ConversationMessage) -> std::cmp::Ordering {
    a.created_at
        .cmp(&b.created_at)
        .then_with(|| a.line_number.cmp(&b.line_number))
        .then_with(|| a.block_index.cmp(&b.block_index))
}

/// Chronological DESC comparator (reverse of [`chrono_asc`]).
fn chrono_desc(a: &ConversationMessage, b: &ConversationMessage) -> std::cmp::Ordering {
    chrono_asc(b, a)
}

/// `(created_at, line_number, block_index)` strictly greater than the cursor
/// tuple — the DuckDB paged/range resume predicate (rows AFTER the cursor
/// under chronological ASC ordering).
fn cursor_after(m: &ConversationMessage, cur_at: &str, cur_line: i64, cur_idx: i64) -> bool {
    tuple_gt(m, cur_at, cur_line, cur_idx)
}

/// `m`'s `(created_at, line_number, block_index)` tuple `>` `(at, line, idx)`.
fn tuple_gt(m: &ConversationMessage, at: &str, line: i64, idx: i64) -> bool {
    let ml = m.line_number as i64;
    let mi = m.block_index as i64;
    m.created_at.as_str() > at
        || (m.created_at.as_str() == at && (ml > line || (ml == line && mi > idx)))
}

/// `m`'s `(created_at, line_number, block_index)` tuple `<` `(at, line, idx)`.
fn tuple_lt(m: &ConversationMessage, at: &str, line: i64, idx: i64) -> bool {
    let ml = m.line_number as i64;
    let mi = m.block_index as i64;
    m.created_at.as_str() < at
        || (m.created_at.as_str() == at && (ml < line || (ml == line && mi < idx)))
}

/// Transcript-side methods — previously bound by the
/// `TranscriptRepository` trait, now inherent on `LanceStore`.
impl LanceStore {
    pub async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        // Idempotent on (transcript_path, line_number, block_index).
        // When the row is freshly written and `embed_eligible`, also
        // enqueue a transcript_embedding_jobs row so the worker
        // picks it up. Idempotent re-inserts (existing row) skip
        // enqueue — caller can call this on every replay without
        // duplicating jobs.
        let table = self
            .conn
            .open_table("conversation_messages")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let exists = table
            .count_rows(Some(format!(
                "transcript_path = {} AND line_number = {} AND block_index = {}",
                sql_quote(&msg.transcript_path),
                msg.line_number,
                msg.block_index,
            )))
            .await
            .map_err(lancedb_err)?;
        let newly_inserted = exists == 0;
        if newly_inserted {
            let batch = conversation_messages_to_record_batch(std::slice::from_ref(msg))?;
            table.add(batch).execute().await.map_err(lancedb_err)?;
        }

        // The enqueue is a SEPARATE Lance commit from the row insert, so a
        // transient failure after the row landed would orphan the block (no job)
        // and the idempotent exists-probe above would skip it forever. Ensure a
        // job exists: a fresh insert always needs one; a replay (row already
        // present) enqueues only when no job row exists yet for the block — which
        // repairs that orphan exactly once and never double-enqueues, since a
        // completed job row is retained and still counts as "present".
        if msg.embed_eligible {
            // Enqueue when: just inserted (row certainly exists, no job yet), OR
            // on a replay where no job row exists yet AND a row truly exists for
            // THIS block id. The job probe is checked first so the common replay
            // (job already present) short-circuits to a single query; the
            // block-exists probe only runs when a repair looks needed, and guards
            // against enqueuing for a message deduplicated away by key.
            let needs_enqueue = newly_inserted
                || (self
                    .transcript_jobs_present_for_blocks(&[msg.message_block_id.as_str()])
                    .await?
                    .is_empty()
                    && self
                        .conversation_message_block_exists(&msg.message_block_id)
                        .await?);
            if needs_enqueue {
                // Provider id is configured once at startup via
                // `set_transcript_job_provider`. Failing loudly here is
                // preferable to silently substituting a default that
                // would later mismatch the worker's `job_provider_id()`.
                let provider = self
                    .transcript_job_provider()
                    .ok_or(StorageError::InvalidData(
                        "transcript embedding job provider not configured; \
                         call LanceStore::set_transcript_job_provider during startup",
                    ))?;
                let job_id = uuid::Uuid::now_v7().to_string();
                let now = crate::storage::current_timestamp();
                self.try_enqueue_transcript_embedding_job(
                    job_id,
                    msg.tenant.clone(),
                    msg.message_block_id.clone(),
                    provider,
                    now,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Bulk variant of [`Self::create_conversation_message`]. Idempotent on
    /// (transcript_path, line_number, block_index) like the single-row form,
    /// but batches the dedup probe (one Lance filter per call rather than
    /// per row) and the writes (one `table.add` for messages + one for
    /// embedding jobs).
    ///
    /// Returns the number of rows that actually landed (input length minus
    /// rows that already existed and minus intra-batch duplicates).
    pub async fn create_conversation_messages_batch(
        &self,
        msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        use std::collections::HashSet;

        if msgs.is_empty() {
            return Ok(0);
        }

        // 1. Build a single filter that pulls every existing row whose
        //    `transcript_path` appears in the batch. For typical
        //    `mem mine` chunks this is one path; even for fan-in writers
        //    the path-set is tiny vs. row count.
        let mut paths: Vec<&str> = msgs.iter().map(|m| m.transcript_path.as_str()).collect();
        paths.sort_unstable();
        paths.dedup();
        let in_list = paths
            .iter()
            .map(|p| sql_quote(p))
            .collect::<Vec<_>>()
            .join(", ");
        let table = self
            .conn
            .open_table("conversation_messages")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let existing = self
            .query_conversation_messages(format!("transcript_path IN ({in_list})"))
            .await?;
        // Capture the existing rows' keys (for dedup) AND their message_block_ids
        // (so step 4's job reconciliation only ever targets blocks that actually
        // have a row — a message deduplicated away by key must not get a job).
        let mut seen: HashSet<(String, u64, u32)> = HashSet::with_capacity(existing.len());
        let mut existing_block_ids: HashSet<String> = HashSet::with_capacity(existing.len());
        for m in existing {
            seen.insert((m.transcript_path, m.line_number, m.block_index));
            existing_block_ids.insert(m.message_block_id);
        }

        // 2. Walk the input, dropping rows whose key is already in
        //    `seen` (DB OR intra-batch dup). Insert key into `seen` so a
        //    subsequent row with the same key is also skipped.
        let mut to_insert: Vec<&ConversationMessage> = Vec::with_capacity(msgs.len());
        for msg in msgs {
            let key = (
                msg.transcript_path.clone(),
                msg.line_number,
                msg.block_index,
            );
            if seen.insert(key) {
                to_insert.push(msg);
            }
        }
        // 3. One multi-row insert for the genuinely-new rows.
        if !to_insert.is_empty() {
            let owned: Vec<ConversationMessage> = to_insert.iter().map(|m| (*m).clone()).collect();
            let batch = conversation_messages_to_record_batch(&owned)?;
            table.add(batch).execute().await.map_err(lancedb_err)?;
        }

        // 4. Reconcile embedding jobs for every embed-eligible block that has a
        //    row — freshly inserted OR already present — not just the inserted
        //    subset. The enqueue is a separate commit from the row insert, so a
        //    row that already existed may be orphaned (its original enqueue
        //    failed after the row committed). Enqueue exactly the blocks that
        //    have no job row yet, so a `mem mine` replay repairs orphans without
        //    ever double-enqueuing (a completed job row is retained and counts as
        //    "present"). Blocks deduplicated away by key own no row, so they are
        //    excluded via `rows_present`.
        let mut rows_present: HashSet<&str> = HashSet::with_capacity(to_insert.len());
        for m in &to_insert {
            rows_present.insert(m.message_block_id.as_str());
        }
        for bid in &existing_block_ids {
            rows_present.insert(bid.as_str());
        }
        let mut eligible_block_ids: Vec<&str> = msgs
            .iter()
            .filter(|m| m.embed_eligible && rows_present.contains(m.message_block_id.as_str()))
            .map(|m| m.message_block_id.as_str())
            .collect();
        eligible_block_ids.sort_unstable();
        eligible_block_ids.dedup();
        if !eligible_block_ids.is_empty() {
            let present = self
                .transcript_jobs_present_for_blocks(&eligible_block_ids)
                .await?;
            let now = crate::storage::current_timestamp();
            let mut emitted: HashSet<&str> = HashSet::new();
            let mut cached_provider: Option<String> = None;
            let mut jobs: Vec<TranscriptEmbeddingJobRow> = Vec::new();
            for msg in msgs
                .iter()
                .filter(|m| m.embed_eligible && rows_present.contains(m.message_block_id.as_str()))
            {
                let bid = msg.message_block_id.as_str();
                if present.contains(bid) || !emitted.insert(bid) {
                    continue;
                }
                let provider = match &cached_provider {
                    Some(p) => p.clone(),
                    None => {
                        let p = self
                            .transcript_job_provider()
                            .ok_or(StorageError::InvalidData(
                                "transcript embedding job provider not configured; \
                                 call LanceStore::set_transcript_job_provider during startup",
                            ))?;
                        cached_provider = Some(p.clone());
                        p
                    }
                };
                jobs.push(TranscriptEmbeddingJobRow {
                    job_id: uuid::Uuid::now_v7().to_string(),
                    tenant: msg.tenant.clone(),
                    message_block_id: msg.message_block_id.clone(),
                    provider,
                    status: "pending".to_string(),
                    attempt_count: 0,
                    last_error: None,
                    available_at: now.clone(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                });
            }
            if !jobs.is_empty() {
                let job_table = self
                    .conn
                    .open_table("transcript_embedding_jobs")
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
                let job_batch = transcript_embedding_job_rows_to_record_batch(&jobs)?;
                job_table
                    .add(job_batch)
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
            }
        }

        Ok(to_insert.len())
    }

    // Transcript reads + writes are both lance-native and live in this
    // module (route-B): writes (create_conversation_message,
    // create_conversation_messages) and reads (get_by_session,
    // get_by_session_paged, list_transcript_sessions,
    // fetch_conversation_messages_by_ids, context_window_for_block,
    // anchor_session_candidates, recent_conversation_messages,
    // semantic_search_transcripts, bm25_transcript_candidates), plus the
    // embedding-job helpers below. Read shapes are parity-gated by
    // `tests/parity_golden.rs`.

    /// Route-B bucket "transcript_ann": native lancedb-Rust equivalent of
    /// `DuckDbQuery::semantic_search_transcripts`.
    ///
    /// Runs a lance-native vector ANN (`nearest_to`) over ALL tenants'
    /// chunk embeddings, collapses chunk-rows to one row per
    /// `message_block_id` keeping the MIN `_distance`, hydrates against
    /// `conversation_messages` filtering `tenant = ? AND embed_eligible =
    /// true`, orders `best_distance ASC`, and `LIMIT`s to `limit`. We mirror
    /// each step with the native API:
    ///
    /// 1. Empty embedding / `limit == 0` → `Ok(vec![])`. Missing
    ///    `conversation_message_embeddings` table (lazy-created on first
    ///    upsert) → `Ok(vec![])`, mirroring the capsule-ANN resilience.
    /// 2. `nearest_to(query_embedding).limit(oversample)` — NO tenant /
    ///    embed_eligible predicate on the vector query (those columns aren't
    ///    on the embeddings table; the JOIN supplies them → POSTFILTER).
    /// 3. CHUNK-COLLAPSE in Rust: GROUP BY `message_block_id` keeping the MIN
    ///    `_distance` (a message may carry N chunk-embeddings).
    /// 4. Fetch the `ConversationMessage` rows for the collapsed ids by
    ///    scanning `conversation_messages` natively (one read over the
    ///    small collapsed id-set) and apply the JOIN
    ///    filter `tenant == ? AND embed_eligible == true`.
    /// 5. ORDER BY `best_distance ASC`, take `limit`, build
    ///    `(ConversationMessage, similarity)` where `similarity = 1 -
    ///    L²/2` — the same cosine derivation the DuckDB side uses for
    ///    normalized embeddings. (The parity golden only compares
    ///    `message_block_id`s, so the f32 just needs to be sane.)
    ///
    /// Parity-gated by `tests/parity_golden.rs`.
    pub async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Lazy-created table: a brand-new store has no transcript embeddings
        // until the first upsert. Mirror the DuckDB resilience → empty result.
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "conversation_message_embeddings") {
            return Ok(Vec::new());
        }

        // Same oversample clamp as the DuckDB source: clamp limit to
        // [1, 1024], then `oversample = min(limit * 4, 4096)` so the ANN
        // branch returns enough distinct messages after chunk-collapse.
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        const MAX_ANN_OVERSAMPLE: i64 = 4_096;
        let oversample = lim.saturating_mul(4).min(MAX_ANN_OVERSAMPLE);

        // POSTFILTER, not prefilter: `nearest_to(...).limit(oversample)`
        // across ALL tenants' chunk vectors (no tenant / embed_eligible
        // predicate — those columns live on `conversation_messages`, supplied
        // by the JOIN below). `nearest_to` adds a `_distance` (Float32) column
        // ordered ascending.
        const TABLE: &str = "conversation_message_embeddings";
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .nearest_to(query_embedding)
            .map_err(lancedb_err)?
            .limit(usize::try_from(oversample).unwrap_or(usize::MAX))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;

        // CHUNK-COLLAPSE: GROUP BY message_block_id keeping MIN(_distance).
        // A message embedded as N chunk vectors yields N rows here; we fold
        // them to one best-distance hit before the JOIN (behaviour-preserving
        // for single-embedding messages). Mirrors the DuckDB inner subquery.
        let mut best_distance: std::collections::HashMap<String, f32> =
            std::collections::HashMap::new();
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let ids = parse_col::<StringArray>(b, TABLE, "message_block_id")?;
            let dists = parse_col::<Float32Array>(b, TABLE, "_distance")?;
            for i in 0..b.num_rows() {
                let id = ids.value(i).to_string();
                let d = dists.value(i);
                best_distance
                    .entry(id)
                    .and_modify(|cur| {
                        if d < *cur {
                            *cur = d;
                        }
                    })
                    .or_insert(d);
            }
        }
        if best_distance.is_empty() {
            return Ok(Vec::new());
        }

        // Fetch the collapsed messages and apply the JOIN filter
        // `tenant == ? AND embed_eligible == true` by scanning
        // `conversation_messages` natively here (one read over the small
        // collapsed id-set). An `id IN (...)`
        // `only_if` over the (small) collapsed id-set keeps it to one read.
        let id_list = best_distance
            .keys()
            .map(|id| sql_quote(id))
            .collect::<Vec<_>>()
            .join(", ");
        let messages = self
            .query_conversation_messages(format!(
                "tenant = {} AND embed_eligible = true AND message_block_id IN ({id_list})",
                sql_quote(tenant),
            ))
            .await?;

        // ORDER BY best_distance ASC, tie-break message_block_id ASC for a
        // deterministic order, then take `limit`. Carry the raw distance
        // through the sort, then map to `1 - L²/2` — the cosine similarity
        // for normalized embeddings (same derivation as the DuckDB source).
        let mut scored: Vec<(ConversationMessage, f32)> = messages
            .into_iter()
            .filter_map(|m| best_distance.get(&m.message_block_id).map(|&d| (m, d)))
            .collect();
        scored.sort_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then_with(|| a.0.message_block_id.cmp(&b.0.message_block_id))
        });
        scored.truncate(limit);
        Ok(scored
            .into_iter()
            .map(|(m, d)| (m, 1.0_f32 - d / 2.0_f32))
            .collect())
    }

    /// Rebuild the Tantivy transcript FTS index from the current
    /// `conversation_messages` corpus. Scans every row across all tenants
    /// (the index is tenant-tagged and filters at query time), keeping
    /// only `embed_eligible = true` rows — the same scope the DuckDB
    /// `bm25_transcript_candidates` query enforces in its outer WHERE
    /// (`tenant = ? AND embed_eligible = true`). A full rebuild, matching
    /// the route-B "startup full-rebuild" strategy (see
    /// `crate::storage::fts`). Marks the index built so the lazy path in
    /// [`Self::bm25_transcript_candidates`] doesn't redundantly rebuild.
    pub async fn rebuild_transcript_fts(&self) -> Result<(), StorageError> {
        // Scan all tenants — the FTS index is tenant-tagged and filters
        // tenant at query time (POSTFILTER, same posture as the rest of
        // route-B). `embed_eligible = true` mirrors the DuckDB BM25 outer
        // filter; the index never holds ineligible blocks.
        let rows = self
            .query_conversation_messages("embed_eligible = true".to_string())
            .await?;
        let docs: Vec<crate::storage::fts::FtsDoc> = rows
            .into_iter()
            .map(|m| crate::storage::fts::FtsDoc {
                id: m.message_block_id,
                tenant: m.tenant,
                content: m.content,
            })
            .collect();
        // Tantivy writes are synchronous + CPU-bound; run them off the
        // async reactor.
        let fts = self.transcript_fts.clone();
        tokio::task::spawn_blocking(move || fts.rebuild(&docs))
            .await
            .map_err(|e| {
                StorageError::InvalidInput(format!("transcript fts rebuild join: {e}"))
            })??;
        self.transcript_fts_built
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Route-B bucket "transcript_fts": native (Tantivy) equivalent of
    /// `DuckDbQuery::bm25_transcript_candidates`.
    ///
    /// Queries the in-RAM Tantivy index
    /// ([`crate::storage::fts::FtsIndex`]) built from the transcript
    /// corpus, filtering `tenant = ? AND embed_eligible = true`,
    /// ordering by BM25 score DESC and limiting to `k`. The index is
    /// (re)built via [`Self::rebuild_transcript_fts`] — eagerly by
    /// `rebuild_query_indexes`, or lazily here on first use if it was never
    /// built (so the route-B read engine works standalone). The query is
    /// term-split through the jieba tokenizer so unspaced CJK runs match
    /// (the load-bearing CJK fix — see `fts` module docs).
    ///
    /// Steps:
    /// 1. Empty / whitespace `query` or `k == 0` → `Ok(vec![])`.
    /// 2. `bm25(tenant, query, k)` → top-k `message_block_id`s in BM25
    ///    score order (the index already filters `tenant` + only holds
    ///    `embed_eligible` rows).
    /// 3. Fetch the `ConversationMessage` rows for those ids (one native
    ///    `conversation_messages` scan, defensively re-applying `tenant =
    ///    ? AND embed_eligible = true`) and return them in BM25 order,
    ///    dropping any id that vanished between index build and fetch.
    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // Lazy build: a route-B store that never ran rebuild_query_indexes
        // still needs a populated index. Idempotent — the eager path flips
        // `transcript_fts_built` so this only fires once.
        if !self
            .transcript_fts_built
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.rebuild_transcript_fts().await?;
        }
        // BM25 ranking off the reactor (synchronous Tantivy search).
        let ranked = {
            let fts = self.transcript_fts.clone();
            let tenant_owned = tenant.to_string();
            let query_owned = query.to_string();
            tokio::task::spawn_blocking(move || fts.bm25(&tenant_owned, &query_owned, k))
                .await
                .map_err(|e| {
                    StorageError::InvalidInput(format!("transcript fts query join: {e}"))
                })??
        };
        if ranked.is_empty() {
            return Ok(Vec::new());
        }

        // Hydrate the ranked ids → ConversationMessage rows. One native scan
        // over the (small) ranked id-set, re-applying the JOIN filter
        // `tenant = ? AND embed_eligible = true` defensively.
        let id_list = ranked
            .iter()
            .map(|(id, _)| sql_quote(id))
            .collect::<Vec<_>>()
            .join(", ");
        let messages = self
            .query_conversation_messages(format!(
                "tenant = {} AND embed_eligible = true AND message_block_id IN ({id_list})",
                sql_quote(tenant),
            ))
            .await?;

        // Return in BM25 rank order, dropping any id that vanished between
        // index build and fetch (same tolerance the DuckDB JOIN has).
        let by_id: std::collections::HashMap<String, ConversationMessage> = messages
            .into_iter()
            .map(|m| (m.message_block_id.clone(), m))
            .collect();
        let mut out = Vec::with_capacity(ranked.len());
        for (id, _rank) in ranked {
            if let Some(m) = by_id.get(&id) {
                out.push(m.clone());
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockType, ConversationMessage, MessageRole};
    use tempfile::tempdir;

    #[allow(clippy::too_many_arguments)]
    fn msg(
        id: &str,
        tenant: &str,
        session: Option<&str>,
        line: u64,
        block_idx: u32,
        block_type: BlockType,
        content: &str,
        created_at: &str,
    ) -> ConversationMessage {
        ConversationMessage {
            message_block_id: id.into(),
            session_id: session.map(String::from),
            tenant: tenant.into(),
            caller_agent: "claude-code".into(),
            transcript_path: format!("/tmp/{id}.jsonl"),
            line_number: line,
            block_index: block_idx,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type,
            content: content.into(),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: matches!(block_type, BlockType::Text | BlockType::Thinking),
            created_at: created_at.into(),
            meta_json: None,
        }
    }

    // The `lancedb_transcript_repository_round_trip` test that used
    // to live here was deleted along with the 8 lance-side transcript
    // readers it exercised (`get_conversation_messages_by_session`,
    // `get_conversation_messages_by_session_paged`,
    // `list_transcript_sessions`, `fetch_conversation_messages_by_ids`,
    // `context_window_for_block`, `anchor_session_candidates`,
    // `recent_conversation_messages`, `bm25_transcript_candidates`).
    // The read shapes are parity-gated by `tests/parity_golden.rs`
    // (the `transcript_*` buckets), which seed via
    // `LanceStore::create_conversation_message` and assert the
    // lance-native read output against frozen goldens.

    /// Bulk insert path: dedup against existing rows + intra-batch
    /// dedup + bulk job enqueue. Counts must match `inserted` and the
    /// transcript_embedding_jobs row count must equal the embed-eligible
    /// new rows.
    #[tokio::test]
    pub async fn create_conversation_messages_batch_dedups_and_enqueues() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();
        repo.set_transcript_job_provider("fake-test");

        // All rows share the same transcript_path so the dedup key
        // `(transcript_path, line, block)` actually collides for
        // duplicates. (The `msg` helper derives transcript_path from
        // `id`, which we override below.)
        let shared_path = "/tmp/shared.jsonl";

        // Pre-seed one row so dedup against existing has something to
        // hit.
        let mut pre = msg(
            "pre_1",
            "tenant-a",
            Some("sess_a"),
            10,
            0,
            BlockType::Text,
            "pre-existing block",
            "00000001778000000010",
        );
        pre.transcript_path = shared_path.to_string();
        repo.create_conversation_message(&pre).await.unwrap();

        // Build 4 rows: one duplicate of the pre-seeded key, two
        // intra-batch duplicates of the same fresh key, and two unique
        // fresh keys (one text=embed_eligible, one tool_use=ineligible).
        let mut dup_pre = msg(
            "dup_pre",
            "tenant-a",
            Some("sess_a"),
            10,
            0,
            BlockType::Text,
            "duplicate of pre_1",
            "00000001778000000011",
        );
        dup_pre.transcript_path = shared_path.to_string();
        let mut new_a = msg(
            "new_a",
            "tenant-a",
            Some("sess_a"),
            12,
            0,
            BlockType::Text,
            "new fresh block A",
            "00000001778000000020",
        );
        new_a.transcript_path = shared_path.to_string();
        let mut new_a_dup = msg(
            "new_a_dup",
            "tenant-a",
            Some("sess_a"),
            12,
            0,
            BlockType::Text,
            "intra-batch dup of new_a",
            "00000001778000000021",
        );
        new_a_dup.transcript_path = shared_path.to_string();
        let mut new_b = msg(
            "new_b",
            "tenant-a",
            Some("sess_a"),
            14,
            0,
            BlockType::ToolUse,
            "{\"tool\":\"Bash\"}",
            "00000001778000000030",
        );
        new_b.transcript_path = shared_path.to_string();

        let inserted = repo
            .create_conversation_messages_batch(&[
                dup_pre.clone(),
                new_a.clone(),
                new_a_dup.clone(),
                new_b.clone(),
            ])
            .await
            .unwrap();
        assert_eq!(inserted, 2, "only new_a + new_b should land");

        // Verify the table actually contains pre_1 + new_a + new_b
        // (3 distinct ids). We go through the lance-side
        // `query_conversation_messages` helper directly rather than the
        // session-scoped reader (`get_conversation_messages_by_session`)
        // to keep this write-path test independent of the read layer.
        let all = repo
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {}",
                super::sql_quote("tenant-a"),
                super::sql_quote("sess_a"),
            ))
            .await
            .unwrap();
        let ids: Vec<&str> = all.iter().map(|m| m.message_block_id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"pre_1"));
        assert!(ids.contains(&"new_a"));
        assert!(ids.contains(&"new_b"));

        // Embedding jobs: pre-seed enqueued one (text=embed_eligible),
        // batch enqueued one more (new_a is text; new_b is tool_use,
        // ineligible). Expect 2 total.
        let jobs = repo
            .query_transcript_embedding_jobs(format!("tenant = {}", super::sql_quote("tenant-a")))
            .await
            .unwrap();
        assert_eq!(jobs.len(), 2);
    }

    /// Empty input is a clean no-op (no Lance write, no embedding-job
    /// enqueue, no panic when the provider hasn't been configured).
    #[tokio::test]
    pub async fn create_conversation_messages_batch_empty_is_noop() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();
        // Note: provider intentionally NOT configured — empty path
        // must not touch the enqueue branch.
        let inserted = repo.create_conversation_messages_batch(&[]).await.unwrap();
        assert_eq!(inserted, 0);
    }

    async fn job_count(repo: &LanceStore, block_id: &str) -> usize {
        repo.query_transcript_embedding_jobs(format!(
            "message_block_id = {}",
            super::sql_quote(block_id)
        ))
        .await
        .unwrap()
        .len()
    }

    async fn drop_job(repo: &LanceStore, block_id: &str) {
        repo.conn
            .open_table("transcript_embedding_jobs")
            .execute()
            .await
            .unwrap()
            .delete(&format!(
                "message_block_id = {}",
                super::sql_quote(block_id)
            ))
            .await
            .unwrap();
    }

    /// Regression: the message insert and the embedding-job enqueue are two
    /// separate Lance commits. If the row lands but the enqueue then fails
    /// (transient), the block is orphaned — no job — and the idempotent
    /// re-insert used to `return Ok(())` on the exists-probe BEFORE reaching the
    /// enqueue, so the job was never created and the block silently lost ANN
    /// coverage. Replaying create_conversation_message must REPAIR the missing
    /// job, and must not duplicate one that already exists.
    #[tokio::test]
    pub async fn create_conversation_message_replay_repairs_orphaned_job() {
        let dir = tempdir().unwrap();
        let repo = LanceStore::open(&dir.path().join("lance.store"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake-test");

        let m = msg(
            "mb_orphan",
            "tenant-a",
            Some("sess_a"),
            1,
            0,
            BlockType::Text,
            "an embed-eligible block",
            "00000001778000000010",
        );
        repo.create_conversation_message(&m).await.unwrap();
        assert_eq!(
            job_count(&repo, "mb_orphan").await,
            1,
            "first write enqueues one job"
        );

        // Simulate the enqueue having failed after the row committed.
        drop_job(&repo, "mb_orphan").await;
        assert_eq!(
            job_count(&repo, "mb_orphan").await,
            0,
            "precondition: orphaned"
        );

        // Replay: row already exists; the fix must re-create the missing job.
        repo.create_conversation_message(&m).await.unwrap();
        assert_eq!(
            job_count(&repo, "mb_orphan").await,
            1,
            "replay repairs the orphan"
        );

        // Replaying again must NOT duplicate the now-present job.
        repo.create_conversation_message(&m).await.unwrap();
        assert_eq!(
            job_count(&repo, "mb_orphan").await,
            1,
            "no duplicate enqueue"
        );
    }

    /// Same orphan-repair guarantee on the bulk path: when every row in the
    /// batch already exists (a `mem mine` replay), the old code early-returned
    /// before the enqueue block, so an orphaned block was never repaired. The
    /// reconciliation must run over the whole batch regardless of how many rows
    /// are freshly inserted.
    #[tokio::test]
    pub async fn create_conversation_messages_batch_replay_repairs_orphaned_job() {
        let dir = tempdir().unwrap();
        let repo = LanceStore::open(&dir.path().join("lance.store"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake-test");

        let mut m = msg(
            "mb_b_orphan",
            "tenant-a",
            Some("sess_a"),
            5,
            0,
            BlockType::Text,
            "bulk embed-eligible block",
            "00000001778000000050",
        );
        m.transcript_path = "/tmp/bulk.jsonl".to_string();

        assert_eq!(
            repo.create_conversation_messages_batch(&[m.clone()])
                .await
                .unwrap(),
            1
        );
        assert_eq!(job_count(&repo, "mb_b_orphan").await, 1);

        drop_job(&repo, "mb_b_orphan").await;
        assert_eq!(
            job_count(&repo, "mb_b_orphan").await,
            0,
            "precondition: orphaned"
        );

        // Replay the same batch: every row already exists (inserted == 0), but
        // the orphaned job must still be repaired.
        assert_eq!(
            repo.create_conversation_messages_batch(&[m.clone()])
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            job_count(&repo, "mb_b_orphan").await,
            1,
            "bulk replay repairs the orphan"
        );

        // And no duplicate on a further replay.
        repo.create_conversation_messages_batch(&[m.clone()])
            .await
            .unwrap();
        assert_eq!(
            job_count(&repo, "mb_b_orphan").await,
            1,
            "no duplicate enqueue"
        );
    }

    /// Same HIGH-bug regression as the capsule queue, for the transcript
    /// queue: an orphaned `processing` job (worker crash / restart mid-embed /
    /// mid-batch error) must be reclaimable once its lease elapses, not before.
    #[tokio::test]
    pub async fn claim_reclaims_orphaned_processing_transcript_jobs_after_lease() {
        use crate::storage::{timestamp_add_ms, EMBEDDING_JOB_LEASE_MS};

        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let claimed_at = "00000001778000000000";
        repo.try_enqueue_transcript_embedding_job(
            "tjob_orph".into(),
            "tenant-a".into(),
            "mb-orph".into(),
            "fake-test".into(),
            claimed_at.into(),
        )
        .await
        .unwrap();

        let first = repo
            .claim_next_n_transcript_embedding_jobs(claimed_at, 5, 5)
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].job_id, "tjob_orph");

        // Within the lease → still in-flight, must NOT be reclaimed.
        let within = timestamp_add_ms(claimed_at, EMBEDDING_JOB_LEASE_MS - 1);
        let none = repo
            .claim_next_n_transcript_embedding_jobs(&within, 5, 5)
            .await
            .unwrap();
        assert!(
            none.is_empty(),
            "in-lease transcript job must not be reclaimed"
        );

        // Past the lease → orphan reclaimed.
        let past = timestamp_add_ms(claimed_at, EMBEDDING_JOB_LEASE_MS + 1);
        let reclaimed = repo
            .claim_next_n_transcript_embedding_jobs(&past, 5, 5)
            .await
            .unwrap();
        assert_eq!(
            reclaimed.len(),
            1,
            "orphaned transcript job must be reclaimed after the lease"
        );
        assert_eq!(reclaimed[0].job_id, "tjob_orph");
    }
}
