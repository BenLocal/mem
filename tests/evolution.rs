//! Integration tests for the capsule self-evolution line (E1 MVP) —
//! doc `docs/evolution-worker.md` §10 E1 acceptance:
//!   - `evolution_candidates` storage round-trip + restart persistence
//!     (evidence must survive a process restart),
//!   - map-layer clustering + cross-cycle alignment,
//!   - K-cycle anti-jitter gate (2 cycles do NOT trigger, 3 do),
//!   - operator ① merge (keep-longest canonical + Archived losers +
//!     `merged_into` lineage edges; NEVER physical delete),
//!   - operator ② generalize (PendingConfirmation proposal capsule,
//!     sources stay Active, `generalizes` lineage edges),
//!   - dry-run previews write nothing.

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
    storage::{EvolutionCandidate, EvolutionCandidateStore, Store},
    worker::evolution_worker,
};
use tempfile::tempdir;
use tower::util::ServiceExt;

mod common;

const TENANT: &str = "local";
const DIM: usize = 8;

fn candidate(id: &str, op_kind: &str, members: &[&str], evidence: f32) -> EvolutionCandidate {
    EvolutionCandidate {
        candidate_id: id.to_string(),
        tenant: TENANT.to_string(),
        op_kind: op_kind.to_string(),
        member_ids: members.iter().map(|s| s.to_string()).collect(),
        params: "{}".to_string(),
        evidence,
        consecutive_cycles: 1,
        status: "pending".to_string(),
        first_proposed_at: "00000000000000000001".to_string(),
        last_signal_at: "00000000000000000001".to_string(),
        executed_at: None,
        result_capsule_ids: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn candidate_roundtrip_upsert_and_status_filter() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());

    // Empty list on a fresh store.
    let got = store
        .list_evolution_candidates(TENANT, Some("pending"))
        .await
        .unwrap();
    assert!(got.is_empty());

    // Insert two candidates, one of each op kind.
    store
        .upsert_evolution_candidate(candidate("cand-1", "merge", &["a", "b"], 1.0))
        .await
        .unwrap();
    store
        .upsert_evolution_candidate(candidate("cand-2", "generalize", &["c", "d", "e"], 1.0))
        .await
        .unwrap();

    let got = store
        .list_evolution_candidates(TENANT, Some("pending"))
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    let merge = got.iter().find(|c| c.candidate_id == "cand-1").unwrap();
    assert_eq!(merge.op_kind, "merge");
    assert_eq!(merge.member_ids, vec!["a".to_string(), "b".to_string()]);
    assert!((merge.evidence - 1.0).abs() < 1e-6);
    assert_eq!(merge.consecutive_cycles, 1);

    // Upsert same id = update in place, not a duplicate row.
    let mut updated = candidate("cand-1", "merge", &["a", "b"], 1.7);
    updated.consecutive_cycles = 2;
    store.upsert_evolution_candidate(updated).await.unwrap();
    let got = store
        .list_evolution_candidates(TENANT, Some("pending"))
        .await
        .unwrap();
    assert_eq!(got.len(), 2, "upsert must not duplicate");
    let merge = got.iter().find(|c| c.candidate_id == "cand-1").unwrap();
    assert!((merge.evidence - 1.7).abs() < 1e-6);
    assert_eq!(merge.consecutive_cycles, 2);

    // Status flip via upsert; status filter respects it.
    let mut cancelled = candidate("cand-2", "generalize", &["c", "d", "e"], 0.3);
    cancelled.status = "cancelled".to_string();
    store.upsert_evolution_candidate(cancelled).await.unwrap();
    let pending = store
        .list_evolution_candidates(TENANT, Some("pending"))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    let all = store.list_evolution_candidates(TENANT, None).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn candidate_evidence_survives_restart() {
    // E1 acceptance: "重启证据不丢" — the anti-jitter gate state must
    // be durable, otherwise a restart resets every candidate's clock.
    let dir = tempdir().unwrap();
    let path = dir.path().join("evo.lance");
    {
        let store = Store::open(&path).await.unwrap();
        let mut c = candidate("cand-persist", "merge", &["x", "y"], 2.4);
        c.consecutive_cycles = 2;
        store.upsert_evolution_candidate(c).await.unwrap();
        // store dropped here = "process exit"
    }
    let reopened = Store::open(&path).await.unwrap();
    let got = reopened
        .list_evolution_candidates(TENANT, Some("pending"))
        .await
        .unwrap();
    assert_eq!(got.len(), 1, "candidate must survive restart");
    assert!((got[0].evidence - 2.4).abs() < 1e-6);
    assert_eq!(got[0].consecutive_cycles, 2);
}

// ───────────────────────── worker sweep ─────────────────────────

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn capsule_full(
    id: &str,
    content: &str,
    capsule_type: CapabilityCapsuleType,
    project: Option<&str>,
    topics: &[&str],
    tags: &[&str],
    confidence: f32,
    created_at: &str,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: capsule_type,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary of {id}"),
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        project: project.map(str::to_string),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        topics: topics.iter().map(|s| s.to_string()).collect(),
        confidence,
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

fn evo_settings() -> EvolutionSettings {
    EvolutionSettings {
        enabled: true,
        interval_secs: 86_400,
        k_cycles: 3,
        evidence_decay: 0.7,
        hysteresis: 0.5,
        cluster_threshold: 0.80,
        merge_threshold: 0.88,
        generalize_min_n: 4,
        scan_limit: 1_000,
        prune_idle_cycles: 3,
        synthesis: EvolutionSynthesisMode::Review,
    }
}

async fn status_of(store: &Store, id: &str) -> CapabilityCapsuleStatus {
    store
        .get_capability_capsule_for_tenant(TENANT, id)
        .await
        .unwrap()
        .expect("capsule present — evolution must never physically delete")
        .status
}

/// Two near-duplicate experiences (cosine ≈ 0.9999 ≥ merge 0.88).
/// `winner` has the longer content so keep-longest selects it.
async fn seed_merge_pair(store: &Store) {
    seed(
        store,
        &capsule_full(
            "winner",
            "a long and detailed lesson about lance write paths and refresh",
            CapabilityCapsuleType::Experience,
            Some("mem"),
            &["rust", "lance"],
            &[],
            0.7,
            "00000000000000000001",
        ),
        (1.0, 0.0),
    )
    .await;
    seed(
        store,
        &capsule_full(
            "loser",
            "short lance lesson",
            CapabilityCapsuleType::Experience,
            Some("mem"),
            &["rust", "lance"],
            &[],
            0.7,
            "00000000000000000002",
        ),
        (0.99, 0.01),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_gate_holds_two_cycles_then_executes_on_third() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_merge_pair(&store).await;
    let settings = evo_settings();

    // Cycle 1 + 2: proposal detected, candidate accumulates, nothing executes.
    for cycle in 1..=2 {
        let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
            .await
            .unwrap();
        assert_eq!(
            report.proposals.len(),
            1,
            "cycle {cycle}: one merge proposal"
        );
        assert_eq!(report.proposals[0].op_kind, "merge");
        assert!(
            report.executed.is_empty(),
            "cycle {cycle}: K gate must hold"
        );
        assert_eq!(
            status_of(&store, "winner").await,
            CapabilityCapsuleStatus::Active
        );
        assert_eq!(
            status_of(&store, "loser").await,
            CapabilityCapsuleStatus::Active
        );
    }
    let pending = store
        .list_evolution_candidates(TENANT, Some("pending"))
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].consecutive_cycles, 2);

    // Cycle 3: gate opens — keep-longest merge executes.
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(report.executed.len(), 1, "cycle 3 must execute the merge");
    assert_eq!(report.executed[0].op_kind, "merge");
    assert_eq!(
        report.executed[0].result_capsule_ids,
        vec!["winner".to_string()],
        "keep-longest must pick the longer content as canonical"
    );

    // Verbatim-safe: loser is Archived (row kept), never deleted.
    assert_eq!(
        status_of(&store, "winner").await,
        CapabilityCapsuleStatus::Active
    );
    assert_eq!(
        status_of(&store, "loser").await,
        CapabilityCapsuleStatus::Archived
    );
    // Archived rows leave the Active pool (the map population and the
    // search-candidate set both filter on status).
    let all = store
        .list_capability_capsules_for_tenant(TENANT)
        .await
        .unwrap();
    let active_ids: Vec<&str> = all
        .iter()
        .filter(|c| c.status == CapabilityCapsuleStatus::Active)
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(
        active_ids,
        vec!["winner"],
        "only the canonical stays active"
    );
    // Verbatim-safe: the loser ROW still exists (content preserved).
    assert!(all.iter().any(|c| c.capability_capsule_id == "loser"));

    // Lineage: merged_into edge loser → winner, tagged extractor=evolution.
    let edges = store.neighbors("capability_capsule:loser").await.unwrap();
    let merged: Vec<_> = edges
        .iter()
        .filter(|e| e.relation == "merged_into")
        .collect();
    assert_eq!(merged.len(), 1, "exactly one merged_into lineage edge");
    assert_eq!(merged[0].to_node_id, "capability_capsule:winner");
    assert_eq!(merged[0].extractor.as_deref(), Some("evolution"));

    // Candidate is executed with the canonical recorded for rollback.
    let executed = store
        .list_evolution_candidates(TENANT, Some("executed"))
        .await
        .unwrap();
    assert_eq!(executed.len(), 1);
    assert_eq!(executed[0].result_capsule_ids, vec!["winner".to_string()]);
    assert!(executed[0].executed_at.is_some());

    // Cycle 4: the executed merge must not re-propose (loser archived
    // out of the map). NOTE: since E4 the surviving canonical — now a
    // zero-recall map singleton — legitimately draws a ⑤
    // `reweight_decay` proposal, so the assertion is op-scoped rather
    // than "no proposals at all".
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert!(
        report.proposals.iter().all(|p| p.op_kind != "merge"),
        "no merge re-proposal after execution: got {:?}",
        report.proposals
    );
    assert!(report.executed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn generalize_proposes_pending_capsule_sources_stay_active() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    // Four episodic capsules chained at cosine 0.866 (≥ 0.80 cluster,
    // < 0.88 merge) sharing two topics, confidence 0.7 ≥ 0.6 floor.
    let angles = [0.0_f32, 30.0, 60.0, 90.0];
    for (i, deg) in angles.iter().enumerate() {
        let rad = deg.to_radians();
        seed(
            &store,
            &capsule_full(
                &format!("epi-{i}"),
                &format!("episodic lesson number {i} about lance ann indexes"),
                CapabilityCapsuleType::Experience,
                Some("mem"),
                &["rust", "lance"],
                &[],
                0.7,
                &format!("0000000000000000000{}", i + 1),
            ),
            (rad.cos(), rad.sin()),
        )
        .await;
    }
    let settings = evo_settings();

    for cycle in 1..=2 {
        let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
            .await
            .unwrap();
        assert_eq!(
            report.proposals.len(),
            1,
            "cycle {cycle}: one generalize proposal"
        );
        assert_eq!(report.proposals[0].op_kind, "generalize");
        assert!(
            report.executed.is_empty(),
            "cycle {cycle}: K gate must hold"
        );
        assert!(
            store.list_pending_review(TENANT).await.unwrap().is_empty(),
            "cycle {cycle}: no proposal capsule before the gate opens"
        );
    }

    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(
        report.executed.len(),
        1,
        "cycle 3 executes the generalize proposal"
    );

    // One PendingConfirmation proposal capsule carrying structured raw
    // material (no generated prose — review backend).
    let pending = store.list_pending_review(TENANT).await.unwrap();
    assert_eq!(pending.len(), 1);
    let proposal = &pending[0];
    assert_eq!(
        proposal.status,
        CapabilityCapsuleStatus::PendingConfirmation
    );
    for i in 0..4 {
        assert!(
            proposal.content.contains(&format!("epi-{i}")),
            "raw material must list source epi-{i}"
        );
    }
    assert!(proposal.summary.contains("[evolution:generalize]"));

    // ★ Sources stay Active — generalization complements, never replaces.
    for i in 0..4 {
        assert_eq!(
            status_of(&store, &format!("epi-{i}")).await,
            CapabilityCapsuleStatus::Active,
        );
    }

    // Lineage: generalizes edges proposal → each source.
    let edges = store
        .neighbors(&format!(
            "capability_capsule:{}",
            proposal.capability_capsule_id
        ))
        .await
        .unwrap();
    let gen_edges: Vec<_> = edges
        .iter()
        .filter(|e| e.relation == "generalizes")
        .collect();
    assert_eq!(gen_edges.len(), 4, "one generalizes edge per source");
    for e in &gen_edges {
        assert_eq!(e.extractor.as_deref(), Some("evolution"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_previews_and_writes_nothing() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_merge_pair(&store).await;
    let settings = evo_settings();

    // Three dry-run sweeps — more than K — must still write nothing:
    // no candidates, no status changes, no edges, no executions.
    for _ in 0..3 {
        let report = evolution_worker::sweep_once(&*store, &settings, TENANT, true)
            .await
            .unwrap();
        assert!(report.dry_run);
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].op_kind, "merge");
        assert!(report.executed.is_empty(), "dry-run must never execute");
    }
    assert!(
        store
            .list_evolution_candidates(TENANT, None)
            .await
            .unwrap()
            .is_empty(),
        "dry-run must not persist candidates"
    );
    assert_eq!(
        status_of(&store, "winner").await,
        CapabilityCapsuleStatus::Active
    );
    assert_eq!(
        status_of(&store, "loser").await,
        CapabilityCapsuleStatus::Active
    );
    assert!(store
        .neighbors("capability_capsule:loser")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_excludes_guidance_and_cross_project_pairs() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    // Near-dup Preference pair — guidance types are protected.
    seed(
        &store,
        &capsule_full(
            "pref-a",
            "always run cargo fmt before committing changes",
            CapabilityCapsuleType::Preference,
            Some("mem"),
            &[],
            &[],
            0.7,
            "00000000000000000001",
        ),
        (1.0, 0.0),
    )
    .await;
    seed(
        &store,
        &capsule_full(
            "pref-b",
            "run cargo fmt before commit",
            CapabilityCapsuleType::Preference,
            Some("mem"),
            &[],
            &[],
            0.7,
            "00000000000000000002",
        ),
        (0.99, 0.01),
    )
    .await;
    // Near-dup Experience pair split across projects — must not merge.
    seed(
        &store,
        &capsule_full(
            "proj-a",
            "a lesson that lives in project alpha about caching",
            CapabilityCapsuleType::Experience,
            Some("alpha"),
            &[],
            &[],
            0.7,
            "00000000000000000003",
        ),
        (0.0, 1.0),
    )
    .await;
    seed(
        &store,
        &capsule_full(
            "proj-b",
            "a lesson in project beta about caching too",
            CapabilityCapsuleType::Experience,
            Some("beta"),
            &[],
            &[],
            0.7,
            "00000000000000000004",
        ),
        (0.01, 0.99),
    )
    .await;

    let settings = evo_settings();
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, true)
        .await
        .unwrap();
    assert!(
        report.proposals.is_empty(),
        "guidance types and cross-project pairs must never be merge-proposed, got {:?}",
        report.proposals,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_non_dry_sweep_is_noop() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed_merge_pair(&store).await;
    let mut settings = evo_settings();
    settings.enabled = false;

    // Real sweeps are a no-op while disabled (idle-archive precedent)…
    for _ in 0..3 {
        let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
            .await
            .unwrap();
        assert!(report.proposals.is_empty());
        assert!(report.executed.is_empty());
    }
    assert!(store
        .list_evolution_candidates(TENANT, None)
        .await
        .unwrap()
        .is_empty());
    // …but the dry-run preview works regardless of the switch.
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, true)
        .await
        .unwrap();
    assert_eq!(report.proposals.len(), 1);
}

// ───────────────────────── HTTP endpoint ─────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn http_reviews_evolution_dry_run_previews() {
    let (_dir, store) = common::test_store().await;
    seed_merge_pair(&store).await;
    let state = common::test_app_state(
        store.clone(),
        mem::service::CapabilityCapsuleService::new(store.clone()),
    );
    let router = mem::http::router().with_state(state);

    let request = Request::builder()
        .method("POST")
        .uri("/reviews/evolution")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"tenant": TENANT, "dry_run": true}).to_string(),
        ))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["dry_run"], true);
    let proposals = json["proposals"].as_array().unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0]["op_kind"], "merge");
    // Member previews carry id + summary so an operator can eyeball
    // the cluster from the HTTP response alone.
    let members = proposals[0]["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert!(members[0]["capability_capsule_id"].is_string());
    assert!(members[0]["summary"].is_string());

    // Default OFF + dry-run: nothing persisted.
    assert!(store
        .list_evolution_candidates(TENANT, None)
        .await
        .unwrap()
        .is_empty());
}

// ───────────────── E1.5: topics ∪ tags shared signal ─────────────────

/// Four episodic capsules chained into one map cluster, `topics`
/// EMPTY, shared themes carried entirely by `tags` — the live-corpus
/// shape (E1 dry-run 2026-06-11: 265/265 capsules have empty topics,
/// 255/265 have ≥2 tags). Generalize must fire on tags alone.
#[tokio::test(flavor = "multi_thread")]
async fn generalize_fires_on_pure_tags_corpus() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let angles = [0.0_f32, 30.0, 60.0, 90.0];
    for (i, deg) in angles.iter().enumerate() {
        let rad = deg.to_radians();
        seed(
            &store,
            &capsule_full(
                &format!("tag-{i}"),
                &format!("episodic lesson {i} about lance ann indexes"),
                CapabilityCapsuleType::Experience,
                Some("mem"),
                &[], // topics empty — live-corpus shape
                &["rust", "lance"],
                0.7,
                &format!("0000000000000000000{}", i + 1),
            ),
            (rad.cos(), rad.sin()),
        )
        .await;
    }
    let settings = evo_settings();
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, true)
        .await
        .unwrap();
    let gen: Vec<_> = report
        .proposals
        .iter()
        .filter(|p| p.op_kind == "generalize")
        .collect();
    assert_eq!(
        gen.len(),
        1,
        "tags-only corpus must yield a generalize proposal"
    );
    assert_eq!(gen[0].member_ids.len(), 4);
}

/// Shared themes only emerge after lowercasing and unioning across
/// BOTH fields: half the members carry "RUST" as a topic and "Lance"
/// as a tag, the other half the reverse in opposite cases. The
/// proposal capsule's `topics` must come out lowercased + sorted.
#[tokio::test(flavor = "multi_thread")]
async fn generalize_normalizes_case_across_topics_and_tags() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let angles = [0.0_f32, 30.0, 60.0, 90.0];
    for (i, deg) in angles.iter().enumerate() {
        let rad = deg.to_radians();
        let (topics, tags): (&[&str], &[&str]) = if i % 2 == 0 {
            (&["RUST"], &["Lance"])
        } else {
            (&["lance"], &["Rust"])
        };
        seed(
            &store,
            &capsule_full(
                &format!("mix-{i}"),
                &format!("episodic lesson {i} about lance ann indexes"),
                CapabilityCapsuleType::Experience,
                Some("mem"),
                topics,
                tags,
                0.7,
                &format!("0000000000000000000{}", i + 1),
            ),
            (rad.cos(), rad.sin()),
        )
        .await;
    }
    // k_cycles=1 → executes on first real sweep, so we can assert the
    // proposal capsule's normalized topics end-to-end.
    let mut settings = evo_settings();
    settings.k_cycles = 1;
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, false)
        .await
        .unwrap();
    assert_eq!(report.executed.len(), 1);
    let pending = store.list_pending_review(TENANT).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].topics,
        vec!["lance".to_string(), "rust".to_string()],
        "proposal topics must be the lowercased sorted union"
    );
}

/// Same embedding cluster but pairwise-disjoint themes — generalize
/// must NOT glue unrelated subjects together just because they sit
/// close in embedding space.
#[tokio::test(flavor = "multi_thread")]
async fn generalize_rejects_disjoint_theme_cluster() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let angles = [0.0_f32, 30.0, 60.0, 90.0];
    let themes: [&[&str]; 4] = [
        &["docker", "cifs"],
        &["mysql", "deadlock"],
        &["socks5", "tunnel"],
        &["playwright", "elementui"],
    ];
    for (i, deg) in angles.iter().enumerate() {
        let rad = deg.to_radians();
        seed(
            &store,
            &capsule_full(
                &format!("dis-{i}"),
                &format!("episodic lesson {i} on an unrelated subject"),
                CapabilityCapsuleType::Experience,
                Some("mem"),
                &[],
                themes[i],
                0.7,
                &format!("0000000000000000000{}", i + 1),
            ),
            (rad.cos(), rad.sin()),
        )
        .await;
    }
    let settings = evo_settings();
    let report = evolution_worker::sweep_once(&*store, &settings, TENANT, true)
        .await
        .unwrap();
    assert!(
        report.proposals.iter().all(|p| p.op_kind != "generalize"),
        "disjoint themes must never generalize, got {:?}",
        report.proposals,
    );
}
