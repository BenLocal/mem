//! `CapsuleSearchStore` for the ClickHouse backend.
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P4).**
//!
//! Hybrid recall =
//! - `ann_candidate_ids`: brute `cosineDistance()` over the
//!   `embedding Array(Float32)` column, chunk-collapsed (`GROUP BY
//!   capability_capsule_id`, `min(dist)`), POSTFILTER on tenant (top-k
//!   across all tenants first, then drop foreign — matches the lance
//!   `nearest_to` postfilter quirk). The experimental `vector_similarity`
//!   HNSW index is a scale optimization, not P4.
//! - `bm25_candidate_ids`: a **coarse lexical candidate channel** —
//!   ClickHouse has no BM25 scoring, so this is a case-insensitive
//!   substring match (`positionCaseInsensitiveUTF8`) over `content`,
//!   live-filtered like the lance path. Rank = ClickHouse-returned
//!   position (RRF fuses by RANK, not score). CJK / token recall is
//!   weak vs the jieba-Tantivy path — parity here is **soft** only
//!   (overlap@10), the semantic channel backs it up. See §4(e) / §10.
//! - `hybrid_candidates_compose`: identical Rust shape to
//!   `Store::hybrid_candidates_compose` (oversample → `rrf_merge` →
//!   hydrate → status/diary post-filter → sort), reusing
//!   `pipeline::ranking::rrf_merge`.
//!
//! The pool / recent / version reads load the tenant's
//! `capability_capsules` rows via `FINAL` and run the SAME Rust-side
//! filters the lance backend uses (lifecycle-pool dedup + version-chain
//! BFS), so they match field-for-field.

use std::collections::{HashMap, HashSet, VecDeque};

use async_trait::async_trait;
use clickhouse::Row;
use serde::Deserialize;

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, ChCapsuleRow};
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
    CapabilityCapsuleVersionLink,
};
use crate::storage::capsule_search_store::CapsuleSearchStore;
use crate::storage::capsule_store::CapsuleStore;
use crate::storage::types::StorageError;

#[derive(Row, Deserialize)]
struct IdRow {
    capability_capsule_id: String,
}

fn is_excluded_status(s: &CapabilityCapsuleStatus) -> bool {
    matches!(
        s,
        CapabilityCapsuleStatus::Rejected | CapabilityCapsuleStatus::Archived
    )
}
fn is_diary(t: &CapabilityCapsuleType) -> bool {
    matches!(t, CapabilityCapsuleType::Diary)
}
fn is_guidance(t: &CapabilityCapsuleType) -> bool {
    matches!(
        t,
        CapabilityCapsuleType::Preference | CapabilityCapsuleType::Workflow
    )
}

impl ClickHouseBackend {
    /// Load every `capability_capsules` row for `tenant` (latest version
    /// each via `FINAL`), parsed to records — the shared base for the
    /// Rust-side pool / recent / version reads below.
    async fn tenant_capsule_records(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        let rows = self
            .client
            .query("SELECT ?fields FROM capability_capsules FINAL WHERE tenant = ?")
            .bind(tenant)
            .fetch_all::<ChCapsuleRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(ChCapsuleRow::into_record).collect())
    }

    /// Ids superseded by an `Active` row in the same tenant — dropped from
    /// the live pool (the supersede dedup, verbatim from the lance impl).
    fn active_supersede_targets(rows: &[CapabilityCapsuleRecord]) -> HashSet<String> {
        rows.iter()
            .filter(|r| matches!(r.status, CapabilityCapsuleStatus::Active))
            .filter_map(|r| r.supersedes_capability_capsule_id.clone())
            .collect()
    }
}

#[async_trait]
impl CapsuleSearchStore for ClickHouseBackend {
    async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Mirror `LanceStore::search_candidates`: load the tenant pool, then
        // filter/sort in Rust identically (status, diary, active-supersede,
        // optional MEM_RECALL_POOL_LIMIT recency cap with guidance exemption).
        let rows = self.tenant_capsule_records(tenant).await?;

        let pool_limit = std::env::var("MEM_RECALL_POOL_LIMIT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0);

        // The recency allow-list (when capped) is over the raw
        // status/diary-filtered set, NOT the supersede-deduped one.
        let bound_ids: Option<HashSet<String>> = pool_limit.map(|n| {
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

        let supersede_targets = Self::active_supersede_targets(&rows);

        let mut out: Vec<CapabilityCapsuleRecord> = rows
            .into_iter()
            .filter(|r| !is_excluded_status(&r.status))
            .filter(|r| !is_diary(&r.capability_capsule_type))
            .filter(|r| !supersede_targets.contains(r.capability_capsule_id.as_str()))
            .filter(|r| match &bound_ids {
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

    async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Same live filter as `search_candidates` (status / diary /
        // supersede) ordered updated_at DESC, version DESC, id ASC + LIMIT.
        let rows = self.tenant_capsule_records(tenant).await?;
        let supersede_targets = Self::active_supersede_targets(&rows);
        let mut out: Vec<CapabilityCapsuleRecord> = rows
            .into_iter()
            .filter(|r| !is_excluded_status(&r.status))
            .filter(|r| !is_diary(&r.capability_capsule_type))
            .filter(|r| !supersede_targets.contains(r.capability_capsule_id.as_str()))
            .collect();
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.version.cmp(&a.version))
                .then_with(|| a.capability_capsule_id.cmp(&b.capability_capsule_id))
        });
        out.truncate(limit);
        Ok(out)
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        // Identical to the CapsuleStore impl (P1) — both traits declare it.
        CapsuleStore::fetch_capability_capsules_by_ids(self, tenant, ids).await
    }

    async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        let rows = self
            .client
            .query(
                "SELECT capability_capsule_id FROM capability_capsules FINAL \
                 WHERE tenant = ? ORDER BY updated_at DESC",
            )
            .bind(tenant)
            .fetch_all::<IdRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows.into_iter().map(|r| r.capability_capsule_id).collect())
    }

    async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        // Verbatim from `LanceStore::list_capability_capsule_versions_for_tenant`:
        // load tenant rows, bidirectional BFS over the supersede chain.
        let rows = self.tenant_capsule_records(tenant).await?;
        let by_id: HashMap<&str, &CapabilityCapsuleRecord> = rows
            .iter()
            .map(|r| (r.capability_capsule_id.as_str(), r))
            .collect();
        let mut successors: HashMap<&str, Vec<&str>> = HashMap::new();
        for r in &rows {
            if let Some(pred) = r.supersedes_capability_capsule_id.as_deref() {
                successors
                    .entry(pred)
                    .or_default()
                    .push(r.capability_capsule_id.as_str());
            }
        }

        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        if by_id.contains_key(capability_capsule_id) {
            visited.insert(capability_capsule_id);
            queue.push_back(capability_capsule_id);
        }
        while let Some(id) = queue.pop_front() {
            let rec = match by_id.get(id) {
                Some(r) => r,
                None => continue,
            };
            if let Some(pred) = rec.supersedes_capability_capsule_id.as_deref() {
                if by_id.contains_key(pred) && visited.insert(pred) {
                    queue.push_back(pred);
                }
            }
            if let Some(succs) = successors.get(id) {
                for &succ in succs {
                    if visited.insert(succ) {
                        queue.push_back(succ);
                    }
                }
            }
        }

        let mut links: Vec<CapabilityCapsuleVersionLink> = visited
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
        // ORDER BY version DESC, updated_at DESC.
        links.sort_by(|a, b| {
            b.version
                .cmp(&a.version)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        Ok(links)
    }

    async fn hybrid_candidates(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        self.hybrid_candidates_compose(tenant, query_text, query_embedding, k)
            .await
    }

    async fn hybrid_candidates_compose(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        // Identical Rust shape to `Store::hybrid_candidates_compose`.
        let has_text = !query_text.trim().is_empty();
        let has_vec = !query_embedding.is_empty();
        if (!has_text && !has_vec) || k == 0 {
            return Ok(Vec::new());
        }

        let oversample = k.saturating_mul(2);
        let bm25 = if has_text {
            self.bm25_candidate_ids(tenant, query_text, oversample)
                .await?
        } else {
            Vec::new()
        };
        let ann = if has_vec {
            self.ann_candidate_ids(tenant, query_embedding, oversample)
                .await?
        } else {
            Vec::new()
        };

        let merged = crate::pipeline::ranking::rrf_merge(&bm25, &ann);
        if merged.is_empty() {
            return Ok(Vec::new());
        }

        let fetch_n = (k.saturating_mul(3)).min(merged.len());
        let top_ids: Vec<&str> = merged
            .iter()
            .take(fetch_n)
            .map(|(id, _)| id.as_str())
            .collect();
        let records =
            CapsuleStore::fetch_capability_capsules_by_ids(self, tenant, &top_ids).await?;

        let score_by_id: HashMap<&str, f32> =
            merged.iter().map(|(id, s)| (id.as_str(), *s)).collect();
        let mut out: Vec<(CapabilityCapsuleRecord, f32)> = records
            .into_iter()
            .filter(|r| !is_excluded_status(&r.status) && !is_diary(&r.capability_capsule_type))
            .map(|r| {
                let s = *score_by_id
                    .get(r.capability_capsule_id.as_str())
                    .unwrap_or(&0.0);
                (r, s)
            })
            .collect();

        out.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.0.updated_at.cmp(&a.0.updated_at))
                .then_with(|| a.0.capability_capsule_id.cmp(&b.0.capability_capsule_id))
        });
        out.truncate(k);
        Ok(out)
    }

    async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        let q = query_text.trim();
        if q.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // Coarse lexical channel: case-insensitive substring over `content`,
        // live-filtered (status NOT archived/rejected, type != diary) like the
        // lance bm25 path. No BM25 score — rank = returned position (RRF fuses
        // by rank). CJK / token recall weak; soft-parity only (§10).
        let rows = self
            .client
            .query(
                "SELECT capability_capsule_id FROM capability_capsules FINAL \
                 WHERE tenant = ? \
                 AND status NOT IN ('rejected', 'archived') \
                 AND capability_capsule_type != 'diary' \
                 AND positionCaseInsensitiveUTF8(content, ?) > 0 \
                 ORDER BY updated_at DESC, capability_capsule_id ASC \
                 LIMIT ?",
            )
            .bind(tenant)
            .bind(q)
            .bind(k)
            .fetch_all::<IdRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| (r.capability_capsule_id, (i + 1) as i64))
            .collect())
    }

    async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        if query_embedding.is_empty() || k == 0 {
            return Ok(Vec::new());
        }
        // POSTFILTER + chunk-collapse, mirroring the lance `nearest_to`
        // postfilter: take the top-k nearest embedding rows across ALL
        // tenants, then keep this tenant's, collapsing chunks by min distance.
        // (The migration creates the table eagerly, so there's no lance-style
        // lazy-missing-table case here.)
        let qvec = query_embedding.to_vec();
        let rows = self
            .client
            .query(
                "SELECT capability_capsule_id FROM ( \
                   SELECT capability_capsule_id, tenant, \
                          cosineDistance(embedding, ?) AS d \
                   FROM capability_capsule_embeddings FINAL \
                   ORDER BY d ASC LIMIT ? \
                 ) WHERE tenant = ? \
                 GROUP BY capability_capsule_id \
                 ORDER BY min(d) ASC, capability_capsule_id ASC",
            )
            .bind(qvec)
            .bind(k)
            .bind(tenant)
            .fetch_all::<IdRow>()
            .await
            .map_err(ch_err)?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| (r.capability_capsule_id, (i + 1) as i64))
            .collect())
    }
}
