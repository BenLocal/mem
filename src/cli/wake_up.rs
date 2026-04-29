use anyhow::Result;
use clap::Args;

#[derive(Debug, Args)]
pub struct WakeUpArgs {
    #[arg(long, default_value = "local")]
    pub tenant: String,

    #[arg(long, default_value = "800")]
    pub token_budget: usize,

    #[arg(long, default_value = "http://127.0.0.1:3000")]
    pub base_url: String,
}

pub async fn run(args: WakeUpArgs) -> Result<String> {
    let base_url = std::env::var("MEM_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or(args.base_url);

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
        "tenant": args.tenant,
        "query": "",
        "intent": "wake_up",
        "scope_filters": [],
        "token_budget": args.token_budget,
        "caller_agent": "claude-code",
        "expand_graph": false,
    });

    let resp = client
        .post(format!("{}/memories/search", base_url))
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
    }

    Ok(output)
}
