//! E2 acceptance — evolution ① merge live (doc `docs/evolution-worker.md`
//! §10 E2):
//!   - post-merge retrieval returns ONLY the canonical (loser leaves the
//!     `search_candidates` pool but the row survives — verbatim-safe
//!     Archived, never deleted; assertion shape reuses
//!     `tests/version_chain_dedup.rs`),
//!   - dry-run preview set == live execution set (same seed, same
//!     settings — the preview must be a faithful rehearsal),
//!   - rollback restores the pre-merge retrieval semantics (losers back
//!     to Active, `merged_into` lineage edges closed, candidate row kept
//!     as `rolled_back` — §11 "回滚后的世界与执行前在检索语义上等价").

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use mem::{
    config::{EvolutionSettings, EvolutionSynthesisMode},
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
    storage::{EvolutionCandidateStore, Store},
    worker::evolution_worker,
};
use tempfile::tempdir;
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";
const DIM: usize = 8;

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

fn capsule(id: &str, content: &str, tags: &[&str], created_at: &str) -> CapabilityCapsuleRecord {
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
        tags: tags.iter().map(|s| s.to_string()).collect(),
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

async fn seed(store: &Store, c: &CapabilityCapsuleRecord, vector2d: (f32, f32)) {
    let mut v = vec![0.0_f32; DIM];
    v[0] = vector2d.0;
    v[1] = vector2d.1;
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

/// k_cycles=1 so the first sweep executes — these tests exercise the
/// execute/rollback behaviour, not the anti-jitter gate (that's
/// `tests/evolution.rs::merge_gate_holds_two_cycles_then_executes_on_third`).
fn settings_execute_first_sweep() -> EvolutionSettings {
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
        synthesis: EvolutionSynthesisMode::Review,
    }
}

/// Two near-duplicates (cosine ≈ 0.9999 ≥ merge 0.88); `winner` is
/// longer so keep-longest selects it as canonical.
async fn seed_merge_pair(store: &Store) {
    seed(
        store,
        &capsule(
            "winner",
            "a long and detailed lesson about lance write paths and refresh",
            &["rust", "lance"],
            "00000000000000000001",
        ),
        (1.0, 0.0),
    )
    .await;
    seed(
        store,
        &capsule(
            "loser",
            "short lance lesson",
            &["rust", "lance"],
            "00000000000000000002",
        ),
        (0.99, 0.01),
    )
    .await;
}

async fn pool_ids(store: &Store) -> Vec<String> {
    store
        .search_candidates(TENANT)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.capability_capsule_id)
        .collect()
}

/// E2 acceptance (a): after ① merge executes, retrieval sees ONLY the
/// canonical — the loser drops out of the `search_candidates` pool
/// (the candidate source for every ranked read) while its row survives
/// as `Archived` (verbatim rule: evolution never physically deletes).
#[tokio::test(flavor = "multi_thread")]
async fn merged_loser_leaves_search_pool_canonical_stays() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_merge_pair(&store).await;

    let before = pool_ids(&store).await;
    assert!(before.contains(&"winner".to_string()) && before.contains(&"loser".to_string()));

    let report =
        evolution_worker::sweep_once(&*store, &settings_execute_first_sweep(), TENANT, false)
            .await
            .unwrap();
    assert_eq!(report.executed.len(), 1, "merge must execute on sweep 1");
    assert_eq!(
        report.executed[0].result_capsule_ids,
        vec!["winner".to_string()]
    );

    let after = pool_ids(&store).await;
    assert!(
        after.contains(&"winner".to_string()),
        "canonical must stay retrievable: got {after:?}"
    );
    assert!(
        !after.contains(&"loser".to_string()),
        "archived loser must leave the search pool: got {after:?}"
    );

    // Verbatim-safe: the loser row still exists, merely Archived.
    let loser = store
        .get_capability_capsule_for_tenant(TENANT, "loser")
        .await
        .unwrap()
        .expect("loser row must survive the merge");
    assert_eq!(loser.status, CapabilityCapsuleStatus::Archived);
}

/// E2 acceptance (b): the dry-run preview is a faithful rehearsal — on
/// identical state and settings, the set of (op_kind, member_ids) it
/// previews equals the set the subsequent real sweep executes.
#[tokio::test(flavor = "multi_thread")]
async fn dry_run_preview_set_matches_live_execution_set() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_merge_pair(&store).await;
    let settings = settings_execute_first_sweep();

    let preview = evolution_worker::sweep_once(&*store, &settings, TENANT, true)
        .await
        .unwrap();
    assert!(preview.executed.is_empty(), "dry-run must execute nothing");
    let mut previewed: Vec<(String, Vec<String>)> = preview
        .proposals
        .iter()
        .map(|p| (p.op_kind.clone(), p.member_ids.clone()))
        .collect();
    previewed.sort();

    let live = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    let mut executed: Vec<(String, Vec<String>)> = live
        .executed
        .iter()
        .map(|e| (e.op_kind.clone(), e.member_ids.clone()))
        .collect();
    executed.sort();

    assert!(!executed.is_empty(), "live sweep must execute");
    assert_eq!(
        previewed, executed,
        "dry-run preview and live execution must agree on the operation set"
    );
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

async fn status_of(store: &Store, id: &str) -> CapabilityCapsuleStatus {
    store
        .get_capability_capsule_for_tenant(TENANT, id)
        .await
        .unwrap()
        .expect("capsule row must exist — evolution never physically deletes")
        .status
}

/// E2 acceptance (c), §11: rolling back an executed ① merge restores the
/// pre-merge retrieval semantics — losers back to Active (and back in the
/// search pool), `merged_into` lineage edges closed (not deleted), the
/// candidate row kept as an auditable `rolled_back` tombstone.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_merge_restores_losers_and_closes_lineage_edges() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_merge_pair(&store).await;

    let report =
        evolution_worker::sweep_once(&*store, &settings_execute_first_sweep(), TENANT, false)
            .await
            .unwrap();
    let candidate_id = report.executed[0].candidate_id.clone();
    assert_eq!(
        status_of(&store, "loser").await,
        CapabilityCapsuleStatus::Archived
    );
    assert!(
        active_relations(&store, "loser")
            .await
            .contains(&"merged_into".to_string()),
        "merge must have written an active merged_into lineage edge"
    );

    let rb = evolution_worker::rollback_candidate(&*store, TENANT, &candidate_id)
        .await
        .unwrap();
    assert_eq!(rb.op_kind, "merge");
    assert_eq!(rb.restored, vec!["loser".to_string()]);

    // Retrieval semantics equal to the pre-merge world (§11).
    assert_eq!(
        status_of(&store, "loser").await,
        CapabilityCapsuleStatus::Active
    );
    assert_eq!(
        status_of(&store, "winner").await,
        CapabilityCapsuleStatus::Active
    );
    let pool = pool_ids(&store).await;
    assert!(
        pool.contains(&"winner".to_string()) && pool.contains(&"loser".to_string()),
        "both members must be retrievable again: got {pool:?}"
    );
    assert!(
        !active_relations(&store, "loser")
            .await
            .contains(&"merged_into".to_string()),
        "merged_into lineage edge must be closed (valid_to stamped), not left active"
    );

    // Audit trail: candidate row survives as rolled_back.
    let rolled = store
        .list_evolution_candidates(TENANT, Some("rolled_back"))
        .await
        .unwrap();
    assert_eq!(rolled.len(), 1);
    assert_eq!(rolled[0].candidate_id, candidate_id);
}

/// §11 ②: rolling back an executed generalize archives the proposal
/// capsule and closes its `generalizes` edges; the sources were never
/// touched so they simply stay Active.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_generalize_archives_proposal_and_closes_edges() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    // Four episodic members sharing two themes; merge_threshold pinned
    // above any pair cosine so ONLY generalize fires on this cluster.
    for (i, angle) in [0.0_f32, 0.05, 0.10, 0.15].iter().enumerate() {
        seed(
            &store,
            &capsule(
                &format!("src{i}"),
                &format!("episodic lesson {i} about lance write paths"),
                &["rust", "lance"],
                "00000000000000000001",
            ),
            (angle.cos(), angle.sin()),
        )
        .await;
    }
    let mut settings = settings_execute_first_sweep();
    settings.merge_threshold = 1.1; // unreachable — generalize-only sweep

    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(report.executed.len(), 1, "generalize must execute");
    assert_eq!(report.executed[0].op_kind, "generalize");
    let candidate_id = report.executed[0].candidate_id.clone();
    let proposal_id = report.executed[0].result_capsule_ids[0].clone();
    assert_eq!(
        status_of(&store, &proposal_id).await,
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert_eq!(active_relations(&store, &proposal_id).await.len(), 4);

    let rb = evolution_worker::rollback_candidate(&*store, TENANT, &candidate_id)
        .await
        .unwrap();
    assert_eq!(rb.op_kind, "generalize");
    assert_eq!(rb.restored, vec![proposal_id.clone()]);

    assert_eq!(
        status_of(&store, &proposal_id).await,
        CapabilityCapsuleStatus::Archived
    );
    assert!(
        active_relations(&store, &proposal_id).await.is_empty(),
        "all generalizes lineage edges must be closed"
    );
    for i in 0..4 {
        assert_eq!(
            status_of(&store, &format!("src{i}")).await,
            CapabilityCapsuleStatus::Active,
            "sources were never touched by generalize — rollback must not touch them either"
        );
    }
    let rolled = store
        .list_evolution_candidates(TENANT, Some("rolled_back"))
        .await
        .unwrap();
    assert_eq!(rolled.len(), 1);
}

/// Unknown / never-executed candidate ids must fail loudly, not
/// silently no-op.
#[tokio::test(flavor = "multi_thread")]
async fn rollback_unknown_candidate_errors() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let err = evolution_worker::rollback_candidate(&*store, TENANT, "evo_nope").await;
    assert!(err.is_err(), "unknown candidate must be an error");
}

/// The operator surface: `POST /reviews/evolution/rollback` — §11's
/// "回滚单元 = 一条 executed 候选" over HTTP (the running `mem serve` owns
/// the Lance dataset, so rollback must go through the service, not a
/// second writer process).
#[tokio::test(flavor = "multi_thread")]
async fn http_reviews_evolution_rollback_round_trip() {
    let (_dir, store) = common::test_store().await;
    seed_merge_pair(&store).await;
    let report =
        evolution_worker::sweep_once(&*store, &settings_execute_first_sweep(), TENANT, false)
            .await
            .unwrap();
    let candidate_id = report.executed[0].candidate_id.clone();

    let state = common::test_app_state(
        store.clone(),
        mem::service::CapabilityCapsuleService::new(store.clone()),
    );
    let router = mem::http::router().with_state(state);
    let request = Request::builder()
        .method("POST")
        .uri("/reviews/evolution/rollback")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"tenant": TENANT, "candidate_id": candidate_id}).to_string(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["candidate_id"], candidate_id.as_str());
    assert_eq!(json["op_kind"], "merge");
    assert_eq!(json["restored"][0], "loser");

    assert_eq!(
        status_of(&store, "loser").await,
        CapabilityCapsuleStatus::Active
    );
}
