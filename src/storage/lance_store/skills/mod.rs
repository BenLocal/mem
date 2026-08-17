mod codec;
mod runtime;
mod validation;

use arrow_array::RecordBatch;
use futures::TryStreamExt;
use lancedb::{
    query::{ExecutableQuery, QueryBase},
    Connection,
};

use super::{ensure_table, enum_to_str, lancedb_err, sql_quote, LanceStore};
use crate::{
    domain::{
        AgentLoadoutBinding, SessionSkillPin, SkillBundleVersionRecord, SkillHead,
        SkillProposalRecord, SkillProposalStatus, SkillResourceBlob,
    },
    storage::StorageError,
};

use codec::*;
use validation::*;

const PROPOSALS_TABLE: &str = "skill_proposals";
const BLOBS_TABLE: &str = "skill_resource_blobs";
const BUNDLES_TABLE: &str = "skill_bundle_versions";
const HEADS_TABLE: &str = "skill_heads";
const LOADOUTS_TABLE: &str = "agent_loadout_bindings";
const PINS_TABLE: &str = "session_skill_pins";
const FEEDBACK_TABLE: &str = "skill_feedback_events";
const REVOCATIONS_TABLE: &str = "skill_bundle_revocations";
const REVISION_CANDIDATES_TABLE: &str = "skill_revision_candidates";
const COMPILE_DECISIONS_TABLE: &str = "skill_compile_decisions";

const MAX_ID_BYTES: usize = 512;
const MAX_JSON_BYTES: usize = 1024 * 1024;
const MAX_NOTE_BYTES: usize = 64 * 1024;
const MAX_LIST_ROWS: usize = 1_000;
const MAX_AGENT_LOADOUT_BINDINGS: usize = 64;

pub(super) async fn ensure_skill_runtime_tables(conn: &Connection) -> Result<(), StorageError> {
    ensure_table(conn, PROPOSALS_TABLE, proposals_schema()).await?;
    ensure_table(conn, BLOBS_TABLE, blobs_schema()).await?;
    ensure_table(conn, BUNDLES_TABLE, bundles_schema()).await?;
    ensure_table(conn, HEADS_TABLE, heads_schema()).await?;
    ensure_table(conn, LOADOUTS_TABLE, loadouts_schema()).await?;
    ensure_table(conn, PINS_TABLE, pins_schema()).await?;
    ensure_table(conn, FEEDBACK_TABLE, feedback_schema()).await?;
    ensure_table(conn, REVOCATIONS_TABLE, revocations_schema()).await?;
    ensure_table(
        conn,
        REVISION_CANDIDATES_TABLE,
        revision_candidates_schema(),
    )
    .await?;
    ensure_table(conn, COMPILE_DECISIONS_TABLE, compile_decisions_schema()).await
}

impl LanceStore {
    async fn query_skill_rows(
        &self,
        table_name: &'static str,
        filter: String,
        limit: usize,
    ) -> Result<Vec<RecordBatch>, StorageError> {
        let table = self
            .conn
            .open_table(table_name)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .query()
            .only_if(filter)
            .limit(limit)
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("skill runtime stream", error))
    }

    pub async fn insert_skill_proposal(
        &self,
        proposal: SkillProposalRecord,
    ) -> Result<SkillProposalRecord, StorageError> {
        validate_proposal(&proposal)?;
        if let Some(existing) = self
            .get_skill_proposal(&proposal.tenant, &proposal.proposal_id)
            .await?
        {
            return if same_proposal_payload(&existing, &proposal) {
                Ok(existing)
            } else {
                Err(StorageError::Conflict("skill proposal id already exists"))
            };
        }
        let table = self
            .conn
            .open_table(PROPOSALS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(proposal_batch(&proposal)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(proposal)
    }

    pub async fn get_skill_proposal(
        &self,
        tenant: &str,
        proposal_id: &str,
    ) -> Result<Option<SkillProposalRecord>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(proposal_id, "proposal_id")?;
        let batches = self
            .query_skill_rows(
                PROPOSALS_TABLE,
                format!(
                    "tenant = {} AND proposal_id = {}",
                    sql_quote(tenant),
                    sql_quote(proposal_id)
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_proposals)?,
            "duplicate skill proposals",
        )
    }

    pub async fn get_skill_proposal_by_job(
        &self,
        tenant: &str,
        job_id: &str,
    ) -> Result<Option<SkillProposalRecord>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(job_id, "job_id")?;
        let batches = self
            .query_skill_rows(
                PROPOSALS_TABLE,
                format!(
                    "tenant = {} AND job_id = {}",
                    sql_quote(tenant),
                    sql_quote(job_id),
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_proposals)?,
            "duplicate Skill proposals for one compiler job",
        )
    }

    pub async fn update_skill_proposal_outcome(
        &self,
        tenant: &str,
        proposal_id: &str,
        expected_status: SkillProposalStatus,
        status: SkillProposalStatus,
        updated_at: &str,
    ) -> Result<SkillProposalRecord, StorageError> {
        validate_timestamp(updated_at)?;
        let current = self
            .get_skill_proposal(tenant, proposal_id)
            .await?
            .ok_or(StorageError::NotFound("skill proposal"))?;
        if current.status == status {
            return Ok(current);
        }
        if current.status != expected_status {
            return Err(StorageError::Conflict("skill proposal status changed"));
        }
        if !valid_proposal_transition(expected_status, status) {
            return Err(StorageError::InvalidInput(
                "invalid skill proposal status transition".into(),
            ));
        }
        let table = self
            .conn
            .open_table(PROPOSALS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let result = table
            .update()
            .only_if(format!(
                "tenant = {} AND proposal_id = {} AND status = {}",
                sql_quote(tenant),
                sql_quote(proposal_id),
                sql_quote(expected_status.as_db_str())
            ))
            .column("status", sql_quote(status.as_db_str()))
            .column("updated_at", sql_quote(updated_at))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated != 1 {
            return Err(StorageError::Conflict("skill proposal status changed"));
        }
        self.get_skill_proposal(tenant, proposal_id)
            .await?
            .ok_or(StorageError::InvalidData(
                "skill proposal missing after update",
            ))
    }

    pub async fn put_skill_resource_blob(
        &self,
        blob: SkillResourceBlob,
    ) -> Result<SkillResourceBlob, StorageError> {
        validate_blob(&blob)?;
        if let Some(existing) = self
            .get_skill_resource_blob(&blob.tenant, &blob.sha256)
            .await?
        {
            return if same_blob_payload(&existing, &blob) {
                Ok(existing)
            } else {
                Err(StorageError::Conflict("skill resource hash collision"))
            };
        }
        let table = self
            .conn
            .open_table(BLOBS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(blob_batch(&blob)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(blob)
    }

    pub async fn get_skill_resource_blob(
        &self,
        tenant: &str,
        sha256: &str,
    ) -> Result<Option<SkillResourceBlob>, StorageError> {
        let batches = self
            .query_skill_rows(
                BLOBS_TABLE,
                format!(
                    "tenant = {} AND sha256 = {}",
                    sql_quote(tenant),
                    sql_quote(sha256)
                ),
                2,
            )
            .await?;
        one_row(parse_all(&batches, parse_blobs)?, "duplicate skill blobs")
    }

    pub async fn append_skill_bundle_version(
        &self,
        bundle: SkillBundleVersionRecord,
    ) -> Result<SkillBundleVersionRecord, StorageError> {
        validate_bundle(&bundle)?;
        if let Some(existing) = self
            .get_skill_bundle_version(&bundle.tenant, &bundle.skill_id, &bundle.bundle_version_id)
            .await?
        {
            return if same_bundle_payload(&existing, &bundle) {
                Ok(existing)
            } else {
                Err(StorageError::Conflict(
                    "skill bundle version already exists",
                ))
            };
        }
        let proposal = self
            .get_skill_proposal(&bundle.tenant, &bundle.proposal_id)
            .await?
            .ok_or(StorageError::NotFound("skill proposal"))?;
        if !matches!(
            proposal.status,
            SkillProposalStatus::PendingConfirmation | SkillProposalStatus::Accepted
        ) {
            return Err(StorageError::InvalidInput(
                "skill bundle requires a reviewable or accepted proposal".into(),
            ));
        }
        if proposal
            .target_skill_id
            .as_deref()
            .is_some_and(|target| target != bundle.skill_id)
        {
            return Err(StorageError::InvalidInput(
                "skill bundle target does not match proposal".into(),
            ));
        }
        for resource in &bundle.manifest.resources {
            let blob = self
                .get_skill_resource_blob(&bundle.tenant, &resource.sha256)
                .await?
                .ok_or(StorageError::NotFound("skill resource blob"))?;
            if blob.media_type != resource.media_type || blob.size_bytes != resource.size_bytes {
                return Err(StorageError::InvalidData(
                    "skill resource descriptor does not match blob",
                ));
            }
        }
        let table = self
            .conn
            .open_table(BUNDLES_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(bundle_batch(&bundle)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(bundle)
    }

    pub async fn get_skill_bundle_version(
        &self,
        tenant: &str,
        skill_id: &str,
        bundle_version_id: &str,
    ) -> Result<Option<SkillBundleVersionRecord>, StorageError> {
        let batches = self
            .query_skill_rows(
                BUNDLES_TABLE,
                format!(
                    "tenant = {} AND skill_id = {} AND bundle_version_id = {}",
                    sql_quote(tenant),
                    sql_quote(skill_id),
                    sql_quote(bundle_version_id)
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_bundles)?,
            "duplicate skill bundles",
        )
    }

    pub async fn find_skill_bundle_by_workflow_capsule(
        &self,
        tenant: &str,
        workflow_capsule_id: &str,
    ) -> Result<Option<SkillBundleVersionRecord>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(workflow_capsule_id, "workflow_capsule_id")?;
        let batches = self
            .query_skill_rows(
                BUNDLES_TABLE,
                format!(
                    "tenant = {} AND workflow_capsule_id = {}",
                    sql_quote(tenant),
                    sql_quote(workflow_capsule_id)
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_bundles)?,
            "duplicate workflow Skill bundles",
        )
    }

    pub async fn get_skill_head(
        &self,
        tenant: &str,
        skill_id: &str,
    ) -> Result<Option<SkillHead>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(skill_id, "skill_id")?;
        let batches = self
            .query_skill_rows(
                HEADS_TABLE,
                format!(
                    "tenant = {} AND skill_id = {}",
                    sql_quote(tenant),
                    sql_quote(skill_id)
                ),
                2,
            )
            .await?;
        one_row(parse_all(&batches, parse_heads)?, "duplicate skill heads")
    }

    pub async fn compare_and_set_skill_head(
        &self,
        expected_version: Option<&str>,
        head: SkillHead,
    ) -> Result<SkillHead, StorageError> {
        validate_head(&head)?;
        if self
            .get_skill_bundle_version(&head.tenant, &head.skill_id, &head.bundle_version_id)
            .await?
            .is_none()
        {
            return Err(StorageError::NotFound("skill bundle version"));
        }
        let current = self.get_skill_head(&head.tenant, &head.skill_id).await?;
        if let Some(current) = current {
            if current.bundle_version_id == head.bundle_version_id {
                return Ok(current);
            }
            if expected_version != Some(current.bundle_version_id.as_str()) {
                return Err(StorageError::Conflict("skill head version changed"));
            }
            let table = self
                .conn
                .open_table(HEADS_TABLE)
                .execute()
                .await
                .map_err(lancedb_err)?;
            let result = table
                .update()
                .only_if(format!(
                    "tenant = {} AND skill_id = {} AND bundle_version_id = {}",
                    sql_quote(&head.tenant),
                    sql_quote(&head.skill_id),
                    sql_quote(&current.bundle_version_id)
                ))
                .column("bundle_version_id", sql_quote(&head.bundle_version_id))
                .column("updated_at", sql_quote(&head.updated_at))
                .execute()
                .await
                .map_err(lancedb_err)?;
            if result.rows_updated != 1 {
                return Err(StorageError::Conflict("skill head version changed"));
            }
            return Ok(head);
        }
        if expected_version.is_some() {
            return Err(StorageError::Conflict("skill head is missing"));
        }
        let table = self
            .conn
            .open_table(HEADS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(head_batch(&head)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(head)
    }

    pub async fn bind_agent_loadout(
        &self,
        binding: AgentLoadoutBinding,
    ) -> Result<AgentLoadoutBinding, StorageError> {
        validate_loadout(&binding)?;
        if self
            .get_skill_head(&binding.tenant, &binding.skill_id)
            .await?
            .is_none()
        {
            return Err(StorageError::NotFound("skill head"));
        }
        let current = self
            .get_agent_loadout_binding(&binding.tenant, &binding.agent_id, &binding.skill_id)
            .await?;
        if binding.enabled
            && current.as_ref().is_none_or(|current| !current.enabled)
            && self
                .list_agent_loadout(&binding.tenant, &binding.agent_id, MAX_LIST_ROWS)
                .await?
                .len()
                >= MAX_AGENT_LOADOUT_BINDINGS
        {
            return Err(StorageError::InvalidInput(
                "agent loadout binding limit exceeded".into(),
            ));
        }
        if current.as_ref() == Some(&binding) {
            return Ok(binding);
        }
        let table = self
            .conn
            .open_table(LOADOUTS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        if current.is_some() {
            let result = table
                .update()
                .only_if(format!(
                    "tenant = {} AND agent_id = {} AND skill_id = {}",
                    sql_quote(&binding.tenant),
                    sql_quote(&binding.agent_id),
                    sql_quote(&binding.skill_id)
                ))
                .column("mode", sql_quote(binding.mode.as_db_str()))
                .column("priority", binding.priority.to_string())
                .column("enabled", binding.enabled.to_string())
                .column("visibility", sql_quote(&enum_to_str(&binding.visibility)?))
                .column("updated_at", sql_quote(&binding.updated_at))
                .execute()
                .await
                .map_err(lancedb_err)?;
            if result.rows_updated != 1 {
                return Err(StorageError::Conflict("agent loadout binding changed"));
            }
        } else {
            table
                .add(loadout_batch(&binding)?)
                .execute()
                .await
                .map_err(lancedb_err)?;
        }
        Ok(binding)
    }

    pub async fn get_agent_loadout_binding(
        &self,
        tenant: &str,
        agent_id: &str,
        skill_id: &str,
    ) -> Result<Option<AgentLoadoutBinding>, StorageError> {
        let batches = self
            .query_skill_rows(
                LOADOUTS_TABLE,
                format!(
                    "tenant = {} AND agent_id = {} AND skill_id = {}",
                    sql_quote(tenant),
                    sql_quote(agent_id),
                    sql_quote(skill_id)
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_loadouts)?,
            "duplicate loadout bindings",
        )
    }

    pub async fn list_agent_loadout(
        &self,
        tenant: &str,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<AgentLoadoutBinding>, StorageError> {
        validate_key(tenant, "tenant")?;
        validate_key(agent_id, "agent_id")?;
        validate_limit(limit)?;
        let batches = self
            .query_skill_rows(
                LOADOUTS_TABLE,
                format!(
                    "tenant = {} AND agent_id = {} AND enabled = true",
                    sql_quote(tenant),
                    sql_quote(agent_id)
                ),
                MAX_LIST_ROWS,
            )
            .await?;
        let mut rows = parse_all(&batches, parse_loadouts)?;
        rows.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.skill_id.cmp(&right.skill_id))
        });
        rows.truncate(limit);
        Ok(rows)
    }

    pub async fn get_or_pin_session_skill(
        &self,
        pin: SessionSkillPin,
    ) -> Result<SessionSkillPin, StorageError> {
        validate_pin(&pin)?;
        if let Some(existing) = self
            .get_session_skill_pin(&pin.tenant, &pin.session_id, &pin.agent_id, &pin.skill_id)
            .await?
        {
            if existing.expires_at > pin.pinned_at {
                return Ok(existing);
            }
            if self
                .get_skill_bundle_version(&pin.tenant, &pin.skill_id, &pin.bundle_version_id)
                .await?
                .is_none()
            {
                return Err(StorageError::NotFound("skill bundle version"));
            }
            let next = SessionSkillPin {
                revision: existing
                    .revision
                    .checked_add(1)
                    .ok_or(StorageError::Conflict(
                        "session Skill pin revision exhausted",
                    ))?,
                ..pin
            };
            let table = self
                .conn
                .open_table(PINS_TABLE)
                .execute()
                .await
                .map_err(lancedb_err)?;
            let result = table
                .update()
                .only_if(format!(
                    "tenant = {} AND session_id = {} AND agent_id = {} AND skill_id = {} AND revision = {}",
                    sql_quote(&next.tenant),
                    sql_quote(&next.session_id),
                    sql_quote(&next.agent_id),
                    sql_quote(&next.skill_id),
                    existing.revision,
                ))
                .column("bundle_version_id", sql_quote(&next.bundle_version_id))
                .column("pinned_at", sql_quote(&next.pinned_at))
                .column("expires_at", sql_quote(&next.expires_at))
                .column("revision", next.revision.to_string())
                .execute()
                .await
                .map_err(lancedb_err)?;
            if result.rows_updated != 1 {
                return Err(StorageError::Conflict("session Skill pin changed"));
            }
            return Ok(next);
        }
        if self
            .get_skill_bundle_version(&pin.tenant, &pin.skill_id, &pin.bundle_version_id)
            .await?
            .is_none()
        {
            return Err(StorageError::NotFound("skill bundle version"));
        }
        let table = self
            .conn
            .open_table(PINS_TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .add(pin_batch(&pin)?)
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(pin)
    }

    pub async fn get_session_skill_pin(
        &self,
        tenant: &str,
        session_id: &str,
        agent_id: &str,
        skill_id: &str,
    ) -> Result<Option<SessionSkillPin>, StorageError> {
        let batches = self
            .query_skill_rows(
                PINS_TABLE,
                format!(
                    "tenant = {} AND session_id = {} AND agent_id = {} AND skill_id = {}",
                    sql_quote(tenant),
                    sql_quote(session_id),
                    sql_quote(agent_id),
                    sql_quote(skill_id)
                ),
                2,
            )
            .await?;
        one_row(
            parse_all(&batches, parse_pins)?,
            "duplicate session skill pins",
        )
    }
}

fn same_proposal_payload(left: &SkillProposalRecord, right: &SkillProposalRecord) -> bool {
    left.proposal_id == right.proposal_id
        && left.tenant == right.tenant
        && left.job_id == right.job_id
        && left.capsule_id == right.capsule_id
        && left.draft_json == right.draft_json
        && left.provenance_json == right.provenance_json
        && left.target_skill_id == right.target_skill_id
        && left.expected_head_version == right.expected_head_version
}

fn same_blob_payload(left: &SkillResourceBlob, right: &SkillResourceBlob) -> bool {
    left.tenant == right.tenant
        && left.sha256 == right.sha256
        && left.media_type == right.media_type
        && left.content == right.content
        && left.size_bytes == right.size_bytes
}

fn same_bundle_payload(left: &SkillBundleVersionRecord, right: &SkillBundleVersionRecord) -> bool {
    left.tenant == right.tenant
        && left.skill_id == right.skill_id
        && left.bundle_version_id == right.bundle_version_id
        && left.proposal_id == right.proposal_id
        && left.workflow_capsule_id == right.workflow_capsule_id
        && left.previous_bundle_version_id == right.previous_bundle_version_id
        && left.manifest == right.manifest
        && left.manifest_sha256 == right.manifest_sha256
}
