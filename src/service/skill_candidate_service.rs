use std::{collections::HashSet, sync::Arc};

use tokio::sync::Mutex;

use crate::domain::{
    skill_candidate_evidence_key, SkillCandidatePolicy, SkillCandidateReconcileReport,
};
use crate::pipeline::skill_candidate::plan_skill_candidate_jobs;
use crate::storage::{current_timestamp, SkillCandidateStore, StorageError};

const DEFAULT_MAX_BUILDS: usize = 100_000;
const DEFAULT_MAX_ROUNDS: usize = 20_000;

/// Repairs deterministic Skill-candidate jobs from the immutable completed
/// tool-round projection. It creates queue receipts only: no LLM call, Skill
/// mutation, capsule write, or transcript rewrite occurs here.
#[derive(Clone)]
pub struct SkillCandidateService {
    store: Arc<dyn SkillCandidateStore>,
    policy: SkillCandidatePolicy,
    max_builds: usize,
    max_rounds: usize,
    reconcile_gate: Arc<Mutex<()>>,
}

impl SkillCandidateService {
    pub fn new(store: Arc<dyn SkillCandidateStore>) -> Self {
        Self::with_limits(
            store,
            SkillCandidatePolicy::default(),
            DEFAULT_MAX_BUILDS,
            DEFAULT_MAX_ROUNDS,
        )
    }

    pub fn with_limits(
        store: Arc<dyn SkillCandidateStore>,
        policy: SkillCandidatePolicy,
        max_builds: usize,
        max_rounds: usize,
    ) -> Self {
        Self {
            store,
            policy,
            max_builds,
            max_rounds,
            reconcile_gate: Arc::new(Mutex::new(())),
        }
    }

    pub async fn reconcile(&self) -> Result<SkillCandidateReconcileReport, StorageError> {
        let _guard = self
            .reconcile_gate
            .try_lock()
            .map_err(|_| StorageError::Conflict("skill candidate reconcile already in progress"))?;
        let evidence = self
            .store
            .latest_skill_candidate_evidence(self.max_builds, self.max_rounds)
            .await?;
        let jobs = plan_skill_candidate_jobs(&evidence, &self.policy);
        let active_evidence_keys: HashSet<_> = evidence
            .iter()
            .map(|item| {
                skill_candidate_evidence_key(
                    &item.round.round_id,
                    &item.round.source_fingerprint,
                    item.round.projector_version,
                    item.round.task_signal_version,
                )
            })
            .collect();
        let now = current_timestamp();
        let reconciled = self
            .store
            .reconcile_skill_candidate_jobs(
                &jobs,
                &active_evidence_keys,
                self.policy.trigger_version,
                &now,
            )
            .await?;
        Ok(SkillCandidateReconcileReport {
            evidence_count: evidence.len(),
            planned_job_count: jobs.len(),
            inserted_job_count: reconciled.inserted,
            existing_job_count: reconciled.existing,
            staled_job_count: reconciled.staled,
        })
    }
}
