use std::sync::Arc;

use arrow_array::{
    builder::{BinaryBuilder, BooleanBuilder, Int32Builder, StringBuilder, UInt64Builder},
    Array, BinaryArray, BooleanArray, Int32Array, RecordBatch, StringArray, UInt64Array,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

use super::super::{enum_from_str, enum_to_str, parse_col};
use super::{
    BLOBS_TABLE, BUNDLES_TABLE, COMPILE_DECISIONS_TABLE, FEEDBACK_TABLE, HEADS_TABLE,
    LOADOUTS_TABLE, PINS_TABLE, PROPOSALS_TABLE, REVISION_CANDIDATES_TABLE, REVOCATIONS_TABLE,
};
use crate::{
    domain::{
        AgentLoadoutBinding, AgentLoadoutMode, SessionSkillPin, SkillBundleRevocation,
        SkillBundleVersionRecord, SkillCompileDecisionRecord, SkillFeedbackEvent, SkillHead,
        SkillProposalRecord, SkillProposalStatus, SkillResourceBlob, SkillRevisionCandidate,
    },
    storage::StorageError,
};

pub(super) fn proposals_schema() -> Schema {
    Schema::new(vec![
        Field::new("proposal_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("job_id", DataType::Utf8, false),
        Field::new("capsule_id", DataType::Utf8, false),
        Field::new("draft_json", DataType::Utf8, false),
        Field::new("provenance_json", DataType::Utf8, false),
        Field::new("target_skill_id", DataType::Utf8, true),
        Field::new("expected_head_version", DataType::Utf8, true),
        Field::new("status", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

pub(super) fn blobs_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("sha256", DataType::Utf8, false),
        Field::new("media_type", DataType::Utf8, false),
        Field::new("content", DataType::Binary, false),
        Field::new("size_bytes", DataType::UInt64, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

pub(super) fn bundles_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("bundle_version_id", DataType::Utf8, false),
        Field::new("proposal_id", DataType::Utf8, false),
        Field::new("workflow_capsule_id", DataType::Utf8, false),
        Field::new("previous_bundle_version_id", DataType::Utf8, true),
        Field::new("manifest_json", DataType::Utf8, false),
        Field::new("manifest_sha256", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

pub(super) fn heads_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("bundle_version_id", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

pub(super) fn loadouts_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("agent_id", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("mode", DataType::Utf8, false),
        Field::new("priority", DataType::Int32, false),
        Field::new("enabled", DataType::Boolean, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
    ])
}

pub(super) fn pins_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("agent_id", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("bundle_version_id", DataType::Utf8, false),
        Field::new("pinned_at", DataType::Utf8, false),
        Field::new("expires_at", DataType::Utf8, false),
        Field::new("revision", DataType::UInt64, false),
    ])
}

pub(super) fn revocations_schema() -> Schema {
    Schema::new(vec![
        Field::new("revocation_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("bundle_version_id", DataType::Utf8, false),
        Field::new("reason_code", DataType::Utf8, false),
        Field::new("revoked_by_role", DataType::Utf8, false),
        Field::new("revoked_at", DataType::Utf8, false),
    ])
}

pub(super) fn revision_candidates_schema() -> Schema {
    Schema::new(vec![
        Field::new("job_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("base_bundle_version_id", DataType::Utf8, false),
        Field::new("base_capability_capsule_id", DataType::Utf8, false),
        Field::new("feedback_event_ids_json", DataType::Utf8, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

pub(super) fn feedback_schema() -> Schema {
    Schema::new(vec![
        Field::new("tenant", DataType::Utf8, false),
        Field::new("feedback_id", DataType::Utf8, false),
        Field::new("skill_id", DataType::Utf8, false),
        Field::new("bundle_version_id", DataType::Utf8, false),
        Field::new("feedback_kind", DataType::Utf8, false),
        Field::new("note", DataType::Utf8, true),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

pub(super) fn compile_decisions_schema() -> Schema {
    Schema::new(vec![
        Field::new("job_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("input_fingerprint", DataType::Utf8, false),
        Field::new("decision_kind", DataType::Utf8, false),
        Field::new("canonical_signature", DataType::Utf8, true),
        Field::new("target_capability_capsule_id", DataType::Utf8, true),
        Field::new("artifact_class", DataType::Utf8, true),
        Field::new("reason", DataType::Utf8, true),
        Field::new("model_id", DataType::Utf8, false),
        Field::new("finish_reason", DataType::Utf8, false),
        Field::new("prompt_tokens", DataType::UInt64, false),
        Field::new("completion_tokens", DataType::UInt64, false),
        Field::new("created_at", DataType::Utf8, false),
    ])
}

fn append_optional(builder: &mut StringBuilder, value: Option<&str>) {
    match value {
        Some(value) => builder.append_value(value),
        None => builder.append_null(),
    }
}

pub(super) fn proposal_batch(record: &SkillProposalRecord) -> Result<RecordBatch, StorageError> {
    let mut proposal_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut job_id = StringBuilder::new();
    let mut capsule_id = StringBuilder::new();
    let mut draft_json = StringBuilder::new();
    let mut provenance_json = StringBuilder::new();
    let mut target_skill_id = StringBuilder::new();
    let mut expected_head_version = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    proposal_id.append_value(&record.proposal_id);
    tenant.append_value(&record.tenant);
    job_id.append_value(&record.job_id);
    capsule_id.append_value(&record.capsule_id);
    draft_json.append_value(&record.draft_json);
    provenance_json.append_value(&record.provenance_json);
    append_optional(&mut target_skill_id, record.target_skill_id.as_deref());
    append_optional(
        &mut expected_head_version,
        record.expected_head_version.as_deref(),
    );
    status.append_value(record.status.as_db_str());
    created_at.append_value(&record.created_at);
    updated_at.append_value(&record.updated_at);
    make_batch(
        proposals_schema(),
        vec![
            Arc::new(proposal_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(job_id.finish()),
            Arc::new(capsule_id.finish()),
            Arc::new(draft_json.finish()),
            Arc::new(provenance_json.finish()),
            Arc::new(target_skill_id.finish()),
            Arc::new(expected_head_version.finish()),
            Arc::new(status.finish()),
            Arc::new(created_at.finish()),
            Arc::new(updated_at.finish()),
        ],
    )
}

pub(super) fn blob_batch(record: &SkillResourceBlob) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut sha256 = StringBuilder::new();
    let mut media_type = StringBuilder::new();
    let mut content = BinaryBuilder::new();
    let mut size_bytes = UInt64Builder::new();
    let mut created_at = StringBuilder::new();
    tenant.append_value(&record.tenant);
    sha256.append_value(&record.sha256);
    media_type.append_value(&record.media_type);
    content.append_value(&record.content);
    size_bytes.append_value(record.size_bytes);
    created_at.append_value(&record.created_at);
    make_batch(
        blobs_schema(),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(sha256.finish()),
            Arc::new(media_type.finish()),
            Arc::new(content.finish()),
            Arc::new(size_bytes.finish()),
            Arc::new(created_at.finish()),
        ],
    )
}

pub(super) fn bundle_batch(record: &SkillBundleVersionRecord) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut bundle_version_id = StringBuilder::new();
    let mut proposal_id = StringBuilder::new();
    let mut workflow_capsule_id = StringBuilder::new();
    let mut previous_bundle_version_id = StringBuilder::new();
    let mut manifest_json = StringBuilder::new();
    let mut manifest_sha256 = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    tenant.append_value(&record.tenant);
    skill_id.append_value(&record.skill_id);
    bundle_version_id.append_value(&record.bundle_version_id);
    proposal_id.append_value(&record.proposal_id);
    workflow_capsule_id.append_value(&record.workflow_capsule_id);
    append_optional(
        &mut previous_bundle_version_id,
        record.previous_bundle_version_id.as_deref(),
    );
    manifest_json.append_value(serde_json::to_string(&record.manifest)?);
    manifest_sha256.append_value(&record.manifest_sha256);
    created_at.append_value(&record.created_at);
    make_batch(
        bundles_schema(),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(bundle_version_id.finish()),
            Arc::new(proposal_id.finish()),
            Arc::new(workflow_capsule_id.finish()),
            Arc::new(previous_bundle_version_id.finish()),
            Arc::new(manifest_json.finish()),
            Arc::new(manifest_sha256.finish()),
            Arc::new(created_at.finish()),
        ],
    )
}

pub(super) fn head_batch(record: &SkillHead) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut bundle_version_id = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    tenant.append_value(&record.tenant);
    skill_id.append_value(&record.skill_id);
    bundle_version_id.append_value(&record.bundle_version_id);
    updated_at.append_value(&record.updated_at);
    make_batch(
        heads_schema(),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(bundle_version_id.finish()),
            Arc::new(updated_at.finish()),
        ],
    )
}

pub(super) fn loadout_batch(record: &AgentLoadoutBinding) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut agent_id = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut mode = StringBuilder::new();
    let mut priority = Int32Builder::new();
    let mut enabled = BooleanBuilder::new();
    let mut visibility = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    tenant.append_value(&record.tenant);
    agent_id.append_value(&record.agent_id);
    skill_id.append_value(&record.skill_id);
    mode.append_value(record.mode.as_db_str());
    priority.append_value(record.priority);
    enabled.append_value(record.enabled);
    visibility.append_value(enum_to_str(&record.visibility)?);
    updated_at.append_value(&record.updated_at);
    make_batch(
        loadouts_schema(),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(agent_id.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(mode.finish()),
            Arc::new(priority.finish()),
            Arc::new(enabled.finish()),
            Arc::new(visibility.finish()),
            Arc::new(updated_at.finish()),
        ],
    )
}

pub(super) fn pin_batch(record: &SessionSkillPin) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut session_id = StringBuilder::new();
    let mut agent_id = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut bundle_version_id = StringBuilder::new();
    let mut pinned_at = StringBuilder::new();
    let mut expires_at = StringBuilder::new();
    let mut revision = UInt64Builder::new();
    tenant.append_value(&record.tenant);
    session_id.append_value(&record.session_id);
    agent_id.append_value(&record.agent_id);
    skill_id.append_value(&record.skill_id);
    bundle_version_id.append_value(&record.bundle_version_id);
    pinned_at.append_value(&record.pinned_at);
    expires_at.append_value(&record.expires_at);
    revision.append_value(record.revision);
    make_batch(
        pins_schema(),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(session_id.finish()),
            Arc::new(agent_id.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(bundle_version_id.finish()),
            Arc::new(pinned_at.finish()),
            Arc::new(expires_at.finish()),
            Arc::new(revision.finish()),
        ],
    )
}

pub(super) fn revocation_batch(
    record: &SkillBundleRevocation,
) -> Result<RecordBatch, StorageError> {
    let mut revocation_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut bundle_version_id = StringBuilder::new();
    let mut reason_code = StringBuilder::new();
    let mut revoked_by_role = StringBuilder::new();
    let mut revoked_at = StringBuilder::new();
    revocation_id.append_value(&record.revocation_id);
    tenant.append_value(&record.tenant);
    skill_id.append_value(&record.skill_id);
    bundle_version_id.append_value(&record.bundle_version_id);
    reason_code.append_value(&record.reason_code);
    revoked_by_role.append_value(&record.revoked_by_role);
    revoked_at.append_value(&record.revoked_at);
    make_batch(
        revocations_schema(),
        vec![
            Arc::new(revocation_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(bundle_version_id.finish()),
            Arc::new(reason_code.finish()),
            Arc::new(revoked_by_role.finish()),
            Arc::new(revoked_at.finish()),
        ],
    )
}

pub(super) fn revision_candidate_batch(
    record: &SkillRevisionCandidate,
) -> Result<RecordBatch, StorageError> {
    let mut job_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut base_bundle_version_id = StringBuilder::new();
    let mut base_capability_capsule_id = StringBuilder::new();
    let mut feedback_event_ids_json = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    job_id.append_value(&record.job_id);
    tenant.append_value(&record.tenant);
    skill_id.append_value(&record.skill_id);
    base_bundle_version_id.append_value(&record.base_bundle_version_id);
    base_capability_capsule_id.append_value(&record.base_capability_capsule_id);
    feedback_event_ids_json.append_value(serde_json::to_string(&record.feedback_event_ids)?);
    created_at.append_value(&record.created_at);
    make_batch(
        revision_candidates_schema(),
        vec![
            Arc::new(job_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(base_bundle_version_id.finish()),
            Arc::new(base_capability_capsule_id.finish()),
            Arc::new(feedback_event_ids_json.finish()),
            Arc::new(created_at.finish()),
        ],
    )
}

pub(super) fn feedback_batch(record: &SkillFeedbackEvent) -> Result<RecordBatch, StorageError> {
    let mut tenant = StringBuilder::new();
    let mut feedback_id = StringBuilder::new();
    let mut skill_id = StringBuilder::new();
    let mut bundle_version_id = StringBuilder::new();
    let mut feedback_kind = StringBuilder::new();
    let mut note = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    tenant.append_value(&record.tenant);
    feedback_id.append_value(&record.feedback_id);
    skill_id.append_value(&record.skill_id);
    bundle_version_id.append_value(&record.bundle_version_id);
    feedback_kind.append_value(&record.feedback_kind);
    append_optional(&mut note, record.note.as_deref());
    created_at.append_value(&record.created_at);
    make_batch(
        feedback_schema(),
        vec![
            Arc::new(tenant.finish()),
            Arc::new(feedback_id.finish()),
            Arc::new(skill_id.finish()),
            Arc::new(bundle_version_id.finish()),
            Arc::new(feedback_kind.finish()),
            Arc::new(note.finish()),
            Arc::new(created_at.finish()),
        ],
    )
}

pub(super) fn compile_decision_batch(
    record: &SkillCompileDecisionRecord,
) -> Result<RecordBatch, StorageError> {
    let mut job_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut input_fingerprint = StringBuilder::new();
    let mut decision_kind = StringBuilder::new();
    let mut canonical_signature = StringBuilder::new();
    let mut target_capability_capsule_id = StringBuilder::new();
    let mut artifact_class = StringBuilder::new();
    let mut reason = StringBuilder::new();
    let mut model_id = StringBuilder::new();
    let mut finish_reason = StringBuilder::new();
    let mut prompt_tokens = UInt64Builder::new();
    let mut completion_tokens = UInt64Builder::new();
    let mut created_at = StringBuilder::new();
    job_id.append_value(&record.job_id);
    tenant.append_value(&record.tenant);
    input_fingerprint.append_value(&record.input_fingerprint);
    decision_kind.append_value(&record.decision_kind);
    append_optional(
        &mut canonical_signature,
        record.canonical_signature.as_deref(),
    );
    append_optional(
        &mut target_capability_capsule_id,
        record.target_capability_capsule_id.as_deref(),
    );
    append_optional(&mut artifact_class, record.artifact_class.as_deref());
    append_optional(&mut reason, record.reason.as_deref());
    model_id.append_value(&record.model_id);
    finish_reason.append_value(&record.finish_reason);
    prompt_tokens.append_value(record.prompt_tokens);
    completion_tokens.append_value(record.completion_tokens);
    created_at.append_value(&record.created_at);
    make_batch(
        compile_decisions_schema(),
        vec![
            Arc::new(job_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(input_fingerprint.finish()),
            Arc::new(decision_kind.finish()),
            Arc::new(canonical_signature.finish()),
            Arc::new(target_capability_capsule_id.finish()),
            Arc::new(artifact_class.finish()),
            Arc::new(reason.finish()),
            Arc::new(model_id.finish()),
            Arc::new(finish_reason.finish()),
            Arc::new(prompt_tokens.finish()),
            Arc::new(completion_tokens.finish()),
            Arc::new(created_at.finish()),
        ],
    )
}

fn make_batch(
    schema: Schema,
    columns: Vec<Arc<dyn arrow_array::Array>>,
) -> Result<RecordBatch, StorageError> {
    RecordBatch::try_new(Arc::new(schema), columns)
        .map_err(|error| StorageError::backend("skill runtime record batch", error))
}

fn optional_string(array: &StringArray, index: usize) -> Option<String> {
    (!array.is_null(index)).then(|| array.value(index).to_string())
}

pub(super) fn parse_proposals(
    batch: &RecordBatch,
) -> Result<Vec<SkillProposalRecord>, StorageError> {
    let proposal_id = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "proposal_id")?;
    let tenant = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "tenant")?;
    let job_id = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "job_id")?;
    let capsule_id = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "capsule_id")?;
    let draft_json = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "draft_json")?;
    let provenance_json = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "provenance_json")?;
    let target_skill_id = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "target_skill_id")?;
    let expected_head_version =
        parse_col::<StringArray>(batch, PROPOSALS_TABLE, "expected_head_version")?;
    let status = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "status")?;
    let created_at = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "created_at")?;
    let updated_at = parse_col::<StringArray>(batch, PROPOSALS_TABLE, "updated_at")?;
    let mut records = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        records.push(SkillProposalRecord {
            proposal_id: proposal_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            job_id: job_id.value(index).to_string(),
            capsule_id: capsule_id.value(index).to_string(),
            draft_json: draft_json.value(index).to_string(),
            provenance_json: provenance_json.value(index).to_string(),
            target_skill_id: optional_string(target_skill_id, index),
            expected_head_version: optional_string(expected_head_version, index),
            status: SkillProposalStatus::from_db_str(status.value(index))
                .ok_or(StorageError::InvalidData("invalid skill proposal status"))?,
            created_at: created_at.value(index).to_string(),
            updated_at: updated_at.value(index).to_string(),
        });
    }
    Ok(records)
}

pub(super) fn parse_blobs(batch: &RecordBatch) -> Result<Vec<SkillResourceBlob>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, BLOBS_TABLE, "tenant")?;
    let sha256 = parse_col::<StringArray>(batch, BLOBS_TABLE, "sha256")?;
    let media_type = parse_col::<StringArray>(batch, BLOBS_TABLE, "media_type")?;
    let content = parse_col::<BinaryArray>(batch, BLOBS_TABLE, "content")?;
    let size_bytes = parse_col::<UInt64Array>(batch, BLOBS_TABLE, "size_bytes")?;
    let created_at = parse_col::<StringArray>(batch, BLOBS_TABLE, "created_at")?;
    Ok((0..batch.num_rows())
        .map(|index| SkillResourceBlob {
            tenant: tenant.value(index).to_string(),
            sha256: sha256.value(index).to_string(),
            media_type: media_type.value(index).to_string(),
            content: content.value(index).to_vec(),
            size_bytes: size_bytes.value(index),
            created_at: created_at.value(index).to_string(),
        })
        .collect())
}

pub(super) fn parse_bundles(
    batch: &RecordBatch,
) -> Result<Vec<SkillBundleVersionRecord>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, BUNDLES_TABLE, "tenant")?;
    let skill_id = parse_col::<StringArray>(batch, BUNDLES_TABLE, "skill_id")?;
    let bundle_version_id = parse_col::<StringArray>(batch, BUNDLES_TABLE, "bundle_version_id")?;
    let proposal_id = parse_col::<StringArray>(batch, BUNDLES_TABLE, "proposal_id")?;
    let workflow_capsule_id =
        parse_col::<StringArray>(batch, BUNDLES_TABLE, "workflow_capsule_id")?;
    let previous_bundle_version_id =
        parse_col::<StringArray>(batch, BUNDLES_TABLE, "previous_bundle_version_id")?;
    let manifest_json = parse_col::<StringArray>(batch, BUNDLES_TABLE, "manifest_json")?;
    let manifest_sha256 = parse_col::<StringArray>(batch, BUNDLES_TABLE, "manifest_sha256")?;
    let created_at = parse_col::<StringArray>(batch, BUNDLES_TABLE, "created_at")?;
    let mut records = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        records.push(SkillBundleVersionRecord {
            tenant: tenant.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            bundle_version_id: bundle_version_id.value(index).to_string(),
            proposal_id: proposal_id.value(index).to_string(),
            workflow_capsule_id: workflow_capsule_id.value(index).to_string(),
            previous_bundle_version_id: optional_string(previous_bundle_version_id, index),
            manifest: serde_json::from_str(manifest_json.value(index))?,
            manifest_sha256: manifest_sha256.value(index).to_string(),
            created_at: created_at.value(index).to_string(),
        });
    }
    Ok(records)
}

pub(super) fn parse_heads(batch: &RecordBatch) -> Result<Vec<SkillHead>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, HEADS_TABLE, "tenant")?;
    let skill_id = parse_col::<StringArray>(batch, HEADS_TABLE, "skill_id")?;
    let bundle_version_id = parse_col::<StringArray>(batch, HEADS_TABLE, "bundle_version_id")?;
    let updated_at = parse_col::<StringArray>(batch, HEADS_TABLE, "updated_at")?;
    Ok((0..batch.num_rows())
        .map(|index| SkillHead {
            tenant: tenant.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            bundle_version_id: bundle_version_id.value(index).to_string(),
            updated_at: updated_at.value(index).to_string(),
        })
        .collect())
}

pub(super) fn parse_loadouts(
    batch: &RecordBatch,
) -> Result<Vec<AgentLoadoutBinding>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, LOADOUTS_TABLE, "tenant")?;
    let agent_id = parse_col::<StringArray>(batch, LOADOUTS_TABLE, "agent_id")?;
    let skill_id = parse_col::<StringArray>(batch, LOADOUTS_TABLE, "skill_id")?;
    let mode = parse_col::<StringArray>(batch, LOADOUTS_TABLE, "mode")?;
    let priority = parse_col::<Int32Array>(batch, LOADOUTS_TABLE, "priority")?;
    let enabled = parse_col::<BooleanArray>(batch, LOADOUTS_TABLE, "enabled")?;
    let visibility = parse_col::<StringArray>(batch, LOADOUTS_TABLE, "visibility")?;
    let updated_at = parse_col::<StringArray>(batch, LOADOUTS_TABLE, "updated_at")?;
    let mut records = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        records.push(AgentLoadoutBinding {
            tenant: tenant.value(index).to_string(),
            agent_id: agent_id.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            mode: AgentLoadoutMode::from_db_str(mode.value(index))
                .ok_or(StorageError::InvalidData("invalid agent loadout mode"))?,
            priority: priority.value(index),
            enabled: enabled.value(index),
            visibility: enum_from_str(visibility.value(index))?,
            updated_at: updated_at.value(index).to_string(),
        });
    }
    Ok(records)
}

pub(super) fn parse_pins(batch: &RecordBatch) -> Result<Vec<SessionSkillPin>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, PINS_TABLE, "tenant")?;
    let session_id = parse_col::<StringArray>(batch, PINS_TABLE, "session_id")?;
    let agent_id = parse_col::<StringArray>(batch, PINS_TABLE, "agent_id")?;
    let skill_id = parse_col::<StringArray>(batch, PINS_TABLE, "skill_id")?;
    let bundle_version_id = parse_col::<StringArray>(batch, PINS_TABLE, "bundle_version_id")?;
    let pinned_at = parse_col::<StringArray>(batch, PINS_TABLE, "pinned_at")?;
    let expires_at = parse_col::<StringArray>(batch, PINS_TABLE, "expires_at")?;
    let revision = parse_col::<UInt64Array>(batch, PINS_TABLE, "revision")?;
    Ok((0..batch.num_rows())
        .map(|index| SessionSkillPin {
            tenant: tenant.value(index).to_string(),
            session_id: session_id.value(index).to_string(),
            agent_id: agent_id.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            bundle_version_id: bundle_version_id.value(index).to_string(),
            pinned_at: pinned_at.value(index).to_string(),
            expires_at: expires_at.value(index).to_string(),
            revision: revision.value(index),
        })
        .collect())
}

pub(super) fn parse_revocations(
    batch: &RecordBatch,
) -> Result<Vec<SkillBundleRevocation>, StorageError> {
    let revocation_id = parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "revocation_id")?;
    let tenant = parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "tenant")?;
    let skill_id = parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "skill_id")?;
    let bundle_version_id =
        parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "bundle_version_id")?;
    let reason_code = parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "reason_code")?;
    let revoked_by_role = parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "revoked_by_role")?;
    let revoked_at = parse_col::<StringArray>(batch, REVOCATIONS_TABLE, "revoked_at")?;
    Ok((0..batch.num_rows())
        .map(|index| SkillBundleRevocation {
            revocation_id: revocation_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            bundle_version_id: bundle_version_id.value(index).to_string(),
            reason_code: reason_code.value(index).to_string(),
            revoked_by_role: revoked_by_role.value(index).to_string(),
            revoked_at: revoked_at.value(index).to_string(),
        })
        .collect())
}

pub(super) fn parse_revision_candidates(
    batch: &RecordBatch,
) -> Result<Vec<SkillRevisionCandidate>, StorageError> {
    let job_id = parse_col::<StringArray>(batch, REVISION_CANDIDATES_TABLE, "job_id")?;
    let tenant = parse_col::<StringArray>(batch, REVISION_CANDIDATES_TABLE, "tenant")?;
    let skill_id = parse_col::<StringArray>(batch, REVISION_CANDIDATES_TABLE, "skill_id")?;
    let base_bundle_version_id =
        parse_col::<StringArray>(batch, REVISION_CANDIDATES_TABLE, "base_bundle_version_id")?;
    let base_capability_capsule_id = parse_col::<StringArray>(
        batch,
        REVISION_CANDIDATES_TABLE,
        "base_capability_capsule_id",
    )?;
    let feedback_event_ids_json =
        parse_col::<StringArray>(batch, REVISION_CANDIDATES_TABLE, "feedback_event_ids_json")?;
    let created_at = parse_col::<StringArray>(batch, REVISION_CANDIDATES_TABLE, "created_at")?;
    let mut records = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        records.push(SkillRevisionCandidate {
            job_id: job_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            base_bundle_version_id: base_bundle_version_id.value(index).to_string(),
            base_capability_capsule_id: base_capability_capsule_id.value(index).to_string(),
            feedback_event_ids: serde_json::from_str(feedback_event_ids_json.value(index))?,
            created_at: created_at.value(index).to_string(),
        });
    }
    Ok(records)
}

pub(super) fn parse_feedback(batch: &RecordBatch) -> Result<Vec<SkillFeedbackEvent>, StorageError> {
    let tenant = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "tenant")?;
    let feedback_id = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "feedback_id")?;
    let skill_id = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "skill_id")?;
    let bundle_version_id = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "bundle_version_id")?;
    let feedback_kind = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "feedback_kind")?;
    let note = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "note")?;
    let created_at = parse_col::<StringArray>(batch, FEEDBACK_TABLE, "created_at")?;
    Ok((0..batch.num_rows())
        .map(|index| SkillFeedbackEvent {
            tenant: tenant.value(index).to_string(),
            feedback_id: feedback_id.value(index).to_string(),
            skill_id: skill_id.value(index).to_string(),
            bundle_version_id: bundle_version_id.value(index).to_string(),
            feedback_kind: feedback_kind.value(index).to_string(),
            note: optional_string(note, index),
            created_at: created_at.value(index).to_string(),
        })
        .collect())
}

pub(super) fn parse_compile_decisions(
    batch: &RecordBatch,
) -> Result<Vec<SkillCompileDecisionRecord>, StorageError> {
    let job_id = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "job_id")?;
    let tenant = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "tenant")?;
    let input_fingerprint =
        parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "input_fingerprint")?;
    let decision_kind = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "decision_kind")?;
    let canonical_signature =
        parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "canonical_signature")?;
    let target_capability_capsule_id = parse_col::<StringArray>(
        batch,
        COMPILE_DECISIONS_TABLE,
        "target_capability_capsule_id",
    )?;
    let artifact_class =
        parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "artifact_class")?;
    let reason = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "reason")?;
    let model_id = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "model_id")?;
    let finish_reason = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "finish_reason")?;
    let prompt_tokens = parse_col::<UInt64Array>(batch, COMPILE_DECISIONS_TABLE, "prompt_tokens")?;
    let completion_tokens =
        parse_col::<UInt64Array>(batch, COMPILE_DECISIONS_TABLE, "completion_tokens")?;
    let created_at = parse_col::<StringArray>(batch, COMPILE_DECISIONS_TABLE, "created_at")?;
    Ok((0..batch.num_rows())
        .map(|index| SkillCompileDecisionRecord {
            job_id: job_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            input_fingerprint: input_fingerprint.value(index).to_string(),
            decision_kind: decision_kind.value(index).to_string(),
            canonical_signature: optional_string(canonical_signature, index),
            target_capability_capsule_id: optional_string(target_capability_capsule_id, index),
            artifact_class: optional_string(artifact_class, index),
            reason: optional_string(reason, index),
            model_id: model_id.value(index).to_string(),
            finish_reason: finish_reason.value(index).to_string(),
            prompt_tokens: prompt_tokens.value(index),
            completion_tokens: completion_tokens.value(index),
            created_at: created_at.value(index).to_string(),
        })
        .collect())
}
