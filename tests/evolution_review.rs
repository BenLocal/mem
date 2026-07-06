//! E3 acceptance — ② generalize review-loop closure (doc
//! `docs/evolution-worker.md` §10 E3):
//!   - reject: the placeholder's lineage edges close, the executed
//!     candidate row flips to `rejected` (so the executed-history
//!     suppression stops matching), and the cluster's re-proposal has
//!     to sit through the K-cycle gate again before it can reach the
//!     review queue ("reject → 候选终态 且 K 期内不复提"),
//!   - edit_accept: the successor capsule (new id!) re-owns the
//!     `generalizes` lineage toward the sources — the "accept 时写边"
//!     acceptance, needed because edit-accept mints a new id and closes
//!     the placeholder's edges,
//!   - sources stay Active through the whole loop (generalize never
//!     touches them).

use std::sync::Arc;

use mem::{
    config::{EvolutionSettings, EvolutionSynthesisMode},
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
        EditPendingRequest, Scope, Visibility,
    },
    service::CapabilityCapsuleService,
    storage::{EvolutionCandidateStore, Store},
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

fn capsule(id: &str, content: &str, created_at: &str) -> CapabilityCapsuleRecord {
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
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec!["rust".into(), "lance".into()],
        topics: vec![],
        confidence: 0.7,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "test-agent".into(),
        created_at: created_at.into(),
        updated_at: created_at.into(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

/// Four episodic near-neighbours sharing two tag themes — the
/// generalize trigger shape. `merge_threshold` is pinned unreachable so
/// ONLY generalize fires.
async fn seed_generalize_cluster(store: &Store) {
    for (i, angle) in [0.0_f32, 0.05, 0.10, 0.15].iter().enumerate() {
        let c = capsule(
            &format!("src{i}"),
            &format!("episodic lesson {i} about lance write paths"),
            "00000000000000000001",
        );
        let mut v = vec![0.0_f32; DIM];
        v[0] = angle.cos();
        v[1] = angle.sin();
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
}

fn generalize_only_settings(k_cycles: u32) -> EvolutionSettings {
    EvolutionSettings {
        enabled: true,
        interval_secs: 86_400,
        k_cycles,
        evidence_decay: 0.7,
        hysteresis: 0.5,
        cluster_threshold: 0.80,
        merge_threshold: 1.1, // unreachable — generalize-only
        generalize_min_n: 4,
        scan_limit: 1_000,
        prune_idle_cycles: 3,
        split_threshold: 0.5,
        synthesis: EvolutionSynthesisMode::Review,
    }
}

async fn status_of(store: &Store, id: &str) -> CapabilityCapsuleStatus {
    store
        .get_capability_capsule_for_tenant(TENANT, id)
        .await
        .unwrap()
        .expect("capsule row must exist")
        .status
}

async fn active_relations(store: &Store, capsule_id: &str) -> Vec<String> {
    store
        .neighbors(&format!("capability_capsule:{capsule_id}"))
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.valid_to.is_none())
        .map(|e| e.relation)
        .collect()
}

/// E3 acceptance (reject leg): reject → placeholder edges closed +
/// candidate row flips `executed` → `rejected` + the re-proposal has to
/// re-earn K consecutive cycles before a new placeholder reaches the
/// review queue.
#[tokio::test(flavor = "multi_thread")]
async fn reject_closes_edges_flips_candidate_and_regates_reproposal() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed_generalize_cluster(&store).await;
    let settings = generalize_only_settings(2);

    // Cycle 1 holds (K=2), cycle 2 executes → placeholder in review.
    let r1 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert!(r1.executed.is_empty(), "K gate must hold on cycle 1");
    let r2 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(r2.executed.len(), 1, "cycle 2 must execute the generalize");
    let candidate_id = r2.executed[0].candidate_id.clone();
    let placeholder = r2.executed[0].result_capsule_ids[0].clone();
    assert_eq!(active_relations(&store, &placeholder).await.len(), 4);

    // Reviewer says no.
    service.reject_pending(TENANT, &placeholder).await.unwrap();
    assert_eq!(
        status_of(&store, &placeholder).await,
        CapabilityCapsuleStatus::Rejected
    );
    assert!(
        active_relations(&store, &placeholder).await.is_empty(),
        "rejected placeholder must not keep active lineage edges"
    );
    let rejected = store
        .list_evolution_candidates(TENANT, Some("rejected"))
        .await
        .unwrap();
    assert_eq!(rejected.len(), 1, "candidate row must flip to rejected");
    assert_eq!(rejected[0].candidate_id, candidate_id);
    assert!(store
        .list_evolution_candidates(TENANT, Some("executed"))
        .await
        .unwrap()
        .is_empty());

    // Cycle 3: the cluster may re-propose, but as a NEW candidate the K
    // gate holds it — nothing reaches the review queue yet.
    let r3 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert!(
        r3.executed.is_empty(),
        "re-proposal must re-earn the K gate — no execution on cycle 3"
    );
    assert!(
        store.list_pending_review(TENANT).await.unwrap().is_empty(),
        "no new placeholder may reach review inside the K window"
    );

    // Cycle 4: gate re-opens — the proposal is allowed back into review.
    let r4 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(r4.executed.len(), 1, "gate re-opens after K cycles");
    assert_eq!(store.list_pending_review(TENANT).await.unwrap().len(), 1);

    // Sources were never touched across the whole loop.
    for i in 0..4 {
        assert_eq!(
            status_of(&store, &format!("src{i}")).await,
            CapabilityCapsuleStatus::Active
        );
    }
}

/// E3 acceptance (accept leg): `review_edit_accept` writes the real
/// generalization content → the ACTIVE successor re-owns the
/// `generalizes` lineage toward every source ("accept 时写边" — the
/// successor is a new id, so the proposal-time edges on the placeholder
/// can't serve it), sources stay Active and untouched.
#[tokio::test(flavor = "multi_thread")]
async fn edit_accept_rewrites_generalize_lineage_to_successor() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed_generalize_cluster(&store).await;

    let report = evolution_worker::sweep_once(&*store, &generalize_only_settings(1), TENANT, false)
        .await
        .unwrap();
    let placeholder = report.executed[0].result_capsule_ids[0].clone();

    let response = service
        .edit_and_accept_pending(
            TENANT,
            EditPendingRequest {
                capability_capsule_id: placeholder.clone(),
                summary: "lance write-path lessons generalized".into(),
                content: "General principle distilled by the reviewer from four episodic lessons about lance write paths.".into(),
                evidence: vec![],
                code_refs: vec![],
                tags: vec!["rust".into(), "lance".into()],
            },
        )
        .await
        .unwrap();
    let successor = response.capability_capsule;
    assert_eq!(successor.status, CapabilityCapsuleStatus::Active);
    assert_ne!(successor.capability_capsule_id, placeholder);

    let successor_rels = active_relations(&store, &successor.capability_capsule_id).await;
    assert_eq!(
        successor_rels
            .iter()
            .filter(|r| r.as_str() == "generalizes")
            .count(),
        4,
        "successor must re-own one generalizes edge per source: got {successor_rels:?}"
    );
    assert!(
        !active_relations(&store, &placeholder)
            .await
            .contains(&"generalizes".to_string()),
        "placeholder's proposal-time edges must be closed by the accept"
    );
    for i in 0..4 {
        assert_eq!(
            status_of(&store, &format!("src{i}")).await,
            CapabilityCapsuleStatus::Active,
            "sources stay Active — generalize never supersedes them"
        );
    }
}

/// Audit 2026-07-03 #1: while an executed one-shot candidate awaits its
/// review verdict, further sweeps keep detecting the same stable
/// cluster — the executed history must suppress re-execution, or the
/// review queue fills with duplicate placeholders.
#[tokio::test(flavor = "multi_thread")]
async fn executed_generalize_is_not_reproposed_while_awaiting_review() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_generalize_cluster(&store).await;
    let settings = generalize_only_settings(1); // gate opens immediately

    let r1 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(r1.executed.len(), 1, "K=1 executes on the first sweep");

    for cycle in 2..=3 {
        let r = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
            .await
            .unwrap();
        assert!(
            r.executed.is_empty(),
            "cycle {cycle} re-executed an already-executed generalize: {:?}",
            r.executed
        );
    }
    assert_eq!(
        store.list_pending_review(TENANT).await.unwrap().len(),
        1,
        "exactly one placeholder may exist while the reviewer decides"
    );
}

/// Audit 2026-07-03 ⑦: accepting a placeholder settles its candidate —
/// the row flips `executed` → `accepted`, §11 rollback refuses it
/// (instead of "succeeding" while the accepted successor stays live),
/// and the settled cluster does not re-propose.
#[tokio::test(flavor = "multi_thread")]
async fn accept_settles_candidate_blocks_rollback_and_reproposal() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed_generalize_cluster(&store).await;
    let settings = generalize_only_settings(1);

    let r1 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    let candidate_id = r1.executed[0].candidate_id.clone();
    let placeholder = r1.executed[0].result_capsule_ids[0].clone();

    service
        .edit_and_accept_pending(
            TENANT,
            EditPendingRequest {
                capability_capsule_id: placeholder.clone(),
                summary: "generalized".into(),
                content: "Reviewer-written generalization of the four lessons.".into(),
                evidence: vec![],
                code_refs: vec![],
                tags: vec!["rust".into()],
            },
        )
        .await
        .unwrap();

    let accepted = store
        .list_evolution_candidates(TENANT, Some("accepted"))
        .await
        .unwrap();
    assert_eq!(
        accepted.len(),
        1,
        "accept must settle the candidate row to 'accepted'"
    );
    assert_eq!(accepted[0].candidate_id, candidate_id);

    let rollback = evolution_worker::rollback_candidate(&*store, TENANT, &candidate_id).await;
    assert!(
        rollback.is_err(),
        "rollback after accept must refuse — the accepted successor owns the lineage"
    );

    let r2 = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert!(
        r2.executed.is_empty(),
        "a settled (accepted) cluster must not re-propose: {:?}",
        r2.executed
    );
    assert!(
        store.list_pending_review(TENANT).await.unwrap().is_empty(),
        "no new placeholder after accept"
    );
}

/// Audit 2026-07-03 ⑦ (plain-accept leg): `review_accept` without an
/// edit settles the candidate the same way `review_edit_accept` does.
#[tokio::test(flavor = "multi_thread")]
async fn plain_accept_also_settles_the_candidate() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed_generalize_cluster(&store).await;

    let r1 = evolution_worker::sweep_once(&*store, &generalize_only_settings(1), TENANT, false)
        .await
        .unwrap();
    let placeholder = r1.executed[0].result_capsule_ids[0].clone();

    service.accept_pending(TENANT, &placeholder).await.unwrap();

    assert_eq!(
        store
            .list_evolution_candidates(TENANT, Some("accepted"))
            .await
            .unwrap()
            .len(),
        1,
        "plain accept must settle the candidate row"
    );
    assert!(store
        .list_evolution_candidates(TENANT, Some("executed"))
        .await
        .unwrap()
        .is_empty());
}
