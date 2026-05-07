use anyhow::Result;
use clap::Args;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<mem-save>(.*?)</mem-save>").unwrap());
static PATTERN_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?:我会记住：|关键发现：|重要：)(.+?)(?:\n|$)").unwrap(),
        Regex::new(r"(?:I'll remember:|Key insight:|Important:)(.+?)(?:\n|$)").unwrap(),
    ]
});

#[derive(Debug, Args)]
pub struct MineArgs {
    /// Path to Claude Code transcript file
    pub transcript_path: PathBuf,

    /// Tenant ID
    #[arg(long, default_value = "local")]
    pub tenant: String,

    /// Source agent name
    #[arg(long, default_value = "claude-code")]
    pub agent: String,

    /// Base URL for mem service
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub base_url: String,
}

pub struct ExtractedMemory {
    pub content: String,
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
}

/// One transcript block destined for `/transcripts/messages`.
///
/// Field semantics mirror `http::transcripts::IngestRequest`. The CLI
/// produces these from a single linear pass over the JSONL transcript so
/// the "memories" extract pipeline and the "transcript archive" pipeline
/// share a single I/O cost.
pub struct ArchivedBlock {
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
    pub block_index: usize,
    pub message_uuid: Option<String>,
    /// Lowercase: "user" | "assistant" | "system". Matches Task 2's
    /// `MessageRole` serde rename rule (`rename_all = "lowercase"`).
    pub role: String,
    /// snake_case: "text" | "tool_use" | "tool_result" | "thinking".
    /// Matches Task 2's `BlockType` serde rename rule (`rename_all =
    /// "snake_case"`).
    pub block_type: String,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
}

/// Backwards-compatible wrapper retained for the legacy unit tests in
/// `tests/cli_mine.rs`. New code should prefer [`parse_transcript_full`]
/// which also returns the per-block archive payload.
pub fn parse_transcript(path: &Path) -> Result<Vec<ExtractedMemory>> {
    parse_transcript_full(path).map(|(mems, _blocks)| mems)
}

/// Parses a Claude Code JSONL transcript into both extracted memories
/// (legacy `<mem-save>` / pattern matches) and a flat list of every
/// block ready to be POSTed to `/transcripts/messages`.
///
/// Only `assistant` `text` blocks feed the memory extractor — that
/// preserves the pre-existing extraction behavior. Every block of every
/// message (user / assistant / system, all four block types) is added
/// to the archive output.
pub fn parse_transcript_full(path: &Path) -> Result<(Vec<ExtractedMemory>, Vec<ArchivedBlock>)> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut memories = Vec::new();
    let mut blocks = Vec::new();

    for (line_idx, line) in reader.lines().enumerate() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Claude Code transcripts use `type` as the message-envelope
        // discriminator. Only `user`, `assistant`, `system` carry blocks
        // we want to archive; meta lines (e.g. "custom-title") are
        // skipped.
        let role = match value["type"].as_str() {
            Some(r @ ("user" | "assistant" | "system")) => r,
            _ => continue,
        };

        let session_id = value["sessionId"].as_str().unwrap_or("").to_string();
        let timestamp = value["timestamp"].as_str().unwrap_or("").to_string();
        let message_uuid = value["uuid"].as_str().map(|s| s.to_string());

        let Some(content_array) = value["message"]["content"].as_array() else {
            continue;
        };

        let line_number = line_idx + 1;

        for (block_idx, item) in content_array.iter().enumerate() {
            let block_type = item["type"].as_str().unwrap_or("");

            // Memory extraction (legacy path) only runs on assistant
            // text blocks — same condition the original code enforced.
            if role == "assistant" && block_type == "text" {
                if let Some(text) = item["text"].as_str() {
                    if let Some(extracted) = extract_memory(text) {
                        memories.push(ExtractedMemory {
                            content: extracted,
                            session_id: session_id.clone(),
                            timestamp: timestamp.clone(),
                            line_number,
                        });
                    }
                }
            }

            // Archive every recognized block. Unknown types are skipped
            // (not an error — Claude Code may add new block kinds and we
            // shouldn't fail mining a transcript over them).
            let archived = match block_type {
                "text" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "text".to_string(),
                    content: item["text"].as_str().unwrap_or("").to_string(),
                    tool_name: None,
                    tool_use_id: None,
                }),
                "thinking" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "thinking".to_string(),
                    content: item["thinking"].as_str().unwrap_or("").to_string(),
                    tool_name: None,
                    tool_use_id: None,
                }),
                "tool_use" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "tool_use".to_string(),
                    content: serde_json::to_string(&item["input"]).unwrap_or_default(),
                    tool_name: item["name"].as_str().map(|s| s.to_string()),
                    tool_use_id: item["id"].as_str().map(|s| s.to_string()),
                }),
                "tool_result" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "tool_result".to_string(),
                    content: extract_tool_result_content(&item["content"]),
                    tool_name: None,
                    tool_use_id: item["tool_use_id"].as_str().map(|s| s.to_string()),
                }),
                _ => {
                    // Unknown block type: silently drop. Logging would
                    // spam stderr on transcripts that legitimately use
                    // novel kinds.
                    None
                }
            };
            if let Some(b) = archived {
                blocks.push(b);
            }
        }
    }

    Ok((memories, blocks))
}

/// `tool_result.content` in Claude Code transcripts comes in two shapes:
/// a plain string (older runs) or an array of `{type, text}` objects
/// (newer multi-part results). Concatenate text parts when an array is
/// supplied; pass strings through verbatim. Anything else stringifies
/// the whole JSON value as a fallback so no information is lost.
fn extract_tool_result_content(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(arr) = value.as_array() {
        let mut parts = Vec::with_capacity(arr.len());
        for item in arr {
            if let Some(t) = item["text"].as_str() {
                parts.push(t.to_string());
            } else if let Some(s) = item.as_str() {
                parts.push(s.to_string());
            }
        }
        return parts.join("\n");
    }
    if value.is_null() {
        return String::new();
    }
    serde_json::to_string(value).unwrap_or_default()
}

fn extract_memory(text: &str) -> Option<String> {
    let candidate = if let Some(cap) = TAG_RE.captures(text) {
        cap[1].trim().to_string()
    } else {
        let mut found: Option<String> = None;
        for re in PATTERN_RES.iter() {
            if let Some(cap) = re.captures(text) {
                found = Some(cap[1].trim().to_string());
                break;
            }
        }
        found?
    };

    if looks_like_real_memory(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

/// Reject obvious garbage extractions (e.g. `<mem-save>...</mem-save>`
/// that the lazy `(.*?)` group ate when assistant text *described* the
/// `<mem-save>` tag rather than using it). Two real-world misfires this
/// guard catches:
///   - mem_019e0054-6c48 = `"这种显式片段）`  (7-char partial sentence)
///   - mem_019e0054-6c99 = `...`              (3-char placeholder)
///
/// Heuristic: minimum 12 chars AND at least 4 alphanumeric / CJK chars
/// (Unicode `is_alphanumeric` covers Chinese, Japanese, Korean, etc.).
/// Calibrated against observed garbage; legitimate one-liners like
/// `use bun.lockb` (13 chars) still pass.
fn looks_like_real_memory(s: &str) -> bool {
    const MIN_LEN: usize = 12;
    const MIN_SUBSTANTIVE: usize = 4;
    if s.chars().count() < MIN_LEN {
        return false;
    }
    let substantive = s.chars().filter(|c| c.is_alphanumeric()).count();
    substantive >= MIN_SUBSTANTIVE
}

pub async fn run(args: MineArgs) -> i32 {
    let (memories, blocks) = match parse_transcript_full(&args.transcript_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to parse transcript: {}", e);
            return 1;
        }
    };

    let client = reqwest::Client::new();
    let mut mem_ok: u32 = 0;
    let mut mem_fail: u32 = 0;
    let mut block_ok: u32 = 0;
    let mut block_fail: u32 = 0;

    // Legacy memories pipeline — unchanged from the original
    // implementation. The idempotency_key shape and 409-as-success
    // handling are explicitly preserved.
    for memory in memories {
        let idempotency_key = format!("{}:{}", args.transcript_path.display(), memory.line_number);

        let payload = serde_json::json!({
            "tenant": args.tenant,
            "memory_type": "experience",
            "content": memory.content,
            "scope": "global",
            "source_agent": args.agent,
            "idempotency_key": idempotency_key,
            "write_mode": "auto",
        });

        match client
            .post(format!("{}/memories", args.base_url))
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status() == 409 => {
                mem_ok += 1;
            }
            Ok(resp) => {
                eprintln!("Failed to save memory: {}", resp.status());
                mem_fail += 1;
            }
            Err(e) => {
                eprintln!("Request error: {}", e);
                mem_fail += 1;
            }
        }
    }

    // New transcript-archive pipeline. Block-level idempotency is
    // enforced server-side by the `(transcript_path, line_number,
    // block_index)` unique constraint; a duplicate insert still returns
    // 200 OK (with a freshly minted message_block_id that the server
    // discards via `INSERT ... ON CONFLICT DO NOTHING`), so we only
    // need to count 2xx responses.
    for b in blocks {
        let embed_eligible = matches!(b.block_type.as_str(), "text" | "thinking");
        let payload = serde_json::json!({
            "session_id": b.session_id,
            "tenant": args.tenant,
            "caller_agent": args.agent,
            "transcript_path": args.transcript_path.display().to_string(),
            "line_number": b.line_number,
            "block_index": b.block_index,
            "message_uuid": b.message_uuid,
            "role": b.role,
            "block_type": b.block_type,
            "content": b.content,
            "tool_name": b.tool_name,
            "tool_use_id": b.tool_use_id,
            "embed_eligible": embed_eligible,
            "created_at": b.timestamp,
        });

        match client
            .post(format!("{}/transcripts/messages", args.base_url))
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                block_ok += 1;
            }
            Ok(resp) => {
                eprintln!("Failed to archive block: {}", resp.status());
                block_fail += 1;
            }
            Err(e) => {
                eprintln!("Block POST error: {}", e);
                block_fail += 1;
            }
        }
    }

    // Counts reflect what the CLI *sent* (HTTP 2xx), not what the server
    // actually inserted. The server deduplicates by (transcript_path,
    // line_number, block_index) for transcript blocks and by
    // idempotency_key for memories, so re-running mine on the same file
    // returns 2xx without double-inserting. Use `mem-cli` / DuckDB queries
    // to count rows on disk if you need exact insert deltas.
    println!(
        "Mined: memories sent={}/{} blocks sent={}/{} (server-side dedup applied)",
        mem_ok,
        mem_ok + mem_fail,
        block_ok,
        block_ok + block_fail
    );
    if mem_fail > 0 || block_fail > 0 {
        1
    } else {
        0
    }
}

// `mod extract_tests` lives at file end so clippy::items_after_test_module
// doesn't fire — the lint requires no real items appear after a test
// module.
#[cfg(test)]
mod extract_tests {
    use super::*;

    #[test]
    fn rejects_three_dots() {
        assert!(extract_memory("<mem-save>...</mem-save>").is_none());
    }

    #[test]
    fn rejects_short_partial_fragment() {
        // observed in production: assistant explained the tag and got a
        // trailing fragment captured.
        assert!(extract_memory("<mem-save>\"这种显式片段）</mem-save>").is_none());
    }

    #[test]
    fn keeps_legit_short_memory() {
        let s = "<mem-save>use rustls for TLS not native-tls</mem-save>";
        assert_eq!(
            extract_memory(s).as_deref(),
            Some("use rustls for TLS not native-tls"),
        );
    }

    #[test]
    fn keeps_chinese_memory() {
        let s = "<mem-save>记住：用 tokio 而不是 std::thread</mem-save>";
        assert!(extract_memory(s).is_some());
    }

    #[test]
    fn keeps_pattern_match() {
        let s = "I'll remember: use bun for fast installs";
        assert_eq!(
            extract_memory(s).as_deref(),
            Some("use bun for fast installs")
        );
    }

    #[test]
    fn rejects_pattern_match_too_short() {
        let s = "I'll remember: ok";
        assert!(extract_memory(s).is_none());
    }
}
