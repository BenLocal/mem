//! E4 acceptance — ⑤ reweight + ⑥ Hebbian (doc `docs/evolution-worker.md`
//! §10 E4):
//!   - reweight is auditable: every confidence / decay nudge lands as a
//!     `feedback_events` row (system kinds `system_reweight_up` /
//!     `system_reweight_decay`), visible in `feedback_summary`,
//!   - stable + highly-recalled clusters gain +0.02 confidence per
//!     signal cycle (recurring after the K gate opens), capped at 0.9,
//!   - K-cycle orphans with zero recalls gain +0.05 decay per cycle —
//!     archiving stays `idle_archive_worker`'s job,
//!   - co-recall (same `last_used_at` flush batch) earns a
//!     `co_recalled_with` edge after K FRESH batches; a stale batch
//!     re-observed across sweeps accumulates nothing,
//!   - prune closes ONLY idle `extractor="evolution"` co_recalled_with
//!     edges — caller edges and `user_tunnel:*` are never touched,
//!   - the public feedback API rejects system-emitted kinds,
//!   - dry-run writes nothing (no events, no edges, no candidates).

use std::sync::Arc;

use mem::{
    config::{EvolutionSettings, EvolutionSynthesisMode},
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, FeedbackKind,
        GraphEdge, Scope, Visibility,
    },
    service::CapabilityCapsuleService,
    storage::{current_timestamp, CapsuleStore, EvolutionCandidateStore, Store},
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

fn capsule(id: &str, confidence: f32) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary of {id}"),
        content: format!("episodic content of {id} about lance write paths"),
        evidence: vec![],
        code_refs: vec![],
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec!["rust".into(), "lance".into()],
        topics: vec![],
        confidence,
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

/// ①② silenced (merge unreachable, generalize min-n unreachable) so the
/// tests isolate ⑤⑥.
fn dynamics_settings(k_cycles: u32) -> EvolutionSettings {
    EvolutionSettings {
        enabled: true,
        interval_secs: 86_400,
        k_cycles,
        evidence_decay: 0.7,
        hysteresis: 0.5,
        cluster_threshold: 0.80,
        merge_threshold: 1.1,
        generalize_min_n: 99,
        scan_limit: 1_000,
        prune_idle_cycles: 3,
        split_threshold: 0.5,
        synthesis: EvolutionSynthesisMode::Review,
    }
}

async fn sweep(
    store: &Store,
    settings: &EvolutionSettings,
    dry: bool,
) -> evolution_worker::EvolutionReport {
    evolution_worker::sweep_once(store, settings, TENANT, dry)
        .await
        .unwrap()
}

async fn record_of(store: &Store, id: &str) -> CapabilityCapsuleRecord {
    store
        .get_capability_capsule_for_tenant(TENANT, id)
        .await
        .unwrap()
        .expect("capsule row must exist")
}

async fn active_corecall_edges(store: &Store) -> Vec<GraphEdge> {
    store
        .query_predicate("co_recalled_with", None)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.valid_to.is_none())
        .collect()
}

/// ⑤ reweight-up: a stable cluster whose members are actively recalled
/// (>0.5 share) gains +0.02 confidence per signal cycle once the K gate
/// opens — recurring, audit-trailed in `feedback_events`, capped at 0.9.
#[tokio::test(flavor = "multi_thread")]
async fn stable_high_recall_cluster_gains_confidence_auditable() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    // Three near-neighbours; m2 sits at the 0.9 cap already.
    seed(&store, &capsule("m0", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("m1", 0.7), (0.999, 0.02)).await;
    seed(&store, &capsule("m2", 0.92), (0.998, 0.04)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["m0".into(), "m1".into()], &now)
        .await
        .unwrap(); // 2/3 members recently recalled → share > 0.5
    let settings = dynamics_settings(2);

    let r1 = sweep(&store, &settings, false).await;
    assert!(r1.executed.is_empty(), "K gate must hold on cycle 1");
    assert_eq!(
        store
            .feedback_summary("m0")
            .await
            .unwrap()
            .system_reweight_up,
        0
    );

    let r2 = sweep(&store, &settings, false).await;
    assert!(
        r2.executed.iter().any(|e| e.op_kind == "reweight_up"),
        "cycle 2 must execute reweight_up: got {:?}",
        r2.executed
    );
    assert!((record_of(&store, "m0").await.confidence - 0.72).abs() < 1e-3);
    assert!((record_of(&store, "m1").await.confidence - 0.72).abs() < 1e-3);
    assert!(
        (record_of(&store, "m2").await.confidence - 0.92).abs() < 1e-3,
        "members at/above the 0.9 cap must not be bumped"
    );
    let summary = store.feedback_summary("m0").await.unwrap();
    assert_eq!(
        summary.system_reweight_up, 1,
        "each nudge must be auditable in feedback_events"
    );

    // Recurring: the signal held → another +0.02 on cycle 3.
    let r3 = sweep(&store, &settings, false).await;
    assert!(r3.executed.iter().any(|e| e.op_kind == "reweight_up"));
    assert!((record_of(&store, "m0").await.confidence - 0.74).abs() < 1e-3);
    assert_eq!(
        store
            .feedback_summary("m0")
            .await
            .unwrap()
            .system_reweight_up,
        2
    );
}

/// ⑤ reweight-decay: a K-cycle orphan with ZERO recalls slides faster
/// toward idle-archive (+0.05 decay per cycle) — status untouched, the
/// nudge is auditable. A recalled singleton is NOT an orphan.
#[tokio::test(flavor = "multi_thread")]
async fn orphan_with_zero_recall_decays_auditable() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("orphan", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("recalled", 0.7), (0.0, 1.0)).await; // orthogonal singleton
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["recalled".into()], &now)
        .await
        .unwrap();
    let settings = dynamics_settings(2);

    let r1 = sweep(&store, &settings, false).await;
    assert!(r1.executed.is_empty());
    let r2 = sweep(&store, &settings, false).await;
    assert!(
        r2.executed.iter().any(|e| e.op_kind == "reweight_decay"),
        "cycle 2 must execute reweight_decay: got {:?}",
        r2.executed
    );
    let orphan = record_of(&store, "orphan").await;
    assert!((orphan.decay_score - 0.05).abs() < 1e-3);
    assert_eq!(
        orphan.status,
        CapabilityCapsuleStatus::Active,
        "⑤ only adjusts signal — archiving stays idle_archive_worker's job"
    );
    assert_eq!(
        store
            .feedback_summary("orphan")
            .await
            .unwrap()
            .system_reweight_decay,
        1
    );
    // The recalled singleton is not an orphan — untouched.
    assert!((record_of(&store, "recalled").await.decay_score).abs() < 1e-6);

    let _ = sweep(&store, &settings, false).await;
    assert!((record_of(&store, "orphan").await.decay_score - 0.10).abs() < 1e-3);
}

/// ⑥ co-recall: two capsules bumped in the same `last_used_worker`
/// flush share a `last_used_at` value — K FRESH batches (a new
/// timestamp each cycle) earn a `co_recalled_with` edge with
/// `extractor="evolution"`.
#[tokio::test(flavor = "multi_thread")]
async fn corecall_pair_earns_edge_after_k_fresh_batches() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("a", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("b", 0.7), (0.0, 1.0)).await; // no cluster relation
    let settings = dynamics_settings(2);

    let ts1 = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["a".into(), "b".into()], &ts1)
        .await
        .unwrap();
    let r1 = sweep(&store, &settings, false).await;
    assert!(r1.executed.is_empty(), "K gate must hold on cycle 1");
    assert!(active_corecall_edges(&store).await.is_empty());

    let ts2 = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["a".into(), "b".into()], &ts2)
        .await
        .unwrap();
    let r2 = sweep(&store, &settings, false).await;
    assert!(
        r2.executed.iter().any(|e| e.op_kind == "corecall"),
        "second FRESH batch must open the gate: got {:?}",
        r2.executed
    );
    let edges = active_corecall_edges(&store).await;
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].extractor.as_deref(), Some("evolution"));
    assert_eq!(edges[0].from_node_id, "capability_capsule:a");
    assert_eq!(edges[0].to_node_id, "capability_capsule:b");
}

/// ⑥ freshness guard: the SAME batch timestamp re-observed on later
/// sweeps is not new evidence — the candidate decays instead of
/// accumulating, and no edge is ever written.
#[tokio::test(flavor = "multi_thread")]
async fn stale_corecall_batch_does_not_accumulate() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("a", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("b", 0.7), (0.0, 1.0)).await;
    let settings = dynamics_settings(2);

    let ts1 = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["a".into(), "b".into()], &ts1)
        .await
        .unwrap();
    for cycle in 1..=3 {
        let r = sweep(&store, &settings, false).await;
        assert!(
            !r.executed.iter().any(|e| e.op_kind == "corecall"),
            "cycle {cycle}: one stale batch must never execute"
        );
    }
    assert!(active_corecall_edges(&store).await.is_empty());
}

/// ⑥ prune: only IDLE `extractor="evolution"` co_recalled_with edges
/// are closed — caller-written edges and `user_tunnel:*` edges are
/// exempt by contract, and closing means `valid_to`, never deletion.
#[tokio::test(flavor = "multi_thread")]
async fn prune_closes_only_idle_evolution_corecall_edges() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    for id in ["c1", "c2", "c3", "c4"] {
        seed(&store, &capsule(id, 0.7), (1.0, 0.0)).await;
    }
    let old = "00000000000000001000".to_string();
    let mk = |from: &str, to: &str, relation: &str, extractor: Option<&str>| GraphEdge {
        from_node_id: from.into(),
        to_node_id: to.into(),
        relation: relation.into(),
        valid_from: old.clone(),
        valid_to: None,
        confidence: None,
        extractor: extractor.map(str::to_string),
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    };
    store
        .add_edge_direct(&mk(
            "capability_capsule:c1",
            "capability_capsule:c2",
            "co_recalled_with",
            Some("evolution"),
        ))
        .await
        .unwrap();
    store
        .add_edge_direct(&mk(
            "capability_capsule:c3",
            "capability_capsule:c4",
            "co_recalled_with",
            None, // caller-written — exempt
        ))
        .await
        .unwrap();
    store
        .add_edge_direct(&mk(
            "entity:p1",
            "entity:p2",
            "user_tunnel:topic:rust",
            Some("topic_tunnel"), // tunnel — exempt
        ))
        .await
        .unwrap();

    // K pinned high: nothing executes, the sweep only prunes.
    let report = sweep(&store, &dynamics_settings(99), false).await;
    assert_eq!(report.pruned_edges, 1, "exactly the idle evolution edge");

    let corecall = store
        .query_predicate("co_recalled_with", None)
        .await
        .unwrap();
    let active: Vec<&GraphEdge> = corecall.iter().filter(|e| e.valid_to.is_none()).collect();
    assert_eq!(active.len(), 1, "caller edge must survive");
    assert_eq!(active[0].extractor, None);
    let closed: Vec<&GraphEdge> = corecall.iter().filter(|e| e.valid_to.is_some()).collect();
    assert_eq!(closed.len(), 1, "evolution edge closed, not deleted");
    let tunnels = store
        .query_predicate("user_tunnel:topic:rust", None)
        .await
        .unwrap();
    assert!(
        tunnels.iter().any(|e| e.valid_to.is_none()),
        "user_tunnel edges are never pruned"
    );
}

/// System-emitted kinds are worker-only — the public feedback surface
/// must reject them (an external caller could otherwise forge signal
/// or trigger AutoPromoted's status flip).
#[tokio::test(flavor = "multi_thread")]
async fn public_feedback_api_rejects_system_kinds() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed(&store, &capsule("cap", 0.7), (1.0, 0.0)).await;

    for kind in [
        FeedbackKind::SystemReweightUp,
        FeedbackKind::SystemReweightDecay,
        FeedbackKind::AutoPromoted,
    ] {
        assert!(
            service
                .submit_feedback(TENANT, "cap", kind.clone(), None)
                .await
                .is_err(),
            "system kind {kind:?} must be rejected by the public API"
        );
    }
    assert!(service
        .submit_feedback(TENANT, "cap", FeedbackKind::Useful, None)
        .await
        .is_ok());
}

/// Dry-run previews ⑤⑥ but writes NOTHING — no feedback events, no
/// edges, no candidate rows, no prune.
#[tokio::test(flavor = "multi_thread")]
async fn dry_run_writes_no_reweight_no_edges() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("m0", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("m1", 0.7), (0.999, 0.02)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["m0".into(), "m1".into()], &now)
        .await
        .unwrap();

    let report = sweep(&store, &dynamics_settings(1), true).await;
    assert!(!report.proposals.is_empty(), "signals must be previewed");
    assert!(report.executed.is_empty());
    assert!((record_of(&store, "m0").await.confidence - 0.7).abs() < 1e-6);
    assert_eq!(store.feedback_summary("m0").await.unwrap().total, 0);
    assert!(active_corecall_edges(&store).await.is_empty());
    assert!(store
        .list_evolution_candidates(TENANT, None)
        .await
        .unwrap()
        .is_empty());
}

/// Audit 2026-07-03 #2(a): the hourly decay sweep stamps `last_used_at`
/// on EVERY active row (it is the decay clock, not recall evidence).
/// ⑤ reweight-up must key on `last_recalled_at` — a decay-stamped
/// cluster with zero real recalls earns nothing.
#[tokio::test(flavor = "multi_thread")]
async fn decay_sweep_stamp_is_not_recall_evidence_for_reweight_up() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("m0", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("m1", 0.7), (0.999, 0.02)).await;
    seed(&store, &capsule("m2", 0.7), (0.998, 0.04)).await;
    // One decay tick — rate 0 so only the clock stamp moves.
    let now = current_timestamp();
    let now_ms: f64 = now.parse::<u64>().unwrap() as f64;
    store
        .apply_time_decay(0.0, now_ms, 86_400_000.0, &now)
        .await
        .unwrap();

    let settings = dynamics_settings(2);
    for cycle in 1..=3 {
        let r = sweep(&store, &settings, false).await;
        assert!(
            !r.executed.iter().any(|e| e.op_kind == "reweight_up"),
            "cycle {cycle}: decay-stamped cluster must not earn reweight_up"
        );
    }
    assert!(
        (record_of(&store, "m0").await.confidence - 0.7).abs() < 1e-3,
        "confidence must not move without real recalls"
    );
}

/// Audit 2026-07-03 #2(b): ⑤ reweight-decay keys on `last_recalled_at`
/// (never recalled), so the orphan lane still fires after the decay
/// sweep has stamped `last_used_at` on every row.
#[tokio::test(flavor = "multi_thread")]
async fn orphan_decay_fires_even_after_decay_sweep_stamp() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("orphan", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("recalled", 0.7), (0.0, 1.0)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["recalled".into()], &now)
        .await
        .unwrap();
    let now = current_timestamp();
    let now_ms: f64 = now.parse::<u64>().unwrap() as f64;
    store
        .apply_time_decay(0.0, now_ms, 86_400_000.0, &now)
        .await
        .unwrap();

    let settings = dynamics_settings(2);
    let r1 = sweep(&store, &settings, false).await;
    assert!(r1.executed.is_empty(), "K gate must hold on cycle 1");
    let r2 = sweep(&store, &settings, false).await;
    assert!(
        r2.executed.iter().any(|e| e.op_kind == "reweight_decay"),
        "orphan lane must survive the decay stamp: got {:?}",
        r2.executed
    );
    let orphan = record_of(&store, "orphan").await;
    assert!((orphan.decay_score - 0.05).abs() < 1e-3);
}

/// Audit 2026-07-03 #2(c): the decay sweep homogenizes `last_used_at`
/// across the corpus — that shared stamp is NOT a co-recall batch and
/// must never mint Hebbian edges.
#[tokio::test(flavor = "multi_thread")]
async fn decay_sweep_stamp_mints_no_corecall_edges() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("a", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("b", 0.7), (0.0, 1.0)).await;
    seed(&store, &capsule("c", 0.7), (0.5, 0.86)).await;
    let now = current_timestamp();
    let now_ms: f64 = now.parse::<u64>().unwrap() as f64;
    store
        .apply_time_decay(0.0, now_ms, 86_400_000.0, &now)
        .await
        .unwrap();

    // K=1 executes anything that proposes — nothing may propose.
    let r = sweep(&store, &dynamics_settings(1), false).await;
    assert!(
        !r.executed.iter().any(|e| e.op_kind == "corecall"),
        "decay stamp executed corecall: {:?}",
        r.executed
    );
    assert!(
        active_corecall_edges(&store).await.is_empty(),
        "no Hebbian edge may come from the decay stamp"
    );
}

/// Audit 2026-07-03 ⑧: prune judges idleness only for edges with at
/// least one endpoint inside THIS sweep's scanned active set. Another
/// tenant's (or an out-of-window) edge is invisible to the scan and
/// must be left alone — fail-safe, not fail-prune.
#[tokio::test(flavor = "multi_thread")]
async fn prune_skips_edges_whose_endpoints_are_outside_the_scan() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("mine", 0.7), (1.0, 0.0)).await;
    // Endpoints belong to another tenant — not in this sweep's map.
    store
        .add_edge_direct(&GraphEdge {
            from_node_id: "capability_capsule:other_a".into(),
            to_node_id: "capability_capsule:other_b".into(),
            relation: "co_recalled_with".into(),
            valid_from: "00000000000000001000".into(), // ancient
            valid_to: None,
            confidence: None,
            extractor: Some("evolution".into()),
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        })
        .await
        .unwrap();

    let report = sweep(&store, &dynamics_settings(99), false).await;
    assert_eq!(
        report.pruned_edges, 0,
        "foreign-endpoint edge must not be pruned"
    );
    assert_eq!(
        active_corecall_edges(&store).await.len(),
        1,
        "the other tenant's edge must stay open"
    );
}

/// Audit 2026-07-03 ⑫: guidance capsules (Preference / Workflow) are
/// curated, not evolved — ⑤ reweight-up must not bump their
/// confidence, mirroring the ①④⑤-decay exclusions.
#[tokio::test(flavor = "multi_thread")]
async fn reweight_up_never_touches_guidance_capsules() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let mut p0 = capsule("p0", 0.7);
    p0.capability_capsule_type = CapabilityCapsuleType::Preference;
    let mut p1 = capsule("p1", 0.7);
    p1.capability_capsule_type = CapabilityCapsuleType::Preference;
    seed(&store, &p0, (1.0, 0.0)).await;
    seed(&store, &p1, (0.999, 0.02)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["p0".into(), "p1".into()], &now)
        .await
        .unwrap();

    let r = sweep(&store, &dynamics_settings(1), false).await;
    assert!(
        !r.executed.iter().any(|e| e.op_kind == "reweight_up"),
        "guidance-only cluster must not earn reweight_up: {:?}",
        r.executed
    );
    assert!((record_of(&store, "p0").await.confidence - 0.7).abs() < 1e-3);
}

/// Audit 2026-07-03 ⑪: a cancelled corecall candidate remembers its
/// batch — re-detecting the SAME batch_ts after cancellation must not
/// mint a fresh candidate row (create→cancel churn would leak one row
/// per hysteresis window forever).
#[tokio::test(flavor = "multi_thread")]
async fn cancelled_corecall_does_not_churn_duplicate_candidates() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("a", 0.7), (1.0, 0.0)).await;
    seed(&store, &capsule("b", 0.7), (0.0, 1.0)).await;
    let settings = dynamics_settings(2);
    let ts = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["a".into(), "b".into()], &ts)
        .await
        .unwrap();

    // s1 creates the candidate; s2 holds (silence decay 0.7); s3
    // cancels (0.49 < hysteresis 0.5).
    for _ in 1..=3 {
        sweep(&store, &settings, false).await;
    }
    let cancelled: Vec<_> = store
        .list_evolution_candidates(TENANT, Some("cancelled"))
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.op_kind == "corecall")
        .collect();
    assert_eq!(cancelled.len(), 1, "the churn precondition: one cancel");

    // s4/s5: the same stale batch re-detected must NOT re-enter.
    for cycle in 4..=5 {
        sweep(&store, &settings, false).await;
        let pending: Vec<_> = store
            .list_evolution_candidates(TENANT, Some("pending"))
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.op_kind == "corecall")
            .collect();
        assert!(
            pending.is_empty(),
            "cycle {cycle}: cancelled batch_ts re-minted a candidate"
        );
    }
}
