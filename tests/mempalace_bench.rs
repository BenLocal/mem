//! O6c — LongMemEval parity bench (closes oss-memory-diff O6c).
//!
//! Produces an externally-comparable PUBLIC number for mem's retrieval quality
//! on LongMemEval — the benchmark Zep / agentmemory / Cognee report against.
//!
//! IMPORTANT — what this measures, and what it does NOT:
//! * Metric = **session-level memory recall@k of the evidence sessions**
//!   (`answer_session_ids`). This is one of LongMemEval's own official metrics
//!   ("session-level memory recall accuracy"), computed here with
//!   `pipeline::eval_metrics`. It is directly comparable to agentmemory's
//!   reported recall@5.
//! * It is **NOT** end-to-end QA accuracy (the number Zep reports as 63.8% with
//!   GPT-4o). That requires a QA model + an LLM judge; this environment has no
//!   reachable LLM gateway, so QA accuracy is out of scope here. Do not compare
//!   this recall number against a QA-accuracy number — they are different axes.
//!
//! Dataset: the real `longmemeval_s_cleaned.json` (official HuggingFace release,
//! 500 questions, ~40 haystack sessions each) when present at
//! `tests/mempalace_bench/data/longmemeval_s_cleaned.json` (gitignored — download
//! with:
//!   curl -L --proxy "$HTTPS_PROXY" -o tests/mempalace_bench/data/longmemeval_s_cleaned.json \
//!     https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json
//! ). When that file is absent (fresh checkout / no network) the bench falls back
//! to the bundled `subset.json` — a tiny FORMAT-FAITHFUL but SYNTHETIC set, whose
//! number is illustrative only and is labelled as such in the output.
//!
//! Because the embedding model is a local 0.6B Qwen3 on CPU (~1-2s/batch),
//! embedding all 500×40 sessions is hours of compute. The bench therefore runs a
//! deterministic, type-stratified SAMPLE of `LONGMEMEVAL_SAMPLE` questions
//! (default 50; set to 0 for the full set). The sample size is printed and must
//! be carried into any public quote.
//!
//! Run: `cargo test --test mempalace_bench -- --ignored --nocapture`
//! `#[ignore]` + not in CI (real model, minutes-to-hours of local inference).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use mem::config::EmbeddingSettings;
use mem::domain::capability_capsule::{
    CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
};
use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::embedding::{EmbedAnythingEmbeddingProvider, EmbeddingProvider};
use mem::pipeline::eval_metrics::{mrr, recall_any_at_k, recall_at_k};
use mem::pipeline::retrieve::rank_with_hybrid_and_graph;
use mem::service::CapabilityCapsuleService;
use mem::storage::{GraphStore, Store};
use serde::Deserialize;
use tempfile::tempdir;

const TENANT: &str = "lme";
/// Embedding batch size (matches the live embedding worker default).
const EMBED_BATCH: usize = 8;
/// The six LongMemEval question types (abstention `_abs` excluded from recall).
const TYPES: [&str; 6] = [
    "single-session-user",
    "single-session-assistant",
    "single-session-preference",
    "temporal-reasoning",
    "knowledge-update",
    "multi-session",
];

#[derive(Deserialize)]
struct Turn {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct LmeItem {
    question_id: String,
    #[serde(default)]
    question_type: String,
    question: String,
    haystack_session_ids: Vec<String>,
    haystack_sessions: Vec<Vec<Turn>>,
    #[serde(default)]
    answer_session_ids: Vec<String>,
}

#[derive(Default, Clone)]
struct Acc {
    n: f64,
    r1: f64,
    r5: f64,
    r10: f64,
    any1: f64,
    any5: f64,
    any10: f64,
    mrr: f64,
}

impl Acc {
    fn add(&mut self, run: &[String], qrels: &HashSet<String>) {
        self.n += 1.0;
        self.r1 += recall_at_k(run, qrels, 1);
        self.r5 += recall_at_k(run, qrels, 5);
        self.r10 += recall_at_k(run, qrels, 10);
        self.any1 += recall_any_at_k(run, qrels, 1);
        self.any5 += recall_any_at_k(run, qrels, 5);
        self.any10 += recall_any_at_k(run, qrels, 10);
        self.mrr += mrr(run, qrels);
    }
    fn avg(&self) -> (f64, f64, f64, f64, f64, f64, f64) {
        let d = self.n.max(1.0);
        (
            self.r1 / d,
            self.r5 / d,
            self.r10 / d,
            self.any1 / d,
            self.any5 / d,
            self.any10 / d,
            self.mrr / d,
        )
    }
}

fn join_turns(turns: &[Turn]) -> String {
    turns
        .iter()
        .map(|t| format!("{}: {}", t.role, t.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Deterministic, type-stratified sample of up to `n` items (0 = all). Items are
/// bucketed by `question_type` in file order, then pulled round-robin across the
/// six types so every type is represented before any type is over-sampled.
fn stratified_sample(items: Vec<LmeItem>, n: usize) -> Vec<LmeItem> {
    if n == 0 || n >= items.len() {
        return items;
    }
    let mut buckets: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        let key = TYPES
            .iter()
            .copied()
            .find(|t| *t == it.question_type)
            .unwrap_or("other");
        buckets.entry(key).or_default().push(i);
    }
    let mut order: Vec<usize> = Vec::new();
    let mut round = 0;
    while order.len() < n {
        let mut progressed = false;
        for ids in buckets.values() {
            if let Some(&idx) = ids.get(round) {
                order.push(idx);
                progressed = true;
                if order.len() >= n {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
        round += 1;
    }
    order.sort_unstable();
    let keep: HashSet<usize> = order.into_iter().collect();
    items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, it)| it)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LongMemEval parity bench — real model + dataset; run with --ignored"]
async fn longmemeval_session_recall() {
    // ---- dataset: prefer the real official file, else bundled synthetic subset.
    let real_path = std::env::var("LONGMEMEVAL_DATA")
        .unwrap_or_else(|_| "tests/mempalace_bench/data/longmemeval_s_cleaned.json".to_string());
    let (raw, is_real) = match std::fs::read_to_string(&real_path) {
        Ok(s) => (s, true),
        Err(_) => (
            include_str!("mempalace_bench/subset.json").to_string(),
            false,
        ),
    };
    let all: Vec<LmeItem> = serde_json::from_str(&raw).expect("parse longmemeval json");

    // Keep only answerable items with a clean parallel haystack + evidence labels.
    let answerable: Vec<LmeItem> = all
        .into_iter()
        .filter(|it| {
            !it.question_id.ends_with("_abs")
                && !it.answer_session_ids.is_empty()
                && it.haystack_session_ids.len() == it.haystack_sessions.len()
        })
        .collect();

    let sample_n: usize = std::env::var("LONGMEMEVAL_SAMPLE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(50);
    let items = stratified_sample(answerable, sample_n);
    assert!(!items.is_empty(), "no answerable items to evaluate");

    // ---- one embedanything provider (model loads once, shared across questions).
    let settings = EmbeddingSettings::development_defaults();
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(
        EmbedAnythingEmbeddingProvider::from_settings(&settings).expect("embedanything provider"),
    );
    let model = provider.model().to_string();
    let dim = provider.dim();

    eprintln!(
        "\n== LongMemEval session-recall bench ==\nsource={} | questions={} | model={} dim={}",
        if is_real {
            format!("REAL longmemeval_s_cleaned.json ({real_path})")
        } else {
            "BUNDLED synthetic subset (illustrative only)".to_string()
        },
        items.len(),
        model,
        dim,
    );

    let started = Instant::now();
    let mut overall = Acc::default();
    let mut by_type: BTreeMap<String, Acc> = BTreeMap::new();

    for (qi, item) in items.iter().enumerate() {
        // Fresh store per question = clean haystack isolation (each LongMemEval
        // question is scored against its own ~40-session history).
        let dir = tempdir().expect("tempdir");
        let store = Arc::new(
            Store::open(&dir.path().join("lme.duckdb"))
                .await
                .expect("Store::open"),
        );
        let svc = CapabilityCapsuleService::with_providers(
            store.clone(),
            "fake".into(),
            Some(provider.clone()),
        );

        // Ingest one capsule per haystack session; remember uuid -> session id.
        let contents: Vec<String> = item
            .haystack_sessions
            .iter()
            .map(|s| join_turns(s))
            .collect();
        let mut uuid_to_sid: HashMap<String, String> = HashMap::new();
        let mut stored: Vec<(String, String)> = Vec::new(); // (uuid, content)
        for (sid, content) in item.haystack_session_ids.iter().zip(contents.iter()) {
            let resp = svc
                .ingest(IngestCapabilityCapsuleRequest {
                    tenant: TENANT.into(),
                    capability_capsule_type: CapabilityCapsuleType::Implementation,
                    content: content.clone(),
                    summary: None,
                    evidence: vec![],
                    code_refs: vec![],
                    scope: Scope::Repo,
                    visibility: Visibility::Shared,
                    project: Some("lme".into()),
                    repo: Some("lme".into()),
                    module: None,
                    task_type: None,
                    tags: vec![],
                    topics: vec![],
                    source_agent: "bench".into(),
                    idempotency_key: Some(sid.clone()),
                    write_mode: WriteMode::Auto,
                    supersedes_capability_capsule_id: None,
                    expires_at: None,
                })
                .await
                .expect("ingest");
            uuid_to_sid.insert(resp.capability_capsule_id.clone(), sid.clone());
            stored.push((resp.capability_capsule_id, content.clone()));
        }

        // Hydrate content_hash + updated_at for the embedding upsert.
        let ids: Vec<&str> = stored.iter().map(|(id, _)| id.as_str()).collect();
        let recs = store
            .fetch_capability_capsules_by_ids(TENANT, &ids)
            .await
            .expect("fetch by id");
        let rec_by_id: HashMap<String, _> = recs
            .into_iter()
            .map(|r| (r.capability_capsule_id.clone(), r))
            .collect();

        // Batch-embed session contents (3-5x throughput vs per-item) and upsert.
        for batch in stored.chunks(EMBED_BATCH) {
            let texts: Vec<&str> = batch.iter().map(|(_, c)| c.as_str()).collect();
            let embs = provider.embed_batch(&texts).await.expect("embed_batch");
            for ((uuid, _), emb_res) in batch.iter().zip(embs.into_iter()) {
                let emb = emb_res.expect("embed element");
                let rec = rec_by_id.get(uuid).expect("rec present");
                store
                    .upsert_capability_capsule_embedding_chunks(
                        uuid,
                        TENANT,
                        &model,
                        dim as i64,
                        &[emb],
                        &rec.content_hash,
                        &rec.updated_at,
                        &rec.updated_at,
                    )
                    .await
                    .expect("embedding upsert");
            }
        }

        // Retrieve with the real hybrid pipeline.
        let query_vec = provider
            .embed_query(&item.question)
            .await
            .expect("embed query");
        let k = item.haystack_session_ids.len().max(50);
        let pool = store
            .search_candidates(TENANT)
            .await
            .expect("search_candidates");
        let hybrid_hits = store
            .hybrid_candidates(TENANT, &item.question, query_vec.as_slice(), k)
            .await
            .expect("hybrid_candidates");
        let request = SearchCapabilityCapsuleRequest {
            query: item.question.clone(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 8192,
            caller_agent: "bench".into(),
            expand_graph: false,
            tenant: Some(TENANT.into()),
            min_score: Some(0),
        };
        let graph: &dyn GraphStore = store.as_ref();
        let ranked = rank_with_hybrid_and_graph(pool, hybrid_hits, &request, graph, None)
            .await
            .expect("rank_with_hybrid_and_graph");

        // Map ranked capsules back to session ids (one capsule per session).
        let mut run: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for r in &ranked {
            if let Some(sid) = uuid_to_sid.get(&r.capability_capsule_id) {
                if seen.insert(sid.clone()) {
                    run.push(sid.clone());
                }
            }
        }
        let qrels: HashSet<String> = item.answer_session_ids.iter().cloned().collect();

        overall.add(&run, &qrels);
        by_type
            .entry(item.question_type.clone())
            .or_default()
            .add(&run, &qrels);

        if (qi + 1) % 10 == 0 || qi + 1 == items.len() {
            eprintln!(
                "  ...{}/{} ({:.0}s elapsed)",
                qi + 1,
                items.len(),
                started.elapsed().as_secs_f64()
            );
        }
    }

    // ---- report.
    let (r1, r5, r10, a1, a5, a10, m) = overall.avg();
    println!("\n================ LongMemEval session-level memory recall ================");
    println!(
        "dataset   : {}",
        if is_real {
            "REAL longmemeval_s_cleaned.json"
        } else {
            "BUNDLED synthetic subset — ILLUSTRATIVE ONLY, not real LongMemEval"
        }
    );
    println!(
        "questions : {} (type-stratified sample; LONGMEMEVAL_SAMPLE=0 for full 500)",
        overall.n as usize
    );
    println!("metric    : session-level memory recall of evidence sessions (answer_session_ids)");
    println!("            *** retrieval recall — NOT end-to-end QA accuracy (≠ Zep 63.8%) ***");
    println!("pipeline  : mem hybrid (jieba BM25 + {model} ANN + RRF)");
    println!("------------------------------------------------------------------------");
    println!(
        "recall@1={r1:.3}  recall@5={r5:.3}  recall@10={r10:.3}   (fraction of evidence sessions)"
    );
    println!(
        "any@1  ={a1:.3}  any@5  ={a5:.3}  any@10 ={a10:.3}   (>=1 evidence session in top-k)"
    );
    println!("mrr    ={m:.3}");
    println!("--- by question type ---");
    for (t, acc) in &by_type {
        let (_, tr5, _, _, ta5, _, tm) = acc.avg();
        println!(
            "  {t:<28} n={:<3} recall@5={tr5:.3} any@5={ta5:.3} mrr={tm:.3}",
            acc.n as usize
        );
    }

    // README candidate line (honest framing baked in).
    println!("\n--- README candidate line ---");
    if is_real {
        println!(
            "LongMemEval-S **session-level memory recall** (mem hybrid retrieval; recall@5 = **{a5:.3}** \
             of evidence sessions, n={} type-stratified sample) — this is retrieval recall (LongMemEval's \
             session-level metric, comparable to agentmemory's recall@5), NOT end-to-end QA accuracy \
             (≠ Zep's 63.8%, a different/harder axis). Reproduce: `cargo test --test mempalace_bench --ignored`.",
            overall.n as usize
        );
    } else {
        println!(
            "[no public number — real dataset not run] Ran on the bundled synthetic format-faithful subset \
             (recall@5={a5:.3}, n={}); illustrative only. Drop the official longmemeval_s_cleaned.json into \
             tests/mempalace_bench/data/ and re-run for a real, comparable number.",
            overall.n as usize
        );
    }
    println!("========================================================================");

    // Sanity: the bench must actually have scored questions.
    assert!(overall.n > 0.0, "no questions scored");
}
