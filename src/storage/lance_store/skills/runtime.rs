use super::{
    codec::{
        compile_decision_batch, feedback_batch, parse_compile_decisions, parse_feedback,
        parse_revision_candidates, parse_revocations, revision_candidate_batch, revocation_batch,
    },
    validation::{
        one_row, parse_all, validate_compile_decision, validate_feedback, validate_key,
        validate_limit, validate_revision_candidate, validate_revocation,
    },
    COMPILE_DECISIONS_TABLE, FEEDBACK_TABLE, REVISION_CANDIDATES_TABLE, REVOCATIONS_TABLE,
};
use crate::{
    domain::{
        SkillBundleRevocation, SkillCompileDecisionRecord, SkillFeedbackEvent,
        SkillRevisionCandidate,
    },
    storage::{lance_store::lancedb_err, StorageError},
};

use super::super::{sql_quote, LanceStore};

impl LanceStore {
    pub async fn insert_skill_compile_decision(
        &self,
        decision: SkillCompileDecisionRecord,
    ) -> Result<SkillCompileDecisionRecord, StorageError> {
        validate_compile_decision(&decision)?;
        if let Some(existing) = self
            .get_skill_compile_decision(&decision.tenant, &decision.job_id)
            .await?
        {
            return if same_compile_decision_payload(&existing, &decision) {
                Ok(existing)
            } else {
                Err(StorageError::Conflict(
                    "Skill compile decision job already settled",
                ))
            };
        }
        let table = self
            .conn
            .open_table(COMPILE_DECISIONS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(compile_decision_batch(&decision)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(decision)
    }

    pub async fn get_skill_compile_decision(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillCompileDecisionRecord>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(job_id, "job_id")?;
        let batches = self
            .query_skill_rows(
                COMPILE_DECISIONS_TABLE,
                format!(
                    "tenant = {} AND job_id = {}",
                    sql_quote(tenant),
                    sql_quote(job_id),
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_compile_decisions)?,
            "duplicate Skill compile decisions",
        )
    }

    pub async fn append_skill_feedback(
        &self,
        feedback: SkillFeedbackEvent,
    ) -> Result<SkillFeedbackEvent, StorageError> {
        validate_feedback(&feedback)?;
        if let Some(existing) = self
            .get_skill_feedback(&feedback.tenant, &feedback.feedback_id)
            .await?
        {
            return if same_feedback_payload(&existing, &feedback) {
                Ok(existing)
            } else {
                Err(StorageError::Conflict("skill feedback id already exists"))
            };
        }
        if self
            .get_skill_bundle_version(
                &feedback.tenant,
                &feedback.skill_id,
                &feedback.bundle_version_id,
            )
            .await?
            .is_none()
        {
            return Err(StorageError::NotFound("skill bundle version"));
        }
        let table = self
            .conn
            .open_table(FEEDBACK_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(feedback_batch(&feedback)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(feedback)
    }

    pub async fn revoke_skill_bundle(
        &self,
        revocation: SkillBundleRevocation,
    ) -> Result<SkillBundleRevocation, StorageError> {
        validate_revocation(&revocation)?;
        if self
            .get_skill_bundle_version(
                &revocation.tenant,
                &revocation.skill_id,
                &revocation.bundle_version_id,
            )
            .await?
            .is_none()
        {
            return Err(StorageError::NotFound("skill bundle version"));
        }
        if let Some(existing) = self
            .get_skill_bundle_revocation(
                &revocation.tenant,
                &revocation.skill_id,
                &revocation.bundle_version_id,
            )
            .await?
        {
            return if existing.tenant == revocation.tenant
                && existing.skill_id == revocation.skill_id
                && existing.bundle_version_id == revocation.bundle_version_id
                && existing.reason_code == revocation.reason_code
            {
                Ok(existing)
            } else {
                Err(StorageError::Conflict("skill bundle already revoked"))
            };
        }
        let table = self
            .conn
            .open_table(REVOCATIONS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(revocation_batch(&revocation)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(revocation)
    }

    pub async fn get_skill_bundle_revocation(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<Option<SkillBundleRevocation>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(skill_id, "skill_id")?;
        validate_key(bundle_version_id, "bundle_version_id")?;
        let batches = self
            .query_skill_rows(
                REVOCATIONS_TABLE,
                format!(
                    "tenant = {} AND skill_id = {} AND bundle_version_id = {}",
                    sql_quote(tenant),
                    sql_quote(skill_id),
                    sql_quote(bundle_version_id),
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_revocations)?,
            "duplicate skill bundle revocations",
        )
    }

    pub async fn insert_skill_revision_candidate(
        &self,
        candidate: SkillRevisionCandidate,
    ) -> Result<SkillRevisionCandidate, StorageError> {
        validate_revision_candidate(&candidate)?;
        if let Some(existing) = self
            .get_skill_revision_candidate(&candidate.tenant, &candidate.job_id)
            .await?
        {
            return if existing.job_id == candidate.job_id
                && existing.tenant == candidate.tenant
                && existing.skill_id == candidate.skill_id
                && existing.base_bundle_version_id == candidate.base_bundle_version_id
                && existing.base_capability_capsule_id == candidate.base_capability_capsule_id
                && existing.feedback_event_ids == candidate.feedback_event_ids
            {
                Ok(existing)
            } else {
                Err(StorageError::Conflict(
                    "Skill revision candidate id already exists",
                ))
            };
        }
        let table = self
            .conn
            .open_table(REVISION_CANDIDATES_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(revision_candidate_batch(&candidate)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(candidate)
    }

    pub async fn get_skill_revision_candidate(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillRevisionCandidate>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(job_id, "job_id")?;
        let batches = self
            .query_skill_rows(
                REVISION_CANDIDATES_TABLE,
                format!(
                    "tenant = {} AND job_id = {}",
                    sql_quote(tenant),
                    sql_quote(job_id),
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_revision_candidates)?,
            "duplicate Skill revision candidates",
        )
    }

    async fn get_skill_feedback(
        &self,
        tenant: &str,
        feedback_id: &str,
    ) -> Result<Option<SkillFeedbackEvent>, StorageError> {
        let batches = self
            .query_skill_rows(
                FEEDBACK_TABLE,
                format!(
                    "tenant = {} AND feedback_id = {}",
                    sql_quote(tenant),
                    sql_quote(feedback_id)
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_feedback)?,
            "duplicate skill feedback",
        )
    }

    pub async fn list_skill_feedback(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
        limit: usize,
    ) -> Result<Vec<SkillFeedbackEvent>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(skill_id, "skill_id")?;
        validate_key(bundle_version_id, "bundle_version_id")?;
        validate_limit(limit)?;
        let batches = self
            .query_skill_rows(
                FEEDBACK_TABLE,
                format!(
                    "tenant = {} AND skill_id = {} AND bundle_version_id = {}",
                    sql_quote(tenant),
                    sql_quote(skill_id),
                    sql_quote(bundle_version_id)
                ),
                limit,
            )
            .await?;
        let mut rows = parse_all(&batches, parse_feedback)?;
        rows.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.feedback_id.cmp(&right.feedback_id))
        });
        Ok(rows)
    }
}

fn same_feedback_payload(left: &SkillFeedbackEvent, right: &SkillFeedbackEvent) -> bool {
    left.tenant == right.tenant
        && left.feedback_id == right.feedback_id
        && left.skill_id == right.skill_id
        && left.bundle_version_id == right.bundle_version_id
        && left.feedback_kind == right.feedback_kind
        && left.note == right.note
}

fn same_compile_decision_payload(
    left: &SkillCompileDecisionRecord,
    right: &SkillCompileDecisionRecord,
) -> bool {
    left.job_id == right.job_id
        && left.tenant == right.tenant
        && left.input_fingerprint == right.input_fingerprint
        && left.decision_kind == right.decision_kind
        && left.canonical_signature == right.canonical_signature
        && left.target_capability_capsule_id == right.target_capability_capsule_id
        && left.artifact_class == right.artifact_class
        && left.reason == right.reason
        && left.model_id == right.model_id
        && left.finish_reason == right.finish_reason
        && left.prompt_tokens == right.prompt_tokens
        && left.completion_tokens == right.completion_tokens
}
