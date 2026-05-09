use anyhow::Result;
use clap::Args;

use super::common::RemoteArgs;

#[derive(Debug, Args)]
pub struct WakeUpArgs {
    #[command(flatten)]
    pub remote: RemoteArgs,

    #[arg(long, default_value = "800")]
    pub token_budget: usize,
}

pub async fn run(args: WakeUpArgs) -> Result<String> {
    let mut output = String::from("## Recent Context\n\n");

    // L0: Identity file
    if let Ok(home) = std::env::var("HOME") {
        if let Ok(identity) = std::fs::read_to_string(format!("{}/.mem/identity.txt", home)) {
            output.push_str(&identity);
            output.push_str("\n\n");
        }
    }

    // L1: Query recent memories
    let client = reqwest::Client::new();
    let payload = serde_json::json!({
        "tenant": args.remote.tenant,
        "query": "",
        "intent": "wake_up",
        "scope_filters": [],
        "token_budget": args.token_budget,
        "caller_agent": "claude-code",
        "expand_graph": false,
    });

    let resp = client
        .post(format!(
            "{}/capability_capsules/search",
            args.remote.base_url
        ))
        .json(&payload)
        .send()
        .await?;

    if resp.status().is_success() {
        let data: serde_json::Value = resp.json().await?;

        // Extract from directives
        if let Some(directives) = data["directives"].as_array() {
            for directive in directives {
                if let Some(text) = directive["text"].as_str() {
                    output.push_str("- ");
                    output.push_str(text);
                    output.push('\n');
                }
            }
        }

        // Extract from relevant_facts
        if let Some(facts) = data["relevant_facts"].as_array() {
            for fact in facts {
                if let Some(text) = fact["text"].as_str() {
                    output.push_str("- ");
                    output.push_str(text);
                    output.push('\n');
                }
            }
        }

        // Extract from reusable_patterns
        if let Some(patterns) = data["reusable_patterns"].as_array() {
            for pattern in patterns {
                if let Some(text) = pattern["text"].as_str() {
                    output.push_str("- ");
                    output.push_str(text);
                    output.push('\n');
                }
            }
        }

        // Recent conversations — populated only on the wake-up fast
        // path. Each entry is one Claude Code session's freshest
        // text/thinking blocks. The session_id is exposed so the
        // agent can reverse-look up the full session via
        // POST /transcripts {session_id} or the
        // mcp__mem__transcript_session_get MCP tool.
        if let Some(sessions) = data["recent_conversations"].as_array() {
            if !sessions.is_empty() {
                output.push_str("\n## Recent Conversations\n\n");
                for s in sessions {
                    let session_id = s["session_id"].as_str().unwrap_or("?");
                    let last_at = s["last_at"].as_str().unwrap_or("");
                    let agent = s["caller_agent"].as_str().unwrap_or("agent");
                    let block_count = s["block_count"].as_i64().unwrap_or(0);
                    output.push_str(&format!(
                        "### session {session_id}  ({agent}, {block_count} blocks, last: {last_at})\n",
                    ));
                    if let Some(highlights) = s["highlights"].as_array() {
                        for h in highlights {
                            let role = h["role"].as_str().unwrap_or("?");
                            let text = h["text"].as_str().unwrap_or("");
                            output.push_str(&format!("- **{role}:** {text}\n"));
                        }
                    }
                    output.push('\n');
                }
            }
        }
    }

    Ok(output)
}
