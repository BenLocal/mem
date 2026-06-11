//! Memory reads (`memories` table) — list, filter, lookup, BM25,
//! semantic vector search, and version-chain walk. All inherent on
//! `DuckDbQuery`.

use duckdb::{params, OptionalExt};

use super::{enum_to_text, get_string_list, parse_enum, spawn_blocking_storage, DuckDbQuery};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
    CapabilityCapsuleVersionLink, CapsuleStats,
};
use crate::storage::types::StorageError;

/// True if `err` is the Lance "dataset at path … was not found" error
/// for the `capability_capsule_embeddings` table. The table is
/// lazy-created on first upsert (dim is provider-dependent), so a
/// brand-new store hits this on every search until the first ingest
/// lands. `hybrid_candidates` uses this to decide whether to retry as
/// text-only instead of bubbling up a 500.
fn is_capability_capsule_embeddings_missing(err: &duckdb::Error) -> bool {
    let msg = err.to_string();
    msg.contains("Failed to open Lance dataset") && msg.contains("capability_capsule_embeddings")
}

/// 28-column projection shared by every memory-row read method.
/// Order must match `row_to_capability_capsule_record` below — keep in sync.
const CAPABILITY_CAPSULE_COLS: &str =
    "capability_capsule_id, tenant, capability_capsule_type, status, scope, visibility, version, \
    summary, content, evidence, code_refs, project, repo, module, task_type, \
    tags, topics, confidence, decay_score, content_hash, idempotency_key, \
    session_id, supersedes_capability_capsule_id, source_agent, created_at, updated_at, \
    last_validated_at, last_used_at, last_recalled_at, expires_at";

/// Parse one row of the 30-column SELECT above into a [`CapabilityCapsuleRecord`].
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
        version: row.get::<_, i64>(6)?,
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
        last_used_at: row.get(27)?,
        last_recalled_at: row.get(28)?,
        expires_at: row.get(29)?,
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

/// Build the optional `search_candidates` lifecycle-pool cap clause
/// (`MEM_RECALL_POOL_LIMIT`). `None` → empty string → the unbounded full
/// pool (default). `Some(n)` → keep all `preference` / `workflow`
/// guidance plus the `n` most-recently-written other rows. `n` is a
/// parsed `usize`, safe to interpolate; the subquery reuses the outer
/// `?1`/`?2`/`?3` binds (tenant / rejected / archived). Pure for testing.
fn pool_bound_clause(pool_limit: Option<usize>) -> String {
    match pool_limit {
        Some(n) => format!(
            "AND (c.capability_capsule_type IN ('preference', 'workflow') \
                  OR c.capability_capsule_id IN ( \
                      SELECT capability_capsule_id FROM ns.main.capability_capsules \
                      WHERE tenant = ?1 AND status NOT IN (?2, ?3) \
                        AND capability_capsule_type != 'diary' \
                      ORDER BY updated_at DESC LIMIT {n} \
                  )) ",
        ),
        None => String::new(),
    }
}

impl DuckDbQuery {
    pub async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.fresh_conn().await?;
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

    /// Capsule-pool snapshot for `tenant`: total row count + per-status
    /// breakdown. One `GROUP BY status` query; Rust folds the rows into
    /// the discrete fields on `CapsuleStats`. Unknown status strings
    /// (future enum additions reading old data) are silently dropped —
    /// caller can detect via `sum(by_status) != total`.
    pub async fn capsule_stats(&self, tenant: &str) -> Result<CapsuleStats, StorageError> {
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT status, COUNT(*) FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 GROUP BY status",
            )?;
            let rows = stmt.query_map(params![tenant], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            let mut stats = CapsuleStats::default();
            for r in rows {
                let (status, count) = r.map_err(StorageError::DuckDb)?;
                stats.total += count;
                match status.as_str() {
                    "pending_confirmation" => stats.pending_confirmation = count,
                    "provisional" => stats.provisional = count,
                    "active" => stats.active = count,
                    "archived" => stats.archived = count,
                    "rejected" => stats.rejected = count,
                    _ => {}
                }
            }
            Ok(stats)
        })
        .await
    }

    /// Distinct `project` values in `tenant`, ordered alphabetically.
    /// MemPalace's `tool_list_wings` analogue — a navigation hint
    /// for MCP clients building a sidebar / tree. NULL projects are
    /// dropped so the result is always a meaningful name list. Diary
    /// capsules are included since they participate in the project /
    /// repo space too (a "diary about project X" is still a thing
    /// for that project).
    pub async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT DISTINCT project FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND project IS NOT NULL \
                 ORDER BY project",
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

    /// Two-level taxonomy: distinct `(project, repo)` pairs for
    /// `tenant`, grouped under the project. MemPalace's
    /// `tool_get_taxonomy` analogue — gives an MCP client the full
    /// project → repo navigation tree in one round trip. Both project
    /// and repo NULLs are dropped per the same reasoning as
    /// `list_wings`.
    ///
    /// Output shape: `[(project, Vec<repo>)]`, sorted by project then
    /// repo. A project with no recorded repo appears as `(project,
    /// [])`. The flat list is grouped by the caller (service / HTTP
    /// layer) rather than collated in SQL because DuckDB lacks a
    /// clean `GROUP_CONCAT` over distinct elements.
    pub async fn get_taxonomy(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT project, repo FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND project IS NOT NULL \
                 GROUP BY project, repo \
                 ORDER BY project, repo",
            )?;
            let rows = stmt.query_map(params![tenant], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
            for r in rows {
                let (project, repo) = r.map_err(StorageError::DuckDb)?;
                match grouped.last_mut() {
                    Some(last) if last.0 == project => {
                        if let Some(r) = repo {
                            last.1.push(r);
                        }
                    }
                    _ => {
                        let repos = repo.map(|r| vec![r]).unwrap_or_default();
                        grouped.push((project, repos));
                    }
                }
            }
            Ok(grouped)
        })
        .await
    }

    /// Scope-filtered browse path: capsules in `tenant` matching any
    /// subset of `(project, repo, module, capability_capsule_type,
    /// status, source_agent)`, ordered `(updated_at DESC,
    /// capability_capsule_id ASC)`, paginated by the composite cursor
    /// `(updated_at, capability_capsule_id)`. Unlike
    /// `hybrid_candidates` this is **embedding-free** — caller browses
    /// by scope, no query text or vector required. Use when the
    /// caller wants to enumerate everything under `project=X`
    /// regardless of search hit relevance.
    ///
    /// `source_agent` is the lever the agent-diary read tool uses to
    /// scope diary entries to one caller — passing both
    /// `capsule_type="diary"` and `source_agent="claude-code"` returns
    /// only that agent's notebook.
    ///
    /// Each filter is optional; a `None` filter is a no-op (does not
    /// require the column to be NULL). Limit clamped 1..=200 inside
    /// the function — caller-supplied 0 or absurdly large values
    /// won't surprise. `has_more` is reported via the standard
    /// `LIMIT N+1` trick so the caller can decide whether to paginate.
    #[allow(clippy::too_many_arguments)]
    pub async fn list_capability_capsules_in_scope(
        &self,
        tenant: &str,
        project: Option<&str>,
        repo: Option<&str>,
        module: Option<&str>,
        capsule_type: Option<&str>,
        status: Option<&str>,
        source_agent: Option<&str>,
        cursor: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<(Vec<CapabilityCapsuleRecord>, bool), StorageError> {
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let project = project.map(str::to_owned);
        let repo = repo.map(str::to_owned);
        let module = module.map(str::to_owned);
        let capsule_type = capsule_type.map(str::to_owned);
        let status = status.map(str::to_owned);
        let source_agent = source_agent.map(str::to_owned);
        let cursor: Option<(String, String)> =
            cursor.map(|(updated_at, id)| (updated_at.to_owned(), id.to_owned()));
        let lim = i64::try_from(limit.clamp(1, 200)).unwrap_or(50);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let mut sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules WHERE tenant = ?1",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![Box::new(tenant)];
            if let Some(v) = project {
                sql.push_str(&format!(" AND project = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(v));
            }
            if let Some(v) = repo {
                sql.push_str(&format!(" AND repo = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(v));
            }
            if let Some(v) = module {
                sql.push_str(&format!(" AND module = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(v));
            }
            if let Some(v) = capsule_type {
                sql.push_str(&format!(
                    " AND capability_capsule_type = ?{}",
                    params_vec.len() + 1
                ));
                params_vec.push(Box::new(v));
            }
            if let Some(v) = status {
                sql.push_str(&format!(" AND status = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(v));
            }
            if let Some(v) = source_agent {
                sql.push_str(&format!(" AND source_agent = ?{}", params_vec.len() + 1));
                params_vec.push(Box::new(v));
            }
            if let Some((cur_updated, cur_id)) = cursor {
                // Composite cursor: strictly after (updated_at, id).
                // updated_at sorts DESC, so the resume condition is
                // updated_at < cur OR (updated_at = cur AND id > cur_id).
                let p = params_vec.len();
                sql.push_str(&format!(
                    " AND (updated_at < ?{a} \
                       OR (updated_at = ?{a} AND capability_capsule_id > ?{b}))",
                    a = p + 1,
                    b = p + 2,
                ));
                params_vec.push(Box::new(cur_updated));
                params_vec.push(Box::new(cur_id));
            }
            sql.push_str(" ORDER BY updated_at DESC, capability_capsule_id ASC");
            sql.push_str(&format!(" LIMIT ?{}", params_vec.len() + 1));
            let fetch = lim.saturating_add(1);
            params_vec.push(Box::new(fetch));
            let mut stmt = conn.prepare(&sql)?;
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_capability_capsule_record)?;
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

    /// Single memory by `(tenant, capability_capsule_id)`. Returns `Ok(None)` when
    /// no row matches (the canonical "not found" path — distinct from
    /// errors).
    pub async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.fresh_conn().await?;
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
        let conn = self.fresh_conn().await?;
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
        let conn = self.fresh_conn().await?;
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
    /// Candidate set for the auto-promote sweep. Returns
    /// `PendingConfirmation` rows whose:
    /// - `capability_capsule_type` is in the allow-list `types`,
    /// - `updated_at` is strictly before `cutoff_updated_at` (a
    ///   20-digit ms timestamp; see `storage::current_timestamp`),
    /// - `decay_score` is strictly below `max_decay_score`.
    ///
    /// Empty `types` short-circuits to `Ok(vec![])` — the caller's
    /// allow-list is "promote nothing of any type", which is a valid
    /// way to disable the sweep without flipping the master switch.
    /// Ordered `created_at ASC` so oldest rows promote first (and
    /// callers that cap N per sweep don't keep re-seeing the same
    /// young rows).
    pub async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let cutoff = cutoff_updated_at.to_string();
        let status = enum_to_text(&CapabilityCapsuleStatus::PendingConfirmation)?;
        let type_strs: Vec<String> = types
            .iter()
            .map(enum_to_text)
            .collect::<Result<Vec<_>, _>>()?;
        // `decay_score` is `FLOAT` (Float32) on the Lance side — cast
        // the bind to `f64` here so duckdb-rs picks `REAL` and the
        // comparison stays homogeneous (DuckDB auto-promotes f32→f64
        // on the column side but we want to be explicit).
        let max_decay = max_decay_score as f64;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // ?1 tenant, ?2 status, ?3 cutoff_updated_at, ?4 max_decay,
            // ?5..?(N+4) type allow-list.
            let placeholders = (5..=type_strs.len() + 4)
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND status = ?2 \
                   AND updated_at < ?3 \
                   AND decay_score < ?4 \
                   AND capability_capsule_type IN ({placeholders}) \
                 ORDER BY created_at ASC",
                CAPABILITY_CAPSULE_COLS = CAPABILITY_CAPSULE_COLS,
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn duckdb::ToSql>> = vec![
                Box::new(tenant),
                Box::new(status),
                Box::new(cutoff),
                Box::new(max_decay),
            ];
            for t in type_strs {
                params_vec.push(Box::new(t));
            }
            let params_refs: Vec<&dyn duckdb::ToSql> =
                params_vec.iter().map(|b| b.as_ref()).collect();
            let rows = stmt.query_map(params_refs.as_slice(), row_to_capability_capsule_record)?;
            collect_capability_capsules(rows)
        })
        .await
    }

    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let conn = self.fresh_conn().await?;
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
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let lim = i64::try_from(limit).unwrap_or(64).clamp(1, 1024);
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT {CAPABILITY_CAPSULE_COLS} FROM ns.main.capability_capsules \
                 WHERE tenant = ?1 AND status NOT IN (?2, ?3) AND capability_capsule_type != 'diary' \
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
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        let active = enum_to_text(&CapabilityCapsuleStatus::Active)?;
        // Optional lifecycle-pool cap (perf at scale). `MEM_RECALL_POOL_LIMIT`
        // unset / 0 / invalid → unbounded (the full active pool — default,
        // unchanged behaviour). When set to N>0, only the N most-recently-
        // written non-guidance rows enter the pool; `Preference` / `Workflow`
        // capsules are ALWAYS included (they are floor-exempt "always
        // applicable" guidance — capping them would silently drop directives).
        // The trade-off: a very old capsule that matches only via Rust-side
        // lexical / scope / graph (not the BM25/ANN hybrid hits, which are
        // unionised in separately by retrieve) drops out of the lifecycle
        // fallback. Bounds the per-search row fetch as the corpus grows.
        let pool_limit = std::env::var("MEM_RECALL_POOL_LIMIT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0);
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // Outer alias `c` so the NOT EXISTS subquery can correlate
            // on `c.capability_capsule_id` + `c.tenant`. The subquery
            // (`s`) suppresses any row that has been superseded by
            // another *active* row in the same tenant — version-chain
            // dedup at retrieve time (strategy-readiness §4.4 #3).
            // Browsing paths (`list_capability_capsule_ids_for_tenant`,
            // `list_in_scope`, `fetch_capability_capsules_by_ids`) are
            // deliberately NOT filtered — admin reads want every row.
            let bound_clause = pool_bound_clause(pool_limit);
            let sql = format!(
                "SELECT {cols_c} FROM ns.main.capability_capsules c \
                 WHERE c.tenant = ?1 AND c.status NOT IN (?2, ?3) \
                   AND c.capability_capsule_type != 'diary' \
                   AND NOT EXISTS ( \
                       SELECT 1 FROM ns.main.capability_capsules s \
                       WHERE s.supersedes_capability_capsule_id = c.capability_capsule_id \
                         AND s.tenant = c.tenant \
                         AND s.status = ?4 \
                   ) \
                   {bound_clause}\
                 ORDER BY c.updated_at DESC, c.version DESC, c.capability_capsule_id ASC",
                cols_c = CAPABILITY_CAPSULE_COLS
                    .split(',')
                    .map(|col| format!("c.{}", col.trim()))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                params![tenant, rejected, archived, active],
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
        let conn = self.fresh_conn().await?;
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
        let conn = self.fresh_conn().await?;
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

    /// Top-K BM25 candidate ids over `capability_capsules.content` for
    /// `tenant`. Filters out `rejected` / `archived` status and `diary`
    /// type in the CTE so the caller doesn't have to re-filter at
    /// hydration time. Returns `(capability_capsule_id, rank_lex)` where
    /// rank_lex is 1-based and ordered by `_score DESC, capability_capsule_id ASC`
    /// — matches the ordering the legacy fused query used inside its
    /// `fts` CTE.
    ///
    /// `k` is clamped at the SQL boundary (`lance_fts(... k => k)`)
    /// and additionally clamped to `[1, 1024]`. Empty `query_text`
    /// returns `Ok(vec![])` without touching DuckDB.
    ///
    /// **LANCE-SPECIFIC**: depends on the lance DuckDB extension's
    /// `lance_fts(...)` SQL table function — see
    /// `docs/backend-coupling.md` §2.3. Trait extraction should
    /// expose this as the abstract `top_k_bm25_candidates` primitive
    /// each backend implements with its own FTS engine.
    pub async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_text.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let query_text = query_text.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = "SELECT capability_capsule_id, \
                              ROW_NUMBER() OVER (ORDER BY _score DESC, capability_capsule_id ASC) AS rank_lex \
                       FROM lance_fts('ns.main.capability_capsules', 'content', ?1, k => ?2) \
                       WHERE tenant = ?3 AND status NOT IN (?4, ?5) AND capability_capsule_type != 'diary'";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(
                params![query_text, k_i, tenant, rejected, archived],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(StorageError::DuckDb)?);
            }
            Ok(out)
        })
        .await
    }

    /// Top-K vector candidate ids over
    /// `capability_capsule_embeddings.embedding` for `tenant`. Returns
    /// `(capability_capsule_id, rank_sem)` 1-based, ordered by
    /// `_distance ASC, capability_capsule_id ASC` — same ordering the
    /// legacy fused query's `vec` CTE used.
    ///
    /// **No status / capsule-type filter** in the SQL — the
    /// embeddings table doesn't carry those columns. Callers that
    /// care must filter at hydration / merge time (e.g. by re-checking
    /// `status` / `capability_capsule_type` after
    /// `fetch_capability_capsules_by_ids`).
    ///
    /// Empty `query_embedding` or `k == 0` short-circuits to
    /// `Ok(vec![])`. If the embeddings table doesn't exist yet (lazy-
    /// created on first upsert; brand-new store before any embed
    /// completes), returns `Ok(vec![])` rather than erroring — same
    /// resilience the legacy fused query carried inline.
    ///
    /// **LANCE-SPECIFIC**: depends on the lance DuckDB extension's
    /// `lance_vector_search(...)` SQL table function. Trait
    /// extraction should expose this as the abstract
    /// `top_k_vector_candidates` primitive — each backend implements
    /// with its own ANN path (pgvector HNSW, external service, etc).
    pub async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        // Vector literal interpolated into SQL — duckdb-rs has no
        // FLOAT[] bind path. Same approach as the legacy fused query.
        let vector_lit = format!(
            "[{}]::FLOAT[]",
            query_embedding
                .iter()
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        );
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            let sql = format!(
                "SELECT e.capability_capsule_id, \
                        ROW_NUMBER() OVER (ORDER BY e._distance ASC, e.capability_capsule_id ASC) AS rank_sem \
                 FROM lance_vector_search('ns.main.capability_capsule_embeddings', 'embedding', {vector_lit}, k => ?1) AS e \
                 WHERE e.tenant = ?2",
            );
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(e) if is_capability_capsule_embeddings_missing(&e) => return Ok(Vec::new()),
                Err(e) => return Err(StorageError::DuckDb(e)),
            };
            let rows = match stmt.query_map(params![k_i, tenant], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            }) {
                Ok(r) => r,
                Err(e) if is_capability_capsule_embeddings_missing(&e) => return Ok(Vec::new()),
                Err(e) => return Err(StorageError::DuckDb(e)),
            };
            let mut out = Vec::new();
            for r in rows {
                match r {
                    Ok(pair) => out.push(pair),
                    Err(e) if is_capability_capsule_embeddings_missing(&e) => return Ok(Vec::new()),
                    Err(e) => return Err(StorageError::DuckDb(e)),
                }
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
    /// **Inner-K oversample**: the FTS table function gets `k * 2` so the
    /// outer tenant + status filter doesn't truncate the result below `k`
    /// (same posture as `bm25_candidates`); the ANN branch gets `k * 4`
    /// because its embeddings table holds N chunk-rows per capsule (③),
    /// which collapse to fewer distinct capsules after the GROUP BY.
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
        // The appended `rrf_score` sits at the column index *after* all the
        // capsule columns. Derive it from the column count so adding/removing
        // a capsule column can never drift this index again (it has twice
        // before: last_used_at, then last_recalled_at).
        let rrf_col_idx = CAPABILITY_CAPSULE_COLS.split(',').count();

        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let query_text = query_text.to_string();
        let k_i = i64::try_from(k).unwrap_or(64).clamp(1, 1024);
        // FTS runs over capability_capsules (one row per capsule), so k*2
        // is enough headroom for the outer tenant/status filter.
        let oversample = k_i.saturating_mul(2);
        // ③ The embeddings table now holds N rows per capsule (one per
        // chunk), so lance_vector_search returns chunk-rows that collapse
        // to fewer distinct capsules after GROUP BY. Oversample the ANN
        // branch harder so a handful of long (many-chunk) capsules near
        // the top can't crowd distinct capsules out of the k results.
        let vec_oversample = k_i.saturating_mul(4);
        let rejected = enum_to_text(&CapabilityCapsuleStatus::Rejected)?;
        let archived = enum_to_text(&CapabilityCapsuleStatus::Archived)?;
        let active = enum_to_text(&CapabilityCapsuleStatus::Active)?;

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

            // Build + execute the hybrid SQL with `effective_has_vec`
            // controlling whether the `lance_vector_search(...)` branch
            // is included. Factored into a closure so we can retry
            // text-only when the embeddings dataset doesn't exist yet
            // (lazy-created on first upsert).
            let run = |effective_has_vec: bool| -> Result<
                Vec<(CapabilityCapsuleRecord, f32)>,
                StorageError,
            > {
                // Build the SQL + params in lockstep so DuckDB's strict
                // bind-count check (excess binds reject the prepare
                // with InvalidParameterCount) doesn't fire. Each shape
                // numbers placeholders contiguously.
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
                            WHERE tenant = ?{p3} AND status NOT IN (?{p4}, ?{p5}) AND capability_capsule_type != 'diary' \
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

                let vec_cte = if effective_has_vec {
                    // Vector literal is interpolated into SQL
                    // (duckdb-rs has no FLOAT[] bind path); tenant is
                    // rebound here since the FTS branch may or may not
                    // have bound it. Rebind tenant — duckdb-rs has no
                    // positional reuse, and CTE branches that don't run
                    // cost nothing.
                    let p_tenant_v = bind(Box::new(tenant.clone()));
                    let p_over = bind(Box::new(vec_oversample));
                    format!(
                        "vec AS ( \
                            SELECT capability_capsule_id, \
                                   ROW_NUMBER() OVER (ORDER BY best_distance ASC, capability_capsule_id ASC) AS rank_sem \
                            FROM ( \
                                SELECT e.capability_capsule_id AS capability_capsule_id, \
                                       MIN(e._distance) AS best_distance \
                                FROM lance_vector_search( \
                                        'ns.main.capability_capsule_embeddings', 'embedding', \
                                        {vector_lit}, k => ?{p_over} \
                                      ) AS e \
                                WHERE e.tenant = ?{p_tenant_v} \
                                GROUP BY e.capability_capsule_id \
                            ) \
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

                // Outer hydration filter — always rebound, regardless
                // of which inner CTE produced the row.
                let p_outer_tenant = bind(Box::new(tenant.clone()));
                let p_outer_arch = bind(Box::new(archived.clone()));
                let p_outer_rej = bind(Box::new(rejected.clone()));
                let p_outer_active = bind(Box::new(active.clone()));
                let p_outer_lim = bind(Box::new(k_i));

                // The NOT EXISTS clause is the version-chain dedup
                // (strategy-readiness §4.4 #3): drop any row that's
                // been superseded by another active row in the same
                // tenant. Without it, supersede creates a new active
                // row but leaves the old one Active too, so search
                // returns both. `s` is the superseder; `m` is the
                // (potentially) old version we suppress.
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
                       AND m.capability_capsule_type != 'diary' \
                       AND NOT EXISTS ( \
                           SELECT 1 FROM ns.main.capability_capsules s \
                           WHERE s.supersedes_capability_capsule_id = m.capability_capsule_id \
                             AND s.tenant = m.tenant \
                             AND s.status = ?{p_outer_active} \
                       ) \
                     ORDER BY f.rrf_score DESC, m.updated_at DESC, m.capability_capsule_id ASC \
                     LIMIT ?{p_outer_lim}"
                );

                let params_refs: Vec<&dyn duckdb::ToSql> =
                    params_vec.iter().map(|b| b.as_ref()).collect();
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_refs.as_slice(), |row| {
                    let mem = row_to_capability_capsule_record(row)?;
                    // rrf_score is the column right after the capsule columns
                    // of `m_cols`; `rrf_col_idx` is derived from the column
                    // count so it can't drift when columns change.
                    let rrf: f32 = row.get(rrf_col_idx)?;
                    Ok((mem, rrf))
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(StorageError::DuckDb)?);
                }
                Ok(out)
            };

            // First attempt with the full hybrid SQL. If the embeddings
            // dataset hasn't been lazy-created yet (no successful
            // ingest has run), drop the vec branch and retry as
            // text-only — or return empty when there was no text to
            // fall back to.
            match run(has_vec) {
                Err(StorageError::DuckDb(e))
                    if has_vec && is_capability_capsule_embeddings_missing(&e) =>
                {
                    tracing::debug!(
                        "capability_capsule_embeddings dataset not yet created; \
                         falling back to {}",
                        if has_text { "text-only search" } else { "empty result" }
                    );
                    if has_text {
                        run(false)
                    } else {
                        Ok(Vec::new())
                    }
                }
                other => other,
            }
        })
        .await
    }

    /// Version-chain metadata for a single capsule. Walks both
    /// directions of the `supersedes_capability_capsule_id` link:
    /// **backward** to every predecessor the requested capsule chains
    /// down to, and **forward** to every successor that chains back
    /// up through the requested capsule. Returns each link as a
    /// `CapabilityCapsuleVersionLink` ordered `version DESC,
    /// updated_at DESC` (newest first).
    ///
    /// Walk is implemented as one recursive CTE in DuckDB SQL so the
    /// chain (typically 1–3 links, occasionally more) round-trips in
    /// one query. `tenant` filters every recursion step, so capsules
    /// from other tenants never leak in even if their ids
    /// accidentally collide.
    pub async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        let conn = self.fresh_conn().await?;
        let tenant = tenant.to_string();
        let capability_capsule_id = capability_capsule_id.to_string();
        spawn_blocking_storage(move || {
            let conn = conn.lock().expect("duckdb_query mutex poisoned");
            // Recursive CTE: anchor = the requested capsule; then
            // UNION on (predecessors via supersedes_capability_capsule_id
            // pointing INTO chain) and (successors via
            // supersedes_capability_capsule_id pointing FROM chain).
            // Tenant filter applied at every step.
            let mut stmt = conn.prepare(
                "WITH RECURSIVE chain AS ( \
                    SELECT capability_capsule_id, version, status, updated_at, \
                           supersedes_capability_capsule_id \
                    FROM ns.main.capability_capsules \
                    WHERE tenant = ?1 AND capability_capsule_id = ?2 \
                  UNION \
                    SELECT c.capability_capsule_id, c.version, c.status, c.updated_at, \
                           c.supersedes_capability_capsule_id \
                    FROM ns.main.capability_capsules c \
                    JOIN chain ch \
                      ON c.capability_capsule_id = ch.supersedes_capability_capsule_id \
                      OR c.supersedes_capability_capsule_id = ch.capability_capsule_id \
                    WHERE c.tenant = ?1 \
                ) \
                SELECT capability_capsule_id, version, status, updated_at, \
                       supersedes_capability_capsule_id \
                FROM chain \
                ORDER BY version DESC, updated_at DESC",
            )?;
            let rows = stmt.query_map(params![tenant, capability_capsule_id], |row| {
                Ok(CapabilityCapsuleVersionLink {
                    capability_capsule_id: row.get(0)?,
                    version: row.get::<_, i64>(1)?,
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

    #[test]
    fn pool_bound_clause_off_by_default_caps_with_guidance_exempt() {
        // Unset / 0 → empty → unbounded full pool (default behaviour).
        assert_eq!(pool_bound_clause(None), "");
        // Set → cap the non-guidance rows, always keep preference/workflow.
        let c = pool_bound_clause(Some(500));
        assert!(c.contains("LIMIT 500"), "the cap is applied: {c}");
        assert!(
            c.contains("'preference'") && c.contains("'workflow'"),
            "guidance types are exempt from the cap: {c}"
        );
        assert!(
            c.contains("capability_capsule_id IN ("),
            "non-guidance rows bounded by a recency subquery: {c}"
        );
    }

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
            last_used_at: None,
            last_recalled_at: None,
            expires_at: None,
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
        assert_eq!(a[0].version, 1i64);
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

        // limit clamps to >=1 even when caller passes 0 — caller
        // ergonomics, so `recent_active_capability_capsules(_, 0)`
        // doesn't surprise with an empty result.
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
        // **Version-chain dedup** (strategy-readiness §4.4 #3): m_b is
        // suppressed because m_b_v2 supersedes it and is active.
        // Browsing paths (`list_capability_capsule_ids_for_tenant`,
        // `fetch_capability_capsules_by_ids`) still surface m_b — see
        // the `tests/version_chain_dedup.rs` integration suite for the
        // browsing-vs-search split.
        let cands = q.search_candidates("tenant-a").await.unwrap();
        let cand_ids: Vec<&str> = cands
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            cand_ids,
            vec!["m_b_v2", "m_a"],
            "tenant-a search candidates: archived excluded + superseded m_b suppressed by active m_b_v2"
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

        // list_capability_capsule_versions_for_tenant: walks the
        // `supersedes` chain rooted at the requested capsule, both
        // backward (predecessors) and forward (successors). In this
        // fixture m_b ←supersedes m_b_v2 is the only chain — m_a and
        // m_arc are independent and must NOT leak into the result.
        let chain = q
            .list_capability_capsule_versions_for_tenant("tenant-a", "m_b")
            .await
            .unwrap();
        let chain_ids: Vec<&str> = chain
            .iter()
            .map(|l| l.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            chain_ids,
            vec!["m_b_v2", "m_b"],
            "version-chain walk must isolate the rooted capsule's lineage from other tenant-a rows; got {chain_ids:?}",
        );
        // The supersedes link is preserved on both ends.
        assert_eq!(
            chain[0].capability_capsule_id, "m_b_v2",
            "version DESC ordering: successor first"
        );
        assert_eq!(
            chain[0].supersedes_capability_capsule_id.as_deref(),
            Some("m_b")
        );
        assert!(chain[1].supersedes_capability_capsule_id.is_none());

        // Walk also starts from any node in the chain — request mid-
        // chain id m_b_v2 returns the same set.
        let chain_from_successor = q
            .list_capability_capsule_versions_for_tenant("tenant-a", "m_b_v2")
            .await
            .unwrap();
        let from_successor_ids: Vec<&str> = chain_from_successor
            .iter()
            .map(|l| l.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            from_successor_ids,
            vec!["m_b_v2", "m_b"],
            "walk rooted at successor must surface the predecessor too",
        );

        // Empty result when the requested id doesn't exist (no anchor
        // row to start from).
        let unknown = q
            .list_capability_capsule_versions_for_tenant("tenant-a", "does-not-exist")
            .await
            .unwrap();
        assert!(unknown.is_empty());

        // Cross-tenant: tenant-b has m_other (no supersedes), no
        // chain. Result is just m_other — none of tenant-a's chain
        // leaks across tenant boundaries.
        let cross = q
            .list_capability_capsule_versions_for_tenant("tenant-b", "m_other")
            .await
            .unwrap();
        let cross_ids: Vec<&str> = cross
            .iter()
            .map(|l| l.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            cross_ids,
            vec!["m_other"],
            "tenant filter must apply at every recursion step",
        );

        // Cross-tenant id miss: requesting tenant-a's m_b under
        // tenant-b returns empty (no anchor row matches the tenant).
        let miss = q
            .list_capability_capsule_versions_for_tenant("tenant-b", "m_b")
            .await
            .unwrap();
        assert!(
            miss.is_empty(),
            "tenant-a's id must not surface under tenant-b"
        );
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

    /// ③ multi-chunk capsules: a capsule with N embedding rows (one per
    /// content chunk) must (a) be findable when the query matches ANY
    /// chunk — including a *tail* chunk the single-vector scheme would
    /// have truncated away — and (b) appear EXACTLY ONCE in results,
    /// because `hybrid_candidates` collapses chunk-rows to one capsule
    /// via GROUP BY. The discriminator vs the old single-vector store:
    /// a query near the HEAD chunk *and* a query near the TAIL chunk both
    /// find the same capsule, which last-write-wins single-vector storage
    /// could never satisfy (it would keep only one of the two vectors).
    #[tokio::test]
    async fn duckdb_query_hybrid_candidates_dedups_multi_chunk_capsule() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // c_long → two chunk vectors; c_short → one (distractor).
        let long = fixture("c_long", "tenant-a");
        let short = fixture("c_short", "tenant-a");
        for m in [&long, &short] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
        }

        let now = "00000001778000000000";
        // c_long's two chunks point in orthogonal directions ("head" vs
        // "tail"); each query below matches exactly one of them.
        let v_head = vec![1.0_f32, 0.0, 0.0, 0.0];
        let v_tail = vec![0.0_f32, 0.0, 1.0, 0.0];
        lance
            .upsert_capability_capsule_embedding_chunks(
                "c_long",
                "tenant-a",
                "fake-test",
                4,
                &[v_head, v_tail],
                "c-long-hash",
                now,
                now,
            )
            .await
            .unwrap();
        lance
            .upsert_capability_capsule_embedding_chunks(
                "c_short",
                "tenant-a",
                "fake-test",
                4,
                std::slice::from_ref(&vec![0.0_f32, 1.0, 0.0, 0.0]),
                "c-short-hash",
                now,
                now,
            )
            .await
            .unwrap();

        let q = DuckDbQuery::open(&path).await.unwrap();

        // Query ≈ TAIL chunk. Under single-vector storage the tail had no
        // embedding of its own, so c_long would be unfindable here.
        let query_tail = vec![0.0_f32, 0.0, 0.99, 0.14];
        let tail_hits = q
            .hybrid_candidates("tenant-a", "", &query_tail, 10)
            .await
            .unwrap();
        assert_eq!(
            tail_hits
                .iter()
                .filter(|(m, _)| m.capability_capsule_id == "c_long")
                .count(),
            1,
            "tail query: c_long must appear exactly once (GROUP BY dedup); got {tail_hits:?}"
        );
        assert_eq!(
            tail_hits[0].0.capability_capsule_id, "c_long",
            "tail query must rank c_long (matched via tail chunk) first; got {tail_hits:?}"
        );

        // Query ≈ HEAD chunk also finds c_long exactly once — proving both
        // chunk rows coexist (not last-write-wins single-vector storage).
        let query_head = vec![0.99_f32, 0.14, 0.0, 0.0];
        let head_hits = q
            .hybrid_candidates("tenant-a", "", &query_head, 10)
            .await
            .unwrap();
        assert_eq!(
            head_hits
                .iter()
                .filter(|(m, _)| m.capability_capsule_id == "c_long")
                .count(),
            1,
            "head query: c_long must appear exactly once; got {head_hits:?}"
        );
        assert_eq!(
            head_hits[0].0.capability_capsule_id, "c_long",
            "head query must rank c_long (matched via head chunk) first; got {head_hits:?}"
        );
    }

    /// Fresh stores have no `capability_capsule_embeddings` table yet
    /// (it's lazy-created on first upsert). A search that asks for the
    /// vector branch must not 500 — it should silently fall back to
    /// FTS-only when text is supplied, or return empty when vec is the
    /// only signal.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_hybrid_candidates_missing_embeddings_table() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Capsules only — no embedding upserts, so the
        // `capability_capsule_embeddings` dataset stays uncreated.
        let mut m1 = fixture("m1", "tenant-a");
        m1.content = "ANN HNSW retrieval".into();
        let mut m2 = fixture("m2", "tenant-a");
        m2.content = "Lance datasets store vectors".into();
        for m in [&m1, &m2] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();
        let query_vec = vec![1.0_f32, 0.0, 0.0, 0.0];

        // Hybrid (text + vec): vec branch errors on missing dataset, we
        // fall back to text-only and still surface FTS hits.
        let hybrid = q
            .hybrid_candidates("tenant-a", "ANN HNSW retrieval", &query_vec, 10)
            .await
            .expect("missing embeddings table must not bubble up as 500");
        let ids: Vec<_> = hybrid
            .iter()
            .map(|(m, _)| m.capability_capsule_id.as_str())
            .collect();
        assert!(
            ids.contains(&"m1"),
            "text-only fallback should surface FTS hits; got {ids:?}"
        );

        // Vec-only: no text to fall back to → empty result, still not
        // a 500.
        let vec_only = q
            .hybrid_candidates("tenant-a", "", &query_vec, 10)
            .await
            .expect("missing embeddings table must not bubble up as 500");
        assert!(
            vec_only.is_empty(),
            "vec-only with no embeddings table must return empty; got {vec_only:?}"
        );
    }

    /// Scope-filtered browse path: distinct `project` filters narrow
    /// the result set; pagination via `(updated_at, capability_capsule_id)`
    /// cursor walks deterministically through the set.
    #[tokio::test(flavor = "multi_thread")]
    async fn duckdb_query_list_capability_capsules_in_scope() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // 5 capsules in tenant-a across 2 projects + 1 in tenant-b
        // (must never appear when querying tenant-a).
        let make = |id: &str, project: Option<&str>, updated_at: &str| {
            let mut r = fixture(id, "tenant-a");
            r.project = project.map(String::from);
            r.updated_at = updated_at.into();
            r
        };
        for c in [
            make("a1", Some("mem"), "00000001778000000050"),
            make("a2", Some("mem"), "00000001778000000030"),
            make("a3", Some("mem"), "00000001778000000010"),
            make("a4", Some("aiclass"), "00000001778000000040"),
            make("a5", Some("aiclass"), "00000001778000000020"),
        ] {
            lance.insert_capability_capsule(c).await.unwrap();
        }
        let mut cross = fixture("b1", "tenant-b");
        cross.project = Some("mem".into());
        cross.updated_at = "00000001778000000060".into();
        lance.insert_capability_capsule(cross).await.unwrap();

        let q = DuckDbQuery::open(&path).await.unwrap();

        // No filter (other than tenant): 5 rows, ordered updated_at DESC.
        let (all, more_all) = q
            .list_capability_capsules_in_scope(
                "tenant-a", None, None, None, None, None, None, None, 100,
            )
            .await
            .unwrap();
        let all_ids: Vec<&str> = all
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(all_ids, vec!["a1", "a4", "a2", "a5", "a3"]);
        assert!(!more_all);

        // Project filter narrows to 3.
        let (mem_only, _) = q
            .list_capability_capsules_in_scope(
                "tenant-a",
                Some("mem"),
                None,
                None,
                None,
                None,
                None,
                None,
                100,
            )
            .await
            .unwrap();
        let mem_ids: Vec<&str> = mem_only
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(mem_ids, vec!["a1", "a2", "a3"]);

        // Pagination: limit=2 → first page, cursor → second page.
        let (page1, has_more) = q
            .list_capability_capsules_in_scope(
                "tenant-a", None, None, None, None, None, None, None, 2,
            )
            .await
            .unwrap();
        assert_eq!(page1.len(), 2);
        assert!(has_more);
        let last = page1.last().unwrap();
        let cursor = (
            last.updated_at.as_str(),
            last.capability_capsule_id.as_str(),
        );
        let (page2, _) = q
            .list_capability_capsules_in_scope(
                "tenant-a",
                None,
                None,
                None,
                None,
                None,
                None,
                Some(cursor),
                100,
            )
            .await
            .unwrap();
        let page2_ids: Vec<&str> = page2
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        // Page1 was [a1, a4]; page2 picks up at a2, a5, a3.
        assert_eq!(page2_ids, vec!["a2", "a5", "a3"]);

        // Cross-tenant isolation: tenant-b returns only b1.
        let (b, _) = q
            .list_capability_capsules_in_scope(
                "tenant-b", None, None, None, None, None, None, None, 100,
            )
            .await
            .unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].capability_capsule_id, "b1");
    }

    /// `capability_capsule_type=diary` rows are excluded from the
    /// shared search pool (`hybrid_candidates`), but `list_in_scope`
    /// with the explicit type filter still surfaces them — that's
    /// how `agent_diary_read` reaches them.
    #[tokio::test(flavor = "multi_thread")]
    async fn diary_excluded_from_hybrid_excluded_from_list_in_scope_when_unset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Two rows: one regular Implementation, one Diary entry.
        let mut diary = fixture("d1", "tenant-a");
        diary.capability_capsule_type = CapabilityCapsuleType::Diary;
        diary.content = "scratchpad: tried approach X, didn't work because Y".into();
        diary.summary = "scratchpad: tried approach X".into();
        diary.source_agent = "claude-code".into();
        diary.updated_at = "00000001778000000050".into();
        let mut regular = fixture("r1", "tenant-a");
        regular.content = "use bun.lockb for deterministic installs".into();
        regular.summary = "deterministic installs".into();
        regular.updated_at = "00000001778000000040".into();
        for r in [diary, regular] {
            lance.insert_capability_capsule(r).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        // Hybrid candidates (BM25 over the shared pool) must not
        // surface the diary entry.
        let hits = q
            .hybrid_candidates("tenant-a", "scratchpad approach", &[], 10)
            .await
            .unwrap();
        let hit_ids: Vec<&str> = hits
            .iter()
            .map(|(r, _)| r.capability_capsule_id.as_str())
            .collect();
        assert!(
            !hit_ids.contains(&"d1"),
            "diary entries must be excluded from hybrid_candidates; got {hit_ids:?}"
        );

        // list_in_scope without a type filter also drops diary — the
        // shared pool conventions apply (diary is opt-in only).
        let (no_filter, _) = q
            .list_capability_capsules_in_scope(
                "tenant-a", None, None, None, None, None, None, None, 100,
            )
            .await
            .unwrap();
        let no_filter_ids: Vec<&str> = no_filter
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        // The default list_in_scope does NOT auto-exclude diary —
        // exclusion is enforced in `hybrid_candidates` only. Diary is
        // visible here so that `agent_diary_read` (which uses this
        // path with type=diary filter) works.
        assert!(
            no_filter_ids.contains(&"d1"),
            "list_in_scope without filter must include diary so the read tool can find it; got {no_filter_ids:?}"
        );

        // Explicit type=diary surfaces just the diary.
        let (diary_only, _) = q
            .list_capability_capsules_in_scope(
                "tenant-a",
                None,
                None,
                None,
                Some("diary"),
                None,
                None,
                None,
                100,
            )
            .await
            .unwrap();
        let d_ids: Vec<&str> = diary_only
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(d_ids, vec!["d1"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capsule_stats_groups_per_status_and_scopes_by_tenant() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("store");
        let lance = LanceStore::open(&path).await.unwrap();

        // Seed tenant-a with 2 active, 1 pending, 1 archived;
        // tenant-b with 1 rejected. Distinct content_hash per row so
        // ingest dedup doesn't collapse them.
        let mut a1 = fixture("c_a1", "tenant-a");
        a1.content_hash = "h-a1".into();
        let mut a2 = fixture("c_a2", "tenant-a");
        a2.content_hash = "h-a2".into();
        let mut p1 = fixture("c_p1", "tenant-a");
        p1.status = CapabilityCapsuleStatus::PendingConfirmation;
        p1.content_hash = "h-p1".into();
        let mut x1 = fixture("c_x1", "tenant-a");
        x1.status = CapabilityCapsuleStatus::Archived;
        x1.content_hash = "h-x1".into();
        let mut r1 = fixture("c_r1", "tenant-b");
        r1.status = CapabilityCapsuleStatus::Rejected;
        r1.content_hash = "h-r1".into();
        for m in [&a1, &a2, &p1, &x1, &r1] {
            lance.insert_capability_capsule(m.clone()).await.unwrap();
        }

        let q = DuckDbQuery::open(&path).await.unwrap();

        let a = q.capsule_stats("tenant-a").await.unwrap();
        assert_eq!(a.total, 4);
        assert_eq!(a.active, 2);
        assert_eq!(a.pending_confirmation, 1);
        assert_eq!(a.archived, 1);
        assert_eq!(a.rejected, 0);
        assert_eq!(a.provisional, 0);

        let b = q.capsule_stats("tenant-b").await.unwrap();
        assert_eq!(b.total, 1);
        assert_eq!(b.rejected, 1);
        assert_eq!(b.active, 0);

        let empty = q.capsule_stats("does-not-exist").await.unwrap();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.active, 0);
    }
}
