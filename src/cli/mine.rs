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

pub async fn run(args: MineArgs) -> i32 {
    let memories = match parse_transcript(&args.transcript_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to parse transcript: {}", e);
            return 1;
        }
    };

    let client = reqwest::Client::new();
    let mut success = 0;
    let mut failed = 0;

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
                success += 1;
            }
            Ok(resp) => {
                eprintln!("Failed to save memory: {}", resp.status());
                failed += 1;
            }
            Err(e) => {
                eprintln!("Request error: {}", e);
                failed += 1;
            }
        }
    }

    println!(
        "Mined {} memories ({} success, {} failed)",
        success + failed,
        success,
        failed
    );
    if failed > 0 {
        1
    } else {
        0
    }
}
