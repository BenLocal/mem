//! Memory reads (`memories` table) — list, filter, lookup, BM25,
//! semantic vector search, and version-chain walk. All inherent on
//! `DuckDbQuery`.

use duckdb::{params, OptionalExt};

use super::{enum_to_text, get_string_list, parse_enum, spawn_blocking_storage, DuckDbQuery};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleVersionLink,
};
use crate::storage::types::StorageError;

/// 27-column projection shared by every memory-row read method.
/// Order must match `row_to_capability_capsule_record` below — keep in sync.
const CAPABILITY_CAPSULE_COLS: &str =
    "capability_capsule_id, tenant, capability_capsule_type, status, scope, visibility, version, \
    summary, content, evidence, code_refs, project, repo, module, task_type, \
    tags, topics, confidence, decay_score, content_hash, idempotency_key, \
    session_id, supersedes_capability_capsule_id, source_agent, created_at, updated_at, \
    last_validated_at";

/// Parse one row of the 27-column SELECT above into a [`CapabilityCapsuleRecord`].
///
/// Type expectations (Lance Arrow → DuckDB SQL via the lance extension):
///   - `Utf8` → `VARCHAR` → `String` / `Option<String>`
///   - `List<Utf8>` → `VARCHAR[]` → `Vec<String>`
///   - `UInt64` → `UBIGINT` → `u64`
///   - `Float32` → `FLOAT` (a.k.a. `REAL`) → `f32`
///
/// Enum fields (`capability_capsule_type`, `status`, `scope`, `visibility`) live as
/// snake_case Utf8 strings on the Lance side per LanceStore's schema;
/// here we round-trip them through `serde_json::Value::String` so
/// `#[serde(rename_all = "snake_case")]` on the enum lookups them
/// without needing a hand-written from-str table.
fn row_to_capability_capsule_record(
    row: &duckdb::Row<'_>,
) -> duckdb::Result<CapabilityCapsuleRecord> {
    Ok(CapabilityCapsuleRecord {
        capability_capsule_id: row.get(0)?,
        tenant: row.get(1)?,
        capability_capsule_type: parse_enum(&row.get::<_, String>(2)?)?,
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
        supersedes_capability_capsule_id: row.get(22)?,
        source_agent: row.get(23)?,
        created_at: row.get(24)?,
        updated_at: row.get(25)?,
        last_validated_at: row.get(26)?,
    })
}

/// Collect rows from a `query_map` iterator into a `Vec<CapabilityCapsuleRecord>`,
/// converting the per-row `duckdb::Error` to `StorageError`.
fn collect_capability_capsules<I>(rows: I) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>
where
    I: Iterator<Item = duckdb::Result<CapabilityCapsuleRecord>>,
{
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(StorageError::DuckDb)?);
    }
    Ok(out)
}

impl DuckDbQuery {
    pub async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules WHERE tenant = ?1",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant], row_to_capability_capsule_record)?;
            collect_capability_capsules(rows)
        })
        .await
    }

    /// Single memory by `(tenant, capability_capsule_id)`. Returns `Ok(None)` when
    /// no row matches (the canonical "not found" path — distinct from
    /// errors).
    pub async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let capability_capsule_id = capability_capsule_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND capability_capsule_id = ?2",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, capability_capsule_id],
                row_to_capability_capsule_record,
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Single pending-confirmation memory by `(tenant, capability_capsule_id)`.
    /// Used by the review-queue UI's edit/inspect flow — surfaces
    /// `Ok(None)` if the row is gone or has already been
    /// accepted/rejected (status moved off `pending_confirmation`).
    pub async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let capability_capsule_id = capability_capsule_id.to_string();
        let status = enum_to_text(&CapabilityCapsuleStatus::PendingConfirmation)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND capability_capsule_id = ?2 AND status = ?3",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, capability_capsule_id, status],
                row_to_capability_capsule_record,
            )
            .optional()
            .map_err(StorageError::DuckDb)
        })
        .await
    }

    /// Idempotency check used by `CapabilityCapsuleService::ingest`. Matches on
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
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let idempotency_key = idempotency_key.clone();
        let content_hash = content_hash.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules
                 WHERE tenant = ?1
                   AND ((?2 IS NOT NULL AND idempotency_key = ?2) OR content_hash = ?3)
                 ORDER BY
                    CASE WHEN ?2 IS NOT NULL AND idempotency_key = ?2 THEN 0 ELSE 1 END,
                    updated_at DESC
                 LIMIT 1",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            conn.query_row(
                &sql,
                params![tenant, idempotency_key.as_deref(), content_hash],
                row_to_capability_capsule_record,
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
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let status = enum_to_text(&CapabilityCapsuleStatus::PendingConfirmation)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND status = ?2 \
                 ORDER BY created_at DESC",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![tenant, status], row_to_capability_capsule_record)?;
            collect_capability_capsules(rows)
        })
        .await
    }

    /// Most-recent non-rejected, non-archived memories under `tenant`
    /// — the empty-query fallback for `mem wake-up`. Ordered
    /// `(updated_at DESC, version DESC, capability_capsule_id ASC)` to keep ties
    /// deterministic when a batch of rows shares an `updated_at`
    /// timestamp.
    ///
    /// `limit` is clamped to `[1, 1024]` (mirrors the legacy bound).
    pub async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND status NOT IN (?2, ?3) \
                 ORDER BY updated_at DESC, version DESC, capability_capsule_id ASC \
                 LIMIT ?4",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![tenant, rejected, archived, lim],
                row_to_capability_capsule_record,
            )?;
            collect_capability_capsules(rows)
        })
        .await
    }

    /// Candidate pool for the ranking pipeline. Same row shape /
    /// ordering as `recent_active_capability_capsules` but **unbounded** — pulls
    /// the entire live (non-rejected, non-archived) set for `tenant`
    /// and returns it. Used by `pipeline::retrieve` to score every
    /// candidate; service code is expected to top-N afterward.
    ///
    /// For tenants with thousands of memories the wake-up fast path
    /// uses `recent_active_capability_capsules` instead — same filter, push the
    /// LIMIT to SQL.
    ///
    /// We do the status filter in SQL (rather than the legacy "fetch
    /// all then filter in Rust") because pushing predicates is the
    /// whole point of having DuckDB on top — every byte of an archived
    /// row that doesn't make it into Rust is a win.
    pub async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND status NOT IN (?2, ?3) \
                 ORDER BY updated_at DESC, version DESC, capability_capsule_id ASC",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![tenant, rejected, archived],
                row_to_capability_capsule_record,
            )?;
            collect_capability_capsules(rows)
        })
        .await
    }

    /// Bulk fetch by capability_capsule_id list, scoped to `tenant`. Uses
    /// `WHERE capability_capsule_id IN (?, ?, ...)` with N parameter binders.
    /// Returns rows in DB-natural order, **not** in input slice order;
    /// callers that need to preserve `ids` ordering reshape via a
    /// HashMap (the legacy hybrid-search path does this).
    ///
    /// Empty `ids` short-circuits to `Ok(vec![])` without touching the
    /// connection.
    ///
    /// Used by post-search hydration in `pipeline::retrieve`: ANN /
    /// BM25 returns capability_capsule_ids only; this fills the row data.
    pub async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
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
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND capability_capsule_id IN ({placeholders})",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant)];
            for id in ids {
                params_vec.push(Box::new(id));
            }
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_capability_capsule_record)?;
            collect_capability_capsules(rows)
        })
        .await
    }

    /// Project just `capability_capsule_id` column for `tenant`, ordered
    /// `updated_at DESC`. Cheap admin / repair operation that doesn't
    /// need to hydrate the full row.
    pub async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT capability_capsule_id FROM ns.main.capability_capsules \
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
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if query.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let query = query.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        let oversample = k_i.saturating_mul(2);
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} \
                 FROM lance_fts('ns.main.capability_capsules', 'content', ?1, k => ?2) \
                 WHERE tenant = ?3 AND status NOT IN (?4, ?5) \
                 ORDER BY _score DESC \
                 LIMIT ?6",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![query, oversample, tenant, archived, rejected, k_i],
                row_to_capability_capsule_record,
            )?;
            collect_capability_capsules(rows)
        })
        .await
    }

    /// Semantic recall via ANN over the `capability_capsule_embeddings` table's
    /// `embedding` column. Routes through the lance extension's
    /// `lance_vector_search(...)` SQL table function; joins back to
    /// `ns.main.capability_capsules` to hydrate the full `CapabilityCapsuleRecord`. Returns
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
    pub async fn semantic_search_capability_capsules(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
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
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // SELECT both the 27 memory cols (m.<col>) and _distance.
            // Done explicitly rather than via `m.*` so column ordering
            // stays in lock-step with `row_to_capability_capsule_record`'s indices.
            let m_cols = CAPABILITY_CAPSULE_COLS
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
                        'ns.main.capability_capsule_embeddings', 'embedding', {vector_lit}, k => ?1 \
                      ) AS e \
                 JOIN ns.main.capability_capsules AS m ON m.capability_capsule_id = e.capability_capsule_id \
                 WHERE m.tenant = ?2 AND m.status NOT IN (?3, ?4) \
                 ORDER BY e._distance ASC \
                 LIMIT ?5",
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![oversample, tenant, archived, rejected, lim],
                |row| {
                    let mem = row_to_capability_capsule_record(row)?;
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

    /// Cross-table hybrid recall: BM25 over `capability_capsules.content`
    /// joined with ANN over `capability_capsule_embeddings.embedding`,
    /// fused with Reciprocal Rank Fusion (RRF, k=60) inline in DuckDB
    /// SQL. Returns `(record, rrf_score)` ordered by RRF score DESC.
    ///
    /// Validated by `examples/hybrid_sql_poc.rs`. Replaces the dual
    /// fan-out (`bm25_candidates` + `semantic_search_capability_capsules`)
    /// plus the manual Rust-side RRF in `pipeline::retrieve` with a
    /// single SQL call.
    ///
    /// **Three query shapes** (driven by which inputs are non-empty):
    ///   - `query_text` non-empty + `query_embedding` non-empty → hybrid
    ///   - `query_text` non-empty + embedding empty → BM25-only
    ///   - `query_text` empty + embedding non-empty → vector-only
    ///   - both empty → returns `Vec::new()` (no signal to rank on)
    ///
    /// **RRF**: `score = COALESCE(1/(60+rank_lex), 0) + COALESCE(1/(60+rank_sem), 0)`.
    /// Items in only one source still get their partial score via
    /// `FULL OUTER JOIN`; items in both rank highest by construction.
    ///
    /// **Inner-K oversample**: each lance table function gets `k * 2` so
    /// the outer tenant + status filter doesn't truncate the result
    /// below `k`. Same posture as `bm25_candidates`.
    ///
    /// **Tenant filter pushdown**: applied to both inner CTEs (the
    /// embeddings table also carries `tenant` for early filtering) and
    /// re-applied in the outer hydration JOIN to cover vec-only items.
    ///
    /// **Status exclusion**: rows with `status IN ('rejected',
    /// 'archived')` are excluded from the FTS CTE and from the outer
    /// hydration. Vec-only matches that point at archived/rejected rows
    /// drop in the outer JOIN's WHERE.
    pub async fn hybrid_candidates(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        let has_text = !query_text.trim().is_empty();
        let has_vec = !query_embedding.is_empty();
        if (!has_text && !has_vec) || k == 0 {
            return Ok(Vec::new());
        }

        let m_cols = CAPABILITY_CAPSULE_COLS
            .split(',')
            .map(|c| format!("m.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ");

        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        let query_text = query_text.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        let oversample = k_i.saturating_mul(2);
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;

        let vector_lit = if has_vec {
            format!(
                "[{}]::FLOAT[]",
                query_embedding
                    .iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        } else {
            String::new()
        };

        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");

            // Build the SQL + params in lockstep so DuckDB's strict
            // bind-count check (excess binds reject the prepare with
            // InvalidParameterCount) doesn't fire. Each shape numbers
            // placeholders contiguously.
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = Vec::with_capacity(7);
            let mut next_idx = 0_usize;
            let mut bind = |v: Box<dyn duckdb::ToSql>| -> usize {
                params_vec.push(v);
                next_idx += 1;
                next_idx
            };

            let fts_cte = if has_text {
                let p1 = bind(Box::new(query_text.clone()));
                let p2 = bind(Box::new(oversample));
                let p3 = bind(Box::new(tenant.clone()));
                let p4 = bind(Box::new(archived.clone()));
                let p5 = bind(Box::new(rejected.clone()));
                format!(
                    "fts AS ( \
                        SELECT capability_capsule_id, \
                               ROW_NUMBER() OVER (ORDER BY _score DESC, capability_capsule_id ASC) AS rank_lex \
                        FROM lance_fts('ns.main.capability_capsules', 'content', ?{p1}, k => ?{p2}) \
                        WHERE tenant = ?{p3} AND status NOT IN (?{p4}, ?{p5}) \
                    )"
                )
            } else {
                "fts AS ( \
                    SELECT NULL::VARCHAR AS capability_capsule_id, \
                           NULL::BIGINT  AS rank_lex \
                    WHERE FALSE \
                )"
                    .to_string()
            };

            let vec_cte = if has_vec {
                // Vector literal is interpolated into SQL (duckdb-rs
                // has no FLOAT[] bind path); tenant is rebound here
                // since the FTS branch may or may not have bound it.
                // Rebind tenant — duckdb-rs has no positional reuse,
                // and CTE branches that don't run cost nothing.
                let p_tenant_v = bind(Box::new(tenant.clone()));
                let p_over = bind(Box::new(oversample));
                format!(
                    "vec AS ( \
                        SELECT e.capability_capsule_id, \
                               ROW_NUMBER() OVER (ORDER BY e._distance ASC, e.capability_capsule_id ASC) AS rank_sem \
                        FROM lance_vector_search( \
                                'ns.main.capability_capsule_embeddings', 'embedding', \
                                {vector_lit}, k => ?{p_over} \
                              ) AS e \
                        WHERE e.tenant = ?{p_tenant_v} \
                    )"
                )
            } else {
                "vec AS ( \
                    SELECT NULL::VARCHAR AS capability_capsule_id, \
                           NULL::BIGINT  AS rank_sem \
                    WHERE FALSE \
                )"
                    .to_string()
            };

            // Outer hydration filter — always rebound, regardless of
            // which inner CTE produced the row.
            let p_outer_tenant = bind(Box::new(tenant.clone()));
            let p_outer_arch = bind(Box::new(archived.clone()));
            let p_outer_rej = bind(Box::new(rejected.clone()));
            let p_outer_lim = bind(Box::new(k_i));

            let sql = format!(
                "WITH \
                 {fts_cte}, \
                 {vec_cte}, \
                 fused AS ( \
                     SELECT \
                         COALESCE(fts.capability_capsule_id, vec.capability_capsule_id) AS capability_capsule_id, \
                           COALESCE(1.0 / (60.0 + fts.rank_lex), 0.0) \
                         + COALESCE(1.0 / (60.0 + vec.rank_sem), 0.0) AS rrf_score \
                     FROM fts FULL OUTER JOIN vec USING (capability_capsule_id) \
                 ) \
                 SELECT {m_cols}, CAST(f.rrf_score AS FLOAT) AS rrf_score \
                 FROM fused f \
                 JOIN ns.main.capability_capsules m USING (capability_capsule_id) \
                 WHERE m.tenant = ?{p_outer_tenant} \
                   AND m.status NOT IN (?{p_outer_arch}, ?{p_outer_rej}) \
                 ORDER BY f.rrf_score DESC, m.updated_at DESC, m.capability_capsule_id ASC \
                 LIMIT ?{p_outer_lim}"
            );

            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params_refs.as_slice(), |row| {
                let mem = row_to_capability_capsule_record(row)?;
                let rrf: f32 = row.get(27)?;
                Ok((mem, rrf))
            })?;
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
    /// `capability_capsule_id` parameter but ignores it — the SQL only filters by
    /// tenant. Service-layer callers (`get_memory_detail`) expect the
    /// returned chain to be tenant-scoped *and* memory-scoped, so
    /// they're getting a too-broad answer today. Mirroring the broken
    /// behavior here for cutover parity; will fix with a proper
    /// version-chain walk (`WHERE capability_capsule_id = ?2 OR
    /// supersedes_capability_capsule_id = ?2`, recursive) in a follow-up.
    pub async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        let _ = capability_capsule_id;
        let conn = self.conn.clone();
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT capability_capsule_id, version, status, updated_at, supersedes_capability_capsule_id \
                 FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 \
                 ORDER BY version DESC, updated_at DESC",
            )?;
            let rows = stmt.query_map(params![tenant], |row| {
                Ok(CapabilityCapsuleVersionLink {
                    capability_capsule_id: row.get(0)?,
                    version: row.get::<_, u64>(1)?,
                    status: parse_enum(&row.get::<_, String>(2)?)?,
                    updated_at: row.get(3)?,
                    supersedes_capability_capsule_id: row.get(4)?,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use crate::storage::lance_store::LanceStore;
    use tempfile::tempdir;

    fn fixture(capability_capsule_id: &str, tenant: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: capability_capsule_id.into(),
            tenant: tenant.into(),
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            status: CapabilityCapsuleStatus::Active,
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
            supersedes_capability_capsule_id: None,
            source_agent: "test".into(),
            created_at: "00000001778000000000".into(),
            updated_at: "00000001778000000000".into(),
            last_validated_at: None,
        }
    }

    ///   - Tenant filter scopes correctly.
    #[tokio::test(flavor = "multi_thread")]
    async fn lance_write_then_duckdb_read_memories() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");

        // 1. Create + populate Lance dataset via the writer.
        let lance = LanceStore::open(&path).await.expect("LanceStore::open");
        lance
            .insert_capability_capsule(fixture("m1", "tenant-a"))
            .await
            .expect("insert m1");
        lance
            .insert_capability_capsule(fixture("m2", "tenant-a"))
            .await
            .expect("insert m2");
        lance
            .insert_capability_capsule(fixture("m3", "tenant-b"))
            .await
            .expect("insert m3");

        // 2. Open DuckDB query layer on the same path.
        let q = DuckDbQuery::open(&path).await.expect("DuckDbQuery::open");

        // 3. Read back through SQL. tenant-a → 2 rows; tenant-b → 1 row.
        let mut a = q
            .list_capability_capsules_for_tenant("tenant-a")
            .await
            .expect("list tenant-a");
        a.sort_by(|x, y| x.capability_capsule_id.cmp(&y.capability_capsule_id));
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].capability_capsule_id, "m1");
        assert_eq!(a[1].capability_capsule_id, "m2");
        // Spot-check rich types preserved through the SQL boundary.
        assert_eq!(a[0].evidence, vec!["src/main.rs:42", "Cargo.toml:11"]);
        assert_eq!(a[0].code_refs, vec!["foo::bar()"]);
        assert_eq!(a[0].tags, vec!["tooling"]);
        assert_eq!(a[0].topics, vec!["bun"]);
        assert_eq!(a[0].version, 1u64);
        assert!((a[0].confidence - 0.7).abs() < 1e-6);
        assert_eq!(a[0].project.as_deref(), Some("mem"));
        assert!(a[0].module.is_none());
        assert_eq!(a[0].status, CapabilityCapsuleStatus::Active);
        assert_eq!(
            a[0].capability_capsule_type,
            CapabilityCapsuleType::Implementation
        );

        let b = q
            .list_capability_capsules_for_tenant("tenant-b")
            .await
            .expect("list tenant-b");
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].capability_capsule_id, "m3");

        // Tenant that has no rows returns empty (not an error).
        let none = q
            .list_capability_capsules_for_tenant("does-not-exist")
            .await
            .expect("list missing tenant");
        assert!(none.is_empty());
    }

    /// Exercises the 4 single-row / filtered-list methods that build
    /// on the same SELECT prefix as `list_capability_capsules_for_tenant`:
    /// `get_capability_capsule_for_tenant`, `get_pending`,
    /// `find_by_idempotency_or_hash`, `list_pending_review`,
    /// `recent_active_capability_capsules`.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_memory_filters() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Seed: 1 active, 1 pending, 1 archived (excluded from
        // recent_active_capability_capsules), 1 rejected (also excluded), 1 in
        // tenant-b (cross-tenant exclusion).
        let mut active = fixture("m_active", "tenant-a");
        active.idempotency_key = Some("idemp-active".into());
        active.content_hash = "hash-active".into();
        active.updated_at = "00000001778000000020".into();
        let mut pending = fixture("m_pending", "tenant-a");
        pending.status = CapabilityCapsuleStatus::PendingConfirmation;
        pending.idempotency_key = Some("idemp-pending".into());
        pending.content_hash = "hash-pending".into();
        pending.updated_at = "00000001778000000010".into();
        let mut archived = fixture("m_archived", "tenant-a");
        archived.status = CapabilityCapsuleStatus::Archived;
        archived.updated_at = "00000001778000000005".into();
        let mut rejected = fixture("m_rejected", "tenant-a");
        rejected.status = CapabilityCapsuleStatus::Rejected;
        rejected.updated_at = "00000001778000000006".into();
        let other_tenant = fixture("m_other", "tenant-b");

        for m in [&active, &pending, &archived, &rejected, &other_tenant] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // get_capability_capsule_for_tenant — hit + miss + cross-tenant.
        let hit = q
            .get_capability_capsule_for_tenant("tenant-a", "m_active")
            .await
            .unwrap()
            .expect("active memory should exist");
        assert_eq!(hit.capability_capsule_id, "m_active");
        assert_eq!(hit.status, CapabilityCapsuleStatus::Active);
        let miss = q
            .get_capability_capsule_for_tenant("tenant-a", "does-not-exist")
            .await
            .unwrap();
        assert!(miss.is_none());
        let cross = q
            .get_capability_capsule_for_tenant("tenant-b", "m_active")
            .await
            .unwrap();
        assert!(cross.is_none(), "tenant filter must scope cross-tenant");

        // get_pending — only pending status surfaces.
        let pend = q
            .get_pending("tenant-a", "m_pending")
            .await
            .unwrap()
            .expect("pending row");
        assert_eq!(pend.capability_capsule_id, "m_pending");
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
        assert_eq!(by_idemp.capability_capsule_id, "m_active");
        let by_hash_only = q
            .find_by_idempotency_or_hash("tenant-a", &None, "hash-pending")
            .await
            .unwrap()
            .expect("hash match");
        assert_eq!(by_hash_only.capability_capsule_id, "m_pending");
        let by_miss = q
            .find_by_idempotency_or_hash("tenant-a", &None, "no-such-hash")
            .await
            .unwrap();
        assert!(by_miss.is_none());

        // list_pending_review — only pending_confirmation.
        let pending_list = q.list_pending_review("tenant-a").await.unwrap();
        assert_eq!(pending_list.len(), 1);
        assert_eq!(pending_list[0].capability_capsule_id, "m_pending");
        let other = q.list_pending_review("tenant-b").await.unwrap();
        assert!(other.is_empty(), "no pending in tenant-b");

        // recent_active_capability_capsules — pending + active stay; archived +
        // rejected drop. Cross-tenant excluded.
        let recent = q
            .recent_active_capability_capsules("tenant-a", 50)
            .await
            .unwrap();
        let recent_ids: Vec<&str> = recent
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            recent_ids,
            vec!["m_active", "m_pending"],
            "ordered by updated_at DESC; archived/rejected excluded"
        );
        let recent_b = q
            .recent_active_capability_capsules("tenant-b", 50)
            .await
            .unwrap();
        assert_eq!(recent_b.len(), 1);
        assert_eq!(recent_b[0].capability_capsule_id, "m_other");

        // limit clamps to >=1 even when caller passes 0 (mirrors the
        // legacy DuckDB-as-storage clamp).
        let recent_clamped = q
            .recent_active_capability_capsules("tenant-a", 0)
            .await
            .unwrap();
        assert_eq!(recent_clamped.len(), 1);
    }

    /// Cluster A round-trip: `search_candidates`,
    /// `fetch_capability_capsules_by_ids`, `list_capability_capsule_ids_for_tenant`,
    /// `list_capability_capsule_versions_for_tenant`. All four operate on the
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
        arc.status = CapabilityCapsuleStatus::Archived;
        arc.updated_at = "00000001778000000030".into();
        let mut bv2 = fixture("m_b_v2", "tenant-a");
        bv2.supersedes_capability_capsule_id = Some("m_b".into());
        bv2.version = 2;
        bv2.updated_at = "00000001778000000060".into();
        let mut other = fixture("m_other", "tenant-b");
        other.updated_at = "00000001778000000020".into();
        for m in [&a, &b, &arc, &bv2, &other] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // search_candidates: archived/rejected excluded; tenant-scoped;
        // ordered (updated_at DESC, version DESC, capability_capsule_id ASC).
        let cands = q.search_candidates("tenant-a").await.unwrap();
        let cand_ids: Vec<&str> = cands
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            cand_ids,
            vec!["m_b_v2", "m_a", "m_b"],
            "tenant-a candidates: archived excluded, ordered by updated_at DESC"
        );
        let cands_b = q.search_candidates("tenant-b").await.unwrap();
        assert_eq!(cands_b.len(), 1);

        // fetch_capability_capsules_by_ids: in-clause batch lookup. Empty → empty.
        let empty = q
            .fetch_capability_capsules_by_ids("tenant-a", &[])
            .await
            .unwrap();
        assert!(empty.is_empty());

        let some = q
            .fetch_capability_capsules_by_ids("tenant-a", &["m_a", "m_b", "does-not-exist"])
            .await
            .unwrap();
        let some_ids: std::collections::HashSet<&str> = some
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(some.len(), 2);
        assert!(some_ids.contains("m_a"));
        assert!(some_ids.contains("m_b"));

        // tenant filter scopes the IN-clause: same id under different
        // tenant returns nothing.
        let cross = q
            .fetch_capability_capsules_by_ids("tenant-b", &["m_a"])
            .await
            .unwrap();
        assert!(
            cross.is_empty(),
            "tenant-a id must not appear in tenant-b lookup"
        );

        // list_capability_capsule_ids_for_tenant: just IDs, ordered updated_at DESC.
        let ids_a = q
            .list_capability_capsule_ids_for_tenant("tenant-a")
            .await
            .unwrap();
        assert_eq!(
            ids_a,
            vec!["m_b_v2", "m_a", "m_b", "m_arc"],
            "all 4 tenant-a rows incl. archived; updated_at DESC"
        );
        let ids_empty = q
            .list_capability_capsule_ids_for_tenant("does-not-exist")
            .await
            .unwrap();
        assert!(ids_empty.is_empty());

        // list_capability_capsule_versions_for_tenant: ordered (version DESC,
        // updated_at DESC). NOTE: passes capability_capsule_id but the legacy
        // implementation ignores it; we mirror that here so behavior
        // stays parity until a follow-up fixes the version-chain
        // walk.
        let chain = q
            .list_capability_capsule_versions_for_tenant("tenant-a", "m_b")
            .await
            .unwrap();
        // Returns ALL tenant-a rows' version links — m_b_v2 (v=2) +
        // m_a (v=2) + m_b (v=1) + m_arc (v=1, fixture default).
        assert_eq!(chain.len(), 4);
        // The supersedes link is preserved.
        let bv2_link = chain
            .iter()
            .find(|l| l.capability_capsule_id == "m_b_v2")
            .expect("m_b_v2 in chain");
        assert_eq!(
            bv2_link.supersedes_capability_capsule_id.as_deref(),
            Some("m_b")
        );
        let b_link = chain
            .iter()
            .find(|l| l.capability_capsule_id == "m_b")
            .expect("m_b in chain");
        assert!(b_link.supersedes_capability_capsule_id.is_none());
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
        c.status = CapabilityCapsuleStatus::Archived;
        c.content = "Tantivy provides BM25 in DuckDB build".into();
        let mut d = fixture("m_other", "tenant-b");
        d.content = "DuckDB connection pool tenant-b".into();
        for m in [&a, &b, &c, &d] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
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
        let ids: Vec<&str> = hits
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
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
        let b_ids: Vec<&str> = b_hits
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert!(b_ids.contains(&"m_other"));

        // Different query word.
        let lance_hits = q.bm25_candidates("tenant-a", "LanceDB", 10).await.unwrap();
        let lance_ids: Vec<&str> = lance_hits
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert!(lance_ids.contains(&"m_lance"), "got {lance_ids:?}");
    }

    /// `semantic_search_capability_capsules` over `lance_vector_search(...)` with
    /// JOIN to memories. Inserts 3 memories with hand-rolled 4-d unit
    /// vectors via `upsert_capability_capsule_embedding`, then queries with a
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
        m3.status = CapabilityCapsuleStatus::Archived;
        let m4 = fixture("m_v4", "tenant-b");
        for m in [&m1, &m2, &m3, &m4] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
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
                .upsert_capability_capsule_embedding(
                    id,
                    tenant,
                    "fake-test",
                    4,
                    &to_blob(vec),
                    hash,
                    now,
                    now,
                )
                .await
                .unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // Query close to v1: m_v1 ranks first; m_v2 also returns;
        // m_v3 archived → excluded; m_v4 cross-tenant → excluded.
        let query = vec![0.99_f32, 0.14, 0.0, 0.0];
        let hits = q
            .semantic_search_capability_capsules("tenant-a", &query, 10)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "tenant-a active memories with embeddings → 2 (m_v1, m_v2); got {hits:?}"
        );
        assert_eq!(
            hits[0].0.capability_capsule_id, "m_v1",
            "v1 ranks first (closest)"
        );
        assert_eq!(hits[1].0.capability_capsule_id, "m_v2");
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
            .semantic_search_capability_capsules("tenant-a", &[], 10)
            .await
            .unwrap();
        assert!(empty1.is_empty());
        let empty2 = q
            .semantic_search_capability_capsules("tenant-a", &query, 0)
            .await
            .unwrap();
        assert!(empty2.is_empty());

        // tenant-b sees its own row.
        let b_hits = q
            .semantic_search_capability_capsules("tenant-b", &query, 10)
            .await
            .unwrap();
        assert_eq!(b_hits.len(), 1);
        assert_eq!(b_hits[0].0.capability_capsule_id, "m_v4");
    }

    /// Cross-table SQL hybrid: lance_fts + lance_vector_search joined
    /// on capability_capsule_id, RRF fused inline. Mirrors the assertions
    /// in `examples/hybrid_sql_poc.rs` against real fixture data.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_hybrid_candidates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // 4 capsules; only h_dual and h_lex have BM25-relevant content
        // for the query "ANN HNSW retrieval".
        let mut h_dual = fixture("h_dual", "tenant-a");
        h_dual.content = "ANN retrieval with HNSW and inverted lists".into();
        let mut h_lex = fixture("h_lex", "tenant-a");
        h_lex.content = "Lance datasets support ANN via HNSW".into();
        let mut h_vec = fixture("h_vec", "tenant-a");
        h_vec.content = "DuckDB stores canonical capsule records and indexes".into();
        let mut h_other = fixture("h_other", "tenant-b");
        h_other.content = "Cross-tenant noise that mentions ANN HNSW".into();

        for m in [&h_dual, &h_lex, &h_vec, &h_other] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
        }

        // 4-d unit vectors. Query ≈ [0.99, 0.14, 0, 0] — closest to
        // h_dual then h_vec; h_lex orthogonal-ish, h_other cross-tenant.
        fn to_blob(v: &[f32]) -> Vec<u8> {
            let mut out = Vec::with_capacity(v.len() * 4);
            for f in v {
                out.extend_from_slice(&f.to_ne_bytes());
            }
            out
        }
        let v_dual = vec![1.0_f32, 0.0, 0.0, 0.0];
        let v_lex = vec![0.0_f32, 1.0, 0.0, 0.0];
        let v_vec = vec![0.99_f32, 0.14, 0.0, 0.0];
        let v_other = vec![0.5_f32, 0.5, 0.0, 0.0];
        let now = "00000001778000000000";
        for (id, tenant, vec, hash) in [
            ("h_dual", "tenant-a", &v_dual, "h-dual"),
            ("h_lex", "tenant-a", &v_lex, "h-lex"),
            ("h_vec", "tenant-a", &v_vec, "h-vec"),
            ("h_other", "tenant-b", &v_other, "h-other"),
        ] {
            lance
                .upsert_capability_capsule_embedding(
                    id,
                    tenant,
                    "fake-test",
                    4,
                    &to_blob(vec),
                    hash,
                    now,
                    now,
                )
                .await
                .unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // Hybrid: text + vector both supplied. h_dual hits both → top.
        let query_vec = vec![0.99_f32, 0.14, 0.0, 0.0];
        let hits = q
            .hybrid_candidates("tenant-a", "ANN HNSW retrieval", &query_vec, 10)
            .await
            .unwrap();
        assert!(!hits.is_empty(), "hybrid should return at least one row");
        assert_eq!(
            hits[0].0.capability_capsule_id, "h_dual",
            "h_dual (best of both lex+sem) should rank first; got {hits:?}"
        );
        assert!(
            !hits
                .iter()
                .any(|(m, _)| m.capability_capsule_id == "h_other"),
            "cross-tenant h_other must be excluded"
        );
        // RRF scores are positive and descending.
        for w in hits.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "rrf scores not descending: {} < {}",
                w[0].1,
                w[1].1
            );
        }

        // FTS-only: empty embedding.
        let lex_only = q
            .hybrid_candidates("tenant-a", "ANN HNSW retrieval", &[], 10)
            .await
            .unwrap();
        let lex_ids: Vec<_> = lex_only
            .iter()
            .map(|(m, _)| m.capability_capsule_id.as_str())
            .collect();
        assert!(
            lex_ids.contains(&"h_dual") && lex_ids.contains(&"h_lex"),
            "lex-only should include h_dual + h_lex; got {lex_ids:?}"
        );
        assert!(
            !lex_ids.contains(&"h_other"),
            "cross-tenant must stay excluded in lex-only mode"
        );

        // Vec-only: empty text.
        let vec_only = q
            .hybrid_candidates("tenant-a", "", &query_vec, 10)
            .await
            .unwrap();
        assert!(
            !vec_only.is_empty(),
            "vec-only should return at least one row"
        );
        assert!(
            !vec_only
                .iter()
                .any(|(m, _)| m.capability_capsule_id == "h_other"),
            "cross-tenant must stay excluded in vec-only mode"
        );

        // Empty both → empty result.
        let nada = q.hybrid_candidates("tenant-a", "", &[], 10).await.unwrap();
        assert!(nada.is_empty());

        // k = 0 short-circuits.
        let zero = q
            .hybrid_candidates("tenant-a", "ANN HNSW retrieval", &query_vec, 0)
            .await
            .unwrap();
        assert!(zero.is_empty());
    }
}
