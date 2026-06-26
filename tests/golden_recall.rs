//! O6a/O6b — gold-set recall regression gate (closes oss-memory-diff O6 a/b).
//!
//! Unlike the synthetic ablation bench (`tests/recall_bench.rs`, `#[ignore]`),
//! this is a **non-ignored** test that runs in plain `cargo test`: it ingests a
//! small, hand-curated, Chinese-heavy gold corpus through the *real* ranker
//! (`rank_with_hybrid_and_graph`) and asserts the query-averaged IR metrics stay
//! at or above a versioned floor (`tests/golden_recall/baseline.json`). A ranking
//! change that drops recall/ndcg/mrr/precision fails CI — turning "did this
//! ranking edit make recall worse?" into a reviewable diff.
//!
//! Hermetic by construction: embeddings come from the deterministic
//! `GeometryProvider` (one orthogonal axis per topic, same fn for content and
//! query — no embedding model, no network), and jieba BM25 + the additive
//! ranking stack are exercised over real Chinese text. `min_score = Some(0)`
//! isolates *ranking order* from the absolute relevance floor (which is its own,
//! env-tunable concern).
//!
//! Corpus discipline: pure mem-internal technical content, generic placeholders,
//! no real client names (public repo). Topic anchor terms are mutually exclusive
//! per capsule so the geometry axes stay clean.

#[path = "bench/geometry.rs"]
mod geometry;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use geometry::GeometryProvider;
use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::embedding::EmbeddingProvider;
use mem::pipeline::eval_metrics::{mrr, ndcg_at_k, precision_at_k, recall_at_k};
use mem::pipeline::retrieve::rank_with_hybrid_and_graph;
use mem::service::CapabilityCapsuleService;
use mem::storage::{GraphStore, Store};
use serde::Deserialize;
use tempfile::tempdir;

const CORPUS: &str = include_str!("golden_recall/corpus.json");
const QRELS: &str = include_str!("golden_recall/qrels.json");
const BASELINE: &str = include_str!("golden_recall/baseline.json");

/// Hybrid candidate fan-out before ranking (mirrors `tests/bench/runner.rs`).
const HYBRID_K: usize = 48;
const TENANT: &str = "gold";
/// Float slack so an exact-equal baseline never flakes on rounding.
const EPS: f64 = 1e-6;

#[derive(Deserialize)]
struct GoldCapsule {
    id: String,
    topic: String,
    content: String,
}

#[derive(Deserialize)]
struct GoldQuery {
    #[allow(dead_code)]
    id: String,
    intent: String,
    query: String,
    relevant: Vec<String>,
}

#[derive(Deserialize)]
struct Baseline {
    recall_at_5: f64,
    recall_at_10: f64,
    ndcg_at_10: f64,
    mrr: f64,
    precision_at_5: f64,
}

#[tokio::test(flavor = "multi_thread")]
async fn golden_recall_meets_baseline() {
    let capsules: Vec<GoldCapsule> = serde_json::from_str(CORPUS).expect("corpus.json parse");
    let queries: Vec<GoldQuery> = serde_json::from_str(QRELS).expect("qrels.json parse");
    let baseline: Baseline = serde_json::from_str(BASELINE).expect("baseline.json parse");

    // One orthogonal geometry axis per distinct topic; same embed fn for
    // content and query makes same-topic items nearest neighbours.
    let mut topics: Vec<String> = capsules.iter().map(|c| c.topic.clone()).collect();
    topics.sort();
    topics.dedup();
    let topic_refs: Vec<&str> = topics.iter().map(|s| s.as_str()).collect();
    let dim = topic_refs.len().max(8) + 8;
    let geometry = GeometryProvider::new(&topic_refs, dim);

    let dir = tempdir().expect("tempdir");
    let store = Arc::new(
        Store::open(&dir.path().join("gold.duckdb"))
            .await
            .expect("Store::open"),
    );
    let svc = CapabilityCapsuleService::with_providers(
        store.clone(),
        "fake".into(),
        Some(Arc::new(geometry.clone())),
    );

    // Ingest each gold capsule and attach its designed-geometry embedding.
    let mut id_map: HashMap<String, String> = HashMap::new();
    for cap in &capsules {
        let resp = svc
            .ingest(IngestCapabilityCapsuleRequest {
                tenant: TENANT.into(),
                capability_capsule_type: CapabilityCapsuleType::Implementation,
                content: cap.content.clone(),
                summary: None,
                evidence: vec![],
                code_refs: vec![],
                scope: Scope::Repo,
                visibility: Visibility::Shared,
                project: Some("gold".into()),
                repo: Some("gold".into()),
                module: None,
                task_type: None,
                tags: vec![],
                topics: vec![cap.topic.clone()],
                source_agent: "gold".into(),
                idempotency_key: None,
                write_mode: WriteMode::Auto,
                supersedes_capability_capsule_id: None,
                expires_at: None,
            })
            .await
            .expect("ingest");
        let stored_id = resp.capability_capsule_id;

        let recs = store
            .fetch_capability_capsules_by_ids(TENANT, &[stored_id.as_str()])
            .await
            .expect("fetch by id");
        let rec = recs
            .into_iter()
            .find(|r| r.capability_capsule_id == stored_id)
            .expect("stored record present");

        store
            .upsert_capability_capsule_embedding_chunks(
                &stored_id,
                TENANT,
                "geometry-bench",
                geometry.dim() as i64,
                &[geometry.raw(&cap.content)],
                &rec.content_hash,
                &rec.updated_at,
                &rec.updated_at,
            )
            .await
            .expect("embedding upsert");

        id_map.insert(cap.id.clone(), stored_id);
    }

    // Run each gold query through the real ranker and accumulate metrics.
    let (mut r5, mut r10, mut ndcg, mut mrr_s, mut p5) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let n = queries.len().max(1) as f64;
    for q in &queries {
        let query_vec = geometry.raw(&q.query);
        let pool = store
            .search_candidates(TENANT)
            .await
            .expect("search_candidates");
        let hybrid_hits = store
            .hybrid_candidates(TENANT, &q.query, query_vec.as_slice(), HYBRID_K)
            .await
            .expect("hybrid_candidates");

        let request = SearchCapabilityCapsuleRequest {
            query: q.query.clone(),
            intent: q.intent.clone(),
            scope_filters: vec![],
            token_budget: 4096,
            caller_agent: "gold".into(),
            expand_graph: false,
            tenant: Some(TENANT.into()),
            // Isolate ranking ORDER from the absolute relevance floor.
            min_score: Some(0),
        };
        let graph: &dyn GraphStore = store.as_ref();
        let ranked = rank_with_hybrid_and_graph(pool, hybrid_hits, &request, graph, None)
            .await
            .expect("rank_with_hybrid_and_graph");
        let run: Vec<String> = ranked
            .iter()
            .map(|r| r.capability_capsule_id.clone())
            .collect();

        let qrels: HashSet<String> = q
            .relevant
            .iter()
            .filter_map(|fid| id_map.get(fid).cloned())
            .collect();
        assert!(
            qrels.len() == q.relevant.len(),
            "qrels for {} reference unknown capsule ids",
            q.id
        );

        r5 += recall_at_k(&run, &qrels, 5);
        r10 += recall_at_k(&run, &qrels, 10);
        ndcg += ndcg_at_k(&run, &qrels, 10);
        mrr_s += mrr(&run, &qrels);
        p5 += precision_at_k(&run, &qrels, 5);
    }
    let (r5, r10, ndcg, mrr_s, p5) = (r5 / n, r10 / n, ndcg / n, mrr_s / n, p5 / n);

    println!(
        "\n== golden_recall: {} capsules, {} queries ==",
        capsules.len(),
        queries.len()
    );
    println!(
        "observed: recall@5={r5:.4} recall@10={r10:.4} ndcg@10={ndcg:.4} mrr={mrr_s:.4} precision@5={p5:.4}"
    );
    println!(
        "baseline: recall@5={:.4} recall@10={:.4} ndcg@10={:.4} mrr={:.4} precision@5={:.4}",
        baseline.recall_at_5,
        baseline.recall_at_10,
        baseline.ndcg_at_10,
        baseline.mrr,
        baseline.precision_at_5
    );

    assert!(
        r5 >= baseline.recall_at_5 - EPS,
        "recall@5 regressed: {r5:.4} < baseline {:.4}",
        baseline.recall_at_5
    );
    assert!(
        r10 >= baseline.recall_at_10 - EPS,
        "recall@10 regressed: {r10:.4} < baseline {:.4}",
        baseline.recall_at_10
    );
    assert!(
        ndcg >= baseline.ndcg_at_10 - EPS,
        "ndcg@10 regressed: {ndcg:.4} < baseline {:.4}",
        baseline.ndcg_at_10
    );
    assert!(
        mrr_s >= baseline.mrr - EPS,
        "mrr regressed: {mrr_s:.4} < baseline {:.4}",
        baseline.mrr
    );
    assert!(
        p5 >= baseline.precision_at_5 - EPS,
        "precision@5 regressed: {p5:.4} < baseline {:.4}",
        baseline.precision_at_5
    );
}
