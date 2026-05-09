//! DuckDB SQL query layer over Lance datasets.
//!
//! Reads-only client. Pairs with [`crate::storage::lance_store::LanceStore`]
//! (the writer) — both point at the same on-disk lance directory; rows
//! written through `LanceStore`'s Rust API are immediately visible to
//! SQL here without any DETACH/re-ATTACH ceremony (verified by
//! `examples/lance_duckdb_poc.rs`).
//!
//! Architecture: in-process DuckDB connection. `INSTALL lance; LOAD
//! lance;` resolves the core extension; `ATTACH '<path>' AS ns (TYPE
//! LANCE)` exposes every dataset under the directory as
//! `ns.main.<table>`. From there, all reads are plain SQL — including
//! GROUP BY / window functions / subqueries that the LanceDB native
//! query API doesn't expose. ANN and FTS go through the extension's
//! `lance_vector_search()` / `lance_fts()` table functions.
//!
//! Concurrency: DuckDB is single-writer. We hold the connection in an
//! `Arc<Mutex<Connection>>` so concurrent reads serialize through one
//! mutex. Methods are `async fn` for ergonomic call sites — bodies use
//! `tokio::task::spawn_blocking` to run the blocking SQL on the thread
//! pool, so the runtime worker thread isn't pinned. This mirrors the
//! pattern the legacy `DuckDbRepository` used (and is the only
//! reasonable way to bridge sync `duckdb-rs` 1.x into an async service
//! layer).
//!
//! **Coverage so far** (memories table):
//!   - `list_memories_for_tenant`
//!   - `get_memory_for_tenant`
//!   - `get_pending`
//!   - `find_by_idempotency_or_hash`
//!   - `list_pending_review`
//!   - `recent_active_memories`
//!   - `search_candidates`
//!   - `fetch_memories_by_ids`
//!   - `list_memory_ids_for_tenant`
//!   - `list_memory_versions_for_tenant`
//!   - `bm25_candidates` (via `lance_fts`)
//!   - `semantic_search_memories` (via `lance_vector_search`)
//!
//! **Coverage so far** (conversation_messages table — transcript reads):
//!   - `get_conversation_messages_by_session`
//!   - `get_conversation_messages_by_session_paged`
//!   - `list_transcript_sessions` (`GROUP BY session_id` — the
//!     canonical example of what the DuckDB-as-query layer buys us
//!     over the LanceDB native API, which has no GROUP BY)
//!   - `fetch_conversation_messages_by_ids`
//!   - `context_window_for_block`
//!   - `anchor_session_candidates`
//!   - `recent_conversation_messages`
//!   - `bm25_transcript_candidates` (via `lance_fts`)
//!
//! **Coverage so far** (graph_edges table — graph reads):
//!   - `neighbors`
//!   - `related_memory_ids`
//!
//! **Coverage so far** (entities + entity_aliases — entity-registry reads):
//!   - `get_entity`
//!   - `lookup_alias`
//!   - `list_entities`
//!
//! All trait read methods are now backed by SQL. The next commit
//! introduces the `Store` composition layer (writes via LanceStore,
//! reads via DuckDbQuery) and starts the service-layer cutover.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use duckdb::types::Value;
use duckdb::{params, Connection, OptionalExt};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::{ContextWindow, GraphError, StorageError, TranscriptSessionSummary};
use crate::domain::memory::{GraphEdge, MemoryRecord, MemoryStatus, MemoryVersionLink};
use crate::domain::{
    BlockType, ConversationMessage, Entity, EntityKind, EntityWithAliases, MessageRole,
};
use crate::pipeline::entity_normalize::normalize_alias;

/// Read-only DuckDB SQL client backed by lance datasets ATTACHed at
/// open time. See module-level docs for the architecture.
#[derive(Clone)]
pub struct DuckDbQuery {
    conn: Arc<Mutex<Connection>>,
    /// Original lance directory path. Stored so [`Self::refresh`]
    /// can re-ATTACH after lance writes from outside the DuckDB
    /// connection (which the extension's snapshot caching otherwise
    /// hides).
    lance_path: PathBuf,
}

impl DuckDbQuery {
    /// Open an in-memory DuckDB, install + load the `lance` core
    /// extension, and ATTACH `lance_path` as namespace `ns`. The
    /// directory must already exist with at least one Lance dataset
    /// inside (typically created by `LanceStore::open` before this
    /// method is called).
    ///
    /// Apostrophes in the path are SQL-escaped (doubled) for the ATTACH
    /// statement; the path is otherwise embedded verbatim.
    ///
    /// **Network:** first run downloads the lance extension binary
    /// (~few MB) from `extensions.duckdb.org` into
    /// `~/.duckdb/extensions/<duckdb-version>/<platform>/`. Subsequent
    /// runs are offline.
    ///
    /// **Snapshot caching:** the lance extension caches the dataset
    /// version at first query post-ATTACH. Subsequent writes via the
    /// LanceDB Rust API (which is how `LanceStore` mutates) are
    /// invisible to this connection until [`Self::refresh`] is
    /// called. The `Store` wrapper does that refresh after every
    /// mutating call; direct `DuckDbQuery` users (only the
    /// per-module unit tests today) need to do it themselves.
    pub async fn open(lance_path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = lance_path.as_ref().to_path_buf();
        let path_for_thread = path.clone();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, StorageError> {
            let path_str = path_for_thread
                .to_str()
                .ok_or(StorageError::InvalidData("lance path must be UTF-8"))?;
            let escaped = path_str.replace('\'', "''");
            let c = Connection::open_in_memory()?;
            c.execute_batch("INSTALL lance; LOAD lance;")?;
            c.execute_batch(&format!("ATTACH '{escaped}' AS ns (TYPE LANCE);"))?;
            Ok(c)
        })
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            lance_path: path,
        })
    }

    /// Replace the in-process DuckDB connection with a fresh one
    /// (re-INSTALL/LOAD the lance extension and re-ATTACH the
    /// dataset). The lance extension caches the dataset version
    /// inside a connection's extension state; DETACH + re-ATTACH on
    /// the same connection isn't enough to clear that cache —
    /// empirically (see `store_open_write_read_round_trip` test
    /// probes), only a brand-new Connection picks up writes done
    /// via the LanceDB Rust API since the previous attach.
    ///
    /// Cost: maybe 100ms per call (connection setup + extension
    /// load + ATTACH). Called by `Store` after every mutating method
    /// so reads from the same `DuckDbQuery` instance always see the
    /// latest version. Read-heavy workloads pay nothing extra
    /// because writes are the trigger.
    ///
    /// (TODO: investigate `lance-duckdb` extension internals — if
    /// there's a cheaper way to invalidate the cache, e.g. a
    /// `lance_refresh()` SQL function the extension may expose,
    /// substitute it here.)
    pub async fn refresh(&self) -> Result<(), StorageError> {
        let conn_arc = self.conn.clone();
        let path = self.lance_path.clone();
        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let path_str = path
                .to_str()
                .ok_or(StorageError::InvalidData("lance path must be UTF-8"))?;
            let escaped = path_str.replace('\'', "''");
            let new_conn = Connection::open_in_memory()?;
            new_conn.execute_batch("INSTALL lance; LOAD lance;")?;
            new_conn.execute_batch(&format!("ATTACH '{escaped}' AS ns (TYPE LANCE);"))?;
            // Swap the inner connection. Previous prepared
            // statements are dropped along with the old conn — that
            // matters if a caller cached a `Statement` outside the
            // mutex, but `DuckDbQuery` always re-prepares per call,
            // so it's safe.
            *conn_arc.lock().expect("duckdb_query mutex poisoned") = new_conn;
            Ok(())
        })
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))?
    }

    /// All memories for `tenant`. Mirrors the DuckDB-as-storage
    /// implementation's signature 1:1 so the eventual service-layer
    /// switch is a method-call swap, not a type swap.
    pub async fn list_memories_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories WHERE tenant = ?1",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant], row_to_memory_record)?;
            collect_memories(rows)
        })
        .await
    }

    /// Single memory by `(tenant, memory_id)`. Returns `Ok(None)` when
    /// no row matches (the canonical "not found" path — distinct from
    /// errors).
    pub async fn get_memory_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let memory_id = memory_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND memory_id = ?2",
                MEMORY_COLS = MEMORY_COLS,
            );
            conn.query_row(&sql, params![tenant, memory_id], row_to_memory_record)
                .optional()
                .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Single pending-confirmation memory by `(tenant, memory_id)`.
    /// Used by the review-queue UI's edit/inspect flow — surfaces
    /// `Ok(None)` if the row is gone or has already been
    /// accepted/rejected (status moved off `pending_confirmation`).
    pub async fn get_pending(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let memory_id = memory_id.to_string();
        let status = enum_to_text(&MemoryStatus::PendingConfirmation)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND memory_id = ?2 AND status = ?3",
                MEMORY_COLS = MEMORY_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, memory_id, status],
                row_to_memory_record,
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Idempotency check used by `MemoryService::ingest`. Matches on
    /// either an `idempotency_key` (when caller supplied one) or the
    /// `content_hash` (always; functions as the natural-key duplicate
    /// check). Idempotency-key matches rank first (priority 0) so a
    /// caller-asserted identity wins over content-hash collisions; ties
    /// break by `updated_at DESC`. Returns the top row or `None`.
    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let idempotency_key = idempotency_key.clone();
        let content_hash = content_hash.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories
                 WHERE tenant = ?1
                   AND ((?2 IS NOT NULL AND idempotency_key = ?2) OR content_hash = ?3)
                 ORDER BY
                    CASE WHEN ?2 IS NOT NULL AND idempotency_key = ?2 THEN 0 ELSE 1 END,
                    updated_at DESC
                 LIMIT 1",
                MEMORY_COLS = MEMORY_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, idempotency_key.as_deref(), content_hash],
                row_to_memory_record,
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// All memories awaiting review (status =
    /// `pending_confirmation`) under `tenant`, oldest-newest first
    /// (well, ordered `created_at DESC` per legacy convention — newest
    /// arrivals at the top of the queue).
    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let status = enum_to_text(&MemoryStatus::PendingConfirmation)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND status = ?2 \
                 ORDER BY created_at DESC",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant, status], row_to_memory_record)?;
            collect_memories(rows)
        })
        .await
    }

    /// Most-recent non-rejected, non-archived memories under `tenant`
    /// — the empty-query fallback for `mem wake-up`. Ordered
    /// `(updated_at DESC, version DESC, memory_id ASC)` to keep ties
    /// deterministic when a batch of rows shares an `updated_at`
    /// timestamp.
    ///
    /// `limit` is clamped to `[1, 1024]` (mirrors the legacy bound).
    pub async fn recent_active_memories(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let rejected = enum_to_text(&MemoryStatus::Rejected)?;
        let archived = enum_to_text(&MemoryStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND status NOT IN (?2, ?3) \
                 ORDER BY updated_at DESC, version DESC, memory_id ASC \
                 LIMIT ?4",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![tenant, rejected, archived, lim],
                row_to_memory_record,
            )?;
            collect_memories(rows)
        })
        .await
    }

    /// Candidate pool for the ranking pipeline. Same row shape /
    /// ordering as `recent_active_memories` but **unbounded** — pulls
    /// the entire live (non-rejected, non-archived) set for `tenant`
    /// and returns it. Used by `pipeline::retrieve` to score every
    /// candidate; service code is expected to top-N afterward.
    ///
    /// For tenants with thousands of memories the wake-up fast path
    /// uses `recent_active_memories` instead — same filter, push the
    /// LIMIT to SQL.
    ///
    /// We do the status filter in SQL (rather than the legacy "fetch
    /// all then filter in Rust") because pushing predicates is the
    /// whole point of having DuckDB on top — every byte of an archived
    /// row that doesn't make it into Rust is a win.
    pub async fn search_candidates(&self, tenant: &str) -> Result<Vec<MemoryRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let rejected = enum_to_text(&MemoryStatus::Rejected)?;
        let archived = enum_to_text(&MemoryStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND status NOT IN (?2, ?3) \
                 ORDER BY updated_at DESC, version DESC, memory_id ASC",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant, rejected, archived], row_to_memory_record)?;
            collect_memories(rows)
        })
        .await
    }

    /// Bulk fetch by memory_id list, scoped to `tenant`. Uses
    /// `WHERE memory_id IN (?, ?, ...)` with N parameter binders.
    /// Returns rows in DB-natural order, **not** in input slice order;
    /// callers that need to preserve `ids` ordering reshape via a
    /// HashMap (the legacy hybrid-search path does this).
    ///
    /// Empty `ids` short-circuits to `Ok(vec![])` without touching the
    /// connection.
    ///
    /// Used by post-search hydration in `pipeline::retrieve`: ANN /
    /// BM25 returns memory_ids only; this fills the row data.
    pub async fn fetch_memories_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // ?1 is tenant; ?2..?(N+1) are the ids. Build the
            // placeholder list to match.
            let placeholders = (2..=ids.len() + 1)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM ns.main.memories \
                 WHERE tenant = ?1 AND memory_id IN ({placeholders})",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant)];
            for id in ids {
                params_vec.push(Box::new(id));
            }
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_memory_record)?;
            collect_memories(rows)
        })
        .await
    }

    /// Project just `memory_id` column for `tenant`, ordered
    /// `updated_at DESC`. Cheap admin / repair operation that doesn't
    /// need to hydrate the full row.
    pub async fn list_memory_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT memory_id FROM ns.main.memories \
                 WHERE tenant = ?1 ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map(params![tenant], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }

    /// Lexical recall via BM25 over the `memories.content` column.
    /// Routes through the lance extension's `lance_fts(...)` SQL table
    /// function — the FTS index itself is built once at
    /// [`LanceStore::open`] time on `(memories, content)`.
    ///
    /// Status filter (`NOT IN ('rejected', 'archived')`) and tenant
    /// scope are pushed to the outer `WHERE`. Oversampling: the
    /// table function is asked for `k * 2` BM25 hits (mirrors the
    /// legacy tantivy oversample) so the status filter has slack
    /// before the final `LIMIT k`. `_score` from the function drives
    /// the ORDER BY.
    ///
    /// Empty / whitespace query or `k == 0` short-circuits without
    /// touching DuckDB.
    pub async fn bm25_candidates(
        &self,
        tenant: &str,
        query: &str,
        k: usize,
    ) -> Result<Vec<MemoryRecord>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let query = query.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        let oversample = k_i.saturating_mul(2);
        let rejected = enum_to_text(&MemoryStatus::Rejected)?;
        let archived = enum_to_text(&MemoryStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {MEMORY_COLS} \
                 FROM lance_fts('ns.main.memories', 'content', ?1, k => ?2) \
                 WHERE tenant = ?3 AND status NOT IN (?4, ?5) \
                 ORDER BY _score DESC \
                 LIMIT ?6",
                MEMORY_COLS = MEMORY_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![query, oversample, tenant, archived, rejected, k_i],
                row_to_memory_record,
            )?;
            collect_memories(rows)
        })
        .await
    }

    /// Semantic recall via ANN over the `memory_embeddings` table's
    /// `embedding` column. Routes through the lance extension's
    /// `lance_vector_search(...)` SQL table function; joins back to
    /// `ns.main.memories` to hydrate the full `MemoryRecord`. Returns
    /// `(record, similarity)` pairs ordered by similarity DESC.
    ///
    /// **Score**: cosine similarity ∈ `[0, 1]` for normalized
    /// embeddings, derived from L2² as `1 - L2²/2` (see the
    /// [implementation comment][impl-comment] for why we can't ask
    /// the lance extension for cosine directly). EmbeddingProvider
    /// implementations are required to produce L2-normalized vectors,
    /// so this is the same score shape as the legacy backend's
    /// `cosine_similarity`.
    ///
    /// [impl-comment]: see the SQL building site below.
    ///
    /// **Query vector encoding**: inlined as a `FLOAT[]` literal —
    /// duckdb-rs 1.x has no `ToSql for &[f32]`, and query embeddings
    /// come from the trusted embedding provider stack (always finite,
    /// no NaN/Inf), so `Display`-formatting through the SQL string is
    /// safe and lossless via Rust's f32 round-trippable formatter.
    ///
    /// Empty `query_embedding` or `limit == 0` short-circuits.
    pub async fn semantic_search_memories(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(MemoryRecord, f32)>, StorageError> {
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
        let rejected = enum_to_text(&MemoryStatus::Rejected)?;
        let archived = enum_to_text(&MemoryStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // SELECT both the 27 memory cols (m.<col>) and _distance.
            // Done explicitly rather than via `m.*` so column ordering
            // stays in lock-step with `row_to_memory_record`'s indices.
            let m_cols = MEMORY_COLS
                .split(',')
                .map(|c| format!("m.{}", c.trim()))
                .collect::<Vec<_>>()
                .join(", ");
            // The lance_duckdb extension's `lance_vector_search`
            // accepts these named params only: `k`, `nprobs`,
            // `refine_factor`, `prefilter`, `use_index`,
            // `explain_verbose`. There is **no** `distance_type` kwarg
            // — the function uses L2² (squared euclidean) by default
            // when no vector index is attached, and otherwise inherits
            // the index's distance type. We need cosine similarity to
            // match the service-layer ranker's score contract.
            //
            // Workaround: for **L2-normalized** embeddings (the
            // EmbeddingProvider trait's invariant — every provider mem
            // ships with returns unit vectors),
            //
            //     cos_sim = 1 - L2² / 2
            //
            // because |a - b|² = |a|² + |b|² - 2·a·b
            //                  = 2 - 2·cos_sim  (when |a|=|b|=1).
            //
            // So `1.0 - _distance / 2.0` lands in `[0, 1]` for
            // typical normalized embeddings — same range and ordering
            // as the legacy DuckDB backend's cosine_similarity.
            // (If a non-normalized vector slips in, scores degrade
            // gracefully — same failure mode as the legacy backend.)
            let sql = format!(
                "SELECT {m_cols}, e._distance \
                 FROM lance_vector_search( \
                        'ns.main.memory_embeddings', 'embedding', {vector_lit}, k => ?1 \
                      ) AS e \
                 JOIN ns.main.memories AS m ON m.memory_id = e.memory_id \
                 WHERE m.tenant = ?2 AND m.status NOT IN (?3, ?4) \
                 ORDER BY e._distance ASC \
                 LIMIT ?5",
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![oversample, tenant, archived, rejected, lim],
                |row| {
                    let mem = row_to_memory_record(row)?;
                    let l2_squared: f32 = row.get(27)?;
                    // cos_sim = 1 - L2²/2 for normalized vectors (see
                    // SQL comment above for the derivation).
                    Ok((mem, 1.0_f32 - l2_squared / 2.0_f32))
                },
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }

    /// Version-chain metadata for `tenant` (every memory's version
    /// link, ordered `version DESC, updated_at DESC`).
    ///
    /// **TODO:** the legacy DuckDB-as-storage signature accepts a
    /// `memory_id` parameter but ignores it — the SQL only filters by
    /// tenant. Service-layer callers (`get_memory_detail`) expect the
    /// returned chain to be tenant-scoped *and* memory-scoped, so
    /// they're getting a too-broad answer today. Mirroring the broken
    /// behavior here for cutover parity; will fix with a proper
    /// version-chain walk (`WHERE memory_id = ?2 OR
    /// supersedes_memory_id = ?2`, recursive) in a follow-up.
    pub async fn list_memory_versions_for_tenant(
        &self,
        tenant: &str,
        memory_id: &str,
    ) -> Result<Vec<MemoryVersionLink>, StorageError> {
        let _ = memory_id;
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT memory_id, version, status, updated_at, supersedes_memory_id \
                 FROM ns.main.memories \
                 WHERE tenant = ?1 \
                 ORDER BY version DESC, updated_at DESC",
            )?;
            let rows = stmt.query_map(params![tenant], |row| {
                Ok(MemoryVersionLink {
                    memory_id: row.get(0)?,
                    version: row.get::<_, u64>(1)?,
                    status: parse_enum(&row.get::<_, String>(2)?)?,
                    updated_at: row.get(3)?,
                    supersedes_memory_id: row.get(4)?,
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
        cursor: Option<(&str, i64, i64)>,
        limit: usize,
    ) -> Result<(Vec<ConversationMessage>, bool), StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let session_id = session_id.to_string();
        let since = since.map(str::to_owned);
        let until = until.map(str::to_owned);
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
    /// Mirrors `semantic_search_memories` 1:1 with `memories` →
    /// `conversation_messages` and `memory_id` → `message_block_id`.
    /// Routes through the lance extension's `lance_vector_search`
    /// SQL function; joins back to `ns.main.conversation_messages`
    /// for the full row. Returns `(message, similarity)` pairs in
    /// descending similarity order.
    ///
    /// **Score**: cosine similarity ∈ `[0, 1]` for normalized
    /// embeddings, derived from the L2² distance lance returns as
    /// `1 - L2²/2` — see `semantic_search_memories` for the
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
                let l2_squared: f32 = row.get(15)?; // 15 conv cols → idx 15
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

    // ── Graph reads (`graph_edges` table) ───────────────────────────

    /// Active edges incident on `node_id`. Only edges with
    /// `valid_to IS NULL` are surfaced — closed (superseded) edges
    /// stay in the table for audit but never enter recall. Ordered
    /// `(relation, from_node_id, to_node_id)` for deterministic
    /// output (mirrors the legacy backend).
    pub async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError> {
        let conn = self.conn.clone();
        let node_id = node_id.to_string();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT from_node_id, to_node_id, relation, valid_from, valid_to \
                 FROM ns.main.graph_edges \
                 WHERE (from_node_id = ?1 OR to_node_id = ?1) AND valid_to IS NULL \
                 ORDER BY relation, from_node_id, to_node_id",
            )?;
            let rows = stmt.query_map(params![node_id], row_to_graph_edge)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .await
    }

    /// Memory ids reachable in one hop from any of `node_ids`,
    /// across active edges only. Used by `pipeline::retrieve` to
    /// expand the candidate pool with graph neighbors of seed
    /// nodes (e.g. memories that share an entity).
    ///
    /// Implementation: pull all edges where either endpoint is in
    /// `node_ids`, then for each edge keep the **opposite** endpoint
    /// (the one not in the input set), strip the `memory:` prefix,
    /// dedupe via HashSet, sort. SQL `IN (...)` push-down handles
    /// the filter; the endpoint-selection logic stays in Rust
    /// because it's per-row and DuckDB has no clean "the side that
    /// is NOT in (...)" expression.
    ///
    /// Empty input short-circuits.
    pub async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError> {
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let node_ids: Vec<String> = node_ids.to_vec();
        spawn_blocking_graph(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let placeholders = (1..=node_ids.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT from_node_id, to_node_id FROM ns.main.graph_edges \
                 WHERE (from_node_id IN ({placeholders}) OR to_node_id IN ({placeholders})) \
                   AND valid_to IS NULL"
            );
            let mut stmt = conn.prepare(&sql)?;
            // params are (bound 1..N then 1..N again — same set used
            // twice, once per IN clause).
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::with_capacity(node_ids.len());
            for n in &node_ids {
                params_vec.push(Box::new(n.clone()));
            }
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let node_set: std::collections::HashSet<&str> =
                node_ids.iter().map(|s| s.as_str()).collect();
            let mut memory_ids = std::collections::HashSet::new();
            for r in rows {
                let (from, to) = r?;
                for endpoint in [&from, &to] {
                    if !node_set.contains(endpoint.as_str()) {
                        if let Some(mid) = endpoint.strip_prefix("memory:") {
                            memory_ids.insert(mid.to_string());
                        }
                    }
                }
            }
            let mut out: Vec<String> = memory_ids.into_iter().collect();
            out.sort();
            Ok(out)
        })
        .await
    }

    // ── Entity-registry reads (`entities` + `entity_aliases`) ───────

    /// Fetch an entity row plus its alias list (ordered
    /// `created_at ASC, alias_text ASC`). Returns `Ok(None)` when no
    /// row matches `(tenant, entity_id)`. Two SELECTs because DuckDB
    /// SQL `array_agg(... ORDER BY ...)` would force the alias rows
    /// onto a single GROUP BY row but the legacy code keeps them
    /// in distinct rows; we mirror its shape.
    pub async fn get_entity(
        &self,
        tenant: &str,
        entity_id: &str,
    ) -> Result<Option<EntityWithAliases>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let entity_id = entity_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let entity = conn
                .query_row(
                    "SELECT entity_id, tenant, canonical_name, kind, created_at \
                     FROM ns.main.entities \
                     WHERE tenant = ?1 AND entity_id = ?2",
                    params![&tenant, &entity_id],
                    row_to_entity,
                )
                .optional()
                .map_err(StorageError::DuckDb)?;
            let Some(entity) = entity else {
                return Ok(None);
            };

            let mut stmt = conn.prepare(
                "SELECT alias_text FROM ns.main.entity_aliases \
                 WHERE tenant = ?1 AND entity_id = ?2 \
                 ORDER BY created_at ASC, alias_text ASC",
            )?;
            let rows =
                stmt.query_map(params![&tenant, &entity_id], |row| row.get::<_, String>(0))?;
            let mut aliases = Vec::new();
            for r in rows {
                aliases.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(Some(EntityWithAliases { entity, aliases }))
        })
        .await
    }

    /// Read-only normalized-alias lookup: returns the `entity_id`
    /// currently bound to `normalize_alias(alias)` under `tenant`,
    /// or `None`. Used by service-layer flows that need to pre-check
    /// alias ownership before attempting writes.
    pub async fn lookup_alias(
        &self,
        tenant: &str,
        alias: &str,
    ) -> Result<Option<String>, StorageError> {
        let normalized = normalize_alias(alias);
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.query_row(
                "SELECT entity_id FROM ns.main.entity_aliases \
                 WHERE tenant = ?1 AND alias_text = ?2",
                params![tenant, normalized],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// List entities under `tenant`, optionally filtered by `kind`
    /// and a `LIKE`-substring on `canonical_name`. Ordered
    /// `created_at DESC`, capped at `limit`.
    ///
    /// The `LIKE` pattern is parameterised — wrap the query in
    /// `%...%` so substring match works without the caller knowing
    /// about SQL wildcards. (DuckDB `LIKE` is case-sensitive; the
    /// legacy backend was the same — kept for parity. A future
    /// follow-up could swap to `ILIKE` for case-insensitive
    /// search.)
    pub async fn list_entities(
        &self,
        tenant: &str,
        kind_filter: Option<EntityKind>,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Entity>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let kind_filter = kind_filter.map(|k| k.as_db_str().to_string());
        let query = query.map(|q| format!("%{q}%"));
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut sql = String::from(
                "SELECT entity_id, tenant, canonical_name, kind, created_at \
                 FROM ns.main.entities WHERE tenant = ?1",
            );
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant)];
            if let Some(k) = kind_filter {
                sql.push_str(&format!(" AND kind = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(k));
            }
            if let Some(pat) = query {
                sql.push_str(&format!(
                    " AND canonical_name LIKE ?{}",
                    params_vec.len() + 1
                ));
                params_vec.push(Box::new(pat));
            }
            sql.push_str(" ORDER BY created_at DESC");
            sql.push_str(&format!(" LIMIT ?{}", params_vec.len() + 1));
            params_vec.push(Box::new(lim));

            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_entity)?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }

    // ── Embedding job status helpers ────────────────────────────────

    /// Read the `status` column of an embedding_jobs row by id. Used
    /// by the embedding worker to skip mid-flight processing if a
    /// concurrent caller (e.g. a supersede flow) marked the job
    /// stale before the embed completed. `None` if the row is gone.
    pub async fn get_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn.clone();
        let job_id = job_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.query_row(
                "SELECT status FROM ns.main.embedding_jobs WHERE job_id = ?1",
                params![job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Same shape as [`Self::get_embedding_job_status`] but for the
    /// transcript-side queue. Used by the transcript embedding worker
    /// to skip mid-flight processing if the job got marked stale by
    /// a concurrent caller.
    pub async fn get_transcript_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let conn = self.conn.clone();
        let job_id = job_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.query_row(
                "SELECT status FROM ns.main.transcript_embedding_jobs WHERE job_id = ?1",
                params![job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    // ── Bulk writes via DuckDB SQL ──────────────────────────────────

    /// Bulk decay sweep: increment `memories.decay_score` for every
    /// active row by a fraction of the days elapsed since
    /// `updated_at`, capped at 1.0, and bump `updated_at` to `now`.
    /// Used by the decay worker (called once per hour). Issued via
    /// DuckDB SQL through the lance extension — single statement,
    /// no Rust-side iteration.
    ///
    /// `now_ms` is the current timestamp in milliseconds (numeric);
    /// `now_ms_str` is the same value zero-padded to the 20-char
    /// string that mem uses for sortable timestamps.
    /// `decay_rate_per_day` is the per-day delta multiplier (e.g.
    /// `0.01` = 1% / day); `ms_per_day` is the time-base divisor
    /// (constant 86_400_000 in production but exposed for tests).
    ///
    /// Writes via DuckDB-side SQL invalidate the connection's own
    /// cache automatically (LanceStore Rust API writes do not — see
    /// [`Self::refresh`] doc), so no manual refresh is needed here.
    pub async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        let conn = self.conn.clone();
        let now_ms_str = now_ms_str.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            conn.execute(
                "UPDATE ns.main.memories \
                 SET decay_score = least(1.0, decay_score + ?1 * ((?2 - updated_at::double) / ?3)), \
                     updated_at = ?4 \
                 WHERE status = 'active' AND decay_score < 1.0",
                params![decay_rate_per_day, now_ms, ms_per_day, now_ms_str],
            )
            .map_err(StorageError::DuckDb)?;
            Ok(())
        })
        .await
    }
}

/// Run a synchronous DuckDB query body on a blocking-pool thread and
/// surface the result back to the async caller. Standardizes the
/// `spawn_blocking` ↔ `StorageError` conversion so individual methods
/// stay clean.
async fn spawn_blocking_storage<T, F>(f: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| StorageError::InvalidInput(format!("spawn_blocking join: {e}")))?
}

/// `spawn_blocking_storage` analogue for graph methods, which
/// surface `GraphError` instead of `StorageError`. Returns
/// `GraphError::Backend` for both spawn-join failures and per-row
/// `duckdb::Error`s — same shape the legacy backend's
/// `From<duckdb::Error> for GraphError` produced.
async fn spawn_blocking_graph<T, F>(f: F) -> Result<T, GraphError>
where
    F: FnOnce() -> Result<T, GraphError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| GraphError::Backend(format!("spawn_blocking join: {e}")))?
}

/// Decode a `graph_edges` row into a [`GraphEdge`]. The `valid_to`
/// column is nullable (closed edges have a timestamp; active edges
/// are NULL).
fn row_to_graph_edge(row: &duckdb::Row<'_>) -> duckdb::Result<GraphEdge> {
    Ok(GraphEdge {
        from_node_id: row.get(0)?,
        to_node_id: row.get(1)?,
        relation: row.get(2)?,
        valid_from: row.get(3)?,
        valid_to: row.get(4)?,
    })
}

/// Decode an `entities` row into an [`Entity`]. The `kind` column is
/// stored as a snake_case Utf8 string (mirrors LanceStore's encoding);
/// we go through `EntityKind::from_db_str` rather than a serde round
/// trip because the domain type already exposes that helper for the
/// legacy DuckDB-as-storage code path.
fn row_to_entity(row: &duckdb::Row<'_>) -> duckdb::Result<Entity> {
    let kind: String = row.get(3)?;
    Ok(Entity {
        entity_id: row.get(0)?,
        tenant: row.get(1)?,
        canonical_name: row.get(2)?,
        kind: EntityKind::from_db_str(&kind).ok_or_else(|| {
            duckdb::Error::FromSqlConversionFailure(
                3,
                duckdb::types::Type::Text,
                format!("invalid entity kind {kind:?}").into(),
            )
        })?,
        created_at: row.get(4)?,
    })
}

/// 15-column projection shared by every conversation_messages read.
/// Order must match `row_to_conversation_message` below — keep in sync.
const CONVERSATION_COLS: &str = "message_block_id, session_id, tenant, caller_agent, \
    transcript_path, line_number, block_index, message_uuid, role, block_type, content, \
    tool_name, tool_use_id, embed_eligible, created_at";

/// Parse one row of the 15-column conversation_messages SELECT into a
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

/// Collect rows from a `query_map` iterator into a `Vec<MemoryRecord>`,
/// converting the per-row `duckdb::Error` to `StorageError`.
fn collect_memories<I>(rows: I) -> Result<Vec<MemoryRecord>, StorageError>
where
    I: Iterator<Item = duckdb::Result<MemoryRecord>>,
{
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(StorageError::DuckDb)?);
    }
    Ok(out)
}

/// Serialize an enum to its snake_case Utf8 storage form, matching what
/// LanceStore writes. Inverse of `parse_enum`. Used for SQL parameter
/// binding when filtering by enum-string columns (e.g.
/// `status = 'pending_confirmation'`).
fn enum_to_text<T: Serialize>(value: &T) -> Result<String, StorageError> {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .ok_or(StorageError::InvalidData("enum serializes as non-string"))
}

/// 27-column projection shared by every memory-row read method.
/// Order must match `row_to_memory_record` below — keep in sync.
const MEMORY_COLS: &str = "memory_id, tenant, memory_type, status, scope, visibility, version, \
    summary, content, evidence, code_refs, project, repo, module, task_type, \
    tags, topics, confidence, decay_score, content_hash, idempotency_key, \
    session_id, supersedes_memory_id, source_agent, created_at, updated_at, \
    last_validated_at";

/// Parse one row of the 27-column SELECT above into a [`MemoryRecord`].
///
/// Type expectations (Lance Arrow → DuckDB SQL via the lance extension):
///   - `Utf8` → `VARCHAR` → `String` / `Option<String>`
///   - `List<Utf8>` → `VARCHAR[]` → `Vec<String>`
///   - `UInt64` → `UBIGINT` → `u64`
///   - `Float32` → `FLOAT` (a.k.a. `REAL`) → `f32`
///
/// Enum fields (`memory_type`, `status`, `scope`, `visibility`) live as
/// snake_case Utf8 strings on the Lance side per LanceStore's schema;
/// here we round-trip them through `serde_json::Value::String` so
/// `#[serde(rename_all = "snake_case")]` on the enum lookups them
/// without needing a hand-written from-str table.
fn row_to_memory_record(row: &duckdb::Row<'_>) -> duckdb::Result<MemoryRecord> {
    Ok(MemoryRecord {
        memory_id: row.get(0)?,
        tenant: row.get(1)?,
        memory_type: parse_enum(&row.get::<_, String>(2)?)?,
        status: parse_enum(&row.get::<_, String>(3)?)?,
        scope: parse_enum(&row.get::<_, String>(4)?)?,
        visibility: parse_enum(&row.get::<_, String>(5)?)?,
        version: row.get::<_, u64>(6)?,
        summary: row.get(7)?,
        content: row.get(8)?,
        evidence: get_string_list(row, 9)?,
        code_refs: get_string_list(row, 10)?,
        project: row.get(11)?,
        repo: row.get(12)?,
        module: row.get(13)?,
        task_type: row.get(14)?,
        tags: get_string_list(row, 15)?,
        topics: get_string_list(row, 16)?,
        confidence: row.get::<_, f32>(17)?,
        decay_score: row.get::<_, f32>(18)?,
        content_hash: row.get(19)?,
        idempotency_key: row.get(20)?,
        session_id: row.get(21)?,
        supersedes_memory_id: row.get(22)?,
        source_agent: row.get(23)?,
        created_at: row.get(24)?,
        updated_at: row.get(25)?,
        last_validated_at: row.get(26)?,
    })
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> duckdb::Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|e| {
        duckdb::Error::FromSqlConversionFailure(0, duckdb::types::Type::Text, Box::new(e))
    })
}

/// Extract a `LIST(VARCHAR)` column as `Vec<String>`. duckdb-rs 1.x
/// doesn't ship a `FromSql` impl for `Vec<String>`, so we go through the
/// `Value` enum. NULL list → empty Vec (mem semantics: missing list ==
/// no items).
fn get_string_list(row: &duckdb::Row<'_>, idx: usize) -> duckdb::Result<Vec<String>> {
    let v: Value = row.get(idx)?;
    let items = match v {
        Value::List(items) | Value::Array(items) => items,
        Value::Null => return Ok(Vec::new()),
        other => {
            return Err(duckdb::Error::FromSqlConversionFailure(
                idx,
                duckdb::types::Type::Any,
                format!("expected LIST(VARCHAR) at column {idx}, got {other:?}").into(),
            ));
        }
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::Text(s) => Ok(s),
            Value::Null => Ok(String::new()),
            other => Err(duckdb::Error::FromSqlConversionFailure(
                idx,
                duckdb::types::Type::Any,
                format!("expected VARCHAR list element, got {other:?}").into(),
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryStatus, MemoryType, Scope, Visibility};
    use crate::storage::lance_store::LanceStore;
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
            summary: "round-trip".into(),
            content: "use bun for fast installs".into(),
            evidence: vec!["src/main.rs:42".into(), "Cargo.toml:11".into()],
            code_refs: vec!["foo::bar()".into()],
            project: Some("mem".into()),
            repo: Some("mem".into()),
            module: None,
            task_type: None,
            tags: vec!["tooling".into()],
            topics: vec!["bun".into()],
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

    /// Cross-stack round-trip: insert via LanceStore (Rust API write),
    /// list via DuckDbQuery (DuckDB SQL read against the same on-disk
    /// lance dataset). Validates:
    ///   - `INSTALL lance; LOAD lance; ATTACH ...` against a freshly
    ///     created lance directory.
    ///   - All 27 column types parse correctly through the SQL boundary
    ///     (incl. `List<Utf8>` → `VARCHAR[]` → `Vec<String>`,
    ///     `UInt64` → `UBIGINT` → `u64`, `Float32` → `FLOAT` → `f32`).
    ///   - Tenant filter scopes correctly.
    #[tokio::test(flavor = "multi_thread")]
    async fn lance_write_then_duckdb_read_memories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");

        // 1. Create + populate Lance dataset via the writer.
        let lance = LanceStore::open(&path).await.expect("LanceStore::open");
        lance
            .insert_memory(fixture("m1", "tenant-a"))
            .await
            .expect("insert m1");
        lance
            .insert_memory(fixture("m2", "tenant-a"))
            .await
            .expect("insert m2");
        lance
            .insert_memory(fixture("m3", "tenant-b"))
            .await
            .expect("insert m3");

        // 2. Open DuckDB query layer on the same path.
        let q = DuckDbQuery::open(&path).await.expect("DuckDbQuery::open");

        // 3. Read back through SQL. tenant-a → 2 rows; tenant-b → 1 row.
        let mut a = q
            .list_memories_for_tenant("tenant-a")
            .await
            .expect("list tenant-a");
        a.sort_by(|x, y| x.memory_id.cmp(&y.memory_id));
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].memory_id, "m1");
        assert_eq!(a[1].memory_id, "m2");
        // Spot-check rich types preserved through the SQL boundary.
        assert_eq!(a[0].evidence, vec!["src/main.rs:42", "Cargo.toml:11"]);
        assert_eq!(a[0].code_refs, vec!["foo::bar()"]);
        assert_eq!(a[0].tags, vec!["tooling"]);
        assert_eq!(a[0].topics, vec!["bun"]);
        assert_eq!(a[0].version, 1u64);
        assert!((a[0].confidence - 0.7).abs() < 1e-6);
        assert_eq!(a[0].project.as_deref(), Some("mem"));
        assert!(a[0].module.is_none());
        assert_eq!(a[0].status, MemoryStatus::Active);
        assert_eq!(a[0].memory_type, MemoryType::Implementation);

        let b = q
            .list_memories_for_tenant("tenant-b")
            .await
            .expect("list tenant-b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].memory_id, "m3");

        // Tenant that has no rows returns empty (not an error).
        let none = q
            .list_memories_for_tenant("does-not-exist")
            .await
            .expect("list missing tenant");
        assert!(none.is_empty());
    }

    /// Exercises the 4 single-row / filtered-list methods that build
    /// on the same SELECT prefix as `list_memories_for_tenant`:
    /// `get_memory_for_tenant`, `get_pending`,
    /// `find_by_idempotency_or_hash`, `list_pending_review`,
    /// `recent_active_memories`.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_memory_filters() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Seed: 1 active, 1 pending, 1 archived (excluded from
        // recent_active_memories), 1 rejected (also excluded), 1 in
        // tenant-b (cross-tenant exclusion).
        let mut active = fixture("m_active", "tenant-a");
        active.idempotency_key = Some("idemp-active".into());
        active.content_hash = "hash-active".into();
        active.updated_at = "00000001778000000020".into();
        let mut pending = fixture("m_pending", "tenant-a");
        pending.status = MemoryStatus::PendingConfirmation;
        pending.idempotency_key = Some("idemp-pending".into());
        pending.content_hash = "hash-pending".into();
        pending.updated_at = "00000001778000000010".into();
        let mut archived = fixture("m_archived", "tenant-a");
        archived.status = MemoryStatus::Archived;
        archived.updated_at = "00000001778000000005".into();
        let mut rejected = fixture("m_rejected", "tenant-a");
        rejected.status = MemoryStatus::Rejected;
        rejected.updated_at = "00000001778000000006".into();
        let other_tenant = fixture("m_other", "tenant-b");

        for m in [&active, &pending, &archived, &rejected, &other_tenant] {
            lance.insert_memory(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // get_memory_for_tenant — hit + miss + cross-tenant.
        let hit = q
            .get_memory_for_tenant("tenant-a", "m_active")
            .await
            .unwrap()
            .expect("active memory should exist");
        assert_eq!(hit.memory_id, "m_active");
        assert_eq!(hit.status, MemoryStatus::Active);
        let miss = q
            .get_memory_for_tenant("tenant-a", "does-not-exist")
            .await
            .unwrap();
        assert!(miss.is_none());
        let cross = q
            .get_memory_for_tenant("tenant-b", "m_active")
            .await
            .unwrap();
        assert!(cross.is_none(), "tenant filter must scope cross-tenant");

        // get_pending — only pending status surfaces.
        let pend = q
            .get_pending("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("pending row");
        assert_eq!(pend.memory_id, "m_pending");
        let pend_active = q.get_pending("tenant-a", "m_active").await.unwrap();
        assert!(
            pend_active.is_none(),
            "active row must not surface through get_pending"
        );

        // find_by_idempotency_or_hash:
        //   (a) idempotency-key match wins over content-hash match,
        //   (b) None idempotency_key falls through to hash,
        //   (c) miss → None.
        let by_idemp = q
            .find_by_idempotency_or_hash(
                "tenant-a",
                &Some("idemp-active".into()),
                "hash-pending", // would also match m_pending by hash
            )
            .await
            .unwrap()
            .expect("idempotency-key match should win");
        assert_eq!(by_idemp.memory_id, "m_active");
        let by_hash_only = q
            .find_by_idempotency_or_hash("tenant-a", &None, "hash-pending")
            .await
            .unwrap()
            .expect("hash match");
        assert_eq!(by_hash_only.memory_id, "m_pending");
        let by_miss = q
            .find_by_idempotency_or_hash("tenant-a", &None, "no-such-hash")
            .await
            .unwrap();
        assert!(by_miss.is_none());

        // list_pending_review — only pending_confirmation.
        let pending_list = q.list_pending_review("tenant-a").await.unwrap();
        assert_eq!(pending_list.len(), 1);
        assert_eq!(pending_list[0].memory_id, "m_pending");
        let other = q.list_pending_review("tenant-b").await.unwrap();
        assert!(other.is_empty(), "no pending in tenant-b");

        // recent_active_memories — pending + active stay; archived +
        // rejected drop. Cross-tenant excluded.
        let recent = q.recent_active_memories("tenant-a", 50).await.unwrap();
        let recent_ids: Vec<&str> = recent.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(
            recent_ids,
            vec!["m_active", "m_pending"],
            "ordered by updated_at DESC; archived/rejected excluded"
        );
        let recent_b = q.recent_active_memories("tenant-b", 50).await.unwrap();
        assert_eq!(recent_b.len(), 1);
        assert_eq!(recent_b[0].memory_id, "m_other");

        // limit clamps to >=1 even when caller passes 0 (mirrors the
        // legacy DuckDB-as-storage clamp).
        let recent_clamped = q.recent_active_memories("tenant-a", 0).await.unwrap();
        assert_eq!(recent_clamped.len(), 1);
    }

    /// Cluster A round-trip: `search_candidates`,
    /// `fetch_memories_by_ids`, `list_memory_ids_for_tenant`,
    /// `list_memory_versions_for_tenant`. All four operate on the
    /// memories table only; share the same fixture seeding.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_memory_collections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Seed: 4 memories — 2 active, 1 archived (excluded from
        // candidates), 1 in tenant-b. Spread updated_at so DESC
        // ordering is observable.
        let mut a = fixture("m_a", "tenant-a");
        a.updated_at = "00000001778000000050".into();
        a.version = 2;
        let mut b = fixture("m_b", "tenant-a");
        b.updated_at = "00000001778000000040".into();
        b.version = 1;
        let mut arc = fixture("m_arc", "tenant-a");
        arc.status = MemoryStatus::Archived;
        arc.updated_at = "00000001778000000030".into();
        let mut bv2 = fixture("m_b_v2", "tenant-a");
        bv2.supersedes_memory_id = Some("m_b".into());
        bv2.version = 2;
        bv2.updated_at = "00000001778000000060".into();
        let mut other = fixture("m_other", "tenant-b");
        other.updated_at = "00000001778000000020".into();
        for m in [&a, &b, &arc, &bv2, &other] {
            lance.insert_memory(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // search_candidates: archived/rejected excluded; tenant-scoped;
        // ordered (updated_at DESC, version DESC, memory_id ASC).
        let cands = q.search_candidates("tenant-a").await.unwrap();
        let cand_ids: Vec<&str> = cands.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(
            cand_ids,
            vec!["m_b_v2", "m_a", "m_b"],
            "tenant-a candidates: archived excluded, ordered by updated_at DESC"
        );
        let cands_b = q.search_candidates("tenant-b").await.unwrap();
        assert_eq!(cands_b.len(), 1);

        // fetch_memories_by_ids: in-clause batch lookup. Empty → empty.
        let empty = q.fetch_memories_by_ids("tenant-a", &[]).await.unwrap();
        assert!(empty.is_empty());

        let some = q
            .fetch_memories_by_ids("tenant-a", &["m_a", "m_b", "does-not-exist"])
            .await
            .unwrap();
        let some_ids: std::collections::HashSet<&str> =
            some.iter().map(|m| m.memory_id.as_str()).collect();
        assert_eq!(some.len(), 2);
        assert!(some_ids.contains("m_a"));
        assert!(some_ids.contains("m_b"));

        // tenant filter scopes the IN-clause: same id under different
        // tenant returns nothing.
        let cross = q.fetch_memories_by_ids("tenant-b", &["m_a"]).await.unwrap();
        assert!(
            cross.is_empty(),
            "tenant-a id must not appear in tenant-b lookup"
        );

        // list_memory_ids_for_tenant: just IDs, ordered updated_at DESC.
        let ids_a = q.list_memory_ids_for_tenant("tenant-a").await.unwrap();
        assert_eq!(
            ids_a,
            vec!["m_b_v2", "m_a", "m_b", "m_arc"],
            "all 4 tenant-a rows incl. archived; updated_at DESC"
        );
        let ids_empty = q
            .list_memory_ids_for_tenant("does-not-exist")
            .await
            .unwrap();
        assert!(ids_empty.is_empty());

        // list_memory_versions_for_tenant: ordered (version DESC,
        // updated_at DESC). NOTE: passes memory_id but the legacy
        // implementation ignores it; we mirror that here so behavior
        // stays parity until a follow-up fixes the version-chain
        // walk.
        let chain = q
            .list_memory_versions_for_tenant("tenant-a", "m_b")
            .await
            .unwrap();
        // Returns ALL tenant-a rows' version links — m_b_v2 (v=2) +
        // m_a (v=2) + m_b (v=1) + m_arc (v=1, fixture default).
        assert_eq!(chain.len(), 4);
        // The supersedes link is preserved.
        let bv2_link = chain
            .iter()
            .find(|l| l.memory_id == "m_b_v2")
            .expect("m_b_v2 in chain");
        assert_eq!(bv2_link.supersedes_memory_id.as_deref(), Some("m_b"));
        let b_link = chain
            .iter()
            .find(|l| l.memory_id == "m_b")
            .expect("m_b in chain");
        assert!(b_link.supersedes_memory_id.is_none());
    }

    /// `bm25_candidates` over the lance extension's `lance_fts(...)`
    /// SQL function, with the FTS index built at LanceStore::open.
    /// Verifies:
    ///   - Empty / k=0 → empty Vec.
    ///   - Lexical match returns the right rows.
    ///   - Tenant filter scopes correctly.
    ///   - Archived/rejected rows are excluded by the outer WHERE.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_bm25_candidates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // 4 memories with distinct content — 3 in tenant-a (one
        // archived), 1 in tenant-b.
        let mut a = fixture("m_duck", "tenant-a");
        a.content = "DuckDB single mutex serializes writes".into();
        let mut b = fixture("m_lance", "tenant-a");
        b.content = "LanceDB native vector search uses ANN".into();
        let mut c = fixture("m_arc", "tenant-a");
        c.status = MemoryStatus::Archived;
        c.content = "Tantivy provides BM25 in DuckDB build".into();
        let mut d = fixture("m_other", "tenant-b");
        d.content = "DuckDB connection pool tenant-b".into();
        for m in [&a, &b, &c, &d] {
            lance.insert_memory(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // empty query / k=0 short-circuits.
        let none1 = q.bm25_candidates("tenant-a", "", 5).await.unwrap();
        assert!(none1.is_empty());
        let none2 = q.bm25_candidates("tenant-a", "DuckDB", 0).await.unwrap();
        assert!(none2.is_empty());

        // 'DuckDB' matches m_duck (tenant-a, active) and m_arc
        // (tenant-a, archived). Archived must be filtered out.
        let hits = q.bm25_candidates("tenant-a", "DuckDB", 10).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(ids.contains(&"m_duck"), "got {ids:?}");
        assert!(
            !ids.contains(&"m_arc"),
            "archived row must be filtered out by status NOT IN clause; got {ids:?}",
        );
        assert!(
            !ids.contains(&"m_other"),
            "tenant-b row must not appear in tenant-a query; got {ids:?}",
        );

        // tenant-b sees its own row.
        let b_hits = q.bm25_candidates("tenant-b", "DuckDB", 10).await.unwrap();
        let b_ids: Vec<&str> = b_hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(b_ids.contains(&"m_other"));

        // Different query word.
        let lance_hits = q.bm25_candidates("tenant-a", "LanceDB", 10).await.unwrap();
        let lance_ids: Vec<&str> = lance_hits.iter().map(|m| m.memory_id.as_str()).collect();
        assert!(lance_ids.contains(&"m_lance"), "got {lance_ids:?}");
    }

    /// `semantic_search_memories` over `lance_vector_search(...)` with
    /// JOIN to memories. Inserts 3 memories with hand-rolled 4-d unit
    /// vectors via `upsert_memory_embedding`, then queries with a
    /// vector close to one of them and asserts ordering / score
    /// shape / tenant scope / archived exclusion.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_semantic_search_memories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        let m1 = fixture("m_v1", "tenant-a");
        let m2 = fixture("m_v2", "tenant-a");
        let mut m3 = fixture("m_v3", "tenant-a");
        m3.status = MemoryStatus::Archived;
        let m4 = fixture("m_v4", "tenant-b");
        for m in [&m1, &m2, &m3, &m4] {
            lance.insert_memory(m.clone()).await.unwrap();
        }

        // 4-d unit vectors. v1 ≈ [1,0,0,0]; v2 distant; v3 in tenant-a
        // but archived; v4 in tenant-b.
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
        let v4 = vec![0.0_f32, 0.0, 0.0, 1.0];
        let now = "00000001778000000000";
        for (id, tenant, vec, hash) in [
            ("m_v1", "tenant-a", &v1, "h1"),
            ("m_v2", "tenant-a", &v2, "h2"),
            ("m_v3", "tenant-a", &v3, "h3"),
            ("m_v4", "tenant-b", &v4, "h4"),
        ] {
            lance
                .upsert_memory_embedding(id, tenant, "fake-test", 4, &to_blob(vec), hash, now, now)
                .await
                .unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // Query close to v1: m_v1 ranks first; m_v2 also returns;
        // m_v3 archived → excluded; m_v4 cross-tenant → excluded.
        let query = vec![0.99_f32, 0.14, 0.0, 0.0];
        let hits = q
            .semantic_search_memories("tenant-a", &query, 10)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "tenant-a active memories with embeddings → 2 (m_v1, m_v2); got {hits:?}"
        );
        assert_eq!(hits[0].0.memory_id, "m_v1", "v1 ranks first (closest)");
        assert_eq!(hits[1].0.memory_id, "m_v2");
        assert!(
            hits[0].1 > hits[1].1,
            "similarity is descending: {} > {}",
            hits[0].1,
            hits[1].1,
        );
        // Sanity: similarity ∈ (0, 1] for normalized vectors close to
        // v1; the closer hit (≈cos(0.14)) should be > 0.99.
        assert!(hits[0].1 > 0.99);

        // Empty / 0-limit short-circuits.
        let empty1 = q
            .semantic_search_memories("tenant-a", &[], 10)
            .await
            .unwrap();
        assert!(empty1.is_empty());
        let empty2 = q
            .semantic_search_memories("tenant-a", &query, 0)
            .await
            .unwrap();
        assert!(empty2.is_empty());

        // tenant-b sees its own row.
        let b_hits = q
            .semantic_search_memories("tenant-b", &query, 10)
            .await
            .unwrap();
        assert_eq!(b_hits.len(), 1);
        assert_eq!(b_hits[0].0.memory_id, "m_v4");
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

    /// Transcript-table reads cross-stack round-trip. Same fixture
    /// shape as `lance_store::tests::lancedb_transcript_repository_round_trip`
    /// but every read goes through DuckDB SQL via the lance extension.
    /// Exercises every transcript method; `list_transcript_sessions` in
    /// particular replaces the in-memory aggregation that the legacy
    /// LanceDB-trait impl had to do (no GROUP BY in the LanceDB
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
            .get_conversation_messages_by_session_paged("tenant-a", "sess_a", None, None, None, 2)
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

    /// Graph + entity read cross-stack round-trip. Writes via the
    /// LanceStore Rust API (`sync_memory_edges`, `resolve_or_create`,
    /// `add_alias`) seed the tables; reads come back through DuckDB
    /// SQL via the lance extension. Exercises:
    ///   - `neighbors`: active-only filter, ordering, both-direction
    ///     incidence
    ///   - `related_memory_ids`: opposite-endpoint extraction, dedupe,
    ///     `memory:` prefix strip, empty input short-circuit
    ///   - `get_entity`: alias list ordered (created_at, alias_text)
    ///     ASC, missing id → None
    ///   - `lookup_alias`: normalization (case + whitespace
    ///     collapse), miss → None
    ///   - `list_entities`: tenant scope, kind filter, LIKE on
    ///     canonical_name, created_at DESC ordering, limit
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_graph_and_entity_reads() {
        use crate::domain::memory::GraphEdge;

        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // ── seed graph_edges via the writer ─────────────────────
        // mem:m1 mentions ent:e1; mem:m2 mentions ent:e1; mem:m1
        // discusses ent:e2. All active.
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
        lance
            .sync_memory_edges(&edges, "00000001778000010000")
            .await
            .unwrap();
        // Add a closed edge — must NOT surface from neighbors /
        // related_memory_ids. Easiest way: add then close.
        lance
            .sync_memory_edges(
                &[GraphEdge {
                    from_node_id: "memory:m_closed".into(),
                    to_node_id: "entity:e1".into(),
                    relation: "mentions".into(),
                    valid_from: "00000001778000000000".into(),
                    valid_to: None,
                }],
                "00000001778000010000",
            )
            .await
            .unwrap();
        lance.close_edges_for_memory("m_closed").await.unwrap();

        // ── seed entities + aliases via the writer ──────────────
        let id_rust = lance
            .resolve_or_create(
                "tenant-a",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000000",
            )
            .await
            .unwrap();
        let id_duck = lance
            .resolve_or_create(
                "tenant-a",
                "DuckDB",
                EntityKind::Project,
                "00000001778000000010",
            )
            .await
            .unwrap();
        let id_b = lance
            .resolve_or_create(
                "tenant-b",
                "Rust Async",
                EntityKind::Topic,
                "00000001778000000005",
            )
            .await
            .unwrap();
        // Add a second alias on id_rust.
        lance
            .add_alias("tenant-a", &id_rust, "Tokio", "00000001778000000020")
            .await
            .unwrap();

        let q = DuckDbQuery::open(&path).await.unwrap();

        // ── graph: neighbors ────────────────────────────────────
        // entity:e1 has 2 active neighbors (m1, m2 via 'mentions');
        // m_closed's edge is closed → excluded. Order: relation,
        // from, to → 'mentions'/m1, 'mentions'/m2.
        let n_e1 = q.neighbors("entity:e1").await.unwrap();
        assert_eq!(n_e1.len(), 2);
        assert_eq!(n_e1[0].from_node_id, "memory:m1");
        assert_eq!(n_e1[1].from_node_id, "memory:m2");

        // memory:m1 has 2 active outgoing edges.
        let n_m1 = q.neighbors("memory:m1").await.unwrap();
        assert_eq!(n_m1.len(), 2);

        // No-neighbor node returns empty (not error).
        let n_none = q.neighbors("entity:nonexistent").await.unwrap();
        assert!(n_none.is_empty());

        // ── graph: related_memory_ids ───────────────────────────
        // Empty input → empty Vec.
        let r_empty = q.related_memory_ids(&[]).await.unwrap();
        assert!(r_empty.is_empty());

        // Seeds [e1, e2] → reachable memories: m1 (via e1+e2), m2
        // (via e1). Output sorted; dedupe by HashSet.
        let r = q
            .related_memory_ids(&["entity:e1".into(), "entity:e2".into()])
            .await
            .unwrap();
        assert_eq!(r, vec!["m1".to_string(), "m2".to_string()]);

        // ── entity: lookup_alias ────────────────────────────────
        // Caller-verbatim casing/whitespace collapses to the same
        // normalized form as the seed.
        let look = q.lookup_alias("tenant-a", "rust async").await.unwrap();
        assert_eq!(look.as_deref(), Some(id_rust.as_str()));
        let look_ws = q
            .lookup_alias("tenant-a", "  RUST   ASYNC  ")
            .await
            .unwrap();
        assert_eq!(look_ws.as_deref(), Some(id_rust.as_str()));
        let look_other = q.lookup_alias("tenant-a", "Tokio").await.unwrap();
        assert_eq!(look_other.as_deref(), Some(id_rust.as_str()));
        let miss = q.lookup_alias("tenant-a", "unknown").await.unwrap();
        assert!(miss.is_none());

        // ── entity: get_entity ──────────────────────────────────
        let with_aliases = q
            .get_entity("tenant-a", &id_rust)
            .await
            .unwrap()
            .expect("rust entity exists");
        assert_eq!(with_aliases.entity.canonical_name, "Rust Async");
        assert_eq!(with_aliases.entity.kind, EntityKind::Topic);
        // Aliases ordered by created_at ASC: 'rust async' (added at
        // resolve_or_create time, earlier ts) then 'tokio'.
        assert_eq!(
            with_aliases.aliases,
            vec!["rust async".to_string(), "tokio".to_string()]
        );

        let none = q.get_entity("tenant-a", "does-not-exist").await.unwrap();
        assert!(none.is_none());

        // ── entity: list_entities ───────────────────────────────
        // tenant-a has 2 entities, ordered created_at DESC: id_duck
        // (later ts) → id_rust.
        let all_a = q.list_entities("tenant-a", None, None, 10).await.unwrap();
        assert_eq!(all_a.len(), 2);
        assert_eq!(all_a[0].entity_id, id_duck);
        assert_eq!(all_a[1].entity_id, id_rust);

        // kind filter.
        let topics = q
            .list_entities("tenant-a", Some(EntityKind::Topic), None, 10)
            .await
            .unwrap();
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].entity_id, id_rust);

        // LIKE filter on canonical_name (case-sensitive, mirrors
        // legacy backend).
        let like = q
            .list_entities("tenant-a", None, Some("Rust"), 10)
            .await
            .unwrap();
        assert_eq!(like.len(), 1);
        assert_eq!(like[0].canonical_name, "Rust Async");

        // tenant-b has only id_b (cross-tenant duplicate alias →
        // distinct entity).
        let all_b = q.list_entities("tenant-b", None, None, 10).await.unwrap();
        assert_eq!(all_b.len(), 1);
        assert_eq!(all_b[0].entity_id, id_b);
    }
}
