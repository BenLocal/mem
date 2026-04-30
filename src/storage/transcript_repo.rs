//! Transcript-archive repository methods on `DuckDbRepository`.
//!
//! Split out from `duckdb.rs` once the transcript surface grew to multiple
//! methods. The trait implementation continues to use `DuckDbRepository`'s
//! private fields (`self.conn()`, etc.) — extending the same struct via
//! a separate `impl DuckDbRepository` block in this file.

use std::collections::HashMap;

use duckdb::{params, OptionalExt};

use super::duckdb::{current_timestamp, DuckDbRepository, StorageError};
use super::vector_index::TranscriptEmbeddingRowSource;
use crate::domain::ConversationMessage;

/// Row claimed by the transcript embedding worker (`status = processing`).
///
/// Mirrors `ClaimedEmbeddingJob` for the memories side, with `memory_id`
/// renamed to `message_block_id` and `target_content_hash` dropped (transcript
/// blocks are immutable on insert, so the hash is implicit in the row id).
#[derive(Debug, Clone)]
pub struct ClaimedTranscriptEmbeddingJob {
    pub job_id: String,
    pub tenant: String,
    pub message_block_id: String,
    pub provider: String,
    pub attempt_count: i64,
}

impl DuckDbRepository {
    /// Inserts a single conversation transcript block, idempotent on the
    /// `(transcript_path, line_number, block_index)` unique key, and—when the
    /// row is actually written and `embed_eligible == true`—enqueues a single
    /// `pending` row in `transcript_embedding_jobs`.
    ///
    /// Concurrency: both INSERTs run inside a single `Arc<Mutex<Connection>>`
    /// acquisition (via `self.conn()`); the lock is never released between
    /// them, so a "row written but job not enqueued" partial state is
    /// impossible from concurrent callers.
    ///
    /// Idempotency: implemented via DuckDB's `INSERT OR IGNORE`, which swallows
    /// the unique-constraint violation and reports `affected_rows = 0` on the
    /// duplicate. We deliberately use this rather than `ON CONFLICT (...) DO
    /// NOTHING` to match the in-tree convention already used by
    /// `seed_memory_embedding_for_test`.
    pub async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;

        // Step 1: insert the message row. Duplicate (transcript_path,
        // line_number, block_index) is silently ignored by `insert or ignore`,
        // yielding `inserted == 0`.
        let inserted = conn.execute(
            "insert or ignore into conversation_messages (
                message_block_id, session_id, tenant, caller_agent, transcript_path,
                line_number, block_index, message_uuid, role, block_type, content,
                tool_name, tool_use_id, embed_eligible, created_at
            ) values (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15
            )",
            params![
                msg.message_block_id,
                msg.session_id,
                msg.tenant,
                msg.caller_agent,
                msg.transcript_path,
                msg.line_number as i64,
                msg.block_index as i64,
                msg.message_uuid,
                msg.role.as_db_str(),
                msg.block_type.as_db_str(),
                msg.content,
                msg.tool_name,
                msg.tool_use_id,
                msg.embed_eligible,
                msg.created_at,
            ],
        )?;

        // Step 2: enqueue an embedding job iff a NEW row was written AND it is
        // embed-eligible. Idempotent re-inserts (inserted == 0) skip enqueue,
        // matching the deduplication contract of `try_enqueue_embedding_job`.
        if inserted == 1 && msg.embed_eligible {
            let job_id = uuid::Uuid::now_v7().to_string();
            let now = current_timestamp();
            // Provider id is configured once at startup via
            // `set_transcript_job_provider` in `app.rs`. Failing loudly here is
            // preferable to silently substituting a default that would later
            // mismatch the worker's `job_provider_id()` and dead-letter every
            // job for the wrong reason.
            let provider = self
                .transcript_job_provider()
                .ok_or(StorageError::InvalidData(
                    "transcript embedding job provider not configured; \
                 call DuckDbRepository::set_transcript_job_provider during startup",
                ))?;
            conn.execute(
                "insert into transcript_embedding_jobs (
                    job_id, tenant, message_block_id, provider,
                    status, attempt_count, last_error,
                    available_at, created_at, updated_at
                ) values (
                    ?1, ?2, ?3, ?4,
                    'pending', 0, null,
                    ?5, ?6, ?7
                )",
                params![
                    job_id,
                    msg.tenant,
                    msg.message_block_id,
                    provider,
                    now,
                    now,
                    now,
                ],
            )?;
        }

        Ok(())
    }

    /// Singleton fetch by `(tenant, message_block_id)`. Used by the transcript
    /// embedding worker to materialise the row text before calling the
    /// embedder. Returns `Ok(None)` when no row matches (treated as "row
    /// disappeared after job enqueue" by the worker, which permanently fails
    /// the job).
    pub async fn get_conversation_message_by_id(
        &self,
        tenant: &str,
        message_block_id: &str,
    ) -> Result<Option<ConversationMessage>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path,
                    line_number, block_index, message_uuid, role, block_type, content,
                    tool_name, tool_use_id, embed_eligible, created_at
             from conversation_messages
             where tenant = ?1 and message_block_id = ?2",
        )?;
        let row = stmt
            .query_row(
                params![tenant, message_block_id],
                row_to_conversation_message,
            )
            .optional()?;
        Ok(row)
    }

    /// Returns all `conversation_messages` rows for the given (tenant,
    /// session_id), ordered chronologically. Ties on `created_at` break by
    /// `(line_number, block_index)` to preserve in-line block order from the
    /// source transcript.
    pub async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path,
                    line_number, block_index, message_uuid, role, block_type, content,
                    tool_name, tool_use_id, embed_eligible, created_at
             from conversation_messages
             where tenant = ?1 and session_id = ?2
             order by created_at asc, line_number asc, block_index asc",
        )?;
        let rows = stmt.query_map(params![tenant, session_id], row_to_conversation_message)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Fetches the requested `conversation_messages` rows by `message_block_id`
    /// and re-orders the result to match the input slice. Missing ids are
    /// silently dropped (the caller treats this as "row was deleted between
    /// search and fetch"). Empty input short-circuits without touching the DB.
    pub async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn()?;

        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path,
                    line_number, block_index, message_uuid, role, block_type, content,
                    tool_name, tool_use_id, embed_eligible, created_at
             from conversation_messages
             where tenant = ?1 and message_block_id in ({placeholders})"
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant.to_string())];
        for id in ids {
            params_vec.push(Box::new(id.clone()));
        }
        let params_refs: Vec<&dyn duckdb::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(&params_refs[..], row_to_conversation_message)?;
        let mut by_id: HashMap<String, ConversationMessage> = HashMap::new();
        for r in rows {
            let m = r?;
            by_id.insert(m.message_block_id.clone(), m);
        }
        let ordered: Vec<ConversationMessage> =
            ids.iter().filter_map(|id| by_id.remove(id)).collect();
        Ok(ordered)
    }

    /// Claims the next eligible transcript embedding job, moving it to
    /// `processing`. Eligible means `pending`, or `failed` with
    /// `attempt_count < max_retries` (configured retry budget). Mirror of
    /// `claim_next_embedding_job` for the transcript queue.
    pub async fn claim_next_transcript_embedding_job(
        &self,
        now: &str,
        max_retries: u32,
    ) -> Result<Option<ClaimedTranscriptEmbeddingJob>, StorageError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let max_r = i64::from(max_retries);

        let job_id: Option<String> = tx
            .query_row(
                "select job_id from transcript_embedding_jobs
                 where available_at <= ?1
                   and (
                     status = 'pending'
                     or (status = 'failed' and attempt_count < ?2)
                   )
                 order by available_at asc, created_at asc
                 limit 1",
                params![now, max_r],
                |row| row.get(0),
            )
            .optional()?;

        let Some(job_id) = job_id else {
            tx.commit()?;
            return Ok(None);
        };

        let updated = tx.execute(
            "update transcript_embedding_jobs
             set status = 'processing', updated_at = ?1
             where job_id = ?2
               and (
                 status = 'pending'
                 or (status = 'failed' and attempt_count < ?3)
               )",
            params![now, job_id, max_r],
        )?;

        if updated == 0 {
            tx.commit()?;
            return Ok(None);
        }

        let job = tx.query_row(
            "select job_id, tenant, message_block_id, provider, attempt_count
             from transcript_embedding_jobs where job_id = ?1",
            params![job_id],
            |row| {
                Ok(ClaimedTranscriptEmbeddingJob {
                    job_id: row.get(0)?,
                    tenant: row.get(1)?,
                    message_block_id: row.get(2)?,
                    provider: row.get(3)?,
                    attempt_count: row.get(4)?,
                })
            },
        )?;

        tx.commit()?;
        Ok(Some(job))
    }

    pub async fn complete_transcript_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "update transcript_embedding_jobs
             set status = 'completed', last_error = null, updated_at = ?1
             where job_id = ?2 and status = 'processing'",
            params![now, job_id],
        )?;
        Ok(())
    }

    pub async fn mark_transcript_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "update transcript_embedding_jobs set status = 'stale', updated_at = ?1 where job_id = ?2",
            params![now, job_id],
        )?;
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
        let conn = self.conn()?;
        conn.execute(
            "update transcript_embedding_jobs
             set status = 'failed',
                 attempt_count = ?1,
                 last_error = ?2,
                 available_at = ?3,
                 updated_at = ?4
             where job_id = ?5",
            params![new_attempt_count, last_error, available_at, now, job_id],
        )?;
        Ok(())
    }

    pub async fn permanently_fail_transcript_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "update transcript_embedding_jobs
             set status = 'failed',
                 attempt_count = ?1,
                 last_error = ?2,
                 updated_at = ?3
             where job_id = ?4",
            params![new_attempt_count, last_error, now, job_id],
        )?;
        Ok(())
    }

    pub async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn()?;
        let status: Option<String> = conn
            .query_row(
                "select status from transcript_embedding_jobs where job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status)
    }

    /// Replaces the conversation_message_embeddings row for `message_block_id`
    /// with the new vector. Mirror of `upsert_memory_embedding`.
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
        let conn = self.conn()?;
        conn.execute(
            "delete from conversation_message_embeddings where message_block_id = ?1",
            params![message_block_id],
        )?;
        conn.execute(
            "insert into conversation_message_embeddings (
                message_block_id, tenant, embedding_model, embedding_dim, embedding,
                content_hash, source_updated_at, created_at, updated_at
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                message_block_id,
                tenant,
                embedding_model,
                embedding_dim,
                embedding_blob,
                content_hash,
                source_updated_at,
                now,
                now,
            ],
        )?;
        Ok(())
    }
}

impl TranscriptEmbeddingRowSource for DuckDbRepository {
    fn count_total_transcript_embeddings(&self) -> Result<i64, StorageError> {
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "select count(*) from conversation_message_embeddings",
            params![],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    fn for_each_transcript_embedding(
        &self,
        _batch: usize,
        f: &mut dyn FnMut(&str, &[u8]) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select message_block_id, embedding from conversation_message_embeddings \
             order by message_block_id",
        )?;
        let mut rows = stmt.query(params![])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            f(&id, &blob)?;
        }
        Ok(())
    }
}

fn row_to_conversation_message(
    row: &duckdb::Row<'_>,
) -> Result<ConversationMessage, duckdb::Error> {
    use crate::domain::{BlockType, MessageRole};
    let role_s: String = row.get(8)?;
    let bt_s: String = row.get(9)?;
    Ok(ConversationMessage {
        message_block_id: row.get(0)?,
        session_id: row.get(1)?,
        tenant: row.get(2)?,
        caller_agent: row.get(3)?,
        transcript_path: row.get(4)?,
        line_number: row.get::<_, i64>(5)? as u64,
        block_index: row.get::<_, i64>(6)? as u32,
        message_uuid: row.get(7)?,
        role: MessageRole::from_db_str(&role_s).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(
                8,
                duckdb::types::Type::Text,
                format!("invalid role: {role_s}").into(),
            )
        })?,
        block_type: BlockType::from_db_str(&bt_s).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(
                9,
                duckdb::types::Type::Text,
                format!("invalid block_type: {bt_s}").into(),
            )
        })?,
        content: row.get(10)?,
        tool_name: row.get(11)?,
        tool_use_id: row.get(12)?,
        embed_eligible: row.get(13)?,
        created_at: row.get(14)?,
    })
}
