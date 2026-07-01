//! `TranscriptStore` for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** `conversation_messages` is the append-heavy OLAP
//! side (`ReplacingMergeTree(row_version)`). `semantic_search_transcripts`
//! mirrors the capsule ANN — `cosineDistance` over
//! `conversation_message_embeddings`, chunk-collapsed by `message_block_id`,
//! then hydrated; `bm25_transcript_candidates` is the same coarse substring
//! lexical channel. Embed-eligible writes enqueue a `transcript_embedding_jobs`
//! row with an empty provider (the CH transcript-job provider is deferred, §10).

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, enum_to_str, now_version, opt};
use crate::domain::conversation_message::{BlockType, ConversationMessage, MessageRole};
use crate::storage::types::{ContextWindow, StorageError, TranscriptSessionSummary};
use crate::storage::TranscriptStore;

fn role_from(s: &str) -> MessageRole {
    match s {
        "assistant" => MessageRole::Assistant,
        "system" => MessageRole::System,
        _ => MessageRole::User,
    }
}

fn block_from(s: &str) -> BlockType {
    match s {
        "tool_use" => BlockType::ToolUse,
        "tool_result" => BlockType::ToolResult,
        "thinking" => BlockType::Thinking,
        _ => BlockType::Text,
    }
}

#[derive(Row, Serialize, Deserialize)]
struct ChMsgRow {
    message_block_id: String,
    session_id: String,
    tenant: String,
    caller_agent: String,
    transcript_path: String,
    line_number: u64,
    block_index: u32,
    message_uuid: String,
    role: String,
    block_type: String,
    content: String,
    tool_name: String,
    tool_use_id: String,
    embed_eligible: u8,
    created_at: String,
    meta_json: String,
    row_version: u64,
}

impl ChMsgRow {
    fn from_message(m: &ConversationMessage) -> Self {
        Self {
            message_block_id: m.message_block_id.clone(),
            session_id: m.session_id.clone().unwrap_or_default(),
            tenant: m.tenant.clone(),
            caller_agent: m.caller_agent.clone(),
            transcript_path: m.transcript_path.clone(),
            line_number: m.line_number,
            block_index: m.block_index,
            message_uuid: m.message_uuid.clone().unwrap_or_default(),
            role: enum_to_str(&m.role),
            block_type: enum_to_str(&m.block_type),
            content: m.content.clone(),
            tool_name: m.tool_name.clone().unwrap_or_default(),
            tool_use_id: m.tool_use_id.clone().unwrap_or_default(),
            embed_eligible: m.embed_eligible as u8,
            created_at: m.created_at.clone(),
            meta_json: m.meta_json.clone().unwrap_or_default(),
            row_version: now_version(),
        }
    }

    fn into_message(self) -> ConversationMessage {
        ConversationMessage {
            message_block_id: self.message_block_id,
            session_id: opt(self.session_id),
            tenant: self.tenant,
            caller_agent: self.caller_agent,
            transcript_path: self.transcript_path,
            line_number: self.line_number,
            block_index: self.block_index,
            message_uuid: opt(self.message_uuid),
            role: role_from(&self.role),
            block_type: block_from(&self.block_type),
            content: self.content,
            tool_name: opt(self.tool_name),
            tool_use_id: opt(self.tool_use_id),
            embed_eligible: self.embed_eligible != 0,
            created_at: self.created_at,
            meta_json: opt(self.meta_json),
        }
    }
}

#[derive(Row, Serialize, Deserialize)]
struct ChTranscriptJobRow {
    job_id: String,
    tenant: String,
    message_block_id: String,
    provider: String,
    status: String,
    attempt_count: i64,
    last_error: String,
    available_at: String,
    created_at: String,
    updated_at: String,
    row_version: u64,
}

impl ClickHouseBackend {
    async fn ch_write_messages(&self, msgs: &[ConversationMessage]) -> Result<usize, StorageError> {
        if msgs.is_empty() {
            return Ok(0);
        }
        let mut insert = self
            .client
            .insert::<ChMsgRow>("conversation_messages")
            .await
            .map_err(ch_err)?;
        for m in msgs {
            insert
                .write(&ChMsgRow::from_message(m))
                .await
                .map_err(ch_err)?;
        }
        insert.end().await.map_err(ch_err)?;
        // Enqueue embed-eligible blocks (provider deferred — §10).
        let jobs: Vec<&ConversationMessage> = msgs.iter().filter(|m| m.embed_eligible).collect();
        if !jobs.is_empty() {
            let mut ji = self
                .client
                .insert::<ChTranscriptJobRow>("transcript_embedding_jobs")
                .await
                .map_err(ch_err)?;
            for m in jobs {
                let now = m.created_at.clone();
                ji.write(&ChTranscriptJobRow {
                    job_id: uuid::Uuid::now_v7().to_string(),
                    tenant: m.tenant.clone(),
                    message_block_id: m.message_block_id.clone(),
                    provider: String::new(),
                    status: "pending".to_owned(),
                    attempt_count: 0,
                    last_error: String::new(),
                    available_at: now.clone(),
                    created_at: now.clone(),
                    updated_at: now,
                    row_version: now_version(),
                })
                .await
                .map_err(ch_err)?;
            }
            ji.end().await.map_err(ch_err)?;
        }
        Ok(msgs.len())
    }

    async fn ch_messages(
        &self,
        sql: &str,
        binds: &[&str],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let mut q = self.client.query(sql);
        for b in binds {
            q = q.bind(*b);
        }
        let rows = q.fetch_all::<ChMsgRow>().await.map_err(ch_err)?;
        Ok(rows.into_iter().map(ChMsgRow::into_message).collect())
    }
}

#[async_trait]
impl TranscriptStore for ClickHouseBackend {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        self.ch_write_messages(std::slice::from_ref(msg)).await?;
        Ok(())
    }

    async fn create_conversation_messages(
        &self,
        msgs: &[ConversationMessage],
    ) -> Result<usize, StorageError> {
        self.ch_write_messages(msgs).await
    }

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.ch_messages(
            "SELECT ?fields FROM conversation_messages FINAL \
             WHERE tenant = ? AND session_id = ? \
             ORDER BY created_at ASC, line_number ASC, block_index ASC",
            &[tenant, session_id],
        )
        .await
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
        // Apply the time/role/block_type filters and the composite
        // `(created_at, line_number, block_index)` resume cursor, mirroring the
        // lance + postgres backends. String values bind via `?`; the integer
        // cursor components and the LIMIT are inlined as literals — ClickHouse
        // rejects a bound (string) parameter in a `LIMIT`, and inlining the
        // i64/usize (never user free-text) avoids the int-column-vs-string-bind
        // comparison mismatch. The half-open time window is `>= since AND
        // < until`, same as the other backends.
        let mut sql = String::from(
            "SELECT ?fields FROM conversation_messages FINAL \
             WHERE tenant = ? AND session_id = ?",
        );
        let mut binds: Vec<String> = vec![tenant.to_string(), session_id.to_string()];
        if let Some(s) = since {
            sql.push_str(" AND created_at >= ?");
            binds.push(s.to_string());
        }
        if let Some(u) = until {
            sql.push_str(" AND created_at < ?");
            binds.push(u.to_string());
        }
        if let Some(r) = role {
            sql.push_str(" AND role = ?");
            binds.push(r.to_string());
        }
        if let Some(bt) = block_type {
            sql.push_str(" AND block_type = ?");
            binds.push(bt.to_string());
        }
        if let Some((ca, line, block)) = cursor {
            sql.push_str(&format!(
                " AND (created_at > ? \
                 OR (created_at = ? AND line_number > {line}) \
                 OR (created_at = ? AND line_number = {line} AND block_index > {block}))"
            ));
            binds.push(ca.to_string());
            binds.push(ca.to_string());
            binds.push(ca.to_string());
        }
        sql.push_str(&format!(
            " ORDER BY created_at ASC, line_number ASC, block_index ASC LIMIT {}",
            limit + 1
        ));

        let bind_refs: Vec<&str> = binds.iter().map(String::as_str).collect();
        let mut rows = self.ch_messages(&sql, &bind_refs).await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, has_more))
    }

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT session_id, toInt64(count()) AS block_count, \
                 min(created_at) AS first_at, max(created_at) AS last_at, \
                 any(caller_agent) AS caller_agent \
                 FROM conversation_messages FINAL \
                 WHERE tenant = ? AND session_id != '' \
                 GROUP BY session_id ORDER BY last_at DESC",
            )
            .bind(tenant)
            .fetch_all::<(String, i64, String, String, String)>()
            .await
            .map_err(ch_err)?;
        Ok(rows
            .into_iter()
            .map(
                |(session_id, block_count, first_at, last_at, caller_agent)| {
                    TranscriptSessionSummary {
                        session_id,
                        block_count,
                        first_at,
                        last_at,
                        caller_agent: opt(caller_agent),
                    }
                },
            )
            .collect())
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
        // Cross-session range scan (`session_id != ''`, half-open
        // [time_from, time_to)), with the same composite cursor + role /
        // block_type filters as the per-session paged read. Mirrors the postgres
        // backend. String values bind via `?`; the integer cursor components and
        // the LIMIT are inlined (ClickHouse rejects a bound LIMIT param).
        let mut sql = String::from(
            "SELECT ?fields FROM conversation_messages FINAL \
             WHERE tenant = ? AND session_id != ''",
        );
        let mut binds: Vec<String> = vec![tenant.to_string()];
        if let Some(f) = time_from {
            sql.push_str(" AND created_at >= ?");
            binds.push(f.to_string());
        }
        if let Some(t) = time_to {
            sql.push_str(" AND created_at < ?");
            binds.push(t.to_string());
        }
        if let Some(r) = role {
            sql.push_str(" AND role = ?");
            binds.push(r.to_string());
        }
        if let Some(bt) = block_type {
            sql.push_str(" AND block_type = ?");
            binds.push(bt.to_string());
        }
        if let Some((ca, line, block)) = cursor {
            sql.push_str(&format!(
                " AND (created_at > ? \
                 OR (created_at = ? AND line_number > {line}) \
                 OR (created_at = ? AND line_number = {line} AND block_index > {block}))"
            ));
            binds.push(ca.to_string());
            binds.push(ca.to_string());
            binds.push(ca.to_string());
        }
        sql.push_str(&format!(
            " ORDER BY created_at ASC, line_number ASC, block_index ASC LIMIT {}",
            limit + 1
        ));

        let bind_refs: Vec<&str> = binds.iter().map(String::as_str).collect();
        let mut rows = self.ch_messages(&sql, &bind_refs).await?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, has_more))
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let in_list = ids
            .iter()
            .map(|i| format!("'{}'", i.replace('\'', "\\'")))
            .collect::<Vec<_>>()
            .join(", ");
        self.ch_messages(
            &format!(
                "SELECT ?fields FROM conversation_messages FINAL \
                 WHERE tenant = ? AND message_block_id IN ({in_list})"
            ),
            &[tenant],
        )
        .await
    }

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        _include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        let primary = self
            .ch_messages(
                "SELECT ?fields FROM conversation_messages FINAL \
                 WHERE tenant = ? AND message_block_id = ? LIMIT 1",
                &[tenant, primary_id],
            )
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                StorageError::InvalidInput(format!("conversation block {primary_id} not found"))
            })?;
        let sid = primary.session_id.clone().unwrap_or_default();
        let before = self
            .ch_messages(
                "SELECT ?fields FROM conversation_messages FINAL \
                 WHERE tenant = ? AND session_id = ? AND created_at < ? \
                 ORDER BY created_at DESC LIMIT ?",
                &[tenant, &sid, &primary.created_at, &k_before.to_string()],
            )
            .await?;
        let after = self
            .ch_messages(
                "SELECT ?fields FROM conversation_messages FINAL \
                 WHERE tenant = ? AND session_id = ? AND created_at > ? \
                 ORDER BY created_at ASC LIMIT ?",
                &[tenant, &sid, &primary.created_at, &k_after.to_string()],
            )
            .await?;
        Ok(ContextWindow {
            primary,
            before: before.into_iter().rev().collect(),
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
        let ids = self
            .client
            .query(
                "SELECT message_block_id FROM conversation_messages FINAL \
                 WHERE tenant = ? AND session_id = ? \
                 ORDER BY created_at DESC LIMIT ?",
            )
            .bind(tenant)
            .bind(session_id)
            .bind(k as u64)
            .fetch_all::<String>()
            .await
            .map_err(ch_err)?;
        Ok(ids)
    }

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        self.ch_messages(
            "SELECT ?fields FROM conversation_messages FINAL \
             WHERE tenant = ? ORDER BY created_at DESC LIMIT ?",
            &[tenant, &limit.to_string()],
        )
        .await
    }

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        // Coarse substring lexical channel (no BM25; CJK weak) — §4(e).
        self.ch_messages(
            "SELECT ?fields FROM conversation_messages FINAL \
             WHERE tenant = ? AND embed_eligible = 1 \
             AND positionCaseInsensitiveUTF8(content, ?) > 0 \
             ORDER BY created_at DESC LIMIT ?",
            &[tenant, query, &k.to_string()],
        )
        .await
    }

    async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        oversample: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        // cosineDistance over conversation_message_embeddings, chunk-collapse
        // (GROUP BY message_block_id min(dist)), then hydrate. Mirrors the
        // capsule ANN in search.rs. similarity = 1 - dist (normalized vectors).
        let q = query_embedding.to_vec();
        let hits = self
            .client
            .query(
                "SELECT message_block_id, min(cosineDistance(embedding, ?)) AS dist \
                 FROM conversation_message_embeddings FINAL \
                 WHERE tenant = ? GROUP BY message_block_id ORDER BY dist ASC LIMIT ?",
            )
            .bind(q)
            .bind(tenant)
            .bind(oversample as u64)
            .fetch_all::<(String, f32)>()
            .await
            .map_err(ch_err)?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
        let msgs = self
            .fetch_conversation_messages_by_ids(tenant, &ids)
            .await?;
        let dist: std::collections::HashMap<&str, f32> =
            hits.iter().map(|(id, d)| (id.as_str(), *d)).collect();
        let mut out: Vec<(ConversationMessage, f32)> = msgs
            .into_iter()
            .filter(|m| m.embed_eligible)
            .map(|m| {
                let d = dist
                    .get(m.message_block_id.as_str())
                    .copied()
                    .unwrap_or(2.0);
                (m, 1.0 - d)
            })
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }
}
