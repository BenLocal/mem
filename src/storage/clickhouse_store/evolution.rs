//! `EvolutionCandidateStore` for [`ClickHouseBackend`].
//!
//! **UNVALIDATED scaffold — not yet run against a real ClickHouse
//! (clickhouse-backend P5).** Versioned insert + `FINAL` read; list/result
//! columns are `Array(String)`. Same §4(a) shape as the rest.

use async_trait::async_trait;
use clickhouse::Row;
use serde::{Deserialize, Serialize};

use super::backend::ClickHouseBackend;
use super::capsule_store::{ch_err, now_version, opt};
use crate::storage::lance_store::evolution::EvolutionCandidate;
use crate::storage::types::StorageError;
use crate::storage::EvolutionCandidateStore;

#[derive(Row, Serialize, Deserialize)]
struct ChEvoRow {
    candidate_id: String,
    tenant: String,
    op_kind: String,
    member_ids: Vec<String>,
    params: String,
    evidence: f32,
    consecutive_cycles: i64,
    status: String,
    first_proposed_at: String,
    last_signal_at: String,
    executed_at: String,
    result_capsule_ids: Vec<String>,
    row_version: u64,
}

impl ChEvoRow {
    fn from_candidate(c: &EvolutionCandidate) -> Self {
        Self {
            candidate_id: c.candidate_id.clone(),
            tenant: c.tenant.clone(),
            op_kind: c.op_kind.clone(),
            member_ids: c.member_ids.clone(),
            params: c.params.clone(),
            evidence: c.evidence,
            consecutive_cycles: c.consecutive_cycles,
            status: c.status.clone(),
            first_proposed_at: c.first_proposed_at.clone(),
            last_signal_at: c.last_signal_at.clone(),
            executed_at: c.executed_at.clone().unwrap_or_default(),
            result_capsule_ids: c.result_capsule_ids.clone(),
            row_version: now_version(),
        }
    }

    fn into_candidate(self) -> EvolutionCandidate {
        EvolutionCandidate {
            candidate_id: self.candidate_id,
            tenant: self.tenant,
            op_kind: self.op_kind,
            member_ids: self.member_ids,
            params: self.params,
            evidence: self.evidence,
            consecutive_cycles: self.consecutive_cycles,
            status: self.status,
            first_proposed_at: self.first_proposed_at,
            last_signal_at: self.last_signal_at,
            executed_at: opt(self.executed_at),
            result_capsule_ids: self.result_capsule_ids,
        }
    }
}

#[async_trait]
impl EvolutionCandidateStore for ClickHouseBackend {
    async fn upsert_evolution_candidate(
        &self,
        candidate: EvolutionCandidate,
    ) -> Result<(), StorageError> {
        let mut insert = self
            .client
            .insert::<ChEvoRow>("evolution_candidates")
            .await
            .map_err(ch_err)?;
        insert
            .write(&ChEvoRow::from_candidate(&candidate))
            .await
            .map_err(ch_err)?;
        insert.end().await.map_err(ch_err)?;
        Ok(())
    }

    async fn upsert_evolution_candidates(
        &self,
        candidates: Vec<EvolutionCandidate>,
    ) -> Result<(), StorageError> {
        // Non-Lance backend: no version-manifest churn to fold, so a
        // per-row upsert loop is correct and sufficient.
        for candidate in candidates {
            self.upsert_evolution_candidate(candidate).await?;
        }
        Ok(())
    }

    async fn list_evolution_candidates(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionCandidate>, StorageError> {
        let rows = if let Some(s) = status {
            self.client
                .query(
                    "SELECT ?fields FROM evolution_candidates FINAL \
                     WHERE tenant = ? AND status = ? ORDER BY last_signal_at DESC",
                )
                .bind(tenant)
                .bind(s)
                .fetch_all::<ChEvoRow>()
                .await
                .map_err(ch_err)?
        } else {
            self.client
                .query(
                    "SELECT ?fields FROM evolution_candidates FINAL \
                     WHERE tenant = ? ORDER BY last_signal_at DESC",
                )
                .bind(tenant)
                .fetch_all::<ChEvoRow>()
                .await
                .map_err(ch_err)?
        };
        Ok(rows.into_iter().map(ChEvoRow::into_candidate).collect())
    }
}
