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
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use lancedb::embeddings::{EmbeddingFunction, EmbeddingRegistry, MemoryRegistry};
use lancedb::Connection;
use serde::{de::DeserializeOwned, Serialize};

mod embedding;
mod entities;
mod graph;
mod memories;
mod transcripts;

use crate::domain::memory::{GraphEdge, MemoryRecord};
use crate::domain::{BlockType, ConversationMessage, MessageRole};
use crate::domain::{Entity, EntityKind};
use crate::storage::{FeedbackEvent, StorageError};

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

    pub async fn open_inner(
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
pub(super) fn lancedb_err(e: lancedb::Error) -> StorageError {
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

/// Counterpart of [`ensure_memory_embeddings_table`] for the
/// transcript-side embeddings. Lazy-created on first
/// `upsert_conversation_message_embedding` for the same reason
/// memory_embeddings is lazy: dim is provider-dependent.
async fn ensure_conversation_message_embeddings_table(
    conn: &Connection,
    dim: i32,
) -> Result<(), StorageError> {
    ensure_table(
        conn,
        "conversation_message_embeddings",
        conversation_message_embeddings_schema(dim),
    )
    .await
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

pub(super) fn feedback_events_to_record_batch(
    events: &[FeedbackEvent],
) -> Result<RecordBatch, StorageError> {
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

pub(super) fn record_batch_to_feedback_events(
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
pub(super) fn decode_embedding_blob(
    blob: &[u8],
    expected_dim: usize,
) -> Result<Vec<f32>, StorageError> {
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
pub(super) fn memory_embedding_to_record_batch(
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

/// Arrow schema for `conversation_message_embeddings`. Mirrors
/// `memory_embeddings` 1:1 with `memory_id` → `message_block_id`.
fn conversation_message_embeddings_schema(dim: i32) -> Schema {
    Schema::new(vec![
        Field::new("message_block_id", DataType::Utf8, false),
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

#[allow(clippy::too_many_arguments)]
pub(super) fn conversation_message_embedding_to_record_batch(
    message_block_id: &str,
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

    let mut id_b = StringBuilder::new();
    let mut tenant_b = StringBuilder::new();
    let mut model_b = StringBuilder::new();
    let mut dim_b = Int64Builder::new();
    let mut hash_b = StringBuilder::new();
    let mut src_ts_b = StringBuilder::new();
    let mut created_b = StringBuilder::new();
    let mut updated_b = StringBuilder::new();
    id_b.append_value(message_block_id);
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

    let schema = Arc::new(conversation_message_embeddings_schema(dim));
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(id_b.finish()),
        Arc::new(tenant_b.finish()),
        Arc::new(model_b.finish()),
        Arc::new(dim_b.finish()),
        Arc::new(emb_b.finish()),
        Arc::new(hash_b.finish()),
        Arc::new(src_ts_b.finish()),
        Arc::new(created_b.finish()),
        Arc::new(updated_b.finish()),
    ];
    RecordBatch::try_new(schema, columns).map_err(|e| {
        StorageError::InvalidInput(format!("conversation_message_embedding record batch: {e}"))
    })
}

/// Internal row representation for `embedding_jobs`. Mirrors DuckDB's
/// private `EmbeddingJobRow` (same field set, same types). Used as the
/// intermediate when reading record batches off LanceDB so we can sort
/// in memory before slicing — LanceDB's `QueryBase` doesn't expose
/// ORDER BY, so the queue's "oldest pending first" ordering is enforced
/// after the fact.
#[derive(Debug, Clone)]
pub(crate) struct EmbeddingJobRow {
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

pub(super) fn embedding_job_row_to_record_batch(
    row: &EmbeddingJobRow,
) -> Result<RecordBatch, StorageError> {
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

pub(super) fn record_batch_to_embedding_job_rows(
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
pub(crate) struct TranscriptEmbeddingJobRow {
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

pub(super) fn transcript_embedding_job_row_to_record_batch(
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

pub(super) fn record_batch_to_transcript_embedding_job_rows(
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

pub(super) fn graph_edge_to_record_batch(edge: &GraphEdge) -> Result<RecordBatch, StorageError> {
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

pub(super) fn record_batch_to_graph_edges(
    batch: &RecordBatch,
) -> Result<Vec<GraphEdge>, StorageError> {
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

pub(super) fn entity_to_record_batch(entity: &Entity) -> Result<RecordBatch, StorageError> {
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

pub(super) fn record_batch_to_entities(batch: &RecordBatch) -> Result<Vec<Entity>, StorageError> {
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

pub(super) fn entity_alias_to_record_batch(
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

pub(super) fn conversation_message_to_record_batch(
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

pub(super) fn record_batch_to_conversation_messages(
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
pub(super) fn feedback_adjustments(
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
pub(super) fn enum_to_str<T: Serialize>(v: &T) -> Result<String, StorageError> {
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
pub(super) fn enum_from_str<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(StorageError::Serde)
}

/// Serialize one or more `MemoryRecord`s to an Arrow `RecordBatch` matching
/// the [`memories_schema`] layout. Used by `insert_memory` to feed
/// `Table::add(...)`.
pub(super) fn memories_to_record_batch(
    memories: &[MemoryRecord],
) -> Result<RecordBatch, StorageError> {
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
pub(super) fn record_batch_to_memories(
    batch: &RecordBatch,
) -> Result<Vec<MemoryRecord>, StorageError> {
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
pub(super) fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// In-memory chronological ASC sort matching DuckDB's
/// `(created_at, line_number, block_index)` SQL ORDER BY.
pub(super) fn sort_messages_chronological_asc(msgs: &mut [ConversationMessage]) {
    msgs.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.line_number.cmp(&b.line_number))
            .then_with(|| a.block_index.cmp(&b.block_index))
    });
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
    pub async fn lancedb_insert_and_get_memory_round_trip() {
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
    pub async fn lancedb_filter_methods_round_trip() {
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
    pub async fn lancedb_mutating_methods_round_trip() {
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
    pub async fn lancedb_feedback_round_trip() {
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
    pub async fn lancedb_embedding_round_trip() {
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
    pub async fn lancedb_embedding_jobs_queue_round_trip() {
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
    pub async fn lancedb_bm25_candidates_round_trip() {
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
    pub async fn lancedb_graph_store_round_trip() {
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
    pub async fn lancedb_entity_registry_round_trip() {
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
