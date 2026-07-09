//! H4×merge-fix confluence acceptance (docs/offline-reranker-lane.md
//! follow-up; oss-memory-diff §9 H4 / §10 I5):
//!   - a PROCEDURAL SIBLING cluster (near-dup vectors, pairwise-disjoint
//!     fact anchors — the "N checkpoints of one ongoing migration" shape)
//!     must NOT merge; it becomes a `workflow_generalize` proposal whose
//!     execution mints ONE PendingConfirmation WORKFLOW-type placeholder
//!     and leaves every source Active;
//!   - an in-flight pending MERGE candidate over the same members is
//!     starved (no merge proposal matches it again) — it must never
//!     execute;
//!   - an anchorless near-dup pair keeps the old merge behavior
//!     (fail-open — regression guard for tests/evolution_merge.rs).

use std::sync::Arc;

use mem::{
    config::{EvolutionSettings, EvolutionSynthesisMode},
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::{EvolutionCandidate, EvolutionCandidateStore, Store},
    worker::evolution_worker,
};
use tempfile::tempdir;

const TENANT: &str = "local";
const DIM: usize = 8;

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

fn capsule(id: &str, content: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary of {id}"),
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        project: Some("xmbox-rs".into()),
        repo: Some("xmbox-rs".into()),
        module: None,
        task_type: None,
        tags: vec!["java-to-rust".into(), "migration".into()],
        topics: vec![],
        confidence: 0.8,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test-agent".into(),
        created_at: "00000000000000000001".into(),
        updated_at: "00000000000000000001".into(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

async fn seed(store: &Store, c: &CapabilityCapsuleRecord, x: f32, y: f32) {
    let mut v = vec![0.0_f32; DIM];
    v[0] = x;
    v[1] = y;
    store.insert_capability_capsule(c.clone()).await.unwrap();
    store
        .upsert_capability_capsule_embedding(
            &c.capability_capsule_id,
            &c.tenant,
            "fake",
            DIM as i64,
            &f32_to_blob(&v),
            &c.content_hash,
            &c.updated_at,
            "00000000000000000001",
        )
        .await
        .unwrap();
}

fn settings() -> EvolutionSettings {
    EvolutionSettings {
        enabled: true,
        interval_secs: 86_400,
        k_cycles: 1,
        evidence_decay: 0.7,
        hysteresis: 0.5,
        cluster_threshold: 0.80,
        merge_threshold: 0.88,
        generalize_min_n: 4,
        scan_limit: 1_000,
        prune_idle_cycles: 3,
        split_threshold: 0.5,
        synthesis: EvolutionSynthesisMode::Review,
    }
}

/// Four sibling checkpoints: near-identical vectors, one distinct
/// commit sha each — the exact production shape that must not merge.
async fn seed_sibling_cluster(store: &Store) -> Vec<&'static str> {
    let members = [
        (
            "m1",
            "迁移 commit 24e5cb9 踩点 —— openapi 消费端批次，quirk 若干",
        ),
        (
            "m2",
            "迁移 commit 282b74c 踩点 —— hashids 批次，admin 计划 id 兼容",
        ),
        ("m3", "迁移 commit 4465b9a 踩点 —— os 写类批次，磁盘挂载"),
        ("m4", "迁移 commit 920aace1 踩点 —— resource face 库批次"),
    ];
    for (i, (id, content)) in members.iter().enumerate() {
        // All pairwise cosines ≈ 1.0 (well above merge_threshold 0.88).
        seed(store, &capsule(id, content), 1.0, 0.001 * i as f32).await;
    }
    members.iter().map(|(id, _)| *id).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sibling_cluster_reroutes_merge_into_workflow_generalize() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("wf.lance")).await.unwrap());
    let ids = seed_sibling_cluster(&store).await;

    // An in-flight pending merge candidate over the same members (the
    // production situation at upgrade time) — it must be starved, not
    // executed.
    let mut member_ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    member_ids.sort();
    store
        .upsert_evolution_candidate(EvolutionCandidate {
            candidate_id: "evo_preexisting_merge".into(),
            tenant: TENANT.into(),
            op_kind: "merge".into(),
            member_ids: member_ids.clone(),
            params: "{}".into(),
            evidence: 1.7,
            consecutive_cycles: 2,
            status: "pending".into(),
            first_proposed_at: "00000000000000000001".into(),
            last_signal_at: "00000000000000000001".into(),
            executed_at: None,
            result_capsule_ids: Vec::new(),
        })
        .await
        .unwrap();

    let report = evolution_worker::sweep_once(&*store, &settings(), TENANT, false)
        .await
        .unwrap();

    // No merge anywhere; exactly one workflow_generalize executed.
    assert!(
        report.executed.iter().all(|e| e.op_kind != "merge"),
        "sibling cluster must never merge: {:?}",
        report.executed
    );
    let wf: Vec<_> = report
        .executed
        .iter()
        .filter(|e| e.op_kind == "workflow_generalize")
        .collect();
    assert_eq!(wf.len(), 1, "one workflow proposal: {:?}", report.executed);

    // Placeholder: PendingConfirmation + Workflow type; sources Active.
    let placeholder_id = &wf[0].result_capsule_ids[0];
    let placeholder = store
        .get_capability_capsule_for_tenant(TENANT, placeholder_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        placeholder.status,
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(
        placeholder.capability_capsule_type,
        CapabilityCapsuleType::Workflow
    );
    assert!(
        placeholder.tags.iter().any(|t| t == "evolution:workflow"),
        "tagged for review triage: {:?}",
        placeholder.tags
    );
    for id in &ids {
        let m = store
            .get_capability_capsule_for_tenant(TENANT, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            m.status,
            CapabilityCapsuleStatus::Active,
            "source {id} must stay Active"
        );
    }

    // The pre-existing merge candidate was not executed.
    let merges_executed = store
        .list_evolution_candidates(TENANT, Some("executed"))
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.op_kind == "merge")
        .count();
    assert_eq!(
        merges_executed, 0,
        "starved merge candidate must not execute"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anchorless_near_dups_still_merge_fail_open() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("fo.lance")).await.unwrap());
    seed(
        &store,
        &capsule(
            "winner",
            "a long and detailed lesson about lance write paths",
        ),
        1.0,
        0.0,
    )
    .await;
    seed(&store, &capsule("loser", "short lance lesson"), 0.99, 0.01).await;

    let report = evolution_worker::sweep_once(&*store, &settings(), TENANT, false)
        .await
        .unwrap();
    assert!(
        report.executed.iter().any(|e| e.op_kind == "merge"),
        "anchorless near-dups keep the old merge path: {:?}",
        report.executed
    );
}
