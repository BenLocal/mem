//! LanceDB backend (skeleton).
//!
//! `LanceDbRepository` is the alternate backend to [`crate::storage::DuckDbRepository`].
//! It implements the same four traits — `MemoryRepository`,
//! `TranscriptRepository`, `EntityRegistry`, `GraphStore` — so all upper
//! layers (services, HTTP handlers) work against it interchangeably.
//!
//! **Status:** real implementations exist for `open()` (creates `memories`
//! table), `MemoryRepository::insert_memory`, and
//! `MemoryRepository::get_memory_for_tenant`. Round-trip is end-to-end
//! verified by `lancedb_insert_and_get_memory_round_trip` in this
//! module's `#[cfg(test)] mod tests`. All other methods are still
//! `unimplemented!()` with TODO hints — each future implementation:
//!
//!   1. add `ensure_<table>_table` to `open()` for the table the method touches
//!   2. extend the `*_to_record_batch` / `record_batch_to_*` helpers
//!      (or write new ones for new tables)
//!   3. write the method body using `Connection::open_table` + `Table::add` /
//!      `Table::query().only_if(...)` / `Table::vector_search(...)`
//!   4. add a parity test against DuckDB
//!
//! Helpers `memories_to_record_batch` / `record_batch_to_memories` /
//! `enum_to_str` / `enum_from_str` / `sql_quote` are reusable across
//! upcoming methods.
//!
//! **Schema mapping** (planned, not yet enforced):
//!
//! | mem table                          | LanceDB table                  |
//! |------------------------------------|--------------------------------|
//! | memories                           | `memories`                     |
//! | embedding_jobs                     | `embedding_jobs`               |
//! | memory_embeddings                  | `memory_embeddings` (vector col)|
//! | conversation_messages              | `conversation_messages`        |
//! | conversation_message_embeddings    | `conversation_message_embeddings` (vector col) |
//! | transcript_embedding_jobs          | `transcript_embedding_jobs`    |
//! | feedback_events                    | `feedback_events`              |
//! | episodes                           | `episodes`                     |
//! | sessions                           | `sessions`                     |
//! | entities                           | `entities`                     |
//! | entity_aliases                     | `entity_aliases`               |
//! | graph_edges                        | `graph_edges`                  |
//!
//! Vector columns use LanceDB's native vector type — no separate HNSW
//! sidecar; ANN is built-in.
//!
//! **Compile-time:** behind a `lancedb` Cargo feature. The default mem
//! build does not pull in lance/arrow.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{
    builder::{Float32Builder, ListBuilder, StringBuilder, UInt64Builder},
    Array, Float32Array, ListArray, RecordBatch, StringArray, UInt64Array,
};
use async_trait::async_trait;
use futures::TryStreamExt;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::Connection;
use serde::{de::DeserializeOwned, Serialize};

use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::memory::{FeedbackSummary, GraphEdge, MemoryRecord, MemoryVersionLink};
use crate::domain::session::Session;
use crate::domain::ConversationMessage;
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::storage::duckdb::{ClaimedEmbeddingJob, EmbeddingJobInsert, EntityRegistry};
use crate::storage::{
    ContextWindow, FeedbackEvent, GraphError, GraphStore, MemoryRepository, StorageError,
    TranscriptRepository, TranscriptSessionSummary,
};

/// LanceDB-backed implementation of the storage trait surface.
///
/// Holds an open `lancedb::Connection`. All async DB operations route
/// through it; there is no equivalent of the DuckDB single-Mutex write
/// connection because LanceDB is itself async-native and handles
/// concurrency internally.
#[derive(Clone)]
pub struct LanceDbRepository {
    /// LanceDB connection. Currently unused — every trait method is
    /// `unimplemented!()` placeholder. The first real method to write is
    /// `open()` (creates / opens the schema tables); afterwards method
    /// bodies will hit `self.conn.open_table(...)` etc.
    #[allow(dead_code)]
    conn: Arc<Connection>,
}

impl LanceDbRepository {
    /// Open (or create) a LanceDB store at the given path.
    ///
    /// Connects, then idempotently creates all backend tables. Currently
    /// only the `memories` table schema is defined; remaining tables get
    /// added as their corresponding trait methods become non-stub.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let uri = path
            .to_str()
            .ok_or(StorageError::InvalidData("lancedb path must be UTF-8"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = lancedb::connect(uri).execute().await.map_err(lancedb_err)?;

        ensure_memories_table(&conn).await?;
        // TODO: ensure_embedding_jobs_table, ensure_memory_embeddings_table,
        // ensure_conversation_messages_table, ensure_*…
        // (10 more tables — add as the corresponding trait methods leave
        //  unimplemented!() state).

        Ok(Self {
            conn: Arc::new(conn),
        })
    }
}

/// Map a `lancedb::Error` into our generic [`StorageError`]. We lose the
/// rich variant info but the upper layers don't care — they only branch
/// on `NotFound` vs everything-else.
fn lancedb_err(e: lancedb::Error) -> StorageError {
    StorageError::InvalidInput(format!("lancedb: {e}"))
}

/// Arrow schema for the `memories` LanceDB table. One column per
/// [`MemoryRecord`] field; enums (`memory_type`, `status`, `scope`,
/// `visibility`) are stored as their `serde` snake_case representation
/// for symmetry with the JSON-string encoding the DuckDB backend uses
/// in its `text` columns.
fn memories_schema() -> Schema {
    let str_list = DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
    Schema::new(vec![
        Field::new("memory_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("memory_type", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("scope", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("version", DataType::UInt64, false),
        Field::new("summary", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("evidence", str_list.clone(), false),
        Field::new("code_refs", str_list.clone(), false),
        Field::new("project", DataType::Utf8, true),
        Field::new("repo", DataType::Utf8, true),
        Field::new("module", DataType::Utf8, true),
        Field::new("task_type", DataType::Utf8, true),
        Field::new("tags", str_list.clone(), false),
        Field::new("topics", str_list, false),
        Field::new("confidence", DataType::Float32, false),
        Field::new("decay_score", DataType::Float32, false),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("idempotency_key", DataType::Utf8, true),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("supersedes_memory_id", DataType::Utf8, true),
        Field::new("source_agent", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        Field::new("last_validated_at", DataType::Utf8, true),
    ])
}

async fn ensure_memories_table(conn: &Connection) -> Result<(), StorageError> {
    let names = conn.table_names().execute().await.map_err(lancedb_err)?;
    if names.iter().any(|n| n == "memories") {
        return Ok(());
    }
    let schema = Arc::new(memories_schema());
    conn.create_empty_table("memories", schema)
        .execute()
        .await
        .map_err(lancedb_err)?;
    Ok(())
}

/// Mirror of the DuckDB `encode_text` helper: serialize a snake_case-encoded
/// enum (e.g. `MemoryType`, `MemoryStatus`) to its plain JSON string token.
fn enum_to_str<T: Serialize>(v: &T) -> Result<String, StorageError> {
    serde_json::to_value(v)
        .map_err(StorageError::Serde)?
        .as_str()
        .map(|s| s.to_string())
        .ok_or(StorageError::InvalidData(
            "expected string serialization for enum",
        ))
}

/// Inverse of `enum_to_str`. Used when materializing a `MemoryRecord` from
/// a `RecordBatch` row.
fn enum_from_str<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(StorageError::Serde)
}

/// Serialize one or more `MemoryRecord`s to an Arrow `RecordBatch` matching
/// the [`memories_schema`] layout. Used by `insert_memory` to feed
/// `Table::add(...)`.
fn memories_to_record_batch(memories: &[MemoryRecord]) -> Result<RecordBatch, StorageError> {
    let mut memory_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut memory_type = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut scope = StringBuilder::new();
    let mut visibility = StringBuilder::new();
    let mut version = UInt64Builder::new();
    let mut summary = StringBuilder::new();
    let mut content = StringBuilder::new();
    let mut evidence = ListBuilder::new(StringBuilder::new());
    let mut code_refs = ListBuilder::new(StringBuilder::new());
    let mut project = StringBuilder::new();
    let mut repo = StringBuilder::new();
    let mut module = StringBuilder::new();
    let mut task_type = StringBuilder::new();
    let mut tags = ListBuilder::new(StringBuilder::new());
    let mut topics = ListBuilder::new(StringBuilder::new());
    let mut confidence = Float32Builder::new();
    let mut decay_score = Float32Builder::new();
    let mut content_hash = StringBuilder::new();
    let mut idempotency_key = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut supersedes_memory_id = StringBuilder::new();
    let mut source_agent = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    let mut last_validated_at = StringBuilder::new();

    for m in memories {
        memory_id.append_value(&m.memory_id);
        tenant.append_value(&m.tenant);
        memory_type.append_value(enum_to_str(&m.memory_type)?);
        status.append_value(enum_to_str(&m.status)?);
        scope.append_value(enum_to_str(&m.scope)?);
        visibility.append_value(enum_to_str(&m.visibility)?);
        version.append_value(m.version);
        summary.append_value(&m.summary);
        content.append_value(&m.content);
        for s in &m.evidence {
            evidence.values().append_value(s);
        }
        evidence.append(true);
        for s in &m.code_refs {
            code_refs.values().append_value(s);
        }
        code_refs.append(true);
        match &m.project {
            Some(s) => project.append_value(s),
            None => project.append_null(),
        }
        match &m.repo {
            Some(s) => repo.append_value(s),
            None => repo.append_null(),
        }
        match &m.module {
            Some(s) => module.append_value(s),
            None => module.append_null(),
        }
        match &m.task_type {
            Some(s) => task_type.append_value(s),
            None => task_type.append_null(),
        }
        for s in &m.tags {
            tags.values().append_value(s);
        }
        tags.append(true);
        for s in &m.topics {
            topics.values().append_value(s);
        }
        topics.append(true);
        confidence.append_value(m.confidence);
        decay_score.append_value(m.decay_score);
        content_hash.append_value(&m.content_hash);
        match &m.idempotency_key {
            Some(s) => idempotency_key.append_value(s),
            None => idempotency_key.append_null(),
        }
        match &m.session_id {
            Some(s) => session_id.append_value(s),
            None => session_id.append_null(),
        }
        match &m.supersedes_memory_id {
            Some(s) => supersedes_memory_id.append_value(s),
            None => supersedes_memory_id.append_null(),
        }
        source_agent.append_value(&m.source_agent);
        created_at.append_value(&m.created_at);
        updated_at.append_value(&m.updated_at);
        match &m.last_validated_at {
            Some(s) => last_validated_at.append_value(s),
            None => last_validated_at.append_null(),
        }
    }

    let schema = Arc::new(memories_schema());
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(memory_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(memory_type.finish()),
        Arc::new(status.finish()),
        Arc::new(scope.finish()),
        Arc::new(visibility.finish()),
        Arc::new(version.finish()),
        Arc::new(summary.finish()),
        Arc::new(content.finish()),
        Arc::new(evidence.finish()),
        Arc::new(code_refs.finish()),
        Arc::new(project.finish()),
        Arc::new(repo.finish()),
        Arc::new(module.finish()),
        Arc::new(task_type.finish()),
        Arc::new(tags.finish()),
        Arc::new(topics.finish()),
        Arc::new(confidence.finish()),
        Arc::new(decay_score.finish()),
        Arc::new(content_hash.finish()),
        Arc::new(idempotency_key.finish()),
        Arc::new(session_id.finish()),
        Arc::new(supersedes_memory_id.finish()),
        Arc::new(source_agent.finish()),
        Arc::new(created_at.finish()),
        Arc::new(updated_at.finish()),
        Arc::new(last_validated_at.finish()),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| StorageError::InvalidInput(format!("memories record batch: {e}")))
}

/// Inverse of `memories_to_record_batch`: parse a Lance query result into
/// `MemoryRecord`s.
fn record_batch_to_memories(batch: &RecordBatch) -> Result<Vec<MemoryRecord>, StorageError> {
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
    let memory_id = col::<StringArray>(batch, "memory_id")?;
    let tenant = col::<StringArray>(batch, "tenant")?;
    let memory_type = col::<StringArray>(batch, "memory_type")?;
    let status = col::<StringArray>(batch, "status")?;
    let scope = col::<StringArray>(batch, "scope")?;
    let visibility = col::<StringArray>(batch, "visibility")?;
    let version = col::<UInt64Array>(batch, "version")?;
    let summary = col::<StringArray>(batch, "summary")?;
    let content = col::<StringArray>(batch, "content")?;
    let evidence = col::<ListArray>(batch, "evidence")?;
    let code_refs = col::<ListArray>(batch, "code_refs")?;
    let project = col::<StringArray>(batch, "project")?;
    let repo = col::<StringArray>(batch, "repo")?;
    let module = col::<StringArray>(batch, "module")?;
    let task_type = col::<StringArray>(batch, "task_type")?;
    let tags = col::<ListArray>(batch, "tags")?;
    let topics = col::<ListArray>(batch, "topics")?;
    let confidence = col::<Float32Array>(batch, "confidence")?;
    let decay_score = col::<Float32Array>(batch, "decay_score")?;
    let content_hash = col::<StringArray>(batch, "content_hash")?;
    let idempotency_key = col::<StringArray>(batch, "idempotency_key")?;
    let session_id = col::<StringArray>(batch, "session_id")?;
    let supersedes_memory_id = col::<StringArray>(batch, "supersedes_memory_id")?;
    let source_agent = col::<StringArray>(batch, "source_agent")?;
    let created_at = col::<StringArray>(batch, "created_at")?;
    let updated_at = col::<StringArray>(batch, "updated_at")?;
    let last_validated_at = col::<StringArray>(batch, "last_validated_at")?;

    fn list_at(arr: &ListArray, i: usize) -> Result<Vec<String>, StorageError> {
        let inner = arr.value(i);
        let strs = inner
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or(StorageError::InvalidData("list inner type"))?;
        Ok((0..strs.len()).map(|j| strs.value(j).to_string()).collect())
    }
    let opt = |arr: &StringArray, i: usize| -> Option<String> {
        if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_string())
        }
    };

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(MemoryRecord {
            memory_id: memory_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            memory_type: enum_from_str(memory_type.value(i))?,
            status: enum_from_str(status.value(i))?,
            scope: enum_from_str(scope.value(i))?,
            visibility: enum_from_str(visibility.value(i))?,
            version: version.value(i),
            summary: summary.value(i).to_string(),
            content: content.value(i).to_string(),
            evidence: list_at(evidence, i)?,
            code_refs: list_at(code_refs, i)?,
            project: opt(project, i),
            repo: opt(repo, i),
            module: opt(module, i),
            task_type: opt(task_type, i),
            tags: list_at(tags, i)?,
            topics: list_at(topics, i)?,
            confidence: confidence.value(i),
            decay_score: decay_score.value(i),
            content_hash: content_hash.value(i).to_string(),
            idempotency_key: opt(idempotency_key, i),
            session_id: opt(session_id, i),
            supersedes_memory_id: opt(supersedes_memory_id, i),
            source_agent: source_agent.value(i).to_string(),
            created_at: created_at.value(i).to_string(),
            updated_at: updated_at.value(i).to_string(),
            last_validated_at: opt(last_validated_at, i),
        });
    }
    Ok(out)
}

/// Escape a string literal for a LanceDB filter expression. LanceDB uses
/// DuckDB's predicate flavor — single quotes for string literals, doubled
/// to escape an embedded quote.
fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[async_trait]
#[allow(clippy::too_many_arguments)]
impl MemoryRepository for LanceDbRepository {
    async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
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

    async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        let _ = insert;
        unimplemented!("LanceDb::try_enqueue_embedding_job — see docs/repository.rs trait def")
    }

    async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let _ = memory_id;
        unimplemented!(
            "LanceDb::first_embedding_job_id_for_memory — see docs/repository.rs trait def"
        )
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        let _ = (now, max_retries, n);
        unimplemented!("LanceDb::claim_next_n_embedding_jobs — see docs/repository.rs trait def")
    }

    async fn upsert_memory_embedding(
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
        let _ = (
            memory_id,
            tenant,
            embedding_model,
            embedding_dim,
            embedding_blob,
            content_hash,
            source_updated_at,
            now,
        );
        unimplemented!("LanceDb::upsert_memory_embedding — see docs/repository.rs trait def")
    }

    async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::delete_memory_embedding — see docs/repository.rs trait def")
    }

    async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_memories_for_tenant — see docs/repository.rs trait def")
    }

    async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        let _ = (tenant, query_embedding, limit);
        unimplemented!(
            "LanceDb::semantic_search_memories — use Table::vector_search().column(\"embedding\").limit(limit).execute()"
        )
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        let _ = (job_id, now);
        unimplemented!("LanceDb::complete_embedding_job — see docs/repository.rs trait def")
    }

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        let _ = (job_id, now);
        unimplemented!("LanceDb::mark_embedding_job_stale — see docs/repository.rs trait def")
    }

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let _ = (job_id, new_attempt_count, last_error, available_at, now);
        unimplemented!(
            "LanceDb::reschedule_embedding_job_failure — see docs/repository.rs trait def"
        )
    }

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let _ = (job_id, new_attempt_count, last_error, now);
        unimplemented!("LanceDb::permanently_fail_embedding_job — see docs/repository.rs trait def")
    }

    async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError> {
        let _ = memory_id;
        unimplemented!(
            "LanceDb::delete_embedding_jobs_by_memory_id — see docs/repository.rs trait def"
        )
    }

    async fn get_memory_for_tenant(
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

    async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::get_pending — see docs/repository.rs trait def")
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = (tenant, idempotency_key, content_hash);
        unimplemented!("LanceDb::find_by_idempotency_or_hash — see docs/repository.rs trait def")
    }

    async fn list_pending_review(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_pending_review — see docs/repository.rs trait def")
    }

    async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::search_candidates — see docs/repository.rs trait def")
    }

    async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = (tenant, limit);
        unimplemented!("LanceDb::recent_active_memories — see docs/repository.rs trait def")
    }

    async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = (tenant, query, k);
        unimplemented!(
            "LanceDb::bm25_candidates — use LanceDB native FTS: \
             create_index([\"content\"], Index::FTS(FtsIndexBuilder::default())) on the \
             `memories` table at open(), then query via Table::query().full_text_search(query)"
        )
    }

    async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let _ = (tenant, ids);
        unimplemented!("LanceDb::fetch_memories_by_ids — see docs/repository.rs trait def")
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::accept_pending — see docs/repository.rs trait def")
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::reject_pending — see docs/repository.rs trait def")
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (tenant, original_memory_id, successor);
        unimplemented!("LanceDb::replace_pending_with_successor — see docs/repository.rs trait def")
    }

    async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        let _ = (memory, feedback);
        unimplemented!("LanceDb::apply_feedback — see docs/repository.rs trait def")
    }

    async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::list_feedback_for_memory — see docs/repository.rs trait def")
    }

    async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!(
            "LanceDb::list_memory_versions_for_tenant — see docs/repository.rs trait def"
        )
    }

    async fn feedback_summary(&self, memory_id: &str) -> Result<FeedbackSummary, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::feedback_summary — see docs/repository.rs trait def")
    }

    async fn delete_memory_hard(&self, tenant: &str, memory_id: &str) -> Result<(), StorageError> {
        let _ = (tenant, memory_id);
        unimplemented!("LanceDb::delete_memory_hard — see docs/repository.rs trait def")
    }

    async fn get_memory(&self, memory_id: String) -> Result<Option<MemoryRecord>, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::get_memory — see docs/repository.rs trait def")
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        let _ = episode;
        unimplemented!("LanceDb::insert_episode — see docs/repository.rs trait def")
    }

    async fn list_memory_ids_for_tenant(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_memory_ids_for_tenant — see docs/repository.rs trait def")
    }

    async fn touch_session(
        &self,
        session_id: &str,
        last_seen_at: &str,
    ) -> Result<(), StorageError> {
        let _ = (session_id, last_seen_at);
        unimplemented!("LanceDb::touch_session — see docs/repository.rs trait def")
    }

    async fn latest_active_session(
        &self,
        tenant: &str,
        caller_agent: &str,
    ) -> Result<Option<Session>, StorageError> {
        let _ = (tenant, caller_agent);
        unimplemented!("LanceDb::latest_active_session — see docs/repository.rs trait def")
    }

    async fn open_session(
        &self,
        session_id: &str,
        tenant: &str,
        caller_agent: &str,
        now: &str,
    ) -> Result<Session, StorageError> {
        let _ = (session_id, tenant, caller_agent, now);
        unimplemented!("LanceDb::open_session — see docs/repository.rs trait def")
    }

    async fn close_session(&self, session_id: &str, ended_at: &str) -> Result<(), StorageError> {
        let _ = (session_id, ended_at);
        unimplemented!("LanceDb::close_session — see docs/repository.rs trait def")
    }

    async fn list_successful_episodes_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<EpisodeRecord>, StorageError> {
        let _ = tenant;
        unimplemented!(
            "LanceDb::list_successful_episodes_for_tenant — see docs/repository.rs trait def"
        )
    }

    async fn list_embedding_jobs(
        &self,
        tenant: &str,
        status_filter: Option<&str>,
        memory_id_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<EmbeddingJobInfo>, StorageError> {
        let _ = (tenant, status_filter, memory_id_filter, limit);
        unimplemented!("LanceDb::list_embedding_jobs — see docs/repository.rs trait def")
    }

    async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        let _ = (tenant, memory_id, provider, now);
        unimplemented!(
            "LanceDb::stale_live_embedding_jobs_for_memory — see docs/repository.rs trait def"
        )
    }

    async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        let _ = memory_id;
        unimplemented!("LanceDb::get_memory_embedding_row — see docs/repository.rs trait def")
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let _ = (tenant, memory_id, target_content_hash);
        unimplemented!(
            "LanceDb::latest_embedding_job_status_for_hash — see docs/repository.rs trait def"
        )
    }
}

#[async_trait]
impl TranscriptRepository for LanceDbRepository {
    async fn create_conversation_message(
        &self,
        msg: &ConversationMessage,
    ) -> Result<(), StorageError> {
        let _ = msg;
        unimplemented!("LanceDb::create_conversation_message — see docs/repository.rs trait def")
    }

    async fn get_conversation_messages_by_session(
        &self,
        tenant: &str,
        session_id: &str,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, session_id);
        unimplemented!(
            "LanceDb::get_conversation_messages_by_session — see docs/repository.rs trait def"
        )
    }

    async fn get_conversation_messages_by_session_paged(
        &self,
        tenant: &str,
        session_id: &str,
        since: Option<&str>,
        until: Option<&str>,
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        let _ = (tenant, session_id, since, until, cursor, limit);
        unimplemented!("LanceDb::get_conversation_messages_by_session_paged — see docs/repository.rs trait def")
    }

    async fn list_transcript_sessions(
        &self,
        tenant: &str,
    ) -> Result<Vec<TranscriptSessionSummary>, StorageError> {
        let _ = tenant;
        unimplemented!("LanceDb::list_transcript_sessions — see docs/repository.rs trait def")
    }

    async fn fetch_conversation_messages_by_ids(
        &self,
        tenant: &str,
        ids: &[String],
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, ids);
        unimplemented!(
            "LanceDb::fetch_conversation_messages_by_ids — see docs/repository.rs trait def"
        )
    }

    async fn context_window_for_block(
        &self,
        tenant: &str,
        primary_id: &str,
        k_before: usize,
        k_after: usize,
        include_tool_blocks: bool,
    ) -> Result<ContextWindow, StorageError> {
        let _ = (tenant, primary_id, k_before, k_after, include_tool_blocks);
        unimplemented!("LanceDb::context_window_for_block — see docs/repository.rs trait def")
    }

    async fn anchor_session_candidates(
        &self,
        tenant: &str,
        session_id: &str,
        k: usize,
    ) -> Result<Vec<String>, StorageError> {
        let _ = (tenant, session_id, k);
        unimplemented!("LanceDb::anchor_session_candidates — see docs/repository.rs trait def")
    }

    async fn recent_conversation_messages(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, limit);
        unimplemented!("LanceDb::recent_conversation_messages — see docs/repository.rs trait def")
    }

    async fn bm25_transcript_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<ConversationMessage>, StorageError> {
        let _ = (tenant, query, k);
        unimplemented!(
            "LanceDb::bm25_transcript_candidates — LanceDB native FTS over \
             `conversation_messages.content`. Same pattern as bm25_candidates."
        )
    }
}

#[async_trait]
impl GraphStore for LanceDbRepository {
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let _ = node_id;
        unimplemented!("LanceDb::neighbors — query graph_edges table where from_node_id = ? OR to_node_id = ? AND valid_to is null")
    }

    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError> {
        let _ = (edges, now);
        unimplemented!("LanceDb::sync_memory_edges — idempotent insert into graph_edges table")
    }

    async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let _ = memory_id;
        unimplemented!(
            "LanceDb::close_edges_for_memory — set valid_to = now where from_node_id = memory:<id>"
        )
    }

    async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        let _ = node_ids;
        unimplemented!("LanceDb::related_memory_ids — find memory: prefixed nodes connected to any of node_ids")
    }
}

#[async_trait]
impl EntityRegistry for LanceDbRepository {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        let _ = (tenant, alias, kind, now);
        unimplemented!("LanceDb::resolve_or_create — see docs/repository.rs trait def")
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let _ = (tenant, entity_id);
        unimplemented!("LanceDb::get_entity — see docs/repository.rs trait def")
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        let _ = (tenant, entity_id, alias, now);
        unimplemented!("LanceDb::add_alias — see docs/repository.rs trait def")
    }

    async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        let _ = (tenant, alias);
        unimplemented!("LanceDb::lookup_alias — see docs/repository.rs trait def")
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let _ = (tenant, kind_filter, query, limit);
        unimplemented!("LanceDb::list_entities — see docs/repository.rs trait def")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryStatus, MemoryType, Scope, Visibility};
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
    async fn lancedb_insert_and_get_memory_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceDbRepository::open(&path)
            .await
            .expect("open lancedb store");

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
}
