//! Bench runner: ingest a `Fixture` into a fresh on-disk `Store`, attach
//! designed-geometry embeddings, and run ablation "rungs" through the real
//! public ranker (`rank_with_hybrid_and_graph`). Each rung reports the
//! standard IR metrics averaged over the fixture's queries.
//!
//! Every **non-Oracle** rung is a pure input variation of the same real ranker
//! — lexical / semantic / hybrid arms, graph expansion (K10) and edge dynamics
//! (K9) toggles, and the chunking-on/off embedding contrast (③). The Oracle
//! rung re-orders the candidate union by qrels to give the achievable ceiling;
//! it does not call `rank_with_hybrid_and_graph`.
//!
//! **Why Graph (K10) and Dynamics (K9) show Δ≈0 vs Hybrid on the v1 fixture:**
//! Every capsule is ingested with the same `project`/`repo` ("bench"), so all
//! capsules are 1-hop from the same `project`/`repo` entity and graph expansion
//! applies a near-uniform boost that preserves relative ordering — hence K10
//! and K9 show Δ≈0 vs Hybrid. The rungs still execute the real
//! `compute_graph_boosts` / edge-dynamics code path; demonstrating a non-zero
//! K9/K10 delta needs a fixture with graph-bridge capsules that are relevant
//! ONLY via graph reachability (no lexical/semantic match) plus strength-bearing
//! edges — deferred to a v1.1 fixture redesign.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::embedding::EmbeddingProvider;
use mem::pipeline::chunk;
use mem::pipeline::eval_metrics::{mrr, ndcg_at_k, precision_at_k, recall_at_k};
use mem::pipeline::retrieve::{rank_with_hybrid_and_graph, EdgeDynamicsCtx};
use mem::service::CapabilityCapsuleService;
use mem::storage::{GraphStore, Store};
use tempfile::tempdir;

use crate::bench::fixture::{Fixture, QueryFixture};
use crate::bench::geometry::GeometryProvider;

/// Number of hybrid candidates to fan out before ranking.
const HYBRID_K: usize = 48;
/// Cutoff for the rank-position metrics.
const METRIC_K: usize = 10;

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
    /// Keeps the on-disk tempdir alive for the lifetime of the Store; the
    /// Store ATTACHes the lance dir and reads it for the whole bench run, so
    /// the dir must outlive every read. Dropped (and cleaned) with the struct.
    _dir: tempfile::TempDir,
}

/// Ingest every capsule in `f` into a fresh on-disk `Store`, attach
/// designed-geometry embedding(s) for each, and return the live handle plus a
/// fixture-id → stored-id map.
///
/// Geometry provider gets one orthogonal basis per fixture topic; `dim` is
/// padded a little above the topic count so every topic owns an axis.
///
/// `chunking_on` controls how long capsules are embedded (③ long-content
/// recall):
/// - short capsule (`!cap.long`): a single vector for the whole content
///   (identical in both modes).
/// - long, `chunking_on = true`: split with [`chunk::chunk_text`] and embed
///   EVERY chunk → all chunk vectors are upserted, so the tail topic (which
///   sits after ~13.5k chars of filler) gets its own embedding row.
/// - long, `chunking_on = false`: embed ONLY the first window
///   (`chunk_text(...)[0]`) as a single vector — models the embedder
///   silently dropping the tail. The first window holds the head topic but
///   NOT the tail topic, so tail queries miss semantically.
async fn ingest_fixture(f: &Fixture, chunking_on: bool) -> IngestedFixture {
    let topic_refs: Vec<&str> = f.topics.iter().map(|s| s.as_str()).collect();
    let dim = topic_refs.len().max(8) + 8;
    let geometry = GeometryProvider::new(&topic_refs, dim);

    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("recall_bench.duckdb");
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

        // ③ Compute the embedding vector(s) for this capsule.
        let vectors: Vec<Vec<f32>> = if !cap.long {
            // Short capsule: one vector for the whole content (both modes).
            vec![geometry.raw(&cap.content)]
        } else if chunking_on {
            // Long + chunking on: embed EVERY chunk so the tail is recallable.
            chunk::chunk_text(
                &cap.content,
                chunk::DEFAULT_CHUNK_TOKENS,
                chunk::DEFAULT_CHUNK_OVERLAP,
            )
            .iter()
            .map(|c| geometry.raw(c))
            .collect()
        } else {
            // Long + chunking off: only the first window is embedded; the
            // tail topic (past the window) is dropped from semantic search.
            let chunks = chunk::chunk_text(
                &cap.content,
                chunk::DEFAULT_CHUNK_TOKENS,
                chunk::DEFAULT_CHUNK_OVERLAP,
            );
            vec![geometry.raw(&chunks[0])]
        };

        store
            .upsert_capability_capsule_embedding_chunks(
                &stored_id,
                &f.tenant,
                "geometry-bench",
                geometry.dim() as i64,
                &vectors,
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
        _dir: dir,
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
/// query-averaged metrics.
///
/// Each non-Oracle rung drives the *real* ranker as an input variation — no
/// entity-id plumbing, no separate code path:
/// - `LexicalOnly`: BM25 only (empty query vector ⇒ no vector arm).
/// - `SemanticOnly`: vector only (empty query text ⇒ no BM25 arm).
/// - `Hybrid`: text + vector; no graph expansion.
/// - `Graph` (K10): Hybrid + `expand_graph=true` ⇒ exercises the real
///   `compute_graph_boosts` over the capsule→entity edges ingest created.
/// - `Dynamics` (K9): Graph + an [`EdgeDynamicsCtx`] ⇒ exercises the real
///   decayed-strength weighting + co-access enqueue path.
/// - `ChunkingOn` / `ChunkingOff`: semantic-only (vector arm), run against
///   the store ingested in the matching chunking mode. The difference lives
///   in the embeddings, not the ranker (wired in `run_bench`). Semantic-only
///   is deliberate: BM25 already recalls a long capsule's tail lexically
///   (the full content is stored + FTS-indexed verbatim in BOTH modes), so a
///   hybrid composition would mask the very recall gap ③ chunking closes.
///   Isolating the vector arm is what surfaces the tail-drop the chunk
///   module exists to fix.
/// - `Oracle`: re-orders the achievable candidate union so qrels-relevant
///   ids lead; the achievable ceiling for this candidate set.
async fn run_rung(ing: &IngestedFixture, f: &Fixture, rung: Rung) -> RungReport {
    // Keep the potentiation receiver alive for the whole rung so the K9
    // co-access channel never closes mid-run (events are not consumed).
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let dyn_ctx = EdgeDynamicsCtx {
        now: mem::storage::current_timestamp(),
        tx,
    };

    // The chunking rungs measure long-content TAIL recall, so they are
    // scored over the tail-targeted query subset only — that is the
    // population the ③ mechanism affects. Aggregating over the per-topic
    // short queries would dilute (and on this fixture even invert) the
    // signal, since chunking-on adds filler-chunk vectors that only add ANN
    // noise to short-query recall. Every other rung scores over all queries.
    let chunking_rung = matches!(rung, Rung::ChunkingOn | Rung::ChunkingOff);
    let queries: Vec<&QueryFixture> = if chunking_rung {
        f.queries.iter().filter(|q| q.tail_targeted).collect()
    } else {
        f.queries.iter().collect()
    };

    let mut ndcg_sum = 0.0;
    let mut mrr_sum = 0.0;
    let mut recall_sum = 0.0;
    let mut precision_sum = 0.0;
    let n = queries.len().max(1) as f64;

    for q in queries {
        // Designed-geometry query vector (same embedding fn as content).
        let query_vec = ing.geometry.raw(&q.text);

        // Rung-specific arms of the hybrid candidate fan-out.
        let (text_arm, vec_arm): (&str, &[f32]) = match rung {
            Rung::LexicalOnly => (q.text.as_str(), &[]), // empty vector ⇒ BM25 only
            // Vector-only: empty text ⇒ no BM25 arm. The chunking rungs are
            // semantic-only too, so the ③ embedding tail-drop is not masked
            // by BM25's lexical recall of the verbatim content.
            Rung::SemanticOnly | Rung::ChunkingOn | Rung::ChunkingOff => ("", query_vec.as_slice()),
            _ => (q.text.as_str(), query_vec.as_slice()),
        };

        // Graph expansion on for the K10/K9 rungs only.
        let expand_graph = matches!(rung, Rung::Graph | Rung::Dynamics);
        // Edge dynamics (K9) only for the Dynamics rung.
        let dynamics = matches!(rung, Rung::Dynamics).then_some(&dyn_ctx);

        let pool = ing
            .store
            .search_candidates(&ing.tenant)
            .await
            .expect("search_candidates");
        let hybrid_hits = ing
            .store
            .hybrid_candidates(&ing.tenant, text_arm, vec_arm, HYBRID_K)
            .await
            .expect("hybrid_candidates");

        let qrels = translate_qrels(f, &ing.id_map, &q.id);

        let run: Vec<String> = if rung == Rung::Oracle {
            // Achievable ceiling: union of the hybrid hits ++ pool ids
            // (deduped, first-occurrence order), stable-sorted so qrels-
            // relevant ids lead. Metrics are computed on that ordering.
            let mut seen: HashSet<String> = HashSet::new();
            let mut union: Vec<String> = Vec::new();
            for (m, _) in &hybrid_hits {
                if seen.insert(m.capability_capsule_id.clone()) {
                    union.push(m.capability_capsule_id.clone());
                }
            }
            for m in &pool {
                if seen.insert(m.capability_capsule_id.clone()) {
                    union.push(m.capability_capsule_id.clone());
                }
            }
            // Stable partition: relevant ids first, preserving relative order.
            union.sort_by_key(|id| !qrels.contains(id));
            union
        } else {
            // For vector-only rungs the request query mirrors the empty
            // text arm, so the lifecycle `text_match_score` contributes 0 —
            // otherwise its lexical "content contains term" bonus would
            // re-introduce the tail recall that ③ chunking is meant to
            // isolate, masking the on-vs-off gap.
            let request = SearchCapabilityCapsuleRequest {
                query: text_arm.to_string(),
                intent: "debugging".into(),
                scope_filters: vec![],
                token_budget: 4096,
                caller_agent: "bench".into(),
                expand_graph,
                tenant: Some(ing.tenant.clone()),
            };
            let graph: &dyn GraphStore = ing.store.as_ref();
            let ranked = rank_with_hybrid_and_graph(pool, hybrid_hits, &request, graph, dynamics)
                .await
                .expect("rank_with_hybrid_and_graph");
            ranked
                .iter()
                .map(|r| r.capability_capsule_id.clone())
                .collect()
        };

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

/// Ingest `f` and run each requested rung, returning the per-rung reports.
///
/// All rungs share one chunking-on store (`on`) except `ChunkingOff`, which
/// needs its own store ingested with the tail-dropping embeddings — so a
/// second store (`off`) is built only when that rung is requested. The
/// chunking on/off difference lives entirely in the embeddings; the ranker
/// composition is identical, so the contrast isolates the ③ recall effect.
pub async fn run_bench(f: &Fixture, rungs: &[Rung]) -> BenchReport {
    let on = ingest_fixture(f, true).await;
    let off = if rungs.contains(&Rung::ChunkingOff) {
        Some(ingest_fixture(f, false).await)
    } else {
        None
    };

    let mut reports = Vec::with_capacity(rungs.len());
    for &rung in rungs {
        let ing = if rung == Rung::ChunkingOff {
            off.as_ref()
                .expect("off store built when ChunkingOff requested")
        } else {
            &on
        };
        reports.push(run_rung(ing, f, rung).await);
    }
    BenchReport { reports }
}

fn rung_name(r: Rung) -> &'static str {
    match r {
        Rung::LexicalOnly => "LexicalOnly",
        Rung::SemanticOnly => "SemanticOnly",
        Rung::Hybrid => "Hybrid",
        Rung::Graph => "Graph",
        Rung::Dynamics => "Dynamics",
        Rung::ChunkingOn => "ChunkingOn",
        Rung::ChunkingOff => "ChunkingOff",
        Rung::Oracle => "Oracle",
    }
}

pub fn pretty_table(report: &BenchReport) -> String {
    let baseline = report
        .reports
        .iter()
        .find(|r| r.rung == Rung::Hybrid)
        .map(|r| r.ndcg_at_10);
    let mut out = String::from("rung          ndcg@10  mrr    recall@10  prec@10   Δndcg\n");
    for r in &report.reports {
        let delta = match baseline {
            Some(b) => format!("{:+.3}", r.ndcg_at_10 - b),
            None => String::new(),
        };
        out.push_str(&format!(
            "{:<13} {:.3}    {:.3}  {:.3}      {:.3}     {}\n",
            rung_name(r.rung),
            r.ndcg_at_10,
            r.mrr,
            r.recall_at_10,
            r.precision_at_10,
            delta
        ));
    }
    out
}

pub fn write_json(report: &BenchReport, path: &std::path::Path) -> std::io::Result<()> {
    let mut s = String::from("{\"rungs\":[");
    for (i, r) in report.reports.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"rung\":\"{}\",\"ndcg_at_10\":{},\"mrr\":{},\"recall_at_10\":{},\"precision_at_10\":{}}}",
            rung_name(r.rung),
            r.ndcg_at_10,
            r.mrr,
            r.recall_at_10,
            r.precision_at_10
        ));
    }
    s.push_str("]}");
    std::fs::write(path, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_table_has_all_rungs_and_delta() {
        let report = BenchReport {
            reports: vec![
                RungReport {
                    rung: Rung::Hybrid,
                    ndcg_at_10: 0.5,
                    mrr: 0.5,
                    recall_at_10: 0.5,
                    precision_at_10: 0.5,
                },
                RungReport {
                    rung: Rung::Graph,
                    ndcg_at_10: 0.6,
                    mrr: 0.5,
                    recall_at_10: 0.6,
                    precision_at_10: 0.5,
                },
            ],
        };
        let t = pretty_table(&report);
        assert!(t.contains("Hybrid"));
        assert!(t.contains("Graph"));
        assert!(
            t.contains("+0.100"),
            "expected Δndcg vs hybrid baseline in:\n{t}"
        );
    }

    #[test]
    fn write_json_is_wellformed() {
        let report = BenchReport {
            reports: vec![RungReport {
                rung: Rung::Hybrid,
                ndcg_at_10: 0.5,
                mrr: 0.5,
                recall_at_10: 0.5,
                precision_at_10: 0.5,
            }],
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.json");
        write_json(&report, &path).unwrap();
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.contains("\"ndcg_at_10\""));
        assert!(s.contains("\"Hybrid\""));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "ablation bench — run with --ignored"]
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

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "ablation bench — run with --ignored"]
    async fn chunking_on_beats_off_for_tail_queries() {
        let f =
            crate::bench::synthetic::generate(&crate::bench::synthetic::SyntheticConfig::default());
        let rep = run_bench(&f, &[Rung::ChunkingOn, Rung::ChunkingOff]).await;
        let on = rep
            .reports
            .iter()
            .find(|r| r.rung == Rung::ChunkingOn)
            .unwrap();
        let off = rep
            .reports
            .iter()
            .find(|r| r.rung == Rung::ChunkingOff)
            .unwrap();
        eprintln!(
            "③ chunking tail recall@10: on={:.4} (ndcg {:.4}) off={:.4} (ndcg {:.4})",
            on.recall_at_10, on.ndcg_at_10, off.recall_at_10, off.ndcg_at_10
        );
        assert!(
            on.recall_at_10 > off.recall_at_10,
            "③ on {} must beat off {}",
            on.recall_at_10,
            off.recall_at_10
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "ablation bench — run with --ignored"]
    async fn oracle_is_an_upper_bound() {
        let f =
            crate::bench::synthetic::generate(&crate::bench::synthetic::SyntheticConfig::default());
        let rep = run_bench(&f, &[Rung::Hybrid, Rung::Oracle]).await;
        let hybrid = rep.reports.iter().find(|r| r.rung == Rung::Hybrid).unwrap();
        let oracle = rep.reports.iter().find(|r| r.rung == Rung::Oracle).unwrap();
        eprintln!(
            "oracle ndcg@10: hybrid={:.4} oracle={:.4}",
            hybrid.ndcg_at_10, oracle.ndcg_at_10
        );
        assert!(
            oracle.ndcg_at_10 >= hybrid.ndcg_at_10 - 1e-9,
            "oracle must dominate hybrid"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "ablation bench — run with --ignored"]
    async fn graph_and_dynamics_rungs_execute_and_are_finite() {
        let f =
            crate::bench::synthetic::generate(&crate::bench::synthetic::SyntheticConfig::default());
        let rep = run_bench(&f, &[Rung::Hybrid, Rung::Graph, Rung::Dynamics]).await;
        let hybrid = rep.reports.iter().find(|r| r.rung == Rung::Hybrid).unwrap();
        for rg in [Rung::Graph, Rung::Dynamics] {
            let r = rep.reports.iter().find(|r| r.rung == rg).unwrap();
            eprintln!(
                "{:?} ndcg@10={:.4} (hybrid={:.4})",
                rg, r.ndcg_at_10, hybrid.ndcg_at_10
            );
            assert!(
                r.ndcg_at_10.is_finite() && r.ndcg_at_10 >= 0.0,
                "{:?} ndcg not finite",
                rg
            );
            // Graph expansion must not DEGRADE ranking on this fixture.
            assert!(
                r.ndcg_at_10 >= hybrid.ndcg_at_10 - 1e-6,
                "{:?} degraded vs hybrid",
                rg
            );
        }
    }
}
