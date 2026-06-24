//! Backend-agnostic evolution-candidate store — the durable state of
//! the self-evolution worker's anti-jitter gate (doc
//! `docs/evolution-worker.md` §3.3 / §8.2).
//!
//! A candidate row tracks one proposed evolution operation (merge /
//! generalize) across sweeps: accumulated evidence, consecutive-cycle
//! counter, lifecycle status. Durability is load-bearing: the K-cycle
//! gate only opens after the signal held for K consecutive sweeps, so
//! losing this state on restart would reset every candidate's clock.

use async_trait::async_trait;

pub use crate::storage::lance_store::evolution::EvolutionCandidate;
use crate::storage::types::StorageError;
use crate::storage::Store;

#[async_trait]
pub trait EvolutionCandidateStore: Send + Sync {
    /// Upsert one candidate keyed on `candidate_id` — insert when new,
    /// full-row replace when existing.
    async fn upsert_evolution_candidate(
        &self,
        candidate: EvolutionCandidate,
    ) -> Result<(), StorageError>;

    /// List candidates for `tenant`, optionally filtered by status
    /// (`pending` / `executed` / `cancelled`). Sweep-time read.
    async fn list_evolution_candidates(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError>;
}

#[async_trait]
impl EvolutionCandidateStore for Store {
    async fn upsert_evolution_candidate(
        &self,
        candidate: EvolutionCandidate,
    ) -> Result<(), StorageError> {
        // Route the write through commit_lance_write for a uniform write
        // shape (it's a pass-through since route-B removed the DuckDB read
        // engine; reads are lance-native — same rationale as mine_cursors).
        self.commit_lance_write(self.lance.upsert_evolution_candidate(candidate).await)
            .await
    }

    async fn list_evolution_candidates(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError> {
        self.lance.list_evolution_candidates(tenant, status).await
    }
}
