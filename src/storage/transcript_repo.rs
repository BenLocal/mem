//! Transcript-archive repository methods on `DuckDbRepository`.
//!
//! Split out from `duckdb.rs` once the transcript surface grew to multiple
//! methods. The trait implementation continues to use `DuckDbRepository`'s
//! private fields (`self.conn()`, etc.) — extending the same struct via
//! a separate `impl DuckDbRepository` block in this file.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use duckdb::{params, OptionalExt};
use serde::Serialize;

use super::duckdb::{DuckDbRepository, StorageError};
use super::time::current_timestamp;
use super::vector_index::TranscriptEmbeddingRowSource;
use crate::domain::ConversationMessage;

/// Aggregate row used by the admin web page's transcripts list view.
/// One per `(tenant, session_id)`. `caller_agent` is whatever
/// `max(caller_agent)` returned — typical sessions have a single agent
/// so this is unambiguous; in mixed-agent edge cases it picks one
/// deterministically rather than blocking the listing.
#[derive(Debug, Clone, Serialize)]
pub struct TranscriptSessionSummary {
    pub session_id: String,
    pub block_count: i64,
    pub first_at: String,
    pub last_at: String,
    pub caller_agent: Option<String>,
}

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

/// Result of [`DuckDbRepository::context_window_for_block`]. The
/// `primary` is the requested block; `before` and `after` are temporally
/// adjacent same-session blocks (filtered per `include_tool_blocks`).
#[derive(Debug, Clone)]
pub struct ContextWindow {
    pub primary: ConversationMessage,
    pub before: Vec<ConversationMessage>,
    pub after: Vec<ConversationMessage>,
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
        if inserted == 1 {
            if msg.embed_eligible {
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
            // Mark the transcripts FTS index dirty so the next BM25 read
            // rebuilds. Done unconditionally on a successful insert (not only
            // for embed-eligible rows) because the FTS index covers the full
            // `conversation_messages` table per the Task 2 probe outcome —
            // even ineligible rows go into the index, and the SELECT filters
            // by `embed_eligible = true` at query time.
            self.set_transcripts_fts_dirty();
        }

        Ok(())
    }

    /// BM25 lexical candidates over `conversation_messages.content`,
    /// filtered to `embed_eligible = true` rows (matching the HNSW
    /// pipeline's coverage). Returns up to `k` rows ordered by BM25
    /// score descending.
    ///
    /// The FTS index is built lazily by `ensure_transcript_fts_index_fresh`
    /// on the first read after a write.
    ///
    /// Per the Task 2 probe, the bundled DuckDB FTS extension does NOT
    /// support `where := '...'` predicate indexes, so the index covers
    /// the full table and the SELECT filters at query time.
    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(vec![]);
        }
        self.ensure_transcript_fts_index_fresh()?;

        let scored: Vec<(String, f64)> = {
            let conn = self.conn()?;
            let mut stmt = conn.prepare(
                "with scored as (
                    select message_block_id,
                           fts_main_conversation_messages.match_bm25(message_block_id, ?1, conjunctive := 0) as bm25
                    from conversation_messages
                    where tenant = ?2
                      and embed_eligible = true
                )
                 select message_block_id, bm25
                 from scored
                 where bm25 is not null
                 order by bm25 desc
                 limit ?3",
            )?;
            let rows = stmt.query_map(params![query, tenant, k as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        if scored.is_empty() {
            return Ok(vec![]);
        }

        let id_strings: Vec<String> = scored.iter().map(|(id, _)| id.clone()).collect();
        let mut hydrated = self
            .fetch_conversation_messages_by_ids(tenant, &id_strings)
            .await?;

        // Preserve BM25 rank order — `fetch_conversation_messages_by_ids`
        // already orders by input slice, but we re-sort defensively in case
        // that contract is ever relaxed.
        let rank_by_id: HashMap<&str, usize> = scored
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        hydrated.sort_by_key(|m| {
            *rank_by_id
                .get(m.message_block_id.as_str())
                .unwrap_or(&usize::MAX)
        });
        Ok(hydrated)
    }

    /// Returns up to `k` `message_block_id`s from the given anchor session
    /// (most recent first, embed-eligible only). Used by
    /// `TranscriptService::search` to ensure anchor-session blocks enter
    /// the candidate pool even if no topical (BM25/HNSW) match would have
    /// surfaced them.
    pub async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        if k == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select message_block_id \
             from conversation_messages \
             where tenant = ?1 and session_id = ?2 and embed_eligible = true \
             order by created_at desc \
             limit ?3",
        )?;
        let rows = stmt.query_map(params![tenant, session_id, k as i64], |r| {
            r.get::<_, String>(0)
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Fetch the primary block and up to `k_before` / `k_after` adjacent
    /// blocks in the same `session_id`, ordered by
    /// `(created_at, line_number, block_index)`. If `include_tool_blocks`
    /// is false, `before` and `after` only include `text` and `thinking`
    /// block types (the primary itself is always returned regardless of
    /// its type).
    ///
    /// Returns `Ok` with empty `before`/`after` if the primary has no
    /// `session_id` (NULL session). Returns
    /// `StorageError::NotFound` if the primary id doesn't exist for
    /// this tenant — this is treated as an internal-consistency event
    /// (BM25/HNSW just returned the id) and surfaces as HTTP 500, not 400.
    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        let conn = self.conn()?;

        // 1. Fetch the primary first to get its session and timestamp.
        let primary: ConversationMessage = {
            let mut stmt = conn.prepare(
                "select message_block_id, session_id, tenant, caller_agent, transcript_path, \
                        line_number, block_index, message_uuid, role, block_type, content, \
                        tool_name, tool_use_id, embed_eligible, created_at \
                 from conversation_messages \
                 where tenant = ?1 and message_block_id = ?2",
            )?;
            let mut rows =
                stmt.query_map(params![tenant, primary_id], row_to_conversation_message)?;
            match rows.next() {
                Some(Ok(m)) => m,
                Some(Err(e)) => return Err(StorageError::from(e)),
                None => {
                    return Err(StorageError::NotFound("transcript primary block"));
                }
            }
        };

        let session_id = match primary.session_id.as_deref() {
            Some(s) => s.to_string(),
            None => {
                // No session → no neighbors by definition.
                return Ok(ContextWindow {
                    primary,
                    before: vec![],
                    after: vec![],
                });
            }
        };

        // 2. Block-type filter clause built once.
        let type_filter = if include_tool_blocks {
            ""
        } else {
            "and block_type in ('text', 'thinking')"
        };

        // 3. Fetch `k_before` blocks strictly before the primary's
        //    (created_at, line_number, block_index) tuple. We use an
        //    explicit disjunction rather than DuckDB's tuple comparison
        //    to maximize compatibility across versions.
        let before_sql = format!(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path, \
                    line_number, block_index, message_uuid, role, block_type, content, \
                    tool_name, tool_use_id, embed_eligible, created_at \
             from conversation_messages \
             where tenant = ?1 \
               and session_id = ?2 \
               and (
                    created_at < ?3 \
                    or (created_at = ?3 and line_number < ?4) \
                    or (created_at = ?3 and line_number = ?4 and block_index < ?5)
               ) \
               {type_filter} \
             order by created_at desc, line_number desc, block_index desc \
             limit ?6"
        );
        let before: Vec<ConversationMessage> = {
            let mut stmt = conn.prepare(&before_sql)?;
            let rows = stmt.query_map(
                params![
                    tenant,
                    session_id,
                    primary.created_at,
                    primary.line_number as i64,
                    primary.block_index as i64,
                    k_before as i64,
                ],
                row_to_conversation_message,
            )?;
            // The query returns DESC order; reverse to ASC for caller convenience.
            let mut v: Vec<_> = rows.collect::<Result<Vec<_>, _>>()?;
            v.reverse();
            v
        };

        // 4. Fetch `k_after` blocks strictly after.
        let after_sql = format!(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path, \
                    line_number, block_index, message_uuid, role, block_type, content, \
                    tool_name, tool_use_id, embed_eligible, created_at \
             from conversation_messages \
             where tenant = ?1 \
               and session_id = ?2 \
               and (
                    created_at > ?3 \
                    or (created_at = ?3 and line_number > ?4) \
                    or (created_at = ?3 and line_number = ?4 and block_index > ?5)
               ) \
               {type_filter} \
             order by created_at asc, line_number asc, block_index asc \
             limit ?6"
        );
        let after: Vec<ConversationMessage> = {
            let mut stmt = conn.prepare(&after_sql)?;
            let rows = stmt.query_map(
                params![
                    tenant,
                    session_id,
                    primary.created_at,
                    primary.line_number as i64,
                    primary.block_index as i64,
                    k_after as i64,
                ],
                row_to_conversation_message,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        Ok(ContextWindow {
            primary,
            before,
            after,
        })
    }

    /// Rebuild the transcripts FTS index iff the dirty flag is set. Cheap
    /// when clean. Mirror of `ensure_fts_index_fresh` for memories — see
    /// that method's docs for the drop-then-create rationale.
    pub(crate) fn ensure_transcript_fts_index_fresh(&self) -> Result<(), StorageError> {
        if !self.transcripts_fts_dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let conn = self.conn()?;
        if let Err(e) = super::duckdb::rebuild_transcripts_fts(&conn) {
            self.transcripts_fts_dirty.store(true, Ordering::Release);
            return Err(e);
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

    /// Per-session summary used by the admin web page's transcripts view.
    /// One row per `(tenant, session_id)` with block count + first/last
    /// timestamp + an arbitrary representative caller_agent (the most
    /// recently seen one — sessions are typically single-agent so the
    /// `max(caller_agent)` is fine).
    pub async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select session_id,
                    count(*)             as block_count,
                    min(created_at)      as first_at,
                    max(created_at)      as last_at,
                    max(caller_agent)    as caller_agent
             from conversation_messages
             where tenant = ?1 and session_id is not null
             group by session_id
             order by last_at desc",
        )?;
        let rows = stmt.query_map(params![tenant], |row| {
            Ok(TranscriptSessionSummary {
                session_id: row.get(0)?,
                block_count: row.get(1)?,
                first_at: row.get(2)?,
                last_at: row.get(3)?,
                caller_agent: row.get(4).ok(),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
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

    /// Returns the most recent `conversation_messages` rows for `tenant`,
    /// newest first, capped at `limit`. Used by the transcript search service
    /// as the empty-query fallback (when no embedding query is supplied) and
    /// as a CLI/diagnostic listing helper.
    ///
    /// Tie-breaking on `created_at` mirrors `get_conversation_messages_by_session`
    /// (`line_number, block_index`) so equal-timestamp blocks come back in a
    /// deterministic order — but since the primary sort is DESC, ties resolve
    /// in reverse line order, which matches the "newest line wins" intent.
    pub async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select message_block_id, session_id, tenant, caller_agent, transcript_path,
                    line_number, block_index, message_uuid, role, block_type, content,
                    tool_name, tool_use_id, embed_eligible, created_at
             from conversation_messages
             where tenant = ?1 and embed_eligible = true
             order by created_at desc, line_number desc, block_index desc
             limit ?2",
        )?;
        let rows = stmt.query_map(params![tenant, limit as i64], row_to_conversation_message)?;
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
