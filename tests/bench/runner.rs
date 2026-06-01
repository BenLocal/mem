//! Bench runner: ingest a `Fixture` into a fresh on-disk `Store`, attach
//! designed-geometry embeddings, and run ablation "rungs" through the real
//! public ranker (`rank_with_hybrid_and_graph`). Each rung reports the
//! standard IR metrics averaged over the fixture's queries.
//!
//! Task 3 implements the `Rung::Hybrid` baseline only; the remaining rungs
//! fall through to the hybrid composition until later tasks specialize them.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::embedding::EmbeddingProvider;
use mem::pipeline::eval_metrics::{mrr, ndcg_at_k, precision_at_k, recall_at_k};
use mem::pipeline::retrieve::rank_with_hybrid_and_graph;
use mem::service::CapabilityCapsuleService;
use mem::storage::{GraphStore, Store};
use tempfile::tempdir;

use crate::bench::fixture::Fixture;
use crate::bench::geometry::GeometryProvider;

/// Number of hybrid candidates to fan out before ranking.
const HYBRID_K: usize = 48;
/// Cutoff for the rank-position metrics.
const METRIC_K: usize = 10;

/// Variants other than `Hybrid` are consumed by Tasks 4+ (full rung
/// ladder); allow dead_code to keep the public API stable across tasks.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    LexicalOnly,
    SemanticOnly,
    Hybrid,
    Graph,
    Dynamics,
    ChunkingOn,
    ChunkingOff,
    Oracle,
}

/// Report fields beyond `recall_at_10` are consumed by the Task 5 output
/// layer (pretty table / JSON); allow dead_code until then.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RungReport {
    pub rung: Rung,
    pub ndcg_at_10: f64,
    pub mrr: f64,
    pub recall_at_10: f64,
    pub precision_at_10: f64,
}

#[derive(Debug, Clone)]
pub struct BenchReport {
    pub reports: Vec<RungReport>,
}

/// An ingested fixture: a live `Store` with geometry embeddings attached,
/// plus the geometry provider (for query-vector synthesis) and the
/// fixture-id → stored-id translation map.
struct IngestedFixture {
    store: Arc<Store>,
    geometry: GeometryProvider,
    tenant: String,
    /// fixture capsule id -> stored (UUID) capability_capsule_id.
    id_map: HashMap<String, String>,
}

/// Ingest every capsule in `f` into a fresh on-disk `Store`, attach a
/// designed-geometry embedding for each, and return the live handle plus a
/// fixture-id → stored-id map.
///
/// Geometry provider gets one orthogonal basis per fixture topic; `dim` is
/// padded a little above the topic count so every topic owns an axis.
async fn ingest_fixture(f: &Fixture) -> IngestedFixture {
    let topic_refs: Vec<&str> = f.topics.iter().map(|s| s.as_str()).collect();
    let dim = topic_refs.len().max(8) + 8;
    let geometry = GeometryProvider::new(&topic_refs, dim);

    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("recall_bench.duckdb");
    // Keep the tempdir alive for the lifetime of the Store by leaking it:
    // the Store ATTACHes the lance dir on disk and reads from it for the
    // whole bench run. Dropping the dir mid-run would unlink the data.
    std::mem::forget(dir);
    let store = Arc::new(Store::open(&db).await.expect("Store::open"));

    // Mirror tests/hybrid_search.rs: ingest through the service with the
    // "fake" job provider and the geometry provider as the search provider.
    let svc = CapabilityCapsuleService::with_providers(
        store.clone(),
        "fake".into(),
        Some(Arc::new(geometry.clone())),
    );

    let mut id_map: HashMap<String, String> = HashMap::new();

    for cap in &f.capsules {
        let resp = svc
            .ingest(IngestCapabilityCapsuleRequest {
                tenant: f.tenant.clone(),
                capability_capsule_type: CapabilityCapsuleType::Implementation,
                content: cap.content.clone(),
                summary: None,
                evidence: vec![],
                code_refs: vec![],
                scope: Scope::Repo,
                visibility: Visibility::Shared,
                project: Some("bench".into()),
                repo: Some("bench".into()),
                module: None,
                task_type: None,
                tags: vec![],
                topics: cap.topics.clone(),
                source_agent: "bench".into(),
                idempotency_key: None,
                write_mode: WriteMode::Auto,
                supersedes_capability_capsule_id: None,
            })
            .await
            .expect("ingest");
        let stored_id = resp.capability_capsule_id;

        // Fetch the stored record to obtain its server-assigned
        // content_hash + updated_at, required by the embedding upsert.
        let recs = store
            .fetch_capability_capsules_by_ids(&f.tenant, &[stored_id.as_str()])
            .await
            .expect("fetch by id");
        let rec = recs
            .into_iter()
            .find(|r| r.capability_capsule_id == stored_id)
            .expect("stored record present");

        let content_vec = geometry.raw(&cap.content);
        store
            .upsert_capability_capsule_embedding_chunks(
                &stored_id,
                &f.tenant,
                "geometry-bench",
                geometry.dim() as i64,
                std::slice::from_ref(&content_vec),
                &rec.content_hash,
                &rec.updated_at,
                &rec.updated_at,
            )
            .await
            .expect("embedding upsert");

        id_map.insert(cap.id.clone(), stored_id);
    }

    IngestedFixture {
        store,
        geometry,
        tenant: f.tenant.clone(),
        id_map,
    }
}

/// Translate fixture-id qrels into a set of stored ids for `query_id`.
fn translate_qrels(
    f: &Fixture,
    id_map: &HashMap<String, String>,
    query_id: &str,
) -> HashSet<String> {
    f.qrels
        .get(query_id)
        .map(|set| {
            set.iter()
                .filter_map(|fid| id_map.get(fid).cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Run a single ablation rung over the ingested fixture and return its
/// query-averaged metrics. Task 3 implements `Rung::Hybrid`; every other
/// variant currently shares the hybrid composition.
async fn run_rung(ing: &IngestedFixture, f: &Fixture, rung: Rung) -> RungReport {
    let mut ndcg_sum = 0.0;
    let mut mrr_sum = 0.0;
    let mut recall_sum = 0.0;
    let mut precision_sum = 0.0;
    let n = f.queries.len().max(1) as f64;

    for q in &f.queries {
        // Designed-geometry query vector (same embedding fn as content).
        let query_vec = ing.geometry.raw(&q.text);

        let pool = ing
            .store
            .search_candidates(&ing.tenant)
            .await
            .expect("search_candidates");
        let hybrid_hits = ing
            .store
            .hybrid_candidates(&ing.tenant, &q.text, &query_vec, HYBRID_K)
            .await
            .expect("hybrid_candidates");

        let request = SearchCapabilityCapsuleRequest {
            query: q.text.clone(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 4096,
            caller_agent: "bench".into(),
            // Task 3: hybrid baseline only — no graph expansion yet.
            expand_graph: false,
            tenant: Some(ing.tenant.clone()),
        };

        let graph: &dyn GraphStore = ing.store.as_ref();
        let ranked = rank_with_hybrid_and_graph(pool, hybrid_hits, &request, graph, None)
            .await
            .expect("rank_with_hybrid_and_graph");

        let run: Vec<String> = ranked
            .iter()
            .map(|r| r.capability_capsule_id.clone())
            .collect();
        let qrels = translate_qrels(f, &ing.id_map, &q.id);

        ndcg_sum += ndcg_at_k(&run, &qrels, METRIC_K);
        mrr_sum += mrr(&run, &qrels);
        recall_sum += recall_at_k(&run, &qrels, METRIC_K);
        precision_sum += precision_at_k(&run, &qrels, METRIC_K);
    }

    RungReport {
        rung,
        ndcg_at_10: ndcg_sum / n,
        mrr: mrr_sum / n,
        recall_at_10: recall_sum / n,
        precision_at_10: precision_sum / n,
    }
}

/// Ingest `f` once into a fresh `Store`, then run each requested rung,
/// returning the collected per-rung metric reports.
pub async fn run_bench(f: &Fixture, rungs: &[Rung]) -> BenchReport {
    let ingested = ingest_fixture(f).await;
    let mut reports = Vec::with_capacity(rungs.len());
    for &rung in rungs {
        reports.push(run_rung(&ingested, f, rung).await);
    }
    BenchReport { reports }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn hybrid_rung_recalls_topic_capsules() {
        let f = crate::bench::synthetic::generate(&crate::bench::synthetic::SyntheticConfig {
            num_long: 0,
            ..Default::default()
        });
        let report = run_bench(&f, &[Rung::Hybrid]).await;
        let r = report
            .reports
            .iter()
            .find(|r| r.rung == Rung::Hybrid)
            .unwrap();
        assert!(
            r.recall_at_10 > 0.5,
            "hybrid recall@10 too low: {}",
            r.recall_at_10
        );
    }
}
