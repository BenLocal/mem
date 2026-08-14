mod common;

use mem::domain::{
    skill_candidate_serial_key, CompletedToolRound, CompletedToolRoundIndexBuild,
    RoundIndexBuildStatus, RoundIntegrity, RoundSealKind, SkillCandidateEvidence,
    SkillCandidateJobSpec, SkillCandidatePolicy, SkillCandidateTriggerReason, SourceAdapter,
    COMPLETED_TOOL_ROUND_PROJECTOR_VERSION, COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
};
use mem::pipeline::skill_candidate::plan_skill_candidate_jobs;
use mem::service::SkillCandidateService;
use mem::storage::{
    current_timestamp, CompletedToolRoundStore, SkillCandidateStore, StorageError, Store,
};

fn completed_round(
    round_id: &str,
    session_id: &str,
    task_fingerprint: &str,
    tool_call_count: u32,
) -> CompletedToolRound {
    CompletedToolRound {
        round_id: round_id.into(),
        tenant: "local".into(),
        caller_agent: "codex".into(),
        source_adapter: SourceAdapter::Codex,
        session_id: Some(session_id.into()),
        transcript_path: format!("/tmp/{session_id}.jsonl"),
        start_line_number: 1,
        start_block_index: 0,
        end_line_number: 40,
        end_block_index: 0,
        start_message_uuid: None,
        final_message_uuid: None,
        tool_call_ids: (0..tool_call_count)
            .map(|index| format!("call-{index}"))
            .collect(),
        tool_names: vec!["exec_command".into()],
        tool_call_count,
        matched_result_count: tool_call_count,
        missing_result_count: 0,
        orphan_result_count: 0,
        error_result_count: 0,
        unknown_result_status_count: tool_call_count,
        completed_at: Some("00000001786800000000".into()),
        seal_kind: RoundSealKind::StreamEof,
        integrity: RoundIntegrity::Clean,
        source_fingerprint: format!("source-{round_id}"),
        task_fingerprint: Some(task_fingerprint.into()),
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        tool_pattern_fingerprint: "tool-pattern-shell".into(),
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
    }
}

fn evidence(round: CompletedToolRound, generation_id: &str) -> SkillCandidateEvidence {
    SkillCandidateEvidence {
        generation_id: generation_id.into(),
        projected_at: "00000001786800000000".into(),
        round: round.into(),
    }
}

fn evidence_at(
    mut round: CompletedToolRound,
    generation_id: &str,
    projected_at_ms: u64,
) -> SkillCandidateEvidence {
    round.completed_at = Some(format!("{projected_at_ms:020}"));
    SkillCandidateEvidence {
        generation_id: generation_id.into(),
        projected_at: format!("{projected_at_ms:020}"),
        round: round.into(),
    }
}

fn job_spec(job_id: &str, tenant: &str, caller_agent: &str) -> SkillCandidateJobSpec {
    let mut spec = plan_skill_candidate_jobs(
        &[evidence(
            completed_round(job_id, job_id, job_id, 10),
            "generation-1",
        )],
        &SkillCandidatePolicy::default(),
    )
    .remove(0);
    spec.job_id = job_id.into();
    spec.tenant = tenant.into();
    spec.caller_agent = caller_agent.into();
    spec.serial_key = skill_candidate_serial_key(tenant, caller_agent);
    spec.candidate_key = format!("candidate/{job_id}");
    spec.input_fingerprint = format!("input/{job_id}");
    spec
}

fn completed_build(
    generation_id: &str,
    session_id: &str,
    completed_at: &str,
) -> CompletedToolRoundIndexBuild {
    CompletedToolRoundIndexBuild {
        generation_id: generation_id.into(),
        tenant: "local".into(),
        session_id: session_id.into(),
        projector_version: COMPLETED_TOOL_ROUND_PROJECTOR_VERSION,
        task_signal_version: COMPLETED_TOOL_ROUND_TASK_SIGNAL_VERSION,
        status: RoundIndexBuildStatus::Completed,
        source_block_count: 5,
        source_fingerprint: format!("build/{generation_id}"),
        round_count: 1,
        started_at: completed_at.into(),
        completed_at: Some(completed_at.into()),
    }
}

#[test]
fn clean_ten_call_codex_round_triggers_despite_unknown_result_status() {
    let jobs = plan_skill_candidate_jobs(
        &[evidence(
            completed_round("round-1", "session-1", "task-a", 10),
            "generation-1",
        )],
        &SkillCandidatePolicy::default(),
    );

    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].trigger_reasons,
        vec![SkillCandidateTriggerReason::ToolVolume]
    );
    assert_eq!(jobs[0].tool_call_count, 10);
    assert_eq!(jobs[0].round_refs.len(), 1);
    assert_eq!(jobs[0].round_refs[0].round_id, "round-1");
}

#[test]
fn tenant_agent_serial_key_is_unambiguous_across_field_boundaries() {
    assert_ne!(
        skill_candidate_serial_key("a", "b\0c"),
        skill_candidate_serial_key("a\0b", "c")
    );
}

#[test]
fn repeat_signal_ignores_evidence_older_than_the_policy_window() {
    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
    const NOW: u64 = 1_786_800_000_000;
    let jobs = plan_skill_candidate_jobs(
        &[
            evidence_at(
                completed_round("round-old", "session-1", "task-repeat", 3),
                "generation-old",
                NOW - 31 * DAY_MS,
            ),
            evidence_at(
                completed_round("round-2", "session-1", "task-repeat", 3),
                "generation-2",
                NOW - DAY_MS,
            ),
            evidence_at(
                completed_round("round-3", "session-2", "task-repeat", 3),
                "generation-3",
                NOW,
            ),
        ],
        &SkillCandidatePolicy::default(),
    );

    assert!(jobs.is_empty());
}

#[test]
fn rebuild_time_does_not_refresh_old_repeat_evidence() {
    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
    const NOW: u64 = 1_786_800_000_000;
    let mut old = evidence_at(
        completed_round("round-old", "session-1", "task-repeat", 3),
        "generation-old",
        NOW - 31 * DAY_MS,
    );
    let mut recent_one = evidence_at(
        completed_round("round-2", "session-1", "task-repeat", 3),
        "generation-2",
        NOW - DAY_MS,
    );
    let mut recent_two = evidence_at(
        completed_round("round-3", "session-2", "task-repeat", 3),
        "generation-3",
        NOW,
    );
    for item in [&mut old, &mut recent_one, &mut recent_two] {
        item.projected_at = format!("{:020}", NOW + DAY_MS);
    }

    assert!(plan_skill_candidate_jobs(
        &[old, recent_one, recent_two],
        &SkillCandidatePolicy::default(),
    )
    .is_empty());
}

#[test]
fn repeated_task_requires_three_complete_rounds_across_two_sessions() {
    let jobs = plan_skill_candidate_jobs(
        &[
            evidence_at(
                completed_round("round-1", "session-1", "task-repeat", 3),
                "generation-1",
                1_786_800_000_000,
            ),
            evidence_at(
                completed_round("round-2", "session-1", "task-repeat", 3),
                "generation-2",
                1_786_800_001_000,
            ),
            evidence_at(
                completed_round("round-3", "session-2", "task-repeat", 3),
                "generation-3",
                1_786_800_002_000,
            ),
        ],
        &SkillCandidatePolicy::default(),
    );

    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].trigger_reasons,
        vec![SkillCandidateTriggerReason::RepeatedTask]
    );
    assert_eq!(jobs[0].round_count, 3);
    assert_eq!(jobs[0].distinct_session_count, 2);
}

#[test]
fn every_repeat_cohort_itself_spans_the_required_sessions() {
    let jobs = plan_skill_candidate_jobs(
        &[
            evidence_at(
                completed_round("round-1", "session-a", "task-repeat", 3),
                "generation-1",
                1_786_800_000_000,
            ),
            evidence_at(
                completed_round("round-2", "session-a", "task-repeat", 3),
                "generation-2",
                1_786_800_001_000,
            ),
            evidence_at(
                completed_round("round-3", "session-a", "task-repeat", 3),
                "generation-3",
                1_786_800_002_000,
            ),
            evidence_at(
                completed_round("round-4", "session-b", "task-repeat", 3),
                "generation-4",
                1_786_800_003_000,
            ),
        ],
        &SkillCandidatePolicy::default(),
    );

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].distinct_session_count, 2);
    assert_eq!(
        jobs[0]
            .round_refs
            .iter()
            .map(|reference| reference.round_id.as_str())
            .collect::<Vec<_>>(),
        vec!["round-2", "round-3", "round-4"]
    );
}

#[test]
fn repeat_job_identity_is_stable_when_an_older_cohort_leaves_the_window() {
    const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
    const START: u64 = 1_786_800_000_000;
    let first_window = vec![
        evidence_at(
            completed_round("old-1", "session-a", "task-repeat", 3),
            "generation-old-1",
            START,
        ),
        evidence_at(
            completed_round("old-2", "session-a", "task-repeat", 3),
            "generation-old-2",
            START + 1,
        ),
        evidence_at(
            completed_round("old-3", "session-b", "task-repeat", 3),
            "generation-old-3",
            START + 2,
        ),
        evidence_at(
            completed_round("current-1", "session-c", "task-repeat", 3),
            "generation-current-1",
            START + 29 * DAY_MS,
        ),
        evidence_at(
            completed_round("current-2", "session-c", "task-repeat", 3),
            "generation-current-2",
            START + 29 * DAY_MS + 1,
        ),
        evidence_at(
            completed_round("current-3", "session-d", "task-repeat", 3),
            "generation-current-3",
            START + 29 * DAY_MS + 2,
        ),
    ];
    let before_expiry = plan_skill_candidate_jobs(&first_window, &SkillCandidatePolicy::default());
    let mut after_expiry_evidence = first_window;
    after_expiry_evidence.push(evidence_at(
        completed_round("trailing", "session-d", "task-repeat", 3),
        "generation-trailing",
        START + 31 * DAY_MS,
    ));
    let after_expiry =
        plan_skill_candidate_jobs(&after_expiry_evidence, &SkillCandidatePolicy::default());

    assert_eq!(before_expiry.len(), 1);
    assert_eq!(after_expiry.len(), 1);
    assert_eq!(
        before_expiry[0].input_fingerprint,
        after_expiry[0].input_fingerprint
    );
    assert_eq!(before_expiry[0].job_id, after_expiry[0].job_id);
}

#[test]
fn incomplete_or_all_failed_rounds_never_trigger() {
    let mut gapped = completed_round("gapped", "session-1", "task-a", 10);
    gapped.integrity = RoundIntegrity::Gapped;
    gapped.matched_result_count = 9;
    gapped.missing_result_count = 1;
    let mut all_failed = completed_round("failed", "session-2", "task-b", 10);
    all_failed.unknown_result_status_count = 0;
    all_failed.error_result_count = 10;

    let jobs = plan_skill_candidate_jobs(
        &[
            evidence(gapped, "generation-1"),
            evidence(all_failed, "generation-2"),
        ],
        &SkillCandidatePolicy::default(),
    );

    assert!(jobs.is_empty());
}

#[test]
fn malformed_scope_or_missing_task_signal_cannot_poison_planning() {
    let mut malformed = completed_round("bad", "session-bad", "task-bad", 10);
    malformed.caller_agent = "x".repeat(257);
    let mut missing_signal = completed_round("none", "session-none", "task-none", 10);
    missing_signal.task_fingerprint = None;
    let valid = completed_round("good", "session-good", "task-good", 10);

    let jobs = plan_skill_candidate_jobs(
        &[
            evidence(malformed, "generation-bad"),
            evidence(missing_signal, "generation-none"),
            evidence(valid, "generation-good"),
        ],
        &SkillCandidatePolicy::default(),
    );

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].candidate_key, "task-good");
}

#[test]
fn trailing_evidence_waits_for_the_next_deterministic_cohort() {
    let policy = SkillCandidatePolicy::default();
    let first_three = vec![
        evidence_at(
            completed_round("round-1", "session-1", "task-repeat", 3),
            "generation-1",
            1_786_800_000_000,
        ),
        evidence_at(
            completed_round("round-2", "session-1", "task-repeat", 3),
            "generation-2",
            1_786_800_001_000,
        ),
        evidence_at(
            completed_round("round-3", "session-2", "task-repeat", 3),
            "generation-3",
            1_786_800_002_000,
        ),
    ];
    let first = plan_skill_candidate_jobs(&first_three, &policy);
    let mut with_later = first_three;
    with_later.push(evidence_at(
        completed_round("round-4", "session-3", "task-repeat", 3),
        "generation-4",
        1_786_800_003_000,
    ));
    let later = plan_skill_candidate_jobs(&with_later, &policy);

    assert_eq!(first.len(), 1);
    assert_eq!(later.len(), 1);
    assert_eq!(first[0].job_id, later[0].job_id);
    assert_eq!(first[0].input_fingerprint, later[0].input_fingerprint);
    assert_eq!(later[0].candidate_revision, 1);

    let mut second_cohort = with_later;
    second_cohort.extend([
        evidence_at(
            completed_round("round-5", "session-3", "task-repeat", 3),
            "generation-5",
            1_786_800_004_000,
        ),
        evidence_at(
            completed_round("round-6", "session-4", "task-repeat", 3),
            "generation-6",
            1_786_800_005_000,
        ),
    ]);
    let revised = plan_skill_candidate_jobs(&second_cohort, &policy);
    assert_eq!(revised[0].candidate_revision, 2);
    assert_ne!(first[0].job_id, revised[0].job_id);
    assert_eq!(
        revised[0]
            .round_refs
            .iter()
            .map(|reference| reference.round_id.as_str())
            .collect::<Vec<_>>(),
        vec!["round-4", "round-5", "round-6"]
    );
}

#[test]
fn combined_volume_and_repeat_trigger_keeps_evidence_for_both_reasons() {
    let jobs = plan_skill_candidate_jobs(
        &[
            evidence_at(
                completed_round("round-volume", "session-1", "task-combined", 10),
                "generation-1",
                1_786_800_000_000,
            ),
            evidence_at(
                completed_round("round-repeat-2", "session-1", "task-combined", 3),
                "generation-2",
                1_786_800_001_000,
            ),
            evidence_at(
                completed_round("round-repeat-3", "session-2", "task-combined", 3),
                "generation-3",
                1_786_800_002_000,
            ),
        ],
        &SkillCandidatePolicy::default(),
    );

    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].trigger_reasons,
        vec![
            SkillCandidateTriggerReason::ToolVolume,
            SkillCandidateTriggerReason::RepeatedTask,
        ]
    );
    assert_eq!(jobs[0].candidate_revision, 2);
    assert_eq!(jobs[0].round_count, 3);
    assert_eq!(jobs[0].distinct_session_count, 2);
    assert_eq!(
        jobs[0]
            .round_refs
            .iter()
            .map(|reference| reference.round_id.as_str())
            .collect::<Vec<_>>(),
        vec!["round-volume", "round-repeat-2", "round-repeat-3"]
    );
}

#[test]
fn volume_revisions_use_the_latest_bounded_evidence_window() {
    let policy = SkillCandidatePolicy {
        repeat_min_tool_calls: 11,
        ..SkillCandidatePolicy::default()
    };
    let evidence = (1..=10)
        .map(|index| {
            evidence_at(
                completed_round(
                    &format!("round-{index:02}"),
                    &format!("session-{index:02}"),
                    "task-volume",
                    10,
                ),
                &format!("generation-{index:02}"),
                1_786_800_000_000 + index,
            )
        })
        .collect::<Vec<_>>();

    let jobs = plan_skill_candidate_jobs(&evidence, &policy);

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].candidate_revision, 10);
    assert_eq!(jobs[0].round_count, 8);
    assert_eq!(
        jobs[0]
            .round_refs
            .iter()
            .map(|reference| reference.round_id.as_str())
            .collect::<Vec<_>>(),
        vec![
            "round-03", "round-04", "round-05", "round-06", "round-07", "round-08", "round-09",
            "round-10",
        ]
    );
}

#[tokio::test]
async fn generation_replay_is_a_durable_receipt_not_a_second_job() {
    let (_dir, store) = common::test_store().await;
    let first_plan = plan_skill_candidate_jobs(
        &[evidence(
            completed_round("round-1", "session-1", "task-a", 10),
            "generation-1",
        )],
        &SkillCandidatePolicy::default(),
    );
    let mut replay_evidence = evidence(
        completed_round("round-1", "session-1", "task-a", 10),
        "generation-2",
    );
    replay_evidence.projected_at = "00000001786800001000".into();
    let replay_plan =
        plan_skill_candidate_jobs(&[replay_evidence], &SkillCandidatePolicy::default());
    assert_eq!(first_plan[0].job_id, replay_plan[0].job_id);

    let first = store
        .ensure_skill_candidate_jobs(&first_plan, "00000001786800002000")
        .await
        .unwrap();
    let replay = store
        .ensure_skill_candidate_jobs(&replay_plan, "00000001786800003000")
        .await
        .unwrap();

    assert_eq!(first.inserted, 1);
    assert_eq!(first.existing, 0);
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.existing, 1);
    let claimed = store
        .claim_skill_candidate_jobs("00000001786800004000", "00000001786800304000", 3, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job.job_id, first_plan[0].job_id);

    store
        .complete_skill_candidate_job(
            &claimed[0].job.job_id,
            &claimed[0].lease_token,
            "00000001786800005000",
        )
        .await
        .unwrap();
    let after_completion = store
        .ensure_skill_candidate_jobs(&replay_plan, "00000001786800006000")
        .await
        .unwrap();
    assert_eq!(after_completion.inserted, 0);
    assert_eq!(after_completion.existing, 1);
    assert!(store
        .claim_skill_candidate_jobs("00000001786800007000", "00000001786800307000", 3, 10,)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn claim_takes_only_the_oldest_job_per_tenant_agent_lane() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(&[job_spec("a-1", "local", "codex")], "00000001786800001000")
        .await
        .unwrap();
    store
        .ensure_skill_candidate_jobs(
            &[
                job_spec("a-2", "local", "codex"),
                job_spec("b-1", "local", "claude"),
                job_spec("c-1", "other", "codex"),
            ],
            "00000001786800002000",
        )
        .await
        .unwrap();

    let claimed = store
        .claim_skill_candidate_jobs("00000001786800003000", "00000001786800303000", 3, 10)
        .await
        .unwrap();
    let mut ids: Vec<_> = claimed
        .iter()
        .map(|claimed| claimed.job.job_id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["a-1", "b-1", "c-1"]);

    let a1 = claimed
        .iter()
        .find(|claimed| claimed.job.job_id == "a-1")
        .unwrap();
    store
        .complete_skill_candidate_job(&a1.job.job_id, &a1.lease_token, "00000001786800004000")
        .await
        .unwrap();
    let next = store
        .claim_skill_candidate_jobs("00000001786800005000", "00000001786800305000", 3, 10)
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].job.job_id, "a-2");
}

#[tokio::test]
async fn expired_lease_is_reclaimed_with_a_fenced_token() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[job_spec("lease-1", "local", "codex")],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let first = store
        .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 3, 1)
        .await
        .unwrap()
        .remove(0);
    assert!(store
        .claim_skill_candidate_jobs("00000001786800002500", "00000001786800003500", 3, 1,)
        .await
        .unwrap()
        .is_empty());

    let second = store
        .claim_skill_candidate_jobs("00000001786800004000", "00000001786800005000", 3, 1)
        .await
        .unwrap()
        .remove(0);
    assert_ne!(first.lease_token, second.lease_token);
    assert!(matches!(
        store
            .complete_skill_candidate_job(
                &first.job.job_id,
                &first.lease_token,
                "00000001786800004500",
            )
            .await,
        Err(StorageError::Conflict(_))
    ));
    store
        .complete_skill_candidate_job(
            &second.job.job_id,
            &second.lease_token,
            "00000001786800004600",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_generation_replay_inserts_one_durable_receipt() {
    let (_dir, store) = common::test_store().await;
    let spec = job_spec("replay-1", "local", "codex");
    let left_store = store.clone();
    let right_store = store.clone();
    let left_spec = spec.clone();
    let right_spec = spec;

    let (left, right) = tokio::join!(
        async move {
            left_store
                .ensure_skill_candidate_jobs(&[left_spec], "00000001786800001000")
                .await
                .unwrap()
        },
        async move {
            right_store
                .ensure_skill_candidate_jobs(&[right_spec], "00000001786800001000")
                .await
                .unwrap()
        }
    );

    assert_eq!(left.inserted + right.inserted, 1);
    assert_eq!(left.existing + right.existing, 1);
    assert_eq!(
        store
            .claim_skill_candidate_jobs("00000001786800002000", "00000001786800302000", 3, 10,)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn concurrent_claimers_never_take_two_jobs_from_one_lane() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[
                job_spec("lane-1", "local", "codex"),
                job_spec("lane-2", "local", "codex"),
            ],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let left = store.clone();
    let right = store.clone();

    let (left_claimed, right_claimed) = tokio::join!(
        async move {
            left.claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 3, 10)
                .await
                .unwrap()
        },
        async move {
            right
                .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 3, 10)
                .await
                .unwrap()
        }
    );

    assert_eq!(left_claimed.len() + right_claimed.len(), 1);
}

#[tokio::test]
async fn completed_receipt_survives_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("durable.lance");
    let spec = job_spec("durable-1", "local", "codex");
    {
        let store = Store::open(&path).await.unwrap();
        store
            .ensure_skill_candidate_jobs(std::slice::from_ref(&spec), "00000001786800001000")
            .await
            .unwrap();
        let claimed = store
            .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 3, 1)
            .await
            .unwrap()
            .remove(0);
        store
            .complete_skill_candidate_job(
                &claimed.job.job_id,
                &claimed.lease_token,
                "00000001786800002500",
            )
            .await
            .unwrap();
    }
    let reopened = Store::open(&path).await.unwrap();
    let replay = reopened
        .ensure_skill_candidate_jobs(&[spec], "00000001786800004000")
        .await
        .unwrap();
    assert_eq!(replay.inserted, 0);
    assert_eq!(replay.existing, 1);
    assert!(reopened
        .claim_skill_candidate_jobs("00000001786800005000", "00000001786800006000", 3, 10,)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn evidence_scan_uses_only_the_latest_completed_generation_per_session() {
    let (_dir, store) = common::test_store().await;
    let old = completed_round("old", "session-a", "task-old", 10);
    let current = completed_round("current", "session-a", "task-current", 10);
    let other = completed_round("other", "session-b", "task-other", 10);
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-old", "session-a", "00000001786800001000"),
            &[old],
        )
        .await
        .unwrap();
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-current", "session-a", "00000001786800002000"),
            &[current],
        )
        .await
        .unwrap();
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-other", "session-b", "00000001786800003000"),
            &[other],
        )
        .await
        .unwrap();

    let evidence = store
        .latest_skill_candidate_evidence(100, 100)
        .await
        .unwrap();
    let mut ids: Vec<_> = evidence
        .iter()
        .map(|item| item.round.round_id.as_str())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, vec!["current", "other"]);
    assert!(evidence
        .iter()
        .all(|item| item.round.task_signal_version == 1));
}

#[tokio::test]
async fn publication_rejects_a_round_from_a_different_projector_version() {
    let (_dir, store) = common::test_store().await;
    let mut round = completed_round("wrong-projector", "session-a", "task-a", 10);
    round.projector_version = COMPLETED_TOOL_ROUND_PROJECTOR_VERSION + 1;

    assert!(matches!(
        store
            .publish_completed_tool_round_generation(
                &completed_build("generation-wrong", "session-a", "00000001786800001000"),
                &[round],
            )
            .await,
        Err(StorageError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn failure_retries_at_schedule_then_dead_letters_after_max_attempts() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[
                job_spec("retry-1", "local", "codex"),
                job_spec("retry-2", "local", "codex"),
            ],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let first = store
        .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 2, 1)
        .await
        .unwrap()
        .remove(0);
    store
        .fail_skill_candidate_job(
            &first.job.job_id,
            &first.lease_token,
            "extractor_unavailable",
            "00000001786800005000",
            "00000001786800002500",
            2,
        )
        .await
        .unwrap();
    assert!(store
        .claim_skill_candidate_jobs("00000001786800004000", "00000001786800006000", 2, 10,)
        .await
        .unwrap()
        .is_empty());

    let second_attempt = store
        .claim_skill_candidate_jobs("00000001786800005000", "00000001786800006000", 2, 10)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(second_attempt.job.job_id, "retry-1");
    assert_eq!(second_attempt.job.attempt_count, 2);
    store
        .fail_skill_candidate_job(
            &second_attempt.job.job_id,
            &second_attempt.lease_token,
            "invalid_extractor_output",
            "00000001786800007000",
            "00000001786800005500",
            2,
        )
        .await
        .unwrap();

    let next = store
        .claim_skill_candidate_jobs("00000001786800008000", "00000001786800009000", 2, 10)
        .await
        .unwrap();
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].job.job_id, "retry-2");
}

#[tokio::test]
async fn expired_final_attempt_dead_letters_and_unblocks_the_lane() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[
                job_spec("crashed-1", "local", "codex"),
                job_spec("crashed-2", "local", "codex"),
            ],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let first = store
        .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 1, 1)
        .await
        .unwrap();
    assert_eq!(first[0].job.job_id, "crashed-1");

    let after_expiry = store
        .claim_skill_candidate_jobs("00000001786800004000", "00000001786800005000", 1, 10)
        .await
        .unwrap();
    assert_eq!(after_expiry.len(), 1);
    assert_eq!(after_expiry[0].job.job_id, "crashed-2");
}

#[tokio::test]
async fn lowering_max_attempts_dead_letters_retry_wait_and_unblocks_lane() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[
                job_spec("lowered-1", "local", "codex"),
                job_spec("lowered-2", "local", "codex"),
            ],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let first = store
        .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 3, 1)
        .await
        .unwrap()
        .remove(0);
    store
        .fail_skill_candidate_job(
            &first.job.job_id,
            &first.lease_token,
            "temporary",
            "00000001786800005000",
            "00000001786800002500",
            3,
        )
        .await
        .unwrap();

    let claimed = store
        .claim_skill_candidate_jobs("00000001786800004000", "00000001786800005000", 1, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job.job_id, "lowered-2");
}

#[tokio::test]
async fn expired_lease_cannot_complete_before_reclaim() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[job_spec("late-1", "local", "codex")],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let first = store
        .claim_skill_candidate_jobs("00000001786800002000", "00000001786800003000", 2, 1)
        .await
        .unwrap()
        .remove(0);

    assert!(matches!(
        store
            .complete_skill_candidate_job(
                &first.job.job_id,
                &first.lease_token,
                "00000001786800004000",
            )
            .await,
        Err(StorageError::Conflict(_))
    ));
    let reclaimed = store
        .claim_skill_candidate_jobs("00000001786800004000", "00000001786800005000", 2, 1)
        .await
        .unwrap()
        .remove(0);
    assert_ne!(first.lease_token, reclaimed.lease_token);
}

#[tokio::test]
async fn complete_rejects_an_invalid_timestamp_without_mutating_the_lease() {
    let (_dir, store) = common::test_store().await;
    store
        .ensure_skill_candidate_jobs(
            &[job_spec("invalid-time", "local", "codex")],
            "00000001786800001000",
        )
        .await
        .unwrap();
    let claimed = store
        .claim_skill_candidate_jobs("00000001786800002000", "00000001786800004000", 2, 1)
        .await
        .unwrap()
        .remove(0);

    assert!(matches!(
        store
            .complete_skill_candidate_job(&claimed.job.job_id, &claimed.lease_token, "")
            .await,
        Err(StorageError::InvalidInput(_))
    ));
    store
        .complete_skill_candidate_job(
            &claimed.job.job_id,
            &claimed.lease_token,
            "00000001786800003000",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn reconcile_repairs_jobs_from_completed_generations_idempotently() {
    let (_dir, store) = common::test_store().await;
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-1", "session-a", "00000001786800001000"),
            &[completed_round("round-1", "session-a", "task-a", 10)],
        )
        .await
        .unwrap();
    let service = SkillCandidateService::new(store.clone());

    let first = service.reconcile().await.unwrap();
    let replay = service.reconcile().await.unwrap();

    assert_eq!(first.evidence_count, 1);
    assert_eq!(first.planned_job_count, 1);
    assert_eq!(first.inserted_job_count, 1);
    assert_eq!(replay.inserted_job_count, 0);
    assert_eq!(replay.existing_job_count, 1);
    assert_eq!(
        store
            .claim_skill_candidate_jobs("99999999999999999990", "99999999999999999999", 3, 10,)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn corrected_head_stales_an_unclaimed_candidate_receipt() {
    let (_dir, store) = common::test_store().await;
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-qualifying", "session-a", "00000001786800001000"),
            &[completed_round(
                "round-qualifying",
                "session-a",
                "task-a",
                10,
            )],
        )
        .await
        .unwrap();
    let service = SkillCandidateService::new(store.clone());
    let first = service.reconcile().await.unwrap();
    assert_eq!(first.inserted_job_count, 1);

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-corrected", "session-a", "00000001786800002000"),
            &[completed_round("round-corrected", "session-a", "task-a", 1)],
        )
        .await
        .unwrap();
    let corrected = service.reconcile().await.unwrap();

    assert_eq!(corrected.planned_job_count, 0);
    assert_eq!(corrected.staled_job_count, 1);
    assert!(store
        .claim_skill_candidate_jobs("99999999999999999990", "99999999999999999999", 3, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn corrected_head_fences_an_expired_worker_claim() {
    let (_dir, store) = common::test_store().await;
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-qualifying", "session-a", "00000001786800001000"),
            &[completed_round(
                "round-qualifying",
                "session-a",
                "task-a",
                10,
            )],
        )
        .await
        .unwrap();
    let service = SkillCandidateService::new(store.clone());
    service.reconcile().await.unwrap();
    let claim_now = current_timestamp();
    let lease_expires_at = format!("{:020}", claim_now.parse::<u64>().unwrap() + 1);
    let claimed = store
        .claim_skill_candidate_jobs(&claim_now, &lease_expires_at, 3, 1)
        .await
        .unwrap()
        .remove(0);
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-corrected", "session-a", "00000001786800003000"),
            &[completed_round("round-corrected", "session-a", "task-a", 1)],
        )
        .await
        .unwrap();
    let corrected = service.reconcile().await.unwrap();

    assert_eq!(corrected.staled_job_count, 1);
    assert!(matches!(
        store
            .complete_skill_candidate_job(
                &claimed.job.job_id,
                &claimed.lease_token,
                "00000001786800004000",
            )
            .await,
        Err(StorageError::Conflict(_))
    ));
}

#[tokio::test]
async fn obsolete_trigger_version_is_staled_even_when_its_round_still_exists() {
    let (_dir, store) = common::test_store().await;
    let round = completed_round("round-a", "session-a", "task-a", 10);
    let mut legacy = plan_skill_candidate_jobs(
        &[evidence(round.clone(), "generation-a")],
        &SkillCandidatePolicy::default(),
    )
    .remove(0);
    legacy.job_id = "legacy-trigger-job".into();
    legacy.trigger_version -= 1;
    store
        .ensure_skill_candidate_jobs(&[legacy], "00000001786800000000")
        .await
        .unwrap();
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-a", "session-a", "00000001786800001000"),
            &[round],
        )
        .await
        .unwrap();

    let report = SkillCandidateService::new(store.clone())
        .reconcile()
        .await
        .unwrap();

    assert_eq!(report.staled_job_count, 1);
    let claimed = store
        .claim_skill_candidate_jobs("99999999999999999990", "99999999999999999999", 3, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_ne!(claimed[0].job.job_id, "legacy-trigger-job");
}

#[tokio::test]
async fn obsolete_task_signal_evidence_is_staled_without_a_projector_change() {
    let (_dir, store) = common::test_store().await;
    let round = completed_round("round-a", "session-a", "task-a", 10);
    let mut legacy = plan_skill_candidate_jobs(
        &[evidence(round.clone(), "generation-a")],
        &SkillCandidatePolicy::default(),
    )
    .remove(0);
    legacy.job_id = "legacy-task-signal-job".into();
    legacy.round_refs[0].task_signal_version = 0;
    store
        .ensure_skill_candidate_jobs(&[legacy], "00000001786800000000")
        .await
        .unwrap();
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-a", "session-a", "00000001786800001000"),
            &[round],
        )
        .await
        .unwrap();

    let report = SkillCandidateService::new(store.clone())
        .reconcile()
        .await
        .unwrap();

    assert_eq!(report.staled_job_count, 1);
    let claimed = store
        .claim_skill_candidate_jobs("99999999999999999990", "99999999999999999999", 3, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_ne!(claimed[0].job.job_id, "legacy-task-signal-job");
}

#[tokio::test]
async fn corrected_head_does_not_cancel_a_live_processing_lease() {
    let (_dir, store) = common::test_store().await;
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-qualifying", "session-a", "00000001786800001000"),
            &[completed_round(
                "round-qualifying",
                "session-a",
                "task-a",
                10,
            )],
        )
        .await
        .unwrap();
    let service = SkillCandidateService::new(store.clone());
    service.reconcile().await.unwrap();
    let claimed = store
        .claim_skill_candidate_jobs("00000001786800002000", "99999999999999999999", 3, 1)
        .await
        .unwrap()
        .remove(0);

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-corrected", "session-a", "00000001786800003000"),
            &[completed_round("round-corrected", "session-a", "task-a", 1)],
        )
        .await
        .unwrap();
    let corrected = service.reconcile().await.unwrap();

    assert_eq!(corrected.staled_job_count, 0);
    store
        .complete_skill_candidate_job(
            &claimed.job.job_id,
            &claimed.lease_token,
            "00000001786800004000",
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn restored_evidence_reactivates_the_same_stale_receipt() {
    let (_dir, store) = common::test_store().await;
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-first", "session-a", "00000001786800001000"),
            &[completed_round("round-a", "session-a", "task-a", 10)],
        )
        .await
        .unwrap();
    let service = SkillCandidateService::new(store.clone());
    service.reconcile().await.unwrap();

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-corrected", "session-a", "00000001786800002000"),
            &[completed_round("round-corrected", "session-a", "task-a", 1)],
        )
        .await
        .unwrap();
    assert_eq!(service.reconcile().await.unwrap().staled_job_count, 1);

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-restored", "session-a", "00000001786800003000"),
            &[completed_round("round-a", "session-a", "task-a", 10)],
        )
        .await
        .unwrap();
    let restored = service.reconcile().await.unwrap();
    assert_eq!(restored.inserted_job_count, 0);
    assert_eq!(restored.existing_job_count, 1);
    assert_eq!(restored.staled_job_count, 0);
    assert_eq!(
        store
            .claim_skill_candidate_jobs("00000001786800004000", "00000001786800005000", 3, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn restored_older_evidence_reactivates_even_when_a_newer_candidate_is_planned() {
    let (_dir, store) = common::test_store().await;
    let round_a = completed_round("round-a", "session-a", "task-a", 10);
    let receipt_a = plan_skill_candidate_jobs(
        &[evidence(round_a.clone(), "generation-a")],
        &SkillCandidatePolicy::default(),
    )
    .remove(0)
    .job_id;
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-a", "session-a", "00000001786800001000"),
            std::slice::from_ref(&round_a),
        )
        .await
        .unwrap();
    let service = SkillCandidateService::new(store.clone());
    service.reconcile().await.unwrap();

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-a-fixed", "session-a", "00000001786800002000"),
            &[completed_round("round-a-fixed", "session-a", "task-a", 1)],
        )
        .await
        .unwrap();
    assert_eq!(service.reconcile().await.unwrap().staled_job_count, 1);

    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-b", "session-b", "00000001786800003000"),
            &[completed_round("round-b", "session-b", "task-a", 10)],
        )
        .await
        .unwrap();
    service.reconcile().await.unwrap();
    store
        .publish_completed_tool_round_generation(
            &completed_build("generation-a-restored", "session-a", "00000001786800004000"),
            &[round_a],
        )
        .await
        .unwrap();
    service.reconcile().await.unwrap();

    let claimed = store
        .claim_skill_candidate_jobs("99999999999999999990", "99999999999999999999", 3, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job.job_id, receipt_a);
}
