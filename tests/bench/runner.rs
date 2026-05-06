//! Ablation runner. Loads a Fixture, ingests it into a fresh DuckDB,
//! runs each Rung (config tuple), aggregates RungReport per rung.

use super::fixture::*;
use super::judgment::derive_judgments;
use super::oracle::oracle_rerank;
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::{EmbeddingProvider, FakeEmbeddingProvider};
use mem::pipeline::eval_metrics::*;
use mem::pipeline::transcript_recall::{score_candidates, ScoringOpts};
use mem::storage::{DuckDbRepository, VectorIndex};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const TOP_K: usize = 20;
const EMBED_DIM: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceMix {
    Bm25Only,
    HnswOnly,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankPolicy {
    None,
    OracleByJudgment,
}

#[derive(Debug, Clone, Copy)]
pub struct Rung {
    pub name: &'static str,
    pub source: SourceMix,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
    pub rerank: RerankPolicy,
}

#[rustfmt::skip]
pub const RUNGS: &[Rung] = &[
    Rung { name: "bm25-only",           source: SourceMix::Bm25Only, disable_session_cooc: true,  disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "hnsw-only",           source: SourceMix::HnswOnly, disable_session_cooc: true,  disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "hybrid-rrf",          source: SourceMix::Both,     disable_session_cooc: true,  disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "+session-cooc",       source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: true,  disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "+anchor",             source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: true,  rerank: RerankPolicy::None },
    Rung { name: "+freshness (full)",   source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: false, rerank: RerankPolicy::None },
    Rung { name: "+oracle-rerank",      source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: false, rerank: RerankPolicy::OracleByJudgment },
    Rung { name: "all-minus-cooc",      source: SourceMix::Both,     disable_session_cooc: true,  disable_anchor: false, disable_freshness: false, rerank: RerankPolicy::None },
    Rung { name: "all-minus-anchor",    source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: true,  disable_freshness: false, rerank: RerankPolicy::None },
    Rung { name: "all-minus-freshness", source: SourceMix::Both,     disable_session_cooc: false, disable_anchor: false, disable_freshness: true,  rerank: RerankPolicy::None },
];

#[derive(Debug, Clone, Default)]
pub struct RungReport {
    pub name: String,
    pub ndcg_at_5: f64,
    pub ndcg_at_10: f64,
    pub ndcg_at_20: f64,
    pub mrr: f64,
    pub recall_at_10: f64,
    pub precision_at_10: f64,
    pub per_query: Vec<PerQueryMetrics>,
}

#[derive(Debug, Clone)]
pub struct PerQueryMetrics {
    pub query_id: String,
    pub ndcg_at_10: f64,
    pub mrr: f64,
}

#[derive(Debug, Clone, Default)]
pub struct BenchReport {
    pub fixture_kind: String,
    pub session_count: usize,
    pub query_count: usize,
    pub rungs: Vec<RungReport>,
}

impl BenchReport {
    pub fn rung_by_name(&self, name: &str) -> &RungReport {
        self.rungs
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("rung {name} not in report"))
    }
}

pub async fn run_bench(fixture: Fixture) -> BenchReport {
    let mut report = BenchReport {
        fixture_kind: format!("{:?}", fixture.kind),
        session_count: fixture.sessions.len(),
        query_count: fixture.queries.len(),
        rungs: Vec::with_capacity(RUNGS.len()),
    };

    for rung in RUNGS {
        let rung_report = run_rung(&fixture, rung).await;
        report.rungs.push(rung_report);
    }

    report
}

/// Map "user"/"assistant"/"system" → typed enum. Defaults to `User` on
/// unrecognized strings (synthetic generator only emits user/assistant).
fn parse_role(s: &str) -> MessageRole {
    MessageRole::from_db_str(s).unwrap_or(MessageRole::User)
}

/// Map "text"/"thinking"/etc → typed enum. Defaults to `Text`.
fn parse_block_type(s: &str) -> BlockType {
    BlockType::from_db_str(s).unwrap_or(BlockType::Text)
}

async fn run_rung(fixture: &Fixture, rung: &Rung) -> RungReport {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let repo = DuckDbRepository::open(tmp.path().join("bench.duckdb"))
        .await
        .expect("open repo");
    repo.set_transcript_job_provider("fake");

    // Ingest fixture: messages + embeddings. Keep an in-memory map of all
    // ingested messages so HNSW search results (which return only ids) can be
    // hydrated into `ConversationMessage` records without an extra DB roundtrip.
    let fake = Arc::new(FakeEmbeddingProvider::new("fake", EMBED_DIM));
    let total_blocks: usize = fixture.sessions.iter().map(|s| s.blocks.len()).sum();
    let index = VectorIndex::new_in_memory(EMBED_DIM, "fake", "fake", total_blocks.max(8));
    let mut block_index: HashMap<String, ConversationMessage> = HashMap::new();

    for (s_pos, session) in fixture.sessions.iter().enumerate() {
        for (b_pos, block) in session.blocks.iter().enumerate() {
            let block_type = parse_block_type(&block.block_type);
            let embed_eligible = matches!(block_type, BlockType::Text | BlockType::Thinking);
            let msg = ConversationMessage {
                message_block_id: block.block_id.clone(),
                session_id: Some(session.session_id.clone()),
                tenant: fixture.tenant.clone(),
                caller_agent: "bench".to_string(),
                transcript_path: format!("/tmp/bench-{}.jsonl", session.session_id),
                line_number: (b_pos as u64) + 1,
                block_index: s_pos as u32, // arbitrary; unique across (path,line,block) triple
                message_uuid: None,
                role: parse_role(&block.role),
                block_type,
                content: block.content.clone(),
                tool_name: None,
                tool_use_id: None,
                embed_eligible,
                created_at: block.created_at.clone(),
            };
            repo.create_conversation_message(&msg)
                .await
                .expect("create message");
            if embed_eligible {
                let v = fake.embed_text(&msg.content).await.expect("embed");
                index
                    .upsert(&msg.message_block_id, &v)
                    .await
                    .expect("upsert embedding");
            }
            block_index.insert(msg.message_block_id.clone(), msg);
        }
    }

    // Derive judgments once per rung. (Cheap for synthetic; cached against
    // shared repo state per rung — not amortized across rungs deliberately so
    // each rung is a clean snapshot.)
    let now = "00000000020260503999";
    let judgments = derive_judgments(fixture, &repo, now).await;

    // For each query, retrieve + score + (optional) rerank + eval.
    let mut per_query: Vec<PerQueryMetrics> = Vec::with_capacity(fixture.queries.len());
    let mut sum = MetricSum::default();

    for query in &fixture.queries {
        let qrels = judgments.get(&query.query_id).cloned().unwrap_or_default();

        let run_session_ids =
            retrieve_and_rank(&repo, &fake, &index, &block_index, fixture, query, rung).await;

        let final_run = match rung.rerank {
            RerankPolicy::None => run_session_ids.clone(),
            RerankPolicy::OracleByJudgment => oracle_rerank(run_session_ids.clone(), &qrels),
        };

        let ndcg5 = ndcg_at_k(&final_run, &qrels, 5);
        let ndcg10 = ndcg_at_k(&final_run, &qrels, 10);
        let ndcg20 = ndcg_at_k(&final_run, &qrels, 20);
        let mrr_val = mrr(&final_run, &qrels);
        let r10 = recall_at_k(&final_run, &qrels, 10);
        let p10 = precision_at_k(&final_run, &qrels, 10);

        sum.add(ndcg5, ndcg10, ndcg20, mrr_val, r10, p10);
        per_query.push(PerQueryMetrics {
            query_id: query.query_id.clone(),
            ndcg_at_10: ndcg10,
            mrr: mrr_val,
        });
    }

    let n = fixture.queries.len() as f64;
    RungReport {
        name: rung.name.to_string(),
        ndcg_at_5: sum.ndcg5 / n,
        ndcg_at_10: sum.ndcg10 / n,
        ndcg_at_20: sum.ndcg20 / n,
        mrr: sum.mrr / n,
        recall_at_10: sum.recall10 / n,
        precision_at_10: sum.precision10 / n,
        per_query,
    }
}

// Bench-vs-production divergence (intentional v1):
// Production TranscriptService::search injects up to `oversample` block ids
// from the anchor_session_id directly into the candidate pool. The bench
// currently does NOT replicate this injection — synthetic fixtures don't set
// anchor_session_id, so the +anchor / all-minus-anchor rungs measure only
// the ScoringOpts::anchor bonus, not the candidate-injection effect. If real
// fixtures with anchored queries become common, this divergence should be
// closed (mirror src/service/transcript_service.rs:178-187).
#[allow(clippy::too_many_arguments)]
async fn retrieve_and_rank(
    repo: &DuckDbRepository,
    fake: &Arc<FakeEmbeddingProvider>,
    index: &VectorIndex,
    block_index: &HashMap<String, ConversationMessage>,
    fixture: &Fixture,
    query: &QueryFixture,
    rung: &Rung,
) -> Vec<SessionId> {
    // Step 1: Get candidates per source mix.
    let oversample: usize = 50;
    let bm25: Vec<ConversationMessage> = match rung.source {
        SourceMix::Bm25Only | SourceMix::Both => repo
            .bm25_transcript_candidates(&fixture.tenant, &query.text, oversample)
            .await
            .unwrap_or_default(),
        SourceMix::HnswOnly => vec![],
    };

    // HNSW: search returns Vec<(id, similarity)>; hydrate via in-memory block_index.
    let hnsw: Vec<ConversationMessage> = match rung.source {
        SourceMix::HnswOnly | SourceMix::Both => {
            let qv = fake.embed_text(&query.text).await.expect("embed query");
            let hits = index.search(&qv, oversample).await.unwrap_or_default();
            hits.into_iter()
                .filter_map(|(id, _sim)| block_index.get(&id).cloned())
                .collect()
        }
        SourceMix::Bm25Only => vec![],
    };

    // Step 2: Build rank maps (rank starts at 1) keyed on message_block_id.
    let mut lex_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in bm25.iter().enumerate() {
        lex_ranks.insert(m.message_block_id.clone(), i + 1);
    }
    let mut sem_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in hnsw.iter().enumerate() {
        sem_ranks.insert(m.message_block_id.clone(), i + 1);
    }

    // Step 3: Union of candidates, deduplicated by message_block_id.
    let mut by_id: HashMap<String, ConversationMessage> = HashMap::new();
    for m in bm25.into_iter().chain(hnsw) {
        by_id.entry(m.message_block_id.clone()).or_insert(m);
    }
    let candidates: Vec<ConversationMessage> = by_id.into_values().collect();

    // Step 4: Score via production pipeline.
    let opts = ScoringOpts {
        anchor_session_id: query.anchor_session_id.as_deref(),
        disable_session_cooc: rung.disable_session_cooc,
        disable_anchor: rung.disable_anchor,
        disable_freshness: rung.disable_freshness,
    };
    let scored = score_candidates(candidates, &lex_ranks, &sem_ranks, opts);

    // Step 5: Project to session-level ranking. Since `scored` is sorted by
    // score descending, the first occurrence of each session_id is its highest-
    // scoring block. Take TOP_K unique sessions.
    let mut session_seen: HashSet<String> = HashSet::new();
    let mut run: Vec<SessionId> = Vec::with_capacity(TOP_K);
    for sb in scored {
        if let Some(sid) = sb.message.session_id.clone() {
            if session_seen.insert(sid.clone()) {
                run.push(sid);
                if run.len() >= TOP_K {
                    break;
                }
            }
        }
    }
    run
}

#[derive(Default)]
struct MetricSum {
    ndcg5: f64,
    ndcg10: f64,
    ndcg20: f64,
    mrr: f64,
    recall10: f64,
    precision10: f64,
}

impl MetricSum {
    fn add(&mut self, n5: f64, n10: f64, n20: f64, m: f64, r10: f64, p10: f64) {
        self.ndcg5 += n5;
        self.ndcg10 += n10;
        self.ndcg20 += n20;
        self.mrr += m;
        self.recall10 += r10;
        self.precision10 += p10;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::synthetic::{generate, SyntheticConfig};

    #[tokio::test(flavor = "multi_thread")]
    async fn run_bench_on_tiny_synthetic_returns_10_rungs() {
        let cfg = SyntheticConfig {
            session_count: 5,
            blocks_per_session: 4,
            query_count: 6,
            ..SyntheticConfig::default()
        };
        let fixture = generate(&cfg);
        let report = run_bench(fixture).await;
        assert_eq!(report.rungs.len(), 10);
        assert_eq!(report.rungs[0].name, "bm25-only");
        // Oracle must be ≥ full stack on every metric.
        let full = report.rung_by_name("+freshness (full)");
        let oracle = report.rung_by_name("+oracle-rerank");
        assert!(
            oracle.ndcg_at_10 >= full.ndcg_at_10,
            "oracle ({}) must ≥ full stack ({})",
            oracle.ndcg_at_10,
            full.ndcg_at_10
        );
    }
}

use std::fmt::Write as _;
use std::path::Path;

pub fn pretty_table(report: &BenchReport) -> String {
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "=== Recall Bench ({}, {} sessions × {} queries) ===",
        report.fixture_kind, report.session_count, report.query_count
    );
    let _ = writeln!(
        &mut out,
        "{:<22}  NDCG@5  NDCG@10 NDCG@20  MRR    R@10   P@10",
        ""
    );
    let baseline = report
        .rungs
        .iter()
        .find(|r| r.name == "+freshness (full)")
        .map(|r| r.ndcg_at_10);
    for r in &report.rungs {
        let delta = match (r.name.starts_with("all-minus-"), baseline) {
            (true, Some(b)) => format!("  (Δ {:+.3})", r.ndcg_at_10 - b),
            _ => String::new(),
        };
        let _ = writeln!(
            &mut out,
            "{:<22}  {:.3}   {:.3}   {:.3}   {:.3}  {:.3}  {:.3}{}",
            r.name,
            r.ndcg_at_5,
            r.ndcg_at_10,
            r.ndcg_at_20,
            r.mrr,
            r.recall_at_10,
            r.precision_at_10,
            delta
        );
    }
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "⚠ Bias notice: judgments derived from co-mention + entity aliases."
    );
    let _ = writeln!(
        &mut out,
        "  HNSW absolute scores under-counted; relative deltas reliable."
    );
    let _ = writeln!(
        &mut out,
        "  See docs/superpowers/specs/2026-05-03-transcript-recall-bench-design.md §3."
    );
    out
}

pub fn write_json(report: &BenchReport, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::json!({
        "fixture_meta": {
            "kind": report.fixture_kind,
            "session_count": report.session_count,
            "query_count": report.query_count,
        },
        "rungs": report.rungs.iter().map(|r| {
            serde_json::json!({
                "name": r.name,
                "ndcg_at_5": r.ndcg_at_5,
                "ndcg_at_10": r.ndcg_at_10,
                "ndcg_at_20": r.ndcg_at_20,
                "mrr": r.mrr,
                "recall_at_10": r.recall_at_10,
                "precision_at_10": r.precision_at_10,
                "per_query": r.per_query.iter().map(|q| {
                    serde_json::json!({
                        "query_id": q.query_id,
                        "ndcg_at_10": q.ndcg_at_10,
                        "mrr": q.mrr,
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    });
    std::fs::write(path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

#[cfg(test)]
mod output_tests {
    use super::*;

    fn fixture_report() -> BenchReport {
        BenchReport {
            fixture_kind: "Synthetic { seed: 42 }".to_string(),
            session_count: 30,
            query_count: 24,
            rungs: vec![
                RungReport {
                    name: "bm25-only".to_string(),
                    ndcg_at_5: 0.612,
                    ndcg_at_10: 0.658,
                    ndcg_at_20: 0.701,
                    mrr: 0.721,
                    recall_at_10: 0.583,
                    precision_at_10: 0.290,
                    per_query: vec![],
                },
                RungReport {
                    name: "+freshness (full)".to_string(),
                    ndcg_at_5: 0.741,
                    ndcg_at_10: 0.782,
                    ndcg_at_20: 0.815,
                    mrr: 0.844,
                    recall_at_10: 0.697,
                    precision_at_10: 0.358,
                    per_query: vec![],
                },
                RungReport {
                    name: "all-minus-cooc".to_string(),
                    ndcg_at_5: 0.735,
                    ndcg_at_10: 0.776,
                    ndcg_at_20: 0.811,
                    mrr: 0.838,
                    recall_at_10: 0.692,
                    precision_at_10: 0.355,
                    per_query: vec![],
                },
            ],
        }
    }

    #[test]
    fn pretty_table_contains_header_and_bias_notice() {
        let s = pretty_table(&fixture_report());
        assert!(s.contains("=== Recall Bench (Synthetic"));
        assert!(s.contains("bm25-only"));
        assert!(s.contains("Bias notice"));
        assert!(s.contains("co-mention"));
    }

    #[test]
    fn pretty_table_emits_delta_for_leave_one_out_rungs() {
        let s = pretty_table(&fixture_report());
        // all-minus-cooc Δ = 0.776 - 0.782 = -0.006
        assert!(
            s.contains("(Δ -0.006)"),
            "expected leave-one-out delta in output: {s}"
        );
    }

    #[test]
    fn write_json_produces_well_formed_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("out").join("recall.json");
        write_json(&fixture_report(), &path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["fixture_meta"]["session_count"], 30);
        assert_eq!(parsed["rungs"].as_array().unwrap().len(), 3);
        assert_eq!(parsed["rungs"][0]["name"], "bm25-only");
    }
}
