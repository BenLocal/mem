use std::fs;
use tempfile::NamedTempFile;

#[test]
fn test_parse_claude_code_transcript() {
    let transcript = r#"{"type":"custom-title","sessionId":"abc"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Test memory</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "Test memory");
}

#[test]
fn test_extract_chinese_pattern() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"我会记住：这是重要信息"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "这是重要信息");
}

#[test]
fn test_extract_english_pattern() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Key insight: This is important"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "This is important");
}

#[test]
fn test_tag_priority_over_pattern() {
    let transcript = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"I'll remember: wrong\n<mem-save>correct</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "correct");
}
