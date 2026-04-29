use anyhow::Result;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

pub struct ExtractedMemory {
    pub content: String,
    pub session_id: String,
    pub timestamp: String,
    pub line_number: usize,
}

pub fn parse_transcript(path: &Path) -> Result<Vec<ExtractedMemory>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut memories = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        let value: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if value["type"] != "assistant" {
            continue;
        }

        let session_id = value["sessionId"].as_str().unwrap_or("").to_string();
        let timestamp = value["timestamp"].as_str().unwrap_or("").to_string();

        if let Some(content_array) = value["message"]["content"].as_array() {
            for item in content_array {
                if let Some(text) = item["text"].as_str() {
                    if let Some(extracted) = extract_memory(text) {
                        memories.push(ExtractedMemory {
                            content: extracted,
                            session_id: session_id.clone(),
                            timestamp: timestamp.clone(),
                            line_number: line_num + 1,
                        });
                    }
                }
            }
        }
    }

    Ok(memories)
}

fn extract_memory(text: &str) -> Option<String> {
    use regex::Regex;

    // Priority 1: Explicit <mem-save> tags
    let tag_re = Regex::new(r"<mem-save>(.*?)</mem-save>").unwrap();
    if let Some(cap) = tag_re.captures(text) {
        return Some(cap[1].trim().to_string());
    }

    // Priority 2: Pattern matching
    let patterns = [
        r"(?:我会记住：|关键发现：|重要：)(.+?)(?:\n|$)",
        r"(?:I'll remember:|Key insight:|Important:)(.+?)(?:\n|$)",
    ];

    for pattern in &patterns {
        let re = Regex::new(pattern).unwrap();
        if let Some(cap) = re.captures(text) {
            return Some(cap[1].trim().to_string());
        }
    }

    None
}
