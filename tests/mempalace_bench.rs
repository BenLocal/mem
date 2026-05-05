//! MemPalace LongMemEval parity bench (closes ROADMAP #15).
//! See docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md.
//!
//! ### Schema reverse-engineering notes (Task 1, 2026-05-05)
//!
//! Inspected mempalace `results_mempal_*.jsonl` sample files from:
//!   results_mempal_raw_session_20260414_1629.jsonl       (254 per-Q records)
//!   results_mempal_hybrid_v4_held_out_session_20260414_1634.jsonl (449 per-Q records)
//!
//! Both files confirmed as pure-JSONL (one JSON object per line, no
//! top-level aggregate line — aggregate is computed externally by the
//! benchmark harness).
//!
//! Per-question record keys observed (top-level):
//!   `question_id`, `question_type`, `question`, `answer`, `retrieval_results`
//!
//! `retrieval_results` keys:
//!   `query`, `ranked_items`, `metrics`
//!
//! `ranked_items[i]` keys:
//!   `corpus_id`, `text`, `timestamp`
//!
//! `metrics` keys:
//!   `session`, `turn`  (each is an object with recall/ndcg at various k)
//!
//! `metrics.session` / `metrics.turn` key naming convention:
//!   **`recall_any@5`** (at-sign, not underscore) and `ndcg_any@5`.
//!   Full set observed: `recall_any@1`, `ndcg_any@1`, `recall_any@3`,
//!   `ndcg_any@3`, `recall_any@5`, `ndcg_any@5`, `recall_any@10`,
//!   `ndcg_any@10`, `recall_any@30`, `ndcg_any@30`, `recall_any@50`,
//!   `ndcg_any@50`. (`turn` omits ndcg keys — only recall_any@k.)
//!
//! For our `results_mem_longmemeval_*.jsonl` we use **snake_case** keys
//! (e.g. `recall_any_at_5`) since they are valid Rust identifiers and
//! play nice with serde. To compare with mempalace `recall_any@5` keys,
//! a `jq` rename suffices:
//!   jq 'with_entries(.key |= sub("_at_"; "@"))'
//!
//! ### LongMemEval question schema (Task 1)
//!
//! Network fetch of the dataset was not performed (dataset is gated /
//! large). Based on probe observation of the retrieval_results corpus_ids
//! (e.g. `sharegpt_PdnvIns_0`, `d414cac5_4`, `answer_280352e9`) the
//! dataset entries are keyed by `question_id` matching these corpus-id
//! prefixes. The probe below prints the full structural shape when
//! `MEM_LONGMEMEVAL_PATH` is set; run it with `--ignored --nocapture` on
//! first access to the real dataset and copy the observed field names into
//! `LongMemEvalQuestion` in Task 3.
//!
//! Assumed top-level shape (per spec):
//!   Array of objects, each with at minimum:
//!     `question_id`  (string)
//!     `question`     (string)
//!     `answer`       (string)
//!     `question_type` (string, e.g. "single-session-user", "temporal-reasoning")
//!     `haystack_sessions` (array of session objects)
//!
//!   Each session object likely has:
//!     `session_id` or similar identifier
//!     `turns`  (array of turn objects with `role`/`content` or similar)

#[tokio::test(flavor = "multi_thread")]
#[ignore = "probe; set MEM_LONGMEMEVAL_PATH=... and run with --ignored"]
async fn longmemeval_format_probe() {
    let path = match std::env::var("MEM_LONGMEMEVAL_PATH") {
        Ok(p) => p,
        Err(_) => {
            eprintln!("MEM_LONGMEMEVAL_PATH not set; skipping probe");
            return;
        }
    };
    let bytes = std::fs::read(&path).expect("read fixture");
    let json: serde_json::Value = serde_json::from_slice(&bytes).expect("parse");

    // Inspect top-level: is it an array of questions, or {questions: [...]}, or other?
    match &json {
        serde_json::Value::Array(arr) => {
            println!("LongMemEval is a top-level array of {} entries", arr.len());
            for (i, entry) in arr.iter().take(5).enumerate() {
                let keys: Vec<_> = entry
                    .as_object()
                    .map(|m| m.keys().cloned().collect())
                    .unwrap_or_default();
                println!("  Q{}: keys = {:?}", i, keys);
                if let Some(haystack) = entry.get("haystack_sessions") {
                    if let Some(arr) = haystack.as_array() {
                        println!("    haystack_sessions count = {}", arr.len());
                        if let Some(first_session) = arr.first() {
                            let sess_keys: Vec<_> = first_session
                                .as_object()
                                .map(|m| m.keys().cloned().collect())
                                .unwrap_or_default();
                            println!("    first session keys = {:?}", sess_keys);
                            if let Some(turns) = first_session.get("turns") {
                                if let Some(t_arr) = turns.as_array() {
                                    println!("    first session has {} turns", t_arr.len());
                                    if let Some(t0) = t_arr.first() {
                                        let turn_keys: Vec<_> = t0
                                            .as_object()
                                            .map(|m| m.keys().cloned().collect())
                                            .unwrap_or_default();
                                        println!("      turn[0] keys = {:?}", turn_keys);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        serde_json::Value::Object(obj) => {
            println!(
                "LongMemEval is an object with top-level keys: {:?}",
                obj.keys().collect::<Vec<_>>()
            );
        }
        other => panic!("unexpected top-level shape: {other:?}"),
    }
    println!(
        "PROBE COMPLETE — copy the field names above into LongMemEvalQuestion struct in Task 3"
    );
}
