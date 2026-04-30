//! Transcript-archive repository methods on `DuckDbRepository`.
//!
//! Split out from `duckdb.rs` once the transcript surface grew to multiple
//! methods. The trait implementation continues to use `DuckDbRepository`'s
//! private fields (`self.conn()`, etc.) — extending the same struct via
//! a separate `impl DuckDbRepository` block in this file.

use std::collections::HashMap;

use duckdb::params;

use super::duckdb::{current_timestamp, DuckDbRepository, StorageError};
use crate::domain::ConversationMessage;

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
            // TODO(transcripts task 8): thread the configured embedding
            // provider id through the repo (e.g. via a `with_embedding_job_provider`
            // builder set in `app.rs`) instead of hardcoding the default.
            let provider = "embedanything";
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
