use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

static TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<mem-save>(.*?)</mem-save>").unwrap());
static PATTERN_RES: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"(?:我会记住：|关键发现：|重要：)(.+?)(?:\n|$)").unwrap(),
        Regex::new(r"(?:I'll remember:|Key insight:|Important:)(.+?)(?:\n|$)").unwrap(),
    ]
});

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

        if let Some(content_array) = value["message"]["content"].as_array() {
            for item in content_array {
                if let Some(text) = item["text"].as_str() {
                    if let Some(extracted) = extract_memory(text) {
                        let session_id = value["sessionId"].as_str().unwrap_or("").to_string();
                        let timestamp = value["timestamp"].as_str().unwrap_or("").to_string();
                        memories.push(ExtractedMemory {
                            content: extracted,
                            session_id,
                            timestamp,
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
    if let Some(cap) = TAG_RE.captures(text) {
        return Some(cap[1].trim().to_string());
    }

    for re in PATTERN_RES.iter() {
        if let Some(cap) = re.captures(text) {
            return Some(cap[1].trim().to_string());
        }
    }

    None
}
