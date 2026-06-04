//! Backend-agnostic maintenance operations — Phase 3 sub-trait.
//!
//! Three sweeps:
//! - `apply_time_decay` — bulk increment of `decay_score` for active
//!   capsules based on time since `updated_at`. Universal — every
//!   backend should be able to express this.
//! - `vacuum_old_versions` — Lance manifest prune. **LANCE-SPECIFIC**
//!   in implementation: Postgres has autovacuum, SQLite has no
//!   equivalent. Per `docs/backend-coupling.md` §7.5 this trait
//!   should probably be capability-style in Phase 4 spike; for now
//!   it's a uniform method and non-Lance backends will need to
//!   return a no-op `VacuumStats`.
//! - `auto_promote_candidates` — query rows eligible for the
//!   auto-promote sweep (pending + idle + low decay). Universal.
//!
//! Returns `Result<_, StorageError>`. See doc §3.1 + §6.4.

use async_trait::async_trait;

use crate::domain::capability_capsule::{CapabilityCapsuleRecord, CapabilityCapsuleType};
use crate::storage::lance_store::{VacuumStats, VectorIndexStats};
use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait MaintenanceStore: Send + Sync {
    /// Bulk decay sweep: per-row `decay_score += rate * days_since`,
    /// capped at 1.0, only on `status = active` rows. `now_ms_str`
    /// is the 20-digit ms timestamp that becomes the new
    /// `updated_at`.
    async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError>;

    /// Prune Lance version manifests older than `older_than_days`
    /// across every managed table. Lance-specific by construction;
    /// non-Lance backends should return a zero-stats `VacuumStats`.
    /// Trait default goes through the aggressive path
    /// ([`Self::vacuum_old_versions_with`] with `aggressive=true`).
    async fn vacuum_old_versions(&self, older_than_days: i64) -> Result<VacuumStats, StorageError> {
        self.vacuum_old_versions_with(older_than_days, true).await
    }

    /// Explicit-flag variant. `aggressive=true` bypasses Lance's
    /// hard 7-day in-flight safety floor (single-writer
    /// local-first default); `aggressive=false` keeps the floor
    /// (multi-writer / shared-dataset deployments). Non-Lance
    /// backends ignore the flag and return zero-stats.
    async fn vacuum_old_versions_with(
        &self,
        older_than_days: i64,
        aggressive: bool,
    ) -> Result<VacuumStats, StorageError>;

    /// Build/refresh ANN vector indexes on the embedding tables so
    /// `lance_vector_search` doesn't brute-force a large unindexed table
    /// (the transcript-search 5–11s root cause). Lance-specific; the
    /// default is a zero-stats no-op so non-Lance backends compile
    /// unchanged. Driven on a cadence by `crate::worker::vacuum_worker`.
    async fn ensure_vector_indexes(&self) -> Result<VectorIndexStats, StorageError> {
        Ok(VectorIndexStats::default())
    }

    /// Capsules eligible for auto-promote: `status=pending`,
    /// `updated_at < cutoff_updated_at`, `decay_score <
    /// max_decay_score`, and `capability_capsule_type ∈ types`.
    /// Empty `types` short-circuits to `Ok(vec![])`.
    async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError>;
}

#[async_trait]
impl MaintenanceStore for Store {
    async fn apply_time_decay(
        &self,
        decay_rate_per_day: f64,
        now_ms: f64,
        ms_per_day: f64,
        now_ms_str: &str,
    ) -> Result<(), StorageError> {
        Store::apply_time_decay(self, decay_rate_per_day, now_ms, ms_per_day, now_ms_str).await
    }

    async fn vacuum_old_versions_with(
        &self,
        older_than_days: i64,
        aggressive: bool,
    ) -> Result<VacuumStats, StorageError> {
        // commit_lance_write refreshes the DuckDB snapshot so the
        // post-vacuum manifest set is visible immediately to readers.
        self.commit_lance_write(
            self.lance
                .vacuum_old_versions_with(older_than_days, aggressive)
                .await,
        )
        .await
    }

    async fn ensure_vector_indexes(&self) -> Result<VectorIndexStats, StorageError> {
        // commit_lance_write marks the DuckDB snapshot dirty so the next
        // read refreshes and `lance_vector_search` picks up the new index.
        self.commit_lance_write(self.lance.ensure_vector_indexes().await)
            .await
    }

    async fn auto_promote_candidates(
        &self,
        tenant: &str,
        cutoff_updated_at: &str,
        types: &[CapabilityCapsuleType],
        max_decay_score: f32,
    ) -> Result<Vec<CapabilityCapsuleRecord>, StorageError> {
        Store::auto_promote_candidates(self, tenant, cutoff_updated_at, types, max_decay_score)
            .await
    }
}
