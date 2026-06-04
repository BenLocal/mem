//! `mem feedback-from-transcript` — scan a Claude Code transcript for
//! capsule-search MCP tool calls (see [`is_capsule_search_tool`] for
//! the accepted name shape), decide which retrieved capsules were
//! actually consumed by the agent, and POST
//! `/capability_capsules/feedback` for each.
//!
//! Heuristic for "consumed": tokenize both the memory's `text` (from
//! the `directives` / `relevant_facts` / `reusable_patterns` sections
//! of the search response) and any *subsequent* assistant
//! `text`/`thinking` block, count distinct shared tokens, and accept
//! as consumed when ≥`HIT_THRESHOLD` (3) match. Tokens are produced by
//! [`fingerprint`]: ASCII alphanumeric runs ≥4 chars are kept whole;
//! CJK runs emit 2-char n-grams to handle no-whitespace languages.
//! This is intentionally paraphrase-tolerant: an agent that quotes
//! "DuckDB" + "MVCC" + "concurrency" from a memory triggers a hit
//! even when the surrounding prose is reworded.
//!
//! Trade-off, per design: `applies_here` is +0.05 confidence, so the
//! occasional false positive is mild. The replaced heuristic
//! (verbatim 40-char prefix substring) was too strict — agents
//! almost never quote 40 chars verbatim, so the signal rarely fired.
//!
//! Default kind is `applies_here`. Negative kinds (`outdated`,
//! `incorrect`, `does_not_apply_here`) are out of scope for this hook —
//! they require human or agent judgment, not automatic inference.

use anyhow::Result;
use clap::Args;
use reqwest::Client;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use super::common::RemoteArgs;

/// True if `name` is one of the capsule/memory search MCP tools whose
/// results we want to scan for consumed hits. Covers a 2x3 matrix:
///
///   - prefix `mcp__mem__` (direct MCP registration) OR
///     `mcp__plugin_mem_mem__` (loaded as a Claude Code plugin)
///   - suffix `memory_search` / `memory_search_contextual` /
///     `memory_bootstrap` (pre-rename) OR
///     `capability_capsule_search` / `capability_capsule_search_contextual` /
///     `capability_capsule_bootstrap` (post-rename, current)
///
/// All six suffix variants share the same
/// `SearchCapabilityCapsuleResponse` body shape, so the downstream
/// snippet-match code works for any of them.
fn is_capsule_search_tool(name: &str) -> bool {
    let Some(suffix) = name
        .strip_prefix("mcp__mem__")
        .or_else(|| name.strip_prefix("mcp__plugin_mem_mem__"))
    else {
        return false;
    };
    matches!(
        suffix,
        "memory_search"
            | "memory_search_contextual"
            | "memory_bootstrap"
            | "capability_capsule_search"
            | "capability_capsule_search_contextual"
            | "capability_capsule_bootstrap"
    )
}

/// True if `name` is the capsule/memory GET MCP tool. A deliberate fetch
/// of a capsule's verbatim content is a strong "I'm using this" signal —
/// the recall hooks even instruct it ("`capability_capsule_get` it for the
/// verbatim content"). Same prefix matrix as [`is_capsule_search_tool`].
fn is_capsule_get_tool(name: &str) -> bool {
    let Some(suffix) = name
        .strip_prefix("mcp__mem__")
        .or_else(|| name.strip_prefix("mcp__plugin_mem_mem__"))
    else {
        return false;
    };
    matches!(suffix, "memory_get" | "capability_capsule_get")
}

/// Pull `mem_…` capsule ids out of a hook-injected recall block. The
/// auto-recall (UserPromptSubmit) and error-recall (PostToolUseFailure)
/// hooks inject `additionalContext` into the transcript as user/system
/// blocks, rendering each hit as `` - <snippet> `[mem_…]` ``. The
/// MCP-search scanner never sees these (they aren't tool calls), so the
/// feedback loop stayed open for the entire hook-driven recall path —
/// extracting the ids here is what re-closes it.
fn extract_injected_ids(line: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = line;
    while let Some(pos) = rest.find("[mem_") {
        let after = &rest[pos + 1..]; // skip the '['
        match after.find(']') {
            Some(end) => {
                let id = &after[..end];
                if is_valid_capsule_id(id) {
                    ids.push(id.to_string());
                }
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    ids
}

/// `mem_` + a UUIDv7 (8 hex digits then `-`). Guards against placeholder /
/// templated `[mem_xxx]` strings that appear in docs or tool output — those
/// would otherwise be POSTed as feedback and 404.
fn is_valid_capsule_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("mem_") else {
        return false;
    };
    let b = rest.as_bytes();
    b.len() >= 9 && b[8] == b'-' && b[..8].iter().all(u8::is_ascii_hexdigit)
}

/// Minimum char count for a kept ASCII-alphanumeric run. Shorter
/// (1-3 char) tokens are mostly stopwords / particles and would
/// dominate false positives.
const ASCII_TOKEN_MIN: usize = 4;

/// Window size for CJK-run n-grams. 2 chars per gram is the sweet
/// spot for Chinese (most distinctive 2-char terms are content
/// words: 学校 / 排查 / 字幕 / 录像 etc.).
const CJK_NGRAM_WINDOW: usize = 2;

/// Cap on fingerprint vector length per memory text. Bounds the
/// per-row work on a multi-KB capsule (the 广信息 handbook is 10K+
/// chars and would otherwise yield 1000s of n-grams).
const FINGERPRINT_BUDGET: usize = 64;

/// Minimum distinct fingerprint tokens that must reappear in a
/// later assistant block before that block counts as "consumed
/// this memory". Raise to reduce false positives; lower to catch
/// more paraphrased usage.
const HIT_THRESHOLD: usize = 3;

#[derive(Debug, Args)]
pub struct FeedbackFromTranscriptArgs {
    /// Path to the Claude Code transcript JSONL file.
    pub transcript_path: PathBuf,

    #[command(flatten)]
    pub remote: RemoteArgs,

    /// One of: `useful`, `applies_here`, `outdated`, `does_not_apply_here`,
    /// `incorrect`. The default is the mildest positive signal.
    #[arg(long, default_value = "applies_here")]
    pub kind: String,

    /// Skip the "consumed" heuristic and signal every retrieved memory.
    /// Use when you want a uniform soft-positive after every search,
    /// not just consumed hits.
    #[arg(long)]
    pub all: bool,
}

/// Typed result of a feedback-from-transcript pass.
#[derive(Debug, Clone, Default)]
pub struct FeedbackCounts {
    pub kind: String,
    pub sent: usize,
    pub consumed: usize,
    pub failed: usize,
}

impl FeedbackCounts {
    pub fn nothing_to_send(&self) -> bool {
        self.consumed == 0
    }
    /// Treat as a hard failure only when every attempt failed *and*
    /// at least one was attempted — matches the legacy exit-code
    /// shape (`failed > 0 && sent == 0`).
    pub fn hard_failure(&self) -> bool {
        self.failed > 0 && self.sent == 0
    }
}

pub async fn run(args: FeedbackFromTranscriptArgs) -> i32 {
    match run_with_counts(args).await {
        Ok(counts) => {
            if counts.nothing_to_send() {
                println!("feedback: no consumed memories detected");
                return 0;
            }
            println!(
                "feedback: kind={} sent={}/{} failed={}",
                counts.kind, counts.sent, counts.consumed, counts.failed,
            );
            if counts.hard_failure() {
                1
            } else {
                0
            }
        }
        Err(e) => {
            eprintln!("scan transcript: {e}");
            1
        }
    }
}

/// Same as [`run`] but returns typed counts to in-process callers (the
/// hook handlers). Errors only surface for unrecoverable transcript-
/// parse failures; per-row HTTP errors are counted in `failed`.
pub async fn run_with_counts(args: FeedbackFromTranscriptArgs) -> anyhow::Result<FeedbackCounts> {
    let consumed = scan_transcript(&args)?;
    if consumed.is_empty() {
        return Ok(FeedbackCounts {
            kind: args.kind,
            ..Default::default()
        });
    }
    let client = Client::new();
    let mut sent = 0usize;
    let mut failed = 0usize;
    for mid in &consumed {
        let body = serde_json::json!({
            "tenant": args.remote.tenant,
            "capability_capsule_id": mid,
            "feedback_kind": args.kind,
        });
        match client
            .post(format!(
                "{}/capability_capsules/feedback",
                args.remote.base_url
            ))
            .json(&body)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => sent += 1,
            Ok(resp) => {
                eprintln!("feedback {mid}: HTTP {}", resp.status());
                failed += 1;
            }
            Err(e) => {
                eprintln!("feedback {mid}: {e}");
                failed += 1;
            }
        }
    }
    Ok(FeedbackCounts {
        kind: args.kind,
        sent,
        consumed: consumed.len(),
        failed,
    })
}

/// One pass over the JSONL transcript; returns the set of
/// capability_capsule_ids that were retrieved AND used. A capsule counts
/// as **retrieved** if it came back from a capsule-search MCP call
/// ([`is_capsule_search_tool`]) OR was hook-injected into a user/system
/// block ([`extract_injected_ids`] — the auto-recall / error-recall path,
/// which is now how most recall happens). A retrieved capsule counts as
/// **used** if the agent later `capability_capsule_get`s it
/// ([`is_capsule_get_tool`] — a deliberate fetch) OR its `text` reappears
/// in a subsequent assistant `text`/`thinking` block. Crediting only
/// *retrieved* gets (not bare inspection gets) keeps precision.
fn scan_transcript(args: &FeedbackFromTranscriptArgs) -> Result<HashSet<String>> {
    let file = File::open(&args.transcript_path)?;
    let reader = BufReader::new(file);

    // (capability_capsule_id, text-prefix snippet, line index where retrieval happened)
    let mut retrieved: Vec<(String, Vec<String>, usize)> = Vec::new();
    // (line index, lower-cased assistant block text) — query corpus.
    let mut assistant_corpus: Vec<(usize, String)> = Vec::new();
    // tool_use_id → line index where the search call was issued.
    let mut search_calls: HashMap<String, usize> = HashMap::new();
    // capsule ids the agent deliberately fetched via capability_capsule_get.
    let mut fetched: HashSet<String> = HashSet::new();

    for (line_idx, line) in reader.lines().enumerate() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = match value["type"].as_str() {
            Some(r @ ("user" | "assistant" | "system")) => r,
            _ => continue,
        };
        // Mirror `cli::mine`: accept both array-of-blocks and plain-string
        // forms of `message.content`. Without this branch, ~80% of
        // user-typed messages (string-content shape) are silently
        // skipped — and our subsequent-text corpus, used for the
        // "consumed" heuristic, loses most of its signal.
        let raw_content = &value["message"]["content"];
        let owned_array;
        let content_array: &Vec<Value> = if let Some(arr) = raw_content.as_array() {
            arr
        } else if let Some(s) = raw_content.as_str() {
            owned_array = vec![serde_json::json!({"type": "text", "text": s})];
            &owned_array
        } else {
            continue;
        };

        for item in content_array {
            let block_type = item["type"].as_str().unwrap_or("");
            match block_type {
                "tool_use" => {
                    let name = item["name"].as_str().unwrap_or("");
                    if is_capsule_search_tool(name) {
                        if let Some(id) = item["id"].as_str() {
                            search_calls.insert(id.to_string(), line_idx);
                        }
                    } else if is_capsule_get_tool(name) {
                        if let Some(cid) = item["input"]["capability_capsule_id"].as_str() {
                            fetched.insert(cid.to_string());
                        }
                    }
                }
                "tool_result" => {
                    let tool_use_id = item["tool_use_id"].as_str().unwrap_or("");
                    // Content is a string (older) or an array of `{type, text}`
                    // chunks — flatten to a string either way.
                    let inner = extract_tool_result_text(&item["content"]);
                    if search_calls.contains_key(tool_use_id) {
                        // MCP capsule-search result: a single text chunk holding
                        // the JSON-encoded SearchCapabilityCapsuleResponse.
                        let Ok(resp) = serde_json::from_str::<Value>(&inner) else {
                            continue;
                        };
                        for section in ["directives", "relevant_facts", "reusable_patterns"] {
                            if let Some(arr) = resp[section].as_array() {
                                for entry in arr {
                                    let mid = entry["capability_capsule_id"].as_str().unwrap_or("");
                                    let text = entry["text"].as_str().unwrap_or("");
                                    if mid.is_empty() {
                                        continue;
                                    }
                                    let fp = if args.all {
                                        Vec::new()
                                    } else {
                                        fingerprint(text)
                                    };
                                    retrieved.push((mid.to_string(), fp, line_idx));
                                }
                            }
                        }
                    } else if inner.contains("mem auto-recall")
                        || inner.contains("related incidents/fixes")
                    {
                        // Hook-injected recall block. The UserPromptSubmit /
                        // PostToolUseFailure `additionalContext` lands in the
                        // transcript as a tool_result (NOT a text block), with
                        // each hit as `` - <snippet> `[mem_…]` ``. Treat each
                        // bullet as a retrieval driving the same consumed
                        // heuristic as MCP-search hits.
                        for ln in inner.lines() {
                            let ids = extract_injected_ids(ln);
                            if ids.is_empty() {
                                continue;
                            }
                            let fp = if args.all {
                                Vec::new()
                            } else {
                                fingerprint(ln)
                            };
                            for id in ids {
                                retrieved.push((id, fp.clone(), line_idx));
                            }
                        }
                    }
                }
                "text" if role == "assistant" => {
                    if let Some(t) = item["text"].as_str() {
                        assistant_corpus.push((line_idx, t.to_string()));
                    }
                }
                "thinking" if role == "assistant" => {
                    if let Some(t) = item["thinking"].as_str() {
                        assistant_corpus.push((line_idx, t.to_string()));
                    }
                }
                _ => {}
            }
        }
    }

    // Pre-compute fingerprint sets for each subsequent assistant
    // block once, so we don't re-tokenize per memory-row check.
    let assistant_fingerprints: Vec<(usize, HashSet<String>)> = assistant_corpus
        .iter()
        .map(|(line, text)| (*line, fingerprint(text).into_iter().collect()))
        .collect();

    let mut used: HashSet<String> = HashSet::new();
    for (mid, fp, ridx) in &retrieved {
        if used.contains(mid) {
            continue;
        }
        // Strong signal: the agent fetched this recalled capsule's verbatim
        // content via capability_capsule_get — credit without needing a
        // fingerprint match.
        if fetched.contains(mid) {
            used.insert(mid.clone());
            continue;
        }
        if args.all || fp.is_empty() {
            // `--all` or no usable fingerprint → accept.
            used.insert(mid.clone());
            continue;
        }
        let consumed = assistant_fingerprints
            .iter()
            .filter(|(l, _)| *l > *ridx)
            .any(|(_, fp_set)| {
                fp.iter().filter(|t| fp_set.contains(t.as_str())).count() >= HIT_THRESHOLD
            });
        if consumed {
            used.insert(mid.clone());
        }
    }
    Ok(used)
}

/// Extract distinctive content tokens from `text` for paraphrase-
/// tolerant matching:
/// - **ASCII / latin alphanumeric runs ≥`ASCII_TOKEN_MIN` chars**:
///   kept whole, lowercased. Drops common 1-3 char particles.
/// - **CJK runs**: emit `CJK_NGRAM_WINDOW` (2) char sliding-window
///   n-grams. No-whitespace languages need this since the whole
///   run would otherwise be one giant token that only matches
///   exact paraphrases.
///
/// Capped at `FINGERPRINT_BUDGET` distinct tokens (preserves input
/// order). Returns empty Vec when the text yields no distinctive
/// tokens — caller treats empty as "no signal, skip" unless
/// `--all` is set.
fn fingerprint(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut tokens: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current: Vec<char> = Vec::new();
    let mut current_has_cjk = false;
    for c in lower.chars() {
        if c.is_alphanumeric() {
            if !c.is_ascii() {
                current_has_cjk = true;
            }
            current.push(c);
        } else {
            flush_chunk(&current, current_has_cjk, &mut tokens, &mut seen);
            current.clear();
            current_has_cjk = false;
            if tokens.len() >= FINGERPRINT_BUDGET {
                return tokens;
            }
        }
    }
    flush_chunk(&current, current_has_cjk, &mut tokens, &mut seen);
    tokens.truncate(FINGERPRINT_BUDGET);
    tokens
}

/// Helper for [`fingerprint`]: emit tokens from one accumulated
/// alphanumeric run. Mixed-script runs (e.g. `duckdb广信息`) take
/// the CJK n-gram path on the principle that any CJK presence
/// means the run is structurally CJK-shaped and benefits from
/// finer-grained segmentation.
fn flush_chunk(
    chunk: &[char],
    has_cjk: bool,
    tokens: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    if chunk.is_empty() {
        return;
    }
    if has_cjk {
        if chunk.len() < CJK_NGRAM_WINDOW {
            return;
        }
        for i in 0..=chunk.len() - CJK_NGRAM_WINDOW {
            let ngram: String = chunk[i..i + CJK_NGRAM_WINDOW].iter().collect();
            if seen.insert(ngram.clone()) {
                tokens.push(ngram);
            }
        }
    } else if chunk.len() >= ASCII_TOKEN_MIN {
        let s: String = chunk.iter().collect();
        if seen.insert(s.clone()) {
            tokens.push(s);
        }
    }
}

/// Mirrors `cli::mine::extract_tool_result_content` shape but kept local
/// so this module doesn't depend on `mine`'s implementation detail.
/// `tool_result.content` is either a plain string or an array of
/// `{type, text}` chunks.
fn extract_tool_result_text(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(arr) = value.as_array() {
        let mut parts = Vec::with_capacity(arr.len());
        for chunk in arr {
            if let Some(t) = chunk["text"].as_str() {
                parts.push(t.to_string());
            } else if let Some(s) = chunk.as_str() {
                parts.push(s.to_string());
            }
        }
        return parts.join("\n");
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_extracts_ascii_tokens_min_4_chars() {
        let fp = fingerprint("DuckDB is single-writer per file but supports MVCC concurrency");
        // Lowercased, dedupes, keeps ≥4-char alphanumeric runs.
        // "is" / "per" / "but" dropped (<4 chars). "mvcc" kept (=4).
        assert!(fp.contains(&"duckdb".to_string()));
        assert!(fp.contains(&"single".to_string()));
        assert!(fp.contains(&"writer".to_string()));
        assert!(fp.contains(&"file".to_string()));
        assert!(fp.contains(&"supports".to_string()));
        assert!(fp.contains(&"mvcc".to_string()));
        assert!(fp.contains(&"concurrency".to_string()));
        assert!(!fp.contains(&"is".to_string()));
        assert!(!fp.contains(&"per".to_string()));
        assert!(!fp.contains(&"but".to_string()));
    }

    #[test]
    fn fingerprint_emits_2_char_ngrams_for_cjk() {
        // 6 distinct Chinese chars → 5 unique 2-grams (sliding).
        let fp = fingerprint("广信息学校排查");
        assert_eq!(fp, vec!["广信", "信息", "息学", "学校", "校排", "排查"]);
    }

    #[test]
    fn fingerprint_paraphrase_overlap_meets_threshold() {
        // Memory + paraphrase share at least HIT_THRESHOLD distinct
        // tokens. This is the key correctness property of the new
        // matcher — the OLD 40-char-prefix heuristic would have
        // missed every case below.
        let memory = "DuckDB is single-writer per file but supports MVCC concurrency";
        let paraphrase = "Watch out — DuckDB's MVCC handling makes concurrency tricky when multiple writers hit one file";
        let mfp = fingerprint(memory);
        let pset: HashSet<String> = fingerprint(paraphrase).into_iter().collect();
        let hits = mfp.iter().filter(|t| pset.contains(t.as_str())).count();
        assert!(
            hits >= HIT_THRESHOLD,
            "expected ≥{HIT_THRESHOLD} shared tokens between memory and paraphrase; got {hits}\nmemory tokens: {mfp:?}\nparaphrase tokens: {pset:?}",
        );
    }

    #[test]
    fn fingerprint_cjk_paraphrase_overlap_meets_threshold() {
        // Chinese memory + Chinese paraphrase with overlapping
        // 2-grams (学校 / 排查 in both).
        let memory = "广信息学校排查参考手册";
        let paraphrase = "今天又翻了一遍广信息学校的排查手册";
        let mfp = fingerprint(memory);
        let pset: HashSet<String> = fingerprint(paraphrase).into_iter().collect();
        let hits = mfp.iter().filter(|t| pset.contains(t.as_str())).count();
        assert!(
            hits >= HIT_THRESHOLD,
            "expected ≥{HIT_THRESHOLD} shared CJK 2-grams; got {hits}\nmemory: {mfp:?}\nparaphrase set: {pset:?}",
        );
    }

    #[test]
    fn fingerprint_rejects_uncorrelated_text() {
        // Memory and an unrelated assistant block should share <
        // HIT_THRESHOLD tokens.
        let memory = "DuckDB single-writer MVCC concurrency lock contention";
        let unrelated = "let me know if you want to grab lunch tomorrow at noon";
        let mfp = fingerprint(memory);
        let pset: HashSet<String> = fingerprint(unrelated).into_iter().collect();
        let hits = mfp.iter().filter(|t| pset.contains(t.as_str())).count();
        assert!(
            hits < HIT_THRESHOLD,
            "uncorrelated text should not meet threshold; got {hits} hits\nmemory: {mfp:?}\nunrelated set: {pset:?}",
        );
    }

    #[test]
    fn fingerprint_respects_budget_cap() {
        // Very long input must not produce more than FINGERPRINT_BUDGET tokens.
        let long_text = (0..200)
            .map(|i| format!("token{i:04}"))
            .collect::<Vec<_>>()
            .join(" ");
        let fp = fingerprint(&long_text);
        assert!(
            fp.len() <= FINGERPRINT_BUDGET,
            "fingerprint must be capped at {FINGERPRINT_BUDGET}; got {}",
            fp.len()
        );
    }

    #[test]
    fn fingerprint_empty_or_trivial_returns_empty() {
        assert!(fingerprint("").is_empty());
        assert!(fingerprint("   ").is_empty());
        // Single 3-char ASCII run drops below MIN.
        assert!(fingerprint("foo").is_empty());
        // Two too-short runs.
        assert!(fingerprint("foo bar baz").is_empty());
    }

    #[test]
    fn extract_tool_result_text_handles_string_and_array() {
        let v = serde_json::json!("plain string");
        assert_eq!(extract_tool_result_text(&v), "plain string");

        let v = serde_json::json!([{"type": "text", "text": "first"}, {"type": "text", "text": "second"}]);
        assert_eq!(extract_tool_result_text(&v), "first\nsecond");
    }

    /// Real transcripts carry four variants of the search-tool name
    /// (pre-/post-rename × direct/plugin-namespace). The matcher must
    /// accept all four and reject unrelated tool names.
    #[test]
    fn is_capsule_search_tool_accepts_all_four_variants() {
        // direct + pre-rename
        assert!(is_capsule_search_tool("mcp__mem__memory_search"));
        assert!(is_capsule_search_tool("mcp__mem__memory_search_contextual"));
        assert!(is_capsule_search_tool("mcp__mem__memory_bootstrap"));
        // plugin + pre-rename
        assert!(is_capsule_search_tool("mcp__plugin_mem_mem__memory_search"));
        assert!(is_capsule_search_tool(
            "mcp__plugin_mem_mem__memory_search_contextual"
        ));
        assert!(is_capsule_search_tool(
            "mcp__plugin_mem_mem__memory_bootstrap"
        ));
        // direct + post-rename
        assert!(is_capsule_search_tool(
            "mcp__mem__capability_capsule_search"
        ));
        assert!(is_capsule_search_tool(
            "mcp__mem__capability_capsule_search_contextual"
        ));
        assert!(is_capsule_search_tool(
            "mcp__mem__capability_capsule_bootstrap"
        ));
        // plugin + post-rename (today's live shape)
        assert!(is_capsule_search_tool(
            "mcp__plugin_mem_mem__capability_capsule_search"
        ));
        assert!(is_capsule_search_tool(
            "mcp__plugin_mem_mem__capability_capsule_search_contextual"
        ));
        assert!(is_capsule_search_tool(
            "mcp__plugin_mem_mem__capability_capsule_bootstrap"
        ));
    }

    #[test]
    fn is_capsule_search_tool_rejects_unrelated() {
        // wrong prefix
        assert!(!is_capsule_search_tool("memory_search"));
        assert!(!is_capsule_search_tool("mcp__other__memory_search"));
        // wrong suffix
        assert!(!is_capsule_search_tool("mcp__mem__memory_ingest"));
        assert!(!is_capsule_search_tool("mcp__mem__memory_feedback"));
        assert!(!is_capsule_search_tool(
            "mcp__plugin_mem_mem__capability_capsule_ingest"
        ));
        // empty
        assert!(!is_capsule_search_tool(""));
    }

    #[test]
    fn is_capsule_get_tool_accepts_variants_rejects_others() {
        assert!(is_capsule_get_tool("mcp__mem__capability_capsule_get"));
        assert!(is_capsule_get_tool(
            "mcp__plugin_mem_mem__capability_capsule_get"
        ));
        assert!(is_capsule_get_tool("mcp__mem__memory_get"));
        // not a get
        assert!(!is_capsule_get_tool(
            "mcp__plugin_mem_mem__capability_capsule_search"
        ));
        assert!(!is_capsule_get_tool(
            "mcp__plugin_mem_mem__capability_capsule_ingest"
        ));
        assert!(!is_capsule_get_tool("capability_capsule_get")); // missing prefix
        assert!(!is_capsule_get_tool(""));
    }

    #[test]
    fn extract_injected_ids_parses_recall_bullets() {
        // The shape the recall hooks inject.
        let line = "- the fix is X (src/a.rs)  `[mem_019e8cf8-56f8-7c42-b4eb-ff60cce4900c]`";
        assert_eq!(
            extract_injected_ids(line),
            vec!["mem_019e8cf8-56f8-7c42-b4eb-ff60cce4900c".to_string()]
        );
        // Multiple ids on one line are all pulled.
        let two = "see `[mem_aaaa1111-0000]` and `[mem_bbbb2222-9999]`";
        assert_eq!(
            extract_injected_ids(two),
            vec![
                "mem_aaaa1111-0000".to_string(),
                "mem_bbbb2222-9999".to_string()
            ]
        );
        // No id / unterminated → empty, no panic.
        assert!(extract_injected_ids("plain assistant text, no ids").is_empty());
        assert!(extract_injected_ids("dangling [mem_no_close").is_empty());
        // Placeholder / templated ids (not a hex UUIDv7) are rejected.
        assert!(extract_injected_ids("see `[mem_xxx]` placeholder").is_empty());
        assert!(extract_injected_ids("`[mem_notahexuuid]`").is_empty());
    }
}
