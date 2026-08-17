use std::collections::HashSet;
use std::sync::Arc;

use arrow_array::{
    builder::{StringBuilder, UInt32Builder},
    Array, RecordBatch, StringArray, UInt32Array,
};
use futures::TryStreamExt;
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use lancedb::table::NewColumnTransform;

use super::{ensure_table, lancedb_err, parse_col, sql_quote, LanceStore};
use crate::domain::{
    skill_candidate_evidence_key, skill_candidate_serial_key, ClaimedSkillCandidateJob,
    SkillCandidateEnsureReport, SkillCandidateJob, SkillCandidateJobSpec, SkillCandidateJobStatus,
    SkillCandidateRoundRef, SkillCandidateTriggerReason,
};
use crate::storage::{timestamp_add_ms, StorageError};

pub(super) const TABLE: &str = "skill_candidate_jobs";
const MAX_ENSURE_BATCH: usize = 256;
const MAX_CLAIM_BATCH: usize = 256;
const MAX_NONTERMINAL_SCAN_ROWS: usize = 100_000;
const MAX_STALE_SCAN_BYTES: usize = 64 * 1024 * 1024;
const STALE_REACTIVATION_PAGE_ROWS: usize = 1_000;
const LEASE_HARD_DEADLINE_MS: u128 = 10 * 60 * 1_000;
const MAX_LEASE_RENEWALS: u32 = 2;

#[derive(Debug)]
struct SkillCandidateQueueRow {
    job_id: String,
    serial_key: String,
    status: SkillCandidateJobStatus,
    attempt_count: u32,
    available_at: String,
    lease_expires_at: Option<String>,
    created_at: String,
}

#[derive(Debug)]
struct SkillCandidateStaleRow {
    job_id: String,
    trigger_version: u32,
    status: SkillCandidateJobStatus,
    lease_expires_at: Option<String>,
    trigger_reasons: Vec<SkillCandidateTriggerReason>,
    round_refs: Vec<SkillCandidateRoundRef>,
}

pub(super) async fn ensure_skill_candidate_jobs_table(
    conn: &lancedb::Connection,
) -> Result<(), StorageError> {
    ensure_table(conn, TABLE, schema()).await?;
    let table = conn
        .open_table(TABLE)
        .execute()
        .await
        .map_err(lancedb_err)?;
    let current = table.schema().await.map_err(lancedb_err)?;
    if current.field_with_name("candidate_revision").is_err() {
        table
            .add_columns(
                NewColumnTransform::AllNulls(Arc::new(Schema::new(vec![Field::new(
                    "candidate_revision",
                    DataType::UInt32,
                    true,
                )]))),
                None,
            )
            .await
            .map_err(lancedb_err)?;
    }
    if current.field_with_name("lease_hard_deadline").is_err() {
        table
            .add_columns(
                NewColumnTransform::AllNulls(Arc::new(Schema::new(vec![Field::new(
                    "lease_hard_deadline",
                    DataType::Utf8,
                    true,
                )]))),
                None,
            )
            .await
            .map_err(lancedb_err)?;
    }
    if current.field_with_name("lease_renewal_count").is_err() {
        table
            .add_columns(
                NewColumnTransform::AllNulls(Arc::new(Schema::new(vec![Field::new(
                    "lease_renewal_count",
                    DataType::UInt32,
                    true,
                )]))),
                None,
            )
            .await
            .map_err(lancedb_err)?;
    }
    Ok(())
}

fn schema() -> Schema {
    Schema::new(vec![
        Field::new("job_id", DataType::Utf8, false),
        Field::new("tenant", DataType::Utf8, false),
        Field::new("caller_agent", DataType::Utf8, false),
        Field::new("serial_key", DataType::Utf8, false),
        Field::new("candidate_key", DataType::Utf8, false),
        Field::new("input_fingerprint", DataType::Utf8, false),
        Field::new("candidate_revision", DataType::UInt32, false),
        Field::new("trigger_version", DataType::UInt32, false),
        Field::new("trigger_reasons_json", DataType::Utf8, false),
        Field::new("round_refs_json", DataType::Utf8, false),
        Field::new("tool_call_count", DataType::UInt32, false),
        Field::new("round_count", DataType::UInt32, false),
        Field::new("distinct_session_count", DataType::UInt32, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("attempt_count", DataType::UInt32, false),
        Field::new("available_at", DataType::Utf8, false),
        Field::new("lease_token", DataType::Utf8, true),
        Field::new("lease_expires_at", DataType::Utf8, true),
        Field::new("lease_hard_deadline", DataType::Utf8, true),
        Field::new("lease_renewal_count", DataType::UInt32, true),
        Field::new("last_error_code", DataType::Utf8, true),
        Field::new("created_at", DataType::Utf8, false),
        Field::new("updated_at", DataType::Utf8, false),
        Field::new("completed_at", DataType::Utf8, true),
    ])
}

fn jobs_to_record_batch(jobs: &[SkillCandidateJob]) -> Result<RecordBatch, StorageError> {
    let mut job_id = StringBuilder::new();
    let mut tenant = StringBuilder::new();
    let mut caller_agent = StringBuilder::new();
    let mut serial_key = StringBuilder::new();
    let mut candidate_key = StringBuilder::new();
    let mut input_fingerprint = StringBuilder::new();
    let mut candidate_revision = UInt32Builder::new();
    let mut trigger_version = UInt32Builder::new();
    let mut trigger_reasons_json = StringBuilder::new();
    let mut round_refs_json = StringBuilder::new();
    let mut tool_call_count = UInt32Builder::new();
    let mut round_count = UInt32Builder::new();
    let mut distinct_session_count = UInt32Builder::new();
    let mut status = StringBuilder::new();
    let mut attempt_count = UInt32Builder::new();
    let mut available_at = StringBuilder::new();
    let mut lease_token = StringBuilder::new();
    let mut lease_expires_at = StringBuilder::new();
    let mut lease_hard_deadline = StringBuilder::new();
    let mut lease_renewal_count = UInt32Builder::new();
    let mut last_error_code = StringBuilder::new();
    let mut created_at = StringBuilder::new();
    let mut updated_at = StringBuilder::new();
    let mut completed_at = StringBuilder::new();

    for job in jobs {
        job_id.append_value(&job.job_id);
        tenant.append_value(&job.tenant);
        caller_agent.append_value(&job.caller_agent);
        serial_key.append_value(&job.serial_key);
        candidate_key.append_value(&job.candidate_key);
        input_fingerprint.append_value(&job.input_fingerprint);
        candidate_revision.append_value(job.candidate_revision);
        trigger_version.append_value(job.trigger_version);
        trigger_reasons_json.append_value(serde_json::to_string(&job.trigger_reasons)?);
        round_refs_json.append_value(serde_json::to_string(&job.round_refs)?);
        tool_call_count.append_value(job.tool_call_count);
        round_count.append_value(job.round_count);
        distinct_session_count.append_value(job.distinct_session_count);
        status.append_value(job.status.as_db_str());
        attempt_count.append_value(job.attempt_count);
        available_at.append_value(&job.available_at);
        append_optional(&mut lease_token, job.lease_token.as_deref());
        append_optional(&mut lease_expires_at, job.lease_expires_at.as_deref());
        lease_hard_deadline.append_null();
        lease_renewal_count.append_value(0);
        append_optional(&mut last_error_code, job.last_error_code.as_deref());
        created_at.append_value(&job.created_at);
        updated_at.append_value(&job.updated_at);
        append_optional(&mut completed_at, job.completed_at.as_deref());
    }

    RecordBatch::try_new(
        Arc::new(schema()),
        vec![
            Arc::new(job_id.finish()),
            Arc::new(tenant.finish()),
            Arc::new(caller_agent.finish()),
            Arc::new(serial_key.finish()),
            Arc::new(candidate_key.finish()),
            Arc::new(input_fingerprint.finish()),
            Arc::new(candidate_revision.finish()),
            Arc::new(trigger_version.finish()),
            Arc::new(trigger_reasons_json.finish()),
            Arc::new(round_refs_json.finish()),
            Arc::new(tool_call_count.finish()),
            Arc::new(round_count.finish()),
            Arc::new(distinct_session_count.finish()),
            Arc::new(status.finish()),
            Arc::new(attempt_count.finish()),
            Arc::new(available_at.finish()),
            Arc::new(lease_token.finish()),
            Arc::new(lease_expires_at.finish()),
            Arc::new(lease_hard_deadline.finish()),
            Arc::new(lease_renewal_count.finish()),
            Arc::new(last_error_code.finish()),
            Arc::new(created_at.finish()),
            Arc::new(updated_at.finish()),
            Arc::new(completed_at.finish()),
        ],
    )
    .map_err(|error| StorageError::backend("skill candidate job record batch", error))
}

fn append_optional(builder: &mut StringBuilder, value: Option<&str>) {
    if let Some(value) = value {
        builder.append_value(value);
    } else {
        builder.append_null();
    }
}

impl LanceStore {
    pub async fn ensure_skill_candidate_jobs(
        &self,
        specs: &[SkillCandidateJobSpec],
        now: &str,
    ) -> Result<SkillCandidateEnsureReport, StorageError> {
        if specs.is_empty() {
            return Ok(SkillCandidateEnsureReport::default());
        }
        if !valid_timestamp(now) {
            return Err(StorageError::InvalidInput(
                "skill candidate timestamp is invalid".into(),
            ));
        }
        validate_job_specs(specs)?;
        let ids = specs
            .iter()
            .map(|spec| sql_quote(&spec.job_id))
            .collect::<Vec<_>>()
            .join(", ");
        let existing_rows = self
            .query_skill_candidate_queue_rows(
                Some(format!("job_id IN ({ids})")),
                specs.len().saturating_add(1),
            )
            .await?;
        let existing: HashSet<_> = existing_rows.iter().map(|job| job.job_id.clone()).collect();
        let stale_ids: Vec<_> = existing_rows
            .iter()
            .filter(|job| job.status == SkillCandidateJobStatus::Stale)
            .map(|job| job.job_id.clone())
            .collect();
        let missing: Vec<_> = specs
            .iter()
            .filter(|spec| !existing.contains(&spec.job_id))
            .map(|spec| SkillCandidateJob {
                job_id: spec.job_id.clone(),
                tenant: spec.tenant.clone(),
                caller_agent: spec.caller_agent.clone(),
                serial_key: spec.serial_key.clone(),
                candidate_key: spec.candidate_key.clone(),
                input_fingerprint: spec.input_fingerprint.clone(),
                candidate_revision: spec.candidate_revision,
                trigger_version: spec.trigger_version,
                trigger_reasons: spec.trigger_reasons.clone(),
                round_refs: spec.round_refs.clone(),
                tool_call_count: spec.tool_call_count,
                round_count: spec.round_count,
                distinct_session_count: spec.distinct_session_count,
                status: SkillCandidateJobStatus::Pending,
                attempt_count: 0,
                available_at: now.to_string(),
                lease_token: None,
                lease_expires_at: None,
                last_error_code: None,
                created_at: now.to_string(),
                updated_at: now.to_string(),
                completed_at: None,
            })
            .collect();
        if !missing.is_empty() || !stale_ids.is_empty() {
            let table = self
                .conn
                .open_table(TABLE)
                .execute()
                .await
                .map_err(lancedb_err)?;
            if !missing.is_empty() {
                table
                    .add(jobs_to_record_batch(&missing)?)
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
            }
            for chunk in stale_ids.chunks(100) {
                let ids = chunk
                    .iter()
                    .map(|job_id| sql_quote(job_id))
                    .collect::<Vec<_>>()
                    .join(", ");
                table
                    .update()
                    .only_if(format!("status = 'stale' AND job_id IN ({ids})"))
                    .column("status", "'pending'")
                    .column("available_at", sql_quote(now))
                    .column("lease_token", "CAST(NULL AS string)")
                    .column("lease_expires_at", "CAST(NULL AS string)")
                    .column("lease_hard_deadline", "CAST(NULL AS string)")
                    .column("lease_renewal_count", "0")
                    .column("last_error_code", "CAST(NULL AS string)")
                    .column("updated_at", sql_quote(now))
                    .execute()
                    .await
                    .map_err(lancedb_err)?;
            }
        }
        Ok(SkillCandidateEnsureReport {
            inserted: missing.len(),
            existing: specs.len() - missing.len(),
            staled: 0,
        })
    }

    pub async fn find_invalid_skill_candidate_job_ids(
        &self,
        active_evidence_keys: &HashSet<String>,
        current_trigger_version: u32,
        now: &str,
    ) -> Result<Vec<String>, StorageError> {
        if !valid_timestamp(now) {
            return Err(StorageError::InvalidInput(
                "skill candidate timestamp is invalid".into(),
            ));
        }
        if active_evidence_keys
            .iter()
            .any(|evidence_key| evidence_key.len() != 64 || !evidence_key.is_ascii())
        {
            return Err(StorageError::InvalidInput(
                "skill candidate active evidence key is invalid".into(),
            ));
        }
        let jobs = self
            .query_skill_candidate_stale_rows(
                Some("status IN ('pending', 'retry_wait', 'processing')".to_string()),
                MAX_NONTERMINAL_SCAN_ROWS.saturating_add(1),
                0,
            )
            .await?;
        if jobs.len() > MAX_NONTERMINAL_SCAN_ROWS {
            return Err(StorageError::InvalidInput(
                "skill candidate nonterminal scan limit exceeded".into(),
            ));
        }
        Ok(jobs
            .into_iter()
            .filter(|job| match job.status {
                SkillCandidateJobStatus::Pending | SkillCandidateJobStatus::RetryWait => true,
                SkillCandidateJobStatus::Processing => job
                    .lease_expires_at
                    .as_deref()
                    .is_none_or(|lease_expires_at| lease_expires_at <= now),
                _ => false,
            })
            .filter(|job| {
                job.trigger_version != current_trigger_version
                    || (!job
                        .trigger_reasons
                        .contains(&SkillCandidateTriggerReason::NegativeFeedback)
                        && (job.round_refs.is_empty()
                            || job.round_refs.iter().any(|reference| {
                                !active_evidence_keys.contains(&skill_candidate_evidence_key(
                                    &reference.round_id,
                                    &reference.source_fingerprint,
                                    reference.projector_version,
                                    reference.task_signal_version,
                                ))
                            })))
            })
            .map(|job| job.job_id)
            .collect())
    }

    pub async fn find_reactivatable_skill_candidate_job_ids(
        &self,
        active_evidence_keys: &HashSet<String>,
        current_trigger_version: u32,
    ) -> Result<Vec<String>, StorageError> {
        if active_evidence_keys
            .iter()
            .any(|evidence_key| evidence_key.len() != 64 || !evidence_key.is_ascii())
        {
            return Err(StorageError::InvalidInput(
                "skill candidate active evidence key is invalid".into(),
            ));
        }
        let mut offset = 0_usize;
        let mut reactivatable = Vec::new();
        loop {
            let jobs = self
                .query_skill_candidate_stale_rows(
                    Some(format!(
                        "status = 'stale' AND trigger_version = {current_trigger_version}"
                    )),
                    STALE_REACTIVATION_PAGE_ROWS,
                    offset,
                )
                .await?;
            let page_len = jobs.len();
            reactivatable.extend(
                jobs.into_iter()
                    .filter(|job| {
                        !job.round_refs.is_empty()
                            && job.round_refs.iter().all(|reference| {
                                active_evidence_keys.contains(&skill_candidate_evidence_key(
                                    &reference.round_id,
                                    &reference.source_fingerprint,
                                    reference.projector_version,
                                    reference.task_signal_version,
                                ))
                            })
                    })
                    .map(|job| job.job_id),
            );
            if page_len < STALE_REACTIVATION_PAGE_ROWS {
                break;
            }
            offset = offset.saturating_add(page_len);
        }
        Ok(reactivatable)
    }

    pub async fn stale_skill_candidate_jobs(
        &self,
        stale_ids: &[String],
        now: &str,
    ) -> Result<usize, StorageError> {
        if !valid_timestamp(now) {
            return Err(StorageError::InvalidInput(
                "skill candidate timestamp is invalid".into(),
            ));
        }
        if stale_ids
            .iter()
            .any(|job_id| job_id.is_empty() || job_id.len() > 128)
        {
            return Err(StorageError::InvalidInput(
                "skill candidate stale job id is invalid".into(),
            ));
        }
        if stale_ids.is_empty() {
            return Ok(0);
        }
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        for chunk in stale_ids.chunks(100) {
            let ids = chunk
                .iter()
                .map(|job_id| sql_quote(job_id))
                .collect::<Vec<_>>()
                .join(", ");
            table
                .update()
                .only_if(format!(
                    "status IN ('pending', 'retry_wait', 'processing') AND job_id IN ({ids})"
                ))
                .column("status", "'stale'")
                .column("lease_token", "CAST(NULL AS string)")
                .column("lease_expires_at", "CAST(NULL AS string)")
                .column("lease_hard_deadline", "CAST(NULL AS string)")
                .column("lease_renewal_count", "0")
                .column("last_error_code", "'evidence_superseded'")
                .column("updated_at", sql_quote(now))
                .execute()
                .await
                .map_err(lancedb_err)?;
        }
        Ok(stale_ids.len())
    }

    pub async fn reactivate_skill_candidate_jobs(
        &self,
        job_ids: &[String],
        now: &str,
    ) -> Result<usize, StorageError> {
        if !valid_timestamp(now) {
            return Err(StorageError::InvalidInput(
                "skill candidate timestamp is invalid".into(),
            ));
        }
        if job_ids
            .iter()
            .any(|job_id| job_id.is_empty() || job_id.len() > 128)
        {
            return Err(StorageError::InvalidInput(
                "skill candidate reactivation job id is invalid".into(),
            ));
        }
        if job_ids.is_empty() {
            return Ok(0);
        }
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        for chunk in job_ids.chunks(100) {
            let ids = chunk
                .iter()
                .map(|job_id| sql_quote(job_id))
                .collect::<Vec<_>>()
                .join(", ");
            table
                .update()
                .only_if(format!("status = 'stale' AND job_id IN ({ids})"))
                .column("status", "'pending'")
                .column("available_at", sql_quote(now))
                .column("lease_token", "CAST(NULL AS string)")
                .column("lease_expires_at", "CAST(NULL AS string)")
                .column("lease_hard_deadline", "CAST(NULL AS string)")
                .column("lease_renewal_count", "0")
                .column("last_error_code", "CAST(NULL AS string)")
                .column("updated_at", sql_quote(now))
                .execute()
                .await
                .map_err(lancedb_err)?;
        }
        Ok(job_ids.len())
    }

    pub async fn skill_candidate_nonterminal_count(&self) -> Result<usize, StorageError> {
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .count_rows(Some(
                "status IN ('pending', 'retry_wait', 'processing')".to_string(),
            ))
            .await
            .map_err(lancedb_err)
    }

    pub async fn skill_candidate_reconcile_additions(
        &self,
        specs: &[SkillCandidateJobSpec],
    ) -> Result<usize, StorageError> {
        let mut additions = 0_usize;
        for chunk in specs.chunks(MAX_ENSURE_BATCH) {
            validate_job_specs(chunk)?;
            let ids = chunk
                .iter()
                .map(|spec| sql_quote(&spec.job_id))
                .collect::<Vec<_>>()
                .join(", ");
            let existing = self
                .query_skill_candidate_queue_rows(
                    Some(format!("job_id IN ({ids})")),
                    chunk.len().saturating_add(1),
                )
                .await?;
            let statuses: std::collections::HashMap<_, _> = existing
                .into_iter()
                .map(|job| (job.job_id, job.status))
                .collect();
            additions = additions.saturating_add(
                chunk
                    .iter()
                    .filter(|spec| !statuses.contains_key(&spec.job_id))
                    .count(),
            );
        }
        Ok(additions)
    }

    pub async fn claim_skill_candidate_jobs(
        &self,
        now: &str,
        lease_expires_at: &str,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<ClaimedSkillCandidateJob>, StorageError> {
        self.claim_skill_candidate_jobs_scoped(None, now, lease_expires_at, max_retries, limit)
            .await
    }

    pub async fn claim_skill_candidate_jobs_for_tenant(
        &self,
        tenant: &str,
        now: &str,
        lease_expires_at: &str,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<ClaimedSkillCandidateJob>, StorageError> {
        if tenant.is_empty() || tenant.len() > 256 || tenant.chars().any(char::is_control) {
            return Err(StorageError::InvalidInput(
                "invalid Skill candidate claim tenant".into(),
            ));
        }
        self.claim_skill_candidate_jobs_scoped(
            Some(tenant),
            now,
            lease_expires_at,
            max_retries,
            limit,
        )
        .await
    }

    async fn claim_skill_candidate_jobs_scoped(
        &self,
        tenant: Option<&str>,
        now: &str,
        lease_expires_at: &str,
        max_retries: u32,
        limit: usize,
    ) -> Result<Vec<ClaimedSkillCandidateJob>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if limit > MAX_CLAIM_BATCH {
            return Err(StorageError::InvalidInput(
                "skill candidate claim limit exceeded".into(),
            ));
        }
        if max_retries == 0 {
            return Err(StorageError::InvalidInput(
                "skill candidate max attempts must be positive".into(),
            ));
        }
        if !valid_timestamp(now) || !valid_timestamp(lease_expires_at) || lease_expires_at <= now {
            return Err(StorageError::InvalidInput(
                "skill candidate lease timestamps are invalid".into(),
            ));
        }
        let lease_hard_deadline = timestamp_add_ms(now, LEASE_HARD_DEADLINE_MS);
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let tenant_clause = tenant
            .map(|tenant| format!(" AND tenant = {}", sql_quote(tenant)))
            .unwrap_or_default();
        table
            .update()
            .only_if(format!(
                "status IN ('pending', 'retry_wait') AND attempt_count >= {}{}",
                max_retries, tenant_clause
            ))
            .column("status", "'dead_letter'")
            .column("last_error_code", "'max_attempts_lowered'")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(format!(
                "status = 'processing' AND attempt_count >= {} AND (lease_expires_at IS NULL OR lease_expires_at <= {}){}",
                max_retries,
                sql_quote(now),
                tenant_clause,
            ))
            .column("status", "'dead_letter'")
            .column("lease_token", "CAST(NULL AS string)")
            .column("lease_expires_at", "CAST(NULL AS string)")
            .column("lease_hard_deadline", "CAST(NULL AS string)")
            .column("lease_renewal_count", "0")
            .column("last_error_code", "'lease_expired_after_max_attempts'")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        let queue_filter = format!(
            "status IN ('pending', 'retry_wait', 'processing'){}",
            tenant_clause
        );
        let mut jobs = self
            .query_skill_candidate_queue_rows(
                Some(queue_filter),
                MAX_NONTERMINAL_SCAN_ROWS.saturating_add(1),
            )
            .await?;
        if jobs.len() > MAX_NONTERMINAL_SCAN_ROWS {
            return Err(StorageError::InvalidInput(
                "skill candidate nonterminal scan limit exceeded".into(),
            ));
        }
        jobs.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        let mut seen_serial_keys = HashSet::new();
        let mut selected = Vec::new();
        for job in jobs {
            if !seen_serial_keys.insert(job.serial_key.clone()) {
                continue;
            }
            let eligible = match job.status {
                SkillCandidateJobStatus::Pending | SkillCandidateJobStatus::RetryWait => {
                    job.available_at.as_str() <= now && job.attempt_count < max_retries
                }
                SkillCandidateJobStatus::Processing => {
                    job.attempt_count < max_retries
                        && job
                            .lease_expires_at
                            .as_deref()
                            .is_none_or(|expires_at| expires_at <= now)
                }
                _ => false,
            };
            if eligible {
                selected.push(job);
                if selected.len() == limit {
                    break;
                }
            }
        }

        let mut claimed = Vec::with_capacity(selected.len());
        for job in selected {
            let token = uuid::Uuid::now_v7().to_string();
            let eligibility = match job.status {
                SkillCandidateJobStatus::Pending => "status = 'pending'".to_string(),
                SkillCandidateJobStatus::RetryWait => format!(
                    "status = 'retry_wait' AND available_at <= {} AND attempt_count < {}",
                    sql_quote(now),
                    max_retries
                ),
                SkillCandidateJobStatus::Processing => format!(
                    "status = 'processing' AND attempt_count < {} AND (lease_expires_at IS NULL OR lease_expires_at <= {})",
                    max_retries, sql_quote(now)
                ),
                _ => continue,
            };
            table
                .update()
                .only_if(format!(
                    "job_id = {} AND {eligibility}{}",
                    sql_quote(&job.job_id),
                    tenant_clause,
                ))
                .column("status", "'processing'")
                .column("attempt_count", (job.attempt_count + 1).to_string())
                .column("lease_token", sql_quote(&token))
                .column("lease_expires_at", sql_quote(lease_expires_at))
                .column("lease_hard_deadline", sql_quote(&lease_hard_deadline))
                .column("lease_renewal_count", "0")
                .column("updated_at", sql_quote(now))
                .execute()
                .await
                .map_err(lancedb_err)?;
            if let Some(job) = self
                .query_skill_candidate_jobs(
                    Some(format!(
                        "job_id = {} AND status = 'processing' AND lease_token = {}{}",
                        sql_quote(&job.job_id),
                        sql_quote(&token),
                        tenant_clause,
                    )),
                    2,
                )
                .await?
                .into_iter()
                .next()
            {
                claimed.push(ClaimedSkillCandidateJob {
                    job,
                    lease_token: token,
                });
            }
        }
        Ok(claimed)
    }

    pub async fn complete_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        if !valid_timestamp(now) {
            return Err(StorageError::InvalidInput(
                "skill candidate timestamp is invalid".into(),
            ));
        }
        let filter = format!(
            "job_id = {} AND status = 'processing' AND lease_token = {} AND lease_expires_at > {}",
            sql_quote(job_id),
            sql_quote(lease_token),
            sql_quote(now)
        );
        if self
            .query_skill_candidate_job_ids(Some(filter.clone()), 2)
            .await?
            .is_empty()
        {
            return Err(StorageError::Conflict("skill candidate lease lost"));
        }
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(filter)
            .column("status", "'completed'")
            .column("lease_token", "CAST(NULL AS string)")
            .column("lease_expires_at", "CAST(NULL AS string)")
            .column("lease_hard_deadline", "CAST(NULL AS string)")
            .column("lease_renewal_count", "0")
            .column("completed_at", sql_quote(now))
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn get_skill_candidate_job(
        &self,
        job_id: &str,
    ) -> Result<Option<SkillCandidateJob>, StorageError> {
        if job_id.is_empty() || job_id.len() > 512 || job_id.chars().any(char::is_control) {
            return Err(StorageError::InvalidInput(
                "skill candidate job id is invalid".into(),
            ));
        }
        let jobs = self
            .query_skill_candidate_jobs(Some(format!("job_id = {}", sql_quote(job_id))), 2)
            .await?;
        if jobs.len() > 1 {
            return Err(StorageError::InvalidData(
                "duplicate skill candidate job id",
            ));
        }
        Ok(jobs.into_iter().next())
    }

    pub async fn list_skill_candidate_jobs(
        &self,
        limit: usize,
    ) -> Result<Vec<SkillCandidateJob>, StorageError> {
        if limit == 0 || limit > 1_000 {
            return Err(StorageError::InvalidInput(
                "skill candidate list limit is invalid".into(),
            ));
        }
        self.query_skill_candidate_jobs(None, limit).await
    }

    pub async fn list_skill_candidate_jobs_for_tenant(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidateJob>, StorageError> {
        if tenant.is_empty()
            || tenant.len() > 256
            || tenant.chars().any(char::is_control)
            || limit == 0
            || limit > 1_000
        {
            return Err(StorageError::InvalidInput(
                "tenant-scoped Skill candidate list is invalid".into(),
            ));
        }
        self.query_skill_candidate_jobs(Some(format!("tenant = {}", sql_quote(tenant))), limit)
            .await
    }

    pub async fn preview_skill_candidate_jobs_for_tenant(
        &self,
        tenant: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidateJob>, StorageError> {
        if tenant.is_empty()
            || tenant.len() > 256
            || tenant.chars().any(char::is_control)
            || limit == 0
            || limit > 1_000
        {
            return Err(StorageError::InvalidInput(
                "tenant-scoped Skill candidate preview is invalid".into(),
            ));
        }
        let mut queue = self
            .query_skill_candidate_queue_rows(
                Some(format!(
                    "tenant = {} AND status IN ('pending', 'retry_wait')",
                    sql_quote(tenant),
                )),
                MAX_NONTERMINAL_SCAN_ROWS.saturating_add(1),
            )
            .await?;
        if queue.len() > MAX_NONTERMINAL_SCAN_ROWS {
            return Err(StorageError::InvalidInput(
                "Skill candidate preview capacity exceeded".into(),
            ));
        }
        queue.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.job_id.cmp(&right.job_id))
        });
        let ids: Vec<_> = queue
            .into_iter()
            .take(limit)
            .map(|job| sql_quote(&job.job_id))
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.query_skill_candidate_jobs(Some(format!("job_id IN ({})", ids.join(", "))), limit)
            .await
    }

    pub async fn renew_skill_candidate_job_lease(
        &self,
        job_id: &str,
        lease_token: &str,
        now: &str,
        lease_expires_at: &str,
    ) -> Result<(), StorageError> {
        if !valid_timestamp(now) || !valid_timestamp(lease_expires_at) || lease_expires_at <= now {
            return Err(StorageError::InvalidInput(
                "skill candidate lease timestamps are invalid".into(),
            ));
        }
        let filter = format!(
            "job_id = {} AND status = 'processing' AND lease_token = {} AND lease_expires_at > {}",
            sql_quote(job_id),
            sql_quote(lease_token),
            sql_quote(now)
        );
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let batches: Vec<RecordBatch> = table
            .query()
            .select(Select::columns(&[
                "lease_expires_at",
                "lease_hard_deadline",
                "lease_renewal_count",
            ]))
            .only_if(filter.clone())
            .limit(2)
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("skill candidate lease stream", error))?;
        let mut lease_states = Vec::new();
        for batch in &batches {
            let expires = parse_col::<StringArray>(batch, TABLE, "lease_expires_at")?;
            let hard_deadline = parse_col::<StringArray>(batch, TABLE, "lease_hard_deadline")?;
            let renewal_count = parse_col::<UInt32Array>(batch, TABLE, "lease_renewal_count")?;
            lease_states.extend((0..batch.num_rows()).map(|index| {
                (
                    optional_string(expires, index),
                    optional_string(hard_deadline, index),
                    if renewal_count.is_null(index) {
                        0
                    } else {
                        renewal_count.value(index)
                    },
                )
            }));
        }
        if lease_states.len() != 1 {
            return Err(StorageError::Conflict("skill candidate lease lost"));
        }
        let (current_expiry, hard_deadline, renewal_count) = lease_states
            .pop()
            .expect("lease state length checked above");
        let current_expiry =
            current_expiry.ok_or(StorageError::Conflict("skill candidate lease lost"))?;
        let hard_deadline = hard_deadline
            .filter(|deadline| valid_timestamp(deadline) && deadline.as_str() > now)
            .ok_or(StorageError::Conflict(
                "skill candidate hard deadline reached",
            ))?;
        if renewal_count >= MAX_LEASE_RENEWALS {
            return Err(StorageError::Conflict(
                "skill candidate lease renewal limit reached",
            ));
        }
        let bounded_expiry = lease_expires_at.min(hard_deadline.as_str());
        if bounded_expiry <= current_expiry.as_str() {
            return Err(StorageError::InvalidInput(
                "skill candidate lease renewal must extend the lease".into(),
            ));
        }
        table
            .update()
            .only_if(format!(
                "{filter} AND lease_hard_deadline = {} AND lease_renewal_count = {}",
                sql_quote(&hard_deadline),
                renewal_count,
            ))
            .column("lease_expires_at", sql_quote(bounded_expiry))
            .column("lease_renewal_count", (renewal_count + 1).to_string())
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn fail_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        error_code: &str,
        retry_at: &str,
        now: &str,
        max_attempts: u32,
    ) -> Result<(), StorageError> {
        if max_attempts == 0 {
            return Err(StorageError::InvalidInput(
                "skill candidate max attempts must be positive".into(),
            ));
        }
        if !valid_timestamp(now) || !valid_timestamp(retry_at) {
            return Err(StorageError::InvalidInput(
                "skill candidate retry timestamps are invalid".into(),
            ));
        }
        if error_code.is_empty()
            || error_code.len() > 128
            || !error_code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(StorageError::InvalidInput(
                "skill candidate error code is invalid".into(),
            ));
        }
        let filter = format!(
            "job_id = {} AND status = 'processing' AND lease_token = {} AND lease_expires_at > {}",
            sql_quote(job_id),
            sql_quote(lease_token),
            sql_quote(now)
        );
        let current = self
            .query_skill_candidate_queue_rows(Some(filter.clone()), 2)
            .await?
            .into_iter()
            .next()
            .ok_or(StorageError::Conflict("skill candidate lease lost"))?;
        let exhausted = current.attempt_count >= max_attempts;
        if !exhausted && retry_at <= now {
            return Err(StorageError::InvalidInput(
                "skill candidate retry_at must be after now".into(),
            ));
        }
        let status = if exhausted {
            SkillCandidateJobStatus::DeadLetter
        } else {
            SkillCandidateJobStatus::RetryWait
        };
        let available_at = if exhausted { now } else { retry_at };
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        table
            .update()
            .only_if(filter)
            .column("status", sql_quote(status.as_db_str()))
            .column("available_at", sql_quote(available_at))
            .column("lease_token", "CAST(NULL AS string)")
            .column("lease_expires_at", "CAST(NULL AS string)")
            .column("lease_hard_deadline", "CAST(NULL AS string)")
            .column("lease_renewal_count", "0")
            .column("last_error_code", sql_quote(error_code))
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        Ok(())
    }

    pub async fn stale_claimed_skill_candidate_job(
        &self,
        job_id: &str,
        lease_token: &str,
        now: &str,
    ) -> Result<(), StorageError> {
        if job_id.is_empty()
            || lease_token.is_empty()
            || !valid_timestamp(now)
            || job_id.len() > 128
            || lease_token.len() > 256
        {
            return Err(StorageError::InvalidInput(
                "invalid claimed Skill candidate stale request".into(),
            ));
        }
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let result = table
            .update()
            .only_if(format!(
                "job_id = {} AND status = 'processing' AND lease_token = {}",
                sql_quote(job_id),
                sql_quote(lease_token),
            ))
            .column("status", "'stale'")
            .column("lease_token", "NULL")
            .column("lease_expires_at", "NULL")
            .column("lease_hard_deadline", "NULL")
            .column("lease_renewal_count", "0")
            .column("updated_at", sql_quote(now))
            .execute()
            .await
            .map_err(lancedb_err)?;
        if result.rows_updated != 1 {
            return Err(StorageError::Conflict("skill candidate lease lost"));
        }
        Ok(())
    }

    async fn query_skill_candidate_jobs(
        &self,
        filter: Option<String>,
        limit: usize,
    ) -> Result<Vec<SkillCandidateJob>, StorageError> {
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut query = table.query().limit(limit);
        if let Some(filter) = filter {
            query = query.only_if(filter);
        }
        let batches: Vec<RecordBatch> = query
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("skill candidate job stream", error))?;
        let mut jobs = Vec::new();
        for batch in &batches {
            jobs.extend(record_batch_to_jobs(batch)?);
        }
        Ok(jobs)
    }

    async fn query_skill_candidate_job_ids(
        &self,
        filter: Option<String>,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut query = table
            .query()
            .select(Select::columns(&["job_id"]))
            .limit(limit);
        if let Some(filter) = filter {
            query = query.only_if(filter);
        }
        let batches: Vec<RecordBatch> = query
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("skill candidate job id stream", error))?;
        let mut ids = Vec::new();
        for batch in &batches {
            let job_id = parse_col::<StringArray>(batch, TABLE, "job_id")?;
            ids.extend((0..batch.num_rows()).map(|index| job_id.value(index).to_string()));
        }
        Ok(ids)
    }

    async fn query_skill_candidate_queue_rows(
        &self,
        filter: Option<String>,
        limit: usize,
    ) -> Result<Vec<SkillCandidateQueueRow>, StorageError> {
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut query = table
            .query()
            .select(Select::columns(&[
                "job_id",
                "serial_key",
                "status",
                "attempt_count",
                "available_at",
                "lease_expires_at",
                "created_at",
            ]))
            .limit(limit);
        if let Some(filter) = filter {
            query = query.only_if(filter);
        }
        let batches: Vec<RecordBatch> = query
            .execute()
            .await
            .map_err(lancedb_err)?
            .try_collect()
            .await
            .map_err(|error| StorageError::backend("skill candidate queue stream", error))?;
        let mut rows = Vec::new();
        for batch in &batches {
            rows.extend(record_batch_to_queue_rows(batch)?);
        }
        Ok(rows)
    }

    async fn query_skill_candidate_stale_rows(
        &self,
        filter: Option<String>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SkillCandidateStaleRow>, StorageError> {
        let table = self
            .conn
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lancedb_err)?;
        let mut query = table
            .query()
            .select(Select::columns(&[
                "job_id",
                "trigger_version",
                "status",
                "lease_expires_at",
                "trigger_reasons_json",
                "round_refs_json",
            ]))
            .limit(limit)
            .offset(offset);
        if let Some(filter) = filter {
            query = query.only_if(filter);
        }
        let mut stream = query.execute().await.map_err(lancedb_err)?;
        let mut rows = Vec::new();
        let mut scanned_bytes = 0_usize;
        while let Some(batch) = stream
            .try_next()
            .await
            .map_err(|error| StorageError::backend("skill candidate stale stream", error))?
        {
            let job_id = parse_col::<StringArray>(&batch, TABLE, "job_id")?;
            let lease_expires_at = parse_col::<StringArray>(&batch, TABLE, "lease_expires_at")?;
            let round_refs_json = parse_col::<StringArray>(&batch, TABLE, "round_refs_json")?;
            let trigger_reasons_json =
                parse_col::<StringArray>(&batch, TABLE, "trigger_reasons_json")?;
            for index in 0..batch.num_rows() {
                scanned_bytes = scanned_bytes
                    .saturating_add(job_id.value(index).len())
                    .saturating_add(optional_string_bytes(lease_expires_at, index))
                    .saturating_add(trigger_reasons_json.value(index).len())
                    .saturating_add(round_refs_json.value(index).len());
                if scanned_bytes > MAX_STALE_SCAN_BYTES {
                    crate::metrics::metrics().inc_skill_candidate_capacity_rejection();
                    return Err(StorageError::InvalidInput(
                        "skill candidate stale scan byte limit exceeded".into(),
                    ));
                }
            }
            rows.extend(record_batch_to_stale_rows(&batch)?);
        }
        Ok(rows)
    }
}

fn validate_job_specs(specs: &[SkillCandidateJobSpec]) -> Result<(), StorageError> {
    if specs.len() > MAX_ENSURE_BATCH {
        return Err(StorageError::InvalidInput(
            "skill candidate ensure batch limit exceeded".into(),
        ));
    }
    let mut ids = HashSet::with_capacity(specs.len());
    for spec in specs {
        let scalar_lengths_valid = !spec.job_id.is_empty()
            && spec.job_id.len() <= 128
            && !spec.tenant.is_empty()
            && spec.tenant.len() <= 256
            && !spec.caller_agent.is_empty()
            && spec.caller_agent.len() <= 256
            && spec.serial_key.len() <= 256
            && spec.candidate_key.len() <= 256
            && spec.input_fingerprint.len() <= 256;
        let feedback_trigger =
            spec.trigger_reasons == [SkillCandidateTriggerReason::NegativeFeedback];
        let refs_valid = (feedback_trigger && spec.round_refs.is_empty())
            || (!feedback_trigger
                && !spec.round_refs.is_empty()
                && spec.round_refs.len() <= 8
                && spec.round_refs.iter().all(|reference| {
                    !reference.session_id.is_empty()
                        && reference.session_id.len() <= 1_024
                        && reference.round_id.len() <= 256
                        && reference.source_fingerprint.len() <= 256
                        && reference.generation_id.len() <= 256
                }));
        let serial_key_valid =
            spec.serial_key == skill_candidate_serial_key(&spec.tenant, &spec.caller_agent);
        if !scalar_lengths_valid
            || !refs_valid
            || !serial_key_valid
            || spec.candidate_revision == 0
            || !ids.insert(&spec.job_id)
        {
            return Err(StorageError::InvalidInput(
                "invalid skill candidate job spec".into(),
            ));
        }
    }
    Ok(())
}

fn valid_timestamp(value: &str) -> bool {
    value.len() == 20 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn record_batch_to_queue_rows(
    batch: &RecordBatch,
) -> Result<Vec<SkillCandidateQueueRow>, StorageError> {
    let job_id = parse_col::<StringArray>(batch, TABLE, "job_id")?;
    let serial_key = parse_col::<StringArray>(batch, TABLE, "serial_key")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let attempt_count = parse_col::<UInt32Array>(batch, TABLE, "attempt_count")?;
    let available_at = parse_col::<StringArray>(batch, TABLE, "available_at")?;
    let lease_expires_at = parse_col::<StringArray>(batch, TABLE, "lease_expires_at")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    let mut rows = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        rows.push(SkillCandidateQueueRow {
            job_id: job_id.value(index).to_string(),
            serial_key: serial_key.value(index).to_string(),
            status: SkillCandidateJobStatus::from_db_str(status.value(index)).ok_or(
                StorageError::InvalidData("invalid skill candidate queue status"),
            )?,
            attempt_count: attempt_count.value(index),
            available_at: available_at.value(index).to_string(),
            lease_expires_at: optional_string(lease_expires_at, index),
            created_at: created_at.value(index).to_string(),
        });
    }
    Ok(rows)
}

fn record_batch_to_stale_rows(
    batch: &RecordBatch,
) -> Result<Vec<SkillCandidateStaleRow>, StorageError> {
    let job_id = parse_col::<StringArray>(batch, TABLE, "job_id")?;
    let trigger_version = parse_col::<UInt32Array>(batch, TABLE, "trigger_version")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let lease_expires_at = parse_col::<StringArray>(batch, TABLE, "lease_expires_at")?;
    let trigger_reasons_json = parse_col::<StringArray>(batch, TABLE, "trigger_reasons_json")?;
    let round_refs_json = parse_col::<StringArray>(batch, TABLE, "round_refs_json")?;
    let mut rows = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        rows.push(SkillCandidateStaleRow {
            job_id: job_id.value(index).to_string(),
            trigger_version: trigger_version.value(index),
            status: SkillCandidateJobStatus::from_db_str(status.value(index)).ok_or(
                StorageError::InvalidData("invalid skill candidate stale status"),
            )?,
            lease_expires_at: optional_string(lease_expires_at, index),
            trigger_reasons: serde_json::from_str(trigger_reasons_json.value(index))?,
            round_refs: serde_json::from_str(round_refs_json.value(index))?,
        });
    }
    Ok(rows)
}

fn record_batch_to_jobs(batch: &RecordBatch) -> Result<Vec<SkillCandidateJob>, StorageError> {
    let job_id = parse_col::<StringArray>(batch, TABLE, "job_id")?;
    let tenant = parse_col::<StringArray>(batch, TABLE, "tenant")?;
    let caller_agent = parse_col::<StringArray>(batch, TABLE, "caller_agent")?;
    let serial_key = parse_col::<StringArray>(batch, TABLE, "serial_key")?;
    let candidate_key = parse_col::<StringArray>(batch, TABLE, "candidate_key")?;
    let input_fingerprint = parse_col::<StringArray>(batch, TABLE, "input_fingerprint")?;
    let candidate_revision = parse_col::<UInt32Array>(batch, TABLE, "candidate_revision")?;
    let trigger_version = parse_col::<UInt32Array>(batch, TABLE, "trigger_version")?;
    let trigger_reasons_json = parse_col::<StringArray>(batch, TABLE, "trigger_reasons_json")?;
    let round_refs_json = parse_col::<StringArray>(batch, TABLE, "round_refs_json")?;
    let tool_call_count = parse_col::<UInt32Array>(batch, TABLE, "tool_call_count")?;
    let round_count = parse_col::<UInt32Array>(batch, TABLE, "round_count")?;
    let distinct_session_count = parse_col::<UInt32Array>(batch, TABLE, "distinct_session_count")?;
    let status = parse_col::<StringArray>(batch, TABLE, "status")?;
    let attempt_count = parse_col::<UInt32Array>(batch, TABLE, "attempt_count")?;
    let available_at = parse_col::<StringArray>(batch, TABLE, "available_at")?;
    let lease_token = parse_col::<StringArray>(batch, TABLE, "lease_token")?;
    let lease_expires_at = parse_col::<StringArray>(batch, TABLE, "lease_expires_at")?;
    let last_error_code = parse_col::<StringArray>(batch, TABLE, "last_error_code")?;
    let created_at = parse_col::<StringArray>(batch, TABLE, "created_at")?;
    let updated_at = parse_col::<StringArray>(batch, TABLE, "updated_at")?;
    let completed_at = parse_col::<StringArray>(batch, TABLE, "completed_at")?;
    let mut out = Vec::with_capacity(batch.num_rows());
    for index in 0..batch.num_rows() {
        out.push(SkillCandidateJob {
            job_id: job_id.value(index).to_string(),
            tenant: tenant.value(index).to_string(),
            caller_agent: caller_agent.value(index).to_string(),
            serial_key: serial_key.value(index).to_string(),
            candidate_key: candidate_key.value(index).to_string(),
            input_fingerprint: input_fingerprint.value(index).to_string(),
            candidate_revision: if candidate_revision.is_null(index) {
                0
            } else {
                candidate_revision.value(index)
            },
            trigger_version: trigger_version.value(index),
            trigger_reasons: serde_json::from_str::<Vec<SkillCandidateTriggerReason>>(
                trigger_reasons_json.value(index),
            )?,
            round_refs: serde_json::from_str::<Vec<SkillCandidateRoundRef>>(
                round_refs_json.value(index),
            )?,
            tool_call_count: tool_call_count.value(index),
            round_count: round_count.value(index),
            distinct_session_count: distinct_session_count.value(index),
            status: SkillCandidateJobStatus::from_db_str(status.value(index)).ok_or(
                StorageError::InvalidData("invalid skill candidate job status"),
            )?,
            attempt_count: attempt_count.value(index),
            available_at: available_at.value(index).to_string(),
            lease_token: optional_string(lease_token, index),
            lease_expires_at: optional_string(lease_expires_at, index),
            last_error_code: optional_string(last_error_code, index),
            created_at: created_at.value(index).to_string(),
            updated_at: updated_at.value(index).to_string(),
            completed_at: optional_string(completed_at, index),
        });
    }
    Ok(out)
}

fn optional_string(array: &StringArray, index: usize) -> Option<String> {
    (!array.is_null(index)).then(|| array.value(index).to_string())
}

fn optional_string_bytes(array: &StringArray, index: usize) -> usize {
    if array.is_null(index) {
        0
    } else {
        array.value(index).len()
    }
}
