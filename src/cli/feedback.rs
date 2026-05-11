//! `mem feedback-from-transcript` — scan a Claude Code transcript for
//! capsule-search MCP tool calls (see [`is_capsule_search_tool`] for
//! the accepted name shape), decide which retrieved capsules were
//! actually consumed by the agent, and POST
//! `/capability_capsules/feedback` for each.
//!
//! Heuristic for "consumed": the memory's `text` (returned in the
//! `directives` / `relevant_facts` / `reusable_patterns` sections of the
//! search response) starts with a 40-char prefix that appears verbatim in
//! a *subsequent* assistant `text`/`thinking` block. False negatives on
//! paraphrased usage are accepted; false positives on `applies_here`
//! (+0.05 confidence) are mild — design favors false-negatives so the
//! signal stays trustworthy.
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

/// Snippet length used for the substring match. Long enough that a hit
/// is unlikely to be coincidental, short enough that minor tail edits
/// (whitespace, punctuation) don't break detection.
const SNIPPET_CHARS: usize = 40;

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
/// capability_capsule_ids that were both retrieved by a capsule-search
/// MCP call (see [`is_capsule_search_tool`]) AND whose `text`
/// reappears in a subsequent assistant `text`/`thinking` block.
fn scan_transcript(args: &FeedbackFromTranscriptArgs) -> Result<HashSet<String>> {
    let file = File::open(&args.transcript_path)?;
    let reader = BufReader::new(file);

    // (capability_capsule_id, text-prefix snippet, line index where retrieval happened)
    let mut retrieved: Vec<(String, String, usize)> = Vec::new();
    // (line index, lower-cased assistant block text) — query corpus.
    let mut assistant_corpus: Vec<(usize, String)> = Vec::new();
    // tool_use_id → line index where the search call was issued.
    let mut search_calls: HashMap<String, usize> = HashMap::new();

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
                    }
                }
                "tool_result" => {
                    let tool_use_id = item["tool_use_id"].as_str().unwrap_or("");
                    if !search_calls.contains_key(tool_use_id) {
                        continue;
                    }
                    // Result content is either a string (older) or an
                    // array of `{type, text}` chunks. The MCP wrapper
                    // emits a single text chunk holding the JSON-encoded
                    // SearchCapabilityCapsuleResponse — extract that string before
                    // re-parsing.
                    let inner = extract_tool_result_text(&item["content"]);
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
                                let snippet = if args.all {
                                    String::new()
                                } else {
                                    snippet_lower(text)
                                };
                                retrieved.push((mid.to_string(), snippet, line_idx));
                            }
                        }
                    }
                }
                "text" if role == "assistant" => {
                    if let Some(t) = item["text"].as_str() {
                        assistant_corpus.push((line_idx, t.to_lowercase()));
                    }
                }
                "thinking" if role == "assistant" => {
                    if let Some(t) = item["thinking"].as_str() {
                        assistant_corpus.push((line_idx, t.to_lowercase()));
                    }
                }
                _ => {}
            }
        }
    }

    let mut used: HashSet<String> = HashSet::new();
    for (mid, snippet, ridx) in &retrieved {
        if used.contains(mid) {
            continue;
        }
        if args.all || snippet.is_empty() {
            // `--all` or no snippet to match against → accept.
            used.insert(mid.clone());
            continue;
        }
        let hit = assistant_corpus
            .iter()
            .filter(|(l, _)| *l > *ridx)
            .any(|(_, t)| t.contains(snippet));
        if hit {
            used.insert(mid.clone());
        }
    }
    Ok(used)
}

/// Lowercased, char-bounded prefix of `text` for substring matching.
/// Returns empty string for trivially short input — caller treats empty
/// as "no usable snippet, skip" unless `--all` is set.
fn snippet_lower(text: &str) -> String {
    let trimmed = text.trim();
    let prefix: String = trimmed.chars().take(SNIPPET_CHARS).collect();
    if prefix.chars().count() < 12 {
        // Too short to be a distinctive marker; treat as no signal.
        return String::new();
    }
    prefix.to_lowercase()
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
    fn snippet_skips_short_text() {
        assert_eq!(snippet_lower("short"), "");
        assert_eq!(snippet_lower("           "), "");
    }

    #[test]
    fn snippet_lowercases_and_truncates() {
        let s = snippet_lower("DuckDB is single-writer per file but supports MVCC concurrency");
        assert!(s.starts_with("duckdb is single-writer"));
        assert!(s.chars().count() <= SNIPPET_CHARS);
    }

    #[test]
    fn snippet_handles_unicode_boundaries() {
        // 50 Chinese chars: char-bounded slicing must not panic.
        let text: String = "中".repeat(50);
        let s = snippet_lower(&text);
        assert_eq!(s.chars().count(), SNIPPET_CHARS);
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
}
