//! Backend-agnostic capsule search — Phase 3 sub-trait.
//!
//! Carries the QW-1 portable primitives (`bm25_candidate_ids`,
//! `ann_candidate_ids`) + the convenience aggregates that compose
//! them (`hybrid_candidates`, `hybrid_candidates_compose`,
//! `search_candidates`, `recent_active_capability_capsules`,
//! `fetch_capability_capsules_by_ids`).
//!
//! **LANCE-SPECIFIC bits** (already annotated on `Store`):
//! - `hybrid_candidates`: fused-SQL fast path (lance_fts +
//!   lance_vector_search in one statement). 14–29% faster than
//!   compose at k ∈ {10,50,100} per QW-1 bench.
//! - `bm25_candidate_ids` / `ann_candidate_ids`: lance_fts /
//!   lance_vector_search wrappers.
//!
//! Other backends implement `hybrid_candidates` by routing to
//! `hybrid_candidates_compose` (the portable form). Trait surface
//! is uniform; the optimization knob is per-backend.
//!
//! See `docs/backend-coupling.md` §3.1 + §4.1 + §6.4.

use async_trait::async_trait;

use crate::domain::capability_capsule::{CapabilityCapsuleRecord, CapabilityCapsuleVersionLink};
use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait CapsuleSearchStore: Send + Sync {
    /// Full live (non-rejected, non-archived, non-diary) pool for
    /// `tenant`. Returned unbounded — `pipeline::retrieve` scores
    /// every candidate.
    async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;

    /// Same live filter as `search_candidates` but ordered
    /// `updated_at DESC, version DESC, id ASC` with a `LIMIT`.
    /// Used by `mem wake-up` fast path.
    async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;

    /// Bulk fetch by id list, scoped to `tenant`. Empty `ids`
    /// short-circuits to `Ok(vec![])`. Order is not guaranteed to
    /// match input slice order.
    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;

    /// Project just `capability_capsule_id` column for `tenant`,
    /// ordered `updated_at DESC`. Cheap admin / repair operation.
    async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError>;

    /// Version-chain metadata for one capsule in `tenant`. Returns
    /// every version link, ordered `version DESC, updated_at DESC`.
    async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError>;

    /// Cross-table hybrid recall: BM25 + ANN + RRF. Returns
    /// `(record, rrf_score)` ordered by
    /// `(rrf_score DESC, updated_at DESC, capability_capsule_id ASC)`.
    /// Three input shapes (text + vec, text-only, vec-only); both
    /// empty returns `Ok(vec![])`. The default impl on each backend
    /// may use a fused SQL path (Lance) or the compose form (others).
    async fn hybrid_candidates(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError>;

    /// Portable compose form of `hybrid_candidates`: explicitly
    /// `bm25_candidate_ids` + `ann_candidate_ids` + Rust-side RRF +
    /// `fetch_capability_capsules_by_ids` + sort. Same outputs as
    /// `hybrid_candidates` within f32 rounding; left exposed as the
    /// reference shape future backends can route through.
    async fn hybrid_candidates_compose(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError>;

    /// Top-K BM25 candidate ids over `capability_capsules.content`
    /// for `tenant`, filtered by live status + non-diary type. 1-based
    /// rank, ordered by `(_score DESC, id ASC)`. Empty `query_text`
    /// short-circuits to `Ok(vec![])`.
    async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError>;

    /// Top-K ANN candidate ids over
    /// `capability_capsule_embeddings.embedding` for `tenant`. 1-based
    /// rank, ordered by `(_distance ASC, id ASC)`. **No status /
    /// type filter** — embeddings table doesn't carry those columns;
    /// caller filters after hydration. Empty query / lazy-missing
    /// table short-circuits to `Ok(vec![])`.
    async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError>;
}

#[async_trait]
impl CapsuleSearchStore for Store {
    async fn search_candidates(
        &self,
        tenant: &str,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::search_candidates(self, tenant).await
    }

    async fn recent_active_capability_capsules(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::recent_active_capability_capsules(self, tenant, limit).await
    }

    async fn fetch_capability_capsules_by_ids(
        &self,
        tenant: &str,
        ids: &[&str],
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::fetch_capability_capsules_by_ids(self, tenant, ids).await
    }

    async fn list_capability_capsule_ids_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, StorageError> {
        Store::list_capability_capsule_ids_for_tenant(self, tenant).await
    }

    async fn list_capability_capsule_versions_for_tenant(
        &self,
        tenant: &str,
        capability_capsule_id: &str,
    ) -> Result<Vec<CapabilityCapsuleVersionLink>, StorageError> {
        Store::list_capability_capsule_versions_for_tenant(self, tenant, capability_capsule_id)
            .await
    }

    async fn hybrid_candidates(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        Store::hybrid_candidates(self, tenant, query_text, query_embedding, k).await
    }

    async fn hybrid_candidates_compose(
        &self,
        tenant: &str,
        query_text: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(CapabilityCapsuleRecord, f32)>, StorageError> {
        Store::hybrid_candidates_compose(self, tenant, query_text, query_embedding, k).await
    }

    async fn bm25_candidate_ids(
        &self,
        tenant: &str,
        query_text: &str,
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        Store::bm25_candidate_ids(self, tenant, query_text, k).await
    }

    async fn ann_candidate_ids(
        &self,
        tenant: &str,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, i64)>, StorageError> {
        Store::ann_candidate_ids(self, tenant, query_embedding, k).await
    }
}
