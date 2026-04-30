use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::{Arc, Mutex, MutexGuard, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use duckdb::{params, Connection, OptionalExt};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::domain::{
    episode::EpisodeRecord,
    memory::{FeedbackKind, FeedbackSummary, MemoryRecord, MemoryStatus, MemoryVersionLink},
    session::Session,
};
use crate::pipeline::ingest::{compute_content_hash_from_record, CONTENT_HASH_LEN};

use super::schema;
use super::vector_index::{EmbeddingRowSource, VectorIndex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackEvent {
    pub feedback_id: String,
    pub memory_id: String,
    pub feedback_kind: String,
    pub created_at: String,
}

/// Row claimed by the embedding worker (`status = processing`).
#[derive(Debug, Clone)]
pub struct ClaimedEmbeddingJob {
    pub job_id: String,
    pub tenant: String,
    pub memory_id: String,
    pub target_content_hash: String,
    pub provider: String,
    pub attempt_count: i64,
}

/// Insert payload for `embedding_jobs` (worker and APIs use the same row shape).
#[derive(Debug, Clone)]
pub struct EmbeddingJobInsert {
    pub job_id: String,
    pub tenant: String,
    pub memory_id: String,
    pub target_content_hash: String,
    pub provider: String,
    pub available_at: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
struct EmbeddingJobRow {
    job_id: String,
    tenant: String,
    memory_id: String,
    target_content_hash: String,
    provider: String,
    status: String,
    attempt_count: i64,
    last_error: Option<String>,
    available_at: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
struct MemoryEmbeddingRow {
    memory_id: String,
    tenant: String,
    embedding_model: String,
    embedding_dim: i64,
    embedding: Vec<u8>,
    content_hash: String,
    source_updated_at: String,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone)]
pub struct DuckDbRepository {
    conn: Arc<Mutex<Connection>>,
    vector_index: Arc<RwLock<Option<Arc<VectorIndex>>>>,
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("duckdb error: {0}")]
    DuckDb(#[from] duckdb::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid data: {0}")]
    InvalidData(&'static str),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("vector index error: {0}")]
    VectorIndex(String),
}

impl DuckDbRepository {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        schema::bootstrap(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            vector_index: Arc::new(RwLock::new(None)),
        })
    }

    pub fn attach_vector_index(&self, idx: Arc<VectorIndex>) {
        *self
            .vector_index
            .write()
            .expect("vector_index lock poisoned") = Some(idx);
    }

    pub fn has_vector_index(&self) -> bool {
        self.vector_index
            .read()
            .expect("vector_index lock poisoned")
            .is_some()
    }

    pub(crate) fn vector_index(&self) -> Option<Arc<VectorIndex>> {
        self.vector_index
            .read()
            .expect("vector_index lock poisoned")
            .clone()
    }

    pub async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
        let conn = self.conn()?;
        let stored = memory.clone();
        conn.execute(
            "insert into memories (
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                last_validated_at
            ) values (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20,
                ?21, ?22, ?23, ?24, ?25,
                ?26
            )",
            params![
                stored.memory_id,
                stored.tenant,
                encode_text(&stored.memory_type)?,
                encode_text(&stored.status)?,
                encode_text(&stored.scope)?,
                encode_text(&stored.visibility)?,
                stored.version as i64,
                stored.summary,
                stored.content,
                encode_json(&stored.evidence)?,
                encode_json(&stored.code_refs)?,
                stored.project,
                stored.repo,
                stored.module,
                stored.task_type,
                encode_json(&stored.tags)?,
                f64::from(stored.confidence),
                f64::from(stored.decay_score),
                stored.content_hash,
                stored.idempotency_key,
                stored.session_id,
                stored.supersedes_memory_id,
                stored.source_agent,
                stored.created_at,
                stored.updated_at,
                stored.last_validated_at,
            ],
        )?;

        Ok(memory)
    }

    /// Enqueues a pending embedding job unless a live (`pending` or `processing`) job
    /// already exists for the same `(tenant, memory_id, target_content_hash, provider)`.
    /// Returns `true` if a new row was inserted.
    ///
    /// Concurrency: dedupe is enforced via the transactional count-then-insert below, atomic
    /// w.r.t. concurrent callers because the underlying `Arc<Mutex<Connection>>` serializes
    /// all DB access in this process. There is intentionally no DB-level partial unique
    /// constraint (DuckDB bundled does not support one).
    pub async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let live: i64 = tx.query_row(
            "select count(*) from embedding_jobs
             where tenant = ?1
               and memory_id = ?2
               and target_content_hash = ?3
               and provider = ?4
               and status in ('pending', 'processing')",
            params![
                &insert.tenant,
                &insert.memory_id,
                &insert.target_content_hash,
                &insert.provider,
            ],
            |row| row.get(0),
        )?;
        if live > 0 {
            return Ok(false);
        }

        tx.execute(
            "insert into embedding_jobs (
                job_id, tenant, memory_id, target_content_hash, provider,
                status, attempt_count, last_error, available_at, created_at, updated_at
            ) values (
                ?1, ?2, ?3, ?4, ?5,
                'pending', 0, null, ?6, ?7, ?8
            )",
            params![
                insert.job_id,
                insert.tenant,
                insert.memory_id,
                insert.target_content_hash,
                insert.provider,
                insert.available_at,
                insert.created_at,
                insert.updated_at,
            ],
        )?;
        tx.commit()?;
        Ok(true)
    }

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
        msg: &crate::domain::ConversationMessage,
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

    pub async fn count_embedding_jobs_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<i64, StorageError> {
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "select count(*) from embedding_jobs where memory_id = ?1",
            params![memory_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub async fn count_memory_embeddings_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<i64, StorageError> {
        let conn = self.conn()?;
        let count: i64 = conn.query_row(
            "select count(*) from memory_embeddings where memory_id = ?1",
            params![memory_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub async fn count_total_memory_embeddings(&self) -> Result<i64, StorageError> {
        let conn = self.conn()?;
        let count: i64 =
            conn.query_row("select count(*) from memory_embeddings", params![], |row| {
                row.get(0)
            })?;
        Ok(count)
    }

    /// Test-only seed used by integration tests that bypass the worker.
    #[doc(hidden)]
    pub async fn seed_memory_embedding_for_test(
        &self,
        memory_id: &str,
        tenant: &str,
        vec: &[f32],
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        let now = current_timestamp();
        // Insert a minimal placeholder memories row first to satisfy the FK constraint
        // on memory_embeddings.memory_id references memories(memory_id).
        conn.execute(
            "insert or ignore into memories (
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                supersedes_memory_id, source_agent, created_at, updated_at, last_validated_at
            ) values (
                ?1, ?2, 'implementation', 'active', 'repo', 'shared', 1, 'seed',
                'seed', '[]', '[]', null, null, null, null,
                '[]', 1.0, 0.0, 'seed', null,
                null, 'test', ?3, ?3, null
            )",
            params![memory_id, tenant, now],
        )?;
        let mut blob = Vec::with_capacity(vec.len() * 4);
        for v in vec {
            blob.extend_from_slice(&v.to_ne_bytes());
        }
        conn.execute(
            "insert or replace into memory_embeddings (
                memory_id, tenant, embedding_model, embedding_dim, embedding,
                content_hash, source_updated_at, created_at, updated_at
            ) values (?1, ?2, 'fake', ?3, ?4, 'seed', ?5, ?5, ?5)",
            params![memory_id, tenant, vec.len() as i64, blob, now],
        )?;
        Ok(())
    }

    pub async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn()?;
        let id: Option<String> = conn
            .query_row(
                "select job_id from embedding_jobs where memory_id = ?1 order by created_at asc limit 1",
                params![memory_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    pub async fn get_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn()?;
        let status: Option<String> = conn
            .query_row(
                "select status from embedding_jobs where job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status)
    }

    /// Claims the next eligible job, moving it to `processing`. Eligible means `pending`, or
    /// `failed` with `attempt_count < max_retries` (configured retry budget).
    pub async fn claim_next_embedding_job(
        &self,
        now: &str,
        max_retries: u32,
    ) -> Result<Option<ClaimedEmbeddingJob>, StorageError> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let max_r = i64::from(max_retries);

        let job_id: Option<String> = tx
            .query_row(
                "select job_id from embedding_jobs
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
            "update embedding_jobs
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
            "select job_id, tenant, memory_id, target_content_hash, provider, attempt_count
             from embedding_jobs where job_id = ?1",
            params![job_id],
            |row| {
                Ok(ClaimedEmbeddingJob {
                    job_id: row.get(0)?,
                    tenant: row.get(1)?,
                    memory_id: row.get(2)?,
                    target_content_hash: row.get(3)?,
                    provider: row.get(4)?,
                    attempt_count: row.get(5)?,
                })
            },
        )?;

        tx.commit()?;
        Ok(Some(job))
    }

    #[allow(clippy::too_many_arguments)]
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
        let conn = self.conn()?;
        conn.execute(
            "delete from memory_embeddings where memory_id = ?1",
            params![memory_id],
        )?;
        conn.execute(
            "insert into memory_embeddings (
                memory_id, tenant, embedding_model, embedding_dim, embedding,
                content_hash, source_updated_at, created_at, updated_at
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                memory_id,
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

    pub async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        {
            let conn = self.conn()?;
            conn.execute(
                "delete from memory_embeddings where memory_id = ?1",
                params![memory_id],
            )?;
        } // drop conn (MutexGuard) before the async remove call
        if let Some(idx) = self.vector_index() {
            if let Err(err) = idx.remove(memory_id).await {
                tracing::warn!(
                    memory_id,
                    error = %err,
                    "vector_index.remove failed (best-effort)"
                );
            }
        }
        Ok(())
    }

    /// Marks in-flight and queued jobs as `stale` so a new job can be enqueued (e.g. forced rebuild).
    pub async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        let conn = self.conn()?;
        let n = conn.execute(
            "update embedding_jobs
             set status = 'stale', updated_at = ?1
             where tenant = ?2 and memory_id = ?3 and provider = ?4
               and status in ('pending', 'processing')",
            params![now, tenant, memory_id, provider],
        )?;
        Ok(n)
    }

    /// Snapshot row for embedding metadata (`model`, stored `content_hash`, `updated_at`).
    pub async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        let conn = self.conn()?;
        let row: Option<(String, String, String)> = conn
            .query_row(
                "select embedding_model, content_hash, updated_at
                 from memory_embeddings where memory_id = ?1",
                params![memory_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Latest job status for an exact `(tenant, memory_id, target_content_hash)` match.
    pub async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn()?;
        let status: Option<String> = conn
            .query_row(
                "select status from embedding_jobs
                 where tenant = ?1 and memory_id = ?2 and target_content_hash = ?3
                 order by updated_at desc limit 1",
                params![tenant, memory_id, target_content_hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(status)
    }

    pub async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::domain::embeddings::EmbeddingJobInfo>, StorageError> {
        let conn = self.conn()?;
        let lim = i64::try_from(limit).unwrap_or(1000).min(10_000);
        let mut stmt = if status_filter.is_some() && memory_id_filter.is_some() {
            conn.prepare(
                "select job_id, tenant, memory_id, target_content_hash, provider, status,
                        attempt_count, last_error, available_at, created_at, updated_at
                 from embedding_jobs
                 where tenant = ?1 and status = ?2 and memory_id = ?3
                 order by updated_at desc
                 limit ?4",
            )?
        } else if status_filter.is_some() {
            conn.prepare(
                "select job_id, tenant, memory_id, target_content_hash, provider, status,
                        attempt_count, last_error, available_at, created_at, updated_at
                 from embedding_jobs
                 where tenant = ?1 and status = ?2
                 order by updated_at desc
                 limit ?3",
            )?
        } else if memory_id_filter.is_some() {
            conn.prepare(
                "select job_id, tenant, memory_id, target_content_hash, provider, status,
                        attempt_count, last_error, available_at, created_at, updated_at
                 from embedding_jobs
                 where tenant = ?1 and memory_id = ?2
                 order by updated_at desc
                 limit ?3",
            )?
        } else {
            conn.prepare(
                "select job_id, tenant, memory_id, target_content_hash, provider, status,
                        attempt_count, last_error, available_at, created_at, updated_at
                 from embedding_jobs
                 where tenant = ?1
                 order by updated_at desc
                 limit ?2",
            )?
        };

        let map_row =
            |row: &duckdb::Row| -> duckdb::Result<crate::domain::embeddings::EmbeddingJobInfo> {
                let attempt: i64 = row.get(6)?;
                Ok(crate::domain::embeddings::EmbeddingJobInfo {
                    job_id: row.get(0)?,
                    tenant: row.get(1)?,
                    memory_id: row.get(2)?,
                    target_content_hash: row.get(3)?,
                    provider: row.get(4)?,
                    status: row.get(5)?,
                    attempt_count: u32::try_from(attempt).unwrap_or(0),
                    last_error: row.get(7)?,
                    available_at: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                })
            };

        let rows = match (status_filter, memory_id_filter) {
            (Some(st), Some(mid)) => stmt.query_map(params![tenant, st, mid, lim], map_row)?,
            (Some(st), None) => stmt.query_map(params![tenant, st, lim], map_row)?,
            (None, Some(mid)) => stmt.query_map(params![tenant, mid, lim], map_row)?,
            (None, None) => stmt.query_map(params![tenant, lim], map_row)?,
        };

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub async fn list_memory_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("select memory_id from memories where tenant = ?1 order by updated_at desc")?;
        let rows = stmt.query_map(params![tenant], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Joins `memories` with valid `memory_embeddings` (hash match), scores by cosine similarity in Rust.
    /// Uses the attached `VectorIndex` (ANN path) when available; falls back to the legacy linear scan
    /// when no index is attached or `MEM_VECTOR_INDEX_USE_LEGACY=1` is set.
    pub async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(vec![]);
        }

        let Some(idx) = self.vector_index() else {
            // No index attached: behave as the legacy linear scan would.
            tracing::warn!(
                "vector index not attached; falling back to legacy linear-scan search (deprecated)"
            );
            return self
                .legacy_semantic_search_memories(tenant, query_embedding, limit)
                .await;
        };

        // Config fields `vector_index_use_legacy` / `vector_index_oversample` exist on
        // `EmbeddingSettings` but are not plumbed into this struct (Option A decision: env-var
        // reads here provide runtime-override flexibility without requiring a repo rebuild).
        let use_legacy = std::env::var("MEM_VECTOR_INDEX_USE_LEGACY")
            .ok()
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if use_legacy {
            tracing::warn!(
                "MEM_VECTOR_INDEX_USE_LEGACY=1; routing to deprecated legacy linear-scan search"
            );
            return self
                .legacy_semantic_search_memories(tenant, query_embedding, limit)
                .await;
        }
        let oversample = std::env::var("MEM_VECTOR_INDEX_OVERSAMPLE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(4);

        let k = limit.saturating_mul(oversample).max(limit);
        let hits = idx
            .search(query_embedding, k)
            .await
            .map_err(|e| StorageError::VectorIndex(format!("vector_index search: {e}")))?;
        if hits.is_empty() {
            return Ok(vec![]);
        }

        let id_strs: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        let rows = self.fetch_memories_by_ids(tenant, &id_strs).await?;

        let by_id: std::collections::HashMap<&str, f32> =
            hits.iter().map(|(i, s)| (i.as_str(), *s)).collect();
        let mut scored: Vec<(MemoryRecord, f32)> = rows
            .into_iter()
            .filter_map(|m| by_id.get(m.memory_id.as_str()).map(|s| (m, *s)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.memory_id.cmp(&b.0.memory_id))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    /// Legacy linear-scan implementation of semantic search.
    /// Preserved verbatim from before Task 14; activated by `MEM_VECTOR_INDEX_USE_LEGACY=1`
    /// or when no `VectorIndex` is attached.
    async fn legacy_semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select
                m.memory_id, m.tenant, m.memory_type, m.status, m.scope, m.visibility, m.version,
                m.summary, m.content, m.evidence_json, m.code_refs_json, m.project, m.repo,
                m.module, m.task_type, m.tags_json, m.confidence, m.decay_score, m.content_hash,
                m.idempotency_key, m.session_id, m.supersedes_memory_id, m.source_agent,
                m.created_at, m.updated_at, m.last_validated_at,
                e.embedding
             from memories m
             inner join memory_embeddings e on m.memory_id = e.memory_id
             where m.tenant = ?1
               and m.content_hash = e.content_hash
               and m.status not in ('rejected', 'archived')
             order by m.updated_at desc
             limit 2000",
        )?;

        let rows = stmt.query_map(params![tenant], map_memory_with_blob)?;
        let mut scored = Vec::new();
        for row in rows {
            let (memory, blob) = row?;
            let Ok(emb) = decode_f32_blob(&blob, query_embedding.len()) else {
                continue;
            };
            let sim = cosine_similarity(&emb, query_embedding);
            scored.push((memory, sim));
        }

        scored.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.memory_id.cmp(&right.0.memory_id))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    pub async fn complete_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "update embedding_jobs
             set status = 'completed', last_error = null, updated_at = ?1
             where job_id = ?2 and status = 'processing'",
            params![now, job_id],
        )?;
        Ok(())
    }

    pub async fn mark_embedding_job_stale(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "update embedding_jobs set status = 'stale', updated_at = ?1 where job_id = ?2",
            params![now, job_id],
        )?;
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
        let conn = self.conn()?;
        conn.execute(
            "update embedding_jobs
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

    pub async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "update embedding_jobs
             set status = 'failed',
                 attempt_count = ?1,
                 last_error = ?2,
                 updated_at = ?3
             where job_id = ?4",
            params![new_attempt_count, last_error, now, job_id],
        )?;
        Ok(())
    }

    pub async fn get_memory(
        &self,
        memory_id: String,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                    last_validated_at
                 from memories
                 where memory_id = ?1",
                params![memory_id],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                    last_validated_at
                 from memories
                 where tenant = ?1 and memory_id = ?2",
                params![tenant, memory_id],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                    last_validated_at
                 from memories
                 where tenant = ?1 and memory_id = ?2 and status = ?3",
                params![
                    tenant,
                    memory_id,
                    encode_text(&MemoryStatus::PendingConfirmation)?
                ],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let memory = conn
            .query_row(
                "select
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                    last_validated_at
                 from memories
                 where tenant = ?1
                   and (((?2 is not null and idempotency_key = ?2) or content_hash = ?3))
                 order by
                    case when ?2 is not null and idempotency_key = ?2 then 0 else 1 end,
                    updated_at desc
                 limit 1",
                params![tenant, idempotency_key.as_deref(), content_hash],
                map_memory_row,
            )
            .optional()?;

        Ok(memory)
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                last_validated_at
             from memories
             where tenant = ?1 and status = ?2
             order by created_at desc",
        )?;
        let rows = stmt.query_map(
            params![tenant, encode_text(&MemoryStatus::PendingConfirmation)?],
            map_memory_row,
        )?;
        let collected = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(collected)
    }

    pub async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select
                memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                content, evidence_json, code_refs_json, project, repo, module, task_type,
                tags_json, confidence, decay_score, content_hash, idempotency_key,
                session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                last_validated_at
             from memories
             where tenant = ?1
             order by updated_at desc, version desc, memory_id asc",
        )?;
        let rows = stmt.query_map(params![tenant], map_memory_row)?;
        let candidates = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|memory| {
                !matches!(
                    memory.status,
                    MemoryStatus::Rejected | MemoryStatus::Archived
                )
            })
            .collect::<Vec<_>>();
        Ok(candidates)
    }

    /// Returns [`MemoryRecord`] rows for the given ids, filtered to a tenant and
    /// the standard "live" status set (excludes `rejected` and `archived`).
    /// Used by the rewritten `semantic_search_memories` (Task 14) as an ANN post-filter.
    pub async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let conn = self.conn()?;

        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select
                memory_id, tenant, memory_type, status, scope, visibility, version,
                summary, content, evidence_json, code_refs_json, project, repo,
                module, task_type, tags_json, confidence, decay_score, content_hash,
                idempotency_key, session_id, supersedes_memory_id, source_agent,
                created_at, updated_at, last_validated_at
             from memories
             where tenant = ?1
               and status not in ('rejected', 'archived')
               and memory_id in ({placeholders})"
        );

        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant.to_string())];
        for id in ids {
            params_vec.push(Box::new(id.to_string()));
        }
        let params_refs: Vec<&dyn duckdb::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();

        let rows = stmt.query_map(&params_refs[..], map_memory_row)?;
        let mut out = Vec::with_capacity(ids.len());
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, MemoryStatus::Active)
            .await
    }

    pub async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, MemoryStatus::Rejected)
            .await
    }

    pub async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        {
            let updated_at = current_timestamp();
            let conn = self.conn()?;
            let (jobs, embedding) = load_embedding_references(&conn, original_memory_id)?;
            delete_embedding_references(&conn, original_memory_id)?;
            let stored = successor.clone();
            let rows_updated = conn.execute(
                "update memories
                 set status = ?1, updated_at = ?2
                 where tenant = ?3 and memory_id = ?4 and status = ?5",
                params![
                    encode_text(&MemoryStatus::Rejected)?,
                    updated_at,
                    tenant,
                    original_memory_id,
                    encode_text(&MemoryStatus::PendingConfirmation)?,
                ],
            )?;

            if rows_updated == 0 {
                restore_embedding_references(&conn, &jobs, embedding.as_ref())?;
                return Err(StorageError::InvalidData("pending memory not found"));
            }

            conn.execute(
                "insert into memories (
                    memory_id, tenant, memory_type, status, scope, visibility, version, summary,
                    content, evidence_json, code_refs_json, project, repo, module, task_type,
                    tags_json, confidence, decay_score, content_hash, idempotency_key,
                    session_id, supersedes_memory_id, source_agent, created_at, updated_at,
                    last_validated_at
                ) values (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                    ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                    ?16, ?17, ?18, ?19, ?20,
                    ?21, ?22, ?23, ?24, ?25,
                    ?26
                )",
                params![
                    stored.memory_id,
                    stored.tenant,
                    encode_text(&stored.memory_type)?,
                    encode_text(&stored.status)?,
                    encode_text(&stored.scope)?,
                    encode_text(&stored.visibility)?,
                    stored.version as i64,
                    stored.summary,
                    stored.content,
                    encode_json(&stored.evidence)?,
                    encode_json(&stored.code_refs)?,
                    stored.project,
                    stored.repo,
                    stored.module,
                    stored.task_type,
                    encode_json(&stored.tags)?,
                    f64::from(stored.confidence),
                    f64::from(stored.decay_score),
                    stored.content_hash,
                    stored.idempotency_key,
                    stored.session_id,
                    stored.supersedes_memory_id,
                    stored.source_agent,
                    stored.created_at,
                    stored.updated_at,
                    stored.last_validated_at,
                ],
            )?;
            restore_embedding_references(&conn, &jobs, embedding.as_ref())?;
        }

        Ok(successor)
    }

    pub async fn insert_feedback(
        &self,
        feedback: FeedbackEvent,
    ) -> Result<FeedbackEvent, StorageError> {
        let conn = self.conn()?;
        let stored = feedback.clone();
        conn.execute(
            "insert into feedback_events (feedback_id, memory_id, feedback_kind, created_at)
             values (?1, ?2, ?3, ?4)",
            params![
                stored.feedback_id,
                stored.memory_id,
                stored.feedback_kind,
                stored.created_at
            ],
        )?;
        Ok(feedback)
    }

    pub async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        let adjustments = feedback_adjustments(&feedback.feedback_kind)
            .ok_or(StorageError::InvalidData("invalid feedback kind"))?;
        let updated_at = feedback.created_at.clone();
        let mut updated = memory.clone();
        updated.updated_at = updated_at.clone();
        updated.confidence = (updated.confidence + adjustments.confidence_delta).clamp(0.0, 1.0);
        updated.decay_score = (updated.decay_score + adjustments.decay_delta).clamp(0.0, 1.0);
        if let Some(status) = adjustments.status {
            updated.status = status;
        }
        if adjustments.mark_validated {
            updated.last_validated_at = Some(updated_at.clone());
        }

        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "insert into feedback_events (feedback_id, memory_id, feedback_kind, created_at)
             values (?1, ?2, ?3, ?4)",
            params![
                feedback.feedback_id,
                feedback.memory_id,
                feedback.feedback_kind,
                feedback.created_at
            ],
        )?;

        let rows_updated = tx.execute(
            "update memories
             set status = ?1,
                 confidence = ?2,
                 decay_score = ?3,
                 updated_at = ?4,
                 last_validated_at = ?5
             where tenant = ?6 and memory_id = ?7",
            params![
                encode_text(&updated.status)?,
                f64::from(updated.confidence),
                f64::from(updated.decay_score),
                updated.updated_at.clone(),
                updated.last_validated_at.clone(),
                updated.tenant.clone(),
                updated.memory_id.clone(),
            ],
        )?;

        if rows_updated == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }

        tx.commit()?;
        Ok(updated)
    }

    pub async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select feedback_id, memory_id, feedback_kind, created_at
             from feedback_events
             where memory_id = ?1
             order by created_at asc",
        )?;
        let rows = stmt.query_map(params![memory_id], |row| {
            Ok(FeedbackEvent {
                feedback_id: row.get(0)?,
                memory_id: row.get(1)?,
                feedback_kind: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        let collected = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(collected)
    }

    pub async fn insert_episode(
        &self,
        episode: EpisodeRecord,
    ) -> Result<EpisodeRecord, StorageError> {
        let conn = self.conn()?;
        let stored = episode.clone();
        conn.execute(
            "insert into episodes (
                episode_id, tenant, goal, steps_json, outcome, evidence_json, scope, visibility,
                project, repo, module, tags_json, source_agent, idempotency_key, created_at,
                updated_at, workflow_candidate_json
             ) values (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17
             )",
            params![
                stored.episode_id,
                stored.tenant,
                stored.goal,
                encode_json(&stored.steps)?,
                stored.outcome,
                encode_json(&stored.evidence)?,
                encode_text(&stored.scope)?,
                encode_text(&stored.visibility)?,
                stored.project,
                stored.repo,
                stored.module,
                encode_json(&stored.tags)?,
                stored.source_agent,
                stored.idempotency_key,
                stored.created_at,
                stored.updated_at,
                encode_optional_json(&stored.workflow_candidate)?,
            ],
        )?;
        Ok(episode)
    }

    pub async fn get_episode(
        &self,
        episode_id: &str,
    ) -> Result<Option<EpisodeRecord>, StorageError> {
        let conn = self.conn()?;
        let episode = conn
            .query_row(
                "select
                    episode_id, tenant, goal, steps_json, outcome, evidence_json, scope,
                    visibility, project, repo, module, tags_json, source_agent, idempotency_key,
                    created_at, updated_at, workflow_candidate_json
                 from episodes
                 where episode_id = ?1",
                params![episode_id],
                map_episode_row,
            )
            .optional()?;

        Ok(episode)
    }

    pub async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select
                episode_id, tenant, goal, steps_json, outcome, evidence_json, scope,
                visibility, project, repo, module, tags_json, source_agent, idempotency_key,
                created_at, updated_at, workflow_candidate_json
             from episodes
             where tenant = ?1 and lower(trim(outcome)) = 'success'
             order by created_at asc, episode_id asc",
        )?;
        let rows = stmt.query_map(params![tenant], map_episode_row)?;
        let collected = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(collected)
    }

    pub async fn list_memory_versions(
        &self,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let memory = self
            .get_memory(memory_id.to_string())
            .await?
            .ok_or(StorageError::InvalidData("memory not found"))?;

        self.list_memory_versions_for_tenant(&memory.tenant, memory_id)
            .await
    }

    pub async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "select memory_id, version, status, updated_at, supersedes_memory_id
             from memories
             where tenant = ?1
             order by version desc, updated_at desc",
        )?;
        let rows = stmt.query_map(params![tenant], |row| {
            Ok(MemoryVersionLink {
                memory_id: row.get(0)?,
                version: to_u64(row.get::<_, i64>(1)?).map_err(to_from_sql_error)?,
                status: decode_text(&row.get::<_, String>(2)?).map_err(to_from_sql_error)?,
                updated_at: row.get(3)?,
                supersedes_memory_id: row.get(4)?,
            })
        })?;
        let all_versions = rows.collect::<Result<Vec<_>, _>>()?;
        let mut by_id = HashMap::new();
        let mut neighbors: HashMap<String, Vec<String>> = HashMap::new();

        for version in all_versions {
            let current_id = version.memory_id.clone();
            if let Some(parent_id) = version.supersedes_memory_id.clone() {
                neighbors
                    .entry(current_id.clone())
                    .or_default()
                    .push(parent_id.clone());
                neighbors
                    .entry(parent_id)
                    .or_default()
                    .push(current_id.clone());
            }
            by_id.insert(current_id, version);
        }

        if !by_id.contains_key(memory_id) {
            return Err(StorageError::InvalidData("memory not found"));
        }

        let mut queue = VecDeque::from([memory_id.to_string()]);
        let mut connected = HashSet::new();

        while let Some(current_id) = queue.pop_front() {
            if !connected.insert(current_id.clone()) {
                continue;
            }

            if let Some(next_ids) = neighbors.get(&current_id) {
                for next_id in next_ids {
                    queue.push_back(next_id.clone());
                }
            }
        }

        let mut collected = connected
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect::<Vec<_>>();
        collected.sort_by(|left, right| {
            right
                .version
                .cmp(&left.version)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
        });
        Ok(collected)
    }

    pub async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        let feedback = self.list_feedback_for_memory(memory_id).await?;
        let mut summary = FeedbackSummary::default();
        for event in feedback {
            summary.total += 1;
            match event.feedback_kind.as_str() {
                "useful" => summary.useful += 1,
                "outdated" => summary.outdated += 1,
                "incorrect" => summary.incorrect += 1,
                "applies_here" => summary.applies_here += 1,
                "does_not_apply_here" => summary.does_not_apply_here += 1,
                _ => {}
            }
        }
        Ok(summary)
    }

    pub(crate) fn conn(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.conn
            .lock()
            .map_err(|_| StorageError::InvalidData("duckdb connection mutex poisoned"))
    }

    async fn update_status(
        &self,
        tenant: &str,
        memory_id: &str,
        status: MemoryStatus,
    ) -> Result<MemoryRecord, StorageError> {
        let updated_at = current_timestamp();
        {
            let conn = self.conn()?;
            let (jobs, embedding) = load_embedding_references(&conn, memory_id)?;
            delete_embedding_references(&conn, memory_id)?;
            let rows_updated = conn.execute(
                "update memories
                 set status = ?1, updated_at = ?2
                 where tenant = ?3 and memory_id = ?4 and status = ?5",
                params![
                    encode_text(&status)?,
                    updated_at,
                    tenant,
                    memory_id,
                    encode_text(&MemoryStatus::PendingConfirmation)?,
                ],
            )?;

            if rows_updated == 0 {
                restore_embedding_references(&conn, &jobs, embedding.as_ref())?;
                return Err(StorageError::InvalidData("pending memory not found"));
            }
            restore_embedding_references(&conn, &jobs, embedding.as_ref())?;
        }

        self.get_memory(memory_id.to_string())
            .await?
            .ok_or(StorageError::InvalidData("updated memory not found"))
    }

    /// Return the most recently touched open session for `(tenant, caller_agent)`, or `None`.
    pub async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        let conn = self.conn()?;
        let session = conn
            .query_row(
                "SELECT session_id, tenant, caller_agent, started_at, last_seen_at,
                        ended_at, goal, memory_count
                 FROM sessions
                 WHERE tenant = ?1 AND caller_agent = ?2 AND ended_at IS NULL
                 ORDER BY last_seen_at DESC
                 LIMIT 1",
                params![tenant, caller_agent],
                |row| {
                    let mc: i64 = row.get(7)?;
                    Ok(Session {
                        session_id: row.get(0)?,
                        tenant: row.get(1)?,
                        caller_agent: row.get(2)?,
                        started_at: row.get(3)?,
                        last_seen_at: row.get(4)?,
                        ended_at: row.get(5)?,
                        goal: row.get(6)?,
                        memory_count: mc as u32,
                    })
                },
            )
            .optional()?;
        Ok(session)
    }

    /// Insert a new session row and return it.
    pub async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO sessions
                (session_id, tenant, caller_agent, started_at, last_seen_at, memory_count)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)",
            params![session_id, tenant, caller_agent, now, now],
        )?;
        Ok(Session {
            session_id: session_id.to_string(),
            tenant: tenant.to_string(),
            caller_agent: caller_agent.to_string(),
            started_at: now.to_string(),
            last_seen_at: now.to_string(),
            ended_at: None,
            goal: None,
            memory_count: 0,
        })
    }

    /// Close an open session by setting `ended_at`.  No-op if already closed.
    pub async fn close_session(
        &self,
        session_id: &str,
        ended_at: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE session_id = ?2 AND ended_at IS NULL",
            params![ended_at, session_id],
        )?;
        Ok(())
    }

    /// Bump `last_seen_at` and increment `memory_count` for an open session.
    pub async fn touch_session(
        &self,
        session_id: &str,
        last_seen_at: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE sessions
             SET last_seen_at = ?1, memory_count = memory_count + 1
             WHERE session_id = ?2",
            params![last_seen_at, session_id],
        )?;
        Ok(())
    }
}

impl EmbeddingRowSource for DuckDbRepository {
    fn count_total_memory_embeddings(&self) -> Result<i64, StorageError> {
        let conn = self.conn()?;
        let count: i64 =
            conn.query_row("select count(*) from memory_embeddings", params![], |row| {
                row.get(0)
            })?;
        Ok(count)
    }

    fn for_each_embedding(
        &self,
        _batch: usize,
        f: &mut dyn FnMut(&str, &[u8]) -> Result<(), StorageError>,
    ) -> Result<(), StorageError> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("select memory_id, embedding from memory_embeddings order by memory_id")?;
        let mut rows = stmt.query(params![])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            f(&id, &blob)?;
        }
        Ok(())
    }
}

fn load_embedding_references(
    conn: &Connection,
    memory_id: &str,
) -> Result<(Vec<EmbeddingJobRow>, Option<MemoryEmbeddingRow>), StorageError> {
    let mut jobs_stmt = conn.prepare(
        "select
            job_id, tenant, memory_id, target_content_hash, provider, status,
            attempt_count, last_error, available_at, created_at, updated_at
         from embedding_jobs
         where memory_id = ?1",
    )?;
    let jobs_iter = jobs_stmt.query_map(params![memory_id], |row| {
        Ok(EmbeddingJobRow {
            job_id: row.get(0)?,
            tenant: row.get(1)?,
            memory_id: row.get(2)?,
            target_content_hash: row.get(3)?,
            provider: row.get(4)?,
            status: row.get(5)?,
            attempt_count: row.get(6)?,
            last_error: row.get(7)?,
            available_at: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        })
    })?;
    let jobs = jobs_iter.collect::<Result<Vec<_>, _>>()?;

    let embedding = conn
        .query_row(
            "select
                memory_id, tenant, embedding_model, embedding_dim, embedding,
                content_hash, source_updated_at, created_at, updated_at
             from memory_embeddings
             where memory_id = ?1",
            params![memory_id],
            |row| {
                Ok(MemoryEmbeddingRow {
                    memory_id: row.get(0)?,
                    tenant: row.get(1)?,
                    embedding_model: row.get(2)?,
                    embedding_dim: row.get(3)?,
                    embedding: row.get(4)?,
                    content_hash: row.get(5)?,
                    source_updated_at: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            },
        )
        .optional()?;

    Ok((jobs, embedding))
}

fn delete_embedding_references(conn: &Connection, memory_id: &str) -> Result<(), StorageError> {
    conn.execute(
        "delete from embedding_jobs where memory_id = ?1",
        params![memory_id],
    )?;
    conn.execute(
        "delete from memory_embeddings where memory_id = ?1",
        params![memory_id],
    )?;
    Ok(())
}

fn restore_embedding_references(
    conn: &Connection,
    jobs: &[EmbeddingJobRow],
    embedding: Option<&MemoryEmbeddingRow>,
) -> Result<(), StorageError> {
    for job in jobs {
        conn.execute(
            "insert into embedding_jobs (
                job_id, tenant, memory_id, target_content_hash, provider, status,
                attempt_count, last_error, available_at, created_at, updated_at
            ) values (
                ?1, ?2, ?3, ?4, ?5, ?6,
                ?7, ?8, ?9, ?10, ?11
            )",
            params![
                &job.job_id,
                &job.tenant,
                &job.memory_id,
                &job.target_content_hash,
                &job.provider,
                &job.status,
                job.attempt_count,
                &job.last_error,
                &job.available_at,
                &job.created_at,
                &job.updated_at,
            ],
        )?;
    }

    if let Some(embedding) = embedding {
        conn.execute(
            "insert into memory_embeddings (
                memory_id, tenant, embedding_model, embedding_dim, embedding,
                content_hash, source_updated_at, created_at, updated_at
            ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                &embedding.memory_id,
                &embedding.tenant,
                &embedding.embedding_model,
                embedding.embedding_dim,
                &embedding.embedding,
                &embedding.content_hash,
                &embedding.source_updated_at,
                &embedding.created_at,
                &embedding.updated_at,
            ],
        )?;
    }

    Ok(())
}

fn map_memory_with_blob(row: &duckdb::Row<'_>) -> Result<(MemoryRecord, Vec<u8>), duckdb::Error> {
    let memory = map_memory_row(row)?;
    let blob: Vec<u8> = row.get(26)?;
    Ok((memory, blob))
}

fn decode_f32_blob(blob: &[u8], expected_len: usize) -> Result<Vec<f32>, StorageError> {
    let expected_bytes = expected_len
        .checked_mul(4)
        .ok_or(StorageError::InvalidData("embedding dimension overflow"))?;
    if blob.len() != expected_bytes {
        return Err(StorageError::InvalidData("embedding blob length mismatch"));
    }
    let mut out = Vec::with_capacity(expected_len);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

/// Recompute `memories.content_hash` for any row whose hash predates the
/// sha256 switch (closes mempalace-diff §8 #1). Old rows used a 16-char
/// `DefaultHasher` digest with a per-process random seed — meaningful only
/// within the writing process, useless for dedupe across restarts. Detected
/// by `length(content_hash) != CONTENT_HASH_LEN`.
///
/// Also propagates the new hash to `memory_embeddings.content_hash` for the
/// same `memory_id`. The embedding contents didn't change, only the hash
/// function did, so we keep the existing vector and just refresh the staleness
/// sentinel — otherwise every legacy embedding would look stale and trigger a
/// pointless re-embed at next worker tick.
///
/// `embedding_jobs.target_content_hash` is intentionally left alone: pending
/// jobs targeting old hashes will be marked `stale` by the worker on the next
/// tick (it compares against the now-fresh `memories.content_hash`), which is
/// exactly the desired outcome.
pub(crate) fn migrate_content_hash_to_sha256(conn: &Connection) -> Result<usize, StorageError> {
    let mut stmt = conn.prepare(
        "select
            memory_id, tenant, memory_type, status, scope, visibility, version, summary,
            content, evidence_json, code_refs_json, project, repo, module, task_type,
            tags_json, confidence, decay_score, content_hash, idempotency_key,
            session_id, supersedes_memory_id, source_agent, created_at, updated_at,
            last_validated_at
         from memories
         where length(content_hash) != ?1",
    )?;
    let legacy: Vec<MemoryRecord> = stmt
        .query_map(params![CONTENT_HASH_LEN as i64], map_memory_row)?
        .collect::<Result<Vec<_>, _>>()?;
    if legacy.is_empty() {
        return Ok(0);
    }
    let n = legacy.len();
    for record in legacy {
        let new_hash = compute_content_hash_from_record(&record);
        conn.execute(
            "update memories set content_hash = ?1 where memory_id = ?2",
            params![new_hash, record.memory_id],
        )?;
        conn.execute(
            "update memory_embeddings set content_hash = ?1 where memory_id = ?2",
            params![new_hash, record.memory_id],
        )?;
    }
    Ok(n)
}

fn map_memory_row(row: &duckdb::Row<'_>) -> Result<MemoryRecord, duckdb::Error> {
    Ok(MemoryRecord {
        memory_id: row.get(0)?,
        tenant: row.get(1)?,
        memory_type: decode_text(&row.get::<_, String>(2)?).map_err(to_from_sql_error)?,
        status: decode_text(&row.get::<_, String>(3)?).map_err(to_from_sql_error)?,
        scope: decode_text(&row.get::<_, String>(4)?).map_err(to_from_sql_error)?,
        visibility: decode_text(&row.get::<_, String>(5)?).map_err(to_from_sql_error)?,
        version: to_u64(row.get::<_, i64>(6)?).map_err(to_from_sql_error)?,
        summary: row.get(7)?,
        content: row.get(8)?,
        evidence: decode_json(&row.get::<_, String>(9)?).map_err(to_from_sql_error)?,
        code_refs: decode_json(&row.get::<_, String>(10)?).map_err(to_from_sql_error)?,
        project: row.get(11)?,
        repo: row.get(12)?,
        module: row.get(13)?,
        task_type: row.get(14)?,
        tags: decode_json(&row.get::<_, String>(15)?).map_err(to_from_sql_error)?,
        confidence: row.get::<_, f64>(16)? as f32,
        decay_score: row.get::<_, f64>(17)? as f32,
        content_hash: row.get(18)?,
        idempotency_key: row.get(19)?,
        session_id: row.get(20)?,
        supersedes_memory_id: row.get(21)?,
        source_agent: row.get(22)?,
        created_at: row.get(23)?,
        updated_at: row.get(24)?,
        last_validated_at: row.get(25)?,
    })
}

fn map_episode_row(row: &duckdb::Row<'_>) -> Result<EpisodeRecord, duckdb::Error> {
    Ok(EpisodeRecord {
        episode_id: row.get(0)?,
        tenant: row.get(1)?,
        goal: row.get(2)?,
        steps: decode_json(&row.get::<_, String>(3)?).map_err(to_from_sql_error)?,
        outcome: row.get(4)?,
        evidence: decode_json(&row.get::<_, String>(5)?).map_err(to_from_sql_error)?,
        scope: decode_text(&row.get::<_, String>(6)?).map_err(to_from_sql_error)?,
        visibility: decode_text(&row.get::<_, String>(7)?).map_err(to_from_sql_error)?,
        project: row.get(8)?,
        repo: row.get(9)?,
        module: row.get(10)?,
        tags: decode_json(&row.get::<_, String>(11)?).map_err(to_from_sql_error)?,
        source_agent: row.get(12)?,
        idempotency_key: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
        workflow_candidate: decode_optional_json(row.get::<_, Option<String>>(16)?)
            .map_err(to_from_sql_error)?,
    })
}

fn encode_json<T: Serialize>(value: &T) -> Result<String, StorageError> {
    Ok(serde_json::to_string(value)?)
}

fn encode_optional_json<T: Serialize>(value: &Option<T>) -> Result<Option<String>, StorageError> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn decode_json<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    Ok(serde_json::from_str(value)?)
}

fn decode_optional_json<T: DeserializeOwned>(
    value: Option<String>,
) -> Result<Option<T>, StorageError> {
    value
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(Into::into)
}

fn encode_text<T: Serialize>(value: &T) -> Result<String, StorageError> {
    let value = serde_json::to_value(value)?;
    match value {
        Value::String(value) => Ok(value),
        _ => Err(StorageError::InvalidData(
            "expected string-compatible value",
        )),
    }
}

fn decode_text<T: DeserializeOwned>(value: &str) -> Result<T, StorageError> {
    Ok(serde_json::from_value(Value::String(value.to_owned()))?)
}

fn to_u64(value: i64) -> Result<u64, StorageError> {
    u64::try_from(value)
        .map_err(|_| StorageError::InvalidData("negative integer in unsigned field"))
}

fn to_from_sql_error(error: StorageError) -> duckdb::Error {
    duckdb::Error::FromSqlConversionFailure(0, duckdb::types::Type::Text, Box::new(error))
}

fn current_timestamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis();
    format!("{millis:020}")
}

struct FeedbackAdjustments {
    confidence_delta: f32,
    decay_delta: f32,
    status: Option<MemoryStatus>,
    mark_validated: bool,
}

fn feedback_adjustments(feedback_kind: &str) -> Option<FeedbackAdjustments> {
    let feedback_kind = decode_feedback_kind(feedback_kind)?;
    Some(FeedbackAdjustments {
        confidence_delta: feedback_kind.confidence_delta(),
        decay_delta: feedback_kind.decay_delta(),
        status: if feedback_kind.archived_status() {
            Some(MemoryStatus::Archived)
        } else {
            None
        },
        mark_validated: feedback_kind.marks_validated(),
    })
}

fn decode_feedback_kind(value: &str) -> Option<FeedbackKind> {
    match value {
        "useful" => Some(FeedbackKind::Useful),
        "outdated" => Some(FeedbackKind::Outdated),
        "incorrect" => Some(FeedbackKind::Incorrect),
        "applies_here" => Some(FeedbackKind::AppliesHere),
        "does_not_apply_here" => Some(FeedbackKind::DoesNotApplyHere),
        _ => None,
    }
}
