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
//! they require human or agent judgment, not automatic inference. That
//! keeps explicit negative volume near zero by design; wrongness flows
//! through `supersede` (version chains) and never-recalled staleness
//! through the evolution worker's `reweight_decay` lane, so the absence
//! of explicit negative feedback is expected, not a broken loop
//! (audits 2026-06-12 + 2026-07-13).
//!
//! Once per session: each pass stores a cursor under the pseudo-path
//! `<transcript_path>#feedback` (mine-cursor store) and only credits
//! capsules whose FIRST crediting evidence line lies beyond it, so the
//! Stop / PreCompact hooks re-running over one growing transcript no
//! longer re-send `applies_here` for the same capsules every pass.

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
    /// Capsules whose crediting evidence sits at or before the
    /// per-transcript cursor — already credited by an earlier pass over
    /// this (growing) transcript, so skipped this time.
    pub deduped: usize,
}

/// Result of one transcript scan: which capsules earned crediting
/// evidence, where, and how far the scan read.
#[derive(Debug, Default)]
struct ScanOutcome {
    /// capsule id → **1-based** line number of the FIRST crediting
    /// evidence: the earliest of (a) a `capability_capsule_get` of the id,
    /// (b) the first assistant block whose fingerprint overlap meets
    /// [`HIT_THRESHOLD`], or (c) the retrieval line itself under `--all` /
    /// when the retrieved text has no usable fingerprint.
    credited: HashMap<String, usize>,
    /// Total lines read — the cursor watermark after this pass.
    lines_scanned: usize,
}

impl ScanOutcome {
    /// Ids whose first evidence is strictly after `cursor` (a 1-based
    /// line number). Evidence at or before the cursor was visible to —
    /// and therefore credited by — the pass that stored that cursor.
    /// `None` (first pass / cursor fetch failure) keeps everything:
    /// fail-open to the legacy full-credit behavior.
    fn credited_since(&self, cursor: Option<i64>) -> HashSet<String> {
        self.credited
            .iter()
            .filter(|(_, &line)| cursor.is_none_or(|c| line as i64 > c))
            .map(|(id, _)| id.clone())
            .collect()
    }
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
                if counts.deduped > 0 {
                    println!(
                        "feedback: no new consumed memories ({} already credited by an earlier pass)",
                        counts.deduped
                    );
                } else {
                    println!("feedback: no consumed memories detected");
                }
                return 0;
            }
            println!(
                "feedback: kind={} sent={}/{} failed={} deduped={}",
                counts.kind, counts.sent, counts.consumed, counts.failed, counts.deduped,
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
///
/// Dedup across passes: the Stop / PreCompact hooks re-run this over the
/// same growing transcript every ~15 exchanges, which used to re-POST
/// `applies_here` for every already-credited capsule each time (measured
/// 2026-07-10: 491 sends over 66 distinct capsules — half the active pool
/// pinned at confidence 1.0). Each pass now stores a cursor under the
/// pseudo-path `<transcript_path>#feedback` in the mine-cursor store
/// (reusing `/mine/cursors` — no new endpoint or table) and only credits
/// capsules whose first evidence line is beyond it. Cursor read failures
/// fail open (full credit, at worst one duplicate pass); the cursor only
/// advances after a pass with zero send failures.
pub async fn run_with_counts(args: FeedbackFromTranscriptArgs) -> anyhow::Result<FeedbackCounts> {
    let outcome = scan_transcript(&args.transcript_path, args.all)?;
    let client = Client::new();
    let cursor_key = format!("{}#feedback", args.transcript_path.display());
    let cursor: Option<i64> = match client
        .get(format!("{}/mine/cursors", args.remote.base_url))
        .query(&[("transcript_path", cursor_key.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("last_line_number").and_then(|n| n.as_i64())),
        _ => None,
    };
    let to_send = outcome.credited_since(cursor);
    let deduped = outcome.credited.len() - to_send.len();

    let mut sent = 0usize;
    let mut failed = 0usize;
    for mid in &to_send {
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
    // Advance the watermark after a clean pass — including an
    // empty-send one — so the next pass over this transcript skips
    // everything credited (or evaluated) up to here. Best-effort, same
    // contract as mine's cursor: a lost write only costs one duplicate
    // pass, never correctness.
    if failed == 0 {
        let body = serde_json::json!({
            "transcript_path": cursor_key,
            "last_line_number": outcome.lines_scanned as i64,
        });
        if let Err(e) = client
            .post(format!("{}/mine/cursors", args.remote.base_url))
            .json(&body)
            .send()
            .await
        {
            eprintln!("feedback cursor advance: {e}");
        }
    }
    Ok(FeedbackCounts {
        kind: args.kind,
        sent,
        consumed: to_send.len(),
        failed,
        deduped,
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
/// Populate the retrieval / usage collections from ONE Codex rollout line —
/// the Codex analog of the per-line logic inside [`scan_transcript`]:
///
/// - a `developer` / `user` / `system` message carrying a recall banner
///   → `retrieved`. Codex injects the UserPromptSubmit / SessionStart
///   `additionalContext` as a `role:"developer"` message (not a Claude
///   `hook_additional_context` attachment). Assistant messages are NEVER
///   treated as retrieval — they'd self-credit a session discussing mem.
/// - assistant `message` text + `reasoning` summary → `assistant_corpus`
///   (the "did the agent use it" signal).
/// - a `function_call` to a capsule search/get MCP tool + its
///   `function_call_output` → `search_calls` / `fetched` / `retrieved`.
///   The tool name is matched by substring because Codex's MCP tool prefix
///   differs from Claude's `mcp__…__` (and no real Codex MCP-tool data
///   exists yet to pin the exact form).
#[allow(clippy::too_many_arguments)]
fn collect_codex_line(
    value: &Value,
    line_idx: usize,
    all: bool,
    retrieved: &mut Vec<(String, Vec<String>, usize)>,
    assistant_corpus: &mut Vec<(usize, String)>,
    search_calls: &mut HashMap<String, usize>,
    fetched: &mut HashMap<String, usize>,
) {
    if value["type"].as_str() != Some("response_item") {
        return;
    }
    let payload = &value["payload"];
    match payload["type"].as_str().unwrap_or("") {
        "message" => {
            let text = crate::cli::mine::codex_message_text(&payload["content"]);
            if payload["role"].as_str() == Some("assistant") {
                assistant_corpus.push((line_idx, text));
            } else {
                push_codex_banner_ids(&text, all, line_idx, retrieved);
            }
        }
        "reasoning" => {
            let text = crate::cli::mine::codex_summary_text(&payload["summary"]);
            if !text.is_empty() {
                assistant_corpus.push((line_idx, text));
            }
        }
        "function_call" => {
            let name = payload["name"].as_str().unwrap_or("");
            if name.contains("capability_capsule_search") {
                if let Some(cid) = payload["call_id"].as_str() {
                    search_calls.insert(cid.to_string(), line_idx);
                }
            } else if name.contains("capability_capsule_get") {
                if let Ok(parsed) =
                    serde_json::from_str::<Value>(payload["arguments"].as_str().unwrap_or(""))
                {
                    if let Some(cid) = parsed["capability_capsule_id"].as_str() {
                        fetched.entry(cid.to_string()).or_insert(line_idx + 1);
                    }
                }
            }
        }
        "function_call_output" => {
            let call_id = payload["call_id"].as_str().unwrap_or("");
            let inner = crate::cli::mine::codex_output_text(&payload["output"]);
            if search_calls.contains_key(call_id) {
                if let Ok(resp) = serde_json::from_str::<Value>(&inner) {
                    for section in ["directives", "relevant_facts", "reusable_patterns"] {
                        if let Some(arr) = resp[section].as_array() {
                            for entry in arr {
                                let mid = entry["capability_capsule_id"].as_str().unwrap_or("");
                                if mid.is_empty() {
                                    continue;
                                }
                                let text = entry["text"].as_str().unwrap_or("");
                                let fp = if all { Vec::new() } else { fingerprint(text) };
                                retrieved.push((mid.to_string(), fp, line_idx));
                            }
                        }
                    }
                }
            } else {
                push_codex_banner_ids(&inner, all, line_idx, retrieved);
            }
        }
        _ => {}
    }
}

/// Extract `[mem_…]` ids from every line of a banner-bearing text into
/// `retrieved`. No-op when the text carries no recall-banner marker. Free
/// function (not a closure) so it doesn't hold a long-lived borrow of
/// `retrieved` that would collide with the direct pushes in
/// [`collect_codex_line`].
fn push_codex_banner_ids(
    text: &str,
    all: bool,
    line_idx: usize,
    retrieved: &mut Vec<(String, Vec<String>, usize)>,
) {
    if !(text.contains("mem auto-recall") || text.contains("related incidents/fixes")) {
        return;
    }
    for ln in text.lines() {
        let ids = extract_injected_ids(ln);
        if ids.is_empty() {
            continue;
        }
        let fp = if all { Vec::new() } else { fingerprint(ln) };
        for id in ids {
            retrieved.push((id, fp.clone(), line_idx));
        }
    }
}

/// pi analog of [`collect_codex_line`]. One `{"type":"message", message:{role,
/// content:[…]}}` line; `content[]` blocks are Anthropic-shaped.
/// - user/system `text` block with a recall banner → `retrieved`.
/// - assistant `text` block → `assistant_corpus` (the "did the agent use it"
///   signal). Assistant text is NEVER treated as retrieval (would self-credit
///   a session that merely discussed mem).
/// - `tool_use` to a capsule search/get tool + its `tool_result` →
///   `search_calls` / `fetched` / `retrieved`. Tool name matched by substring
///   (pi's MCP tool prefix differs from Claude's `mcp__…__`).
#[allow(clippy::too_many_arguments)]
fn collect_pi_line(
    value: &Value,
    line_idx: usize,
    all: bool,
    retrieved: &mut Vec<(String, Vec<String>, usize)>,
    assistant_corpus: &mut Vec<(usize, String)>,
    search_calls: &mut HashMap<String, usize>,
    fetched: &mut HashMap<String, usize>,
) {
    if value["type"].as_str() != Some("message") {
        return;
    }
    let msg = &value["message"];
    let role = msg["role"].as_str().unwrap_or("");
    let content = match msg["content"].as_array() {
        Some(arr) => arr,
        None => return,
    };

    for item in content {
        match item["type"].as_str().unwrap_or("") {
            "text" => {
                let text = item["text"].as_str().unwrap_or("");
                if role == "assistant" {
                    assistant_corpus.push((line_idx, text.to_string()));
                } else {
                    push_codex_banner_ids(text, all, line_idx, retrieved);
                }
            }
            "tool_use" => {
                let name = item["name"].as_str().unwrap_or("");
                if name.contains("capability_capsule_search") {
                    if let Some(cid) = item["id"].as_str() {
                        search_calls.insert(cid.to_string(), line_idx);
                    }
                } else if name.contains("capability_capsule_get") {
                    if let Some(cid) = item["input"]["capability_capsule_id"].as_str() {
                        fetched.entry(cid.to_string()).or_insert(line_idx + 1);
                    }
                }
            }
            "tool_result" => {
                let call_id = item["tool_use_id"].as_str().unwrap_or("");
                let inner = crate::cli::mine::codex_output_text(&item["content"]);
                if search_calls.contains_key(call_id) {
                    if let Ok(resp) = serde_json::from_str::<Value>(&inner) {
                        for section in ["directives", "relevant_facts", "reusable_patterns"] {
                            if let Some(arr) = resp[section].as_array() {
                                for entry in arr {
                                    let mid = entry["capability_capsule_id"].as_str().unwrap_or("");
                                    if mid.is_empty() {
                                        continue;
                                    }
                                    let text = entry["text"].as_str().unwrap_or("");
                                    let fp = if all { Vec::new() } else { fingerprint(text) };
                                    retrieved.push((mid.to_string(), fp, line_idx));
                                }
                            }
                        }
                    }
                } else {
                    push_codex_banner_ids(&inner, all, line_idx, retrieved);
                }
            }
            _ => {}
        }
    }
}

fn scan_transcript(transcript_path: &std::path::Path, all: bool) -> Result<ScanOutcome> {
    let file = File::open(transcript_path)?;
    let reader = BufReader::new(file);

    // (capability_capsule_id, text-prefix snippet, line index where retrieval happened)
    let mut retrieved: Vec<(String, Vec<String>, usize)> = Vec::new();
    // (line index, lower-cased assistant block text) — query corpus.
    let mut assistant_corpus: Vec<(usize, String)> = Vec::new();
    // tool_use_id → line index where the search call was issued.
    let mut search_calls: HashMap<String, usize> = HashMap::new();
    // capsule id → 1-based line of the FIRST deliberate
    // capability_capsule_get fetch.
    let mut fetched: HashMap<String, usize> = HashMap::new();
    let mut lines_scanned = 0usize;

    // Codex (`rollout-*.jsonl`) has a different envelope than Claude Code;
    // detect once and route each line to the matching collector. Claude is
    // the default (unchanged legacy path).
    let format = crate::cli::mine::detect_transcript_format(transcript_path);

    for (line_idx, line) in reader.lines().enumerate() {
        lines_scanned = line_idx + 1;
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if format == crate::cli::mine::TranscriptFormat::CodexRollout {
            collect_codex_line(
                &value,
                line_idx,
                all,
                &mut retrieved,
                &mut assistant_corpus,
                &mut search_calls,
                &mut fetched,
            );
            continue;
        }

        if format == crate::cli::mine::TranscriptFormat::PiSession {
            collect_pi_line(
                &value,
                line_idx,
                all,
                &mut retrieved,
                &mut assistant_corpus,
                &mut search_calls,
                &mut fetched,
            );
            continue;
        }

        // Hook-injected recall now lands as a `type:"attachment"` line
        // with `attachment.type == "hook_additional_context"` and the
        // text under `attachment.content` (a list of strings). Claude
        // Code changed this from the old user/system-block injection, so
        // the `tool_result` branch below no longer sees it — handle it
        // here, extracting the `[mem_…]` ids the same way. Without this,
        // the feedback loop is silently dead for hook-driven recall (the
        // dominant recall path).
        if value["type"].as_str() == Some("attachment") {
            let attachment = &value["attachment"];
            if attachment["type"].as_str() == Some("hook_additional_context") {
                let texts: Vec<&str> = match &attachment["content"] {
                    Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect(),
                    Value::String(s) => vec![s.as_str()],
                    _ => Vec::new(),
                };
                for inner in texts {
                    if !(inner.contains("mem auto-recall")
                        || inner.contains("related incidents/fixes"))
                    {
                        continue;
                    }
                    for ln in inner.lines() {
                        let ids = extract_injected_ids(ln);
                        if ids.is_empty() {
                            continue;
                        }
                        let fp = if all { Vec::new() } else { fingerprint(ln) };
                        for id in ids {
                            retrieved.push((id, fp.clone(), line_idx));
                        }
                    }
                }
            }
            continue; // attachment lines carry no `message.content`
        }

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
                            fetched.entry(cid.to_string()).or_insert(line_idx + 1);
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
                                    let fp = if all { Vec::new() } else { fingerprint(text) };
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
                            let fp = if all { Vec::new() } else { fingerprint(ln) };
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

    let mut credited: HashMap<String, usize> = HashMap::new();
    for (mid, fp, ridx) in &retrieved {
        // Strong signal: the agent fetched this recalled capsule's verbatim
        // content via capability_capsule_get — credit without needing a
        // fingerprint match. Otherwise `--all` / no usable fingerprint
        // credit on retrieval alone; the fingerprint path credits on the
        // FIRST assistant block after the retrieval that meets the
        // threshold. The evidence line (1-based) is what the cursor
        // filter in [`ScanOutcome::credited_since`] compares against.
        let evidence: Option<usize> = if let Some(&get_line) = fetched.get(mid) {
            Some(get_line)
        } else if all || fp.is_empty() {
            Some(ridx + 1)
        } else {
            assistant_fingerprints
                .iter()
                .filter(|(l, _)| *l > *ridx)
                .find(|(_, fp_set)| {
                    fp.iter().filter(|t| fp_set.contains(t.as_str())).count() >= HIT_THRESHOLD
                })
                .map(|(l, _)| l + 1)
        };
        // A capsule retrieved several times keeps its earliest evidence.
        if let Some(e) = evidence {
            credited
                .entry(mid.clone())
                .and_modify(|cur| *cur = (*cur).min(e))
                .or_insert(e);
        }
    }
    Ok(ScanOutcome {
        credited,
        lines_scanned,
    })
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

    /// Regression: hook recall now lands as a `type:"attachment"` /
    /// `hook_additional_context` line, not a user/system block. A capsule
    /// injected that way and then reused by a later assistant block MUST be
    /// credited as consumed — before the attachment branch existed,
    /// `scan_transcript` `continue`d past it and returned an empty set, so
    /// the whole hook-driven feedback loop was silently dead.
    #[test]
    fn scan_transcript_credits_hook_attachment_recall_consumed_by_assistant() {
        use std::io::Write;

        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        // The attachment banner carries the recall header (passes the
        // `mem auto-recall` guard) plus one `[mem_…]` bullet whose
        // distinctive tokens the assistant later reuses.
        let banner = format!(
            "🧠 mem auto-recall — memories relevant to this prompt\n\
             - DuckDB single-writer MVCC concurrency lock contention `[{id}]`"
        );
        let attachment = serde_json::json!({
            "type": "attachment",
            "attachment": { "type": "hook_additional_context", "content": [banner] },
        });
        let assistant = serde_json::json!({
            "type": "assistant",
            "message": { "role": "assistant", "content": [{
                "type": "text",
                "text": "Right — DuckDB is single-writer, so MVCC concurrency and \
                         lock contention bite when two writers share one file.",
            }]},
        });

        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{attachment}").unwrap();
        writeln!(f, "{assistant}").unwrap();

        let consumed = scan_transcript(f.path(), false).unwrap();
        assert!(
            consumed.credited.contains_key(id),
            "hook-attachment recall reused by a later assistant block must be \
             credited as consumed, got {:?}",
            consumed.credited
        );

        // A banner with NO subsequent reuse stays uncredited (the heuristic
        // is not just "id appeared in an attachment").
        let mut g = tempfile::NamedTempFile::new().unwrap();
        writeln!(g, "{attachment}").unwrap();
        let unconsumed = scan_transcript(g.path(), false).unwrap();
        assert!(
            !unconsumed.credited.contains_key(id),
            "an un-reused recalled capsule must not be credited, got {:?}",
            unconsumed.credited
        );
    }

    // ── Codex rollout feedback support ──────────────────────────────────
    // In a Codex `rollout-*.jsonl` the recall banner (UserPromptSubmit
    // additionalContext) is injected as a `response_item` message with
    // `role:"developer"` — not a Claude `hook_additional_context`
    // attachment. Capsule usage lands in `role:"assistant"` output_text /
    // reasoning. scan_transcript must detect the Codex shape and populate
    // the same retrieval/usage signals.

    #[test]
    fn scan_codex_rollout_credits_banner_reused_by_assistant() {
        use std::io::Write;

        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        let banner = format!(
            "🧠 mem auto-recall — memories relevant to this prompt\n\
             - DuckDB single-writer MVCC concurrency lock contention `[{id}]`"
        );
        let meta = serde_json::json!({"type":"session_meta","payload":{"session_id":"cx"},"timestamp":"t"});
        let dev = serde_json::json!({
            "type":"response_item",
            "payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":banner}]},
            "timestamp":"t"
        });
        let asst = serde_json::json!({
            "type":"response_item",
            "payload":{"type":"message","role":"assistant","content":[{"type":"output_text",
                "text":"Right — DuckDB is single-writer, so MVCC concurrency and \
                        lock contention bite when two writers share one file."}]},
            "timestamp":"t"
        });

        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{dev}").unwrap();
        writeln!(f, "{asst}").unwrap();
        let consumed = scan_transcript(f.path(), false).unwrap();
        assert!(
            consumed.credited.contains_key(id),
            "codex developer-message recall reused by a later assistant block must be credited, got {:?}",
            consumed.credited
        );

        // No reuse → not credited.
        let mut g = tempfile::NamedTempFile::new().unwrap();
        writeln!(g, "{meta}").unwrap();
        writeln!(g, "{dev}").unwrap();
        let unconsumed = scan_transcript(g.path(), false).unwrap();
        assert!(
            !unconsumed.credited.contains_key(id),
            "un-reused codex recall must not be credited, got {:?}",
            unconsumed.credited
        );
    }

    #[test]
    fn scan_codex_assistant_banner_is_not_retrieval() {
        // An ASSISTANT message that merely quotes the banner text must NOT
        // count as a retrieval (it's usage corpus, not injection) — else a
        // Codex session discussing mem would self-credit.
        use std::io::Write;
        let id = "mem_019ebbbb-cccc-7ddd-8eee-ffffffffffff";
        let asst = serde_json::json!({
            "type":"response_item",
            "payload":{"type":"message","role":"assistant","content":[{"type":"output_text",
                "text":format!("as the 🧠 mem auto-recall banner showed `[{id}]`")}]},
            "timestamp":"t"
        });
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"cx\"}}}}"
        )
        .unwrap();
        writeln!(f, "{asst}").unwrap();
        let out = scan_transcript(f.path(), false).unwrap();
        assert!(
            !out.credited.contains_key(id),
            "assistant self-mention of the banner must not be credited, got {:?}",
            out.credited
        );
    }

    #[test]
    fn scan_codex_function_call_get_credits_strong_signal() {
        // A codex `function_call` to a capsule-get MCP tool is a strong
        // "the agent fetched this recalled capsule" signal — credited even
        // without fingerprint reuse. Tool name matched loosely (Codex's MCP
        // prefix differs from Claude's `mcp__…__`).
        use std::io::Write;
        let id = "mem_019ecccc-dddd-7eee-8fff-000000000000";
        let banner = format!("🧠 mem auto-recall\n- some fact `[{id}]`");
        let meta = serde_json::json!({"type":"session_meta","payload":{"session_id":"cx"}});
        let dev = serde_json::json!({
            "type":"response_item",
            "payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":banner}]}
        });
        let get = serde_json::json!({
            "type":"response_item",
            "payload":{"type":"function_call","name":"mem__capability_capsule_get",
                "arguments":format!("{{\"capability_capsule_id\":\"{id}\"}}"),"call_id":"c1"}
        });
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{meta}").unwrap();
        writeln!(f, "{dev}").unwrap();
        writeln!(f, "{get}").unwrap();
        let out = scan_transcript(f.path(), false).unwrap();
        assert!(
            out.credited.contains_key(id),
            "a capability_capsule_get of a recalled id must credit it, got {:?}",
            out.credited
        );
    }

    #[test]
    fn scan_pi_transcript_credits_banner_reused_by_assistant() {
        // A pi user message carrying an injected recall banner, followed by
        // an assistant message that reuses the retrieved text — mirrors
        // `scan_codex_rollout_credits_banner_reused_by_assistant` but with
        // pi's `{type:"message", message:{role, content:[…]}}` envelope.
        use std::io::Write;

        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        let banner = format!(
            "🧠 mem auto-recall — memories relevant to this prompt\n\
             - DuckDB single-writer MVCC concurrency lock contention `[{id}]`"
        );
        let session = serde_json::json!({
            "type":"session","version":3,"id":"sess-1","timestamp":"t","cwd":"/r"
        });
        let user = serde_json::json!({
            "type":"message","id":"u1",
            "message":{"role":"user","content":[{"type":"text","text":banner}]}
        });
        let asst = serde_json::json!({
            "type":"message","id":"a1",
            "message":{"role":"assistant","content":[{"type":"text",
                "text":"Right — DuckDB is single-writer, so MVCC concurrency and \
                        lock contention bite when two writers share one file."}]}
        });

        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{session}").unwrap();
        writeln!(f, "{user}").unwrap();
        writeln!(f, "{asst}").unwrap();
        let consumed = scan_transcript(f.path(), false).unwrap();
        assert!(
            consumed.credited.contains_key(id),
            "pi user-message recall reused by a later assistant block must be credited, got {:?}",
            consumed.credited
        );

        // No reuse → not credited.
        let mut g = tempfile::NamedTempFile::new().unwrap();
        writeln!(g, "{session}").unwrap();
        writeln!(g, "{user}").unwrap();
        let unconsumed = scan_transcript(g.path(), false).unwrap();
        assert!(
            !unconsumed.credited.contains_key(id),
            "un-reused pi recall must not be credited, got {:?}",
            unconsumed.credited
        );
    }

    // ---- round-trip with the REAL banner renderer (progressive disclosure) --
    //
    // These tests feed `format_prompt_recall_styled`'s verbatim output into
    // `scan_transcript`, binding the renderer and the parser into one test
    // suite. The renderer/parser format coupling has silently broken three
    // times (see capsule mem_019e9214); after this, a format drift fails at
    // test time instead of killing the feedback loop in production.

    fn rendered_index_banner(id: &str) -> String {
        let cap = serde_json::json!({
            "relevant_facts": [{
                "text": "完整正文：泛化算子的共享主题信号从共享 topics 改为 topics 与 tags 的并集，解锁结构性沉默。",
                "source_summary": "泛化共享信号改 topics∪tags 摘要行",
                "capability_capsule_id": id,
            }]
        });
        let v = crate::cli::hook::format_prompt_recall_styled(
            &cap,
            &serde_json::json!({}),
            crate::cli::hook::RecallStyle::Index,
        );
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("renderer must emit additionalContext")
            .to_string()
    }

    fn attachment_line(banner: &str) -> String {
        serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "hook_additional_context", "content": [banner]},
        })
        .to_string()
    }

    #[test]
    fn roundtrip_index_banner_plus_get_is_consumed() {
        use std::io::Write;
        let id = "mem_01900000-0000-7000-8000-000000000abc";
        let get_call = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "toolu_rt1",
                "name": "mcp__plugin_mem_mem__capability_capsule_get",
                "input": {"capability_capsule_id": id},
            }]},
        });
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", attachment_line(&rendered_index_banner(id))).unwrap();
        writeln!(f, "{get_call}").unwrap();
        let consumed = scan_transcript(f.path(), false).unwrap();
        assert!(
            consumed.credited.contains_key(id),
            "index banner + deliberate get must credit, got {:?}",
            consumed.credited
        );
    }

    #[test]
    fn roundtrip_index_banner_alone_is_not_consumed() {
        use std::io::Write;
        let id = "mem_01900000-0000-7000-8000-000000000abc";
        let unrelated = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "text",
                "text": "Completely unrelated reply about playwright selectors and npx caches.",
            }]},
        });
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", attachment_line(&rendered_index_banner(id))).unwrap();
        writeln!(f, "{unrelated}").unwrap();
        let consumed = scan_transcript(f.path(), false).unwrap();
        assert!(
            !consumed.credited.contains_key(id),
            "skimmed-and-ignored index banner must stay silent, got {:?}",
            consumed.credited
        );
    }

    // ---- cursor-aware crediting (duplicate re-credit fix, 2026-07-13) ----
    //
    // `feedback-from-transcript` used to rescan the whole transcript on
    // every Stop-hook pass and re-POST `applies_here` for the same capsules
    // (measured 2026-07-10: 491 sends over 66 distinct capsules, max 19×
    // for one capsule in a day — pinning 49% of the active pool at
    // confidence 1.0). The fix keys crediting on the FIRST evidence line
    // and filters against a per-transcript cursor.

    /// Shared fixture: banner bullet whose distinctive tokens the
    /// consuming assistant block reuses (same pair as the attachment
    /// regression test above).
    fn banner_attachment(id: &str) -> String {
        let banner = format!(
            "🧠 mem auto-recall — memories relevant to this prompt\n\
             - DuckDB single-writer MVCC concurrency lock contention `[{id}]`"
        );
        attachment_line(&banner)
    }

    fn consuming_assistant() -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "text",
                "text": "Right — DuckDB is single-writer, so MVCC concurrency and \
                         lock contention bite when two writers share one file.",
            }]},
        })
        .to_string()
    }

    fn unrelated_assistant() -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "text",
                "text": "Completely unrelated reply about playwright selectors and npx caches.",
            }]},
        })
        .to_string()
    }

    #[test]
    fn scan_records_first_evidence_line_and_watermark() {
        use std::io::Write;
        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        // Banner L1, consuming assistant L2, identical re-consumption L3.
        // Evidence must be the FIRST match (line 2, 1-based), and the
        // watermark must cover the whole file (3 lines).
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", banner_attachment(id)).unwrap();
        writeln!(f, "{}", consuming_assistant()).unwrap();
        writeln!(f, "{}", consuming_assistant()).unwrap();

        let outcome = scan_transcript(f.path(), false).unwrap();
        assert_eq!(
            outcome.credited.get(id).copied(),
            Some(2),
            "first crediting evidence must be the earliest matching line, got {:?}",
            outcome.credited
        );
        assert_eq!(outcome.lines_scanned, 3);
    }

    #[test]
    fn credited_since_drops_first_evidence_at_or_before_cursor() {
        use std::io::Write;
        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        // Banner L1, consume L2, filler L3, re-reference L4. A cursor at 3
        // means L2 was already credited by a previous pass — the L4
        // re-reference must NOT re-credit (once per session).
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", banner_attachment(id)).unwrap();
        writeln!(f, "{}", consuming_assistant()).unwrap();
        writeln!(f, "{}", unrelated_assistant()).unwrap();
        writeln!(f, "{}", consuming_assistant()).unwrap();

        let outcome = scan_transcript(f.path(), false).unwrap();
        assert!(
            outcome.credited_since(Some(3)).is_empty(),
            "first evidence (L2) ≤ cursor (3) must suppress re-crediting"
        );
        assert!(
            outcome.credited_since(Some(1)).contains(id),
            "first evidence (L2) > cursor (1) must credit"
        );
    }

    #[test]
    fn credited_since_keeps_late_consumption_of_pre_cursor_retrieval() {
        use std::io::Write;
        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        // Banner L1 (before cursor), first consumption only at L3 (after
        // cursor 2). The retrieval being old must not hide the fresh
        // evidence — this is the edge the evidence-line rule preserves.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", banner_attachment(id)).unwrap();
        writeln!(f, "{}", unrelated_assistant()).unwrap();
        writeln!(f, "{}", consuming_assistant()).unwrap();

        let outcome = scan_transcript(f.path(), false).unwrap();
        assert!(
            outcome.credited_since(Some(2)).contains(id),
            "consumption evidence (L3) after the cursor (2) must credit even \
             though the retrieval (L1) predates it"
        );
    }

    #[test]
    fn credited_since_none_keeps_everything() {
        use std::io::Write;
        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", banner_attachment(id)).unwrap();
        writeln!(f, "{}", consuming_assistant()).unwrap();

        let outcome = scan_transcript(f.path(), false).unwrap();
        assert!(
            outcome.credited_since(None).contains(id),
            "no cursor (fail-open / first run) must credit everything"
        );
    }

    #[test]
    fn all_mode_evidence_is_retrieval_line() {
        use std::io::Write;
        let id = "mem_019e9999-aaaa-7bbb-8ccc-dddddddddddd";
        // `--all` credits on retrieval alone, so the evidence line is the
        // banner line itself.
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", banner_attachment(id)).unwrap();

        let outcome = scan_transcript(f.path(), true).unwrap();
        assert_eq!(outcome.credited.get(id).copied(), Some(1));
        assert_eq!(outcome.lines_scanned, 1);
    }

    #[test]
    fn roundtrip_assistant_id_citation_is_consumed() {
        use std::io::Write;
        // Citing the capsule id in assistant prose credits through the
        // n-gram path: a UUIDv7 contributes 5 hex tokens >= 4 chars, well
        // past HIT_THRESHOLD — no dedicated citation code needed.
        let id = "mem_01900000-1111-7222-8333-000000000abc";
        let citing = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": [{
                "type": "text",
                "text": format!("依据 {id} 的结论继续处理。"),
            }]},
        });
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", attachment_line(&rendered_index_banner(id))).unwrap();
        writeln!(f, "{citing}").unwrap();
        let consumed = scan_transcript(f.path(), false).unwrap();
        assert!(
            consumed.credited.contains_key(id),
            "assistant id citation must credit via fingerprint, got {:?}",
            consumed.credited
        );
    }
}
