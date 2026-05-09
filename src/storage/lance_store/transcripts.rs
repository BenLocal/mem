//! Transcript pipeline (parallel to memories): conversation_messages
//! reads/writes, transcript_embedding_jobs queue, and
//! conversation_message_embeddings upsert/delete. All inherent on
//! LanceStore.

use arrow_array::RecordBatch;
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    conversation_message_embedding_to_record_batch, conversation_message_to_record_batch,
    decode_embedding_blob, ensure_conversation_message_embeddings_table, lancedb_err,
    record_batch_to_conversation_messages, record_batch_to_transcript_embedding_job_rows,
    sort_messages_chronological_asc, sql_quote, transcript_embedding_job_row_to_record_batch,
    LanceStore, TranscriptEmbeddingJobRow,
};
use crate::domain::{BlockType, ConversationMessage};
use crate::storage::types::{
    ClaimedTranscriptEmbeddingJob, ContextWindow, StorageError, TranscriptSessionSummary,
};

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
        let max_r = i64::from(max_retries);
        let filter = format!(
            "available_at <= {} AND (status = 'pending' OR (status = 'failed' AND attempt_count < {}))",
            sql_quote(now),
            max_r,
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
        let vector = decode_embedding_blob(embedding_blob, embedding_dim as usize)?;

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
        // enqueue, matching the DuckDB-as-storage contract.
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
        let batch = conversation_message_to_record_batch(msg)?;
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

    pub async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let mut msgs = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {}",
                sql_quote(tenant),
                sql_quote(session_id),
            ))
            .await?;
        sort_messages_chronological_asc(&mut msgs);
        Ok(msgs)
    }

    pub async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        // LanceDB doesn't support tuple-comparison cursors; fetch all
        // matching rows for (tenant, session_id) [+ since/until] then
        // apply cursor + sort + limit in Rust. Acceptable because a
        // single session is bounded by transcript length (typically
        // <10K rows even for long sessions).
        let mut filter = format!(
            "tenant = {} AND session_id = {}",
            sql_quote(tenant),
            sql_quote(session_id),
        );
        if let Some(s) = since {
            filter.push_str(&format!(" AND created_at >= {}", sql_quote(s)));
        }
        if let Some(u) = until {
            filter.push_str(&format!(" AND created_at < {}", sql_quote(u)));
        }
        let mut msgs = self.query_conversation_messages(filter).await?;
        if let Some((cur_at, cur_line, cur_idx)) = cursor {
            msgs.retain(|m| {
                let cmp_at = m.created_at.as_str().cmp(cur_at);
                if cmp_at != std::cmp::Ordering::Equal {
                    return cmp_at == std::cmp::Ordering::Greater;
                }
                let m_line = m.line_number as i64;
                if m_line != cur_line {
                    return m_line > cur_line;
                }
                (m.block_index as i64) > cur_idx
            });
        }
        sort_messages_chronological_asc(&mut msgs);
        let has_more = msgs.len() > limit;
        if has_more {
            msgs.truncate(limit);
        }
        Ok((msgs, has_more))
    }

    pub async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        // GROUP BY is not exposed in LanceDB's QueryBase, so we pull all
        // rows for tenant (skipping null session_ids) and aggregate in
        // Rust. Tenant transcript volume is bounded by the on-disk
        // archive size; for the local-first tenant=local case this is
        // small enough.
        let msgs = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id IS NOT NULL",
                sql_quote(tenant),
            ))
            .await?;
        use std::collections::HashMap;
        struct Acc {
            block_count: i64,
            first_at: String,
            last_at: String,
            caller_agent: Option<String>,
        }
        let mut by_session: HashMap<String, Acc> = HashMap::new();
        for m in &msgs {
            let Some(sid) = &m.session_id else { continue };
            let entry = by_session.entry(sid.clone()).or_insert_with(|| Acc {
                block_count: 0,
                first_at: m.created_at.clone(),
                last_at: m.created_at.clone(),
                caller_agent: Some(m.caller_agent.clone()),
            });
            entry.block_count += 1;
            if m.created_at < entry.first_at {
                entry.first_at = m.created_at.clone();
            }
            if m.created_at > entry.last_at {
                entry.last_at = m.created_at.clone();
                // max(caller_agent) — DuckDB picks the lexicographically
                // largest; we mirror by tracking max-string seen.
            }
            if let Some(existing) = &entry.caller_agent {
                if &m.caller_agent > existing {
                    entry.caller_agent = Some(m.caller_agent.clone());
                }
            } else {
                entry.caller_agent = Some(m.caller_agent.clone());
            }
        }
        let mut out: Vec<TranscriptSessionSummary> = by_session
            .into_iter()
            .map(|(sid, a)| TranscriptSessionSummary {
                session_id: sid,
                block_count: a.block_count,
                first_at: a.first_at,
                last_at: a.last_at,
                caller_agent: a.caller_agent,
            })
            .collect();
        // ORDER BY last_at DESC.
        out.sort_by(|a, b| b.last_at.cmp(&a.last_at));
        Ok(out)
    }

    pub async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let in_list = ids
            .iter()
            .map(|s| sql_quote(s))
            .collect::<Vec<_>>()
            .join(",");
        let msgs = self
            .query_conversation_messages(format!(
                "tenant = {} AND message_block_id IN ({})",
                sql_quote(tenant),
                in_list,
            ))
            .await?;
        // Re-order to match input slice (skip missing ids silently).
        use std::collections::HashMap;
        let mut by_id: HashMap<String, ConversationMessage> = msgs
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

    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        // Step 1: fetch the primary block.
        let primary_vec = self
            .query_conversation_messages(format!(
                "tenant = {} AND message_block_id = {}",
                sql_quote(tenant),
                sql_quote(primary_id),
            ))
            .await?;
        let primary = primary_vec
            .into_iter()
            .next()
            .ok_or(StorageError::NotFound("transcript primary block"))?;
        let session_id = match primary.session_id.clone() {
            Some(s) => s,
            None => {
                // No session → no neighbors by definition.
                return Ok(ContextWindow {
                    primary,
                    before: vec![],
                    after: vec![],
                });
            }
        };

        // Step 2: pull all messages for (tenant, session_id) — same
        // bounded-by-session size argument as paged. Filter + sort in
        // Rust because LanceDB has no SQL CASE/tuple comparison.
        let mut all = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {}",
                sql_quote(tenant),
                sql_quote(&session_id),
            ))
            .await?;
        if !include_tool_blocks {
            // Primary itself stays regardless; before/after filter applies
            // to neighbors only — easiest to filter neighbors after the
            // partition step.
        }
        sort_messages_chronological_asc(&mut all);
        let primary_key = (
            primary.created_at.clone(),
            primary.line_number as i64,
            primary.block_index as i64,
        );
        let mut before_buf: Vec<ConversationMessage> = Vec::new();
        let mut after_buf: Vec<ConversationMessage> = Vec::new();
        for m in all {
            if m.message_block_id == primary.message_block_id {
                continue;
            }
            let m_key = (
                m.created_at.clone(),
                m.line_number as i64,
                m.block_index as i64,
            );
            let cmp = m_key.cmp(&primary_key);
            if !include_tool_blocks
                && !matches!(m.block_type, BlockType::Text | BlockType::Thinking)
            {
                continue;
            }
            if cmp == std::cmp::Ordering::Less {
                before_buf.push(m);
            } else if cmp == std::cmp::Ordering::Greater {
                after_buf.push(m);
            }
        }
        // before is currently ASC; take the last k_before rows (closest
        // to primary), keeping ASC order for the caller's convenience.
        if before_buf.len() > k_before {
            let drop = before_buf.len() - k_before;
            before_buf.drain(0..drop);
        }
        if after_buf.len() > k_after {
            after_buf.truncate(k_after);
        }
        Ok(ContextWindow {
            primary,
            before: before_buf,
            after: after_buf,
        })
    }

    pub async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        if k == 0 {
            return Ok(vec![]);
        }
        let mut msgs = self
            .query_conversation_messages(format!(
                "tenant = {} AND session_id = {} AND embed_eligible = true",
                sql_quote(tenant),
                sql_quote(session_id),
            ))
            .await?;
        // ORDER BY created_at DESC, take k.
        msgs.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        msgs.truncate(k);
        Ok(msgs.into_iter().map(|m| m.message_block_id).collect())
    }

    pub async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let mut msgs = self
            .query_conversation_messages(format!(
                "tenant = {} AND embed_eligible = true",
                sql_quote(tenant),
            ))
            .await?;
        // ORDER BY created_at DESC, line_number DESC, block_index DESC.
        msgs.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.line_number.cmp(&a.line_number))
                .then_with(|| b.block_index.cmp(&a.block_index))
        });
        msgs.truncate(limit);
        Ok(msgs)
    }

    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(vec![]);
        }
        let table = self
            .conn
            .open_table("conversation_messages")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // FTS index built at open() time (see `ensure_fts_index`).
        // Oversample so the embed_eligible drop doesn't immediately
        // starve the result (mirrors DuckDB tantivy oversample).
        let oversample = k.saturating_mul(2).max(k);
        let fts_query = lancedb::index::scalar::FullTextSearchQuery::new(query.to_string());
        let stream = table
            .query()
            .full_text_search(fts_query)
            .only_if(format!(
                "tenant = {} AND embed_eligible = true",
                sql_quote(tenant),
            ))
            .limit(oversample)
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
        out.truncate(k);
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
        }
    }

    /// recent_conversation_messages → bm25_transcript_candidates.
    #[tokio::test]
    pub async fn lancedb_transcript_repository_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();
        // Required because `create_conversation_message` now enqueues
        // a transcript_embedding_jobs row when the message is
        // embed_eligible — the enqueue stamps `provider`, which must
        // be configured up front.
        repo.set_transcript_job_provider("fake-test");

        // 4 blocks, 2 sessions × 2 tenants.
        let m1 = msg(
            "blk_1",
            "tenant-a",
            Some("sess_a"),
            10,
            0,
            BlockType::Text,
            "DuckDB single mutex serializes writes",
            "00000001778000000010",
        );
        let m2 = msg(
            "blk_2",
            "tenant-a",
            Some("sess_a"),
            12,
            0,
            BlockType::ToolUse,
            "{\"tool\":\"Bash\"}",
            "00000001778000000020",
        );
        let m3 = msg(
            "blk_3",
            "tenant-a",
            Some("sess_a"),
            14,
            0,
            BlockType::Thinking,
            "let's switch to LanceDB native FTS",
            "00000001778000000030",
        );
        let m4 = msg(
            "blk_4",
            "tenant-b",
            Some("sess_b"),
            5,
            0,
            BlockType::Text,
            "another tenant transcript",
            "00000001778000000040",
        );

        for m in [&m1, &m2, &m3, &m4] {
            repo.create_conversation_message(m).await.unwrap();
        }
        // Idempotent re-create — same (transcript_path, line, idx) is a no-op.
        repo.create_conversation_message(&m1).await.unwrap();

        // get_by_session: 3 blocks for sess_a, ordered ASC by
        // (created_at, line_number, block_index).
        let sess_a = repo
            .get_conversation_messages_by_session("tenant-a", "sess_a")
            .await
            .unwrap();
        assert_eq!(sess_a.len(), 3, "got {sess_a:?}");
        assert_eq!(sess_a[0].message_block_id, "blk_1");
        assert_eq!(sess_a[1].message_block_id, "blk_2");
        assert_eq!(sess_a[2].message_block_id, "blk_3");

        // list_sessions: tenant-a has 1, tenant-b has 1.
        let summaries_a = repo.list_transcript_sessions("tenant-a").await.unwrap();
        assert_eq!(summaries_a.len(), 1);
        assert_eq!(summaries_a[0].session_id, "sess_a");
        assert_eq!(summaries_a[0].block_count, 3);
        assert_eq!(summaries_a[0].first_at, "00000001778000000010");
        assert_eq!(summaries_a[0].last_at, "00000001778000000030");
        let summaries_b = repo.list_transcript_sessions("tenant-b").await.unwrap();
        assert_eq!(summaries_b.len(), 1);
        assert_eq!(summaries_b[0].session_id, "sess_b");

        // fetch_by_ids: input-order preserving; missing ids dropped.
        let by_ids = repo
            .fetch_conversation_messages_by_ids(
                "tenant-a",
                &["blk_3".into(), "blk_1".into(), "missing".into()],
            )
            .await
            .unwrap();
        assert_eq!(by_ids.len(), 2);
        assert_eq!(by_ids[0].message_block_id, "blk_3");
        assert_eq!(by_ids[1].message_block_id, "blk_1");

        // context_window for blk_2 (the tool_use middle block).
        // include_tool_blocks=true → before=[blk_1], after=[blk_3].
        let win_with = repo
            .context_window_for_block("tenant-a", "blk_2", 5, 5, true)
            .await
            .unwrap();
        assert_eq!(win_with.primary.message_block_id, "blk_2");
        assert_eq!(win_with.before.len(), 1);
        assert_eq!(win_with.before[0].message_block_id, "blk_1");
        assert_eq!(win_with.after.len(), 1);
        assert_eq!(win_with.after[0].message_block_id, "blk_3");

        // include_tool_blocks=false on blk_2 itself: primary still
        // returned, neighbors only contain text/thinking. blk_1 (text)
        // before, blk_3 (thinking) after — both eligible.
        let win_no = repo
            .context_window_for_block("tenant-a", "blk_2", 5, 5, false)
            .await
            .unwrap();
        assert_eq!(win_no.primary.message_block_id, "blk_2");
        assert_eq!(win_no.before.len(), 1);
        assert_eq!(win_no.after.len(), 1);

        // context_window with k_before/k_after = 0 → empty windows.
        let win_zero = repo
            .context_window_for_block("tenant-a", "blk_2", 0, 0, true)
            .await
            .unwrap();
        assert!(win_zero.before.is_empty());
        assert!(win_zero.after.is_empty());

        // context_window for unknown block → NotFound.
        let nf = repo
            .context_window_for_block("tenant-a", "does-not-exist", 5, 5, true)
            .await
            .unwrap_err();
        assert!(matches!(
            nf,
            StorageError::NotFound("transcript primary block")
        ));

        // anchor_session_candidates: embed_eligible only, DESC by created_at.
        // sess_a has 2 eligible blocks (blk_1 text, blk_3 thinking) —
        // blk_2 is tool_use → ineligible.
        let anchors = repo
            .anchor_session_candidates("tenant-a", "sess_a", 5)
            .await
            .unwrap();
        assert_eq!(anchors.len(), 2);
        assert_eq!(anchors[0], "blk_3"); // newest first
        assert_eq!(anchors[1], "blk_1");
        // k=0 → empty.
        let z = repo
            .anchor_session_candidates("tenant-a", "sess_a", 0)
            .await
            .unwrap();
        assert!(z.is_empty());

        // recent_conversation_messages: tenant-a embed_eligible only.
        let recent = repo
            .recent_conversation_messages("tenant-a", 10)
            .await
            .unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].message_block_id, "blk_3");
        assert_eq!(recent[1].message_block_id, "blk_1");

        // bm25_transcript_candidates: lazy FTS index, embed_eligible filter.
        // "DuckDB" → matches blk_1 (text). "LanceDB" → blk_3 (thinking).
        // tenant-b's matching block (if any) should be filtered out.
        let bm25_duck = repo
            .bm25_transcript_candidates("tenant-a", "DuckDB", 5)
            .await
            .unwrap();
        let duck_ids: Vec<&str> = bm25_duck
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert!(duck_ids.contains(&"blk_1"), "got {duck_ids:?}");
        let bm25_lance = repo
            .bm25_transcript_candidates("tenant-a", "LanceDB", 5)
            .await
            .unwrap();
        let lance_ids: Vec<&str> = bm25_lance
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert!(lance_ids.contains(&"blk_3"), "got {lance_ids:?}");

        // empty query / k=0 short-circuits.
        let empty1 = repo
            .bm25_transcript_candidates("tenant-a", "", 5)
            .await
            .unwrap();
        assert!(empty1.is_empty());
        let empty2 = repo
            .bm25_transcript_candidates("tenant-a", "anything", 0)
            .await
            .unwrap();
        assert!(empty2.is_empty());

        // get_paged: cursor + has_more.
        let (page1, more1) = repo
            .get_conversation_messages_by_session_paged("tenant-a", "sess_a", None, None, None, 2)
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert!(more1);
        assert_eq!(page1[0].message_block_id, "blk_1");
        assert_eq!(page1[1].message_block_id, "blk_2");

        let last = page1.last().unwrap();
        let (page2, more2) = repo
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                None,
                None,
                Some((
                    last.created_at.as_str(),
                    last.line_number as i64,
                    last.block_index as i64,
                )),
                10,
            )
            .await
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert!(!more2);
        assert_eq!(page2[0].message_block_id, "blk_3");

        // since/until window narrows the query.
        let (windowed, _) = repo
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                Some("00000001778000000020"),
                Some("00000001778000000031"),
                None,
                10,
            )
            .await
            .unwrap();
        let win_ids: Vec<&str> = windowed
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert_eq!(win_ids, vec!["blk_2", "blk_3"]);
    }
}
