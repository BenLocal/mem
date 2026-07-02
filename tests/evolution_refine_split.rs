//! E5 acceptance — ③ refine + ④ split detectors + the synthesis
//! backend fold-in (doc `docs/evolution-worker.md` §10 E5):
//!   - ③ fires on a CONTRADICTED but still-recalled capsule
//!     (hanging `suspected_supersede` edge, or ≥2 accumulated
//!     `outdated` feedback events) — the value gate blocks capsules
//!     nobody recalls,
//!   - ④ fires on a multi-chunk capsule whose chunk groups are both
//!     distinct (below `cluster_threshold`) and well separated (every
//!     cross-group pair ≤ `split_threshold`); coherent or mildly
//!     drifting chunks never split,
//!   - both land as `PendingConfirmation` review placeholders with
//!     `refined_from` / `split_from` lineage, sources untouched, and
//!     inherit the whole E3 review loop (edit_accept re-owns lineage
//!     with the op's own relation),
//!   - `synthesis=off` and `synthesis=review` behave identically
//!     (the E5 fold-in acceptance: Phase 1 IS the review backend).

use std::sync::Arc;

use mem::{
    config::{EvolutionSettings, EvolutionSynthesisMode},
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType,
        EditPendingRequest, FeedbackKind, GraphEdge, Scope, Visibility,
    },
    service::CapabilityCapsuleService,
    storage::{current_timestamp, Store},
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

fn vec2d(x: f32, y: f32) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[0] = x;
    v[1] = y;
    v
}

fn capsule(id: &str) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: TENANT.into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::Active,
        scope: Scope::Repo,
        visibility: Visibility::Shared,
        version: 1,
        summary: format!("summary of {id}"),
        content: format!("full verbatim body of {id} about lance write paths"),
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
        created_at: "00000000000000000001".into(),
        updated_at: "00000000000000000001".into(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

async fn seed(store: &Store, c: &CapabilityCapsuleRecord, v: (f32, f32)) {
    store.insert_capability_capsule(c.clone()).await.unwrap();
    store
        .upsert_capability_capsule_embedding(
            &c.capability_capsule_id,
            &c.tenant,
            "fake",
            DIM as i64,
            &f32_to_blob(&vec2d(v.0, v.1)),
            &c.content_hash,
            &c.updated_at,
            "00000000000000000001",
        )
        .await
        .unwrap();
}

async fn seed_chunked(store: &Store, c: &CapabilityCapsuleRecord, chunks: &[(f32, f32)]) {
    store.insert_capability_capsule(c.clone()).await.unwrap();
    let vectors: Vec<Vec<f32>> = chunks.iter().map(|&(x, y)| vec2d(x, y)).collect();
    store
        .upsert_capability_capsule_embedding_chunks(
            &c.capability_capsule_id,
            &c.tenant,
            "fake",
            DIM as i64,
            &vectors,
            &c.content_hash,
            &c.updated_at,
            "00000000000000000001",
        )
        .await
        .unwrap();
}

fn settings(k_cycles: u32, synthesis: EvolutionSynthesisMode) -> EvolutionSettings {
    EvolutionSettings {
        enabled: true,
        interval_secs: 86_400,
        k_cycles,
        evidence_decay: 0.7,
        hysteresis: 0.5,
        cluster_threshold: 0.80,
        merge_threshold: 1.1, // ① silenced
        generalize_min_n: 99, // ② silenced
        scan_limit: 1_000,
        prune_idle_cycles: 3,
        split_threshold: 0.5,
        synthesis,
    }
}

async fn sweep(store: &Store, s: &EvolutionSettings) -> evolution_worker::EvolutionReport {
    evolution_worker::sweep_once(store, s, TENANT, false)
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

fn suspected_edge(from_capsule: &str, to_capsule: &str) -> GraphEdge {
    GraphEdge {
        from_node_id: format!("capability_capsule:{from_capsule}"),
        to_node_id: format!("capability_capsule:{to_capsule}"),
        relation: "suspected_supersede".into(),
        valid_from: "00000000000000000002".into(),
        valid_to: None,
        confidence: None,
        extractor: Some("o7_neardup_cluster".into()),
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    }
}

/// ③ acceptance: contradiction (hanging suspected_supersede) + value
/// (recently recalled) → K-gated refine placeholder with `refined_from`
/// lineage; the source is never touched, and the placeholder references
/// the source by id + summary WITHOUT copying its content (verbatim
/// rule, generalize precedent).
#[tokio::test(flavor = "multi_thread")]
async fn refine_fires_on_suspected_supersede_for_recalled_capsule() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("src"), (1.0, 0.0)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["src".into()], &now)
        .await
        .unwrap();
    store
        .add_edge_direct(&suspected_edge("dup", "src"))
        .await
        .unwrap();
    let s = settings(2, EvolutionSynthesisMode::Review);

    let r1 = sweep(&store, &s).await;
    assert!(r1.executed.is_empty(), "K gate must hold on cycle 1");
    assert!(r1.proposals.iter().any(|p| p.op_kind == "refine"));

    let r2 = sweep(&store, &s).await;
    let refine = r2
        .executed
        .iter()
        .find(|e| e.op_kind == "refine")
        .expect("cycle 2 must execute the refine");
    let placeholder_id = refine.result_capsule_ids[0].clone();
    let placeholder = record_of(&store, &placeholder_id).await;
    assert_eq!(
        placeholder.status,
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert!(placeholder.tags.contains(&"evolution:refine".to_string()));
    assert_eq!(placeholder.evidence, vec!["src".to_string()]);
    assert!(placeholder.summary.contains("[evolution:refine]"));
    assert!(placeholder.content.contains("src"));
    assert!(
        placeholder.content.contains("suspected_supersede"),
        "conflict evidence must be listed: {}",
        placeholder.content
    );
    assert!(
        !placeholder.content.contains("full verbatim body of src"),
        "verbatim rule: the placeholder must not copy the source content"
    );
    assert!(active_relations(&store, &placeholder_id)
        .await
        .contains(&"refined_from".to_string()));
    assert_eq!(
        record_of(&store, "src").await.status,
        CapabilityCapsuleStatus::Active,
        "③ never touches the source in Phase 1"
    );
}

/// ③ value gate: the same contradiction signal on a capsule NOBODY
/// recalls must not propose — decay/idle-archive own the cold case.
#[tokio::test(flavor = "multi_thread")]
async fn refine_value_gate_blocks_unrecalled_capsules() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    seed(&store, &capsule("cold"), (1.0, 0.0)).await; // never recalled
    store
        .add_edge_direct(&suspected_edge("dup", "cold"))
        .await
        .unwrap();

    let r = sweep(&store, &settings(1, EvolutionSynthesisMode::Review)).await;
    assert!(
        !r.proposals.iter().any(|p| p.op_kind == "refine"),
        "no refine proposal without the recall value signal: {:?}",
        r.proposals
    );
}

/// ③ second contradiction channel: ≥2 accumulated `outdated` feedback
/// events on a recalled capsule.
#[tokio::test(flavor = "multi_thread")]
async fn refine_fires_on_accumulated_outdated_feedback() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed(&store, &capsule("stale"), (1.0, 0.0)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["stale".into()], &now)
        .await
        .unwrap();
    for _ in 0..2 {
        service
            .submit_feedback(TENANT, "stale", FeedbackKind::Outdated, None)
            .await
            .unwrap();
    }

    let r = sweep(&store, &settings(1, EvolutionSynthesisMode::Review)).await;
    let refine = r
        .executed
        .iter()
        .find(|e| e.op_kind == "refine")
        .expect("outdated x2 + recall must execute a refine");
    let placeholder = record_of(&store, &refine.result_capsule_ids[0]).await;
    assert!(
        placeholder.content.contains("outdated"),
        "conflict evidence must name the outdated signal: {}",
        placeholder.content
    );
}

/// H2 (oss-memory-diff §9): the verbatim notes riding `outdated`
/// feedback events flow into the refine placeholder as conflict
/// evidence — the reviewer sees WHY it's stale, not just a count
/// (review-gated version of MemOS's natural-language correction).
#[tokio::test(flavor = "multi_thread")]
async fn refine_placeholder_carries_outdated_feedback_notes() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed(&store, &capsule("stale"), (1.0, 0.0)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["stale".into()], &now)
        .await
        .unwrap();
    for note in [
        "the lance write path moved to route-B in June",
        "MEM_READ_ENGINE was removed entirely",
    ] {
        service
            .submit_feedback(
                TENANT,
                "stale",
                FeedbackKind::Outdated,
                Some(note.to_string()),
            )
            .await
            .unwrap();
    }

    let r = sweep(&store, &settings(1, EvolutionSynthesisMode::Review)).await;
    let refine = r
        .executed
        .iter()
        .find(|e| e.op_kind == "refine")
        .expect("outdated x2 must execute a refine");
    let placeholder = record_of(&store, &refine.result_capsule_ids[0]).await;
    assert!(
        placeholder
            .content
            .contains("the lance write path moved to route-B in June"),
        "first outdated note must appear verbatim: {}",
        placeholder.content
    );
    assert!(
        placeholder
            .content
            .contains("MEM_READ_ENGINE was removed entirely"),
        "second outdated note must appear verbatim: {}",
        placeholder.content
    );
}

/// ④ acceptance: chunks in two distinct, well-separated groups →
/// split placeholder with the group plan + `split_from` lineage;
/// coherent chunks and two-groups-but-not-separated both stay silent.
#[tokio::test(flavor = "multi_thread")]
async fn split_fires_only_on_separated_chunk_groups() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    // Well-separated: chunks 0,1 near x-axis, chunk 2 on y-axis
    // (cross cosine ≈ 0 ≤ split 0.5).
    seed_chunked(
        &store,
        &capsule("multi"),
        &[(1.0, 0.0), (0.99, 0.02), (0.0, 1.0)],
    )
    .await;
    // Coherent: one group only.
    seed_chunked(
        &store,
        &capsule("coherent"),
        &[(1.0, 0.0), (0.99, 0.02), (0.98, 0.04)],
    )
    .await;
    // Two groups (cos 45° ≈ 0.707 < 0.80) but NOT separated
    // (0.707 > split 0.5) — mild drift must not shred the capsule.
    seed_chunked(&store, &capsule("drift"), &[(1.0, 0.0), (0.707, 0.707)]).await;
    let now = current_timestamp();
    // Recalled so ⑤ orphan decay stays out of the picture.
    store
        .bump_last_used_at(
            TENANT,
            &["multi".into(), "coherent".into(), "drift".into()],
            &now,
        )
        .await
        .unwrap();

    let r = sweep(&store, &settings(1, EvolutionSynthesisMode::Review)).await;
    let splits: Vec<_> = r.executed.iter().filter(|e| e.op_kind == "split").collect();
    assert_eq!(splits.len(), 1, "exactly one split: {:?}", r.executed);
    assert_eq!(splits[0].member_ids, vec!["multi".to_string()]);
    let placeholder = record_of(&store, &splits[0].result_capsule_ids[0]).await;
    assert_eq!(
        placeholder.status,
        CapabilityCapsuleStatus::PendingConfirmation
    );
    assert!(placeholder.tags.contains(&"evolution:split".to_string()));
    assert!(placeholder.summary.contains("[evolution:split]"));
    assert!(
        placeholder.content.contains("group 1") && placeholder.content.contains("group 2"),
        "chunk-group plan must be listed: {}",
        placeholder.content
    );
    assert!(active_relations(&store, &placeholder.capability_capsule_id)
        .await
        .contains(&"split_from".to_string()));
    assert_eq!(
        record_of(&store, "multi").await.status,
        CapabilityCapsuleStatus::Active,
        "④ never touches the source in Phase 1"
    );
}

/// E3 inheritance: edit-accepting a ③ placeholder re-owns lineage with
/// the op's OWN relation (`refined_from`, not `generalizes`).
#[tokio::test(flavor = "multi_thread")]
async fn edit_accept_refine_lineage_uses_refined_from() {
    let dir = tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
    let service = CapabilityCapsuleService::new(store.clone());
    seed(&store, &capsule("src"), (1.0, 0.0)).await;
    let now = current_timestamp();
    store
        .bump_last_used_at(TENANT, &["src".into()], &now)
        .await
        .unwrap();
    store
        .add_edge_direct(&suspected_edge("dup", "src"))
        .await
        .unwrap();
    let r = sweep(&store, &settings(1, EvolutionSynthesisMode::Review)).await;
    let placeholder_id = r
        .executed
        .iter()
        .find(|e| e.op_kind == "refine")
        .unwrap()
        .result_capsule_ids[0]
        .clone();

    let response = service
        .edit_and_accept_pending(
            TENANT,
            EditPendingRequest {
                capability_capsule_id: placeholder_id,
                summary: "corrected lesson".into(),
                content: "The reviewer-reconciled corrected version of the lesson.".into(),
                evidence: vec![],
                code_refs: vec![],
                tags: vec!["rust".into()],
            },
        )
        .await
        .unwrap();
    let successor_rels =
        active_relations(&store, &response.capability_capsule.capability_capsule_id).await;
    assert!(
        successor_rels.contains(&"refined_from".to_string()),
        "successor must re-own refined_from lineage: {successor_rels:?}"
    );
    assert!(
        !successor_rels.contains(&"generalizes".to_string()),
        "refine must not masquerade as generalize lineage"
    );
}

/// E5 fold-in acceptance: `synthesis=off` → `review` switch has NO
/// behavior difference — Phase 1's detect+review form IS the review
/// backend (doc §6.2's design closure).
#[tokio::test(flavor = "multi_thread")]
async fn synthesis_off_and_review_behave_identically() {
    let mut placeholders: Vec<(String, String)> = Vec::new();
    for mode in [EvolutionSynthesisMode::Off, EvolutionSynthesisMode::Review] {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("evo.lance")).await.unwrap());
        seed(&store, &capsule("src"), (1.0, 0.0)).await;
        let now = current_timestamp();
        store
            .bump_last_used_at(TENANT, &["src".into()], &now)
            .await
            .unwrap();
        store
            .add_edge_direct(&suspected_edge("dup", "src"))
            .await
            .unwrap();
        let r = sweep(&store, &settings(1, mode)).await;
        let refine = r
            .executed
            .iter()
            .find(|e| e.op_kind == "refine")
            .expect("both modes must execute the refine");
        let p = record_of(&store, &refine.result_capsule_ids[0]).await;
        placeholders.push((p.summary, p.content));
    }
    assert_eq!(
        placeholders[0], placeholders[1],
        "off and review must produce byte-identical placeholders"
    );
}
