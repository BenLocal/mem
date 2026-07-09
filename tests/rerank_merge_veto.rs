//! I2 P2 acceptance (docs/offline-reranker-lane.md §4) — the evolution
//! merge cross-encoder veto:
//!   - a merge candidate whose loser scores below `MEM_RERANK_MERGE_FLOOR`
//!     against the survivor is CANCELLED (suppressed from re-proposal),
//!     both capsules stay Active — nothing archived;
//!   - a candidate that clears the floor merges exactly as before;
//!   - with the lane disabled (default) the gate is a no-op even for a
//!     low-scoring pair.
//!
//! All phases run inside ONE test fn: the gate reads env live and env is
//! process-global — sequential phases avoid races without a lock.
//! Provider = `fake` (marker `rerank-low` → 0.05, else 0.95); the real
//! candle provider has its own `#[ignore]` smoke in `src/rerank/`.

use std::sync::Arc;

use mem::{
    config::{EvolutionSettings, EvolutionSynthesisMode},
    domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    },
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

/// Seed a near-duplicate pair (cosine ≈ 0.9999 ≥ merge 0.88). The loser
/// optionally carries the fake provider's low marker.
async fn seed_pair(store: &Store, loser_low: bool) {
    seed(
        store,
        &capsule(
            "winner",
            "a long and detailed lesson about lance write paths and refresh semantics",
        ),
        (1.0, 0.0),
    )
    .await;
    let loser_content = if loser_low {
        "short lance lesson rerank-low"
    } else {
        "short lance lesson"
    };
    seed(store, &capsule("loser", loser_content), (0.99, 0.01)).await;
}

async fn status_of(store: &Store, id: &str) -> CapabilityCapsuleStatus {
    store
        .get_capability_capsule_for_tenant(TENANT, id)
        .await
        .unwrap()
        .unwrap()
        .status
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_veto_gate_cancels_blocks_and_defaults_off() {
    std::env::set_var("MEM_RERANK_PROVIDER", "fake");

    // ── Phase A: lane ON + low-scoring loser → veto ─────────────────
    std::env::set_var("MEM_RERANK_OFFLINE_ENABLED", "1");
    {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("a.lance")).await.unwrap());
        seed_pair(&store, true).await;

        let report = evolution_worker::sweep_once(&*store, &settings(), TENANT, false)
            .await
            .unwrap();
        assert!(
            report.executed.is_empty(),
            "vetoed merge must not execute: {:?}",
            report.executed
        );
        assert_eq!(
            status_of(&store, "loser").await,
            CapabilityCapsuleStatus::Active,
            "vetoed merge must leave the loser Active"
        );
        // The candidate parks as `cancelled` — suppressed from
        // re-proposal, not retried every sweep.
        let cancelled = store
            .list_evolution_candidates(TENANT, Some("cancelled"))
            .await
            .unwrap();
        assert_eq!(
            cancelled.len(),
            1,
            "veto must settle the candidate as cancelled: {cancelled:?}"
        );
        assert_eq!(cancelled[0].op_kind, "merge");
    }

    // ── Phase B: lane ON + clean pair → merge executes as before ────
    {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("b.lance")).await.unwrap());
        seed_pair(&store, false).await;

        let report = evolution_worker::sweep_once(&*store, &settings(), TENANT, false)
            .await
            .unwrap();
        assert_eq!(report.executed.len(), 1, "clean merge must execute");
        assert_eq!(
            status_of(&store, "loser").await,
            CapabilityCapsuleStatus::Archived,
            "clean merge archives the loser"
        );
    }

    // ── Phase C: lane OFF (default) → marker is ignored ─────────────
    std::env::remove_var("MEM_RERANK_OFFLINE_ENABLED");
    {
        let dir = tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("c.lance")).await.unwrap());
        seed_pair(&store, true).await;

        let report = evolution_worker::sweep_once(&*store, &settings(), TENANT, false)
            .await
            .unwrap();
        assert_eq!(
            report.executed.len(),
            1,
            "with the lane off the gate must be a no-op"
        );
        assert_eq!(
            status_of(&store, "loser").await,
            CapabilityCapsuleStatus::Archived
        );
    }

    std::env::remove_var("MEM_RERANK_PROVIDER");
}
