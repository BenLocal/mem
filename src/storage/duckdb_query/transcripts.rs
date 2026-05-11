//! Transcript reads (`conversation_messages` table) — chronological
//! pages, session listing, single-block context windows, BM25, and
//! semantic vector search. All inherent on `DuckDbQuery`.

use duckdb::{params, OptionalExt};

use super::{spawn_blocking_storage, DuckDbQuery};
use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::storage::types::{ContextWindow, StorageError, TranscriptSessionSummary};

/// 16-column projection shared by every conversation_messages read.
/// Order must match `row_to_conversation_message` below — keep in sync.
const CONVERSATION_COLS: &str = "message_block_id, session_id, tenant, caller_agent, \
    transcript_path, line_number, block_index, message_uuid, role, block_type, content, \
    tool_name, tool_use_id, embed_eligible, created_at, meta_json";

/// Parse one row of the 16-column conversation_messages SELECT into a
/// [`ConversationMessage`].
fn row_to_conversation_message(row: &duckdb::Row<'_>) -> duckdb::Result<ConversationMessage> {
    let role: String = row.get(8)?;
    let block_type: String = row.get(9)?;
    Ok(ConversationMessage {
        message_block_id: row.get(0)?,
        session_id: row.get(1)?,
        tenant: row.get(2)?,
        caller_agent: row.get(3)?,
        transcript_path: row.get(4)?,
        line_number: row.get::<_, u64>(5)?,
        block_index: row.get::<_, u32>(6)?,
        message_uuid: row.get(7)?,
        role: MessageRole::from_db_str(&role).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(
                8,
                duckdb::types::Type::Text,
                format!("invalid role string {role:?}").into(),
            )
        })?,
        block_type: BlockType::from_db_str(&block_type).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(
                9,
                duckdb::types::Type::Text,
                format!("invalid block_type string {block_type:?}").into(),
            )
        })?,
        content: row.get(10)?,
        tool_name: row.get(11)?,
        tool_use_id: row.get(12)?,
        embed_eligible: row.get(13)?,
        created_at: row.get(14)?,
        meta_json: row.get::<_, Option<String>>(15)?,
    })
}

/// Collect rows from a conversation_messages `query_map` iterator
/// into a `Vec<ConversationMessage>`, surfacing per-row
/// `duckdb::Error` as `StorageError::DuckDb`.
fn collect_messages<I>(rows: I) -> Result<Vec<ConversationMessage>, StorageError>
where
    I: Iterator<Item = duckdb::Result<ConversationMessage>>,
{
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(StorageError::DuckDb)?);
    }
    Ok(out)
}

impl DuckDbQuery {
    // ── Transcript reads (`conversation_messages` table) ────────────

    /// All conversation blocks for `(tenant, session_id)`, ordered
    /// chronologically `(created_at ASC, line_number ASC,
    /// block_index ASC)`. Mirrors the legacy backend 1:1.
    pub async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let session_id = session_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND session_id = ?2 \
                 ORDER BY created_at ASC, line_number ASC, block_index ASC",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant, session_id], row_to_conversation_message)?;
            collect_messages(rows)
        })
        .await
    }

    /// Paginated session scroll. Composite cursor `(created_at,
    /// line_number, block_index)` lets the caller resume strictly
    /// after the last row they saw using row-tuple comparison
    /// (DuckDB supports tuple comparison, but we expand it
    /// explicitly for compatibility — same shape the legacy backend
    /// used). `since` / `until` apply to `created_at` only
    /// (inclusive lower, exclusive upper) and stack on top of the
    /// cursor.
    ///
    /// Fetches `limit + 1` rows so `has_more` can be reported
    /// without a separate `count(*)`. If the extra row came back,
    /// drop it and tell the caller `has_more = true`.
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
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let session_id = session_id.to_string();
        let since = since.map(str::to_owned);
        let until = until.map(str::to_owned);
        let role = role.map(str::to_owned);
        let block_type = block_type.map(str::to_owned);
        let cursor: Option<(String, i64, i64)> = cursor.map(|(s, l, b)| (s.to_owned(), l, b));
        let lim = i64::try_from(limit).unwrap_or(64);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND session_id = ?2",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> =
                vec![Box::new(tenant), Box::new(session_id)];
            if let Some(s) = since {
                sql.push_str(&format!(" AND created_at >= ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(s));
            }
            if let Some(u) = until {
                sql.push_str(&format!(" AND created_at < ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(u));
            }
            if let Some(r) = role {
                sql.push_str(&format!(" AND role = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(r));
            }
            if let Some(b) = block_type {
                sql.push_str(&format!(" AND block_type = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(b));
            }
            if let Some((cur_at, cur_line, cur_idx)) = cursor {
                let p = params_vec.len();
                sql.push_str(&format!(
                    " AND (created_at > ?{a} \
                       OR (created_at = ?{a} AND (line_number > ?{b} \
                                              OR (line_number = ?{b} AND block_index > ?{c}))))",
                    a = p + 1,
                    b = p + 2,
                    c = p + 3,
                ));
                params_vec.push(Box::new(cur_at));
                params_vec.push(Box::new(cur_line));
                params_vec.push(Box::new(cur_idx));
            }
            sql.push_str(" ORDER BY created_at ASC, line_number ASC, block_index ASC");
            sql.push_str(&format!(" LIMIT ?{}", params_vec.len() + 1));
            let fetch = lim.saturating_add(1);
            params_vec.push(Box::new(fetch));

            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_conversation_message)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            let has_more = out.len() as i64 == fetch;
            if has_more {
                out.pop();
            }
            Ok((out, has_more))
        })
        .await
    }

    /// Per-session aggregate. Replaces the legacy backend's hand-
    /// written aggregation (count + min + max in Rust over a full
    /// scan) with one DuckDB `GROUP BY` — the canonical example of
    /// what the DuckDB-as-query layer buys us over the LanceDB
    /// native query API. Tenant-scoped; null-session rows excluded.
    /// Ordered `last_at DESC`.
    pub async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT session_id, \
                        count(*)          AS block_count, \
                        min(created_at)   AS first_at, \
                        max(created_at)   AS last_at, \
                        max(caller_agent) AS caller_agent \
                 FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND session_id IS NOT NULL \
                 GROUP BY session_id \
                 ORDER BY last_at DESC",
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
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }

    /// Bulk fetch by `message_block_id` list, scoped to `tenant`.
    /// Returns rows in **input slice order**, with missing ids
    /// silently dropped (per the legacy contract: post-search
    /// hydration tolerates rows that disappeared between search and
    /// fetch). Empty `ids` short-circuits.
    pub async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let ids: Vec<String> = ids.to_vec();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let placeholders = (2..=ids.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND message_block_id IN ({placeholders})",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant)];
            for id in &ids {
                params_vec.push(Box::new(id.clone()));
            }
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_conversation_message)?;
            let mut by_id = std::collections::HashMap::with_capacity(ids.len());
            for r in rows {
                let m = r.map_err(StorageError::DuckDb)?;
                by_id.insert(m.message_block_id.clone(), m);
            }
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(m) = by_id.remove(&id) {
                    out.push(m);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Block + `k_before` predecessors + `k_after` successors in the
    /// same session, ordered by `(created_at, line_number,
    /// block_index)`. The primary block is always returned (even
    /// when `include_tool_blocks=false` and its own block_type is
    /// tool_use/tool_result); the filter applies to neighbors only.
    /// `before` / `after` are returned in chronological ASC order.
    ///
    /// Returns `Err(StorageError::NotFound("transcript primary block"))`
    /// when no row matches the primary id under this tenant.
    /// Returns `before=[]`, `after=[]` when the primary has no
    /// session_id (NULL session).
    pub async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let primary_id = primary_id.to_string();
        let k_before = i64::try_from(k_before).unwrap_or(0);
        let k_after = i64::try_from(k_after).unwrap_or(0);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");

            // 1. Primary fetch.
            let primary_sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND message_block_id = ?2",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let primary: ConversationMessage = match conn
                .query_row(
                    &primary_sql,
                    params![&tenant, &primary_id],
                    row_to_conversation_message,
                )
                .optional()
                .map_err(StorageError::DuckDb)?
            {
                Some(m) => m,
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
                    });
                }
            };

            // 3. Optional block_type filter (applies to neighbors
            // only — primary returned regardless).
            let type_filter = if include_tool_blocks {
                ""
            } else {
                "AND block_type IN ('text', 'thinking') "
            };

            // 4. Predecessors. Strict tuple comparison
            // `(created_at, line_number, block_index) <
            // (primary.created_at, primary.line_number,
            // primary.block_index)`, expanded explicitly for
            // compatibility with non-DuckDB SQL dialects (we don't
            // need the portability here, but the shape is shared
            // with the legacy backend so the cutover is mechanical).
            let before_sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 \
                   AND session_id = ?2 \
                   AND ( \
                        created_at < ?3 \
                     OR (created_at = ?3 AND line_number < ?4) \
                     OR (created_at = ?3 AND line_number = ?4 AND block_index < ?5) \
                   ) \
                   {type_filter}\
                 ORDER BY created_at DESC, line_number DESC, block_index DESC \
                 LIMIT ?6",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let before: Vec<ConversationMessage> = {
                let mut stmt = conn.prepare(&before_sql)?;
                let rows = stmt.query_map(
                    params![
                        &tenant,
                        &session_id,
                        &primary.created_at,
                        primary.line_number as i64,
                        primary.block_index as i64,
                        k_before,
                    ],
                    row_to_conversation_message,
                )?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r.map_err(StorageError::DuckDb)?);
                }
                // Query returns DESC; flip to ASC for the caller.
                v.reverse();
                v
            };

            // 5. Successors (strict tuple comparison >).
            let after_sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 \
                   AND session_id = ?2 \
                   AND ( \
                        created_at > ?3 \
                     OR (created_at = ?3 AND line_number > ?4) \
                     OR (created_at = ?3 AND line_number = ?4 AND block_index > ?5) \
                   ) \
                   {type_filter}\
                 ORDER BY created_at ASC, line_number ASC, block_index ASC \
                 LIMIT ?6",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let after: Vec<ConversationMessage> = {
                let mut stmt = conn.prepare(&after_sql)?;
                let rows = stmt.query_map(
                    params![
                        &tenant,
                        &session_id,
                        &primary.created_at,
                        primary.line_number as i64,
                        primary.block_index as i64,
                        k_after,
                    ],
                    row_to_conversation_message,
                )?;
                let mut v = Vec::new();
                for r in rows {
                    v.push(r.map_err(StorageError::DuckDb)?);
                }
                v
            };

            Ok(ContextWindow {
                primary,
                before,
                after,
            })
        })
        .await
    }

    /// Anchor-session candidates: most-recent embed_eligible blocks
    /// in the given session, capped at `k`. Returns `message_block_id`s
    /// only — the search service then funnels them into the
    /// candidate pool alongside topical (BM25/HNSW) hits so the
    /// active conversation always biases the result set.
    pub async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let session_id = session_id.to_string();
        let k_i = i64::try_from(k).unwrap_or(64);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT message_block_id FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND session_id = ?2 AND embed_eligible = true \
                 ORDER BY created_at DESC \
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![tenant, session_id, k_i], |row| {
                row.get::<_, String>(0)
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }

    /// Most-recent embed_eligible conversation messages for `tenant`,
    /// newest first (`created_at DESC, line_number DESC, block_index
    /// DESC`). Used as the empty-query fallback for transcript
    /// search and as a CLI / diagnostic listing helper.
    pub async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CONVERSATION_COLS} FROM ns.main.conversation_messages \
                 WHERE tenant = ?1 AND embed_eligible = true \
                 ORDER BY created_at DESC, line_number DESC, block_index DESC \
                 LIMIT ?2",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant, lim], row_to_conversation_message)?;
            collect_messages(rows)
        })
        .await
    }

    /// Lexical recall over `conversation_messages.content`. Same
    /// shape as `bm25_candidates` on memories — `lance_fts(...)` for
    /// the BM25 ranker, outer WHERE for tenant + embed_eligible
    /// scope, oversample = `k * 2`, final LIMIT = `k`. The FTS
    /// index on `(conversation_messages, content)` is built at
    /// `LanceStore::open` time.
    pub async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let query = query.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        let oversample = k_i.saturating_mul(2);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CONVERSATION_COLS} \
                 FROM lance_fts('ns.main.conversation_messages', 'content', ?1, k => ?2) \
                 WHERE tenant = ?3 AND embed_eligible = true \
                 ORDER BY _score DESC \
                 LIMIT ?4",
                CONVERSATION_COLS = CONVERSATION_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![query, oversample, tenant, k_i],
                row_to_conversation_message,
            )?;
            collect_messages(rows)
        })
        .await
    }

    /// Semantic recall over `conversation_message_embeddings`.
    /// Mirrors `semantic_search_capability_capsules` 1:1 with `memories` →
    /// `conversation_messages` and `capability_capsule_id` → `message_block_id`.
    /// Routes through the lance extension's `lance_vector_search`
    /// SQL function; joins back to `ns.main.conversation_messages`
    /// for the full row. Returns `(message, similarity)` pairs in
    /// descending similarity order.
    ///
    /// **Score**: cosine similarity ∈ `[0, 1]` for normalized
    /// embeddings, derived from the L2² distance lance returns as
    /// `1 - L2²/2` — see `semantic_search_capability_capsules` for the
    /// derivation. Same workaround applies (lance extension's
    /// `lance_vector_search` doesn't accept a `distance_type`
    /// kwarg, so we transform the L2² return value).
    ///
    /// `embed_eligible = true` is enforced in the outer WHERE: the
    /// transcript embedding worker only computes embeddings for
    /// eligible blocks, but a defense-in-depth filter here keeps
    /// non-eligible rows out of the result even if a stale row
    /// somehow survived.
    pub async fn semantic_search_transcripts(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(ConversationMessage, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let vector_lit = format!(
            "[{}]::FLOAT[]",
            query_embedding
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        );
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let oversample = lim.saturating_mul(2);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let c_cols = CONVERSATION_COLS
                .split(',')
                .map(|c| format!("c.{}", c.trim()))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {c_cols}, e._distance \
                 FROM lance_vector_search( \
                        'ns.main.conversation_message_embeddings', 'embedding', \
                        {vector_lit}, k => ?1 \
                      ) AS e \
                 JOIN ns.main.conversation_messages AS c \
                   ON c.message_block_id = e.message_block_id \
                 WHERE c.tenant = ?2 AND c.embed_eligible = true \
                 ORDER BY e._distance ASC \
                 LIMIT ?3",
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![oversample, tenant, lim], |row| {
                let msg = row_to_conversation_message(row)?;
                let l2_squared: f32 = row.get(16)?; // 16 conv cols → idx 16
                Ok((msg, 1.0_f32 - l2_squared / 2.0_f32))
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockType, ConversationMessage, MessageRole};
    use crate::storage::lance_store::LanceStore;
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

    /// native query API) with a single SQL `GROUP BY session_id`.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_transcript_reads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();
        // Required: create_conversation_message enqueues an
        // embedding job when embed_eligible, and the enqueue stamps
        // `provider` from the configured value.
        lance.set_transcript_job_provider("fake-test");

        // Seed: 3 blocks for sess_a (text → tool_use → thinking), 1
        // for sess_b in tenant-b, 1 null-session block in tenant-a.
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
        let m_null = msg(
            "blk_null",
            "tenant-a",
            None,
            1,
            0,
            BlockType::Text,
            "no session block",
            "00000001778000000005",
        );
        for m in [&m1, &m2, &m3, &m4, &m_null] {
            lance.create_conversation_message(m).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // get_by_session: 3 blocks ordered ASC.
        let sess_a = q
            .get_conversation_messages_by_session("tenant-a", "sess_a")
            .await
            .unwrap();
        assert_eq!(sess_a.len(), 3);
        assert_eq!(sess_a[0].message_block_id, "blk_1");
        assert_eq!(sess_a[1].message_block_id, "blk_2");
        assert_eq!(sess_a[2].message_block_id, "blk_3");

        // list_transcript_sessions: GROUP BY result. Null-session
        // block excluded; sess_b in tenant-b not visible to tenant-a.
        let summaries = q.list_transcript_sessions("tenant-a").await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, "sess_a");
        assert_eq!(summaries[0].block_count, 3);
        assert_eq!(summaries[0].first_at, "00000001778000000010");
        assert_eq!(summaries[0].last_at, "00000001778000000030");
        assert_eq!(summaries[0].caller_agent.as_deref(), Some("claude-code"));
        let summaries_b = q.list_transcript_sessions("tenant-b").await.unwrap();
        assert_eq!(summaries_b.len(), 1);
        assert_eq!(summaries_b[0].session_id, "sess_b");

        // fetch_by_ids: input-order preserved; missing ids dropped.
        let by_ids = q
            .fetch_conversation_messages_by_ids(
                "tenant-a",
                &["blk_3".into(), "blk_1".into(), "missing".into()],
            )
            .await
            .unwrap();
        assert_eq!(by_ids.len(), 2);
        assert_eq!(by_ids[0].message_block_id, "blk_3");
        assert_eq!(by_ids[1].message_block_id, "blk_1");

        // context_window for blk_2 (the tool_use middle block) with
        // tool blocks included → before=[blk_1], after=[blk_3].
        let win_with = q
            .context_window_for_block("tenant-a", "blk_2", 5, 5, true)
            .await
            .unwrap();
        assert_eq!(win_with.primary.message_block_id, "blk_2");
        assert_eq!(win_with.before.len(), 1);
        assert_eq!(win_with.before[0].message_block_id, "blk_1");
        assert_eq!(win_with.after.len(), 1);
        assert_eq!(win_with.after[0].message_block_id, "blk_3");

        // include_tool_blocks=false on blk_2 → primary still
        // returned (tool_use), neighbors filter applies: blk_1
        // (text) before, blk_3 (thinking) after — both eligible.
        let win_no = q
            .context_window_for_block("tenant-a", "blk_2", 5, 5, false)
            .await
            .unwrap();
        assert_eq!(win_no.primary.message_block_id, "blk_2");
        assert_eq!(win_no.before.len(), 1);
        assert_eq!(win_no.after.len(), 1);

        // k=0 → empty windows.
        let win_zero = q
            .context_window_for_block("tenant-a", "blk_2", 0, 0, true)
            .await
            .unwrap();
        assert!(win_zero.before.is_empty());
        assert!(win_zero.after.is_empty());

        // Missing primary → NotFound.
        let nf = q
            .context_window_for_block("tenant-a", "does-not-exist", 5, 5, true)
            .await
            .unwrap_err();
        assert!(matches!(
            nf,
            StorageError::NotFound("transcript primary block")
        ));

        // Null-session primary → empty before/after, no error.
        let null_window = q
            .context_window_for_block("tenant-a", "blk_null", 5, 5, true)
            .await
            .unwrap();
        assert_eq!(null_window.primary.message_block_id, "blk_null");
        assert!(null_window.before.is_empty());
        assert!(null_window.after.is_empty());

        // anchor_session_candidates: embed_eligible only, DESC. blk_2
        // (tool_use) excluded.
        let anchors = q
            .anchor_session_candidates("tenant-a", "sess_a", 5)
            .await
            .unwrap();
        assert_eq!(anchors, vec!["blk_3".to_string(), "blk_1".to_string()]);
        // k=0 → empty.
        let z = q
            .anchor_session_candidates("tenant-a", "sess_a", 0)
            .await
            .unwrap();
        assert!(z.is_empty());

        // recent_conversation_messages: tenant-a embed_eligible only;
        // null-session blk_null is text + eligible so it's in too.
        let recent = q
            .recent_conversation_messages("tenant-a", 10)
            .await
            .unwrap();
        let recent_ids: Vec<&str> = recent.iter().map(|m| m.message_block_id.as_str()).collect();
        assert_eq!(recent_ids, vec!["blk_3", "blk_1", "blk_null"]);

        // BM25 transcript: lance_fts on conversation_messages.content.
        let bm25_duck = q
            .bm25_transcript_candidates("tenant-a", "DuckDB", 5)
            .await
            .unwrap();
        let duck_ids: Vec<&str> = bm25_duck
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert!(duck_ids.contains(&"blk_1"), "got {duck_ids:?}");
        let bm25_lance = q
            .bm25_transcript_candidates("tenant-a", "LanceDB", 5)
            .await
            .unwrap();
        let lance_ids: Vec<&str> = bm25_lance
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert!(lance_ids.contains(&"blk_3"), "got {lance_ids:?}");
        let bm25_empty = q
            .bm25_transcript_candidates("tenant-a", "", 5)
            .await
            .unwrap();
        assert!(bm25_empty.is_empty());

        // get_paged: walk through 2 pages with cursor + has_more flag.
        let (page1, more1) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a", "sess_a", None, None, None, None, None, 2,
            )
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert!(more1);
        assert_eq!(page1[0].message_block_id, "blk_1");
        assert_eq!(page1[1].message_block_id, "blk_2");
        let last = page1.last().unwrap();
        let (page2, more2) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                None,
                None,
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

        // since/until window narrows the page query.
        let (windowed, _) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                Some("00000001778000000020"),
                Some("00000001778000000031"),
                None,
                None,
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

        // role filter — fixture has role=Assistant for every block, so
        // role=user yields 0 rows and role=assistant yields all 3.
        let (role_user, _) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                None,
                None,
                Some("user"),
                None,
                None,
                10,
            )
            .await
            .unwrap();
        assert!(
            role_user.is_empty(),
            "role=user must drop all-assistant fixture rows; got {role_user:?}"
        );
        let (role_assistant, _) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                None,
                None,
                Some("assistant"),
                None,
                None,
                10,
            )
            .await
            .unwrap();
        assert_eq!(role_assistant.len(), 3);

        // block_type filter — fixture is text / tool_use / thinking;
        // each filter narrows to exactly one block.
        let (text_only, _) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                None,
                None,
                None,
                Some("text"),
                None,
                10,
            )
            .await
            .unwrap();
        let text_ids: Vec<&str> = text_only
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert_eq!(text_ids, vec!["blk_1"]);
        let (thinking_only, _) = q
            .get_conversation_messages_by_session_paged(
                "tenant-a",
                "sess_a",
                None,
                None,
                None,
                Some("thinking"),
                None,
                10,
            )
            .await
            .unwrap();
        let thinking_ids: Vec<&str> = thinking_only
            .iter()
            .map(|m| m.message_block_id.as_str())
            .collect();
        assert_eq!(thinking_ids, vec!["blk_3"]);
    }
}
