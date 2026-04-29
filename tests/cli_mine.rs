use std::fs;
use tempfile::NamedTempFile;

#[test]
fn test_parse_claude_code_transcript() {
    let transcript = r#"{"type":"custom-title","sessionId":"abc"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"<mem-save>Test memory</mem-save>"}]},"sessionId":"abc","timestamp":"2026-04-29T10:00:00Z"}
"#;
    let mut file = NamedTempFile::new().unwrap();
    fs::write(file.path(), transcript).unwrap();

    let memories = mem::cli::mine::parse_transcript(file.path()).unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0].content, "Test memory");
}
