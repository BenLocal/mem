//! LanceDB backend (skeleton).
//!
//! `LanceStore` is the alternate backend to [`crate::storage::DuckDbRepository`].
//! It implements the same four traits — `MemoryRepository`,
//! `TranscriptRepository`, `EntityRegistry`, `GraphStore` — so all upper
//! layers (services, HTTP handlers) work against it interchangeably.
//!
//! **Status:** the read path on the `memories` table is fully working —
//! `open` creates the table, `insert_memory` writes a row, and 11
//! filter/lookup methods read back (`get_memory`, `get_memory_for_tenant`,
//! `get_pending`, `find_by_idempotency_or_hash`, `list_memories_for_tenant`,
//! `list_memory_ids_for_tenant`, `list_pending_review`, `search_candidates`,
//! `recent_active_memories`, `fetch_memories_by_ids`). Round-trip is
//! end-to-end verified by two tests in this module's `#[cfg(test)] mod
//! tests`. Mutating methods (accept/reject/supersede/apply_feedback) and
//! all non-`memories`-table methods are still `unimplemented!()` —
//! each future implementation:
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
    builder::{
        FixedSizeListBuilder, Float32Builder, Int64Builder, ListBuilder, StringBuilder,
        UInt64Builder,
    },
    Array, Float32Array, ListArray, RecordBatch, StringArray, UInt64Array,
};
use async_trait::async_trait;
use futures::TryStreamExt;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use lancedb::embeddings::{EmbeddingFunction, EmbeddingRegistry, MemoryRegistry};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{Connection, DistanceType};
use serde::{de::DeserializeOwned, Serialize};

mod embedding;

use crate::domain::embeddings::EmbeddingJobInfo;
use crate::domain::episode::EpisodeRecord;
use crate::domain::memory::{FeedbackSummary, GraphEdge, MemoryRecord, MemoryVersionLink};
use crate::domain::session::Session;
use crate::domain::{AddAliasOutcome, Entity, EntityKind, EntityWithAliases};
use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::storage::duckdb::{ClaimedEmbeddingJob, EmbeddingJobInsert, EntityRegistry};
use crate::storage::{
    ClaimedTranscriptEmbeddingJob, ContextWindow, FeedbackEvent, GraphError, GraphStore,
    MemoryRepository, StorageError, TranscriptRepository, TranscriptSessionSummary,
};

/// LanceDB-backed implementation of the storage trait surface.
///
/// Holds an open `lancedb::Connection`. All async DB operations route
/// through it; there is no equivalent of the DuckDB single-Mutex write
/// connection because LanceDB is itself async-native and handles
/// concurrency internally.
#[derive(Clone)]
pub struct LanceStore {
    /// LanceDB connection.
    conn: Arc<Connection>,
    /// Embedding-provider id for `transcript_embedding_jobs.provider`
    /// rows enqueued by [`Self::create_conversation_message`]. Set
    /// once at startup via [`Self::set_transcript_job_provider`]; if
    /// `None` when a transcript row is inserted with
    /// `embed_eligible == true`, `create_conversation_message`
    /// errors loudly rather than silently substituting a default
    /// that may diverge from the configured provider. Mirrors the
    /// legacy `DuckDbRepository` field of the same name.
    transcript_job_provider: Arc<std::sync::RwLock<Option<String>>>,
}

impl LanceStore {
    /// Open (or create) a LanceDB store at the given path.
    ///
    /// Connects, then idempotently creates all backend tables. Currently
    /// only the `memories` table schema is defined; remaining tables get
    /// added as their corresponding trait methods become non-stub.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_inner(path, None).await
    }

    /// Like [`Self::open`], but registers the given `EmbeddingProvider` as
    /// a LanceDB `EmbeddingFunction` named `"<provider>-<model>"`. Vector
    /// columns can then declare auto-embed against it via
    /// `EmbeddingDefinition::new("content", "<provider>-<model>", None)` —
    /// `Table::add(text_only_batch)` and
    /// `Table::vector_search(text_query)` will internally call back into
    /// the provider through this adapter.
    ///
    /// **Runtime requirement:** the adapter blocks an async embed call
    /// from inside LanceDB's sync `EmbeddingFunction` trait method via
    /// `tokio::task::block_in_place`. The caller must run on a
    /// **multi-thread** tokio runtime; calling this from a current-thread
    /// runtime will panic at first auto-embed.
    pub async fn open_with_provider(
        path: impl AsRef<Path>,
        provider: Arc<dyn crate::embedding::EmbeddingProvider>,
    ) -> Result<Self, StorageError> {
        Self::open_inner(path, Some(provider)).await
    }

    async fn open_inner(
        path: impl AsRef<Path>,
        provider: Option<Arc<dyn crate::embedding::EmbeddingProvider>>,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let uri = path
            .to_str()
            .ok_or(StorageError::InvalidData("lancedb path must be UTF-8"))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut builder = lancedb::connect(uri);
        if let Some(provider) = provider {
            let func = Arc::new(embedding::ProviderEmbeddingFunction::new(provider));
            let registry = Arc::new(MemoryRegistry::new());
            registry
                .register(func.name(), func.clone())
                .map_err(lancedb_err)?;
            builder = builder.embedding_registry(registry);
        }
        let conn = builder.execute().await.map_err(lancedb_err)?;

        ensure_memories_table(&conn).await?;
        ensure_feedback_events_table(&conn).await?;
        ensure_embedding_jobs_table(&conn).await?;
        ensure_graph_edges_table(&conn).await?;
        ensure_entities_table(&conn).await?;
        ensure_entity_aliases_table(&conn).await?;
        ensure_conversation_messages_table(&conn).await?;
        ensure_transcript_embedding_jobs_table(&conn).await?;
        // memory_embeddings is lazy-created on first upsert (dim is
        // provider-dependent and unknown here without provider).
        // TODO: ensure_episodes_table, ensure_sessions_table.

        // FTS indexes for the BM25 read paths. Built once at open
        // time on empty tables — building the index is cheap when
        // the table has no rows, and creating it up front lets the
        // DuckDB query layer (`storage::duckdb_query`) call
        // `lance_fts(...)` directly without first probing for an
        // index. After this, every subsequent open is a no-op:
        // `ensure_fts_index` checks `Table::list_indices` and skips
        // creation when the column is already indexed.
        ensure_fts_index(&conn, "memories", "content").await?;
        ensure_fts_index(&conn, "conversation_messages", "content").await?;

        Ok(Self {
            conn: Arc::new(conn),
            transcript_job_provider: Arc::new(std::sync::RwLock::new(None)),
        })
    }

    /// Configure the embedding provider id stamped on
    /// `transcript_embedding_jobs.provider` rows enqueued by
    /// [`Self::create_conversation_message`]. Called once during
    /// startup (typically from `app.rs` right after
    /// `Store::open_with_provider`). Until set, embed-eligible
    /// transcript writes return [`StorageError::InvalidData`] —
    /// failing loudly is preferable to silently writing with a
    /// default that mismatches the worker's
    /// `EmbeddingSettings::job_provider_id()`.
    pub fn set_transcript_job_provider(&self, provider: impl Into<String>) {
        *self
            .transcript_job_provider
            .write()
            .expect("transcript_job_provider lock poisoned") = Some(provider.into());
    }

    /// Read the configured transcript-job provider id, if any.
    pub(crate) fn transcript_job_provider(&self) -> Option<String> {
        self.transcript_job_provider
            .read()
            .expect("transcript_job_provider lock poisoned")
            .clone()
    }
}

/// Idempotently ensure an FTS (BM25 inverted) index exists on
/// `(table_name, column)`. The lance extension's `lance_fts(...)` SQL
/// table function returns empty results — without erroring — when no
/// FTS index is present on the queried column, which is a real-world
/// trap: a typo'd column name silently turns into "no matches". Pinning
/// the indexes at open() time means the DuckDB query side can call
/// `lance_fts` and trust empty results to mean "no matching rows."
async fn ensure_fts_index(
    conn: &Connection,
    table_name: &str,
    column: &str,
) -> Result<(), StorageError> {
    let table = conn
        .open_table(table_name)
        .execute()
        .await
        .map_err(lancedb_err)?;
    let indices = table.list_indices().await.map_err(lancedb_err)?;
    let already = indices
        .iter()
        .any(|c| c.columns.iter().any(|col| col == column));
    if already {
        return Ok(());
    }
    table
        .create_index(
            &[column],
            lancedb::index::Index::FTS(lancedb::index::scalar::FtsIndexBuilder::default()),
        )
        .execute()
        .await
        .map_err(lancedb_err)?;
    Ok(())
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
    ensure_table(conn, "memories", memories_schema()).await
}

async fn ensure_feedback_events_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "feedback_events", feedback_events_schema()).await
}

/// Idempotently create the `memory_embeddings` table with `dim`-sized
/// vectors. Lazy-created on first `upsert_memory_embedding` because dim
/// is provider-dependent and not known at `LanceStore::open()`
/// time. If the table already exists with a different dim, subsequent
/// `Table::add` calls fail with a schema mismatch error — that's
/// surfaced as the original `lancedb::Error` and is the right behavior
/// (mixing dims would break vector search regardless).
async fn ensure_memory_embeddings_table(conn: &Connection, dim: i32) -> Result<(), StorageError> {
    ensure_table(conn, "memory_embeddings", memory_embeddings_schema(dim)).await
}

async fn ensure_embedding_jobs_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "embedding_jobs", embedding_jobs_schema()).await
}

async fn ensure_graph_edges_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "graph_edges", graph_edges_schema()).await
}

async fn ensure_entities_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "entities", entities_schema()).await
}

async fn ensure_entity_aliases_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "entity_aliases", entity_aliases_schema()).await
}

async fn ensure_transcript_embedding_jobs_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(
        conn,
        "transcript_embedding_jobs",
        transcript_embedding_jobs_schema(),
    )
    .await
}

async fn ensure_conversation_messages_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(
        conn,
        "conversation_messages",
        conversation_messages_schema(),
    )
    .await
}

/// Idempotent `create_empty_table` — checks `Connection::table_names()`
/// first and skips the create call if the table already exists.
async fn ensure_table(conn: &Connection, name: &str, schema: Schema) -> Result<(), StorageError> {
    let names = conn.table_names().execute().await.map_err(lancedb_err)?;
    if names.iter().any(|n| n == name) {
        return Ok(());
    }
    let schema = Arc::new(schema);
    conn.create_empty_table(name, schema)
        .execute()
        .await
        .map_err(lancedb_err)?;
    Ok(())
}

/// Arrow schema for the `feedback_events` LanceDB table. Mirrors the
/// `feedback_events` DuckDB schema (4 columns: feedback_id PK,
/// memory_id, feedback_kind, created_at).
fn feedback_events_schema() -> Schema {
    Schema::new(vec![
        Field::new("feedback_id", DataType::Utf8, false),
        Field::new("memory_id", DataType::Utf8, false),
        Field::new("feedback_kind", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

fn feedback_events_to_record_batch(events: &[FeedbackEvent]) -> Result<RecordBatch, StorageError> {
    let mut feedback_id = StringBuilder::new();
    let mut memory_id = StringBuilder::new();
    let mut feedback_kind = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    for e in events {
        feedback_id.append_value(&e.feedback_id);
        memory_id.append_value(&e.memory_id);
        feedback_kind.append_value(&e.feedback_kind);
        created_at.append_value(&e.created_at);
    }
    let schema = Arc::new(feedback_events_schema());
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(feedback_id.finish()),
        Arc::new(memory_id.finish()),
        Arc::new(feedback_kind.finish()),
        Arc::new(created_at.finish()),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| StorageError::InvalidInput(format!("feedback record batch: {e}")))
}

fn record_batch_to_feedback_events(
    batch: &RecordBatch,
) -> Result<Vec<FeedbackEvent>, StorageError> {
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
    let feedback_id = col::<StringArray>(batch, "feedback_id")?;
    let memory_id = col::<StringArray>(batch, "memory_id")?;
    let feedback_kind = col::<StringArray>(batch, "feedback_kind")?;
    let created_at = col::<StringArray>(batch, "created_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(FeedbackEvent {
            feedback_id: feedback_id.value(i).to_string(),
            memory_id: memory_id.value(i).to_string(),
            feedback_kind: feedback_kind.value(i).to_string(),
            created_at: created_at.value(i).to_string(),
        });
    }
    Ok(out)
}

/// Arrow schema for the `memory_embeddings` LanceDB table. The vector
/// column is `FixedSizeList<Float32, dim>` because LanceDB's ANN index
/// requires a known fixed dimension; `dim` comes from the upserting
/// caller (which knows the embedding model's output size).
fn memory_embeddings_schema(dim: i32) -> Schema {
    Schema::new(vec![
        Field::new("memory_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("embedding_model", DataType::Utf8, false),
        Field::new("embedding_dim", DataType::Int64, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
            false,
        ),
        Field::new("content_hash", DataType::Utf8, false),
        Field::new("source_updated_at", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

/// Decode a native-endian `[f32]` blob into a `Vec<f32>` of length
/// `expected_dim`. Mirrors the DuckDB backend's
/// `f32::from_ne_bytes`-chunking — every embedding row in mem (DuckDB or
/// LanceDB) ultimately came from the same `EmbeddingProvider`, which
/// produces native-endian f32 bytes.
fn decode_embedding_blob(blob: &[u8], expected_dim: usize) -> Result<Vec<f32>, StorageError> {
    if blob.len() != expected_dim * 4 {
        return Err(StorageError::InvalidData(
            "embedding blob length mismatch (expected dim * 4 bytes)",
        ));
    }
    let mut out = Vec::with_capacity(expected_dim);
    for chunk in blob.chunks_exact(4) {
        out.push(f32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

/// Build a one-row `RecordBatch` for `memory_embeddings`. `embedding`
/// must already be the decoded `Vec<f32>` of length `dim`.
#[allow(clippy::too_many_arguments)]
fn memory_embedding_to_record_batch(
    memory_id: &str,
    tenant: &str,
    embedding_model: &str,
    embedding_dim: i64,
    embedding: &[f32],
    content_hash: &str,
    source_updated_at: &str,
    now: &str,
) -> Result<RecordBatch, StorageError> {
    let dim = i32::try_from(embedding.len()).map_err(|_| {
        StorageError::InvalidData("embedding dim does not fit in i32 for FixedSizeList")
    })?;

    let mut memory_id_b = StringBuilder::new();
    let mut tenant_b = StringBuilder::new();
    let mut model_b = StringBuilder::new();
    let mut dim_b = Int64Builder::new();
    let mut hash_b = StringBuilder::new();
    let mut src_ts_b = StringBuilder::new();
    let mut created_b = StringBuilder::new();
    let mut updated_b = StringBuilder::new();
    memory_id_b.append_value(memory_id);
    tenant_b.append_value(tenant);
    model_b.append_value(embedding_model);
    dim_b.append_value(embedding_dim);
    hash_b.append_value(content_hash);
    src_ts_b.append_value(source_updated_at);
    created_b.append_value(now);
    updated_b.append_value(now);

    let mut emb_b = FixedSizeListBuilder::with_capacity(Float32Builder::new(), dim, 1);
    for v in embedding {
        emb_b.values().append_value(*v);
    }
    emb_b.append(true);

    let schema = Arc::new(memory_embeddings_schema(dim));
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(memory_id_b.finish()),
        Arc::new(tenant_b.finish()),
        Arc::new(model_b.finish()),
        Arc::new(dim_b.finish()),
        Arc::new(emb_b.finish()),
        Arc::new(hash_b.finish()),
        Arc::new(src_ts_b.finish()),
        Arc::new(created_b.finish()),
        Arc::new(updated_b.finish()),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| StorageError::InvalidInput(format!("memory_embedding record batch: {e}")))
}

/// Internal row representation for `embedding_jobs`. Mirrors DuckDB's
/// private `EmbeddingJobRow` (same field set, same types). Used as the
/// intermediate when reading record batches off LanceDB so we can sort
/// in memory before slicing — LanceDB's `QueryBase` doesn't expose
/// ORDER BY, so the queue's "oldest pending first" ordering is enforced
/// after the fact.
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

/// Arrow schema for the `embedding_jobs` LanceDB table. 11 columns,
/// scalar-only — mirrors the DuckDB embedding_jobs table 1:1.
fn embedding_jobs_schema() -> Schema {
    Schema::new(vec![
        Field::new("job_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("memory_id", DataType::Utf8, false),
        Field::new("target_content_hash", DataType::Utf8, false),
        Field::new("provider", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("attempt_count", DataType::Int64, false),
        Field::new("last_error", DataType::Utf8, true),
        Field::new("available_at", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

fn embedding_job_row_to_record_batch(row: &EmbeddingJobRow) -> Result<RecordBatch, StorageError> {
    let mut job_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut memory_id = StringBuilder::new();
    let mut target_content_hash = StringBuilder::new();
    let mut provider = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut attempt_count = Int64Builder::new();
    let mut last_error = StringBuilder::new();
    let mut available_at = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    job_id.append_value(&row.job_id);
    tenant.append_value(&row.tenant);
    memory_id.append_value(&row.memory_id);
    target_content_hash.append_value(&row.target_content_hash);
    provider.append_value(&row.provider);
    status.append_value(&row.status);
    attempt_count.append_value(row.attempt_count);
    match &row.last_error {
        Some(s) => last_error.append_value(s),
        None => last_error.append_null(),
    }
    available_at.append_value(&row.available_at);
    created_at.append_value(&row.created_at);
    updated_at.append_value(&row.updated_at);
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(job_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(memory_id.finish()),
        Arc::new(target_content_hash.finish()),
        Arc::new(provider.finish()),
        Arc::new(status.finish()),
        Arc::new(attempt_count.finish()),
        Arc::new(last_error.finish()),
        Arc::new(available_at.finish()),
        Arc::new(created_at.finish()),
        Arc::new(updated_at.finish()),
    ];
    RecordBatch::try_new(Arc::new(embedding_jobs_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("embedding_job record batch: {e}")))
}

fn record_batch_to_embedding_job_rows(
    batch: &RecordBatch,
) -> Result<Vec<EmbeddingJobRow>, StorageError> {
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
    use arrow_array::Int64Array;
    let job_id = col::<StringArray>(batch, "job_id")?;
    let tenant = col::<StringArray>(batch, "tenant")?;
    let memory_id = col::<StringArray>(batch, "memory_id")?;
    let target_content_hash = col::<StringArray>(batch, "target_content_hash")?;
    let provider = col::<StringArray>(batch, "provider")?;
    let status = col::<StringArray>(batch, "status")?;
    let attempt_count = col::<Int64Array>(batch, "attempt_count")?;
    let last_error = col::<StringArray>(batch, "last_error")?;
    let available_at = col::<StringArray>(batch, "available_at")?;
    let created_at = col::<StringArray>(batch, "created_at")?;
    let updated_at = col::<StringArray>(batch, "updated_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(EmbeddingJobRow {
            job_id: job_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            memory_id: memory_id.value(i).to_string(),
            target_content_hash: target_content_hash.value(i).to_string(),
            provider: provider.value(i).to_string(),
            status: status.value(i).to_string(),
            attempt_count: attempt_count.value(i),
            last_error: if last_error.is_null(i) {
                None
            } else {
                Some(last_error.value(i).to_string())
            },
            available_at: available_at.value(i).to_string(),
            created_at: created_at.value(i).to_string(),
            updated_at: updated_at.value(i).to_string(),
        });
    }
    Ok(out)
}

/// Internal row representation for `transcript_embedding_jobs`.
/// Mirrors `EmbeddingJobRow` (memories side) with `memory_id` →
/// `message_block_id` and `target_content_hash` dropped (transcript
/// blocks are immutable, so the row id IS the hash).
#[derive(Debug, Clone)]
struct TranscriptEmbeddingJobRow {
    job_id: String,
    tenant: String,
    message_block_id: String,
    provider: String,
    status: String,
    attempt_count: i64,
    last_error: Option<String>,
    available_at: String,
    created_at: String,
    updated_at: String,
}

/// Arrow schema for the `transcript_embedding_jobs` LanceDB table.
/// 10 columns, scalar-only — same shape as `embedding_jobs` minus
/// `target_content_hash`.
fn transcript_embedding_jobs_schema() -> Schema {
    Schema::new(vec![
        Field::new("job_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("message_block_id", DataType::Utf8, false),
        Field::new("provider", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("attempt_count", DataType::Int64, false),
        Field::new("last_error", DataType::Utf8, true),
        Field::new("available_at", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

fn transcript_embedding_job_row_to_record_batch(
    row: &TranscriptEmbeddingJobRow,
) -> Result<RecordBatch, StorageError> {
    let mut job_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut message_block_id = StringBuilder::new();
    let mut provider = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut attempt_count = Int64Builder::new();
    let mut last_error = StringBuilder::new();
    let mut available_at = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    job_id.append_value(&row.job_id);
    tenant.append_value(&row.tenant);
    message_block_id.append_value(&row.message_block_id);
    provider.append_value(&row.provider);
    status.append_value(&row.status);
    attempt_count.append_value(row.attempt_count);
    match &row.last_error {
        Some(s) => last_error.append_value(s),
        None => last_error.append_null(),
    }
    available_at.append_value(&row.available_at);
    created_at.append_value(&row.created_at);
    updated_at.append_value(&row.updated_at);
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(job_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(message_block_id.finish()),
        Arc::new(provider.finish()),
        Arc::new(status.finish()),
        Arc::new(attempt_count.finish()),
        Arc::new(last_error.finish()),
        Arc::new(available_at.finish()),
        Arc::new(created_at.finish()),
        Arc::new(updated_at.finish()),
    ];
    RecordBatch::try_new(Arc::new(transcript_embedding_jobs_schema()), columns).map_err(|e| {
        StorageError::InvalidInput(format!("transcript_embedding_job record batch: {e}"))
    })
}

fn record_batch_to_transcript_embedding_job_rows(
    batch: &RecordBatch,
) -> Result<Vec<TranscriptEmbeddingJobRow>, StorageError> {
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
    use arrow_array::Int64Array;
    let job_id = col::<StringArray>(batch, "job_id")?;
    let tenant = col::<StringArray>(batch, "tenant")?;
    let message_block_id = col::<StringArray>(batch, "message_block_id")?;
    let provider = col::<StringArray>(batch, "provider")?;
    let status = col::<StringArray>(batch, "status")?;
    let attempt_count = col::<Int64Array>(batch, "attempt_count")?;
    let last_error = col::<StringArray>(batch, "last_error")?;
    let available_at = col::<StringArray>(batch, "available_at")?;
    let created_at = col::<StringArray>(batch, "created_at")?;
    let updated_at = col::<StringArray>(batch, "updated_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(TranscriptEmbeddingJobRow {
            job_id: job_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            message_block_id: message_block_id.value(i).to_string(),
            provider: provider.value(i).to_string(),
            status: status.value(i).to_string(),
            attempt_count: attempt_count.value(i),
            last_error: if last_error.is_null(i) {
                None
            } else {
                Some(last_error.value(i).to_string())
            },
            available_at: available_at.value(i).to_string(),
            created_at: created_at.value(i).to_string(),
            updated_at: updated_at.value(i).to_string(),
        });
    }
    Ok(out)
}

/// Arrow schema for `graph_edges`. Bitemporal edge model — `valid_to`
/// is null for active edges, set to a timestamp when superseded.
fn graph_edges_schema() -> Schema {
    Schema::new(vec![
        Field::new("from_node_id", DataType::Utf8, false),
        Field::new("to_node_id", DataType::Utf8, false),
        Field::new("relation", DataType::Utf8, false),
        Field::new("valid_from", DataType::Utf8, false),
        Field::new("valid_to", DataType::Utf8, true),
    ])
}

fn graph_edge_to_record_batch(edge: &GraphEdge) -> Result<RecordBatch, StorageError> {
    let mut from = StringBuilder::new();
    let mut to = StringBuilder::new();
    let mut relation = StringBuilder::new();
    let mut valid_from = StringBuilder::new();
    let mut valid_to = StringBuilder::new();
    from.append_value(&edge.from_node_id);
    to.append_value(&edge.to_node_id);
    relation.append_value(&edge.relation);
    valid_from.append_value(&edge.valid_from);
    match &edge.valid_to {
        Some(s) => valid_to.append_value(s),
        None => valid_to.append_null(),
    }
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(from.finish()),
        Arc::new(to.finish()),
        Arc::new(relation.finish()),
        Arc::new(valid_from.finish()),
        Arc::new(valid_to.finish()),
    ];
    RecordBatch::try_new(Arc::new(graph_edges_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("graph_edge record batch: {e}")))
}

fn record_batch_to_graph_edges(batch: &RecordBatch) -> Result<Vec<GraphEdge>, StorageError> {
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
    let from = col::<StringArray>(batch, "from_node_id")?;
    let to = col::<StringArray>(batch, "to_node_id")?;
    let relation = col::<StringArray>(batch, "relation")?;
    let valid_from = col::<StringArray>(batch, "valid_from")?;
    let valid_to = col::<StringArray>(batch, "valid_to")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(GraphEdge {
            from_node_id: from.value(i).to_string(),
            to_node_id: to.value(i).to_string(),
            relation: relation.value(i).to_string(),
            valid_from: valid_from.value(i).to_string(),
            valid_to: if valid_to.is_null(i) {
                None
            } else {
                Some(valid_to.value(i).to_string())
            },
        });
    }
    Ok(out)
}

/// Arrow schema for `entities`. Mirrors DuckDB 1:1 (5 cols, scalar).
fn entities_schema() -> Schema {
    Schema::new(vec![
        Field::new("entity_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("canonical_name", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

fn entity_to_record_batch(entity: &Entity) -> Result<RecordBatch, StorageError> {
    let mut entity_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut canonical_name = StringBuilder::new();
    let mut kind = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    entity_id.append_value(&entity.entity_id);
    tenant.append_value(&entity.tenant);
    canonical_name.append_value(&entity.canonical_name);
    kind.append_value(entity.kind.as_db_str());
    created_at.append_value(&entity.created_at);
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(entity_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(canonical_name.finish()),
        Arc::new(kind.finish()),
        Arc::new(created_at.finish()),
    ];
    RecordBatch::try_new(Arc::new(entities_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("entity record batch: {e}")))
}

fn record_batch_to_entities(batch: &RecordBatch) -> Result<Vec<Entity>, StorageError> {
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
    let entity_id = col::<StringArray>(batch, "entity_id")?;
    let tenant = col::<StringArray>(batch, "tenant")?;
    let canonical_name = col::<StringArray>(batch, "canonical_name")?;
    let kind = col::<StringArray>(batch, "kind")?;
    let created_at = col::<StringArray>(batch, "created_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let kind_s = kind.value(i);
        let kind = EntityKind::from_db_str(kind_s)
            .ok_or(StorageError::InvalidData("invalid entity kind"))?;
        out.push(Entity {
            entity_id: entity_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            canonical_name: canonical_name.value(i).to_string(),
            kind,
            created_at: created_at.value(i).to_string(),
        });
    }
    Ok(out)
}

/// Arrow schema for `entity_aliases`. Composite "PK" = (tenant,
/// alias_text) but LanceDB doesn't enforce uniqueness — every write
/// path that touches this table must do its own existence check first.
fn entity_aliases_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("alias_text", DataType::Utf8, false),
        Field::new("entity_id", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

fn entity_alias_to_record_batch(
    tenant_v: &str,
    alias_text_v: &str,
    entity_id_v: &str,
    created_at_v: &str,
) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut alias_text = StringBuilder::new();
    let mut entity_id = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    tenant.append_value(tenant_v);
    alias_text.append_value(alias_text_v);
    entity_id.append_value(entity_id_v);
    created_at.append_value(created_at_v);
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(tenant.finish()),
        Arc::new(alias_text.finish()),
        Arc::new(entity_id.finish()),
        Arc::new(created_at.finish()),
    ];
    RecordBatch::try_new(Arc::new(entity_aliases_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("entity_alias record batch: {e}")))
}

/// Arrow schema for `conversation_messages` (transcript-block archive).
/// Mirrors DuckDB 1:1 — 15 cols, scalar-only. The "PK"-equivalent for
/// idempotency is the composite (transcript_path, line_number,
/// block_index); LanceDB doesn't enforce uniqueness, so writes do a
/// count_rows pre-check.
fn conversation_messages_schema() -> Schema {
    Schema::new(vec![
        Field::new("message_block_id", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, true),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("caller_agent", DataType::Utf8, false),
        Field::new("transcript_path", DataType::Utf8, false),
        Field::new("line_number", DataType::UInt64, false),
        Field::new("block_index", DataType::UInt32, false),
        Field::new("message_uuid", DataType::Utf8, true),
        Field::new("role", DataType::Utf8, false),
        Field::new("block_type", DataType::Utf8, false),
        Field::new("content", DataType::Utf8, false),
        Field::new("tool_name", DataType::Utf8, true),
        Field::new("tool_use_id", DataType::Utf8, true),
        Field::new("embed_eligible", DataType::Boolean, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

fn conversation_message_to_record_batch(
    msg: &ConversationMessage,
) -> Result<RecordBatch, StorageError> {
    use arrow_array::builder::{BooleanBuilder, UInt32Builder};

    let mut message_block_id = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut caller_agent = StringBuilder::new();
    let mut transcript_path = StringBuilder::new();
    let mut line_number = UInt64Builder::new();
    let mut block_index = UInt32Builder::new();
    let mut message_uuid = StringBuilder::new();
    let mut role = StringBuilder::new();
    let mut block_type = StringBuilder::new();
    let mut content = StringBuilder::new();
    let mut tool_name = StringBuilder::new();
    let mut tool_use_id = StringBuilder::new();
    let mut embed_eligible = BooleanBuilder::new();
    let mut created_at = StringBuilder::new();

    message_block_id.append_value(&msg.message_block_id);
    match &msg.session_id {
        Some(s) => session_id.append_value(s),
        None => session_id.append_null(),
    }
    tenant.append_value(&msg.tenant);
    caller_agent.append_value(&msg.caller_agent);
    transcript_path.append_value(&msg.transcript_path);
    line_number.append_value(msg.line_number);
    block_index.append_value(msg.block_index);
    match &msg.message_uuid {
        Some(s) => message_uuid.append_value(s),
        None => message_uuid.append_null(),
    }
    role.append_value(msg.role.as_db_str());
    block_type.append_value(msg.block_type.as_db_str());
    content.append_value(&msg.content);
    match &msg.tool_name {
        Some(s) => tool_name.append_value(s),
        None => tool_name.append_null(),
    }
    match &msg.tool_use_id {
        Some(s) => tool_use_id.append_value(s),
        None => tool_use_id.append_null(),
    }
    embed_eligible.append_value(msg.embed_eligible);
    created_at.append_value(&msg.created_at);

    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(message_block_id.finish()),
        Arc::new(session_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(caller_agent.finish()),
        Arc::new(transcript_path.finish()),
        Arc::new(line_number.finish()),
        Arc::new(block_index.finish()),
        Arc::new(message_uuid.finish()),
        Arc::new(role.finish()),
        Arc::new(block_type.finish()),
        Arc::new(content.finish()),
        Arc::new(tool_name.finish()),
        Arc::new(tool_use_id.finish()),
        Arc::new(embed_eligible.finish()),
        Arc::new(created_at.finish()),
    ];
    RecordBatch::try_new(Arc::new(conversation_messages_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("conversation_message record batch: {e}")))
}

fn record_batch_to_conversation_messages(
    batch: &RecordBatch,
) -> Result<Vec<ConversationMessage>, StorageError> {
    use arrow_array::{BooleanArray, UInt32Array};

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
    let message_block_id = col::<StringArray>(batch, "message_block_id")?;
    let session_id = col::<StringArray>(batch, "session_id")?;
    let tenant = col::<StringArray>(batch, "tenant")?;
    let caller_agent = col::<StringArray>(batch, "caller_agent")?;
    let transcript_path = col::<StringArray>(batch, "transcript_path")?;
    let line_number = col::<UInt64Array>(batch, "line_number")?;
    let block_index = col::<UInt32Array>(batch, "block_index")?;
    let message_uuid = col::<StringArray>(batch, "message_uuid")?;
    let role = col::<StringArray>(batch, "role")?;
    let block_type = col::<StringArray>(batch, "block_type")?;
    let content = col::<StringArray>(batch, "content")?;
    let tool_name = col::<StringArray>(batch, "tool_name")?;
    let tool_use_id = col::<StringArray>(batch, "tool_use_id")?;
    let embed_eligible = col::<BooleanArray>(batch, "embed_eligible")?;
    let created_at = col::<StringArray>(batch, "created_at")?;

    let opt = |arr: &StringArray, i: usize| -> Option<String> {
        if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_string())
        }
    };

    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        let role = MessageRole::from_db_str(role.value(i))
            .ok_or(StorageError::InvalidData("invalid role"))?;
        let block_type = BlockType::from_db_str(block_type.value(i))
            .ok_or(StorageError::InvalidData("invalid block_type"))?;
        out.push(ConversationMessage {
            message_block_id: message_block_id.value(i).to_string(),
            session_id: opt(session_id, i),
            tenant: tenant.value(i).to_string(),
            caller_agent: caller_agent.value(i).to_string(),
            transcript_path: transcript_path.value(i).to_string(),
            line_number: line_number.value(i),
            block_index: block_index.value(i),
            message_uuid: opt(message_uuid, i),
            role,
            block_type,
            content: content.value(i).to_string(),
            tool_name: opt(tool_name, i),
            tool_use_id: opt(tool_use_id, i),
            embed_eligible: embed_eligible.value(i),
            created_at: created_at.value(i).to_string(),
        });
    }
    Ok(out)
}

/// Mirror of DuckDB's private `feedback_adjustments` helper. Resolves a
/// raw `feedback_kind` string to the deltas that `apply_feedback` must
/// apply to the parent memory's confidence / decay / status fields.
fn feedback_adjustments(
    feedback_kind: &str,
) -> Option<(f32, f32, Option<crate::domain::memory::MemoryStatus>, bool)> {
    use crate::domain::memory::FeedbackKind;
    let kind = match feedback_kind {
        "useful" => FeedbackKind::Useful,
        "outdated" => FeedbackKind::Outdated,
        "incorrect" => FeedbackKind::Incorrect,
        "applies_here" => FeedbackKind::AppliesHere,
        "does_not_apply_here" => FeedbackKind::DoesNotApplyHere,
        _ => return None,
    };
    let archive = kind.archived_status();
    Some((
        kind.confidence_delta(),
        kind.decay_delta(),
        archive.then_some(crate::domain::memory::MemoryStatus::Archived),
        kind.marks_validated(),
    ))
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

impl LanceStore {
    /// Apply a status transition to `(tenant, memory_id)` and return the
    /// updated row. Shared by `accept_pending` / `reject_pending` (and a
    /// future `archive_pending` if needed). Mirrors the DuckDB backend's
    /// `update_status` private helper.
    ///
    /// **Not yet implemented:** the embedding-references cleanup that the
    /// DuckDB version does (delete `embedding_jobs` + `memory_embeddings`
    /// rows for this memory) — those tables don't exist on the LanceDB
    /// side yet. Add when those tables land.
    async fn update_status(
        &self,
        tenant: &str,
        memory_id: &str,
        status_str: &str,
    ) -> Result<MemoryRecord, StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let now = crate::storage::current_timestamp();
        let result = table
            .update()
            .only_if(format!(
                "tenant = {} AND memory_id = {}",
                sql_quote(tenant),
                sql_quote(memory_id),
            ))
            .column("status", sql_quote(status_str))
            .column("updated_at", sql_quote(&now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        self.get_memory_for_tenant(tenant, memory_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after status update",
            ))
    }

    /// Run a filter query against the `memories` table and parse all
    /// returned batches into [`MemoryRecord`]s. Shared by every read
    /// method that just needs a `WHERE`-clause + optional `LIMIT`.
    async fn query_memories(
        &self,
        filter: String,
        limit: Option<usize>,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut q = table.query().only_if(filter);
        if let Some(l) = limit {
            q = q.limit(l);
        }
        let stream = q.execute().await.map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_memories(b)?);
        }
        Ok(out)
    }

    /// Read all `embedding_jobs` rows matching `filter`, parsed into
    /// [`EmbeddingJobRow`]s. Shared by every queue read path: the claim
    /// flow, `first_embedding_job_id_for_memory`, `list_embedding_jobs`,
    /// and the duplicate-detection in `try_enqueue_embedding_job`.
    async fn query_embedding_jobs(
        &self,
        filter: String,
    ) -> Result<Vec<EmbeddingJobRow>, StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
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
            out.extend(record_batch_to_embedding_job_rows(b)?);
        }
        Ok(out)
    }

    /// Counterpart of `query_embedding_jobs` for the transcript queue.
    async fn query_transcript_embedding_jobs(
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
/// queue (`try_enqueue_embedding_job` etc.) with `memory_id` →
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
}

#[async_trait]
#[allow(clippy::too_many_arguments)]
impl MemoryRepository for LanceStore {
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
        // Idempotency check: if any live (pending/processing) row already
        // covers this (tenant, memory_id, target_content_hash, provider)
        // tuple, decline the enqueue. LanceDB has no transactions so the
        // count → insert window is racy under concurrent writers, but mem
        // serve runs one writer per DB so the race is single-instance safe.
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let live = table
            .count_rows(Some(format!(
                "tenant = {} AND memory_id = {} AND target_content_hash = {} \
                 AND provider = {} AND (status = 'pending' OR status = 'processing')",
                sql_quote(&insert.tenant),
                sql_quote(&insert.memory_id),
                sql_quote(&insert.target_content_hash),
                sql_quote(&insert.provider),
            )))
            .await
            .map_err(lancedb_err)?;
        if live > 0 {
            return Ok(false);
        }
        let row = EmbeddingJobRow {
            job_id: insert.job_id,
            tenant: insert.tenant,
            memory_id: insert.memory_id,
            target_content_hash: insert.target_content_hash,
            provider: insert.provider,
            status: "pending".to_string(),
            attempt_count: 0,
            last_error: None,
            available_at: insert.available_at,
            created_at: insert.created_at,
            updated_at: insert.updated_at,
        };
        let batch = embedding_job_row_to_record_batch(&row)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(true)
    }

    async fn first_embedding_job_id_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let mut rows = self
            .query_embedding_jobs(format!("memory_id = {}", sql_quote(memory_id)))
            .await?;
        // LanceDB has no ORDER BY — sort in memory by created_at ASC
        // (same shape as the DuckDB SQL).
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(rows.into_iter().next().map(|r| r.job_id))
    }

    async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        if n == 0 {
            return Ok(vec![]);
        }
        // Eligible = available_at <= now AND (pending OR (failed AND
        // attempt_count < max_retries)). LanceDB has no ORDER BY, so we
        // pull all eligible rows and sort by (available_at, created_at)
        // ASC in memory before slicing — queue depth is expected to be
        // small (worker drains continuously) so the in-memory cost is
        // negligible vs. the simpler code.
        //
        // Note: unlike DuckDB we don't sweep orphan jobs here. LanceDB
        // has no FK constraints, so the FK-loop pathology that motivated
        // the orphan sweep on DuckDB cannot occur here. If a memory is
        // deleted, its embedding_jobs rows simply stay until the worker
        // touches them; the FK-error retry loop is a DuckDB-only bug.
        let max_r = i64::from(max_retries);
        let filter = format!(
            "available_at <= {} AND (status = 'pending' OR (status = 'failed' AND attempt_count < {}))",
            sql_quote(now),
            max_r,
        );
        let mut rows = self.query_embedding_jobs(filter).await?;
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
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut claimed = Vec::with_capacity(rows.len());
        for r in rows {
            // Optimistic claim: only update if status is still eligible
            // (pending, or failed-with-budget). A second-instance race
            // would see rows_updated == 0 and we'd skip the row — same
            // shape as DuckDB's "updated == 0 → return None" branch.
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
            claimed.push(ClaimedEmbeddingJob {
                job_id: r.job_id,
                tenant: r.tenant,
                memory_id: r.memory_id,
                target_content_hash: r.target_content_hash,
                provider: r.provider,
                attempt_count: r.attempt_count,
            });
        }
        Ok(claimed)
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
        let dim_i32 = i32::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim does not fit in i32"))?;
        let vector = decode_embedding_blob(embedding_blob, embedding_dim as usize)?;

        ensure_memory_embeddings_table(&self.conn, dim_i32).await?;

        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // upsert = delete-then-insert. LanceDB has no PK enforcement so
        // we sweep any existing row for this memory_id first.
        table
            .delete(&format!("memory_id = {}", sql_quote(memory_id)))
            .await
            .map_err(lancedb_err)?;
        let batch = memory_embedding_to_record_batch(
            memory_id,
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

    async fn delete_memory_embedding(&self, memory_id: &str) -> Result<(), StorageError> {
        // No-op if the table doesn't exist yet (semantic search hasn't
        // been used; nothing to delete).
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "memory_embeddings") {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!("memory_id = {}", sql_quote(memory_id)))
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        self.query_memories(format!("tenant = {}", sql_quote(tenant)), None)
            .await
    }

    async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
        if query_embedding.is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        // No embeddings written yet → empty result (matches DuckDB
        // legacy linear-scan behavior on an empty memory_embeddings).
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "memory_embeddings") {
            return Ok(vec![]);
        }
        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;

        // Vector search with tenant prefilter (default mode). LanceDB
        // filters before ANN, so tenant-scoping is correct even when an
        // ANN index is later attached.
        let stream = table
            .vector_search(query_embedding)
            .map_err(lancedb_err)?
            .distance_type(DistanceType::Cosine)
            .only_if(format!("tenant = {}", sql_quote(tenant)))
            .limit(limit)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;

        // Collect (memory_id, score) pairs in distance-ascending order.
        // LanceDB returns rows already sorted by `_distance`; preserve
        // that order across batches by extending sequentially.
        let mut hits: Vec<(String, f32)> = Vec::new();
        for b in &batches {
            let memory_ids = b
                .column_by_name("memory_id")
                .ok_or(StorageError::InvalidData("missing memory_id column"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or(StorageError::InvalidData("memory_id column type mismatch"))?;
            let distances = b
                .column_by_name("_distance")
                .ok_or(StorageError::InvalidData(
                    "missing _distance column from vector_search",
                ))?
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or(StorageError::InvalidData("_distance column type mismatch"))?;
            for i in 0..b.num_rows() {
                // Cosine distance ∈ [0, 2]; similarity = 1 - distance
                // matches DuckDB backend's cosine_similarity score
                // shape (higher = better, normalized vectors → [0, 1]).
                let score = 1.0 - distances.value(i);
                hits.push((memory_ids.value(i).to_string(), score));
            }
        }

        // Hydrate full MemoryRecord rows. fetch_memories_by_ids returns
        // out of input order, so we rebuild the score-ordered list
        // afterwards via a hashmap lookup.
        let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
        let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        let records = self.fetch_memories_by_ids(tenant, &id_refs).await?;
        let by_id: std::collections::HashMap<String, MemoryRecord> = records
            .into_iter()
            .map(|m| (m.memory_id.clone(), m))
            .collect();
        let mut out = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            if let Some(rec) = by_id.get(&id) {
                out.push((rec.clone(), score));
            }
            // Else: embedding row exists but memory was archived/deleted
            // after embedding write — skip silently, matches DuckDB's
            // implicit-join semantics.
        }
        Ok(out)
    }

    async fn complete_embedding_job(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        // Mirror DuckDB: only complete a row that's currently 'processing'
        // (otherwise it's already completed/stale and we shouldn't bump it).
        // LanceDB doesn't have a NULL literal for last_error inside the
        // update column expression in a way the SQL parser tolerates as
        // an arbitrary expression — we encode "clear last_error" as
        // `CAST(NULL AS string)` so the column value is a SQL NULL.
        let table = self
            .conn
            .open_table("embedding_jobs")
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

    async fn mark_embedding_job_stale(&self, job_id: &str, now: &str) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
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

    async fn reschedule_embedding_job_failure(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        available_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
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

    async fn permanently_fail_embedding_job(
        &self,
        job_id: &str,
        new_attempt_count: i64,
        last_error: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("embedding_jobs")
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

    async fn delete_embedding_jobs_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<usize, StorageError> {
        // Pre-count to return how many rows we delete (LanceDB's
        // DeleteResult only carries num_deleted_rows, but we want this
        // to match DuckDB's `Connection::execute(DELETE)` rowcount
        // contract regardless).
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let count = table
            .count_rows(Some(format!("memory_id = {}", sql_quote(memory_id))))
            .await
            .map_err(lancedb_err)?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .delete(&format!("memory_id = {}", sql_quote(memory_id)))
            .await
            .map_err(lancedb_err)?;
        // Lance servers older than this codebase may report 0 here even
        // when rows were deleted (the count_rows pre-flight is the
        // canonical source for the count we return).
        if result.num_deleted_rows == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.num_deleted_rows).unwrap_or(count))
        }
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
        let filter = format!(
            "tenant = {} AND memory_id = {} AND status = 'pending_confirmation'",
            sql_quote(tenant),
            sql_quote(memory_id),
        );
        Ok(self
            .query_memories(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        // Match either `idempotency_key` (when caller provided one) OR
        // `content_hash` — same precedence as DuckDB's variant.
        let filter = match idempotency_key.as_deref() {
            Some(k) => format!(
                "tenant = {} AND (idempotency_key = {} OR content_hash = {})",
                sql_quote(tenant),
                sql_quote(k),
                sql_quote(content_hash),
            ),
            None => format!(
                "tenant = {} AND content_hash = {}",
                sql_quote(tenant),
                sql_quote(content_hash),
            ),
        };
        Ok(self
            .query_memories(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    async fn list_pending_review(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let filter = format!(
            "tenant = {} AND status = 'pending_confirmation'",
            sql_quote(tenant),
        );
        self.query_memories(filter, None).await
    }

    async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        // Same live-status filter the DuckDB backend uses
        // (`pipeline::retrieve` post-filters this set anyway).
        let filter = format!(
            "tenant = {} AND status NOT IN ('rejected', 'archived')",
            sql_quote(tenant),
        );
        self.query_memories(filter, None).await
    }

    async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        // NOTE: LanceDB's `Query::limit` doesn't guarantee any ordering
        // without a `Table::create_index` on `updated_at`. For now this
        // returns _some_ N rows; switching to ordered results requires
        // an index + `Query::nearest_to` or a sort step. The DuckDB
        // backend uses `ORDER BY updated_at DESC`.
        let filter = format!(
            "tenant = {} AND status NOT IN ('rejected', 'archived')",
            sql_quote(tenant),
        );
        self.query_memories(filter, Some(limit)).await
    }

    async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(vec![]);
        }
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;

        // FTS index is built once at `LanceStore::open` time on the
        // `content` column (see `ensure_fts_index`); no per-call check.
        let fts_query = lancedb::index::scalar::FullTextSearchQuery::new(query.to_string());
        let stream = table
            .query()
            .full_text_search(fts_query)
            .only_if(format!("tenant = {}", sql_quote(tenant)))
            .limit(k)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_memories(b)?);
        }
        Ok(out)
    }

    async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let id_list: Vec<String> = ids.iter().map(|i| sql_quote(i)).collect();
        let filter = format!(
            "tenant = {} AND status NOT IN ('rejected', 'archived') AND memory_id IN ({})",
            sql_quote(tenant),
            id_list.join(", "),
        );
        self.query_memories(filter, None).await
    }

    async fn accept_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, "active").await
    }

    async fn reject_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<MemoryRecord, StorageError> {
        self.update_status(tenant, memory_id, "rejected").await
    }

    async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: MemoryRecord,
    ) -> Result<MemoryRecord, StorageError> {
        // Two-step supersede: archive the old row, then insert the new
        // one. LanceDB has no transaction semantics across these calls,
        // so a crash between them leaves the old archived without a
        // successor — same risk profile as the DuckDB backend's
        // non-tx'd version (see `replace_pending_with_successor` in
        // duckdb/mod.rs).
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let now = crate::storage::current_timestamp();
        table
            .update()
            .only_if(format!(
                "tenant = {} AND memory_id = {}",
                sql_quote(tenant),
                sql_quote(original_memory_id),
            ))
            .column("status", "'archived'")
            .column("updated_at", sql_quote(&now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = memories_to_record_batch(std::slice::from_ref(&successor))?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(successor)
    }

    async fn apply_feedback(
        &self,
        memory: &MemoryRecord,
        feedback: FeedbackEvent,
    ) -> Result<MemoryRecord, StorageError> {
        let (conf_delta, decay_delta, status_after, mark_validated) =
            feedback_adjustments(&feedback.feedback_kind)
                .ok_or(StorageError::InvalidData("invalid feedback kind"))?;
        let updated_at = feedback.created_at.clone();
        let mut updated = memory.clone();
        updated.updated_at = updated_at.clone();
        updated.confidence = (updated.confidence + conf_delta).clamp(0.0, 1.0);
        updated.decay_score = (updated.decay_score + decay_delta).clamp(0.0, 1.0);
        if let Some(ref s) = status_after {
            updated.status = s.clone();
        }
        if mark_validated {
            updated.last_validated_at = Some(updated_at.clone());
        }

        // Always log the event first — independent of the parent UPDATE
        // succeeding, the audit trail is preserved. (Mirrors the DuckDB
        // backend's ordering.)
        let fb_table = self
            .conn
            .open_table("feedback_events")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = feedback_events_to_record_batch(std::slice::from_ref(&feedback))?;
        fb_table.add(batch).execute().await.map_err(lancedb_err)?;

        // Update the parent memory row. Status / last_validated_at are
        // optionally set; confidence + decay + updated_at always.
        let mem_table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut update = mem_table
            .update()
            .only_if(format!("memory_id = {}", sql_quote(&updated.memory_id)))
            .column("confidence", format!("{}", updated.confidence))
            .column("decay_score", format!("{}", updated.decay_score))
            .column("updated_at", sql_quote(&updated.updated_at));
        if let Some(s) = status_after {
            update = update.column("status", sql_quote(&enum_to_str(&s)?));
        }
        if mark_validated {
            update = update.column("last_validated_at", sql_quote(&updated_at));
        }
        update.execute().await.map_err(lancedb_err)?;
        Ok(updated)
    }

    async fn list_feedback_for_memory(
        &self,
        memory_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let table = self
            .conn
            .open_table("feedback_events")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!("memory_id = {}", sql_quote(memory_id)))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(record_batch_to_feedback_events(b)?);
        }
        // DuckDB returns `created_at ASC` order. LanceDB doesn't sort
        // automatically — sort client-side since the row count per
        // memory is small (single-digits typically).
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(out)
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
        // Fetch all events for this memory and aggregate client-side.
        // Counts are tiny (events per memory typically < 10), so the
        // network/parse cost is negligible compared to running a
        // GROUP BY query through LanceDB's filter API.
        let events = self.list_feedback_for_memory(memory_id).await?;
        let mut summary = FeedbackSummary::default();
        for e in events {
            summary.total += 1;
            match e.feedback_kind.as_str() {
                "useful" => summary.useful += 1,
                "outdated" => summary.outdated += 1,
                "incorrect" => summary.incorrect += 1,
                "applies_here" => summary.applies_here += 1,
                "does_not_apply_here" => summary.does_not_apply_here += 1,
                _ => {} // unknown kind — counted in `total` only
            }
        }
        Ok(summary)
    }

    async fn delete_memory_hard(&self, tenant: &str, memory_id: &str) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("memories")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let result = table
            .delete(&format!(
                "tenant = {} AND memory_id = {}",
                sql_quote(tenant),
                sql_quote(memory_id),
            ))
            .await
            .map_err(lancedb_err)?;
        if result.num_deleted_rows == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        // TODO: cascade-delete from embedding_jobs / memory_embeddings /
        // feedback_events / graph_edges once those tables exist on the
        // LanceDB side. The DuckDB backend handles this in
        // `DuckDbRepository::delete_memory_hard` (see ./duckdb/mod.rs).
        Ok(())
    }

    async fn get_memory(&self, memory_id: String) -> Result<Option<MemoryRecord>, StorageError> {
        // Cross-tenant lookup (admin / version-chain path). DuckDB does the
        // same — filters only on memory_id.
        let filter = format!("memory_id = {}", sql_quote(&memory_id));
        Ok(self
            .query_memories(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    async fn insert_episode(&self, episode: EpisodeRecord) -> Result<EpisodeRecord, StorageError> {
        let _ = episode;
        unimplemented!("LanceDb::insert_episode — see docs/repository.rs trait def")
    }

    async fn list_memory_ids_for_tenant(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        Ok(self
            .query_memories(format!("tenant = {}", sql_quote(tenant)), None)
            .await?
            .into_iter()
            .map(|m| m.memory_id)
            .collect())
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
        let mut filter = format!("tenant = {}", sql_quote(tenant));
        if let Some(s) = status_filter {
            filter.push_str(&format!(" AND status = {}", sql_quote(s)));
        }
        if let Some(m) = memory_id_filter {
            filter.push_str(&format!(" AND memory_id = {}", sql_quote(m)));
        }
        let mut rows = self.query_embedding_jobs(filter).await?;
        // ORDER BY updated_at DESC LIMIT n — sort then truncate.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let lim = limit.min(10_000);
        rows.truncate(lim);
        let out = rows
            .into_iter()
            .map(|r| EmbeddingJobInfo {
                job_id: r.job_id,
                tenant: r.tenant,
                memory_id: r.memory_id,
                target_content_hash: r.target_content_hash,
                provider: r.provider,
                status: r.status,
                attempt_count: u32::try_from(r.attempt_count).unwrap_or(u32::MAX),
                last_error: r.last_error,
                available_at: r.available_at,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect();
        Ok(out)
    }

    async fn stale_live_embedding_jobs_for_memory(
        &self,
        tenant: &str,
        memory_id: &str,
        provider: &str,
        now: &str,
    ) -> Result<usize, StorageError> {
        // Pre-count, then UPDATE all matching live rows to status 'stale'.
        // LanceDB's UpdateResult.rows_updated is the canonical rowcount,
        // but we count first so we can return the same shape as DuckDB
        // even if the LanceDB update reports 0 (legacy server quirk —
        // matches the same defensive shape we use in delete_*).
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let filter = format!(
            "tenant = {} AND memory_id = {} AND provider = {} \
             AND (status = 'pending' OR status = 'processing')",
            sql_quote(tenant),
            sql_quote(memory_id),
            sql_quote(provider),
        );
        let count = table
            .count_rows(Some(filter.clone()))
            .await
            .map_err(lancedb_err)?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .update()
            .only_if(filter)
            .column("status", "'stale'")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.rows_updated).unwrap_or(count))
        }
    }

    async fn get_memory_embedding_row(
        &self,
        memory_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        // No memory_embeddings table yet → no row by definition.
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "memory_embeddings") {
            return Ok(None);
        }
        let table = self
            .conn
            .open_table("memory_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!("memory_id = {}", sql_quote(memory_id)))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
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
            let model = col::<StringArray>(b, "embedding_model")?;
            let hash = col::<StringArray>(b, "content_hash")?;
            let updated = col::<StringArray>(b, "updated_at")?;
            return Ok(Some((
                model.value(0).to_string(),
                hash.value(0).to_string(),
                updated.value(0).to_string(),
            )));
        }
        Ok(None)
    }

    async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        memory_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let mut rows = self
            .query_embedding_jobs(format!(
                "tenant = {} AND memory_id = {} AND target_content_hash = {}",
                sql_quote(tenant),
                sql_quote(memory_id),
                sql_quote(target_content_hash),
            ))
            .await?;
        // ORDER BY updated_at DESC LIMIT 1.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(rows.into_iter().next().map(|r| r.status))
    }
}

/// Read all `conversation_messages` rows matching `filter`, parsed into
/// [`ConversationMessage`]s. Shared by every transcript read path.
impl LanceStore {
    async fn query_conversation_messages(
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

#[async_trait]
impl TranscriptRepository for LanceStore {
    async fn create_conversation_message(
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

    async fn get_conversation_messages_by_session(
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

    async fn get_conversation_messages_by_session_paged(
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

    async fn list_transcript_sessions(
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

    async fn fetch_conversation_messages_by_ids(
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

    async fn context_window_for_block(
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

    async fn anchor_session_candidates(
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

    async fn recent_conversation_messages(
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

    async fn bm25_transcript_candidates(
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

/// In-memory chronological ASC sort matching DuckDB's
/// `(created_at, line_number, block_index)` SQL ORDER BY.
fn sort_messages_chronological_asc(msgs: &mut [ConversationMessage]) {
    msgs.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.line_number.cmp(&b.line_number))
            .then_with(|| a.block_index.cmp(&b.block_index))
    });
}

/// Read all `graph_edges` rows matching `filter`, parsed into
/// [`GraphEdge`]s. Helper shared by `neighbors`, `related_memory_ids`,
/// and the existence check in `sync_memory_edges`.
impl LanceStore {
    async fn query_graph_edges(&self, filter: String) -> Result<Vec<GraphEdge>, GraphError> {
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let stream = table
            .query()
            .only_if(filter)
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| GraphError::Backend(format!("lancedb stream: {e}")))?;
        let mut out = Vec::new();
        for b in &batches {
            out.extend(
                record_batch_to_graph_edges(b).map_err(|e| GraphError::Backend(e.to_string()))?,
            );
        }
        Ok(out)
    }
}

#[async_trait]
impl GraphStore for LanceStore {
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        // Active edges only (valid_to is null) where the node sits on
        // either side. Order by (relation, from, to) to match DuckDB's
        // SQL — done in-memory because LanceDB has no ORDER BY.
        let mut edges = self
            .query_graph_edges(format!(
                "(from_node_id = {0} OR to_node_id = {0}) AND valid_to IS NULL",
                sql_quote(node_id),
            ))
            .await?;
        edges.sort_by(|a, b| {
            a.relation
                .cmp(&b.relation)
                .then_with(|| a.from_node_id.cmp(&b.from_node_id))
                .then_with(|| a.to_node_id.cmp(&b.to_node_id))
        });
        Ok(edges)
    }

    async fn sync_memory_edges(&self, edges: &[GraphEdge], now: &str) -> Result<(), GraphError> {
        if edges.is_empty() {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        // Idempotent insert: skip rows where an active edge with the
        // same (from, to, relation) already exists. LanceDB has no
        // transactions; a concurrent writer could race the existence
        // check, but mem serve is single-instance per DB so this is
        // safe in practice (same posture as embedding_jobs enqueue).
        for edge in edges {
            let exists = table
                .count_rows(Some(format!(
                    "from_node_id = {} AND to_node_id = {} AND relation = {} AND valid_to IS NULL",
                    sql_quote(&edge.from_node_id),
                    sql_quote(&edge.to_node_id),
                    sql_quote(&edge.relation),
                )))
                .await
                .map_err(|e| GraphError::Backend(e.to_string()))?;
            if exists > 0 {
                continue;
            }
            // Server overrides valid_from with `now` (matching DuckDB
            // behavior — callers don't need to think about clocks) and
            // forces valid_to = NULL (active).
            let to_write = GraphEdge {
                from_node_id: edge.from_node_id.clone(),
                to_node_id: edge.to_node_id.clone(),
                relation: edge.relation.clone(),
                valid_from: now.to_string(),
                valid_to: None,
            };
            let batch = graph_edge_to_record_batch(&to_write)
                .map_err(|e| GraphError::Backend(e.to_string()))?;
            table
                .add(batch)
                .execute()
                .await
                .map_err(|e| GraphError::Backend(e.to_string()))?;
        }
        Ok(())
    }

    async fn close_edges_for_memory(&self, memory_id: &str) -> Result<usize, GraphError> {
        let from = format!("memory:{memory_id}");
        let now = crate::storage::current_timestamp();
        let table = self
            .conn
            .open_table("graph_edges")
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        let filter = format!("from_node_id = {} AND valid_to IS NULL", sql_quote(&from));
        let count = table
            .count_rows(Some(filter.clone()))
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .update()
            .only_if(filter)
            .column("valid_to", sql_quote(&now))
            .execute()
            .await
            .map_err(|e| GraphError::Backend(e.to_string()))?;
        if result.rows_updated == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.rows_updated).unwrap_or(count))
        }
    }

    async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(vec![]);
        }
        // Build "id IN ('a', 'b', ...)" — LanceDB supports SQL IN, so
        // we match the DuckDB shape directly. No CASE expression
        // though, so we project both endpoints in Rust below.
        let in_list = node_ids
            .iter()
            .map(|n| sql_quote(n))
            .collect::<Vec<_>>()
            .join(",");
        let filter = format!(
            "(from_node_id IN ({0}) OR to_node_id IN ({0})) AND valid_to IS NULL",
            in_list,
        );
        let edges = self.query_graph_edges(filter).await?;
        let node_set: std::collections::HashSet<&str> =
            node_ids.iter().map(|s| s.as_str()).collect();
        let mut memory_ids = std::collections::HashSet::new();
        for e in edges {
            // Adjacency: pick the endpoint that's NOT in node_ids; if
            // both sides are in node_ids, both are recorded (matches
            // the DuckDB "case when from in (...) then to else from"
            // semantics — the SELECT DISTINCT collapses the duplicate).
            for endpoint in [&e.from_node_id, &e.to_node_id] {
                if !node_set.contains(endpoint.as_str()) {
                    if let Some(memory_id) = endpoint.strip_prefix("memory:") {
                        memory_ids.insert(memory_id.to_string());
                    }
                }
            }
        }
        let mut out: Vec<String> = memory_ids.into_iter().collect();
        out.sort();
        Ok(out)
    }
}

#[async_trait]
impl EntityRegistry for LanceStore {
    async fn resolve_or_create(
        &self,
        tenant: &str,
        alias: &str,
        kind: EntityKind,
        now: &str,
    ) -> Result<String, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);

        // Lookup first.
        if let Some(id) = self.lookup_alias(tenant, alias).await? {
            return Ok(id);
        }

        // Auto-promote: insert entity + first alias. No transaction (LanceDB
        // doesn't have them) — under concurrent enqueue the
        // single-writer assumption holds (see embedding_jobs comment).
        let entity_id = uuid::Uuid::now_v7().to_string();
        let entity = Entity {
            entity_id: entity_id.clone(),
            tenant: tenant.to_string(),
            canonical_name: alias.to_string(),
            kind,
            created_at: now.to_string(),
        };
        let entities = self
            .conn
            .open_table("entities")
            .execute()
            .await
            .map_err(lancedb_err)?;
        entities
            .add(entity_to_record_batch(&entity)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let aliases = self
            .conn
            .open_table("entity_aliases")
            .execute()
            .await
            .map_err(lancedb_err)?;
        aliases
            .add(entity_alias_to_record_batch(
                tenant,
                &normalized,
                &entity_id,
                now,
            )?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(entity_id)
    }

    async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let entities = self
            .conn
            .open_table("entities")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = entities
            .query()
            .only_if(format!(
                "tenant = {} AND entity_id = {}",
                sql_quote(tenant),
                sql_quote(entity_id),
            ))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut entity_iter = batches
            .iter()
            .flat_map(|b| record_batch_to_entities(b).unwrap_or_default().into_iter());
        let Some(entity) = entity_iter.next() else {
            return Ok(None);
        };

        // Pull aliases for this entity, sorted by created_at ASC then
        // alias_text ASC (mirror DuckDB SQL).
        let aliases_table = self
            .conn
            .open_table("entity_aliases")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream2 = aliases_table
            .query()
            .only_if(format!(
                "tenant = {} AND entity_id = {}",
                sql_quote(tenant),
                sql_quote(entity_id),
            ))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches2: Vec<RecordBatch> = stream2
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        let mut alias_rows: Vec<(String, String)> = Vec::new(); // (created_at, alias_text)
        for b in &batches2 {
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
            let alias_text = col::<StringArray>(b, "alias_text")?;
            let created_at = col::<StringArray>(b, "created_at")?;
            for i in 0..b.num_rows() {
                alias_rows.push((
                    created_at.value(i).to_string(),
                    alias_text.value(i).to_string(),
                ));
            }
        }
        alias_rows.sort();
        let aliases: Vec<String> = alias_rows.into_iter().map(|(_, a)| a).collect();
        Ok(Some(EntityWithAliases { entity, aliases }))
    }

    async fn add_alias(
        &self,
        tenant: &str,
        entity_id: &str,
        alias: &str,
        now: &str,
    ) -> Result<AddAliasOutcome, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);

        // Existing-owner check: who currently owns the normalized form?
        let existing_owner = self.lookup_alias(tenant, alias).await?;
        match existing_owner {
            None => {
                let aliases_table = self
                    .conn
                    .open_table("entity_aliases")
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
                aliases_table
                    .add(entity_alias_to_record_batch(
                        tenant,
                        &normalized,
                        entity_id,
                        now,
                    )?)
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
                Ok(AddAliasOutcome::Inserted)
            }
            Some(owner) if owner == entity_id => Ok(AddAliasOutcome::AlreadyOnSameEntity),
            Some(other) => Ok(AddAliasOutcome::ConflictWithDifferentEntity(other)),
        }
    }

    async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        use crate::pipeline::entity_normalize::normalize_alias;
        let normalized = normalize_alias(alias);
        let table = self
            .conn
            .open_table("entity_aliases")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!(
                "tenant = {} AND alias_text = {}",
                sql_quote(tenant),
                sql_quote(&normalized),
            ))
            .limit(1)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let entity_id = b
                .column_by_name("entity_id")
                .ok_or(StorageError::InvalidData("missing entity_id column"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or(StorageError::InvalidData("entity_id column type mismatch"))?;
            return Ok(Some(entity_id.value(0).to_string()));
        }
        Ok(None)
    }

    async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let mut filter = format!("tenant = {}", sql_quote(tenant));
        if let Some(k) = kind_filter {
            filter.push_str(&format!(" AND kind = {}", sql_quote(k.as_db_str())));
        }
        // canonical_name LIKE '%query%' — LanceDB's filter parser accepts
        // SQL LIKE patterns with `%` wildcards.
        if let Some(q) = query {
            filter.push_str(&format!(
                " AND canonical_name LIKE {}",
                sql_quote(&format!("%{q}%")),
            ));
        }
        let table = self
            .conn
            .open_table("entities")
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
        let mut entities = Vec::new();
        for b in &batches {
            entities.extend(record_batch_to_entities(b)?);
        }
        // ORDER BY created_at DESC — sort in-memory.
        entities.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        entities.truncate(limit);
        Ok(entities)
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
        let repo = LanceStore::open(&path).await.expect("open lancedb store");

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

    /// Exercises the batch-impl filter methods (`list_memories_for_tenant`,
    /// `list_memory_ids_for_tenant`, `find_by_idempotency_or_hash`,
    /// `search_candidates`, `recent_active_memories`,
    /// `fetch_memories_by_ids`, `list_pending_review`, `get_pending`,
    /// `get_memory`).
    #[tokio::test]
    async fn lancedb_filter_methods_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut a1 = fixture("mem_a_001", "tenant-a");
        a1.idempotency_key = Some("idem-a-1".into());
        let mut a2 = fixture("mem_a_002", "tenant-a");
        a2.status = MemoryStatus::PendingConfirmation;
        a2.content_hash = "h2".repeat(32);
        let mut a3 = fixture("mem_a_003", "tenant-a");
        a3.status = MemoryStatus::Archived;
        let b1 = fixture("mem_b_001", "tenant-b");

        for m in [&a1, &a2, &a3, &b1] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // list_memories_for_tenant
        let a_all = repo.list_memories_for_tenant("tenant-a").await.unwrap();
        assert_eq!(a_all.len(), 3);
        let b_all = repo.list_memories_for_tenant("tenant-b").await.unwrap();
        assert_eq!(b_all.len(), 1);

        // list_memory_ids_for_tenant
        let mut ids_a = repo.list_memory_ids_for_tenant("tenant-a").await.unwrap();
        ids_a.sort();
        assert_eq!(ids_a, vec!["mem_a_001", "mem_a_002", "mem_a_003"]);

        // find_by_idempotency_or_hash — match via idempotency_key
        let by_idem = repo
            .find_by_idempotency_or_hash("tenant-a", &Some("idem-a-1".into()), "no-such-hash")
            .await
            .unwrap();
        assert_eq!(by_idem.unwrap().memory_id, "mem_a_001");

        // ... match via content_hash when no idempotency_key supplied
        let by_hash = repo
            .find_by_idempotency_or_hash("tenant-a", &None, &a2.content_hash)
            .await
            .unwrap();
        assert_eq!(by_hash.unwrap().memory_id, "mem_a_002");

        // search_candidates — drops `archived`
        let cands = repo.search_candidates("tenant-a").await.unwrap();
        let mut cand_ids: Vec<_> = cands.iter().map(|m| m.memory_id.clone()).collect();
        cand_ids.sort();
        assert_eq!(cand_ids, vec!["mem_a_001", "mem_a_002"]);

        // recent_active_memories — same filter, with limit
        let recent = repo.recent_active_memories("tenant-a", 1).await.unwrap();
        assert_eq!(recent.len(), 1);

        // fetch_memories_by_ids — IN clause
        let by_ids = repo
            .fetch_memories_by_ids("tenant-a", &["mem_a_001", "mem_a_002"])
            .await
            .unwrap();
        assert_eq!(by_ids.len(), 2);
        // Empty input — short-circuit, no query.
        assert!(repo
            .fetch_memories_by_ids("tenant-a", &[])
            .await
            .unwrap()
            .is_empty());

        // list_pending_review
        let pending = repo.list_pending_review("tenant-a").await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].memory_id, "mem_a_002");

        // get_pending — exact one
        let p = repo.get_pending("tenant-a", "mem_a_002").await.unwrap();
        assert_eq!(p.unwrap().memory_id, "mem_a_002");
        // get_pending — wrong status returns None
        let np = repo.get_pending("tenant-a", "mem_a_001").await.unwrap();
        assert!(np.is_none());

        // get_memory — cross-tenant (no tenant filter)
        let cross = repo.get_memory("mem_b_001".into()).await.unwrap();
        assert_eq!(cross.unwrap().tenant, "tenant-b");
    }

    /// Mutating-method round-trip: accept_pending, reject_pending,
    /// replace_pending_with_successor, delete_memory_hard.
    #[tokio::test]
    async fn lancedb_mutating_methods_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut p = fixture("mem_p", "tenant");
        p.status = MemoryStatus::PendingConfirmation;
        let mut q = fixture("mem_q", "tenant");
        q.status = MemoryStatus::PendingConfirmation;
        let r = fixture("mem_r", "tenant");
        let s = fixture("mem_s", "tenant");
        for m in [&p, &q, &r, &s] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // accept_pending → status active
        let accepted = repo.accept_pending("tenant", "mem_p").await.unwrap();
        assert_eq!(accepted.status, MemoryStatus::Active);
        assert_eq!(accepted.memory_id, "mem_p");

        // reject_pending → status rejected
        let rejected = repo.reject_pending("tenant", "mem_q").await.unwrap();
        assert_eq!(rejected.status, MemoryStatus::Rejected);

        // After accept/reject, list_pending_review is empty
        let pending = repo.list_pending_review("tenant").await.unwrap();
        assert!(pending.is_empty());

        // replace_pending_with_successor: archive r, insert successor
        let mut succ = fixture("mem_r_v2", "tenant");
        succ.supersedes_memory_id = Some("mem_r".into());
        succ.version = 2;
        let returned = repo
            .replace_pending_with_successor("tenant", "mem_r", succ.clone())
            .await
            .unwrap();
        assert_eq!(returned.memory_id, "mem_r_v2");
        let archived = repo
            .get_memory_for_tenant("tenant", "mem_r")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.status, MemoryStatus::Archived);
        let successor_row = repo
            .get_memory_for_tenant("tenant", "mem_r_v2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(successor_row.supersedes_memory_id, Some("mem_r".into()));
        assert_eq!(successor_row.version, 2);

        // delete_memory_hard
        repo.delete_memory_hard("tenant", "mem_s").await.unwrap();
        let gone = repo.get_memory_for_tenant("tenant", "mem_s").await.unwrap();
        assert!(gone.is_none());

        // delete on non-existent → NotFound-equivalent error
        let err = repo
            .delete_memory_hard("tenant", "does-not-exist")
            .await
            .unwrap_err();
        assert!(
            matches!(err, StorageError::InvalidData("memory not found")),
            "expected NotFound-equivalent, got {err:?}",
        );
    }

    #[tokio::test]
    async fn lancedb_feedback_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let memory = fixture("mem_fb", "tenant");
        repo.insert_memory(memory.clone()).await.unwrap();

        // Apply 3 feedbacks of different kinds
        let make = |kind: &str, ts: &str, suffix: &str| FeedbackEvent {
            feedback_id: format!("fb_{suffix}"),
            memory_id: memory.memory_id.clone(),
            feedback_kind: kind.into(),
            created_at: ts.into(),
        };
        let _ = repo
            .apply_feedback(&memory, make("useful", "2026-05-08T01:00:00Z", "1"))
            .await
            .unwrap();
        let after_useful = repo
            .get_memory_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert!(
            after_useful.confidence > memory.confidence,
            "useful must increase confidence: {} vs {}",
            after_useful.confidence,
            memory.confidence,
        );
        assert!(after_useful.last_validated_at.is_some());

        let _ = repo
            .apply_feedback(&after_useful, make("outdated", "2026-05-08T02:00:00Z", "2"))
            .await
            .unwrap();
        let after_outdated = repo
            .get_memory_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert!(
            after_outdated.decay_score > after_useful.decay_score,
            "outdated must increase decay",
        );

        let _ = repo
            .apply_feedback(
                &after_outdated,
                make("incorrect", "2026-05-08T03:00:00Z", "3"),
            )
            .await
            .unwrap();
        let after_incorrect = repo
            .get_memory_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_incorrect.status,
            MemoryStatus::Archived,
            "incorrect must archive",
        );

        // list_feedback_for_memory — sorted ASC by created_at
        let events = repo.list_feedback_for_memory("mem_fb").await.unwrap();
        let kinds: Vec<_> = events.iter().map(|e| e.feedback_kind.as_str()).collect();
        assert_eq!(kinds, vec!["useful", "outdated", "incorrect"]);

        // feedback_summary — counts per kind
        let summary = repo.feedback_summary("mem_fb").await.unwrap();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.useful, 1);
        assert_eq!(summary.outdated, 1);
        assert_eq!(summary.incorrect, 1);
        assert_eq!(summary.applies_here, 0);
        assert_eq!(summary.does_not_apply_here, 0);

        // Empty feedback for a memory with none
        let summary_none = repo.feedback_summary("never-feedback'd").await.unwrap();
        assert_eq!(summary_none.total, 0);
    }

    /// `upsert_memory_embedding` + `semantic_search_memories` round-trip:
    /// insert two memories, write their embeddings, search by a query
    /// vector, expect both back in cosine-distance order with the closer
    /// vector ranked first. Also exercises tenant prefilter and
    /// `delete_memory_embedding`.
    #[tokio::test(flavor = "multi_thread")]
    async fn lancedb_embedding_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        // Two memories under "tenant-a", one under "tenant-b" (cross-tenant
        // leak test).
        let a1 = fixture("mem_emb_1", "tenant-a");
        let a2 = fixture("mem_emb_2", "tenant-a");
        let b1 = fixture("mem_emb_3", "tenant-b");
        for m in [&a1, &a2, &b1] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // Hand-rolled 4-d unit vectors. q ≈ v1 (close), v2 different,
        // v3 belongs to tenant-b and must not appear in tenant-a search.
        fn to_blob(v: &[f32]) -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 4);
            for f in v {
                out.extend_from_slice(&f.to_ne_bytes());
            }
            out
        }
        let v1 = vec![1.0_f32, 0.0, 0.0, 0.0];
        let v2 = vec![0.0_f32, 1.0, 0.0, 0.0];
        let v3 = vec![0.0_f32, 0.0, 1.0, 0.0];
        repo.upsert_memory_embedding(
            "mem_emb_1",
            "tenant-a",
            "fake-test",
            4,
            &to_blob(&v1),
            "h1",
            "00000001778000000000",
            "00000001778000000000",
        )
        .await
        .unwrap();
        repo.upsert_memory_embedding(
            "mem_emb_2",
            "tenant-a",
            "fake-test",
            4,
            &to_blob(&v2),
            "h2",
            "00000001778000000000",
            "00000001778000000000",
        )
        .await
        .unwrap();
        repo.upsert_memory_embedding(
            "mem_emb_3",
            "tenant-b",
            "fake-test",
            4,
            &to_blob(&v3),
            "h3",
            "00000001778000000000",
            "00000001778000000000",
        )
        .await
        .unwrap();

        // Query close to v1 → mem_emb_1 should rank first; mem_emb_3
        // (tenant-b) must be filtered out.
        let q = vec![0.99_f32, 0.14, 0.0, 0.0]; // ≈ unit, close to v1
        let hits = repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2, "tenant-a should have 2 hits, got {hits:?}");
        assert_eq!(hits[0].0.memory_id, "mem_emb_1", "v1 should rank first");
        assert_eq!(hits[1].0.memory_id, "mem_emb_2");
        // similarity ∈ (0, 1] for close-but-not-identical normalized vecs;
        // strictly greater than the v2 score.
        assert!(hits[0].1 > hits[1].1);

        // Upsert overwrite: re-write mem_emb_1 with v2 — now query close
        // to v1 should rank mem_emb_2 first (because both rows now have
        // v2-like vectors, but mem_emb_1 will be slightly off due to
        // float roundtrip, so we just check the row count stays at 2).
        repo.upsert_memory_embedding(
            "mem_emb_1",
            "tenant-a",
            "fake-test",
            4,
            &to_blob(&v2),
            "h1b",
            "00000001778000000001",
            "00000001778000000001",
        )
        .await
        .unwrap();
        let after_overwrite = repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(after_overwrite.len(), 2);

        // delete_memory_embedding removes the row from the search corpus.
        repo.delete_memory_embedding("mem_emb_2").await.unwrap();
        let after_delete = repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert_eq!(after_delete.len(), 1);
        assert_eq!(after_delete[0].0.memory_id, "mem_emb_1");

        // delete on no-row is a no-op (table exists but no matching row).
        repo.delete_memory_embedding("does-not-exist")
            .await
            .unwrap();

        // Search before any upsert (fresh repo, no memory_embeddings
        // table) returns empty without error.
        let dir2 = tempdir().unwrap();
        let path2 = dir2.path().join("empty.store");
        let empty_repo = LanceStore::open(&path2).await.unwrap();
        let empty_hits = empty_repo
            .semantic_search_memories("tenant-a", &q, 10)
            .await
            .unwrap();
        assert!(empty_hits.is_empty());
        // And delete on a missing table is a no-op.
        empty_repo
            .delete_memory_embedding("anything")
            .await
            .unwrap();
    }

    /// embedding_jobs queue end-to-end:
    /// enqueue (idempotent) → claim → complete; reschedule → re-claim;
    /// permanently_fail; mark_stale; list/filter; stale_live;
    /// delete_by_memory_id; latest_status_for_hash.
    #[tokio::test]
    async fn lancedb_embedding_jobs_queue_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let m1 = fixture("mem_q1", "tenant-a");
        let m2 = fixture("mem_q2", "tenant-a");
        for m in [&m1, &m2] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // Enqueue: first call creates, second is idempotent (dup detected).
        let insert1 = EmbeddingJobInsert {
            job_id: "job_1".into(),
            tenant: "tenant-a".into(),
            memory_id: "mem_q1".into(),
            target_content_hash: "hash_q1".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000000000".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
        };
        let enq1 = repo
            .try_enqueue_embedding_job(insert1.clone())
            .await
            .unwrap();
        assert!(enq1, "first enqueue should create");
        let enq1b = repo.try_enqueue_embedding_job(insert1).await.unwrap();
        assert!(!enq1b, "duplicate enqueue must return false");

        let first = repo
            .first_embedding_job_id_for_memory("mem_q1")
            .await
            .unwrap();
        assert_eq!(first, Some("job_1".into()));
        let none = repo
            .first_embedding_job_id_for_memory("does-not-exist")
            .await
            .unwrap();
        assert!(none.is_none());

        let status = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(status.as_deref(), Some("pending"));

        // Add a second job (different memory) so claim ordering is testable.
        let insert2 = EmbeddingJobInsert {
            job_id: "job_2".into(),
            tenant: "tenant-a".into(),
            memory_id: "mem_q2".into(),
            target_content_hash: "hash_q2".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000000001".into(),
            created_at: "00000001778000000001".into(),
            updated_at: "00000001778000000001".into(),
        };
        repo.try_enqueue_embedding_job(insert2).await.unwrap();

        // Claim 5: only 2 available; ordered by available_at ASC then
        // created_at ASC (job_1 first because earlier available_at).
        let now = "00000001778000010000";
        let claimed = repo.claim_next_n_embedding_jobs(now, 5, 5).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].job_id, "job_1");
        assert_eq!(claimed[1].job_id, "job_2");
        assert_eq!(claimed[0].attempt_count, 0);

        // After claim, both rows are 'processing'.
        let status_after = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(status_after.as_deref(), Some("processing"));

        // Re-claim returns nothing.
        let recl = repo.claim_next_n_embedding_jobs(now, 5, 5).await.unwrap();
        assert!(recl.is_empty());

        repo.complete_embedding_job("job_1", "00000001778000020000")
            .await
            .unwrap();
        let s1 = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(s1.as_deref(), Some("completed"));

        repo.reschedule_embedding_job_failure(
            "job_2",
            1,
            "transient",
            "00000001778000040000",
            "00000001778000030000",
        )
        .await
        .unwrap();
        let s2 = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q2", "hash_q2")
            .await
            .unwrap();
        assert_eq!(s2.as_deref(), Some("failed"));

        // Re-claim with budget=2 should pick job_2 again (failed,
        // attempt_count < max_retries, available_at <= now).
        let now2 = "00000001778000050000";
        let recl2 = repo.claim_next_n_embedding_jobs(now2, 2, 5).await.unwrap();
        assert_eq!(recl2.len(), 1);
        assert_eq!(recl2[0].job_id, "job_2");
        assert_eq!(recl2[0].attempt_count, 1);

        // Permanently fail it (attempt_count beyond budget).
        repo.permanently_fail_embedding_job("job_2", 5, "boom", "00000001778000060000")
            .await
            .unwrap();
        let recl3 = repo.claim_next_n_embedding_jobs(now2, 2, 5).await.unwrap();
        // Failed but attempt_count (5) >= max_retries (2) → not eligible.
        assert!(recl3.is_empty());

        repo.mark_embedding_job_stale("job_1", "00000001778000070000")
            .await
            .unwrap();
        let s_stale = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(s_stale.as_deref(), Some("stale"));

        // list_embedding_jobs: tenant filter.
        let all = repo
            .list_embedding_jobs("tenant-a", None, None, 50)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);

        // status filter.
        let only_failed = repo
            .list_embedding_jobs("tenant-a", Some("failed"), None, 50)
            .await
            .unwrap();
        assert_eq!(only_failed.len(), 1);
        assert_eq!(only_failed[0].job_id, "job_2");
        assert_eq!(only_failed[0].attempt_count, 5);

        // memory_id filter.
        let only_q1 = repo
            .list_embedding_jobs("tenant-a", None, Some("mem_q1"), 50)
            .await
            .unwrap();
        assert_eq!(only_q1.len(), 1);
        assert_eq!(only_q1[0].memory_id, "mem_q1");

        // stale_live: enqueue a fresh pending row, then sweep it stale.
        let insert3 = EmbeddingJobInsert {
            job_id: "job_3".into(),
            tenant: "tenant-a".into(),
            memory_id: "mem_q1".into(),
            target_content_hash: "hash_q1_v2".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000080000".into(),
            created_at: "00000001778000080000".into(),
            updated_at: "00000001778000080000".into(),
        };
        repo.try_enqueue_embedding_job(insert3).await.unwrap();
        let staled = repo
            .stale_live_embedding_jobs_for_memory(
                "tenant-a",
                "mem_q1",
                "fake-test",
                "00000001778000090000",
            )
            .await
            .unwrap();
        assert_eq!(staled, 1);

        let deleted = repo
            .delete_embedding_jobs_by_memory_id("mem_q1")
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        let remaining = repo
            .list_embedding_jobs("tenant-a", None, None, 50)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].memory_id, "mem_q2");

        // delete on no-row → 0.
        let zero = repo
            .delete_embedding_jobs_by_memory_id("nope")
            .await
            .unwrap();
        assert_eq!(zero, 0);
    }

    /// `bm25_candidates` lazy-creates the FTS index on `memories.content`
    /// the first time it's called, then BM25-ranks rows matching the
    /// query — distinct from semantic_search_memories (vector ANN).
    /// Tenant filter must be honored; empty query / k == 0 returns [].
    #[tokio::test]
    async fn lancedb_bm25_candidates_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut a = fixture("mem_b1", "tenant-a");
        a.content = "DuckDB single mutex serializes all writes".into();
        let mut b = fixture("mem_b2", "tenant-a");
        b.content = "LanceDB native vector search uses ANN".into();
        let mut c = fixture("mem_b3", "tenant-a");
        c.content = "Tantivy provides BM25 in DuckDB build".into();
        let mut d = fixture("mem_b4", "tenant-b");
        d.content = "DuckDB connection pool tenant-b".into();
        for m in [&a, &b, &c, &d] {
            repo.insert_memory(m.clone()).await.unwrap();
        }

        // Empty query → []; k=0 → [].
        let none1 = repo.bm25_candidates("tenant-a", "", 10).await.unwrap();
        assert!(none1.is_empty());
        let none2 = repo.bm25_candidates("tenant-a", "DuckDB", 0).await.unwrap();
        assert!(none2.is_empty());

        // Real query: 'DuckDB' should match mem_b1 + mem_b3 (tenant-a)
        // but NOT mem_b4 (tenant-b filter).
        let hits = repo
            .bm25_candidates("tenant-a", "DuckDB", 10)
            .await
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(ids.contains(&"mem_b1"), "got {ids:?}");
        assert!(ids.contains(&"mem_b3"), "got {ids:?}");
        assert!(!ids.contains(&"mem_b2"));
        assert!(
            !ids.contains(&"mem_b4"),
            "tenant filter must exclude tenant-b"
        );

        // Index now exists — second call should reuse, not rebuild.
        let table = repo.conn.open_table("memories").execute().await.unwrap();
        let indices = table.list_indices().await.unwrap();
        assert!(
            indices
                .iter()
                .any(|c| c.columns.iter().any(|col| col == "content")),
            "FTS index should exist on content column after first call",
        );

        // Different query, same tenant.
        let lance_hits = repo
            .bm25_candidates("tenant-a", "LanceDB", 10)
            .await
            .unwrap();
        let lance_ids: Vec<&str> = lance_hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(lance_ids.contains(&"mem_b2"));
    }

    /// `GraphStore` round-trip: sync_memory_edges (idempotent) →
    /// neighbors → close_edges_for_memory → related_memory_ids.
    #[tokio::test]
    async fn lancedb_graph_store_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let edges = vec![
            GraphEdge {
                from_node_id: "memory:m1".into(),
                to_node_id: "entity:e1".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000000000".into(),
                valid_to: None,
            },
            GraphEdge {
                from_node_id: "memory:m2".into(),
                to_node_id: "entity:e1".into(),
                relation: "mentions".into(),
                valid_from: "00000001778000000000".into(),
                valid_to: None,
            },
            GraphEdge {
                from_node_id: "memory:m1".into(),
                to_node_id: "entity:e2".into(),
                relation: "discusses".into(),
                valid_from: "00000001778000000000".into(),
                valid_to: None,
            },
        ];
        repo.sync_memory_edges(&edges, "00000001778000010000")
            .await
            .unwrap();
        // Idempotent: re-sync same edges → no duplicates.
        repo.sync_memory_edges(&edges, "00000001778000020000")
            .await
            .unwrap();
        let after_dup_sync = repo.neighbors("entity:e1").await.unwrap();
        assert_eq!(
            after_dup_sync.len(),
            2,
            "duplicate sync should not create new rows"
        );

        // neighbors at e1: 2 active edges (m1, m2 both 'mentions')
        let n_e1 = repo.neighbors("entity:e1").await.unwrap();
        assert_eq!(n_e1.len(), 2);
        // ordered by relation,from,to — mentions/m1, mentions/m2
        assert_eq!(n_e1[0].from_node_id, "memory:m1");
        assert_eq!(n_e1[1].from_node_id, "memory:m2");

        // related_memory_ids for [e1, e2]: should give {m1, m2}.
        let related = repo
            .related_memory_ids(&["entity:e1".into(), "entity:e2".into()])
            .await
            .unwrap();
        assert_eq!(related, vec!["m1".to_string(), "m2".to_string()]);

        // Close all edges from m1 (mentions e1 + discusses e2).
        let closed = repo.close_edges_for_memory("m1").await.unwrap();
        assert_eq!(closed, 2);

        // After close, neighbors(e1) drops to just m2's edge.
        let n_after = repo.neighbors("entity:e1").await.unwrap();
        assert_eq!(n_after.len(), 1);
        assert_eq!(n_after[0].from_node_id, "memory:m2");

        // related_memory_ids reflects the close — m1 is gone.
        let related2 = repo
            .related_memory_ids(&["entity:e1".into(), "entity:e2".into()])
            .await
            .unwrap();
        assert_eq!(related2, vec!["m2".to_string()]);

        // close on a memory with no active edges → 0.
        let zero = repo.close_edges_for_memory("nope").await.unwrap();
        assert_eq!(zero, 0);

        // Empty input edge list → no-op (no errors).
        repo.sync_memory_edges(&[], "00000001778000030000")
            .await
            .unwrap();

        // Empty input node_ids → empty Vec.
        let empty = repo.related_memory_ids(&[]).await.unwrap();
        assert!(empty.is_empty());
    }

    /// `EntityRegistry` round-trip: resolve_or_create idempotency, alias
    /// normalization (case + whitespace), get_entity, add_alias
    /// (Inserted / AlreadyOnSameEntity / ConflictWithDifferentEntity),
    /// lookup_alias, list_entities (kind + LIKE filters).
    #[tokio::test]
    async fn lancedb_entity_registry_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let id1 = repo
            .resolve_or_create(
                "tenant-a",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000000",
            )
            .await
            .unwrap();
        // Same alias under different casing/whitespace → same entity.
        let id1b = repo
            .resolve_or_create(
                "tenant-a",
                "  rust   ASYNC  ",
                EntityKind::Topic,
                "00000001778000000001",
            )
            .await
            .unwrap();
        assert_eq!(id1, id1b, "normalized alias must round-trip to same entity");

        let id2 = repo
            .resolve_or_create(
                "tenant-a",
                "DuckDB",
                EntityKind::Project,
                "00000001778000000002",
            )
            .await
            .unwrap();
        assert_ne!(id1, id2);

        // Different tenant, same alias → distinct entity.
        let id3 = repo
            .resolve_or_create(
                "tenant-b",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000003",
            )
            .await
            .unwrap();
        assert_ne!(id1, id3);

        let with_aliases = repo
            .get_entity("tenant-a", &id1)
            .await
            .unwrap()
            .expect("entity should exist");
        assert_eq!(with_aliases.entity.canonical_name, "Rust Async");
        assert_eq!(with_aliases.entity.kind, EntityKind::Topic);
        assert_eq!(with_aliases.aliases, vec!["rust async".to_string()]);

        let none = repo.get_entity("tenant-a", "does-not-exist").await.unwrap();
        assert!(none.is_none());

        // add_alias: new alias on same entity → Inserted.
        let r1 = repo
            .add_alias("tenant-a", &id1, "Tokio", "00000001778000000010")
            .await
            .unwrap();
        assert_eq!(r1, AddAliasOutcome::Inserted);

        // Same alias re-added → AlreadyOnSameEntity (idempotent).
        let r2 = repo
            .add_alias("tenant-a", &id1, "tokio", "00000001778000000011")
            .await
            .unwrap();
        assert_eq!(r2, AddAliasOutcome::AlreadyOnSameEntity);

        // Different entity claiming the same alias → Conflict.
        let r3 = repo
            .add_alias("tenant-a", &id2, "Tokio", "00000001778000000012")
            .await
            .unwrap();
        assert_eq!(
            r3,
            AddAliasOutcome::ConflictWithDifferentEntity(id1.clone())
        );

        // lookup_alias short-circuit.
        let look = repo.lookup_alias("tenant-a", "Rust Async").await.unwrap();
        assert_eq!(look.as_deref(), Some(id1.as_str()));
        let look_none = repo.lookup_alias("tenant-a", "unknown").await.unwrap();
        assert!(look_none.is_none());

        // list_entities: tenant-a has 2 entities, ORDER BY created_at DESC.
        let all_a = repo
            .list_entities("tenant-a", None, None, 10)
            .await
            .unwrap();
        assert_eq!(all_a.len(), 2);
        assert_eq!(all_a[0].entity_id, id2);
        assert_eq!(all_a[1].entity_id, id1);

        // kind filter.
        let topics = repo
            .list_entities("tenant-a", Some(EntityKind::Topic), None, 10)
            .await
            .unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].entity_id, id1);

        // LIKE filter on canonical_name.
        let like = repo
            .list_entities("tenant-a", None, Some("Rust"), 10)
            .await
            .unwrap();
        assert_eq!(like.len(), 1);
        assert_eq!(like[0].canonical_name, "Rust Async");

        // tenant-b has just the cross-tenant duplicate.
        let all_b = repo
            .list_entities("tenant-b", None, None, 10)
            .await
            .unwrap();
        assert_eq!(all_b.len(), 1);
        assert_eq!(all_b[0].entity_id, id3);
    }

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

    /// `TranscriptRepository` round-trip: create (idempotent) →
    /// get_by_session (ordering) → list_sessions (aggregation) →
    /// fetch_by_ids (input order) → context_window (before/after,
    /// include_tool_blocks toggle) → anchor_session_candidates →
    /// recent_conversation_messages → bm25_transcript_candidates.
    #[tokio::test]
    async fn lancedb_transcript_repository_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

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
