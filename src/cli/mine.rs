use anyhow::Result;
use clap::{Args, ValueEnum};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::common::RemoteArgs;
use super::feedback::{self, FeedbackCounts, FeedbackFromTranscriptArgs};

// Only the explicit `<mem-save>...</mem-save>` tag triggers extraction.
//
// Earlier (pre-2026-05-08) the extractor also matched prose cues like
// `重要：` / `Important:` / `我会记住：` / `Key insight:` / `关键发现：` /
// `I'll remember:`. Those produced too many false positives in agent
// transcripts that *discussed* the cues — e.g. "提取器只认 `<mem-save>` /
// `重要：` 等显式 cue" matched its own meta-mention and saved the trailing
// text as a memory (`mem_019e061e-...`, archived). Agents that want to
// persist a fact must use `<mem-save>...</mem-save>` (or call
// `capability_capsule_ingest` directly via MCP).
static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<mem-save>(.*?)</mem-save>").unwrap());

/// Output shape for `mem mine`. `human` is the legacy stdout summary
/// agents read interactively; `hook-stop` / `hook-precompact` print a
/// JSON line matching the Claude Code / Codex hook envelope shape, so
/// shell hook scripts can `exec` `mem mine` directly without sed/jq
/// post-processing. When the mine pass yields no rows, hook variants
/// emit `{}` (skip-the-event sentinel).
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum HookFormat {
    /// Print a one-line `Mined: capsules sent=…` summary to stdout.
    Human,
    /// Print a Stop-event envelope: `{"systemMessage":"✦ mem · …"}`.
    HookStop,
    /// Print a PreCompact-event envelope: `{"systemMessage":"✦ mem · pre-compact · …"}`.
    HookPrecompact,
}

#[derive(Debug, Args)]
pub struct MineArgs {
    /// Path to Claude Code transcript file
    pub transcript_path: PathBuf,

    #[command(flatten)]
    pub remote: RemoteArgs,

    /// Source agent name
    #[arg(long, default_value = "claude-code")]
    pub agent: String,

    /// After mining, also POST `applies_here` feedback for capsules
    /// whose retrieved `text` reappeared in later assistant blocks.
    /// Equivalent to running `mem feedback-from-transcript` as a
    /// follow-up pass; folded into the same command so hook scripts
    /// don't have to chain two timeouts + parse two outputs.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub with_feedback: bool,

    /// Soft wall-clock cap (seconds) for the mine pass. 0 = no cap.
    /// On timeout, mine yields whatever rows it managed to send and
    /// downstream output reflects partial counts.
    #[arg(long, default_value = "0")]
    pub mine_timeout_secs: u64,

    /// Soft cap (seconds) for the feedback pass when `--with-feedback`.
    /// 0 = no cap.
    #[arg(long, default_value = "0")]
    pub feedback_timeout_secs: u64,

    /// Output shape. `human` (default) prints the legacy summary line.
    /// `hook-stop` / `hook-precompact` print a JSON envelope ready for
    /// the agent runtime's hook channel.
    #[arg(long, value_enum, default_value_t = HookFormat::Human)]
    pub format: HookFormat,
}

/// Chunk size used by `mine` when fanning out to the `/batch` endpoints.
/// Sized so that one chunk fits comfortably in a single Lance write
/// while keeping HTTP body sizes reasonable.
const MINE_BATCH_CHUNK: usize = 100;

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
/// the "capability_capsules" extract pipeline and the "transcript archive" pipeline
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
    /// JSON-encoded envelope/per-block metadata (cwd, git_branch,
    /// parent_uuid, is_error). `None` when no metadata fields were
    /// present on the source JSONL line.
    pub meta_json: Option<String>,
}

/// Build the `/transcripts/messages/batch` JSON payload for one archived
/// block. The field shape mirrors `http::transcripts::IngestRequest` and is
/// the **single source of truth** for that mapping, shared by `mem mine`
/// (dual-sink: memories + archive) and `mem import` (archive-only). Change
/// it here, not in two places.
///
/// `embed_eligible` follows mine's rule: only `text` / `thinking` blocks
/// carry semantically useful prose; `tool_use` / `tool_result` are skipped
/// so the transcript embedding worker doesn't burn cycles on tool JSON.
pub fn block_to_payload(
    b: &ArchivedBlock,
    transcript_path: &str,
    tenant: &str,
    agent: &str,
) -> serde_json::Value {
    let embed_eligible = matches!(b.block_type.as_str(), "text" | "thinking");
    serde_json::json!({
        "session_id": b.session_id,
        "tenant": tenant,
        "caller_agent": agent,
        "transcript_path": transcript_path,
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
        "meta_json": b.meta_json,
    })
}

/// Soft cap on serialized JSON bytes per batch POST. `mem serve` caps request
/// bodies at axum's 2 MiB default, so a fixed `MINE_BATCH_CHUNK`-block batch
/// from a tool-result-heavy session can exceed it and 413. We bound each batch
/// by BOTH the block count and this byte budget (whichever trips first), with
/// headroom under 2 MiB. A single block larger than this is still sent alone
/// (it only 413s if it alone exceeds the server's hard 2 MiB limit).
const MINE_BATCH_MAX_BYTES: usize = 1_500_000;

/// Partition payloads into batch index-ranges `[start, end)` bounded by both
/// `max_count` blocks and `max_bytes` of serialized JSON, whichever trips
/// first. Pure (no I/O) so the batching logic is unit-testable. `sizes[i]` is
/// the serialized byte length of payload `i`. A single oversized payload lands
/// alone in its own batch (never merged, never dropped).
fn plan_block_batches(sizes: &[usize], max_count: usize, max_bytes: usize) -> Vec<(usize, usize)> {
    let mut batches = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0usize;
    for (i, &sz) in sizes.iter().enumerate() {
        if i > start && (i - start >= max_count || bytes + sz > max_bytes) {
            batches.push((start, i));
            start = i;
            bytes = 0;
        }
        bytes += sz;
    }
    if start < sizes.len() {
        batches.push((start, sizes.len()));
    }
    batches
}

/// Size-aware batched POST of transcript-block payloads to
/// `/transcripts/messages/batch`, shared by `mine` and `import`. Returns
/// `(ok, fail)` — the number of blocks the server accepted (HTTP 2xx) vs.
/// rejected. Block-level idempotency is enforced server-side by the
/// `(transcript_path, line_number, block_index)` triple, so re-sending
/// already-present rows returns 2xx without double-inserting (counted as `ok`,
/// mirroring the single-row endpoint). Batches are bounded by both
/// `MINE_BATCH_CHUNK` blocks and `MINE_BATCH_MAX_BYTES` so a heavy session's
/// large blocks don't overflow the server's request-body limit.
pub async fn post_block_payloads(
    client: &reqwest::Client,
    base_url: &str,
    payloads: &[serde_json::Value],
) -> (u32, u32) {
    let sizes: Vec<usize> = payloads
        .iter()
        .map(|p| serde_json::to_vec(p).map(|v| v.len()).unwrap_or(0))
        .collect();
    let (mut ok, mut fail) = (0u32, 0u32);
    for (start, end) in plan_block_batches(&sizes, MINE_BATCH_CHUNK, MINE_BATCH_MAX_BYTES) {
        let (o, f) = send_block_batch(client, base_url, &payloads[start..end]).await;
        ok += o;
        fail += f;
    }
    (ok, fail)
}

/// POST one batch of block payloads; returns `(ok, fail)` block counts for it.
async fn send_block_batch(
    client: &reqwest::Client,
    base_url: &str,
    batch: &[serde_json::Value],
) -> (u32, u32) {
    match client
        .post(format!("{}/transcripts/messages/batch", base_url))
        .json(batch)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => (batch.len() as u32, 0),
        Ok(resp) => {
            eprintln!("Failed to archive block batch: {}", resp.status());
            (0, batch.len() as u32)
        }
        Err(e) => {
            eprintln!("Block batch request error: {}", e);
            (0, batch.len() as u32)
        }
    }
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

        // Envelope-level metadata (cwd, git_branch, parent_uuid).
        // Repeated identically on every block of this message — the
        // redundancy is the price of a flat row schema, traded against
        // not storing a parent table for envelope-level fields.
        // Claude Code uses both `gitBranch` and `git_branch` over time
        // — accept either.
        let envelope_cwd = value["cwd"].as_str();
        let envelope_branch = value["gitBranch"]
            .as_str()
            .or_else(|| value["git_branch"].as_str());
        let envelope_parent = value["parentUuid"]
            .as_str()
            .or_else(|| value["parent_uuid"].as_str());

        // Claude Code emits user messages in two shapes: an array of
        // structured blocks (when the message has tool-uses or
        // attachments) and a plain string (the common case for raw
        // user-typed text). Treating only the array form drops the
        // bulk of user input — synthesize a single text block when the
        // payload is a string so both shapes archive identically.
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

        let line_number = line_idx + 1;

        for (block_idx, item) in content_array.iter().enumerate() {
            let block_type = item["type"].as_str().unwrap_or("");

            // Compose meta_json: always include envelope fields when
            // present; tool_result blocks additionally carry is_error.
            let block_is_error = if block_type == "tool_result" {
                item["is_error"].as_bool()
            } else {
                None
            };
            let meta_json = build_meta_json(
                envelope_cwd,
                envelope_branch,
                envelope_parent,
                block_is_error,
            );

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
                    meta_json: meta_json.clone(),
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
                    meta_json: meta_json.clone(),
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
                    meta_json: meta_json.clone(),
                }),
                "tool_result" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "tool_result".to_string(),
                    content: serialize_tool_result_content(&item["content"]),
                    tool_name: None,
                    tool_use_id: item["tool_use_id"].as_str().map(|s| s.to_string()),
                    meta_json: meta_json.clone(),
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
/// a plain string (older runs) or an array of structured items like
/// `{"type": "text", "text": ...}` / `{"type": "image", ...}` (newer
/// multi-part results). Preserve both shapes as-is for downstream
/// compression / slot extraction:
///
///   - Strings → returned verbatim (no JSON quoting), so simple
///     consumers (e.g. wake-up text rendering) can use the column
///     directly without parsing.
///   - Arrays / objects → serialized to JSON so structure (text vs
///     image, multiple parts, embedded metadata) survives.
///   - Null → empty string.
///
/// **Design note**: this changed in 2026-05 from `\n`-joining text
/// parts (which lost multi-part structure + dropped non-text parts)
/// to verbatim JSON preservation. Slot-based compression
/// (`commands_run` / `errors_encountered`) needs the structure.
fn serialize_tool_result_content(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if value.is_null() {
        return String::new();
    }
    serde_json::to_string(value).unwrap_or_default()
}

/// Build a JSON-encoded metadata blob from envelope + per-block fields,
/// returning `None` when every input is empty so callers can store NULL
/// instead of `"{}"`. Field names use snake_case to match the
/// `MessageRole` / `BlockType` serde rename rules.
fn build_meta_json(
    cwd: Option<&str>,
    git_branch: Option<&str>,
    parent_uuid: Option<&str>,
    is_error: Option<bool>,
) -> Option<String> {
    let mut map = serde_json::Map::new();
    if let Some(s) = cwd.filter(|s| !s.is_empty()) {
        map.insert("cwd".into(), Value::String(s.to_string()));
    }
    if let Some(s) = git_branch.filter(|s| !s.is_empty()) {
        map.insert("git_branch".into(), Value::String(s.to_string()));
    }
    if let Some(s) = parent_uuid.filter(|s| !s.is_empty()) {
        map.insert("parent_uuid".into(), Value::String(s.to_string()));
    }
    if let Some(b) = is_error {
        map.insert("is_error".into(), Value::Bool(b));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map).to_string())
    }
}

fn extract_memory(text: &str) -> Option<String> {
    let cap = TAG_RE.captures(text)?;
    let candidate = cap[1].trim().to_string();
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

/// Typed result of a mine pass. Fields are wire-counted (HTTP 2xx) — the
/// server applies dedup by `idempotency_key` (capsules) and
/// `(transcript_path, line_number, block_index)` (transcript blocks),
/// so a re-run on the same file produces the same `_ok` totals without
/// double-inserting.
#[derive(Debug, Clone, Default)]
pub struct MineCounts {
    pub mem_ok: u32,
    pub mem_fail: u32,
    pub block_ok: u32,
    pub block_fail: u32,
}

impl MineCounts {
    pub fn capsules_total(&self) -> u32 {
        self.mem_ok + self.mem_fail
    }
    pub fn blocks_total(&self) -> u32 {
        self.block_ok + self.block_fail
    }
    pub fn failed(&self) -> bool {
        self.mem_fail > 0 || self.block_fail > 0
    }
}

pub async fn run(args: MineArgs) -> i32 {
    let format = args.format;
    let with_feedback = args.with_feedback;
    let feedback_timeout = args.feedback_timeout_secs;
    let mine_timeout = args.mine_timeout_secs;
    let transcript_path = args.transcript_path.clone();
    let remote = args.remote.clone();

    let mine_result = with_optional_timeout(mine_timeout, run_with_counts(args)).await;

    let counts = match mine_result {
        Some(Ok(counts)) => counts,
        Some(Err(e)) => {
            // Transcript parse error — hard input failure. Preserve the
            // legacy non-zero exit; hook variants additionally emit `{}`
            // so the runtime never breaks on a malformed file.
            eprintln!("Failed to parse transcript: {}", e);
            if !matches!(format, HookFormat::Human) {
                // Skip-event sentinel; trailing newline matches shell
                // heredoc convention for hook channels.
                println!("{{}}");
            }
            return 1;
        }
        None => {
            eprintln!("mine pass exceeded --mine-timeout-secs={}", mine_timeout);
            if !matches!(format, HookFormat::Human) {
                // Skip-event sentinel; trailing newline matches shell
                // heredoc convention for hook channels.
                println!("{{}}");
            }
            return 1;
        }
    };

    let feedback_counts = if with_feedback {
        run_feedback_inline(&transcript_path, &remote, feedback_timeout).await
    } else {
        None
    };

    match format {
        HookFormat::Human => {
            println!(
                "Mined: capsules sent={}/{} blocks sent={}/{} (server-side dedup applied)",
                counts.mem_ok,
                counts.capsules_total(),
                counts.block_ok,
                counts.blocks_total(),
            );
            if counts.failed() {
                1
            } else {
                0
            }
        }
        HookFormat::HookStop | HookFormat::HookPrecompact => {
            let is_precompact = matches!(format, HookFormat::HookPrecompact);
            let envelope = render_hook_envelope(&counts, feedback_counts.as_ref(), is_precompact);
            println!("{envelope}");
            // Hook channel must never error out — partial mine still
            // emits a useful envelope; the runtime treats non-zero exit
            // as a hook failure.
            0
        }
    }
}

/// Build a `{"systemMessage": "✦ mem · …"}` envelope. Returns the
/// skip-event sentinel `{}` when both capsules and blocks are zero
/// (typical "transcript had nothing new" path on a re-run).
fn render_hook_envelope(
    mine: &MineCounts,
    feedback: Option<&FeedbackCounts>,
    is_precompact: bool,
) -> Value {
    if mine.capsules_total() == 0 && mine.blocks_total() == 0 {
        return Value::Object(Default::default());
    }
    let prefix = if is_precompact {
        "✦ mem · pre-compact · "
    } else {
        "✦ mem · "
    };
    let suffix = if is_precompact {
        " archived"
    } else {
        " woven into the archive"
    };
    let feedback_sent = feedback.map(|f| f.sent).unwrap_or(0);
    // Drop the capsules segment when no `<mem-save>` cues were extracted —
    // sessions that persist via MCP `capability_capsule_ingest` (or the
    // `propose_*` flows) never produce capsule rows from the mine pass, so
    // a permanent `0/0 capsules + …` prefix is noise.
    let capsules_segment = if mine.capsules_total() > 0 {
        format!("{}/{} capsules + ", mine.mem_ok, mine.capsules_total(),)
    } else {
        String::new()
    };
    let msg = if feedback_sent > 0 {
        format!(
            "{prefix}{capsules_segment}{}/{} blocks · {} feedback applied",
            mine.block_ok,
            mine.blocks_total(),
            feedback_sent,
        )
    } else {
        format!(
            "{prefix}{capsules_segment}{}/{} blocks{}",
            mine.block_ok,
            mine.blocks_total(),
            suffix,
        )
    };
    serde_json::json!({ "systemMessage": msg })
}

/// In-process feedback pass. Failures (HTTP, transcript scan) resolve
/// to `None` — feedback is best-effort for the hook flow; we never
/// surface it as an envelope-breaking error.
async fn run_feedback_inline(
    transcript_path: &Path,
    remote: &RemoteArgs,
    timeout_secs: u64,
) -> Option<FeedbackCounts> {
    let args = FeedbackFromTranscriptArgs {
        transcript_path: transcript_path.to_path_buf(),
        remote: remote.clone(),
        kind: "applies_here".to_string(),
        all: false,
    };
    with_optional_timeout(timeout_secs, feedback::run_with_counts(args))
        .await
        .and_then(|r| r.ok())
}

/// Wrap `fut` in a `tokio::time::timeout` when `timeout_secs > 0`.
/// Zero disables the cap. A timeout resolves to `None`.
async fn with_optional_timeout<F, T>(timeout_secs: u64, fut: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    if timeout_secs == 0 {
        return Some(fut.await);
    }
    tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .ok()
}

/// Same as [`run`] but returns counts directly to in-process callers
/// (e.g. the `mem hook` handlers) instead of printing + returning an
/// exit code. Errors only surface for unrecoverable input failures
/// (transcript parse). Per-row HTTP failures are counted in
/// `mem_fail` / `block_fail`, not propagated.
pub async fn run_with_counts(args: MineArgs) -> anyhow::Result<MineCounts> {
    let (memories, blocks) = parse_transcript_full(&args.transcript_path)?;

    let client = reqwest::Client::new();
    let mut mem_ok: u32 = 0;
    let mut mem_fail: u32 = 0;
    let mut block_ok: u32 = 0;
    let mut block_fail: u32 = 0;

    // v3 #32 fast-skip: query the server's per-transcript cursor; if
    // present, drop memories/blocks whose line_number ≤ cursor. Pure
    // perf — server-side dedup (idempotency_key on capsules; (path,
    // line, block_index) on transcript blocks) still catches anything
    // we choose to ship anyway, so a 404 / network failure on cursor
    // read just degrades to the legacy "re-mine + re-dedup" path.
    let transcript_path_str = args.transcript_path.display().to_string();
    let cursor_line: Option<i64> = match client
        .get(format!("{}/mine/cursors", args.remote.base_url))
        .query(&[("transcript_path", transcript_path_str.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("last_line_number").and_then(|n| n.as_i64())),
        _ => None,
    };
    let (memories, blocks) = if let Some(c) = cursor_line {
        let before_mem = memories.len();
        let before_blk = blocks.len();
        let memories: Vec<_> = memories
            .into_iter()
            .filter(|m| (m.line_number as i64) > c)
            .collect();
        let blocks: Vec<_> = blocks
            .into_iter()
            .filter(|b| (b.line_number as i64) > c)
            .collect();
        eprintln!(
            "mine: cursor at line {c}; skipped {} capsules + {} blocks already mined",
            before_mem - memories.len(),
            before_blk - blocks.len(),
        );
        (memories, blocks)
    } else {
        (memories, blocks)
    };

    // Capture max line_number BEFORE the moves below so we can update
    // the cursor after both batches succeed.
    let max_line: Option<i64> = memories
        .iter()
        .map(|m| m.line_number as i64)
        .chain(blocks.iter().map(|b| b.line_number as i64))
        .max();

    // ── Capsules: chunked POST to /capability_capsules/batch.
    //
    // Each request body is the same shape as the single endpoint plus
    // the array wrapper; the server flushes one Lance write per chunk
    // (vs. per row). 201 = all-ok, 207 = mixed; in
    // both cases we parse the per-item `result` field. Any pre-existing
    // capsule (idempotency_key match) returns `result: ok` because the
    // service treats dedup-hit-with-existing-row as success.
    let capsule_payloads: Vec<serde_json::Value> = memories
        .into_iter()
        .map(|memory| {
            let idempotency_key =
                format!("{}:{}", args.transcript_path.display(), memory.line_number);
            serde_json::json!({
                "tenant": args.remote.tenant,
                "capability_capsule_type": "experience",
                "content": memory.content,
                "scope": "global",
                "source_agent": args.agent,
                "idempotency_key": idempotency_key,
                "write_mode": "auto",
            })
        })
        .collect();

    for chunk in capsule_payloads.chunks(MINE_BATCH_CHUNK) {
        match client
            .post(format!(
                "{}/capability_capsules/batch",
                args.remote.base_url
            ))
            .json(chunk)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status() == 207 => {
                let v: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Capsule batch parse error: {}", e);
                        mem_fail += chunk.len() as u32;
                        continue;
                    }
                };
                let items = v.get("items").and_then(|x| x.as_array());
                match items {
                    Some(arr) => {
                        for item in arr {
                            let kind = item.get("result").and_then(|x| x.as_str()).unwrap_or("");
                            if kind == "ok" {
                                mem_ok += 1;
                            } else {
                                let err = item
                                    .get("error")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("unknown");
                                eprintln!("Capsule item error: {}", err);
                                mem_fail += 1;
                            }
                        }
                    }
                    None => {
                        eprintln!("Capsule batch: missing items array");
                        mem_fail += chunk.len() as u32;
                    }
                }
            }
            Ok(resp) => {
                eprintln!("Failed to save capsule batch: {}", resp.status());
                mem_fail += chunk.len() as u32;
            }
            Err(e) => {
                eprintln!("Capsule batch request error: {}", e);
                mem_fail += chunk.len() as u32;
            }
        }
    }

    // ── Transcript blocks: chunked POST to /transcripts/messages/batch.
    //
    // Block-level idempotency is enforced server-side by the
    // `(transcript_path, line_number, block_index)` triple; the batch
    // endpoint silently skips already-present rows and reports the
    // landed count via `inserted`. We count every block we successfully
    // sent (regardless of dedup status) as `block_ok` to mirror the
    // single-row endpoint's "2xx → ok" semantic.
    let block_payloads: Vec<serde_json::Value> = blocks
        .iter()
        .map(|b| block_to_payload(b, &transcript_path_str, &args.remote.tenant, &args.agent))
        .collect();

    let (block_sent_ok, block_sent_fail) =
        post_block_payloads(&client, &args.remote.base_url, &block_payloads).await;
    block_ok += block_sent_ok;
    block_fail += block_sent_fail;

    // Counts reflect what the CLI *sent* (HTTP 2xx), not what the server
    // actually inserted. The server deduplicates by (transcript_path,
    // line_number, block_index) for transcript blocks and by
    // idempotency_key for memories, so re-running mine on the same file
    // returns 2xx without double-inserting. Query the HTTP read endpoints
    // (e.g. `GET /capability_capsules/stats`) or `mem-cli` to count rows
    // if you need exact insert deltas.

    // v3 #32 cursor write — advance the high-water mark only when
    // every batch this run shipped landed cleanly (no per-item failures
    // anywhere). Partial-failure runs leave the cursor untouched so the
    // next mine re-processes the failed lines. Best-effort — a cursor
    // write failure doesn't fail the mine (server-side dedup still
    // protects future runs from double-write).
    let all_clean = mem_fail == 0 && block_fail == 0;
    if all_clean {
        if let Some(line) = max_line {
            let _ = client
                .post(format!("{}/mine/cursors", args.remote.base_url))
                .json(&serde_json::json!({
                    "transcript_path": transcript_path_str,
                    "last_line_number": line,
                }))
                .send()
                .await;
        }
    }

    Ok(MineCounts {
        mem_ok,
        mem_fail,
        block_ok,
        block_fail,
    })
}

// `mod extract_tests` lives at file end so clippy::items_after_test_module
// doesn't fire — the lint requires no real items appear after a test
// module.
#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn splits_on_count_cap() {
        // 5 tiny payloads, count cap 2 → batches of 2,2,1.
        let sizes = vec![10, 10, 10, 10, 10];
        assert_eq!(
            plan_block_batches(&sizes, 2, 1_000_000),
            vec![(0, 2), (2, 4), (4, 5)]
        );
    }

    #[test]
    fn splits_on_byte_cap_before_count() {
        // Byte cap 100 trips before the count cap of 100: 60 + 60 > 100, so
        // each 60-byte payload starts a new batch.
        let sizes = vec![60, 60, 60];
        assert_eq!(
            plan_block_batches(&sizes, 100, 100),
            vec![(0, 1), (1, 2), (2, 3)]
        );
    }

    #[test]
    fn oversized_single_payload_lands_alone() {
        // A payload bigger than the byte budget is never merged or dropped —
        // it gets its own batch, flanked by normally-batched neighbors.
        let sizes = vec![10, 5_000_000, 10];
        assert_eq!(
            plan_block_batches(&sizes, 100, 1_500_000),
            vec![(0, 1), (1, 2), (2, 3)]
        );
    }

    #[test]
    fn packs_until_either_cap() {
        // 40-byte payloads, byte cap 100 → 2 per batch (40+40=80 ok, +40=120 > 100).
        let sizes = vec![40, 40, 40, 40, 40];
        assert_eq!(
            plan_block_batches(&sizes, 100, 100),
            vec![(0, 2), (2, 4), (4, 5)]
        );
    }

    #[test]
    fn empty_input_yields_no_batches() {
        assert!(plan_block_batches(&[], 100, 1_500_000).is_empty());
    }
}

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
    fn rejects_prose_cue_outside_mem_save_tag() {
        // Prose cues like "I'll remember:" / "重要：" used to also trigger
        // extraction; that path was removed after a recursive false-
        // positive bug (`mem_019e061e-...`). Ensure we don't regress.
        assert!(extract_memory("I'll remember: use bun for fast installs").is_none());
        assert!(extract_memory("重要：用 tokio 而不是 std::thread").is_none());
        assert!(extract_memory("Key insight: this matters").is_none());
        assert!(extract_memory("我会记住：保持简单").is_none());
    }

    #[test]
    fn hook_envelope_skips_when_zero_counts() {
        let mine = MineCounts::default();
        let v = render_hook_envelope(&mine, None, false);
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn hook_envelope_stop_uses_woven_into_archive() {
        let mine = MineCounts {
            mem_ok: 3,
            mem_fail: 0,
            block_ok: 12,
            block_fail: 0,
        };
        let v = render_hook_envelope(&mine, None, false);
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(msg.starts_with("✦ mem · "), "got {msg}");
        assert!(msg.contains("3/3 capsules + 12/12 blocks"));
        assert!(msg.contains("woven into the archive"));
    }

    #[test]
    fn hook_envelope_drops_capsules_segment_when_zero() {
        let mine = MineCounts {
            mem_ok: 0,
            mem_fail: 0,
            block_ok: 696,
            block_fail: 0,
        };
        let v = render_hook_envelope(&mine, None, false);
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(!msg.contains("capsules"), "got {msg}");
        assert!(msg.contains("696/696 blocks"), "got {msg}");
        assert!(msg.contains("woven into the archive"), "got {msg}");
    }

    #[test]
    fn hook_envelope_drops_capsules_segment_with_feedback() {
        let mine = MineCounts {
            mem_ok: 0,
            mem_fail: 0,
            block_ok: 696,
            block_fail: 0,
        };
        let feedback = FeedbackCounts {
            kind: "applies_here".to_string(),
            sent: 3,
            consumed: 3,
            failed: 0,
        };
        let v = render_hook_envelope(&mine, Some(&feedback), false);
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(!msg.contains("capsules"), "got {msg}");
        assert!(
            msg.contains("696/696 blocks · 3 feedback applied"),
            "got {msg}"
        );
    }

    #[test]
    fn hook_envelope_precompact_uses_archived() {
        let mine = MineCounts {
            mem_ok: 1,
            mem_fail: 0,
            block_ok: 4,
            block_fail: 0,
        };
        let v = render_hook_envelope(&mine, None, true);
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(msg.starts_with("✦ mem · pre-compact · "), "got {msg}");
        assert!(msg.ends_with("blocks archived"), "got {msg}");
    }

    #[test]
    fn hook_envelope_appends_feedback_when_sent() {
        let mine = MineCounts {
            mem_ok: 1,
            mem_fail: 0,
            block_ok: 5,
            block_fail: 0,
        };
        let feedback = FeedbackCounts {
            kind: "applies_here".to_string(),
            sent: 2,
            consumed: 2,
            failed: 0,
        };
        let v = render_hook_envelope(&mine, Some(&feedback), false);
        let msg = v["systemMessage"].as_str().unwrap();
        assert!(msg.contains("2 feedback applied"), "got {msg}");
    }
}
