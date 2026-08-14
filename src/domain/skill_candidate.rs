use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CompletedToolRound, RoundIntegrity};

pub const SKILL_CANDIDATE_TRIGGER_VERSION: u32 = 2;

pub fn skill_candidate_serial_key(tenant: &str, caller_agent: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_candidate.serial_key.v2");
    update_length_prefixed(&mut hash, tenant);
    update_length_prefixed(&mut hash, caller_agent);
    format!("{:x}", hash.finalize())
}

pub fn skill_candidate_evidence_key(
    round_id: &str,
    source_fingerprint: &str,
    projector_version: u32,
    task_signal_version: u32,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mem.skill_candidate.evidence_key.v2");
    update_length_prefixed(&mut hash, round_id);
    update_length_prefixed(&mut hash, source_fingerprint);
    hash.update(projector_version.to_le_bytes());
    hash.update(task_signal_version.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn update_length_prefixed(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidatePolicy {
    pub min_tool_calls: u32,
    pub repeat_min_tool_calls: u32,
    pub repeat_min_rounds: usize,
    pub repeat_min_sessions: usize,
    pub max_evidence: usize,
    pub repeat_window_ms: u64,
    pub trigger_version: u32,
}

impl Default for SkillCandidatePolicy {
    fn default() -> Self {
        Self {
            min_tool_calls: 10,
            repeat_min_tool_calls: 3,
            repeat_min_rounds: 3,
            repeat_min_sessions: 2,
            max_evidence: 8,
            repeat_window_ms: 30 * 24 * 60 * 60 * 1_000,
            trigger_version: SKILL_CANDIDATE_TRIGGER_VERSION,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidateEvidence {
    pub generation_id: String,
    /// Projection observation time. The storage adapter supplies only latest
    /// completed generations; the planner validates each repeat cohort's
    /// event-time span against the policy window.
    pub projected_at: String,
    pub round: SkillCandidateRoundEvidence,
}

/// Minimal, content-free projection consumed by the deterministic planner.
/// Keep this narrower than `CompletedToolRound`: candidate scans must not
/// materialize transcript paths, tool-call IDs/names, or other variable-length
/// locators that do not participate in triggering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCandidateRoundEvidence {
    pub round_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub session_id: Option<String>,
    pub tool_call_count: u32,
    pub matched_result_count: u32,
    pub missing_result_count: u32,
    pub orphan_result_count: u32,
    pub error_result_count: u32,
    pub unknown_result_status_count: u32,
    pub completed_at: Option<String>,
    pub integrity: RoundIntegrity,
    pub source_fingerprint: String,
    pub task_fingerprint: Option<String>,
    pub task_signal_version: u32,
    pub projector_version: u32,
}

impl From<CompletedToolRound> for SkillCandidateRoundEvidence {
    fn from(round: CompletedToolRound) -> Self {
        Self {
            round_id: round.round_id,
            tenant: round.tenant,
            caller_agent: round.caller_agent,
            session_id: round.session_id,
            tool_call_count: round.tool_call_count,
            matched_result_count: round.matched_result_count,
            missing_result_count: round.missing_result_count,
            orphan_result_count: round.orphan_result_count,
            error_result_count: round.error_result_count,
            unknown_result_status_count: round.unknown_result_status_count,
            completed_at: round.completed_at,
            integrity: round.integrity,
            source_fingerprint: round.source_fingerprint,
            task_fingerprint: round.task_fingerprint,
            task_signal_version: round.task_signal_version,
            projector_version: round.projector_version,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum SkillCandidateTriggerReason {
    ToolVolume,
    RepeatedTask,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCandidateRoundRef {
    pub session_id: String,
    pub round_id: String,
    pub source_fingerprint: String,
    pub projector_version: u32,
    /// Legacy rows deserialize as zero and therefore fail current-evidence
    /// validation after task-signal versioning was introduced.
    #[serde(default)]
    pub task_signal_version: u32,
    /// Audit-only; excluded from `input_fingerprint` and job identity.
    pub generation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCandidateJobSpec {
    pub job_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub serial_key: String,
    pub candidate_key: String,
    pub input_fingerprint: String,
    pub candidate_revision: u32,
    pub trigger_version: u32,
    pub trigger_reasons: Vec<SkillCandidateTriggerReason>,
    pub round_refs: Vec<SkillCandidateRoundRef>,
    pub tool_call_count: u32,
    pub round_count: u32,
    pub distinct_session_count: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillCandidateJobStatus {
    Pending,
    Processing,
    RetryWait,
    Completed,
    DeadLetter,
    Stale,
}

impl SkillCandidateJobStatus {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::RetryWait => "retry_wait",
            Self::Completed => "completed",
            Self::DeadLetter => "dead_letter",
            Self::Stale => "stale",
        }
    }

    pub fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "processing" => Some(Self::Processing),
            "retry_wait" => Some(Self::RetryWait),
            "completed" => Some(Self::Completed),
            "dead_letter" => Some(Self::DeadLetter),
            "stale" => Some(Self::Stale),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCandidateJob {
    pub job_id: String,
    pub tenant: String,
    pub caller_agent: String,
    pub serial_key: String,
    pub candidate_key: String,
    pub input_fingerprint: String,
    pub candidate_revision: u32,
    pub trigger_version: u32,
    pub trigger_reasons: Vec<SkillCandidateTriggerReason>,
    pub round_refs: Vec<SkillCandidateRoundRef>,
    pub tool_call_count: u32,
    pub round_count: u32,
    pub distinct_session_count: u32,
    pub status: SkillCandidateJobStatus,
    pub attempt_count: u32,
    pub available_at: String,
    pub lease_token: Option<String>,
    pub lease_expires_at: Option<String>,
    pub last_error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimedSkillCandidateJob {
    pub job: SkillCandidateJob,
    pub lease_token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCandidateEnsureReport {
    pub inserted: usize,
    pub existing: usize,
    pub staled: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillCandidateReconcileReport {
    pub evidence_count: usize,
    pub planned_job_count: usize,
    pub inserted_job_count: usize,
    pub existing_job_count: usize,
    pub staled_job_count: usize,
}
