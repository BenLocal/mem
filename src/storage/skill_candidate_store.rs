use std::collections::HashSet;

use async_trait::async_trait;

use crate::domain::{
    ClaimedSkillCandidateJob, SkillCandidateEnsureReport, SkillCandidateEvidence,
    SkillCandidateJobSpec,
};

use super::{StorageError, Store};

const MAX_RECONCILE_JOBS: usize = 100_000;
const MAX_RECONCILE_ROUND_REF_BYTES: usize = 64 * 1024 * 1024;

/// Narrow durable-queue seam for deterministic Skill candidates. It remains
/// independent of the umbrella `Backend`, so alternate capsule backends do not
/// silently pretend to implement a Lance-only derived index.
#[async_trait]
pub trait SkillCandidateStore: Send + Sync {
    async fn latest_skill_candidate_evidence(
        &self,
        max_builds: usize,
        max_rounds: usize,
    ) -> Result<Vec<SkillCandidateEvidence>, StorageError>;

    async fn ensure_skill_candidate_jobs(
        &self,
        specs: &[SkillCandidateJobSpec],
        now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError>;

    /// Ensure the current plan and stale nonterminal receipts whose evidence
    /// disappeared or whose trigger version is obsolete. Live leases are not
    /// preempted. All capacity/read checks happen before the first write.
    async fn reconcile_skill_candidate_jobs(
        &self,
        specs: &[SkillCandidateJobSpec],
        active_evidence_keys: &HashSet<String>,
        trigger_version: u32,
        now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError>;

    async fn claim_skill_candidate_jobs(
        &self,
        now: &str,
        lease_expires_at: &str,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<ClaimedSkillCandidateJob>, StorageError>;

    async fn complete_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        now: &str,
    ) -> Result<(), StorageError>;

    async fn fail_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        error_code: &str,
        retry_at: &str,
        now: &str,
        max_attempts: u32,
    ) -> Result<(), StorageError>;
}

#[async_trait]
impl SkillCandidateStore for Store {
    async fn latest_skill_candidate_evidence(
        &self,
        max_builds: usize,
        max_rounds: usize,
    ) -> Result<Vec<SkillCandidateEvidence>, StorageError> {
        self.lance
            .latest_skill_candidate_evidence(max_builds, max_rounds)
            .await
    }

    async fn ensure_skill_candidate_jobs(
        &self,
        specs: &[SkillCandidateJobSpec],
        now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError> {
        let _guard = self.skill_candidate_queue_gate.lock().await;
        self.commit_lance_write(self.lance.ensure_skill_candidate_jobs(specs, now).await)
            .await
    }

    async fn reconcile_skill_candidate_jobs(
        &self,
        specs: &[SkillCandidateJobSpec],
        active_evidence_keys: &HashSet<String>,
        trigger_version: u32,
        now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError> {
        let _guard = self.skill_candidate_queue_gate.lock().await;
        if specs.len() > MAX_RECONCILE_JOBS {
            crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
            return Err(StorageError::InvalidInput(
                "skill candidate reconcile job capacity exceeded".into(),
            ));
        }
        let mut round_ref_bytes = 0_usize;
        for spec in specs {
            round_ref_bytes = round_ref_bytes.saturating_add(
                serde_json::to_vec(&spec.round_refs)
                    .map_err(StorageError::from)?
                    .len(),
            );
            if round_ref_bytes > MAX_RECONCILE_ROUND_REF_BYTES {
                crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
                return Err(StorageError::InvalidInput(
                    "skill candidate reconcile evidence byte capacity exceeded".into(),
                ));
            }
        }
        let planned_job_ids: HashSet<_> = specs.iter().map(|spec| spec.job_id.clone()).collect();
        if planned_job_ids.len() != specs.len() {
            return Err(StorageError::InvalidInput(
                "duplicate skill candidate jobs in reconcile plan".into(),
            ));
        }
        let stale_ids = self
            .lance
            .find_invalid_skill_candidate_job_ids(active_evidence_keys, trigger_version, now)
            .await?;
        let reactivatable_ids = self
            .lance
            .find_reactivatable_skill_candidate_job_ids(active_evidence_keys, trigger_version)
            .await?;
        let current_nonterminal = self.lance.skill_candidate_nonterminal_count().await?;
        let missing_additions = self
            .lance
            .skill_candidate_reconcile_additions(specs)
            .await?;
        if let Err(error) = validate_reconcile_capacity(
            current_nonterminal,
            stale_ids.len(),
            missing_additions.saturating_add(reactivatable_ids.len()),
            MAX_RECONCILE_JOBS,
        ) {
            crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
            return Err(error);
        }
        let mut report = SkillCandidateEnsureReport {
            staled: self
                .commit_lance_write(self.lance.stale_skill_candidate_jobs(&stale_ids, now).await)
                .await?,
            ..SkillCandidateEnsureReport::default()
        };
        self.commit_lance_write(
            self.lance
                .reactivate_skill_candidate_jobs(&reactivatable_ids, now)
                .await,
        )
        .await?;
        for chunk in specs.chunks(256) {
            let ensured = self
                .commit_lance_write(self.lance.ensure_skill_candidate_jobs(chunk, now).await)
                .await?;
            report.inserted += ensured.inserted;
            report.existing += ensured.existing;
        }
        Ok(report)
    }

    async fn claim_skill_candidate_jobs(
        &self,
        now: &str,
        lease_expires_at: &str,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<ClaimedSkillCandidateJob>, StorageError> {
        let _guard = self.skill_candidate_queue_gate.lock().await;
        self.commit_lance_write(
            self.lance
                .claim_skill_candidate_jobs(now, lease_expires_at, max_retries, limit)
                .await,
        )
        .await
    }

    async fn complete_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        let _guard = self.skill_candidate_queue_gate.lock().await;
        self.commit_lance_write(
            self.lance
                .complete_skill_candidate_job(job_id, lease_token, now)
                .await,
        )
        .await
    }

    async fn fail_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        error_code: &str,
        retry_at: &str,
        now: &str,
        max_attempts: u32,
    ) -> Result<(), StorageError> {
        let _guard = self.skill_candidate_queue_gate.lock().await;
        self.commit_lance_write(
            self.lance
                .fail_skill_candidate_job(
                    job_id,
                    lease_token,
                    error_code,
                    retry_at,
                    now,
                    max_attempts,
                )
                .await,
        )
        .await
    }
}

fn validate_reconcile_capacity(
    current_nonterminal: usize,
    staling: usize,
    additions: usize,
    limit: usize,
) -> Result<(), StorageError> {
    let after_stale = current_nonterminal.saturating_sub(staling);
    if after_stale.saturating_add(additions) > limit {
        return Err(StorageError::InvalidInput(
            "skill candidate nonterminal capacity exceeded".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_capacity_accounts_for_existing_rows_before_any_write() {
        assert!(validate_reconcile_capacity(4, 0, 1, 4).is_err());
        assert!(validate_reconcile_capacity(4, 1, 1, 4).is_ok());
    }
}
