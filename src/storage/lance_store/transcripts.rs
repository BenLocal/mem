//! Transcript pipeline (parallel to memories): conversation_messages
//! reads/writes, transcript_embedding_jobs queue, and
//! conversation_message_embeddings upsert/delete. All inherent on
//! LanceStore.

use arrow_array::RecordBatch;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    conversation_message_embedding_to_record_batch, conversation_messages_to_record_batch,
    ensure_conversation_message_embeddings_table, lancedb_err,
    record_batch_to_conversation_messages, record_batch_to_transcript_embedding_job_rows,
    sql_quote, transcript_embedding_job_row_to_record_batch,
    transcript_embedding_job_rows_to_record_batch, LanceStore, TranscriptEmbeddingJobRow,
};
use crate::domain::ConversationMessage;
use crate::embedding::wire::decode_f32_blob;
use crate::storage::types::{ClaimedTranscriptEmbeddingJob, StorageError};
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
        if exists > 0 {
            return Ok(());
        }
        let batch = conversation_messages_to_record_batch(std::slice::from_ref(msg))?;
        table.add(batch).execute().await.map_err(lancedb_err)?;

        if msg.embed_eligible {
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
        let mut seen: HashSet<(String, u64, u32)> = existing
            .into_iter()
            .map(|m| (m.transcript_path, m.line_number, m.block_index))
            .collect();

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
        if to_insert.is_empty() {
            return Ok(0);
        }

        // 3. One multi-row insert.
        let owned: Vec<ConversationMessage> = to_insert.iter().map(|m| (*m).clone()).collect();
        let batch = conversation_messages_to_record_batch(&owned)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;

        // 4. One multi-row enqueue for the embed-eligible subset.
        let mut jobs: Vec<TranscriptEmbeddingJobRow> = Vec::new();
        if to_insert.iter().any(|m| m.embed_eligible) {
            let provider = self
                .transcript_job_provider()
                .ok_or(StorageError::InvalidData(
                    "transcript embedding job provider not configured; \
                         call LanceStore::set_transcript_job_provider during startup",
                ))?;
            let now = crate::storage::current_timestamp();
            for msg in to_insert.iter().filter(|m| m.embed_eligible) {
                jobs.push(TranscriptEmbeddingJobRow {
                    job_id: uuid::Uuid::now_v7().to_string(),
                    tenant: msg.tenant.clone(),
                    message_block_id: msg.message_block_id.clone(),
                    provider: provider.clone(),
                    status: "pending".to_string(),
                    attempt_count: 0,
                    last_error: None,
                    available_at: now.clone(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                });
            }
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

        Ok(to_insert.len())
    }

    // The transcript READ methods (get_by_session,
    // get_by_session_paged, list_transcript_sessions, fetch_by_ids,
    // context_window_for_block, anchor_session_candidates,
    // recent_conversation_messages, bm25_transcript_candidates) all
    // lived here historically. Reads moved to DuckDbQuery — see
    // `src/storage/duckdb_query/transcripts.rs` for the canonical
    // implementations and their tests. This file keeps only the
    // WRITE half (create_conversation_message,
    // create_conversation_messages, semantic_search_transcripts and
    // the embedding-job helpers below).
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
    // The canonical read tests live in
    // `src/storage/duckdb_query/transcripts.rs::tests`, which seed
    // via `LanceStore::create_conversation_message` and assert the
    // read shape via the DuckDB-extension path.

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
        // `query_conversation_messages` helper directly because the
        // session-scoped reader (`get_conversation_messages_by_session`)
        // moved to `DuckDbQuery`.
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
