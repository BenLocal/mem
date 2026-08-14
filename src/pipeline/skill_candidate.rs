use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::domain::{
    skill_candidate_serial_key, RoundIntegrity, SkillCandidateEvidence, SkillCandidateJobSpec,
    SkillCandidatePolicy, SkillCandidateRoundRef, SkillCandidateTriggerReason,
};

/// Pure deterministic planner. Storage supplies only evidence belonging to
/// latest completed generations; this layer validates the event-time span of
/// every repeat cohort against the policy window.
pub fn plan_skill_candidate_jobs(
    evidence: &[SkillCandidateEvidence],
    policy: &SkillCandidatePolicy,
) -> Vec<SkillCandidateJobSpec> {
    let mut groups: BTreeMap<(String, String, String), Vec<&SkillCandidateEvidence>> =
        BTreeMap::new();
    for item in evidence
        .iter()
        .filter(|item| structurally_complete(item) && schedulable(item))
    {
        let candidate_key = item
            .round
            .task_fingerprint
            .clone()
            .expect("schedulable evidence has a task fingerprint");
        groups
            .entry((
                item.round.tenant.clone(),
                item.round.caller_agent.clone(),
                candidate_key,
            ))
            .or_default()
            .push(item);
    }

    groups
        .into_iter()
        .filter_map(|((tenant, caller_agent, candidate_key), mut group)| {
            group.sort_by(|left, right| {
                left.round
                    .completed_at
                    .cmp(&right.round.completed_at)
                    .then_with(|| left.round.round_id.cmp(&right.round.round_id))
            });
            let volume_evidence: Vec<_> = group
                .iter()
                .copied()
                .filter(|item| item.round.tool_call_count >= policy.min_tool_calls)
                .collect();
            let volume = !volume_evidence.is_empty();
            let repeat_evidence: Vec<_> = group
                .iter()
                .copied()
                .filter(|item| {
                    item.round.tool_call_count >= policy.repeat_min_tool_calls
                        && item
                            .round
                            .completed_at
                            .as_deref()
                            .and_then(|value| value.parse::<u64>().ok())
                            .is_some()
                })
                .collect();
            let repeat_cohorts = repeat_cohorts(
                &repeat_evidence,
                policy.repeat_min_rounds,
                policy.repeat_min_sessions,
                policy.max_evidence,
                policy.repeat_window_ms,
            );
            let repeated = !repeat_cohorts.is_empty();
            if !volume && !repeated {
                return None;
            }

            let mut trigger_reasons = Vec::with_capacity(2);
            if volume {
                trigger_reasons.push(SkillCandidateTriggerReason::ToolVolume);
            }
            if repeated {
                trigger_reasons.push(SkillCandidateTriggerReason::RepeatedTask);
            }
            let volume_revision = volume_evidence.len();
            let repeat_revision = repeat_cohorts.len();
            // Both counters are deterministic coverage watermarks. Summing
            // them makes every newly completed volume window or repeat cohort
            // advance the durable receipt revision, even when the other lane
            // is already farther ahead.
            let candidate_revision = volume_revision.saturating_add(repeat_revision).max(1);
            let keep = policy.max_evidence.max(1);
            let volume_selected: Vec<_> = {
                let skip = volume_evidence.len().saturating_sub(keep);
                volume_evidence.into_iter().skip(skip).collect()
            };
            let repeat_selected = repeat_cohorts.last().cloned().unwrap_or_default();
            let selected: Vec<_> = if volume && repeated {
                combined_evidence(&volume_selected, &repeat_selected, keep)
            } else if volume {
                volume_selected
            } else {
                repeat_selected
            };
            let round_refs: Vec<_> = selected
                .iter()
                .filter_map(|item| {
                    Some(SkillCandidateRoundRef {
                        session_id: item.round.session_id.clone()?,
                        round_id: item.round.round_id.clone(),
                        source_fingerprint: item.round.source_fingerprint.clone(),
                        projector_version: item.round.projector_version,
                        task_signal_version: item.round.task_signal_version,
                        generation_id: item.generation_id.clone(),
                    })
                })
                .collect();
            if round_refs.is_empty() {
                return None;
            }
            let input_fingerprint = input_fingerprint(policy.trigger_version, &round_refs);
            let serial_key = skill_candidate_serial_key(&tenant, &caller_agent);
            let mut job_name = b"mem.skill_candidate.job_id.v2".to_vec();
            for value in [
                tenant.as_str(),
                caller_agent.as_str(),
                candidate_key.as_str(),
                input_fingerprint.as_str(),
            ] {
                append_length_prefixed(&mut job_name, value);
            }
            job_name.extend_from_slice(&policy.trigger_version.to_le_bytes());
            let job_id = format!(
                "skill_job_{}",
                Uuid::new_v5(&Uuid::NAMESPACE_OID, &job_name)
            );
            let sessions: BTreeSet<_> = round_refs
                .iter()
                .map(|reference| reference.session_id.as_str())
                .collect();
            Some(SkillCandidateJobSpec {
                job_id,
                tenant,
                caller_agent,
                serial_key,
                candidate_key,
                input_fingerprint,
                candidate_revision: candidate_revision.min(u32::MAX as usize) as u32,
                trigger_version: policy.trigger_version,
                trigger_reasons,
                tool_call_count: selected.iter().map(|item| item.round.tool_call_count).sum(),
                round_count: round_refs.len() as u32,
                distinct_session_count: sessions.len() as u32,
                round_refs,
            })
        })
        .collect()
}

fn repeat_cohorts<'a>(
    evidence: &[&'a SkillCandidateEvidence],
    cohort_size: usize,
    min_sessions: usize,
    max_evidence: usize,
    repeat_window_ms: u64,
) -> Vec<Vec<&'a SkillCandidateEvidence>> {
    if cohort_size == 0
        || min_sessions == 0
        || min_sessions > cohort_size
        || cohort_size > max_evidence
    {
        return Vec::new();
    }
    let mut cohorts = Vec::new();
    let mut cursor = 0_usize;
    while cursor.saturating_add(cohort_size) <= evidence.len() {
        let cohort = &evidence[cursor..cursor + cohort_size];
        let sessions: BTreeSet<_> = cohort
            .iter()
            .filter_map(|item| item.round.session_id.as_deref())
            .collect();
        let within_window = cohort
            .first()
            .zip(cohort.last())
            .and_then(|(first, last)| {
                Some((
                    first.round.completed_at.as_deref()?.parse::<u64>().ok()?,
                    last.round.completed_at.as_deref()?.parse::<u64>().ok()?,
                ))
            })
            .is_some_and(|(first, last)| last.saturating_sub(first) <= repeat_window_ms);
        if sessions.len() >= min_sessions && within_window {
            cohorts.push(cohort.to_vec());
            cursor += cohort_size;
        } else {
            cursor += 1;
        }
    }
    cohorts
}

fn combined_evidence<'a>(
    volume: &[&'a SkillCandidateEvidence],
    repeated: &[&'a SkillCandidateEvidence],
    limit: usize,
) -> Vec<&'a SkillCandidateEvidence> {
    let mut selected = repeated.to_vec();
    let mut seen: BTreeSet<_> = selected
        .iter()
        .map(|item| {
            (
                item.round.round_id.clone(),
                item.round.source_fingerprint.clone(),
            )
        })
        .collect();
    for item in volume.iter().rev() {
        let key = (
            item.round.round_id.clone(),
            item.round.source_fingerprint.clone(),
        );
        if selected.len() < limit && seen.insert(key) {
            selected.push(*item);
        }
    }
    selected.sort_by(|left, right| {
        left.round
            .completed_at
            .cmp(&right.round.completed_at)
            .then_with(|| left.round.round_id.cmp(&right.round.round_id))
    });
    selected
}

fn schedulable(item: &SkillCandidateEvidence) -> bool {
    let round = &item.round;
    !round.tenant.trim().is_empty()
        && round.tenant.len() <= 256
        && !round.tenant.chars().any(char::is_control)
        && !round.caller_agent.trim().is_empty()
        && round.caller_agent.len() <= 256
        && !round.caller_agent.chars().any(char::is_control)
        && round.session_id.as_deref().is_some_and(|session| {
            !session.trim().is_empty()
                && session.len() <= 1_024
                && !session.chars().any(char::is_control)
        })
        && round
            .task_fingerprint
            .as_deref()
            .is_some_and(|fingerprint| !fingerprint.is_empty() && fingerprint.len() <= 256)
}

fn structurally_complete(item: &SkillCandidateEvidence) -> bool {
    let round = &item.round;
    round.integrity == RoundIntegrity::Clean
        && round.tool_call_count > 0
        && round.matched_result_count == round.tool_call_count
        && round.missing_result_count == 0
        && round.orphan_result_count == 0
        && round
            .error_result_count
            .saturating_add(round.unknown_result_status_count)
            <= round.matched_result_count
        && !(round.unknown_result_status_count == 0
            && round.error_result_count == round.tool_call_count)
}

fn input_fingerprint(version: u32, round_refs: &[SkillCandidateRoundRef]) -> String {
    let mut hash = Sha256::new();
    hash.update(version.to_le_bytes());
    for reference in round_refs {
        for value in [
            reference.round_id.as_str(),
            reference.source_fingerprint.as_str(),
        ] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        hash.update(reference.projector_version.to_le_bytes());
        hash.update(reference.task_signal_version.to_le_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn append_length_prefixed(output: &mut Vec<u8>, value: &str) {
    output.extend_from_slice(&(value.len() as u64).to_le_bytes());
    output.extend_from_slice(value.as_bytes());
}
