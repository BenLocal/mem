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
