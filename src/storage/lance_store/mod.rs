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
mod episodes;
mod graph;
mod memories;
mod sessions;
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
        ensure_sessions_table(&conn).await?;
        ensure_episodes_table(&conn).await?;
        // memory_embeddings is lazy-created on first upsert (dim is
        // provider-dependent and unknown here without provider).

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

pub(super) async fn ensure_sessions_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "sessions", sessions_schema()).await
}

pub(super) async fn ensure_episodes_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "episodes", episodes_schema()).await
}

/// Arrow schema for the `sessions` table. Mirrors the legacy DuckDB
/// schema 1:1: session_id PK, tenant + caller_agent for identity,
/// started_at + last_seen_at + ended_at (nullable) for lifecycle, goal
/// (nullable string), memory_count (uint32) for usage stats.
fn sessions_schema() -> Schema {
    Schema::new(vec![
        Field::new("session_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("caller_agent", DataType::Utf8, false),
        Field::new("started_at", DataType::Utf8, false),
        Field::new("last_seen_at", DataType::Utf8, false),
        Field::new("ended_at", DataType::Utf8, true),
        Field::new("goal", DataType::Utf8, true),
        Field::new("memory_count", DataType::UInt32, false),
    ])
}

/// Arrow schema for the `episodes` table.
fn episodes_schema() -> Schema {
    let list_str = || DataType::List(Arc::new(Field::new("item", DataType::Utf8, false)));
    Schema::new(vec![
        Field::new("episode_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("goal", DataType::Utf8, false),
        Field::new("steps", list_str(), false),
        Field::new("outcome", DataType::Utf8, false),
        Field::new("evidence", list_str(), false),
        Field::new("scope", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("project", DataType::Utf8, true),
        Field::new("repo", DataType::Utf8, true),
        Field::new("module", DataType::Utf8, true),
        Field::new("tags", list_str(), false),
        Field::new("source_agent", DataType::Utf8, false),
        Field::new("idempotency_key", DataType::Utf8, true),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        // workflow_candidate as JSON-encoded string (nullable) — saves
        // schema churn vs. modeling the nested struct natively.
        Field::new("workflow_candidate", DataType::Utf8, true),
    ])
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
