use arrow_array::RecordBatch;
use sha2::{Digest, Sha256};

use super::{MAX_ID_BYTES, MAX_JSON_BYTES, MAX_LIST_ROWS, MAX_NOTE_BYTES};
use crate::{
    domain::{
        capability_capsule::Visibility, AgentLoadoutBinding, SessionSkillPin,
        SkillBundleRevocation, SkillBundleVersionRecord, SkillCompileDecisionRecord,
        SkillFeedbackEvent, SkillHead, SkillProposalRecord, SkillProposalStatus, SkillResourceBlob,
        SkillRevisionCandidate,
    },
    storage::StorageError,
};

pub(super) fn parse_all<T>(
    batches: &[RecordBatch],
    parser: fn(&RecordBatch) -> Result<Vec<T>, StorageError>,
) -> Result<Vec<T>, StorageError> {
    let mut rows = Vec::new();
    for batch in batches {
        rows.extend(parser(batch)?);
    }
    Ok(rows)
}

pub(super) fn one_row<T>(
    mut rows: Vec<T>,
    duplicate: &'static str,
) -> Result<Option<T>, StorageError> {
    if rows.len() > 1 {
        return Err(StorageError::InvalidData(duplicate));
    }
    Ok(rows.pop())
}

pub(super) fn validate_key(value: &str, name: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control)
    {
        return Err(StorageError::InvalidInput(format!("invalid {name}")));
    }
    Ok(())
}

pub(super) fn validate_timestamp(value: &str) -> Result<(), StorageError> {
    if value.len() != 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(StorageError::InvalidInput(
            "invalid skill runtime timestamp".into(),
        ));
    }
    Ok(())
}

fn validate_json(value: &str, name: &str) -> Result<(), StorageError> {
    if value.len() > MAX_JSON_BYTES {
        return Err(StorageError::InvalidInput(format!("{name} is too large")));
    }
    serde_json::from_str::<serde_json::Value>(value)
        .map(|_| ())
        .map_err(|_| StorageError::InvalidInput(format!("invalid {name}")))
}

pub(super) fn validate_proposal(record: &SkillProposalRecord) -> Result<(), StorageError> {
    for (value, name) in [
        (&record.tenant, "tenant"),
        (&record.proposal_id, "proposal_id"),
        (&record.job_id, "job_id"),
        (&record.capsule_id, "capsule_id"),
    ] {
        validate_key(value, name)?;
    }
    if let Some(target) = &record.target_skill_id {
        validate_key(target, "target_skill_id")?;
    }
    if let Some(expected) = &record.expected_head_version {
        validate_key(expected, "expected_head_version")?;
    }
    validate_json(&record.draft_json, "draft_json")?;
    validate_json(&record.provenance_json, "provenance_json")?;
    validate_timestamp(&record.created_at)?;
    validate_timestamp(&record.updated_at)
}

pub(super) fn valid_proposal_transition(
    from: SkillProposalStatus,
    to: SkillProposalStatus,
) -> bool {
    matches!(
        (from, to),
        (
            SkillProposalStatus::PendingConfirmation,
            SkillProposalStatus::Accepted
                | SkillProposalStatus::Rejected
                | SkillProposalStatus::NeedsRebase
        ) | (
            SkillProposalStatus::NeedsRebase,
            SkillProposalStatus::PendingConfirmation | SkillProposalStatus::Rejected
        )
    )
}

pub(super) fn validate_blob(record: &SkillResourceBlob) -> Result<(), StorageError> {
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.media_type, "media_type")?;
    validate_timestamp(&record.created_at)?;
    if record.content.len() as u64 != record.size_bytes {
        return Err(StorageError::InvalidInput(
            "skill blob declared size does not match content".into(),
        ));
    }
    if record.size_bytes > crate::domain::skill_bundle::MAX_SINGLE_RESOURCE_BYTES {
        return Err(StorageError::InvalidInput(
            "skill blob exceeds single resource limit".into(),
        ));
    }
    if !crate::domain::skill_bundle::is_allowed_text_media_type(&record.media_type) {
        return Err(StorageError::InvalidInput(
            "skill blob media type must be an allowed text type".into(),
        ));
    }
    let text = std::str::from_utf8(&record.content)
        .map_err(|_| StorageError::InvalidInput("skill blob must be valid UTF-8".into()))?;
    if text.contains('\0') {
        return Err(StorageError::InvalidInput(
            "skill blob cannot contain NUL bytes".into(),
        ));
    }
    let actual = format!("{:x}", Sha256::digest(&record.content));
    if actual != record.sha256 {
        return Err(StorageError::InvalidInput(
            "skill blob sha256 does not match content".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_bundle(record: &SkillBundleVersionRecord) -> Result<(), StorageError> {
    for (value, name) in [
        (&record.tenant, "tenant"),
        (&record.skill_id, "skill_id"),
        (&record.bundle_version_id, "bundle_version_id"),
        (&record.proposal_id, "proposal_id"),
    ] {
        validate_key(value, name)?;
    }
    validate_timestamp(&record.created_at)?;
    record
        .manifest
        .validate()
        .map_err(|error| StorageError::InvalidInput(error.to_string()))?;
    if record.manifest.skill_id.0 != record.skill_id {
        return Err(StorageError::InvalidInput(
            "skill manifest id does not match bundle".into(),
        ));
    }
    if record
        .manifest
        .digest()
        .map_err(|error| StorageError::InvalidInput(error.to_string()))?
        != record.manifest_sha256
    {
        return Err(StorageError::InvalidInput(
            "skill manifest digest does not match bundle".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_head(record: &SkillHead) -> Result<(), StorageError> {
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.skill_id, "skill_id")?;
    validate_key(&record.bundle_version_id, "bundle_version_id")?;
    validate_timestamp(&record.updated_at)
}

pub(super) fn validate_loadout(record: &AgentLoadoutBinding) -> Result<(), StorageError> {
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.agent_id, "agent_id")?;
    validate_key(&record.skill_id, "skill_id")?;
    validate_timestamp(&record.updated_at)?;
    if !matches!(record.visibility, Visibility::Shared | Visibility::System) {
        return Err(StorageError::InvalidInput(
            "agent loadout visibility must be shared or system".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_pin(record: &SessionSkillPin) -> Result<(), StorageError> {
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.session_id, "session_id")?;
    validate_key(&record.agent_id, "agent_id")?;
    validate_key(&record.skill_id, "skill_id")?;
    validate_key(&record.bundle_version_id, "bundle_version_id")?;
    validate_timestamp(&record.pinned_at)?;
    validate_timestamp(&record.expires_at)?;
    if record.expires_at <= record.pinned_at || record.revision == 0 {
        return Err(StorageError::InvalidInput(
            "skill pin must have a future expiry and positive revision".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_revocation(record: &SkillBundleRevocation) -> Result<(), StorageError> {
    validate_key(&record.revocation_id, "revocation_id")?;
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.skill_id, "skill_id")?;
    validate_key(&record.bundle_version_id, "bundle_version_id")?;
    validate_key(&record.reason_code, "reason_code")?;
    if !matches!(record.revoked_by_role.as_str(), "reviewer" | "admin") {
        return Err(StorageError::InvalidInput(
            "invalid Skill revocation actor role".into(),
        ));
    }
    validate_timestamp(&record.revoked_at)
}

pub(super) fn validate_revision_candidate(
    record: &SkillRevisionCandidate,
) -> Result<(), StorageError> {
    validate_key(&record.job_id, "job_id")?;
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.skill_id, "skill_id")?;
    validate_key(&record.base_bundle_version_id, "base_bundle_version_id")?;
    validate_key(
        &record.base_capability_capsule_id,
        "base_capability_capsule_id",
    )?;
    validate_timestamp(&record.created_at)?;
    if record.feedback_event_ids.len() != 3
        || record
            .feedback_event_ids
            .iter()
            .any(|feedback_id| validate_key(feedback_id, "feedback_event_id").is_err())
    {
        return Err(StorageError::InvalidInput(
            "Skill revision candidate requires exactly three feedback events".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_compile_decision(
    record: &SkillCompileDecisionRecord,
) -> Result<(), StorageError> {
    for (value, name) in [
        (&record.job_id, "job_id"),
        (&record.tenant, "tenant"),
        (&record.input_fingerprint, "input_fingerprint"),
        (&record.model_id, "model_id"),
        (&record.finish_reason, "finish_reason"),
    ] {
        validate_key(value, name)?;
    }
    for (value, name) in [
        (&record.canonical_signature, "canonical_signature"),
        (
            &record.target_capability_capsule_id,
            "target_capability_capsule_id",
        ),
        (&record.artifact_class, "artifact_class"),
    ] {
        if let Some(value) = value {
            validate_key(value, name)?;
        }
    }
    validate_timestamp(&record.created_at)?;
    if record.reason.as_ref().is_some_and(|reason| {
        reason.is_empty() || reason.len() > 4_096 || reason.chars().any(char::is_control)
    }) {
        return Err(StorageError::InvalidInput(
            "invalid Skill compile decision reason".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_feedback(record: &SkillFeedbackEvent) -> Result<(), StorageError> {
    validate_key(&record.tenant, "tenant")?;
    validate_key(&record.feedback_id, "feedback_id")?;
    validate_key(&record.skill_id, "skill_id")?;
    validate_key(&record.bundle_version_id, "bundle_version_id")?;
    validate_key(&record.feedback_kind, "feedback_kind")?;
    validate_timestamp(&record.created_at)?;
    if record.note.as_ref().is_some_and(|note| {
        note.len() > MAX_NOTE_BYTES || note.chars().any(|character| character == '\0')
    }) {
        return Err(StorageError::InvalidInput(
            "invalid skill feedback note".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_limit(limit: usize) -> Result<(), StorageError> {
    if limit == 0 || limit > MAX_LIST_ROWS {
        return Err(StorageError::InvalidInput(
            "skill runtime list limit must be between 1 and 1000".into(),
        ));
    }
    Ok(())
}
