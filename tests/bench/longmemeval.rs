//! LongMemEval bench runner. Per-question ingest + 3-rung re-rank.
//! See docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md.

use super::longmemeval_dataset::*;
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::{EmbeddingProvider, FakeEmbeddingProvider};
use mem::storage::{DuckDbRepository, VectorIndex};
use std::sync::Arc;

const TENANT: &str = "bench";

/// Ingest one LongMemEval question's haystack into the given repo + index.
/// Each (session, turn) pair becomes one ConversationMessage; each
/// embed-eligible message gets embedded + upserted into the VectorIndex.
pub async fn ingest_corpus(
    repo: &DuckDbRepository,
    index: &VectorIndex,
    embedder: &Arc<dyn EmbeddingProvider>,
    question: &LongMemEvalQuestion,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut count = 0usize;
    for session in &question.haystack_sessions {
        let session_started_ms = parse_started_at_to_ms(session.started_at.as_deref())
            .unwrap_or_else(|| stable_session_seed_ms(&session.session_id));
        for (turn_idx, turn) in session.turns.iter().enumerate() {
            let block_id = format!(
                "{}_{}_{}",
                question.question_id, session.session_id, turn_idx
            );
            let role = parse_role(&turn.role);
            let content = turn.content.clone();
            let created_ms = session_started_ms + (turn_idx as u64) * 60_000;
            let msg = ConversationMessage {
                message_block_id: block_id.clone(),
                tenant: TENANT.to_string(),
                session_id: Some(session.session_id.clone()),
                role,
                block_type: BlockType::Text,
                content: content.clone(),
                embed_eligible: true,
                created_at: format!("{:020}", created_ms),
                // Bench defaults for fields not used by ranking:
                caller_agent: "bench".to_string(),
                transcript_path: format!("/tmp/lme/{}.jsonl", question.question_id),
                message_uuid: Some(block_id.clone()),
                tool_use_id: None,
                tool_name: None,
                line_number: 0,
                block_index: turn_idx as u32,
            };
            repo.create_conversation_message(&msg).await?;
            let v = embedder.embed_text(&content).await?;
            index.upsert(&block_id, &v).await?;
            count += 1;
        }
    }
    Ok(count)
}

fn parse_role(s: &str) -> MessageRole {
    MessageRole::from_db_str(s).unwrap_or(MessageRole::User)
}

/// Parse an ISO-8601 timestamp like "2024-03-15T00:00:00" into a naive
/// monotonic millisecond seed. NOT a real Unix epoch — leap years and
/// month-length variation are ignored. Sufficient for the bench's
/// ordering needs (turns within a session are monotonic, sessions
/// across the corpus span ~90 days). Returns None on parse failure.
fn parse_started_at_to_ms(s: Option<&str>) -> Option<u64> {
    let s = s?;
    // Best-effort: extract YYYY-MM-DD prefix and convert to ms.
    let date = s.get(..10)?;
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let d: u64 = parts[2].parse().ok()?;
    // Naive: treat year/month/day as a unique offset (not a real epoch).
    Some(((y - 1970) * 365 + m * 30 + d) * 86_400_000)
}

/// Stable per-session millisecond seed (used when started_at is missing).
/// Hash the session_id to a u64 in a deterministic way.
fn stable_session_seed_ms(session_id: &str) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037; // FNV-1a basis
    for b in session_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    1_700_000_000_000 + (h % (90 * 86_400_000))
}

use super::runner::SourceMix;
use mem::pipeline::transcript_recall::{score_candidates, ScoringOpts};
use std::collections::{HashMap, HashSet};

const TOP_K_CANDIDATES: usize = 50;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // rung_id + mempalace_label used by Task 8 bench harness
pub struct LongMemEvalRung {
    pub rung_id: &'static str,
    pub mempalace_label: &'static str,
    pub source: SourceMix,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
}

#[rustfmt::skip]
pub const RUNGS: &[LongMemEvalRung] = &[
    LongMemEvalRung {
        rung_id: "longmemeval_raw",
        mempalace_label: "raw",
        source: SourceMix::HnswOnly,
        disable_session_cooc: true,
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_rooms",
        mempalace_label: "rooms",
        source: SourceMix::HnswOnly,
        disable_session_cooc: false,
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_full",
        mempalace_label: "full",
        source: SourceMix::Both,
        disable_session_cooc: false,
        disable_anchor: false,
        disable_freshness: false,
    },
];

/// Retrieve and rank under the given rung's config; project to session-level
/// top-K. Caller supplies an already-populated repo + vector index for the
/// current question's corpus.
pub async fn retrieve_for_rung(
    repo: &DuckDbRepository,
    index: &VectorIndex,
    embedder: &Arc<dyn EmbeddingProvider>,
    query_text: &str,
    rung: &LongMemEvalRung,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // 1. Get candidates per source mix.
    let bm25 = match rung.source {
        SourceMix::Bm25Only | SourceMix::Both => repo
            .bm25_transcript_candidates(TENANT, query_text, TOP_K_CANDIDATES)
            .await
            .unwrap_or_default(),
        SourceMix::HnswOnly => vec![],
    };
    let hnsw_ids: Vec<(String, f32)> = match rung.source {
        SourceMix::HnswOnly | SourceMix::Both => {
            let qv = embedder.embed_text(query_text).await?;
            index
                .search(&qv, TOP_K_CANDIDATES)
                .await
                .unwrap_or_default()
        }
        SourceMix::Bm25Only => vec![],
    };

    // 2. Build rank maps (rank starts at 1).
    let mut lex_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in bm25.iter().enumerate() {
        lex_ranks.insert(m.message_block_id.clone(), i + 1);
    }
    let mut sem_ranks: HashMap<String, usize> = HashMap::new();
    for (i, (id, _)) in hnsw_ids.iter().enumerate() {
        sem_ranks.insert(id.clone(), i + 1);
    }

    // 3. Hydrate HNSW candidates back to ConversationMessage. BM25 already
    // returned full records; for HNSW-only ids, fetch from repo by id.
    let mut by_id: HashMap<String, mem::domain::ConversationMessage> = HashMap::new();
    for m in bm25.into_iter() {
        by_id.entry(m.message_block_id.clone()).or_insert(m);
    }
    for (id, _) in &hnsw_ids {
        if !by_id.contains_key(id) {
            if let Ok(Some(m)) = repo.get_conversation_message_by_id(TENANT, id).await {
                by_id.insert(id.clone(), m);
            }
        }
    }
    let candidates: Vec<mem::domain::ConversationMessage> = by_id.into_values().collect();

    // 4. Score via production pipeline.
    let opts = ScoringOpts {
        anchor_session_id: None, // LongMemEval has no anchor concept
        disable_session_cooc: rung.disable_session_cooc,
        disable_anchor: rung.disable_anchor,
        disable_freshness: rung.disable_freshness,
    };
    let scored = score_candidates(candidates, &lex_ranks, &sem_ranks, opts);

    // 5. Project to session-level top-K (highest-score block per session).
    let mut session_seen: HashSet<String> = HashSet::new();
    let mut run: Vec<String> = Vec::with_capacity(20);
    for sb in scored {
        if let Some(sid) = sb.message.session_id.clone() {
            if session_seen.insert(sid.clone()) {
                run.push(sid);
                if run.len() >= 20 {
                    break;
                }
            }
        }
    }
    Ok(run)
}

use mem::config::{Config, EmbeddingProviderKind};
use mem::embedding::arc_embedding_provider;
use mem::pipeline::eval_metrics::{ndcg_at_k, recall_all_at_k, recall_any_at_k};
use std::time::Instant;

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by Task 7 (pretty table + JSON output)
pub struct PerQuestionMetrics {
    pub question_id: String,
    pub recall_any_at_5: f64,
    pub recall_any_at_10: f64,
    pub recall_all_at_5: f64,
    pub recall_all_at_10: f64,
    pub ndcg_at_10: f64,
    pub ranked_session_ids: Vec<String>,
    pub answer_session_ids: Vec<String>,
    pub elapsed_ms: u128,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by Task 7 (pretty table + JSON output)
pub struct RungReport {
    pub rung_id: String,
    pub mempalace_label: String,
    pub aggregate_recall_any_at_5: f64,
    pub aggregate_recall_any_at_10: f64,
    pub aggregate_recall_all_at_5: f64,
    pub aggregate_recall_all_at_10: f64,
    pub aggregate_ndcg_at_10: f64,
    pub per_question: Vec<PerQuestionMetrics>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by Task 7 (pretty table + JSON output)
pub struct BenchReport {
    pub system_version: String,
    pub embedding_model: String,
    pub timestamp_ms: u128,
    pub limit: usize,
    pub rungs: Vec<RungReport>,
}

/// Run the full LongMemEval bench across the given questions.
/// For each question: ingest once, retrieve under each of the 3 rungs,
/// score against the gold answer_session_ids, aggregate into RungReports.
pub async fn run_longmemeval_bench(
    questions: Vec<LongMemEvalQuestion>,
) -> Result<BenchReport, Box<dyn std::error::Error>> {
    // Build the production embedding provider from env-var config.
    let cfg = Config::from_env()?;
    let embedder: Arc<dyn EmbeddingProvider> = arc_embedding_provider(&cfg.embedding)?;
    let embedding_dim = cfg.embedding.dim;
    let embedding_model = cfg.embedding.model.clone();
    let provider_str = cfg.embedding.job_provider_id();
    if matches!(cfg.embedding.provider, EmbeddingProviderKind::Fake) {
        eprintln!(
            "WARNING: EMBEDDING_PROVIDER=fake — bench numbers will be \
             meaningless for cross-system comparison. Set \
             EMBEDDING_PROVIDER=embedanything (or similar) before running."
        );
    }

    let mut per_rung_metrics: Vec<Vec<PerQuestionMetrics>> = vec![vec![]; RUNGS.len()];
    let total_qs = questions.len();
    eprintln!(
        "[bench] running LongMemEval over {} questions x 3 rungs",
        total_qs
    );

    for (q_idx, question) in questions.iter().enumerate() {
        if q_idx % 25 == 0 {
            eprintln!("[bench] progress: {}/{}", q_idx, total_qs);
        }
        let q_start = Instant::now();

        // Per-question fresh DB + index. Ingest once.
        let tmp = tempfile::TempDir::new()?;
        let repo = DuckDbRepository::open(&tmp.path().join("bench.duckdb")).await?;
        repo.set_transcript_job_provider(provider_str);
        let total_blocks: usize = question
            .haystack_sessions
            .iter()
            .map(|s| s.turns.len())
            .sum();
        let index = VectorIndex::new_in_memory(
            embedding_dim,
            "bench",
            &embedding_model,
            total_blocks.max(8),
        );
        ingest_corpus(&repo, &index, &embedder, question).await?;

        // Re-rank under each rung.
        let qrels: HashSet<String> = question.answer_session_ids.iter().cloned().collect();
        for (rung_idx, rung) in RUNGS.iter().enumerate() {
            let run = retrieve_for_rung(&repo, &index, &embedder, &question.question, rung).await?;
            let elapsed_ms = q_start.elapsed().as_millis();
            let metrics = PerQuestionMetrics {
                question_id: question.question_id.clone(),
                recall_any_at_5: recall_any_at_k(&run, &qrels, 5),
                recall_any_at_10: recall_any_at_k(&run, &qrels, 10),
                recall_all_at_5: recall_all_at_k(&run, &qrels, 5),
                recall_all_at_10: recall_all_at_k(&run, &qrels, 10),
                ndcg_at_10: ndcg_at_k(&run, &qrels, 10),
                ranked_session_ids: run.clone(),
                answer_session_ids: question.answer_session_ids.clone(),
                elapsed_ms,
            };
            per_rung_metrics[rung_idx].push(metrics);
        }
    }

    let mut rung_reports: Vec<RungReport> = Vec::with_capacity(RUNGS.len());
    for (rung_idx, rung) in RUNGS.iter().enumerate() {
        let pqs = &per_rung_metrics[rung_idx];
        let n = pqs.len() as f64;
        let mean = |sel: fn(&PerQuestionMetrics) -> f64| -> f64 {
            if n == 0.0 {
                0.0
            } else {
                pqs.iter().map(sel).sum::<f64>() / n
            }
        };
        rung_reports.push(RungReport {
            rung_id: rung.rung_id.to_string(),
            mempalace_label: rung.mempalace_label.to_string(),
            aggregate_recall_any_at_5: mean(|p| p.recall_any_at_5),
            aggregate_recall_any_at_10: mean(|p| p.recall_any_at_10),
            aggregate_recall_all_at_5: mean(|p| p.recall_all_at_5),
            aggregate_recall_all_at_10: mean(|p| p.recall_all_at_10),
            aggregate_ndcg_at_10: mean(|p| p.ndcg_at_10),
            per_question: pqs.clone(),
        });
    }

    Ok(BenchReport {
        system_version: git_short_sha().unwrap_or_else(|| "unknown".to_string()),
        embedding_model,
        timestamp_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        limit: total_qs,
        rungs: rung_reports,
    })
}

/// Best-effort git short SHA via `git rev-parse`. Returns None if git
/// isn't available or this isn't a repo.
fn git_short_sha() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const MEMPALACE_BASELINES: &[(&str, &str)] = &[
    ("longmemeval_raw", "raw    = 0.966 R@5"),
    ("longmemeval_rooms", "rooms  = 0.894 R@5"),
    ("longmemeval_full", "full   = (per README)"),
];

pub fn print_comparison_table(report: &BenchReport) {
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "=== Mem vs MemPalace LongMemEval ({} questions, run {}) ===",
        report.limit, report.timestamp_ms
    );
    let _ = writeln!(
        &mut out,
        "                    R@5(any) R@10(any) NDCG@10  | mempalace baseline"
    );
    for r in &report.rungs {
        let baseline = MEMPALACE_BASELINES
            .iter()
            .find(|(id, _)| *id == r.rung_id)
            .map(|(_, b)| *b)
            .unwrap_or("(no baseline)");
        let _ = writeln!(
            &mut out,
            "{:<19}   {:.3}     {:.3}    {:.3}  | mempalace {}",
            r.rung_id,
            r.aggregate_recall_any_at_5,
            r.aggregate_recall_any_at_10,
            r.aggregate_ndcg_at_10,
            baseline
        );
    }
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "! Embedding-model parity caveat: mem uses {} while mempalace",
        report.embedding_model
    );
    let _ = writeln!(
        &mut out,
        "  uses all-MiniLM-L6-v2 (384-dim). The Δ between rungs IS reliable;"
    );
    let _ = writeln!(
        &mut out,
        "  absolute Δ vs mempalace baselines includes both ranking and"
    );
    let _ = writeln!(&mut out, "  embedding-model contributions.");
    print!("{}", out);
}

pub fn write_per_rung_json(
    report: &BenchReport,
    out_dir: &Path,
) -> Result<Vec<PathBuf>, std::io::Error> {
    std::fs::create_dir_all(out_dir)?;
    let mut paths = Vec::with_capacity(report.rungs.len());
    let ts = report.timestamp_ms;
    for r in &report.rungs {
        let filename = format!("results_mem_{}_{}.json", r.rung_id, ts);
        let path = out_dir.join(&filename);
        let payload = serde_json::json!({
            "benchmark": "longmemeval",
            "mode": r.mempalace_label,
            "system": "mem",
            "embedding_model": report.embedding_model,
            "system_version": report.system_version,
            "timestamp_ms": report.timestamp_ms,
            "limit": report.limit,
            "aggregate": {
                "recall_any_at_5": r.aggregate_recall_any_at_5,
                "recall_any_at_10": r.aggregate_recall_any_at_10,
                "recall_all_at_5": r.aggregate_recall_all_at_5,
                "recall_all_at_10": r.aggregate_recall_all_at_10,
                "ndcg_at_10": r.aggregate_ndcg_at_10,
            },
            "per_question": r.per_question.iter().map(|p| serde_json::json!({
                "question_id": p.question_id,
                "recall_any_at_5": p.recall_any_at_5,
                "recall_any_at_10": p.recall_any_at_10,
                "recall_all_at_5": p.recall_all_at_5,
                "recall_all_at_10": p.recall_all_at_10,
                "ndcg_at_10": p.ndcg_at_10,
                "ranked_session_ids": p.ranked_session_ids,
                "answer_session_ids": p.answer_session_ids,
                "elapsed_ms": p.elapsed_ms,
            })).collect::<Vec<_>>(),
        });
        std::fs::write(&path, serde_json::to_string_pretty(&payload)?)?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod output_tests {
    use super::*;

    fn fixture_report() -> BenchReport {
        BenchReport {
            system_version: "abcd1234".to_string(),
            embedding_model: "Qwen3-test".to_string(),
            timestamp_ms: 1730000000000,
            limit: 50,
            rungs: vec![RungReport {
                rung_id: "longmemeval_raw".to_string(),
                mempalace_label: "raw".to_string(),
                aggregate_recall_any_at_5: 0.876,
                aggregate_recall_any_at_10: 0.912,
                aggregate_recall_all_at_5: 0.500,
                aggregate_recall_all_at_10: 0.640,
                aggregate_ndcg_at_10: 0.821,
                per_question: vec![PerQuestionMetrics {
                    question_id: "lme_q_0001".to_string(),
                    recall_any_at_5: 1.0,
                    recall_any_at_10: 1.0,
                    recall_all_at_5: 0.0,
                    recall_all_at_10: 1.0,
                    ndcg_at_10: 0.85,
                    ranked_session_ids: vec!["s1".into(), "s2".into()],
                    answer_session_ids: vec!["s2".into()],
                    elapsed_ms: 320,
                }],
            }],
        }
    }

    #[test]
    fn print_comparison_table_does_not_panic() {
        let report = fixture_report();
        // Smoke-test the print path; the function writes to stdout so we
        // can't easily capture, but at minimum it should not panic.
        print_comparison_table(&report);
    }

    #[test]
    fn write_per_rung_json_creates_one_file_per_rung() {
        let report = fixture_report();
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = write_per_rung_json(&report, tmp.path()).unwrap();
        assert_eq!(paths.len(), 1);
        let bytes = std::fs::read(&paths[0]).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed["benchmark"], "longmemeval");
        assert_eq!(parsed["mode"], "raw");
        assert_eq!(parsed["system"], "mem");
        assert_eq!(parsed["aggregate"]["recall_any_at_5"], 0.876);
        assert_eq!(parsed["per_question"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["per_question"][0]["question_id"], "lme_q_0001");
    }

    #[test]
    fn write_per_rung_json_filename_has_mem_prefix() {
        let report = fixture_report();
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = write_per_rung_json(&report, tmp.path()).unwrap();
        let filename = paths[0].file_name().unwrap().to_string_lossy().to_string();
        assert!(
            filename.starts_with("results_mem_longmemeval_raw_"),
            "expected results_mem_ prefix, got {filename}"
        );
        assert!(filename.ends_with(".json"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn make_question(
        qid: &str,
        sessions: Vec<(&str, Vec<(&str, &str)>)>,
    ) -> LongMemEvalQuestion {
        LongMemEvalQuestion {
            question_id: qid.to_string(),
            question: format!("question for {}", qid),
            haystack_sessions: sessions
                .into_iter()
                .map(|(sid, turns)| LongMemEvalSession {
                    session_id: sid.to_string(),
                    started_at: Some("2024-03-15T00:00:00".to_string()),
                    turns: turns
                        .into_iter()
                        .map(|(role, content)| LongMemEvalTurn {
                            role: role.to_string(),
                            content: content.to_string(),
                        })
                        .collect(),
                })
                .collect(),
            answer_session_ids: vec![],
            question_date: None,
        }
    }

    #[tokio::test]
    async fn ingest_corpus_creates_message_per_turn_and_embedding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ingest.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 16);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", 64));

        let q = make_question(
            "lme_q_0001",
            vec![
                ("sess_1", vec![("user", "hello world"), ("assistant", "hi")]),
                (
                    "sess_2",
                    vec![("user", "tokio runtime"), ("assistant", "yes")],
                ),
            ],
        );
        let count = ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();
        assert_eq!(count, 4, "4 turns ingested");

        // Sanity check: HNSW finds something for a known query.
        let qv = embedder.embed_text("tokio").await.unwrap();
        let hits: Vec<(String, f32)> = index.search(&qv, 4).await.unwrap();
        assert!(!hits.is_empty(), "HNSW should return ingested blocks");
        let ids: HashSet<String> = hits.into_iter().map(|(id, _)| id).collect();
        assert!(ids.iter().all(|id: &String| id.starts_with("lme_q_0001_")));
    }

    #[tokio::test]
    async fn ingest_corpus_handles_missing_started_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ingest.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 8);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", 64));

        // Session with started_at = None
        let q = LongMemEvalQuestion {
            question_id: "q1".to_string(),
            question: "q".to_string(),
            haystack_sessions: vec![LongMemEvalSession {
                session_id: "s1".to_string(),
                started_at: None,
                turns: vec![LongMemEvalTurn {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
            }],
            answer_session_ids: vec![],
            question_date: None,
        };
        let count = ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn retrieve_for_rung_returns_session_level_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ret.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 16);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", 64));

        let q = make_question(
            "lme_q_test",
            vec![
                ("sess_a", vec![("user", "tokio rust async runtime")]),
                ("sess_b", vec![("user", "duckdb columnar storage")]),
                ("sess_c", vec![("user", "tokio futures")]),
            ],
        );
        ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();

        let rung = RUNGS[0]; // longmemeval_raw
        let run = retrieve_for_rung(&repo, &index, &embedder, "tokio runtime", &rung)
            .await
            .unwrap();
        assert!(!run.is_empty(), "raw rung should return some sessions");
        assert!(
            run.iter()
                .all(|s| ["sess_a", "sess_b", "sess_c"].contains(&s.as_str())),
            "all returned ids should be from the ingested sessions, got {:?}",
            run
        );
        // Session ids are unique (no duplicates from session-level projection).
        let unique: HashSet<&String> = run.iter().collect();
        assert_eq!(unique.len(), run.len());
    }

    #[tokio::test]
    async fn run_longmemeval_bench_returns_3_rungs_for_tiny_input() {
        // Uses production Config::from_env. Set EMBEDDING_PROVIDER=fake for
        // this test to avoid model download. Other env vars must be valid.
        std::env::set_var("EMBEDDING_PROVIDER", "fake");
        std::env::set_var("EMBEDDING_MODEL", "fake");
        std::env::set_var("EMBEDDING_DIM", "64");

        let mut q = make_question(
            "lme_q_smoke",
            vec![
                ("sess_a", vec![("user", "tokio rust async")]),
                ("sess_b", vec![("user", "duckdb columnar")]),
            ],
        );
        q.answer_session_ids = vec!["sess_a".to_string()];
        let report = run_longmemeval_bench(vec![q]).await.unwrap();
        assert_eq!(report.rungs.len(), 3);
        for rung_report in &report.rungs {
            assert_eq!(rung_report.per_question.len(), 1);
        }
        assert_eq!(report.limit, 1);
    }

    #[test]
    fn parse_started_at_to_ms_monotonic_across_consecutive_days() {
        let day1 = super::parse_started_at_to_ms(Some("2024-03-15T00:00:00"));
        let day2 = super::parse_started_at_to_ms(Some("2024-03-16T00:00:00"));
        assert!(day1.is_some(), "day1 should parse");
        assert!(day2.is_some(), "day2 should parse");
        assert!(
            day2.unwrap() > day1.unwrap(),
            "day2 ({:?}) should be strictly greater than day1 ({:?})",
            day2,
            day1
        );

        assert_eq!(
            super::parse_started_at_to_ms(None),
            None,
            "None input should return None"
        );
        assert_eq!(
            super::parse_started_at_to_ms(Some("invalid")),
            None,
            "invalid input should return None"
        );
    }
}
