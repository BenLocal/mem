//! H3 — LoCoMo parity bench (oss-memory-diff §9 H3).
//!
//! The track's self-reported baseline moved from LongMemEval-only to
//! LongMemEval + LoCoMo (mem0 / MemOS / Zep all quote LoCoMo);
//! `tests/mempalace_bench.rs` covers the former, this file covers the
//! latter with the SAME metric discipline:
//!
//! * Metric = **session-level memory recall of evidence sessions** —
//!   each LoCoMo QA labels the dialog turns (`evidence: ["D3:5", …]`)
//!   that answer it; we map those to their sessions and score the
//!   ranked session list. It is retrieval recall, **NOT** the
//!   LLM-judged QA accuracy mem0/Zep headline for LoCoMo — do not
//!   compare the two axes.
//! * Category 5 (adversarial / unanswerable) is excluded, like
//!   LongMemEval's `_abs` questions. Categories: 1 multi-hop,
//!   2 temporal, 3 open-domain, 4 single-hop.
//!
//! Dataset: the real `locomo10.json` (official snap-research release,
//! 10 conversations × ~20-35 sessions, ~1500 QA) when present at
//! `tests/locomo_bench/data/locomo10.json` (gitignored — download with:
//!   curl -L --proxy "$HTTPS_PROXY" -o tests/locomo_bench/data/locomo10.json \
//!     https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json
//! ). When absent, falls back to the bundled `subset.json` — a tiny
//! FORMAT-FAITHFUL but SYNTHETIC set whose number is illustrative only
//! and labelled as such in the output.
//!
//! Unlike LongMemEval (fresh store per question), LoCoMo QAs share
//! their conversation's haystack — so the bench builds ONE store per
//! conversation and runs that conversation's sampled QAs against it.
//! `LOCOMO_SAMPLE` caps the QA count (default 50, category-stratified,
//! 0 = full set); only conversations with ≥1 sampled QA are embedded.
//!
//! Run: `cargo test --release --test locomo_bench -- --ignored --nocapture`
//! `#[ignore]` + not in CI (real model, minutes of local inference).

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
use mem::storage::{EmbeddingVectorStore, GraphStore, Store};
use serde::Deserialize;
use tempfile::tempdir;

const TENANT: &str = "locomo";
/// Embedding batch size (matches the live embedding worker default).
const EMBED_BATCH: usize = 8;
/// LoCoMo answerable categories (5 = adversarial is excluded).
const CATEGORIES: [u32; 4] = [1, 2, 3, 4];

fn category_name(c: u32) -> &'static str {
    match c {
        1 => "multi-hop",
        2 => "temporal",
        3 => "open-domain",
        4 => "single-hop",
        _ => "other",
    }
}

#[derive(Deserialize)]
struct LocomoItem {
    #[serde(default)]
    sample_id: String,
    conversation: serde_json::Value,
    qa: Vec<LocomoQa>,
}

#[derive(Deserialize, Clone)]
struct LocomoQa {
    question: String,
    #[serde(default)]
    evidence: Vec<serde_json::Value>,
    #[serde(default)]
    category: u32,
}

/// One parsed session: (`session_<n>` key, rendered text).
struct Session {
    key: String,
    text: String,
}

/// Pull `session_<n>` arrays (plus their `_date_time` stamp when
/// present) out of the dynamic conversation object, rendered as
/// "speaker: text" lines — the same shape `mempalace_bench` embeds.
fn parse_sessions(conversation: &serde_json::Value) -> Vec<Session> {
    let Some(obj) = conversation.as_object() else {
        return Vec::new();
    };
    let mut numbered: Vec<(u64, &Vec<serde_json::Value>)> = Vec::new();
    for (key, value) in obj {
        let Some(n) = key
            .strip_prefix("session_")
            .and_then(|rest| rest.parse::<u64>().ok())
        else {
            continue;
        };
        if let Some(turns) = value.as_array() {
            if !turns.is_empty() {
                numbered.push((n, turns));
            }
        }
    }
    numbered.sort_by_key(|(n, _)| *n);
    numbered
        .into_iter()
        .map(|(n, turns)| {
            let date = obj
                .get(&format!("session_{n}_date_time"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut lines = Vec::new();
            if !date.is_empty() {
                lines.push(format!("date: {date}"));
            }
            for t in turns {
                let speaker = t.get("speaker").and_then(|v| v.as_str()).unwrap_or("?");
                // Image-only turns carry a caption instead of text.
                let text = t
                    .get("text")
                    .and_then(|v| v.as_str())
                    .or_else(|| t.get("blip_caption").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if !text.is_empty() {
                    lines.push(format!("{speaker}: {text}"));
                }
            }
            Session {
                key: format!("session_{n}"),
                text: lines.join("\n"),
            }
        })
        .filter(|s| !s.text.is_empty())
        .collect()
}

/// Evidence codes look like `"D3:5"` (dialog 5 of session 3) — the
/// session-level qrel is `session_3`. Non-string / unparseable entries
/// are dropped.
fn evidence_sessions(evidence: &[serde_json::Value]) -> HashSet<String> {
    let mut out = HashSet::new();
    for ev in evidence {
        let Some(code) = ev.as_str() else { continue };
        let Some(rest) = code.trim().strip_prefix('D') else {
            continue;
        };
        let Some((session, _turn)) = rest.split_once(':') else {
            continue;
        };
        if session.chars().all(|c| c.is_ascii_digit()) && !session.is_empty() {
            out.insert(format!("session_{session}"));
        }
    }
    out
}

#[derive(Default)]
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

/// Deterministic sample of up to `n` QA indices (0 = all), stratified
/// by category AND spread across conversations: each category bucket
/// is ordered "first QA of every conversation before the second of
/// any" (occurrence-within-conversation, then conversation index), and
/// buckets are drained round-robin. Plain file-order buckets (the
/// `mempalace_bench` shape) would let the first conversations' QAs
/// fill the whole sample — a 50-QA sample covering 2 of 10
/// conversations is a much narrower measurement than it looks.
fn stratified_qa_sample(qas: &[(usize, LocomoQa)], n: usize) -> Vec<usize> {
    if n == 0 || n >= qas.len() {
        return (0..qas.len()).collect();
    }
    // Per (category, conversation) occurrence counters give each QA a
    // "how many of my conversation's QAs precede me in this bucket"
    // rank; sorting by (rank, conversation) interleaves conversations.
    let mut occ: HashMap<(u32, usize), usize> = HashMap::new();
    let mut buckets: BTreeMap<u32, Vec<(usize, usize, usize)>> = BTreeMap::new(); // (occ, conv, idx)
    for (i, (conv, qa)) in qas.iter().enumerate() {
        let slot = occ.entry((qa.category, *conv)).or_insert(0);
        buckets
            .entry(qa.category)
            .or_default()
            .push((*slot, *conv, i));
        *slot += 1;
    }
    for ids in buckets.values_mut() {
        ids.sort_unstable();
    }
    let mut order: Vec<usize> = Vec::new();
    let mut round = 0;
    while order.len() < n {
        let mut progressed = false;
        for ids in buckets.values() {
            if let Some(&(_, _, idx)) = ids.get(round) {
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
    order
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "LoCoMo parity bench — real model + dataset; run with --ignored"]
async fn locomo_session_recall() {
    // ---- dataset: prefer the real official file, else bundled synthetic subset.
    let real_path = std::env::var("LOCOMO_DATA")
        .unwrap_or_else(|_| "tests/locomo_bench/data/locomo10.json".to_string());
    let (raw, is_real) = match std::fs::read_to_string(&real_path) {
        Ok(s) => (s, true),
        Err(_) => (include_str!("locomo_bench/subset.json").to_string(), false),
    };
    let conversations: Vec<LocomoItem> = serde_json::from_str(&raw).expect("parse locomo json");

    // Flatten answerable QAs (categories 1-4, with usable evidence) as
    // (conversation index, qa), preserving file order for sampling.
    let mut answerable: Vec<(usize, LocomoQa)> = Vec::new();
    for (ci, item) in conversations.iter().enumerate() {
        for qa in &item.qa {
            if CATEGORIES.contains(&qa.category) && !evidence_sessions(&qa.evidence).is_empty() {
                answerable.push((ci, qa.clone()));
            }
        }
    }
    let sample_n: usize = std::env::var("LOCOMO_SAMPLE")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(50);
    let keep = stratified_qa_sample(&answerable, sample_n);
    let mut by_conv: BTreeMap<usize, Vec<LocomoQa>> = BTreeMap::new();
    for idx in keep {
        let (ci, qa) = &answerable[idx];
        by_conv.entry(*ci).or_default().push(qa.clone());
    }
    assert!(!by_conv.is_empty(), "no answerable QAs to evaluate");

    let graph_on = std::env::var("LOCOMO_GRAPH").is_ok_and(|v| v == "1");

    // ---- one embedanything provider (model loads once, shared).
    let settings = EmbeddingSettings::development_defaults();
    let provider: Arc<dyn EmbeddingProvider> = Arc::new(
        EmbedAnythingEmbeddingProvider::from_settings(&settings).expect("embedanything provider"),
    );
    let model = provider.model().to_string();
    let dim = provider.dim();

    let total_qas: usize = by_conv.values().map(Vec::len).sum();
    eprintln!(
        "\n== LoCoMo session-recall bench ==\nsource={} | conversations={} | QAs={} | model={} dim={}",
        if is_real {
            format!("REAL locomo10.json ({real_path})")
        } else {
            "BUNDLED synthetic subset (illustrative only)".to_string()
        },
        by_conv.len(),
        total_qas,
        model,
        dim,
    );

    let started = Instant::now();
    let mut overall = Acc::default();
    let mut by_category: BTreeMap<u32, Acc> = BTreeMap::new();
    let mut scored = 0usize;

    for (ci, qas) in &by_conv {
        let item = &conversations[*ci];
        let sessions = parse_sessions(&item.conversation);
        if sessions.is_empty() {
            eprintln!("  conversation {ci} has no sessions — skipped");
            continue;
        }

        // One store per conversation: its QAs share the haystack.
        let dir = tempdir().expect("tempdir");
        let store = Arc::new(
            Store::open(&dir.path().join("locomo.lance"))
                .await
                .expect("Store::open"),
        );
        let svc = CapabilityCapsuleService::with_providers(
            store.clone(),
            "fake".into(),
            Some(provider.clone()),
        );

        // Ingest one capsule per session; remember uuid -> session key.
        let mut uuid_to_key: HashMap<String, String> = HashMap::new();
        let mut stored: Vec<(String, String)> = Vec::new();
        for s in &sessions {
            let resp = svc
                .ingest(IngestCapabilityCapsuleRequest {
                    tenant: TENANT.into(),
                    capability_capsule_type: CapabilityCapsuleType::Implementation,
                    content: s.text.clone(),
                    summary: None,
                    evidence: vec![],
                    code_refs: vec![],
                    scope: Scope::Repo,
                    visibility: Visibility::Shared,
                    project: Some("locomo".into()),
                    repo: Some("locomo".into()),
                    module: None,
                    task_type: None,
                    tags: vec![],
                    topics: vec![],
                    source_agent: "bench".into(),
                    idempotency_key: Some(format!("{}:{}", item.sample_id, s.key)),
                    write_mode: WriteMode::Auto,
                    supersedes_capability_capsule_id: None,
                    expires_at: None,
                })
                .await
                .expect("ingest");
            uuid_to_key.insert(resp.capability_capsule_id.clone(), s.key.clone());
            stored.push((resp.capability_capsule_id, s.text.clone()));
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

        // G2 evaluation posture (opt-in via LOCOMO_GRAPH=1): build the
        // production-shaped link graph (H1 `related_to`, band
        // [0.80, 0.92), top-4 — the exact ingest-link lane) so
        // `expand_graph` has capsule↔capsule edges to walk. Measured
        // trade-off on this corpus (n=50): +8pt open-domain any@5 and
        // +4pt any@10, but -15pt multi-hop any@5 (top-5 crowding) — so
        // the DEFAULT headline posture keeps the graph channel off.
        if graph_on {
            for (uuid, _) in &stored {
                if let Some(vec) = store
                    .get_capability_capsule_embedding_vector(uuid)
                    .await
                    .expect("vector read")
                {
                    mem::worker::embedding_worker::link_related_neighbors(
                        store.as_ref(),
                        TENANT,
                        uuid,
                        &vec,
                        0.80,
                        0.92,
                    )
                    .await
                    .expect("ingest link");
                }
            }
        }

        // Score this conversation's sampled QAs against the shared store.
        for qa in qas {
            let query_vec = provider
                .embed_query(&qa.question)
                .await
                .expect("embed query");
            let k = sessions.len().max(50);
            let pool = store
                .search_candidates(TENANT)
                .await
                .expect("search_candidates");
            let hybrid_hits = store
                .hybrid_candidates(TENANT, &qa.question, query_vec.as_slice(), k)
                .await
                .expect("hybrid_candidates");
            let request = SearchCapabilityCapsuleRequest {
                query: qa.question.clone(),
                intent: "debugging".into(),
                scope_filters: vec![],
                token_budget: 8192,
                caller_agent: "bench".into(),
                expand_graph: graph_on,
                tenant: Some(TENANT.into()),
                min_score: Some(0),
            };
            let graph: &dyn GraphStore = store.as_ref();
            let ranked = rank_with_hybrid_and_graph(
                pool,
                hybrid_hits,
                &request,
                graph,
                None,
                Some(store.as_ref() as &dyn mem::storage::CapsuleStore),
            )
            .await
            .expect("rank_with_hybrid_and_graph");

            let mut run: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            for r in &ranked {
                if let Some(key) = uuid_to_key.get(&r.capability_capsule_id) {
                    if seen.insert(key.clone()) {
                        run.push(key.clone());
                    }
                }
            }
            let qrels = evidence_sessions(&qa.evidence);
            overall.add(&run, &qrels);
            by_category
                .entry(qa.category)
                .or_default()
                .add(&run, &qrels);
            scored += 1;
            if scored.is_multiple_of(10) || scored == total_qas {
                eprintln!(
                    "  ...{scored}/{total_qas} ({:.0}s elapsed)",
                    started.elapsed().as_secs_f64()
                );
            }
        }
    }

    // ---- report.
    let (r1, r5, r10, a1, a5, a10, m) = overall.avg();
    println!("\n================ LoCoMo session-level memory recall ================");
    println!(
        "dataset   : {}",
        if is_real {
            "REAL locomo10.json"
        } else {
            "BUNDLED synthetic subset — ILLUSTRATIVE ONLY, not real LoCoMo"
        }
    );
    println!(
        "QAs       : {} (category-stratified sample; LOCOMO_SAMPLE=0 for full set; category 5 adversarial excluded)",
        overall.n as usize
    );
    println!("metric    : session-level memory recall of evidence sessions");
    println!("            *** retrieval recall — NOT LLM-judged QA accuracy (mem0/Zep LoCoMo headline) ***");
    println!(
        "pipeline  : mem hybrid (jieba BM25 + {model} ANN + RRF){}",
        if graph_on {
            " + G2 graph channel (H1 related_to links, expand_graph) [LOCOMO_GRAPH=1]"
        } else {
            ""
        }
    );
    println!("---------------------------------------------------------------------");
    println!(
        "recall@1={r1:.3}  recall@5={r5:.3}  recall@10={r10:.3}   (fraction of evidence sessions)"
    );
    println!(
        "any@1  ={a1:.3}  any@5  ={a5:.3}  any@10 ={a10:.3}   (>=1 evidence session in top-k)"
    );
    println!("mrr    ={m:.3}");
    println!("--- by category ---");
    for (c, acc) in &by_category {
        let (_, cr5, _, _, ca5, _, cm) = acc.avg();
        println!(
            "  {} ({:<11}) n={:<4} recall@5={cr5:.3} any@5={ca5:.3} mrr={cm:.3}",
            c,
            category_name(*c),
            acc.n as usize
        );
    }

    println!("\n--- README candidate line ---");
    if is_real {
        println!(
            "LoCoMo **session-level memory recall** (mem hybrid retrieval; recall@5 = **{a5:.3}** of \
             evidence sessions, n={} category-stratified QA sample, adversarial excluded) — retrieval \
             recall, NOT the LLM-judged LoCoMo QA accuracy mem0/Zep quote (a different/harder axis). \
             Reproduce: `cargo test --release --test locomo_bench -- --ignored`.",
            overall.n as usize
        );
    } else {
        println!(
            "[no public number — real dataset not run] Ran on the bundled synthetic format-faithful \
             subset (recall@5={a5:.3}, n={}); illustrative only. Drop the official locomo10.json into \
             tests/locomo_bench/data/ and re-run for a real, comparable number.",
            overall.n as usize
        );
    }
    println!("=====================================================================");

    assert!(overall.n > 0.0, "no QAs scored");
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn evidence_codes_map_to_session_keys() {
        let ev = vec![
            serde_json::json!("D3:5"),
            serde_json::json!("D3:9"),
            serde_json::json!("D12:1"),
            serde_json::json!("garbage"),
            serde_json::json!(42),
        ];
        let got = evidence_sessions(&ev);
        let want: HashSet<String> = ["session_3", "session_12"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn sessions_parse_in_order_with_dates_and_captions() {
        let conv = serde_json::json!({
            "speaker_a": "Ann",
            "speaker_b": "Ben",
            "session_2": [
                {"speaker": "Ann", "dia_id": "D2:1", "text": "hello again"},
                {"speaker": "Ben", "dia_id": "D2:2", "blip_caption": "a photo of a bridge"}
            ],
            "session_2_date_time": "2:00 pm on 2 May, 2023",
            "session_1": [
                {"speaker": "Ann", "dia_id": "D1:1", "text": "first words"}
            ],
            "session_10": [],
            "session_summary": {"ignored": true}
        });
        let sessions = parse_sessions(&conv);
        assert_eq!(sessions.len(), 2, "empty + non-array keys are dropped");
        assert_eq!(sessions[0].key, "session_1");
        assert!(sessions[0].text.contains("Ann: first words"));
        assert_eq!(sessions[1].key, "session_2");
        assert!(sessions[1].text.contains("date: 2:00 pm on 2 May, 2023"));
        assert!(
            sessions[1].text.contains("Ben: a photo of a bridge"),
            "image turns fall back to the caption"
        );
    }

    #[test]
    fn stratified_sample_spreads_across_conversations() {
        // 5 conversations × 4 QAs of category 1 each; a sample of 5
        // must touch EVERY conversation, not drain conversation 0.
        let qa = || LocomoQa {
            question: "q".into(),
            evidence: vec![serde_json::json!("D1:1")],
            category: 1,
        };
        let mut qas: Vec<(usize, LocomoQa)> = Vec::new();
        for conv in 0..5 {
            for _ in 0..4 {
                qas.push((conv, qa()));
            }
        }
        let keep = stratified_qa_sample(&qas, 5);
        let convs: HashSet<usize> = keep.iter().map(|&i| qas[i].0).collect();
        assert_eq!(
            convs.len(),
            5,
            "sample must cover all 5 conversations: {convs:?}"
        );
    }

    #[test]
    fn stratified_sample_covers_every_category_first() {
        let qa = |cat: u32| LocomoQa {
            question: "q".into(),
            evidence: vec![serde_json::json!("D1:1")],
            category: cat,
        };
        // 6 QAs of cat 1, one each of 2/3/4 — a sample of 4 must take
        // one from EVERY category before doubling up on cat 1.
        let mut qas: Vec<(usize, LocomoQa)> = (0..6).map(|_| (0, qa(1))).collect();
        qas.push((0, qa(2)));
        qas.push((0, qa(3)));
        qas.push((0, qa(4)));
        let keep = stratified_qa_sample(&qas, 4);
        let cats: Vec<u32> = keep.iter().map(|&i| qas[i].1.category).collect();
        for c in [1, 2, 3, 4] {
            assert!(cats.contains(&c), "category {c} missing from {cats:?}");
        }
    }
}
