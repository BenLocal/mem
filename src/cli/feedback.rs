//! `mem feedback-from-transcript` — scan a Claude Code transcript for
//! `mcp__mem__memory_search` calls, decide which retrieved memories were
//! actually consumed by the agent, and POST `/memories/feedback` for each.
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

/// Memory-search MCP tool names we care about. Both share the same
/// response shape (`domain::query::SearchMemoryResponse`).
const SEARCH_TOOL_NAMES: &[&str] = &[
    "mcp__mem__memory_search",
    "mcp__mem__memory_search_contextual",
    "mcp__mem__memory_bootstrap",
];

/// Snippet length used for the substring match. Long enough that a hit
/// is unlikely to be coincidental, short enough that minor tail edits
/// (whitespace, punctuation) don't break detection.
const SNIPPET_CHARS: usize = 40;

#[derive(Debug, Args)]
pub struct FeedbackFromTranscriptArgs {
    /// Path to the Claude Code transcript JSONL file.
    pub transcript_path: PathBuf,

    /// Tenant identifier (passed through to `/memories/feedback`).
    #[arg(long, default_value = "local")]
    pub tenant: String,

    /// One of: `useful`, `applies_here`, `outdated`, `does_not_apply_here`,
    /// `incorrect`. The default is the mildest positive signal.
    #[arg(long, default_value = "applies_here")]
    pub kind: String,

    /// Base URL for the local mem service.
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub base_url: String,

    /// Skip the "consumed" heuristic and signal every retrieved memory.
    /// Use when you want a uniform soft-positive after every search,
    /// not just consumed hits.
    #[arg(long)]
    pub all: bool,
}

pub async fn run(args: FeedbackFromTranscriptArgs) -> i32 {
    let consumed = match scan_transcript(&args) {
        Ok(set) => set,
        Err(e) => {
            eprintln!("scan transcript: {e}");
            return 1;
        }
    };
    if consumed.is_empty() {
        println!("feedback: no consumed memories detected");
        return 0;
    }
    let client = Client::new();
    let mut sent = 0usize;
    let mut failed = 0usize;
    for mid in &consumed {
        let body = serde_json::json!({
            "tenant": args.tenant,
            "memory_id": mid,
            "feedback_kind": args.kind,
        });
        match client
            .post(format!("{}/memories/feedback", args.base_url))
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
    println!(
        "feedback: kind={} sent={}/{} failed={}",
        args.kind,
        sent,
        consumed.len(),
        failed,
    );
    if failed > 0 && sent == 0 {
        1
    } else {
        0
    }
}

/// One pass over the JSONL transcript; returns the set of memory_ids that
/// were both retrieved by an `mcp__mem__memory_search*` call AND whose
/// `text` reappears in a subsequent assistant `text`/`thinking` block.
fn scan_transcript(args: &FeedbackFromTranscriptArgs) -> Result<HashSet<String>> {
    let file = File::open(&args.transcript_path)?;
    let reader = BufReader::new(file);

    // (memory_id, text-prefix snippet, line index where retrieval happened)
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
        let Some(content_array) = value["message"]["content"].as_array() else {
            continue;
        };

        for item in content_array {
            let block_type = item["type"].as_str().unwrap_or("");
            match block_type {
                "tool_use" => {
                    let name = item["name"].as_str().unwrap_or("");
                    if SEARCH_TOOL_NAMES.contains(&name) {
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
                    // SearchMemoryResponse — extract that string before
                    // re-parsing.
                    let inner = extract_tool_result_text(&item["content"]);
                    let Ok(resp) = serde_json::from_str::<Value>(&inner) else {
                        continue;
                    };
                    for section in ["directives", "relevant_facts", "reusable_patterns"] {
                        if let Some(arr) = resp[section].as_array() {
                            for entry in arr {
                                let mid = entry["memory_id"].as_str().unwrap_or("");
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
}
