# mem pi-transcript Parser Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach `mem mine` and `mem feedback-from-transcript` to parse **pi** (`@earendil-works/pi-coding-agent`) session JSONL — a third transcript format alongside Claude Code and Codex — so pi sessions can be mined and feedback-scanned.

**Architecture:** Add a `PiSession` variant to the existing `TranscriptFormat` enum, extend the first-line format sniffer, add a `parse_pi_session` parser (modelled on the existing `parse_codex_rollout`), and add a `collect_pi_line` feedback collector (modelled on `collect_codex_line`). pi's per-message `message.content[]` blocks are Anthropic-shaped (same block kinds as Claude), so block handling largely mirrors the Claude path; only the envelope differs (top-level `type:"message"` + `message.role`, session id from the leading `{"type":"session"}` line, stable per-line `id` as `message_uuid`).

**Tech Stack:** Rust 2021, `serde_json`, `once_cell`+`regex` (already used in `mine.rs`), inline `#[cfg(test)] mod tests`.

This is **Plan 1 of 2** for the mem × pi integration (spec: `docs/superpowers/specs/2026-07-22-mem-pi-extension-design.md`). It is the only mem-side (Rust) change and is independently testable/valuable (enables `mem mine`/`feedback-from-transcript` over pi sessions even before the pi extension exists). Plan 2 (the pi extension) consumes nothing from this plan except the working CLI behavior.

## Global Constraints

- Rust edition 2021. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` must be clean before **every** commit (CI gates both on the full crate incl. `tests/`).
- Unit tests live inline as `#[cfg(test)] mod tests` at the bottom of the source file (no colocated `*_test.rs`). This codebase has NO separate test file for `mine.rs`/`feedback.rs` parser units — add to the existing inline test module.
- Fixtures must use **generic placeholder content** — no real client/school/company names (mem repo is public).
- **Verbatim rule**: archived block `content` is the fact source; never rewrite/truncate at parse time. The parser copies text verbatim.
- pi format facts (verified against a real session at `~/.pi/agent/sessions/<cwd-slug>/<ts>_<uuid>.jsonl`):
  - Line 1: `{"type":"session","version":3,"id":"<uuid>","timestamp":"...","cwd":"..."}`
  - Then `{"type":"model_change",...}`, `{"type":"thinking_level_change",...}` (skip).
  - Messages: `{"type":"message","id":"6e055cf0","parentId":"...","timestamp":"...","message":{"role":"user"|"assistant","content":[{"type":"text","text":"..."}, ...],"timestamp":<ms>}}`.
  - `message.content[]` blocks are Anthropic-shaped: `text` / `thinking` / `tool_use` / `tool_result`.

---

### Task 1: Add `PiSession` format variant + detection + source-agent mapping

**Files:**
- Modify: `src/cli/mine.rs` — `TranscriptFormat` enum (~L260), `detect_transcript_format` (~L287), `effective_source_agent` (~L316), and the inline `#[cfg(test)] mod tests`.

**Interfaces:**
- Consumes: existing `is_codex_rollout_line(&Value) -> bool`, `TranscriptFormat`.
- Produces: `TranscriptFormat::PiSession` variant; `is_pi_session_line(&Value) -> bool`; `detect_transcript_format` now returns `PiSession` for pi files; `effective_source_agent(PiSession, _) == "pi"`.

- [ ] **Step 1: Write the failing test**

Add to the inline `#[cfg(test)] mod tests` in `src/cli/mine.rs`:

```rust
const PI_FIXTURE: &str = r#"{"type":"session","version":3,"id":"019f8771-aaaa-bbbb","timestamp":"2026-07-22T01:29:20.291Z","cwd":"/repo"}
{"type":"model_change","id":"m1","parentId":null,"timestamp":"2026-07-22T01:29:20.354Z","provider":"p","modelId":"x"}
{"type":"message","id":"u1","parentId":"m1","timestamp":"2026-07-22T01:29:36.855Z","message":{"role":"user","content":[{"type":"text","text":"how does pi store sessions"}],"timestamp":1784683776846}}
{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-07-22T01:29:40.000Z","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>pi persists sessions as JSONL under ~/.pi/agent/sessions</mem-save>"}],"timestamp":1784683780000}}
"#;

fn write_pi_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("pi-session.jsonl");
    std::fs::write(&p, PI_FIXTURE).unwrap();
    p
}

#[test]
fn detects_pi_session_format() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_pi_fixture(dir.path());
    assert_eq!(detect_transcript_format(&p), TranscriptFormat::PiSession);
}

#[test]
fn pi_source_agent_is_pi_regardless_of_flag() {
    assert_eq!(
        effective_source_agent(TranscriptFormat::PiSession, "claude-code"),
        "pi"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::mine::tests::detects_pi_session_format cli::mine::tests::pi_source_agent_is_pi_regardless_of_flag`
Expected: FAIL — `no variant named PiSession found for enum TranscriptFormat` (compile error).

- [ ] **Step 3: Write minimal implementation**

In `src/cli/mine.rs`, add the variant to the enum:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptFormat {
    /// Claude Code: `{type: user|assistant|system, message:{content:[…]}}`.
    ClaudeCode,
    /// Codex `rollout-*.jsonl`: `{type, payload, timestamp}` envelopes.
    CodexRollout,
    /// pi session: `{type:"session",version,..}` header then
    /// `{type:"message", message:{role, content:[…]}}` lines.
    PiSession,
}
```

Add the detector helper next to `is_codex_rollout_line`:

```rust
/// True when a parsed JSONL line is a pi session header — `type=="session"`
/// with a `version` field. pi always writes this as its first line, and
/// neither Claude Code nor Codex uses a top-level `type:"session"` + `version`.
fn is_pi_session_line(v: &Value) -> bool {
    v["type"].as_str() == Some("session") && v.get("version").is_some()
}
```

In `detect_transcript_format`, replace the single return expression with a 3-way check (Codex first — it's the most specific; then pi; else Claude):

```rust
        return if is_codex_rollout_line(&v) {
            TranscriptFormat::CodexRollout
        } else if is_pi_session_line(&v) {
            TranscriptFormat::PiSession
        } else {
            TranscriptFormat::ClaudeCode
        };
```

In `effective_source_agent`, add the pi arm:

```rust
pub fn effective_source_agent(format: TranscriptFormat, flag: &str) -> String {
    match format {
        TranscriptFormat::CodexRollout => "codex".to_string(),
        TranscriptFormat::PiSession => "pi".to_string(),
        TranscriptFormat::ClaudeCode => flag.to_string(),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cli::mine::tests::detects_pi_session_format cli::mine::tests::pi_source_agent_is_pi_regardless_of_flag`
Expected: PASS (both).

Then `cargo clippy --all-targets -- -D warnings` — expect: a `match` on `TranscriptFormat` elsewhere (`parse_transcript_full`) may now warn about a non-exhaustive/unhandled variant. If clippy/rustc reports a non-exhaustive match at `parse_transcript_full`'s dispatch, that is fixed in Task 2 — for THIS commit, `parse_transcript_full` currently only checks `== CodexRollout` via an `if`, so no match-exhaustiveness break occurs. Confirm clippy is clean; if it is not due to the new variant, complete Task 2 before committing the two together.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/cli/mine.rs
git commit -m "feat(mine): detect pi session transcript format"
```

---

### Task 2: `parse_pi_session` — extract memories + archive blocks from pi JSONL

**Files:**
- Modify: `src/cli/mine.rs` — add `parse_pi_session`, dispatch in `parse_transcript_full` (~L550), tests.

**Interfaces:**
- Consumes: `ExtractedMemory { content, session_id, timestamp, line_number, pending }`, `ArchivedBlock { session_id, timestamp, line_number, block_index, message_uuid: Option<String>, role, block_type, content, tool_name: Option<String>, tool_use_id: Option<String>, meta_json: Option<String> }`, `extract_memories_from_assistant_text(...)`, `build_meta_json(...)`.
- Produces: `fn parse_pi_session(path: &Path, heuristic: bool) -> Result<(Vec<ExtractedMemory>, Vec<ArchivedBlock>)>`; `parse_transcript_full` routes pi files to it.

- [ ] **Step 1: Write the failing test**

Add to the inline test module in `src/cli/mine.rs` (reuses `PI_FIXTURE`/`write_pi_fixture` from Task 1):

```rust
#[test]
fn parse_pi_session_extracts_memory_and_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let p = write_pi_fixture(dir.path());
    let (mems, blocks) = parse_transcript_full(&p, false).unwrap();

    // The assistant <mem-save> is extracted.
    assert_eq!(mems.len(), 1, "one mem-save memory");
    assert!(mems[0].content.contains("pi persists sessions"));
    assert!(!mems[0].pending);

    // Two message lines → two text blocks (user + assistant); the
    // session/model_change lines produce none.
    assert_eq!(blocks.len(), 2, "user + assistant text blocks");
    let user = &blocks[0];
    assert_eq!(user.role, "user");
    assert_eq!(user.block_type, "text");
    assert_eq!(user.content, "how does pi store sessions");
    // session_id comes from the leading {"type":"session","id":..} line.
    assert_eq!(user.session_id, "019f8771-aaaa-bbbb");
    // message_uuid is pi's stable per-line id (NOT reminted).
    assert_eq!(user.message_uuid.as_deref(), Some("u1"));

    let asst = &blocks[1];
    assert_eq!(asst.role, "assistant");
    assert_eq!(asst.message_uuid.as_deref(), Some("a1"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::mine::tests::parse_pi_session_extracts_memory_and_blocks`
Expected: FAIL — pi file currently falls through to the Claude parser (top-level `type` is `"session"`/`"message"`, not `user`/`assistant`/`system`), so `blocks.len()` is 0 and `mems.len()` is 0.

- [ ] **Step 3: Write minimal implementation**

In `src/cli/mine.rs`, add `parse_pi_session` (place it next to `parse_codex_rollout`). It reuses the Anthropic-block handling shape from the Claude path but reads pi's envelope:

```rust
/// Parse a pi session JSONL into `(memories, archive blocks)` — same output
/// shape as the Claude path. The leading `{"type":"session","id":..}` line
/// supplies the session id; each `{"type":"message", message:{role, content}}`
/// line is one message whose `content[]` blocks are Anthropic-shaped (text /
/// thinking / tool_use / tool_result), so block handling mirrors the Claude
/// parser. pi's per-line `id` is a stable message uuid (never reminted).
fn parse_pi_session(
    path: &Path,
    heuristic: bool,
) -> Result<(Vec<ExtractedMemory>, Vec<ArchivedBlock>)> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut memories = Vec::new();
    let mut blocks = Vec::new();
    let mut session_id = String::new();

    for (line_idx, line) in reader.lines().enumerate() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let line_number = line_idx + 1;

        match value["type"].as_str() {
            Some("session") => {
                if let Some(sid) = value["id"].as_str() {
                    session_id = sid.to_string();
                }
                continue;
            }
            Some("message") => {}
            // model_change / thinking_level_change / anything else: no blocks.
            _ => continue,
        }

        let msg = &value["message"];
        let role = match msg["role"].as_str() {
            Some(r @ ("user" | "assistant" | "system")) => r,
            _ => continue,
        };
        let timestamp = value["timestamp"].as_str().unwrap_or("").to_string();
        let message_uuid = value["id"].as_str().map(|s| s.to_string());

        // pi content is always an array of blocks. Accept a bare string
        // defensively (mirror the Claude parser's string-shape fallback).
        let raw_content = &msg["content"];
        let owned_array;
        let content_array: &Vec<Value> = if let Some(arr) = raw_content.as_array() {
            arr
        } else if let Some(s) = raw_content.as_str() {
            owned_array = vec![serde_json::json!({"type": "text", "text": s})];
            &owned_array
        } else {
            continue;
        };

        for (block_idx, item) in content_array.iter().enumerate() {
            let block_type = item["type"].as_str().unwrap_or("");

            if role == "assistant" && block_type == "text" {
                if let Some(text) = item["text"].as_str() {
                    extract_memories_from_assistant_text(
                        text,
                        heuristic,
                        &session_id,
                        &timestamp,
                        line_number,
                        &mut memories,
                    );
                }
            }

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
                    meta_json: None,
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
                    meta_json: None,
                }),
                "tool_use" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "tool_use".to_string(),
                    content: item["input"].to_string(),
                    tool_name: item["name"].as_str().map(|s| s.to_string()),
                    tool_use_id: item["id"].as_str().map(|s| s.to_string()),
                    meta_json: None,
                }),
                "tool_result" => Some(ArchivedBlock {
                    session_id: session_id.clone(),
                    timestamp: timestamp.clone(),
                    line_number,
                    block_index: block_idx,
                    message_uuid: message_uuid.clone(),
                    role: role.to_string(),
                    block_type: "tool_result".to_string(),
                    content: pi_tool_result_text(&item["content"]),
                    tool_name: None,
                    tool_use_id: item["tool_use_id"].as_str().map(|s| s.to_string()),
                    meta_json: None,
                }),
                _ => None,
            };
            if let Some(b) = archived {
                blocks.push(b);
            }
        }
    }

    Ok((memories, blocks))
}

/// A pi `tool_result` block's `content` is usually a string but may be an
/// array of `{type:"text", text}` items (Anthropic tool-result shape).
/// Concatenate either verbatim.
fn pi_tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|it| it["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}
```

Then wire the dispatch in `parse_transcript_full` — right after the existing Codex check:

```rust
    if detect_transcript_format(path) == TranscriptFormat::CodexRollout {
        return parse_codex_rollout(path, heuristic);
    }
    if detect_transcript_format(path) == TranscriptFormat::PiSession {
        return parse_pi_session(path, heuristic);
    }
```

(Keep the existing Claude body below as the fallthrough.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cli::mine::tests::parse_pi_session_extracts_memory_and_blocks`
Expected: PASS.
Then `cargo test --lib cli::mine` — expect: all existing mine tests still pass (Claude + Codex paths untouched).
Then `cargo clippy --all-targets -- -D warnings` — expect: clean.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/cli/mine.rs
git commit -m "feat(mine): parse pi session JSONL into memories + archive blocks"
```

> **Implementation note (not a blocker):** the confirmed real pi session had only `text` blocks. The `tool_use`/`tool_result` handling above assumes the standard Anthropic block shape inside `message.content[]`. Before Plan 2 wires feedback tool-call crediting, verify against a real pi session that exercised tools that (a) tool calls appear as `tool_use` content blocks and (b) tool results as `tool_result` content blocks — adjust field names (`input`/`id`/`tool_use_id`) if pi differs. Text extraction (the mining core) is unaffected either way; unknown block types are skipped, never errored.

---

### Task 3: `collect_pi_line` — feedback retrieval/usage scan for pi transcripts

**Files:**
- Modify: `src/cli/feedback.rs` — add `collect_pi_line`, dispatch in `scan_transcript` (~L498), tests.

**Interfaces:**
- Consumes: `extract_injected_ids(&str) -> Vec<String>`, `fingerprint(&str) -> Vec<String>`, `push_codex_banner_ids(...)` (generic in practice — matches `"mem auto-recall"`/`"related incidents/fixes"` markers), `crate::cli::mine::detect_transcript_format`, `crate::cli::mine::TranscriptFormat`.
- Produces: `fn collect_pi_line(value, line_idx, all, retrieved, assistant_corpus, search_calls, fetched)`; `scan_transcript` routes pi lines to it.

- [ ] **Step 1: Write the failing test**

Add to the inline `#[cfg(test)] mod tests` in `src/cli/feedback.rs`. The banner text must contain a recall marker (`mem auto-recall`) and a `[mem_…]` id in the shape `extract_injected_ids` recognizes (check the existing Codex/Claude feedback tests in this file for the exact banner line format and mirror it):

```rust
#[test]
fn scan_pi_transcript_credits_banner_ids() {
    // A pi user message carrying an injected recall banner, followed by an
    // assistant message that reuses the retrieved text.
    let fixture = concat!(
        r#"{"type":"session","version":3,"id":"sess-1","timestamp":"t","cwd":"/r"}"#, "\n",
        r#"{"type":"message","id":"u1","message":{"role":"user","content":[{"type":"text","text":"mem auto-recall\n[mem_abc123] pi stores sessions as jsonl"}]}}"#, "\n",
        r#"{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"text","text":"right, pi stores sessions as jsonl so I will read that file"}]}}"#, "\n",
    );
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("pi.jsonl");
    std::fs::write(&p, fixture).unwrap();

    let outcome = scan_transcript(&p, false).unwrap();
    assert!(
        outcome.retrieved.iter().any(|(id, _, _)| id == "mem_abc123"),
        "banner id should be recorded as retrieved: {:?}",
        outcome.retrieved
    );
}
```

> Before writing this, open the existing feedback tests in `src/cli/feedback.rs` and confirm: (a) the exact `ScanOutcome` field name for the retrieved list (`retrieved` vs another) and its tuple shape, and (b) the exact banner line format `extract_injected_ids` parses `[mem_…]` from. Adjust the fixture + assertion to match the proven format used by the Codex test.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib cli::feedback::tests::scan_pi_transcript_credits_banner_ids`
Expected: FAIL — pi lines currently fall through to the Claude branch, whose `role` match reads top-level `type` (`"message"`, not `user`/`assistant`), so nothing is collected and `retrieved` is empty.

- [ ] **Step 3: Write minimal implementation**

In `src/cli/feedback.rs`, add `collect_pi_line` (model it on `collect_codex_line`, adapted to pi's Anthropic block envelope):

```rust
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
                                    let mid =
                                        entry["capability_capsule_id"].as_str().unwrap_or("");
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
```

Then wire the dispatch in `scan_transcript`, right after the existing Codex branch (which does `if format == CodexRollout { collect_codex_line(...); continue; }`):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib cli::feedback::tests::scan_pi_transcript_credits_banner_ids`
Expected: PASS.
Then `cargo test --lib cli::feedback` — expect: existing feedback tests still pass.
Then `cargo clippy --all-targets -- -D warnings` — expect: clean (note `#[allow(clippy::too_many_arguments)]` on `collect_pi_line` mirrors `collect_codex_line`).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/cli/feedback.rs
git commit -m "feat(feedback): scan pi session transcripts for recall crediting"
```

---

## Self-Review

**Spec coverage (spec §3.1):**
- pi format detection → Task 1. ✅
- pi parse into memories + archive blocks → Task 2. ✅
- block_id from stable pi `.id` (no reminting) → Task 2 (`message_uuid = value["id"]`, asserted in test). ✅
- `agent=pi` forced → Task 1 (`effective_source_agent`). ✅
- feedback pi branch + banner round-trip → Task 3. ✅
- (Removed §3.2 `--dump-tools` — not in this plan per the 2026-07-22 MCP-proxy decision.) ✅

**Placeholder scan:** No TBD/TODO. The two "confirm against real pi session" notes are explicit verification steps with a concrete fallback (skip unknown blocks), not vague requirements. Task 3's "open existing feedback tests to confirm ScanOutcome field/banner format" is a real, bounded lookup step, not a placeholder.

**Type consistency:** `TranscriptFormat::PiSession` used identically in Task 1 (define), Task 2 (`parse_transcript_full` dispatch), Task 3 (`scan_transcript` dispatch). `parse_pi_session(&Path, bool) -> Result<(Vec<ExtractedMemory>, Vec<ArchivedBlock>)>` matches `parse_codex_rollout`'s signature. `collect_pi_line` argument list matches `collect_codex_line` exactly. `ArchivedBlock` field names verified against the real struct.

---

## Execution Handoff

After all three tasks land (and `cargo test -q` + `cargo fmt --check` + `cargo clippy --all-targets -- -D warnings` are green on the branch), Plan 1 is complete. Plan 2 (the pi extension) is written separately and depends only on this CLI behavior being live.
