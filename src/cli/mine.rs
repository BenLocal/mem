use anyhow::Result;
use std::path::Path;

pub struct ExtractedMemory {
    pub content: String,
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
}

pub fn parse_transcript(path: &Path) -> Result<Vec<ExtractedMemory>> {
    Ok(vec![])
}
