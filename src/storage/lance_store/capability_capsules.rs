//! Memory CRUD + filter + lookup + embedding-job + episode/session +
//! feedback methods. All inherent on LanceStore. Helpers
//! (`query_capability_capsules`, `update_status`, `query_embedding_jobs`) used
//! across these methods live with their domain rather than in
//! `mod.rs`.

use arrow_array::{Float32Array, RecordBatch, StringArray};
use futures::TryStreamExt;
use lancedb::query::{ExecutableQuery, QueryBase};

use super::{
    capability_capsule_embedding_to_record_batch, capability_capsules_to_record_batch,
    embedding_job_row_to_record_batch, embedding_job_rows_to_record_batch,
    ensure_capability_capsule_embeddings_table, enum_to_str, feedback_events_to_record_batch,
    lancedb_err, parse_col, record_batch_to_capability_capsules,
    record_batch_to_embedding_job_rows, record_batch_to_feedback_events, sql_quote,
    EmbeddingJobRow, LanceStore,
};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleVersionLink, CapsuleStats,
    FeedbackSummary,
};
use crate::domain::embeddings::EmbeddingJobInfo;
use crate::embedding::wire::decode_f32_blob;
use crate::storage::types::{ClaimedEmbeddingJob, EmbeddingJobInsert, FeedbackEvent, StorageError};
use crate::storage::{timestamp_sub_ms, EMBEDDING_JOB_LEASE_MS};

/// Sort priority for `find_by_idempotency_or_hash`'s `ORDER BY`: an
/// idempotency-key match ranks first (0) when the caller supplied a key AND
/// the row carries that exact key; everything else (content-hash-only match)
/// is 1. Mirrors the DuckDB
/// `CASE WHEN ?2 IS NOT NULL AND idempotency_key = ?2 THEN 0 ELSE 1 END`.
fn key_priority(rec: &CapabilityCapsuleRecord, idempotency_key: &Option<String>) -> i32 {
    match idempotency_key {
        Some(k) if rec.idempotency_key.as_deref() == Some(k.as_str()) => 0,
        _ => 1,
    }
}

impl LanceStore {
    /// Apply a status transition to `(tenant, capability_capsule_id)` and return the
    /// updated row. Shared by `accept_pending` / `reject_pending` (and a
    /// future `archive_pending` if needed). Mirrors the DuckDB backend's
    /// `update_status` private helper.
    ///
    /// **Not yet implemented:** the embedding-references cleanup that the
    /// DuckDB version does (delete `embedding_jobs` + `capability_capsule_embeddings`
    /// rows for this memory) — those tables don't exist on the LanceDB
    /// side yet. Add when those tables land.
    pub async fn update_status(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        status_str: &str,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let table = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let now = crate::storage::current_timestamp();
        let result = table
            .update()
            .only_if(format!(
                "tenant = {} AND capability_capsule_id = {}",
                sql_quote(tenant),
                sql_quote(capability_capsule_id),
            ))
            .column("status", sql_quote(status_str))
            .column("updated_at", sql_quote(&now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        self.get_capability_capsule_for_tenant(tenant, capability_capsule_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "memory missing after status update",
            ))
    }

    /// Run a filter query against the `memories` table and parse all
    /// returned batches into [`CapabilityCapsuleRecord`]s. Shared by every read
    /// method that just needs a `WHERE`-clause + optional `LIMIT`.
    pub async fn query_capability_capsules(
        &self,
        filter: String,
        limit: Option<usize>,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let table = self
            .conn
            .open_table("capability_capsules")
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
            out.extend(record_batch_to_capability_capsules(b)?);
        }
        Ok(out)
    }

    /// Route-B bucket "filter": native lancedb-Rust equivalent of
    /// `DuckDbQuery::list_capability_capsules_for_tenant` — every row for
    /// `tenant`, no ordering (callers sort). Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn list_capability_capsules_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        self.query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await
    }

    /// Route-B bucket "fetch-by-ids": native lancedb-Rust equivalent of
    /// `DuckDbQuery::fetch_capability_capsules_by_ids` — bulk fetch by
    /// `capability_capsule_id` list, scoped to `tenant`.
    ///
    /// **Order parity**: the DuckDB version uses `WHERE … IN (…)` and
    /// returns rows in **DB-natural order, NOT input-slice order** (its
    /// docstring spells this out; the only caller — `hybrid_candidates_compose`
    /// — reshapes via a HashMap and re-sorts, so it never depends on the order).
    /// A lance `only_if` scan likewise returns table order, not IN-list order,
    /// so neither side is input-ordered and the contract holds. Empty `ids`
    /// short-circuits to `Ok(vec![])` (mirrors DuckDB). Missing ids are simply
    /// absent from the scan result (same as the DuckDB `IN` semantics).
    ///
    /// Parity-exercised by `tests/parity_golden.rs::lance_parity_matches_golden`
    /// (the compose block hydrates capsules through this on the Lance engine)
    /// and `tests/storage_fetch_by_ids.rs`.
    pub async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let id_list = ids
            .iter()
            .map(|id| sql_quote(id))
            .collect::<Vec<_>>()
            .join(", ");
        self.query_capability_capsules(
            format!(
                "tenant = {} AND capability_capsule_id IN ({id_list})",
                sql_quote(tenant),
            ),
            None,
        )
        .await
    }

    /// Route-B batch-A bucket "list_wings": native lancedb-Rust equivalent
    /// of `DuckDbQuery::list_wings` — DISTINCT `project` for `tenant` where
    /// `project IS NOT NULL`, ordered alphabetically.
    ///
    /// The DuckDB SQL is `SELECT DISTINCT project ... WHERE tenant = ? AND
    /// project IS NOT NULL ORDER BY project`. Diary capsules participate in
    /// the project space too (no type exclusion). Here we fetch the tenant's
    /// rows, collect each `Some(project)` into a `BTreeSet` (dedups AND sorts
    /// in one shot, matching `ORDER BY project`), and return the names.
    /// Parity-gated by `tests/parity_golden.rs`.
    pub async fn list_wings(&self, tenant: &str) -> Result<Vec<String>, StorageError> {
        let rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;
        let projects: std::collections::BTreeSet<String> =
            rows.into_iter().filter_map(|r| r.project).collect();
        Ok(projects.into_iter().collect())
    }

    /// Route-B batch-A bucket "get_pending": native lancedb-Rust equivalent
    /// of `DuckDbQuery::get_pending` — a single row matching `(tenant,
    /// capability_capsule_id)` whose status is `PendingConfirmation`.
    /// Returns `Ok(None)` when the row is absent or has already moved off
    /// `pending_confirmation` (accepted/rejected) — the DuckDB query's
    /// `AND status = 'pending_confirmation'` predicate. Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn get_pending(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let status = enum_to_str(&CapabilityCapsuleStatus::PendingConfirmation)?;
        let rows = self
            .query_capability_capsules(
                format!(
                    "tenant = {} AND capability_capsule_id = {} AND status = {}",
                    sql_quote(tenant),
                    sql_quote(capability_capsule_id),
                    sql_quote(&status),
                ),
                Some(1),
            )
            .await?;
        Ok(rows.into_iter().next())
    }

    /// Route-B native equivalent of `DuckDbQuery::find_by_idempotency_or_hash`
    /// — the ingest-path idempotency check. Matches on either an
    /// `idempotency_key` (only when the caller supplied one) **or** the
    /// `content_hash` (always), scoped to `tenant`.
    ///
    /// Ranking parity (the DuckDB `ORDER BY`): idempotency-key matches rank
    /// first (priority 0) so a caller-asserted identity wins over a
    /// content-hash collision; ties break by `updated_at DESC`. We replicate
    /// exactly — scan the union of (key-match ∪ hash-match) rows, then in Rust
    /// pick the top under `(key_match_first, updated_at DESC)`. Returns the top
    /// row or `None`. LanceDB has no `ORDER BY` / `CASE`, so the priority +
    /// tie-break runs in Rust.
    pub async fn find_by_idempotency_or_hash(
        &self,
        tenant: &str,
        idempotency_key: &Option<String>,
        content_hash: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        // Filter: tenant AND (idempotency_key matches when supplied OR
        // content_hash matches). When no key is supplied, only the hash arm
        // applies — mirrors the DuckDB `(?2 IS NOT NULL AND ...) OR ...`.
        let key_clause = match idempotency_key {
            Some(k) => format!("idempotency_key = {} OR ", sql_quote(k)),
            None => String::new(),
        };
        let filter = format!(
            "tenant = {} AND ({key_clause}content_hash = {})",
            sql_quote(tenant),
            sql_quote(content_hash),
        );
        let mut rows = self.query_capability_capsules(filter, None).await?;
        // ORDER BY (key-match THEN 0 ELSE 1), updated_at DESC. A row's key
        // priority is 0 iff the caller supplied a key and the row carries it.
        rows.sort_by(|a, b| {
            let pa = key_priority(a, idempotency_key);
            let pb = key_priority(b, idempotency_key);
            pa.cmp(&pb).then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        Ok(rows.into_iter().next())
    }

    /// Route-B native equivalent of `DuckDbQuery::auto_promote_candidates` —
    /// the auto-promote sweep candidate set. Returns `PendingConfirmation`
    /// rows under `tenant` whose `capability_capsule_type` is in `types`,
    /// `updated_at < cutoff_updated_at`, and `decay_score < max_decay_score`,
    /// ordered `created_at ASC` (oldest first, so a per-sweep N cap doesn't
    /// re-see young rows).
    ///
    /// Empty `types` short-circuits to `Ok(vec![])` (matches DuckDB — an empty
    /// allow-list disables the sweep without flipping the master switch).
    /// LanceDB has no `ORDER BY`, so the `created_at ASC` sort runs in Rust.
    pub async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[crate::domain::capability_capsule::CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        if types.is_empty() {
            return Ok(Vec::new());
        }
        let status = enum_to_str(&CapabilityCapsuleStatus::PendingConfirmation)?;
        let type_strs = types
            .iter()
            .map(enum_to_str)
            .collect::<Result<Vec<_>, _>>()?;
        let type_list = type_strs
            .iter()
            .map(|t| sql_quote(t))
            .collect::<Vec<_>>()
            .join(", ");
        let filter = format!(
            "tenant = {} AND status = {} AND updated_at < {} \
             AND decay_score < {} AND capability_capsule_type IN ({type_list})",
            sql_quote(tenant),
            sql_quote(&status),
            sql_quote(cutoff_updated_at),
            max_decay_score,
        );
        let mut rows = self.query_capability_capsules(filter, None).await?;
        // ORDER BY created_at ASC.
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(rows)
    }

    /// Route-B batch-A bucket "list_pending_review": native lancedb-Rust
    /// equivalent of `DuckDbQuery::list_pending_review` — every
    /// `PendingConfirmation` row under `tenant`, ordered `created_at DESC`
    /// (newest arrivals first). LanceDB doesn't sort, so we re-sort in Rust
    /// to match the DuckDB `ORDER BY`. Parity-gated by
    /// `tests/parity_golden.rs`.
    pub async fn list_pending_review(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let status = enum_to_str(&CapabilityCapsuleStatus::PendingConfirmation)?;
        let mut rows = self
            .query_capability_capsules(
                format!(
                    "tenant = {} AND status = {}",
                    sql_quote(tenant),
                    sql_quote(&status),
                ),
                None,
            )
            .await?;
        // ORDER BY created_at DESC.
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(rows)
    }

    /// Route-B batch-A bucket "recent_active": native lancedb-Rust
    /// equivalent of `DuckDbQuery::recent_active_capability_capsules` — the
    /// most-recent non-rejected, non-archived, non-diary rows under
    /// `tenant`, ordered `(updated_at DESC, version DESC,
    /// capability_capsule_id ASC)`, limited to `clamp(limit, 1, 1024)`.
    ///
    /// `PendingConfirmation` / `Provisional` / `Active` all pass — the
    /// DuckDB SQL only excludes the two terminal statuses (rejected /
    /// archived) and `diary` type. LanceDB doesn't ORDER BY / LIMIT
    /// deterministically with the right tie-break, so we sort + truncate in
    /// Rust. Parity-gated by `tests/parity_golden.rs`.
    pub async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        use crate::domain::capability_capsule::{
            CapabilityCapsuleStatus as S, CapabilityCapsuleType as T,
        };
        let lim = limit.clamp(1, 1024);
        let mut rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;
        rows.retain(|r| {
            !matches!(r.status, S::Rejected | S::Archived)
                && !matches!(r.capability_capsule_type, T::Diary)
        });
        // ORDER BY updated_at DESC, version DESC, capability_capsule_id ASC.
        rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.version.cmp(&a.version))
                .then_with(|| a.capability_capsule_id.cmp(&b.capability_capsule_id))
        });
        rows.truncate(lim);
        Ok(rows)
    }

    /// Route-B batch-A bucket "list_ids": native lancedb-Rust equivalent of
    /// `DuckDbQuery::list_capability_capsule_ids_for_tenant` — just the
    /// `capability_capsule_id` column for `tenant`, **all statuses**, ordered
    /// `updated_at DESC`. No status / type filter (admin read wants every
    /// row). LanceDB doesn't sort, so we sort in Rust to match the DuckDB
    /// `ORDER BY`. Parity-gated by `tests/parity_golden.rs`.
    pub async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;
        // ORDER BY updated_at DESC. The DuckDB query has no secondary
        // tie-break key, but the parity fixture's timestamps are distinct,
        // so the order is deterministic either way.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(rows.into_iter().map(|r| r.capability_capsule_id).collect())
    }

    /// Route-B batch-A bucket "list_in_scope": native lancedb-Rust
    /// equivalent of `DuckDbQuery::list_capability_capsules_in_scope` — the
    /// embedding-free scope browse path. Each of `(project, repo, module,
    /// capsule_type, status, source_agent)` is an optional equality filter
    /// (a `None` is a no-op); results are ordered `(updated_at DESC,
    /// capability_capsule_id ASC)` and paginated by the composite cursor
    /// `(updated_at, id)`.
    ///
    /// The DuckDB query pushes each filter into the `WHERE`, applies the
    /// composite-cursor resume condition (`updated_at < cur OR (updated_at =
    /// cur AND id > cur_id)`), orders, and fetches `LIMIT clamp(limit,
    /// 1, 200) + 1` to report `has_more` via the standard N+1 trick. We
    /// replicate exactly: equality filters in the lance `only_if`
    /// predicate; the cursor + ordering + N+1 slice in Rust (LanceDB has no
    /// ORDER BY). `enum`-shaped filters (`capsule_type`, `status`) are
    /// matched as the raw snake_case strings the caller passes, identical to
    /// the DuckDB `= ?` bind. Parity-gated by `tests/parity_golden.rs`.
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
        let lim = limit.clamp(1, 200);
        let mut clauses = vec![format!("tenant = {}", sql_quote(tenant))];
        if let Some(v) = project {
            clauses.push(format!("project = {}", sql_quote(v)));
        }
        if let Some(v) = repo {
            clauses.push(format!("repo = {}", sql_quote(v)));
        }
        if let Some(v) = module {
            clauses.push(format!("module = {}", sql_quote(v)));
        }
        if let Some(v) = capsule_type {
            clauses.push(format!("capability_capsule_type = {}", sql_quote(v)));
        }
        if let Some(v) = status {
            clauses.push(format!("status = {}", sql_quote(v)));
        }
        if let Some(v) = source_agent {
            clauses.push(format!("source_agent = {}", sql_quote(v)));
        }
        let filter = clauses.join(" AND ");
        let mut rows = self.query_capability_capsules(filter, None).await?;

        // Composite cursor: strictly after (updated_at, id) under the
        // ORDER BY (updated_at DESC, id ASC). Resume condition mirrors the
        // DuckDB SQL: updated_at < cur OR (updated_at = cur AND id > cur_id).
        if let Some((cur_updated, cur_id)) = cursor {
            rows.retain(|r| {
                r.updated_at.as_str() < cur_updated
                    || (r.updated_at.as_str() == cur_updated
                        && r.capability_capsule_id.as_str() > cur_id)
            });
        }

        // ORDER BY updated_at DESC, capability_capsule_id ASC.
        rows.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.capability_capsule_id.cmp(&b.capability_capsule_id))
        });

        // N+1 trick: fetch lim+1, has_more if we overshot, then trim.
        let has_more = rows.len() > lim;
        rows.truncate(lim);
        Ok((rows, has_more))
    }

    /// Route-B bucket "ann": native lancedb-Rust equivalent of
    /// `DuckDbQuery::ann_candidate_ids`.
    ///
    /// Returns `(capability_capsule_id, rank_sem)` 1-based, ordered by
    /// `(_distance ASC, capability_capsule_id ASC)`.
    ///
    /// **POSTFILTER parity** (not prefilter): the DuckDB query runs
    /// `lance_vector_search(... k => k)` over ALL tenants' vectors first,
    /// then filters `WHERE tenant = ?`. So a tenant's result can contain
    /// FEWER than `k` rows when another tenant's vectors are nearer (they
    /// consume `k` slots, then drop in the post-filter). We replicate this
    /// exactly: `nearest_to(...).limit(k)` WITHOUT a tenant predicate in
    /// the vector query, then filter `tenant` in Rust afterward.
    ///
    /// Capsules carry exactly one embedding row each (no chunking), so no
    /// GROUP BY / collapse is needed. Empty `query_embedding` or `k == 0`
    /// → `Ok(vec![])`. Missing embeddings table (lazy-created on first
    /// upsert) → `Ok(vec![])`, mirroring the DuckDB resilience.
    pub async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // Lazy-created table: a brand-new store has no embeddings until the
        // first upsert lands. Mirror DuckDB's
        // `is_capability_capsule_embeddings_missing` → empty result.
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "capability_capsule_embeddings") {
            return Ok(Vec::new());
        }
        let table = self
            .conn
            .open_table("capability_capsule_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // POSTFILTER, not prefilter: take the top-k nearest ACROSS ALL
        // tenants (no `.only_if` tenant predicate), then drop other tenants'
        // rows in Rust. `nearest_to` adds a `_distance` (Float32) column and
        // orders by it; `.limit(k)` caps the candidate set to `k` rows.
        let stream = table
            .query()
            .nearest_to(query_embedding)
            .map_err(lancedb_err)?
            .limit(k)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = stream
            .try_collect()
            .await
            .map_err(|e| StorageError::InvalidInput(format!("lancedb stream: {e}")))?;

        const TABLE: &str = "capability_capsule_embeddings";
        // (capability_capsule_id, _distance) for rows in this tenant. Other
        // tenants' rows consumed top-k slots upstream but are dropped here —
        // this is what makes a k=5 query return <5 rows when a foreign
        // vector is among the 5 nearest.
        let mut kept: Vec<(String, f32)> = Vec::new();
        for b in &batches {
            if b.num_rows() == 0 {
                continue;
            }
            let ids = parse_col::<StringArray>(b, TABLE, "capability_capsule_id")?;
            let tenants = parse_col::<StringArray>(b, TABLE, "tenant")?;
            let dists = parse_col::<Float32Array>(b, TABLE, "_distance")?;
            for i in 0..b.num_rows() {
                if tenants.value(i) == tenant {
                    kept.push((ids.value(i).to_string(), dists.value(i)));
                }
            }
        }
        // Match the DuckDB ORDER BY tie-break exactly: (_distance ASC,
        // capability_capsule_id ASC). `nearest_to` already returns rows in
        // ascending distance, but an explicit re-sort makes the id tie-break
        // deterministic and independent of the upstream stream order.
        kept.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        Ok(kept
            .into_iter()
            .enumerate()
            .map(|(idx, (id, _))| (id, idx as i64 + 1))
            .collect())
    }

    /// Rebuild the Tantivy capsule FTS index from the current
    /// `capability_capsules` corpus. Scans every row across all tenants
    /// (the index is tenant-tagged and filters at query time), excluding
    /// the rows the DuckDB `lance_fts` query excludes — archived /
    /// rejected status and `diary` type — so BM25 parity holds (the
    /// DuckDB SQL carries `status NOT IN (rejected, archived) AND
    /// capability_capsule_type != 'diary'`). A full rebuild, matching the
    /// route-B "startup full-rebuild" strategy (see `crate::storage::fts`).
    ///
    /// Marks the index built so the lazy path in
    /// [`Self::bm25_candidate_ids`] doesn't redundantly rebuild.
    pub async fn rebuild_capsule_fts(&self) -> Result<(), StorageError> {
        use crate::domain::capability_capsule::{
            CapabilityCapsuleStatus as S, CapabilityCapsuleType as T,
        };
        // Scan all tenants — the FTS index is tenant-tagged and filters
        // tenant at query time (POSTFILTER, same posture as the rest of
        // route-B). No `WHERE` clause beyond "everything", then drop the
        // excluded rows in Rust to mirror the DuckDB BM25 filter.
        let rows = self
            .query_capability_capsules("true".to_string(), None)
            .await?;
        let docs: Vec<crate::storage::fts::FtsDoc> = rows
            .into_iter()
            .filter(|r| {
                !matches!(r.status, S::Archived | S::Rejected)
                    && !matches!(r.capability_capsule_type, T::Diary)
            })
            .map(|r| crate::storage::fts::FtsDoc {
                id: r.capability_capsule_id,
                tenant: r.tenant,
                content: r.content,
            })
            .collect();
        // Tantivy writes are synchronous + CPU-bound; run them off the
        // async reactor.
        let fts = self.fts.clone();
        tokio::task::spawn_blocking(move || fts.rebuild(&docs))
            .await
            .map_err(|e| StorageError::InvalidInput(format!("fts rebuild join: {e}")))??;
        self.fts_built
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    /// Route-B bucket "fts": native (Tantivy) equivalent of
    /// `DuckDbQuery::bm25_candidate_ids`. Returns
    /// `(capability_capsule_id, rank_lex)` 1-based, ordered by
    /// `(score DESC, capability_capsule_id ASC)`.
    ///
    /// The Tantivy index ([`crate::storage::fts::FtsIndex`]) is built from
    /// the capsule corpus via [`Self::rebuild_capsule_fts`] — eagerly by
    /// `rebuild_query_indexes`, or lazily here on first use if it was
    /// never built (so the route-B read engine works standalone). The
    /// query is term-split through the jieba tokenizer so unspaced CJK
    /// runs match (the load-bearing CJK fix — see `fts` module docs).
    ///
    /// Empty / whitespace `query_text` or `k == 0` → `Ok(vec![])`.
    pub async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_text.trim().is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // Lazy build: a route-B store that never ran rebuild_query_indexes
        // still needs a populated index. Idempotent — the eager path flips
        // `fts_built` so this only fires once.
        if !self.fts_built.load(std::sync::atomic::Ordering::SeqCst) {
            self.rebuild_capsule_fts().await?;
        }
        let fts = self.fts.clone();
        let tenant = tenant.to_string();
        let query_text = query_text.to_string();
        tokio::task::spawn_blocking(move || fts.bm25(&tenant, &query_text, k))
            .await
            .map_err(|e| StorageError::InvalidInput(format!("fts query join: {e}")))?
    }

    /// Route-B bucket "stats": native lancedb-Rust equivalent of
    /// `DuckDbQuery::capsule_stats` — the DuckDB `GROUP BY status`
    /// aggregation folded into the discrete fields on [`CapsuleStats`].
    /// Fetches every row for `tenant` via
    /// [`Self::query_capability_capsules`] and counts the per-row `status`
    /// enum in Rust. `total` is the row count; each field is the count of
    /// that status variant. Parity-gated by `tests/parity_golden.rs`.
    pub async fn capsule_stats(&self, tenant: &str) -> Result<CapsuleStats, StorageError> {
        let rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;
        let mut stats = CapsuleStats::default();
        for r in rows {
            stats.total += 1;
            // Match on the enum variant directly so the field mapping can't
            // drift from the on-disk status strings. Mirrors the DuckDB
            // backend's `GROUP BY status` fold (which keys off the same
            // snake_case strings "active"/"archived"/… the enum serializes
            // to).
            match r.status {
                CapabilityCapsuleStatus::PendingConfirmation => stats.pending_confirmation += 1,
                CapabilityCapsuleStatus::Provisional => stats.provisional += 1,
                CapabilityCapsuleStatus::Active => stats.active += 1,
                CapabilityCapsuleStatus::Archived => stats.archived += 1,
                CapabilityCapsuleStatus::Rejected => stats.rejected += 1,
            }
        }
        Ok(stats)
    }

    /// Route-B bucket "taxonomy": native lancedb-Rust equivalent of
    /// `DuckDbQuery::get_taxonomy` — the DuckDB
    /// `WHERE project IS NOT NULL GROUP BY project, repo
    /// ORDER BY project, repo` query folded into `[(project, Vec<repo>)]`.
    /// Fetches rows for `tenant`, keeps only those with a project (mirrors
    /// `project IS NOT NULL`), collects the DISTINCT `(project, repo)`
    /// pairs, then folds consecutive same-project rows pushing only
    /// `Some(repo)` values. Parity-gated by `tests/parity_golden.rs`.
    pub async fn get_taxonomy(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, Vec<String>)>, StorageError> {
        let rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;
        // DISTINCT (project, repo) for project-bearing rows. A `BTreeSet`
        // dedups AND sorts by `(project, repo)` in one shot; `Option<String>`
        // sorts `None` before `Some(_)`, matching DuckDB's `NULLS FIRST`
        // default for `ORDER BY repo ASC` — verified against
        // `tests/golden/taxonomy.json`.
        let pairs: std::collections::BTreeSet<(String, Option<String>)> = rows
            .into_iter()
            .filter_map(|r| r.project.map(|p| (p, r.repo)))
            .collect();
        // Fold consecutive same-project rows — identical to the DuckDB
        // backend's `grouped.last_mut()` fold.
        let mut grouped: Vec<(String, Vec<String>)> = Vec::new();
        for (project, repo) in pairs {
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
    }

    /// Route-B bucket "version-chain": native lancedb-Rust equivalent of
    /// `DuckDbQuery::search_candidates` — the lifecycle-pool loader.
    ///
    /// Fetches every row for `tenant` via
    /// [`Self::query_capability_capsules`], then reproduces the DuckDB SQL
    /// filter in Rust:
    ///
    /// 1. Keep rows with `status NOT IN ('rejected', 'archived')` and
    ///    `capability_capsule_type != 'diary'`. (`PendingConfirmation` /
    ///    `Provisional` / `Active` all pass — the SQL only excludes the
    ///    two terminal statuses.)
    /// 2. Drop any row that has been **superseded by another *active*
    ///    row** in the same tenant (the DuckDB `NOT EXISTS` subquery —
    ///    version-chain dedup at retrieve time). `s.supersedes = c.id AND
    ///    s.tenant = c.tenant AND s.status = active`.
    /// 3. Optional `MEM_RECALL_POOL_LIMIT` cap: unset / 0 / invalid →
    ///    unbounded full pool (default; the test env does not set it).
    ///    When `Some(n)`, `preference` / `workflow` guidance is ALWAYS
    ///    kept (the floor-exempt guidance), plus the `n` most-recently-
    ///    written (`updated_at DESC, ties unspecified`) NON-guidance rows
    ///    drawn from the raw `status NOT IN (rejected, archived) AND type
    ///    != diary` set (the same recency subquery the DuckDB
    ///    `pool_bound_clause` runs — it does NOT re-apply the supersede
    ///    dedup, so the cap is computed over the pre-dedup set).
    ///
    /// Ordering matches the DuckDB `ORDER BY updated_at DESC, version
    /// DESC, capability_capsule_id ASC` so the returned `Vec` is
    /// deterministic; the parity golden re-sorts the ids anyway.
    /// Parity-gated by `tests/parity_golden.rs`.
    pub async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;

        // Mirror the DuckDB env read: unset / 0 / invalid → unbounded.
        let pool_limit = std::env::var("MEM_RECALL_POOL_LIMIT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0);

        let is_excluded_status = |s: &CapabilityCapsuleStatus| {
            matches!(
                s,
                CapabilityCapsuleStatus::Rejected | CapabilityCapsuleStatus::Archived
            )
        };
        let is_diary = |t: &crate::domain::capability_capsule::CapabilityCapsuleType| {
            matches!(
                t,
                crate::domain::capability_capsule::CapabilityCapsuleType::Diary
            )
        };
        let is_guidance = |t: &crate::domain::capability_capsule::CapabilityCapsuleType| {
            matches!(
                t,
                crate::domain::capability_capsule::CapabilityCapsuleType::Preference
                    | crate::domain::capability_capsule::CapabilityCapsuleType::Workflow
            )
        };

        // Optional `MEM_RECALL_POOL_LIMIT` recency allow-list. Mirrors the
        // DuckDB `pool_bound_clause` subquery: the N most-recently-written
        // (`updated_at DESC`) rows over the raw `status NOT IN (rejected,
        // archived) AND type != diary` set — NOT the supersede-deduped set,
        // and NOT guidance-floored (guidance is exempted separately below).
        let bound_ids: Option<std::collections::HashSet<String>> = pool_limit.map(|n| {
            let mut candidates: Vec<&CapabilityCapsuleRecord> = rows
                .iter()
                .filter(|r| !is_excluded_status(&r.status) && !is_diary(&r.capability_capsule_type))
                .collect();
            candidates.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            candidates
                .into_iter()
                .take(n)
                .map(|r| r.capability_capsule_id.clone())
                .collect()
        });

        // The supersede dedup operates over the SAME tenant's rows: a row is
        // dropped iff some other row supersedes it AND that superseder is
        // `Active`. Index the active superseders by their target (owned so
        // the borrow drops before `rows` is consumed below).
        let active_supersede_targets: std::collections::HashSet<String> = rows
            .iter()
            .filter(|r| matches!(r.status, CapabilityCapsuleStatus::Active))
            .filter_map(|r| r.supersedes_capability_capsule_id.clone())
            .collect();

        let mut out: Vec<CapabilityCapsuleRecord> = rows
            .into_iter()
            .filter(|r| !is_excluded_status(&r.status))
            .filter(|r| !is_diary(&r.capability_capsule_type))
            .filter(|r| !active_supersede_targets.contains(r.capability_capsule_id.as_str()))
            .filter(|r| match &bound_ids {
                // Cap on: keep guidance always, plus the recency allow-list.
                Some(allow) => {
                    is_guidance(&r.capability_capsule_type)
                        || allow.contains(&r.capability_capsule_id)
                }
                None => true,
            })
            .collect();

        // ORDER BY updated_at DESC, version DESC, capability_capsule_id ASC.
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.version.cmp(&a.version))
                .then_with(|| a.capability_capsule_id.cmp(&b.capability_capsule_id))
        });
        Ok(out)
    }

    /// Route-B bucket "version-chain": native lancedb-Rust equivalent of
    /// `DuckDbQuery::list_capability_capsule_versions_for_tenant` — the
    /// supersede version-chain walk.
    ///
    /// Replaces the DuckDB `WITH RECURSIVE` CTE with an iterative BFS in
    /// Rust over the tenant's rows. The CTE walks **both** directions of
    /// the `supersedes_capability_capsule_id` link from every node already
    /// in the chain:
    ///   - BACKWARD: a row `c` joins when `c.id = ch.supersedes` (the
    ///     predecessor the current node points at).
    ///   - FORWARD: a row `c` joins when `c.supersedes = ch.id` (a
    ///     successor pointing back at the current node).
    ///
    /// Tenant filters every step, so foreign-tenant id collisions never
    /// leak in. Returns each link as a [`CapabilityCapsuleVersionLink`]
    /// ordered `version DESC, updated_at DESC` (newest first) — identical
    /// to the DuckDB `ORDER BY`. Parity-gated by `tests/parity_golden.rs`.
    pub async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        // Pull the tenant's rows once and index by id; the chain walk is a
        // pure in-memory BFS (chains are tiny — typically 1–3 links).
        let rows = self
            .query_capability_capsules(format!("tenant = {}", sql_quote(tenant)), None)
            .await?;
        let by_id: std::collections::HashMap<&str, &CapabilityCapsuleRecord> = rows
            .iter()
            .map(|r| (r.capability_capsule_id.as_str(), r))
            .collect();
        // Forward adjacency: capsule_id -> ids of rows that supersede it.
        let mut successors: std::collections::HashMap<&str, Vec<&str>> =
            std::collections::HashMap::new();
        for r in &rows {
            if let Some(pred) = r.supersedes_capability_capsule_id.as_deref() {
                successors
                    .entry(pred)
                    .or_default()
                    .push(r.capability_capsule_id.as_str());
            }
        }

        // BFS from the anchor (if it exists in this tenant). The recursive
        // CTE's anchor row is only emitted when it matches tenant + id, so an
        // absent / foreign-tenant id yields an empty chain — same as DuckDB.
        let mut visited: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        if by_id.contains_key(capability_capsule_id) {
            visited.insert(capability_capsule_id);
            queue.push_back(capability_capsule_id);
        }
        while let Some(id) = queue.pop_front() {
            let rec = match by_id.get(id) {
                Some(r) => r,
                None => continue,
            };
            // BACKWARD: the predecessor this row supersedes.
            if let Some(pred) = rec.supersedes_capability_capsule_id.as_deref() {
                if by_id.contains_key(pred) && visited.insert(pred) {
                    queue.push_back(pred);
                }
            }
            // FORWARD: rows that supersede this row.
            if let Some(succs) = successors.get(id) {
                for &succ in succs {
                    if visited.insert(succ) {
                        queue.push_back(succ);
                    }
                }
            }
        }

        let mut out: Vec<CapabilityCapsuleVersionLink> = visited
            .iter()
            .filter_map(|id| by_id.get(id))
            .map(|r| CapabilityCapsuleVersionLink {
                capability_capsule_id: r.capability_capsule_id.clone(),
                version: r.version,
                status: r.status.clone(),
                updated_at: r.updated_at.clone(),
                supersedes_capability_capsule_id: r.supersedes_capability_capsule_id.clone(),
            })
            .collect();
        // ORDER BY version DESC, updated_at DESC (newest first).
        out.sort_by(|a, b| {
            b.version
                .cmp(&a.version)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        Ok(out)
    }

    /// Read all `embedding_jobs` rows matching `filter`, parsed into
    /// [`EmbeddingJobRow`]s. Shared by every queue read path: the claim
    /// flow, `list_embedding_jobs`, `latest_embedding_job_status_for_hash`,
    /// and the duplicate-detection in `try_enqueue_embedding_job`.
    pub(crate) async fn query_embedding_jobs(
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

    /// Route-B native equivalent of `DuckDbQuery::get_embedding_job_status`:
    /// read the `status` column of an `embedding_jobs` row by `job_id`,
    /// or `None` when the row is gone. `job_id` is the table's "PK"
    /// (one row per id), so the filtered scan yields at most one row.
    pub async fn get_embedding_job_status(
        &self,
        job_id: &str,
    ) -> Result<Option<String>, StorageError> {
        let rows = self
            .query_embedding_jobs(format!("job_id = {}", sql_quote(job_id)))
            .await?;
        Ok(rows.into_iter().next().map(|r| r.status))
    }
}

/// Memory CRUD + filter + embedding-job + feedback methods —
/// previously bound by the `MemoryRepository` trait, now plain
/// inherent. Methods kept as `pub async fn`; signatures unchanged.
#[allow(clippy::too_many_arguments)]
impl LanceStore {
    pub async fn insert_capability_capsule(
        &self,
        memory: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let table = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = capability_capsules_to_record_batch(std::slice::from_ref(&memory))?;
        // `RecordBatch` impls `Scannable` directly — no need to wrap in an
        // iterator. (Re-checking lancedb-0.27.2/src/data/scannable.rs L70.)
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(memory)
    }

    /// Multi-row insert. One Arrow `RecordBatch` carrying every row, one
    /// `table.add` call. No-op when `memories` is empty (avoids minting an
    /// empty batch). Caller is responsible for upstream dedup
    /// (`find_by_idempotency_or_hash`) — this method does not perform it.
    pub async fn insert_capability_capsules_batch(
        &self,
        memories: &[CapabilityCapsuleRecord],
    ) -> Result<(), StorageError> {
        if memories.is_empty() {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = capability_capsules_to_record_batch(memories)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(())
    }

    /// Multi-row enqueue for `embedding_jobs`. One `table.add` of an
    /// N-row `RecordBatch`, no per-row idempotency probe. Caller must
    /// ensure the inserts target *fresh* capsules (just-inserted by
    /// `insert_capability_capsules_batch`) so no live (pending |
    /// processing) row can yet exist for the
    /// `(tenant, capability_capsule_id, target_content_hash, provider)`
    /// tuple — the same invariant the single-row variant relies on at
    /// the application level. No-op when `inserts` is empty.
    pub async fn enqueue_embedding_jobs_batch(
        &self,
        inserts: &[EmbeddingJobInsert],
    ) -> Result<(), StorageError> {
        if inserts.is_empty() {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("embedding_jobs")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let rows: Vec<EmbeddingJobRow> = inserts
            .iter()
            .map(|insert| EmbeddingJobRow {
                job_id: insert.job_id.clone(),
                tenant: insert.tenant.clone(),
                capability_capsule_id: insert.capability_capsule_id.clone(),
                target_content_hash: insert.target_content_hash.clone(),
                provider: insert.provider.clone(),
                status: "pending".to_string(),
                attempt_count: 0,
                last_error: None,
                available_at: insert.available_at.clone(),
                created_at: insert.created_at.clone(),
                updated_at: insert.updated_at.clone(),
            })
            .collect();
        let batch = embedding_job_rows_to_record_batch(&rows)?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn try_enqueue_embedding_job(
        &self,
        insert: EmbeddingJobInsert,
    ) -> Result<bool, StorageError> {
        // Idempotency check: if any live (pending/processing) row already
        // covers this (tenant, capability_capsule_id, target_content_hash, provider)
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
                "tenant = {} AND capability_capsule_id = {} AND target_content_hash = {} \
                 AND provider = {} AND (status = 'pending' OR status = 'processing')",
                sql_quote(&insert.tenant),
                sql_quote(&insert.capability_capsule_id),
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
            capability_capsule_id: insert.capability_capsule_id,
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

    pub async fn claim_next_n_embedding_jobs(
        &self,
        now: &str,
        max_retries: u32,
        n: usize,
    ) -> Result<Vec<ClaimedEmbeddingJob>, StorageError> {
        if n == 0 {
            return Ok(vec![]);
        }
        // Eligible = available_at <= now AND (pending OR (failed AND
        // attempt_count < max_retries) OR (processing AND lease expired)).
        // LanceDB has no ORDER BY, so we pull all eligible rows and sort by
        // (available_at, created_at) ASC in memory before slicing — queue
        // depth is expected to be small (worker drains continuously) so the
        // in-memory cost is negligible vs. the simpler code.
        //
        // The `processing AND updated_at <= now - lease` disjunct reclaims
        // ORPHANED in-flight jobs: a worker crash, a process restart
        // mid-embed, or a mid-batch error (`tick` aborts the rest of the
        // claimed batch on a transient storage error) leaves a row stuck in
        // `processing`. Without this it would never be re-claimed — the
        // capsule silently loses its embedding forever, and `try_enqueue`
        // can't re-create the job because a live `processing` row blocks it.
        // The lease is a visibility timeout (EMBEDDING_JOB_LEASE_MS), far
        // above real embed latency so a genuinely in-flight job is never
        // stolen. (This is unrelated to the DuckDB FK-loop orphan sweep,
        // which guarded against deleted-memory orphans — that pathology
        // can't occur on LanceDB, but worker-interruption orphans can.)
        let max_r = i64::from(max_retries);
        let lease_cutoff = timestamp_sub_ms(now, EMBEDDING_JOB_LEASE_MS);
        let filter = format!(
            "available_at <= {now} AND (status = 'pending' \
             OR (status = 'failed' AND attempt_count < {max_r}) \
             OR (status = 'processing' AND updated_at <= {cutoff}))",
            now = sql_quote(now),
            cutoff = sql_quote(&lease_cutoff),
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
            // (pending, failed-with-budget, or a lease-expired processing
            // orphan). A second-instance race would see rows_updated == 0 and
            // we'd skip the row — same shape as DuckDB's "updated == 0 →
            // return None" branch. Setting updated_at = now renews the lease,
            // so a reclaimed orphan isn't immediately re-stolen.
            let result = table
                .update()
                .only_if(format!(
                    "job_id = {job} AND (status = 'pending' \
                     OR (status = 'failed' AND attempt_count < {max_r}) \
                     OR (status = 'processing' AND updated_at <= {cutoff}))",
                    job = sql_quote(&r.job_id),
                    cutoff = sql_quote(&lease_cutoff),
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
                capability_capsule_id: r.capability_capsule_id,
                target_content_hash: r.target_content_hash,
                provider: r.provider,
                attempt_count: r.attempt_count,
            });
        }
        Ok(claimed)
    }

    pub async fn upsert_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
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
        let vector = decode_f32_blob(embedding_blob, embedding_dim as usize)
            .map_err(StorageError::InvalidData)?;

        ensure_capability_capsule_embeddings_table(&self.conn, dim_i32).await?;

        let table = self
            .conn
            .open_table("capability_capsule_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        // upsert = delete-then-insert. LanceDB has no PK enforcement so
        // we sweep any existing row for this capability_capsule_id first.
        table
            .delete(&format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            ))
            .await
            .map_err(lancedb_err)?;
        let batch = capability_capsule_embedding_to_record_batch(
            capability_capsule_id,
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

    /// ③ chunked: delete all existing embedding rows for the capsule
    /// once, then insert one row per chunk vector. Vectors share
    /// `capability_capsule_id`; search dedups via GROUP BY. Takes raw
    /// `Vec<f32>` (no blob decode — the worker has the vectors already).
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_capability_capsule_embedding_chunks(
        &self,
        capability_capsule_id: &str,
        tenant: &str,
        embedding_model: &str,
        embedding_dim: i64,
        vectors: &[Vec<f32>],
        content_hash: &str,
        source_updated_at: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        if vectors.is_empty() {
            return Ok(());
        }
        let dim_i32 = i32::try_from(embedding_dim)
            .map_err(|_| StorageError::InvalidData("embedding_dim does not fit in i32"))?;
        ensure_capability_capsule_embeddings_table(&self.conn, dim_i32).await?;
        let table = self
            .conn
            .open_table("capability_capsule_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            ))
            .await
            .map_err(lancedb_err)?;
        for vector in vectors {
            let batch = capability_capsule_embedding_to_record_batch(
                capability_capsule_id,
                tenant,
                embedding_model,
                embedding_dim,
                vector,
                content_hash,
                source_updated_at,
                now,
            )?;
            table.add(batch).execute().await.map_err(lancedb_err)?;
        }
        Ok(())
    }

    pub async fn delete_capability_capsule_embedding(
        &self,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        // No-op if the table doesn't exist yet (semantic search hasn't
        // been used; nothing to delete).
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "capability_capsule_embeddings") {
            return Ok(());
        }
        let table = self
            .conn
            .open_table("capability_capsule_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .delete(&format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            ))
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn complete_embedding_job(
        &self,
        job_id: &str,
        now: &str,
    ) -> Result<(), StorageError> {
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

    pub async fn mark_embedding_job_stale(
        &self,
        job_id: &str,
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
            .column("status", "'stale'")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
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

    pub async fn permanently_fail_embedding_job(
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

    /// Delete every `feedback_events` row referencing this capsule.
    /// Cascade helper called by [`Self::delete_capability_capsule_hard`].
    /// Returns the number of rows removed (pre-count is canonical
    /// because Lance servers older than this codebase may report 0
    /// in `num_deleted_rows`).
    pub async fn delete_feedback_events_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
    ) -> Result<usize, StorageError> {
        let table = self
            .conn
            .open_table("feedback_events")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let count = table
            .count_rows(Some(format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            )))
            .await
            .map_err(lancedb_err)?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .delete(&format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            ))
            .await
            .map_err(lancedb_err)?;
        if result.num_deleted_rows == 0 {
            Ok(count)
        } else {
            Ok(usize::try_from(result.num_deleted_rows).unwrap_or(count))
        }
    }

    pub async fn delete_embedding_jobs_by_capability_capsule_id(
        &self,
        capability_capsule_id: &str,
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
            .count_rows(Some(format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            )))
            .await
            .map_err(lancedb_err)?;
        if count == 0 {
            return Ok(0);
        }
        let result = table
            .delete(&format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            ))
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

    pub async fn get_capability_capsule_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        let table = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let filter = format!(
            "tenant = {} AND capability_capsule_id = {}",
            sql_quote(tenant),
            sql_quote(capability_capsule_id),
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
            let mems = record_batch_to_capability_capsules(batch)?;
            if let Some(m) = mems.into_iter().next() {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    /// Set status from a [`CapabilityCapsuleStatus`] enum. The single
    /// transition primitive — `accept_pending` / `reject_pending` and
    /// the O2 review-flag path call this. Routes through
    /// [`Self::update_status`] (lance `.update()` with a tenant+id
    /// filter; reliable `rows_updated`).
    pub async fn set_capsule_status(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        status: crate::domain::capability_capsule::CapabilityCapsuleStatus,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let status_str = enum_to_str(&status)?;
        self.update_status(tenant, capability_capsule_id, &status_str)
            .await
    }

    pub async fn replace_pending_with_successor(
        &self,
        tenant: &str,
        original_memory_id: &str,
        successor: CapabilityCapsuleRecord,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        // Two-step supersede: archive the old row, then insert the new
        // one. LanceDB has no transaction semantics across these calls,
        // so a crash between them can leave the old archived without
        // a successor. The atomicity contract is documented on the
        // trait (see `CapsuleStore::replace_pending_with_successor`
        // — Phase 5 pain #4): backends MAY use real transactions
        // (Postgres does), but the trait does NOT guarantee atomic
        // commit. Callers are spec'd to tolerate partial state.
        let table = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let now = crate::storage::current_timestamp();
        table
            .update()
            .only_if(format!(
                "tenant = {} AND capability_capsule_id = {}",
                sql_quote(tenant),
                sql_quote(original_memory_id),
            ))
            .column("status", "'rejected'")
            .column("updated_at", sql_quote(&now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batch = capability_capsules_to_record_batch(std::slice::from_ref(&successor))?;
        table.add(batch).execute().await.map_err(lancedb_err)?;
        Ok(successor)
    }

    pub async fn apply_feedback(
        &self,
        memory: &CapabilityCapsuleRecord,
        feedback: FeedbackEvent,
    ) -> Result<CapabilityCapsuleRecord, StorageError> {
        let kind =
            crate::domain::capability_capsule::FeedbackKind::from_db_str(&feedback.feedback_kind)
                .ok_or(StorageError::InvalidData("invalid feedback kind"))?;
        let status_after = kind.status_after();
        let updated_at = feedback.created_at.clone();
        let mut updated = memory.clone();
        updated.updated_at = updated_at.clone();
        updated.confidence = (updated.confidence + kind.confidence_delta()).clamp(0.0, 1.0);
        updated.decay_score = (updated.decay_score + kind.decay_delta()).clamp(0.0, 1.0);
        if let Some(ref s) = status_after {
            updated.status = s.clone();
        }
        if kind.marks_validated() {
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
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut update = mem_table
            .update()
            .only_if(format!(
                "capability_capsule_id = {}",
                sql_quote(&updated.capability_capsule_id)
            ))
            .column("confidence", format!("{}", updated.confidence))
            .column("decay_score", format!("{}", updated.decay_score))
            .column("updated_at", sql_quote(&updated.updated_at));
        if let Some(s) = status_after {
            update = update.column("status", sql_quote(&enum_to_str(&s)?));
        }
        if kind.marks_validated() {
            update = update.column("last_validated_at", sql_quote(&updated_at));
        }
        update.execute().await.map_err(lancedb_err)?;
        Ok(updated)
    }

    pub async fn list_feedback_for_memory(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Vec<FeedbackEvent>, StorageError> {
        let table = self
            .conn
            .open_table("feedback_events")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
            ))
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

    pub async fn feedback_summary(
        &self,
        capability_capsule_id: &str,
    ) -> Result<FeedbackSummary, StorageError> {
        // Fetch all events for this memory and aggregate client-side.
        // Counts are tiny (events per memory typically < 10), so the
        // network/parse cost is negligible compared to running a
        // GROUP BY query through LanceDB's filter API.
        let events = self.list_feedback_for_memory(capability_capsule_id).await?;
        let mut summary = FeedbackSummary::default();
        for e in events {
            summary.total += 1;
            match e.feedback_kind.as_str() {
                "useful" => summary.useful += 1,
                "outdated" => summary.outdated += 1,
                "incorrect" => summary.incorrect += 1,
                "applies_here" => summary.applies_here += 1,
                "does_not_apply_here" => summary.does_not_apply_here += 1,
                "auto_promoted" => summary.auto_promoted += 1,
                _ => {} // future kinds — counted in `total` only
            }
        }
        Ok(summary)
    }

    /// Hard-delete a capsule + its satellite rows in 4 dependent
    /// tables. Order:
    ///
    /// 1. `capability_capsules` row (also serves as
    ///    existence-check — `InvalidData("memory not found")` when
    ///    the row isn't there, no satellite work attempted).
    /// 2. `feedback_events` rows referencing this capsule_id.
    /// 3. `embedding_jobs` rows referencing this capsule_id.
    /// 4. `capability_capsule_embeddings` row (one per capsule).
    /// 5. `graph_edges` rows where this capsule is the FROM node —
    ///    these are *closed* (`valid_to = now`) rather than deleted,
    ///    preserving the time-travel graph history per the
    ///    `valid_from / valid_to` schema. Forward-facing edges
    ///    pointing AT this capsule from elsewhere are NOT
    ///    auto-handled (no `to_node_id`-rooted close helper today);
    ///    they survive as dangling pointers — accepted as the
    ///    cheaper trade-off vs. running a tenant-wide scan on every
    ///    hard-delete.
    ///
    /// **Atomicity contract** (same as
    /// `CapsuleStore::replace_pending_with_successor`,
    /// `CapsuleStore::apply_feedback` — Phase 5 pain #4): LanceDB has
    /// no cross-table transaction, so a crash between steps 1 and 5
    /// leaves the capsule gone but one or more satellite tables
    /// still holding orphans. Re-running the call is safe — every
    /// cascade helper is idempotent (delete-from-empty-set is a
    /// no-op + step 1 returns NotFound) so the caller can retry
    /// until it returns NotFound to confirm clean state.
    pub async fn delete_capability_capsule_hard(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<(), StorageError> {
        let table = self
            .conn
            .open_table("capability_capsules")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let result = table
            .delete(&format!(
                "tenant = {} AND capability_capsule_id = {}",
                sql_quote(tenant),
                sql_quote(capability_capsule_id),
            ))
            .await
            .map_err(lancedb_err)?;
        if result.num_deleted_rows == 0 {
            return Err(StorageError::InvalidData("memory not found"));
        }
        // Cascade. Each helper is idempotent on empty-set inputs.
        // Errors propagate so the caller observes partial-state
        // failures; per the atomicity contract, retry of the same
        // hard-delete call after a cascade failure is safe.
        self.delete_feedback_events_by_capability_capsule_id(capability_capsule_id)
            .await?;
        self.delete_embedding_jobs_by_capability_capsule_id(capability_capsule_id)
            .await?;
        self.delete_capability_capsule_embedding(capability_capsule_id)
            .await?;
        self.close_edges_for_capability_capsule(capability_capsule_id)
            .await
            .map_err(|e| StorageError::InvalidInput(format!("close edges: {e}")))?;
        Ok(())
    }

    pub async fn get_capability_capsule(
        &self,
        capability_capsule_id: String,
    ) -> Result<Option<CapabilityCapsuleRecord>, StorageError> {
        // Cross-tenant lookup (admin / version-chain path). DuckDB does the
        // same — filters only on capability_capsule_id.
        let filter = format!(
            "capability_capsule_id = {}",
            sql_quote(&capability_capsule_id)
        );
        Ok(self
            .query_capability_capsules(filter, Some(1))
            .await?
            .into_iter()
            .next())
    }

    pub async fn list_embedding_jobs(
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
            filter.push_str(&format!(" AND capability_capsule_id = {}", sql_quote(m)));
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
                capability_capsule_id: r.capability_capsule_id,
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

    pub async fn stale_live_embedding_jobs_for_capability_capsule(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
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
            "tenant = {} AND capability_capsule_id = {} AND provider = {} \
             AND (status = 'pending' OR status = 'processing')",
            sql_quote(tenant),
            sql_quote(capability_capsule_id),
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

    pub async fn get_capability_capsule_embedding_row(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<(String, String, String)>, StorageError> {
        // No capability_capsule_embeddings table yet → no row by definition.
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "capability_capsule_embeddings") {
            return Ok(None);
        }
        let table = self
            .conn
            .open_table("capability_capsule_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
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
            const TABLE: &str = "capability_capsule_embeddings";
            let model = parse_col::<StringArray>(b, TABLE, "embedding_model")?;
            let hash = parse_col::<StringArray>(b, TABLE, "content_hash")?;
            let updated = parse_col::<StringArray>(b, TABLE, "updated_at")?;
            return Ok(Some((
                model.value(0).to_string(),
                hash.value(0).to_string(),
                updated.value(0).to_string(),
            )));
        }
        Ok(None)
    }

    /// Read the raw embedding vector for `capability_capsule_id`.
    /// Returns `None` when (a) the embeddings table hasn't been
    /// created yet (semantic search never used), or (b) no row exists
    /// for this id. Added for the dedup worker, which needs vectors to
    /// compute pairwise cosine — `get_capability_capsule_embedding_row`
    /// only exposes the metadata triple `(model, hash, updated_at)`.
    pub async fn get_capability_capsule_embedding_vector(
        &self,
        capability_capsule_id: &str,
    ) -> Result<Option<Vec<f32>>, StorageError> {
        let names = self
            .conn
            .table_names()
            .execute()
            .await
            .map_err(lancedb_err)?;
        if !names.iter().any(|n| n == "capability_capsule_embeddings") {
            return Ok(None);
        }
        let table = self
            .conn
            .open_table("capability_capsule_embeddings")
            .execute()
            .await
            .map_err(lancedb_err)?;
        let stream = table
            .query()
            .only_if(format!(
                "capability_capsule_id = {}",
                sql_quote(capability_capsule_id)
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
            // `embedding` is a FixedSizeList<Float32, dim>; extract the
            // single row's underlying Float32Array values.
            let fsl = parse_col::<arrow_array::FixedSizeListArray>(
                b,
                "capability_capsule_embeddings",
                "embedding",
            )?;
            let values = fsl.value(0);
            let floats = values
                .as_any()
                .downcast_ref::<Float32Array>()
                .ok_or_else(|| {
                    tracing::error!(
                        table = "capability_capsule_embeddings",
                        column = "embedding",
                        "FixedSizeList inner is not Float32Array",
                    );
                    StorageError::InvalidData("embedding inner not Float32")
                })?;
            return Ok(Some(floats.values().to_vec()));
        }
        Ok(None)
    }

    pub async fn latest_embedding_job_status_for_hash(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
        target_content_hash: &str,
    ) -> Result<Option<String>, StorageError> {
        let mut rows = self
            .query_embedding_jobs(format!(
                "tenant = {} AND capability_capsule_id = {} AND target_content_hash = {}",
                sql_quote(tenant),
                sql_quote(capability_capsule_id),
                sql_quote(target_content_hash),
            ))
            .await?;
        // ORDER BY updated_at DESC LIMIT 1.
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(rows.into_iter().next().map(|r| r.status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use crate::storage::types::EmbeddingJobInsert;
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

    #[tokio::test]
    pub async fn lancedb_insert_and_get_memory_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.expect("open lancedb store");

        let memory = fixture("mem_lance_001", "tenant-a");
        repo.insert_capability_capsule(memory.clone())
            .await
            .expect("insert_capability_capsule");

        let got = repo
            .get_capability_capsule_for_tenant("tenant-a", "mem_lance_001")
            .await
            .expect("get_capability_capsule_for_tenant")
            .expect("memory should exist");

        assert_eq!(got.capability_capsule_id, memory.capability_capsule_id);
        assert_eq!(got.tenant, memory.tenant);
        assert_eq!(got.capability_capsule_type, memory.capability_capsule_type);
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
            .get_capability_capsule_for_tenant("tenant-a", "does-not-exist")
            .await
            .expect("missing query");
        assert!(missing.is_none());

        // Cross-tenant filter must not leak.
        let wrong_tenant = repo
            .get_capability_capsule_for_tenant("tenant-b", "mem_lance_001")
            .await
            .expect("cross-tenant query");
        assert!(wrong_tenant.is_none());
    }

    // The previous `lancedb_filter_methods_round_trip` test that
    // lived here was deleted along with the lance-side filter
    // readers (`list_capability_capsules_for_tenant`, `get_pending`,
    // `find_by_idempotency_or_hash`, `list_pending_review`,
    // `search_candidates`, `recent_active_capability_capsules`,
    // `fetch_capability_capsules_by_ids`,
    // `list_capability_capsule_versions_for_tenant`,
    // `list_capability_capsule_ids_for_tenant`). The canonical reads
    // are on `DuckDbQuery` and validated by
    // `src/storage/duckdb_query/capability_capsules.rs::tests`, which
    // exercises every filter + the version-chain walk against
    // LanceStore-written data through the DuckDB-extension path.

    /// Mutating-method round-trip: accept_pending, reject_pending,
    /// replace_pending_with_successor, delete_capability_capsule_hard.
    #[tokio::test]
    pub async fn lancedb_mutating_methods_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let mut p = fixture("mem_p", "tenant");
        p.status = CapabilityCapsuleStatus::PendingConfirmation;
        let mut q = fixture("mem_q", "tenant");
        q.status = CapabilityCapsuleStatus::PendingConfirmation;
        let r = fixture("mem_r", "tenant");
        let s = fixture("mem_s", "tenant");
        for m in [&p, &q, &r, &s] {
            repo.insert_capability_capsule(m.clone()).await.unwrap();
        }

        // accept_pending → status active
        let accepted = repo
            .set_capsule_status("tenant", "mem_p", CapabilityCapsuleStatus::Active)
            .await
            .unwrap();
        assert_eq!(accepted.status, CapabilityCapsuleStatus::Active);
        assert_eq!(accepted.capability_capsule_id, "mem_p");

        // reject_pending → status rejected
        let rejected = repo
            .set_capsule_status("tenant", "mem_q", CapabilityCapsuleStatus::Rejected)
            .await
            .unwrap();
        assert_eq!(rejected.status, CapabilityCapsuleStatus::Rejected);

        // (The previous `list_pending_review` follow-up assertion was
        // dropped along with the lance-side reader — the
        // accept/reject status assertions above are the canonical
        // check; the queue-emptiness invariant is covered in
        // `src/storage/duckdb_query/capability_capsules.rs::tests`.)

        // replace_pending_with_successor: archive r, insert successor
        let mut succ = fixture("mem_r_v2", "tenant");
        succ.supersedes_capability_capsule_id = Some("mem_r".into());
        succ.version = 2;
        let returned = repo
            .replace_pending_with_successor("tenant", "mem_r", succ.clone())
            .await
            .unwrap();
        assert_eq!(returned.capability_capsule_id, "mem_r_v2");
        let archived = repo
            .get_capability_capsule_for_tenant("tenant", "mem_r")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.status, CapabilityCapsuleStatus::Rejected);
        let successor_row = repo
            .get_capability_capsule_for_tenant("tenant", "mem_r_v2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            successor_row.supersedes_capability_capsule_id,
            Some("mem_r".into())
        );
        assert_eq!(successor_row.version, 2);

        // delete_capability_capsule_hard
        repo.delete_capability_capsule_hard("tenant", "mem_s")
            .await
            .unwrap();
        let gone = repo
            .get_capability_capsule_for_tenant("tenant", "mem_s")
            .await
            .unwrap();
        assert!(gone.is_none());

        // delete on non-existent → NotFound-equivalent error
        let err = repo
            .delete_capability_capsule_hard("tenant", "does-not-exist")
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
        repo.insert_capability_capsule(memory.clone())
            .await
            .unwrap();

        // Apply 3 feedbacks of different kinds
        let make = |kind: &str, ts: &str, suffix: &str| FeedbackEvent {
            feedback_id: format!("fb_{suffix}"),
            capability_capsule_id: memory.capability_capsule_id.clone(),
            feedback_kind: kind.into(),
            created_at: ts.into(),
            note: None,
        };
        let _ = repo
            .apply_feedback(&memory, make("useful", "2026-05-08T01:00:00Z", "1"))
            .await
            .unwrap();
        let after_useful = repo
            .get_capability_capsule_for_tenant("tenant", "mem_fb")
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
            .get_capability_capsule_for_tenant("tenant", "mem_fb")
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
            .get_capability_capsule_for_tenant("tenant", "mem_fb")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after_incorrect.status,
            CapabilityCapsuleStatus::Archived,
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
            repo.insert_capability_capsule(m.clone()).await.unwrap();
        }

        // Enqueue: first call creates, second is idempotent (dup detected).
        let insert1 = EmbeddingJobInsert {
            job_id: "job_1".into(),
            tenant: "tenant-a".into(),
            capability_capsule_id: "mem_q1".into(),
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

        let status = repo
            .latest_embedding_job_status_for_hash("tenant-a", "mem_q1", "hash_q1")
            .await
            .unwrap();
        assert_eq!(status.as_deref(), Some("pending"));

        // Add a second job (different memory) so claim ordering is testable.
        let insert2 = EmbeddingJobInsert {
            job_id: "job_2".into(),
            tenant: "tenant-a".into(),
            capability_capsule_id: "mem_q2".into(),
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

        // capability_capsule_id filter.
        let only_q1 = repo
            .list_embedding_jobs("tenant-a", None, Some("mem_q1"), 50)
            .await
            .unwrap();
        assert_eq!(only_q1.len(), 1);
        assert_eq!(only_q1[0].capability_capsule_id, "mem_q1");

        // stale_live: enqueue a fresh pending row, then sweep it stale.
        let insert3 = EmbeddingJobInsert {
            job_id: "job_3".into(),
            tenant: "tenant-a".into(),
            capability_capsule_id: "mem_q1".into(),
            target_content_hash: "hash_q1_v2".into(),
            provider: "fake-test".into(),
            available_at: "00000001778000080000".into(),
            created_at: "00000001778000080000".into(),
            updated_at: "00000001778000080000".into(),
        };
        repo.try_enqueue_embedding_job(insert3).await.unwrap();
        let staled = repo
            .stale_live_embedding_jobs_for_capability_capsule(
                "tenant-a",
                "mem_q1",
                "fake-test",
                "00000001778000090000",
            )
            .await
            .unwrap();
        assert_eq!(staled, 1);

        let deleted = repo
            .delete_embedding_jobs_by_capability_capsule_id("mem_q1")
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        let remaining = repo
            .list_embedding_jobs("tenant-a", None, None, 50)
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].capability_capsule_id, "mem_q2");

        // delete on no-row → 0.
        let zero = repo
            .delete_embedding_jobs_by_capability_capsule_id("nope")
            .await
            .unwrap();
        assert_eq!(zero, 0);
    }

    /// HIGH-bug regression: a job left in `processing` (worker crash, process
    /// restart mid-embed, or a mid-batch error abandoning the rest of the
    /// claimed batch) must be reclaimable once its lease elapses — but NOT
    /// before. Without lease-reclaim the claim filter never re-matches a
    /// `processing` row, so the orphan silently loses its embedding forever
    /// and `try_enqueue` can't re-create it (a live `processing` row blocks it).
    #[tokio::test]
    pub async fn claim_reclaims_orphaned_processing_jobs_after_lease() {
        use crate::storage::{timestamp_add_ms, EMBEDDING_JOB_LEASE_MS};

        let dir = tempdir().unwrap();
        let path = dir.path().join("lance.store");
        let repo = LanceStore::open(&path).await.unwrap();

        let m = fixture("mem_orph", "tenant-a");
        repo.insert_capability_capsule(m).await.unwrap();

        let claimed_at = "00000001778000000000";
        repo.try_enqueue_embedding_job(EmbeddingJobInsert {
            job_id: "job_orph".into(),
            tenant: "tenant-a".into(),
            capability_capsule_id: "mem_orph".into(),
            target_content_hash: "hash_orph".into(),
            provider: "fake-test".into(),
            available_at: claimed_at.into(),
            created_at: claimed_at.into(),
            updated_at: claimed_at.into(),
        })
        .await
        .unwrap();

        // First claim → job goes to `processing`, updated_at = claimed_at.
        let first = repo
            .claim_next_n_embedding_jobs(claimed_at, 5, 5)
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].job_id, "job_orph");

        // Re-claim WITHIN the lease window: the job is still legitimately
        // in-flight, so it must NOT be stolen.
        let within_lease = timestamp_add_ms(claimed_at, EMBEDDING_JOB_LEASE_MS - 1);
        let none = repo
            .claim_next_n_embedding_jobs(&within_lease, 5, 5)
            .await
            .unwrap();
        assert!(
            none.is_empty(),
            "a job within its lease must not be reclaimed"
        );

        // Re-claim AFTER the lease elapses: the orphan is reclaimed, and its
        // attempt_count is unchanged (it was interrupted, not failed).
        let past_lease = timestamp_add_ms(claimed_at, EMBEDDING_JOB_LEASE_MS + 1);
        let reclaimed = repo
            .claim_next_n_embedding_jobs(&past_lease, 5, 5)
            .await
            .unwrap();
        assert_eq!(
            reclaimed.len(),
            1,
            "orphaned processing job must be reclaimed after the lease"
        );
        assert_eq!(reclaimed[0].job_id, "job_orph");
        assert_eq!(reclaimed[0].attempt_count, 0);
    }
}
