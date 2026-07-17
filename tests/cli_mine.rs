use std::fs;
use tempfile::NamedTempFile;

#[test]
fn test_parse_claude_code_transcript_extracts_mem_save_tag() {
    let transcript = r#"{"type":"custom-title","sessionId":"abc"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>use rustls for TLS not native-tls</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "use rustls for TLS not native-tls");
}

#[test]
fn test_prose_cues_no_longer_extracted() {
    // Pre-2026-05-08 the extractor also matched bare prose cues like
    // "我会记住：" / "I'll remember:" / "重要：" / "Key insight:" /
    // "关键发现：". That path was removed after the recursive
    // false-positive bug where the assistant *describing* the cue
    // produced a garbage extraction. Confirm none of those forms
    // round-trip through `parse_transcript` anymore.
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我会记住：这是重要信息"}]},"sessionId":"a","timestamp":"2026-04-29T10:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Key insight: This is important"}]},"sessionId":"a","timestamp":"2026-04-29T10:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"重要：保持简单且明确"}]},"sessionId":"a","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert!(
        memories.is_empty(),
        "prose-cue sentences should no longer trigger extraction; got: {:?}",
        memories.iter().map(|m| &m.content).collect::<Vec<_>>(),
    );
}

#[test]
fn test_mem_save_tag_wins_over_inline_prose_cue() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll remember: wrong\n<mem-save>this is the canonical memory tag</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "this is the canonical memory tag");
}

#[test]
fn heuristic_extract_off_by_default_on_via_flag() {
    // O7(b): an untagged assistant block with a high-signal decision + code-ref
    // sentence. Off → nothing (legacy <mem-save>-only). On → one pending
    // (review-gated) candidate.
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我们最终决定采用 Lance 作为本地存储后端，retry 放在 src/storage/decay.rs 里"}]},"sessionId":"h","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let (off, _) = mem::cli::mine::parse_transcript_full(file.path(), false).unwrap();
    assert!(
        off.is_empty(),
        "heuristic off → no candidates, got {:?}",
        off.iter().map(|m| &m.content).collect::<Vec<_>>(),
    );

    let (on, _) = mem::cli::mine::parse_transcript_full(file.path(), true).unwrap();
    assert_eq!(
        on.len(),
        1,
        "heuristic on → 1 candidate, got {:?}",
        on.iter().map(|m| &m.content).collect::<Vec<_>>(),
    );
    assert!(
        on[0].pending,
        "heuristic candidate must be pending (→ PendingConfirmation)",
    );
    assert!(on[0].content.contains("决定采用 Lance"));
}

#[test]
fn tagged_block_is_not_double_mined_under_heuristic() {
    // A block WITH a <mem-save> tag: under heuristic=true it must still yield
    // exactly the tagged memory (pending=false), not an extra heuristic copy.
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我们决定采用 Lance <mem-save>use Lance as the local store</mem-save>"}]},"sessionId":"h2","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let (mems, _) = mem::cli::mine::parse_transcript_full(file.path(), true).unwrap();
    assert_eq!(
        mems.len(),
        1,
        "tagged block → only the tag, got {:?}",
        mems.iter()
            .map(|m| (&m.content, m.pending))
            .collect::<Vec<_>>(),
    );
    assert!(!mems[0].pending);
    assert_eq!(mems[0].content, "use Lance as the local store");
}

// ── Codex rollout support ──────────────────────────────────────────────
// Codex writes `~/.codex/sessions/.../rollout-*.jsonl` with a schema that
// differs entirely from Claude Code's: each line is
// `{type, payload, timestamp}` where only `type=="response_item"` carries
// conversation content, nested under `payload`. The session id lives once in
// the leading `session_meta` line, not on every row.

#[test]
fn test_parse_codex_rollout_extracts_mem_save_tag() {
    let rollout = r#"{"type":"session_meta","payload":{"session_id":"cx1"},"timestamp":"2026-07-16T11:00:00Z"}
{"type":"event_msg","payload":{"type":"task_started"},"timestamp":"2026-07-16T11:00:00Z"}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"help me"}]},"timestamp":"2026-07-16T11:00:01Z"}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"<mem-save>codex rollout parsing works</mem-save>"}]},"timestamp":"2026-07-16T11:00:02Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), rollout).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(
        memories.len(),
        1,
        "got {:?}",
        memories.iter().map(|m| &m.content).collect::<Vec<_>>(),
    );
    assert_eq!(memories[0].content, "codex rollout parsing works");
    // session id comes from the leading session_meta line, applied to every block.
    assert_eq!(memories[0].session_id, "cx1");
}

#[test]
fn test_codex_rollout_archives_block_kinds_with_role_mapping() {
    let rollout = r#"{"type":"session_meta","payload":{"session_id":"cx2"},"timestamp":"2026-07-16T11:00:00Z"}
{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system prompt"}]},"timestamp":"2026-07-16T11:00:01Z"}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"do it"}]},"timestamp":"2026-07-16T11:00:02Z"}
{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"let me think"}],"content":null},"timestamp":"2026-07-16T11:00:03Z"}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]},"timestamp":"2026-07-16T11:00:04Z"}
{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"ls\"}","call_id":"c1"},"timestamp":"2026-07-16T11:00:05Z"}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"file1"},"timestamp":"2026-07-16T11:00:06Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), rollout).unwrap();

    let (_mems, blocks) = mem::cli::mine::parse_transcript_full(file.path(), false).unwrap();
    let kinds: Vec<(String, String)> = blocks
        .iter()
        .map(|b| (b.role.clone(), b.block_type.clone()))
        .collect();
    assert!(
        kinds.contains(&("system".into(), "text".into())),
        "{kinds:?}"
    );
    assert!(kinds.contains(&("user".into(), "text".into())), "{kinds:?}");
    assert!(
        kinds.contains(&("assistant".into(), "thinking".into())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("assistant".into(), "text".into())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("assistant".into(), "tool_use".into())),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&("user".into(), "tool_result".into())),
        "{kinds:?}"
    );

    let tu = blocks.iter().find(|b| b.block_type == "tool_use").unwrap();
    assert_eq!(tu.tool_name.as_deref(), Some("exec_command"));
    assert_eq!(tu.tool_use_id.as_deref(), Some("c1"));
    assert!(
        blocks.iter().all(|b| b.session_id == "cx2"),
        "every block inherits the session_meta session id"
    );
}

#[test]
fn test_claude_transcript_still_parses_after_codex_support() {
    // Regression: adding format detection must not change the Claude path.
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>claude path intact</mem-save>"}]},"sessionId":"clA","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();
    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "claude path intact");
    assert_eq!(memories[0].session_id, "clA");
}
