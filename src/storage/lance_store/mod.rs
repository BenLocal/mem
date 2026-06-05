//! LanceDB write half of the storage stack.
//!
//! `LanceStore` holds a `lancedb::Connection` and owns every WRITE
//! against the on-disk lance dataset directory (12 managed tables —
//! see `mod` declarations below + `ALL_TABLES` in `maintenance.rs`).
//! It's `pub(crate)` since Phase 5+; external callers reach it
//! through the `Backend` umbrella trait or one of the 9 sub-traits
//! in `src/storage/*.rs`, never directly. Reads route through
//! [`super::duckdb_query::DuckDbQuery`] instead — same on-disk lance
//! directory, attached as a DuckDB schema via the `lance` extension.
//!
//! What survives in this file after Phase 5+ dead-code cleanup is
//! exclusively WRITE methods plus a small read surface that's
//! load-bearing for write paths:
//!
//! - Bulk Arrow `RecordBatch` builders (`*_to_record_batch`).
//! - `Connection::open_table` + `.add()` / `.update().only_if(...)` /
//!   `.delete()` driven writes, one method per CRUD operation.
//! - Inline `lookup_alias` used by `resolve_or_create` / `add_alias`
//!   as a precondition (sole entity read kept here).
//! - `record_batch_to_capability_capsules` parser for cross-tenant
//!   `get_capability_capsule` (the one read still hosted here per
//!   `Store::get_capability_capsule`).
//! - `query_capability_capsules` / `query_conversation_messages` /
//!   `query_embedding_jobs` shared helpers — surviving callers are
//!   inside-this-module writes (supersede idempotency, batch dedup).
//!
//! The bulk of the lance-side READ methods (`list_capability_capsules_for_tenant`,
//! `neighbors`, `get_entity`, `bm25_transcript_candidates`, …) were
//! deleted in commit 908ce91 when reads canonically routed through
//! `DuckDbQuery`. If you find yourself adding a read method here,
//! first check that the equivalent doesn't already exist in
//! `duckdb_query/` — adding it back here only makes sense if the
//! call is on a write hot-path that can't afford a DuckDB
//! round-trip (e.g. embedding-job claim).
//!
//! ANN and FTS indexes are built into LanceDB 0.27 natively — no
//! external sidecar. FTS index is built at table-open time
//! (`ensure_fts_index`); ANN is auto-maintained on writes to the
//! vector column.

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
use lancedb::table::NewColumnTransform;
use lancedb::Connection;
use serde::{de::DeserializeOwned, Serialize};

mod capability_capsules;
mod embedding;
mod entities;
mod episodes;
mod graph;
mod maintenance;
pub(crate) mod mine_cursors;
mod sessions;
mod transcripts;

pub use maintenance::{IndexMaintenanceStats, VacuumStats};

use crate::domain::capability_capsule::{CapabilityCapsuleRecord, GraphEdge};
use crate::domain::Entity;
use crate::domain::{BlockType, ConversationMessage, MessageRole};
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
    /// that may diverge from the configured provider.
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

        ensure_capability_capsules_table(&conn).await?;
        ensure_feedback_events_table(&conn).await?;
        ensure_embedding_jobs_table(&conn).await?;
        ensure_graph_edges_table(&conn).await?;
        ensure_entities_table(&conn).await?;
        ensure_entity_aliases_table(&conn).await?;
        ensure_conversation_messages_table(&conn).await?;
        ensure_transcript_embedding_jobs_table(&conn).await?;
        ensure_sessions_table(&conn).await?;
        ensure_episodes_table(&conn).await?;
        ensure_mine_cursors_table(&conn).await?;
        // capability_capsule_embeddings is lazy-created on first upsert (dim is
        // provider-dependent and unknown here without provider).

        // FTS indexes for the BM25 read paths. Built once at open
        // time on empty tables — building the index is cheap when
        // the table has no rows, and creating it up front lets the
        // DuckDB query layer (`storage::duckdb_query`) call
        // `lance_fts(...)` directly without first probing for an
        // index. After this, every subsequent open is a no-op:
        // `ensure_fts_index` checks `Table::list_indices` and skips
        // creation when the column is already indexed.
        ensure_fts_index(&conn, "capability_capsules", "content").await?;
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

/// Typed column accessor shared by every `record_batch_to_*` parser.
/// Returns the same stable `&'static str` `StorageError` as the
/// pre-2026-05-21 inline helpers (so the HTTP layer's error mapping is
/// unchanged), but emits a structured `tracing::error!` line naming
/// the failing table + column + expected vs actual arrow type on
/// every failure.
///
/// This is the canonical version of the `col` helper that used to be
/// duplicated inline inside each parser (a26cdd2 enhanced one of
/// them; this lifts the pattern out so every parser benefits without
/// 5+ near-identical edits). Call from inside each `record_batch_to_*`
/// passing a stable `table` name string for the log context.
pub(super) fn parse_col<'a, T: 'static>(
    batch: &'a RecordBatch,
    table: &'static str,
    name: &'static str,
) -> Result<&'a T, StorageError> {
    let column = batch.column_by_name(name).ok_or_else(|| {
        tracing::error!(table = table, column = name, "missing column in batch");
        StorageError::InvalidData("missing column")
    })?;
    column.as_any().downcast_ref::<T>().ok_or_else(|| {
        tracing::error!(
            table = table,
            column = name,
            expected = std::any::type_name::<T>(),
            actual = %column.data_type(),
            "column type mismatch in batch",
        );
        StorageError::InvalidData("column type mismatch")
    })
}

/// Schema-drift-tolerant reader for the `version` column.
///
/// Commit `45c65f4` (2026-05-17) flipped the declared schema from
/// `UInt64` to `Int64` to drop four `i64::try_from(u64)` shims at the
/// Postgres boundary. New tables created after that commit are `Int64`;
/// long-lived prod dbs created before it keep `UInt64` because
/// `ensure_table` skips when the table already exists (no migration).
///
/// `DuckDbQuery` read paths coerce UInt64→i64 silently via
/// `row.get::<_, i64>`, so single-row gets / lists / searches work on
/// drifted dbs. The Lance read paths (`update_status` → strict
/// `Int64Array` downcast) do not, so `accept_pending` / `reject_pending`
/// / `apply_feedback` on a drifted db blow up with `column type
/// mismatch` immediately after a successful write.
///
/// This reader accepts either Arrow type and returns `i64`. The UInt64
/// branch is unreachable on fresh dbs; deleting it is safe once every
/// long-lived db has either been migrated or recreated.
pub(super) fn parse_version_column(
    batch: &RecordBatch,
    table: &'static str,
) -> Result<arrow_array::Int64Array, StorageError> {
    use arrow_array::{Int64Array, UInt64Array};
    let column = batch.column_by_name("version").ok_or_else(|| {
        tracing::error!(table, column = "version", "missing column in batch");
        StorageError::InvalidData("missing column")
    })?;
    if let Some(arr) = column.as_any().downcast_ref::<Int64Array>() {
        return Ok(arr.clone());
    }
    if let Some(arr) = column.as_any().downcast_ref::<UInt64Array>() {
        let mut out = arrow_array::builder::Int64Builder::with_capacity(arr.len());
        for i in 0..arr.len() {
            let v = arr.value(i);
            let signed = i64::try_from(v).map_err(|_| {
                tracing::error!(
                    table,
                    column = "version",
                    value = v,
                    "legacy UInt64 version > i64::MAX",
                );
                StorageError::InvalidData("version value overflows i64")
            })?;
            out.append_value(signed);
        }
        return Ok(out.finish());
    }
    tracing::error!(
        table,
        column = "version",
        expected = "Int64Array (current) or UInt64Array (pre-45c65f4 legacy)",
        actual = %column.data_type(),
        "version column type mismatch",
    );
    Err(StorageError::InvalidData("column type mismatch"))
}

/// Arrow schema for the `memories` LanceDB table. One column per
/// [`CapabilityCapsuleRecord`] field; enums (`capability_capsule_type`, `status`, `scope`,
/// `visibility`) are stored as their `serde` snake_case representation
/// for symmetry with the JSON-string encoding the DuckDB backend uses
/// in its `text` columns.
fn capability_capsules_schema() -> Schema {
    let str_list = DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
    Schema::new(vec![
        Field::new("capability_capsule_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("capability_capsule_type", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("scope", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("version", DataType::Int64, false),
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
        Field::new("supersedes_capability_capsule_id", DataType::Utf8, true),
        Field::new("source_agent", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        Field::new("last_validated_at", DataType::Utf8, true),
        Field::new("last_used_at", DataType::Utf8, true),
    ])
}

async fn ensure_capability_capsules_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "capability_capsules", capability_capsules_schema()).await?;
    migrate_capability_capsules_add_columns(conn).await
}

async fn ensure_feedback_events_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "feedback_events", feedback_events_schema()).await
}

/// Idempotently create the `capability_capsule_embeddings` table with `dim`-sized
/// vectors. Lazy-created on first `upsert_capability_capsule_embedding` because dim
/// is provider-dependent and not known at `LanceStore::open()`
/// time. If the table already exists with a different dim, subsequent
/// `Table::add` calls fail with a schema mismatch error — that's
/// surfaced as the original `lancedb::Error` and is the right behavior
/// (mixing dims would break vector search regardless).
async fn ensure_capability_capsule_embeddings_table(
    conn: &Connection,
    dim: i32,
) -> Result<(), StorageError> {
    ensure_table(
        conn,
        "capability_capsule_embeddings",
        capability_capsule_embeddings_schema(dim),
    )
    .await
}

/// Counterpart of [`ensure_capability_capsule_embeddings_table`] for the
/// transcript-side embeddings. Lazy-created on first
/// `upsert_conversation_message_embedding` for the same reason
/// capability_capsule_embeddings is lazy: dim is provider-dependent.
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
    ensure_table(conn, "graph_edges", graph_edges_schema()).await?;
    migrate_graph_edges_add_columns(conn).await
}

/// Migration (closes mempalace-diff-v3 K1, K3): pre-K1 `graph_edges`
/// tables on disk have a 5-column schema. Add the nullable `confidence`
/// (K1) and `extractor` (K3) columns — backfilled NULL via Lance's
/// `add_columns(AllNulls)` — so 7-column writes succeed and legacy rows
/// read back as `None`. Idempotent: a freshly-created 7-column table
/// already carries both columns, so this no-ops there. This is mem's
/// first on-disk schema migration; the generic `ensure_table` stays
/// create-only by design, so column-add migrations live in dedicated
/// per-table `ensure_*` wrappers like this one.
async fn migrate_graph_edges_add_columns(conn: &Connection) -> Result<(), StorageError> {
    let table = conn
        .open_table("graph_edges")
        .execute()
        .await
        .map_err(lancedb_err)?;
    let schema = table.schema().await.map_err(lancedb_err)?;
    let mut missing: Vec<Field> = Vec::new();
    if schema.field_with_name("confidence").is_err() {
        missing.push(Field::new("confidence", DataType::Float32, true));
    }
    if schema.field_with_name("extractor").is_err() {
        missing.push(Field::new("extractor", DataType::Utf8, true));
    }
    // K9 dynamics columns (same backfill-NULL migration).
    if schema.field_with_name("strength").is_err() {
        missing.push(Field::new("strength", DataType::Float32, true));
    }
    if schema.field_with_name("stability").is_err() {
        missing.push(Field::new("stability", DataType::Float32, true));
    }
    if schema.field_with_name("last_activated").is_err() {
        missing.push(Field::new("last_activated", DataType::Utf8, true));
    }
    if schema.field_with_name("access_count").is_err() {
        missing.push(Field::new("access_count", DataType::Int64, true));
    }
    if missing.is_empty() {
        return Ok(());
    }
    table
        .add_columns(
            NewColumnTransform::AllNulls(Arc::new(Schema::new(missing))),
            None,
        )
        .await
        .map_err(lancedb_err)?;
    Ok(())
}

/// Migration (roadmap O1): pre-O1 `capability_capsules` tables on disk
/// lack the `last_used_at` column. Add it nullable, backfilled NULL via
/// `add_columns(AllNulls)` — so the decay sweep's
/// `COALESCE(last_used_at, updated_at)` anchor falls back to `updated_at`
/// for legacy rows. Idempotent: a freshly-created table already carries
/// the column, so this no-ops there. Same pattern as
/// [`migrate_graph_edges_add_columns`].
async fn migrate_capability_capsules_add_columns(conn: &Connection) -> Result<(), StorageError> {
    let table = conn
        .open_table("capability_capsules")
        .execute()
        .await
        .map_err(lancedb_err)?;
    let schema = table.schema().await.map_err(lancedb_err)?;
    if schema.field_with_name("last_used_at").is_ok() {
        return Ok(());
    }
    table
        .add_columns(
            NewColumnTransform::AllNulls(Arc::new(Schema::new(vec![Field::new(
                "last_used_at",
                DataType::Utf8,
                true,
            )]))),
            None,
        )
        .await
        .map_err(lancedb_err)?;
    Ok(())
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

pub(super) async fn ensure_mine_cursors_table(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, "mine_cursors", mine_cursors_schema()).await
}

/// Arrow schema for `mine_cursors`. Per-transcript cursor recording
/// the highest `line_number` that the `mem mine` client has shipped
/// to the server. Used as a client-side optimization (v3 #32):
/// a re-run of `mine` against the same file can fast-skip parsed
/// lines whose number is ≤ the cursor, avoiding the parse + HTTP
/// round-trip cost for already-mined content. Server-side dedup
/// (idempotency_key + content_hash) still catches anything that
/// slips past the cursor — so the cursor is purely a perf hint,
/// never a correctness boundary.
///
/// Schema:
///   - `transcript_path` (PK) — absolute path; one cursor per file
///   - `last_line_number` — highest 1-based line number mined
///   - `updated_at` — 20-digit ms timestamp of last cursor write
fn mine_cursors_schema() -> Schema {
    Schema::new(vec![
        Field::new("transcript_path", DataType::Utf8, false),
        Field::new("last_line_number", DataType::Int64, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

/// Arrow schema for the `sessions` table. Layout: session_id PK,
/// tenant + caller_agent for identity, started_at + last_seen_at +
/// ended_at (nullable) for lifecycle, goal (nullable string),
/// memory_count (uint32) for usage stats.
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
///
/// `item` of every `List` field is declared nullable to match the
/// shape Lance/Arrow's default `ListBuilder<StringBuilder>` emits;
/// the parallel `capability_capsules` schema (line ~259) already
/// uses this convention. Marking items non-null here caused every
/// `propose_experience` write to fail validation with `column types
/// must match schema types, expected List(non-null Utf8) but found
/// List(Utf8)`.
fn episodes_schema() -> Schema {
    let list_str = || DataType::List(Arc::new(Field::new("item", DataType::Utf8, true)));
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
/// `feedback_events` schema (5 columns: feedback_id PK,
/// capability_capsule_id, feedback_kind, created_at, optional note).
fn feedback_events_schema() -> Schema {
    Schema::new(vec![
        Field::new("feedback_id", DataType::Utf8, false),
        Field::new("capability_capsule_id", DataType::Utf8, false),
        Field::new("feedback_kind", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        // Optional caller note (verbatim text). Nullable so old rows
        // and clients that never provide one stay valid.
        Field::new("note", DataType::Utf8, true),
    ])
}

pub(super) fn feedback_events_to_record_batch(
    events: &[FeedbackEvent],
) -> Result<RecordBatch, StorageError> {
    let mut feedback_id = StringBuilder::new();
    let mut capability_capsule_id = StringBuilder::new();
    let mut feedback_kind = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut note = StringBuilder::new();
    for e in events {
        feedback_id.append_value(&e.feedback_id);
        capability_capsule_id.append_value(&e.capability_capsule_id);
        feedback_kind.append_value(&e.feedback_kind);
        created_at.append_value(&e.created_at);
        match &e.note {
            Some(n) => note.append_value(n),
            None => note.append_null(),
        }
    }
    let schema = Arc::new(feedback_events_schema());
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(feedback_id.finish()),
        Arc::new(capability_capsule_id.finish()),
        Arc::new(feedback_kind.finish()),
        Arc::new(created_at.finish()),
        Arc::new(note.finish()),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| StorageError::InvalidInput(format!("feedback record batch: {e}")))
}

pub(super) fn record_batch_to_feedback_events(
    batch: &RecordBatch,
) -> Result<Vec<FeedbackEvent>, StorageError> {
    let feedback_id = parse_col::<StringArray>(batch, "feedback_events", "feedback_id")?;
    let capability_capsule_id =
        parse_col::<StringArray>(batch, "feedback_events", "capability_capsule_id")?;
    let feedback_kind = parse_col::<StringArray>(batch, "feedback_events", "feedback_kind")?;
    let created_at = parse_col::<StringArray>(batch, "feedback_events", "created_at")?;
    // `note` is optional in the batch — older datasets that pre-date
    // the column won't have it. Read defensively.
    let note = batch
        .column_by_name("note")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());
    let opt = |arr: &StringArray, i: usize| -> Option<String> {
        if arr.is_null(i) {
            None
        } else {
            Some(arr.value(i).to_string())
        }
    };
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(FeedbackEvent {
            feedback_id: feedback_id.value(i).to_string(),
            capability_capsule_id: capability_capsule_id.value(i).to_string(),
            feedback_kind: feedback_kind.value(i).to_string(),
            created_at: created_at.value(i).to_string(),
            note: note.and_then(|arr| opt(arr, i)),
        });
    }
    Ok(out)
}

/// Arrow schema for the `capability_capsule_embeddings` LanceDB table. The vector
/// column is `FixedSizeList<Float32, dim>` because LanceDB's ANN index
/// requires a known fixed dimension; `dim` comes from the upserting
/// caller (which knows the embedding model's output size).
fn capability_capsule_embeddings_schema(dim: i32) -> Schema {
    Schema::new(vec![
        Field::new("capability_capsule_id", DataType::Utf8, false),
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

// `decode_embedding_blob` moved to `crate::embedding::wire::decode_f32_blob`
// in QW-3; see `docs/backend-coupling.md` §4.3. Callers wrap the
// `&'static str` result into `StorageError::InvalidData` at use site.

/// Build a one-row `RecordBatch` for `capability_capsule_embeddings`. `embedding`
/// must already be the decoded `Vec<f32>` of length `dim`.
#[allow(clippy::too_many_arguments)]
pub(super) fn capability_capsule_embedding_to_record_batch(
    capability_capsule_id: &str,
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
    memory_id_b.append_value(capability_capsule_id);
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

    let schema = Arc::new(capability_capsule_embeddings_schema(dim));
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
    RecordBatch::try_new(schema, columns).map_err(|e| {
        StorageError::InvalidInput(format!("capability_capsule_embedding record batch: {e}"))
    })
}

/// Arrow schema for `conversation_message_embeddings`. Mirrors
/// `capability_capsule_embeddings` 1:1 with `capability_capsule_id` → `message_block_id`.
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
    capability_capsule_id: String,
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
        Field::new("capability_capsule_id", DataType::Utf8, false),
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
    embedding_job_rows_to_record_batch(std::slice::from_ref(row))
}

pub(super) fn embedding_job_rows_to_record_batch(
    rows: &[EmbeddingJobRow],
) -> Result<RecordBatch, StorageError> {
    let mut job_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut capability_capsule_id = StringBuilder::new();
    let mut target_content_hash = StringBuilder::new();
    let mut provider = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut attempt_count = Int64Builder::new();
    let mut last_error = StringBuilder::new();
    let mut available_at = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    for row in rows {
        job_id.append_value(&row.job_id);
        tenant.append_value(&row.tenant);
        capability_capsule_id.append_value(&row.capability_capsule_id);
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
    }
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(job_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(capability_capsule_id.finish()),
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
    use arrow_array::Int64Array;
    const TABLE: &str = "embedding_jobs";
    let job_id = parse_col::<StringArray>(batch, TABLE, "job_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let capability_capsule_id = parse_col::<StringArray>(batch, TABLE, "capability_capsule_id")?;
    let target_content_hash = parse_col::<StringArray>(batch, TABLE, "target_content_hash")?;
    let provider = parse_col::<StringArray>(batch, TABLE, "provider")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let attempt_count = parse_col::<Int64Array>(batch, TABLE, "attempt_count")?;
    let last_error = parse_col::<StringArray>(batch, TABLE, "last_error")?;
    let available_at = parse_col::<StringArray>(batch, TABLE, "available_at")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    let updated_at = parse_col::<StringArray>(batch, TABLE, "updated_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for i in 0..batch.num_rows() {
        out.push(EmbeddingJobRow {
            job_id: job_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            capability_capsule_id: capability_capsule_id.value(i).to_string(),
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
/// Mirrors `EmbeddingJobRow` (memories side) with `capability_capsule_id` →
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
    transcript_embedding_job_rows_to_record_batch(std::slice::from_ref(row))
}

pub(super) fn transcript_embedding_job_rows_to_record_batch(
    rows: &[TranscriptEmbeddingJobRow],
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
    for row in rows {
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
    }
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
    use arrow_array::Int64Array;
    const TABLE: &str = "transcript_embedding_jobs";
    let job_id = parse_col::<StringArray>(batch, TABLE, "job_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let message_block_id = parse_col::<StringArray>(batch, TABLE, "message_block_id")?;
    let provider = parse_col::<StringArray>(batch, TABLE, "provider")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let attempt_count = parse_col::<Int64Array>(batch, TABLE, "attempt_count")?;
    let last_error = parse_col::<StringArray>(batch, TABLE, "last_error")?;
    let available_at = parse_col::<StringArray>(batch, TABLE, "available_at")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    let updated_at = parse_col::<StringArray>(batch, TABLE, "updated_at")?;
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
        // K1 / K3 (closes mempalace-diff-v3 K1, K3): caller-declared
        // confidence + provenance tag. Both nullable so the
        // `add_columns(AllNulls)` migration can backfill legacy rows.
        Field::new("confidence", DataType::Float32, true),
        Field::new("extractor", DataType::Utf8, true),
        // K9 (closes mempalace-diff-v4 K9): edge "living weight"
        // dynamics. All nullable, backfilled NULL by the same
        // `add_columns(AllNulls)` migration as K1/K3.
        Field::new("strength", DataType::Float32, true),
        Field::new("stability", DataType::Float32, true),
        Field::new("last_activated", DataType::Utf8, true),
        Field::new("access_count", DataType::Int64, true),
    ])
}

pub(super) fn graph_edge_to_record_batch(edge: &GraphEdge) -> Result<RecordBatch, StorageError> {
    let mut from = StringBuilder::new();
    let mut to = StringBuilder::new();
    let mut relation = StringBuilder::new();
    let mut valid_from = StringBuilder::new();
    let mut valid_to = StringBuilder::new();
    let mut confidence = Float32Builder::new();
    let mut extractor = StringBuilder::new();
    let mut strength = Float32Builder::new();
    let mut stability = Float32Builder::new();
    let mut last_activated = StringBuilder::new();
    let mut access_count = Int64Builder::new();
    from.append_value(&edge.from_node_id);
    to.append_value(&edge.to_node_id);
    relation.append_value(&edge.relation);
    valid_from.append_value(&edge.valid_from);
    match &edge.valid_to {
        Some(s) => valid_to.append_value(s),
        None => valid_to.append_null(),
    }
    match edge.confidence {
        Some(c) => confidence.append_value(c),
        None => confidence.append_null(),
    }
    match &edge.extractor {
        Some(s) => extractor.append_value(s),
        None => extractor.append_null(),
    }
    match edge.strength {
        Some(v) => strength.append_value(v),
        None => strength.append_null(),
    }
    match edge.stability {
        Some(v) => stability.append_value(v),
        None => stability.append_null(),
    }
    match &edge.last_activated {
        Some(s) => last_activated.append_value(s),
        None => last_activated.append_null(),
    }
    match edge.access_count {
        Some(n) => access_count.append_value(n),
        None => access_count.append_null(),
    }
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(from.finish()),
        Arc::new(to.finish()),
        Arc::new(relation.finish()),
        Arc::new(valid_from.finish()),
        Arc::new(valid_to.finish()),
        Arc::new(confidence.finish()),
        Arc::new(extractor.finish()),
        Arc::new(strength.finish()),
        Arc::new(stability.finish()),
        Arc::new(last_activated.finish()),
        Arc::new(access_count.finish()),
    ];
    RecordBatch::try_new(Arc::new(graph_edges_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("graph_edge record batch: {e}")))
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
        // Catch-all JSON envelope/per-block metadata (cwd, git_branch,
        // parent_uuid, is_error, ...). Nullable so old rows + clients
        // that don't supply it stay valid.
        Field::new("meta_json", DataType::Utf8, true),
    ])
}

pub(super) fn conversation_messages_to_record_batch(
    msgs: &[ConversationMessage],
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
    let mut meta_json = StringBuilder::new();

    for msg in msgs {
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
        match &msg.meta_json {
            Some(s) => meta_json.append_value(s),
            None => meta_json.append_null(),
        }
    }

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
        Arc::new(meta_json.finish()),
    ];
    RecordBatch::try_new(Arc::new(conversation_messages_schema()), columns)
        .map_err(|e| StorageError::InvalidInput(format!("conversation_message record batch: {e}")))
}

pub(super) fn record_batch_to_conversation_messages(
    batch: &RecordBatch,
) -> Result<Vec<ConversationMessage>, StorageError> {
    use arrow_array::{BooleanArray, UInt32Array};
    const TABLE: &str = "conversation_messages";
    let message_block_id = parse_col::<StringArray>(batch, TABLE, "message_block_id")?;
    let session_id = parse_col::<StringArray>(batch, TABLE, "session_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let caller_agent = parse_col::<StringArray>(batch, TABLE, "caller_agent")?;
    let transcript_path = parse_col::<StringArray>(batch, TABLE, "transcript_path")?;
    let line_number = parse_col::<UInt64Array>(batch, TABLE, "line_number")?;
    let block_index = parse_col::<UInt32Array>(batch, TABLE, "block_index")?;
    let message_uuid = parse_col::<StringArray>(batch, TABLE, "message_uuid")?;
    let role = parse_col::<StringArray>(batch, TABLE, "role")?;
    let block_type = parse_col::<StringArray>(batch, TABLE, "block_type")?;
    let content = parse_col::<StringArray>(batch, TABLE, "content")?;
    let tool_name = parse_col::<StringArray>(batch, TABLE, "tool_name")?;
    let tool_use_id = parse_col::<StringArray>(batch, TABLE, "tool_use_id")?;
    let embed_eligible = parse_col::<BooleanArray>(batch, TABLE, "embed_eligible")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    // meta_json is optional in the batch — older datasets that
    // pre-date the column won't have it. Read defensively.
    let meta_json = batch
        .column_by_name("meta_json")
        .and_then(|c| c.as_any().downcast_ref::<StringArray>());

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
            meta_json: meta_json.and_then(|arr| opt(arr, i)),
        });
    }
    Ok(out)
}

// `feedback_adjustments` helper removed Phase 2 side-finding cleanup
// (2026-05-17): callers (storage backends) now use
// `crate::domain::capability_capsule::FeedbackKind::from_db_str()`
// + the domain `confidence_delta` / `decay_delta` / `status_after` /
// `marks_validated` helpers directly. The string→kind parser lived in
// storage by accident; pushing it to domain removes the cross-backend
// duplication.

/// Mirror of the DuckDB `encode_text` helper: serialize a snake_case-encoded
/// enum (e.g. `CapabilityCapsuleType`, `CapabilityCapsuleStatus`) to its plain JSON string token.
pub(super) fn enum_to_str<T: Serialize>(v: &T) -> Result<String, StorageError> {
    serde_json::to_value(v)
        .map_err(StorageError::Serde)?
        .as_str()
        .map(|s| s.to_string())
        .ok_or(StorageError::InvalidData(
            "expected string serialization for enum",
        ))
}

/// Inverse of `enum_to_str`. Used when materializing a `CapabilityCapsuleRecord` from
/// a `RecordBatch` row.
pub(super) fn enum_from_str<T: DeserializeOwned>(s: &str) -> Result<T, StorageError> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).map_err(StorageError::Serde)
}

/// Serialize one or more `CapabilityCapsuleRecord`s to an Arrow `RecordBatch` matching
/// the [`capability_capsules_schema`] layout. Used by `insert_capability_capsule` to feed
/// `Table::add(...)`.
pub(super) fn capability_capsules_to_record_batch(
    memories: &[CapabilityCapsuleRecord],
) -> Result<RecordBatch, StorageError> {
    let mut capability_capsule_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut capability_capsule_type = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut scope = StringBuilder::new();
    let mut visibility = StringBuilder::new();
    let mut version = Int64Builder::new();
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
    let mut supersedes_capability_capsule_id = StringBuilder::new();
    let mut source_agent = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    let mut last_validated_at = StringBuilder::new();
    let mut last_used_at = StringBuilder::new();

    for m in memories {
        capability_capsule_id.append_value(&m.capability_capsule_id);
        tenant.append_value(&m.tenant);
        capability_capsule_type.append_value(enum_to_str(&m.capability_capsule_type)?);
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
        match &m.supersedes_capability_capsule_id {
            Some(s) => supersedes_capability_capsule_id.append_value(s),
            None => supersedes_capability_capsule_id.append_null(),
        }
        source_agent.append_value(&m.source_agent);
        created_at.append_value(&m.created_at);
        updated_at.append_value(&m.updated_at);
        match &m.last_validated_at {
            Some(s) => last_validated_at.append_value(s),
            None => last_validated_at.append_null(),
        }
        match &m.last_used_at {
            Some(s) => last_used_at.append_value(s),
            None => last_used_at.append_null(),
        }
    }

    let schema = Arc::new(capability_capsules_schema());
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(capability_capsule_id.finish()),
        Arc::new(tenant.finish()),
        Arc::new(capability_capsule_type.finish()),
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
        Arc::new(supersedes_capability_capsule_id.finish()),
        Arc::new(source_agent.finish()),
        Arc::new(created_at.finish()),
        Arc::new(updated_at.finish()),
        Arc::new(last_validated_at.finish()),
        Arc::new(last_used_at.finish()),
    ];
    RecordBatch::try_new(schema, columns)
        .map_err(|e| StorageError::InvalidInput(format!("memories record batch: {e}")))
}

/// Inverse of `capability_capsules_to_record_batch`: parse a Lance query result into
/// `CapabilityCapsuleRecord`s.
///
/// Uses the shared [`parse_col`] helper for column-name-tagged decode
/// errors (a26cdd2 + follow-up). On any column failure the server log
/// gets a structured `tracing::error!` line naming the table, the column,
/// and expected-vs-actual arrow type; the HTTP-layer error stays the
/// same `&'static str` flavored `StorageError` so client error bodies
/// are unchanged.
pub(super) fn record_batch_to_capability_capsules(
    batch: &RecordBatch,
) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
    const TABLE: &str = "capability_capsules";
    let capability_capsule_id = parse_col::<StringArray>(batch, TABLE, "capability_capsule_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let capability_capsule_type =
        parse_col::<StringArray>(batch, TABLE, "capability_capsule_type")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let scope = parse_col::<StringArray>(batch, TABLE, "scope")?;
    let visibility = parse_col::<StringArray>(batch, TABLE, "visibility")?;
    let version = parse_version_column(batch, TABLE)?;
    let summary = parse_col::<StringArray>(batch, TABLE, "summary")?;
    let content = parse_col::<StringArray>(batch, TABLE, "content")?;
    let evidence = parse_col::<ListArray>(batch, TABLE, "evidence")?;
    let code_refs = parse_col::<ListArray>(batch, TABLE, "code_refs")?;
    let project = parse_col::<StringArray>(batch, TABLE, "project")?;
    let repo = parse_col::<StringArray>(batch, TABLE, "repo")?;
    let module = parse_col::<StringArray>(batch, TABLE, "module")?;
    let task_type = parse_col::<StringArray>(batch, TABLE, "task_type")?;
    let tags = parse_col::<ListArray>(batch, TABLE, "tags")?;
    let topics = parse_col::<ListArray>(batch, TABLE, "topics")?;
    let confidence = parse_col::<Float32Array>(batch, TABLE, "confidence")?;
    let decay_score = parse_col::<Float32Array>(batch, TABLE, "decay_score")?;
    let content_hash = parse_col::<StringArray>(batch, TABLE, "content_hash")?;
    let idempotency_key = parse_col::<StringArray>(batch, TABLE, "idempotency_key")?;
    let session_id = parse_col::<StringArray>(batch, TABLE, "session_id")?;
    let supersedes_capability_capsule_id =
        parse_col::<StringArray>(batch, TABLE, "supersedes_capability_capsule_id")?;
    let source_agent = parse_col::<StringArray>(batch, TABLE, "source_agent")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    let updated_at = parse_col::<StringArray>(batch, TABLE, "updated_at")?;
    let last_validated_at = parse_col::<StringArray>(batch, TABLE, "last_validated_at")?;
    let last_used_at = parse_col::<StringArray>(batch, TABLE, "last_used_at")?;

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
        out.push(CapabilityCapsuleRecord {
            capability_capsule_id: capability_capsule_id.value(i).to_string(),
            tenant: tenant.value(i).to_string(),
            capability_capsule_type: enum_from_str(capability_capsule_type.value(i))?,
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
            supersedes_capability_capsule_id: opt(supersedes_capability_capsule_id, i),
            source_agent: source_agent.value(i).to_string(),
            created_at: created_at.value(i).to_string(),
            updated_at: updated_at.value(i).to_string(),
            last_validated_at: opt(last_validated_at, i),
            last_used_at: opt(last_used_at, i),
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
