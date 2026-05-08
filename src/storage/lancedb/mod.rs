//! LanceDB backend (skeleton).
//!
//! `LanceDbRepository` is the alternate backend to [`crate::storage::DuckDbRepository`].
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
        // TODO: ensure_embedding_jobs_table, ensure_memory_embeddings_table,
        // ensure_conversation_messages_table, ensure_*…
        // (9 more tables — add as the corresponding trait methods leave
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
    ensure_table(conn, "memories", memories_schema()).await
}

async fn ensure_feedback_events_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "feedback_events", feedback_events_schema()).await
}

/// Idempotently create the `memory_embeddings` table with `dim`-sized
/// vectors. Lazy-created on first `upsert_memory_embedding` because dim
/// is provider-dependent and not known at `LanceDbRepository::open()`
/// time. If the table already exists with a different dim, subsequent
/// `Table::add` calls fail with a schema mismatch error — that's
/// surfaced as the original `lancedb::Error` and is the right behavior
/// (mixing dims would break vector search regardless).
async fn ensure_memory_embeddings_table(conn: &Connection, dim: i32) -> Result<(), StorageError> {
    ensure_table(conn, "memory_embeddings", memory_embeddings_schema(dim)).await
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

impl LanceDbRepository {
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

    /// Exercises the batch-impl filter methods (`list_memories_for_tenant`,
    /// `list_memory_ids_for_tenant`, `find_by_idempotency_or_hash`,
    /// `search_candidates`, `recent_active_memories`,
    /// `fetch_memories_by_ids`, `list_pending_review`, `get_pending`,
    /// `get_memory`).
    #[tokio::test]
    async fn lancedb_filter_methods_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceDbRepository::open(&path).await.unwrap();

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
        let repo = LanceDbRepository::open(&path).await.unwrap();

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
        let repo = LanceDbRepository::open(&path).await.unwrap();

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
        let repo = LanceDbRepository::open(&path).await.unwrap();

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
        let empty_repo = LanceDbRepository::open(&path2).await.unwrap();
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
}
