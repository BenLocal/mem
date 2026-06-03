use anyhow::Result;
use clap::{Args, ValueEnum};

use super::common::RemoteArgs;

/// Output shape for `mem wake-up`.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum WakeUpFormat {
    /// Markdown body suitable for human display or for piping into a
    /// file. The default; matches legacy behavior.
    Plain,
    /// JSON line shaped like the agent runtime's SessionStart hook
    /// envelope:
    /// `{"hookSpecificOutput":{"hookEventName":"SessionStart",
    /// "additionalContext":"<markdown>"}}`. Empty body resolves to
    /// `{}` so the runtime treats it as "no context to inject".
    HookSessionStart,
}

#[derive(Debug, Args)]
pub struct WakeUpArgs {
    #[command(flatten)]
    pub remote: RemoteArgs,

    #[arg(long, default_value = "800")]
    pub token_budget: usize,

    /// Output shape. Default `plain` returns the markdown body verbatim
    /// (legacy behavior). `hook-session-start` wraps it in the hook
    /// envelope so shell hook scripts can `exec` `mem wake-up` without
    /// touching `jq`.
    #[arg(long, value_enum, default_value_t = WakeUpFormat::Plain)]
    pub format: WakeUpFormat,

    /// Scope filter(s) in `kind:value` form (`repo:mem`, `project:mem`,
    /// `module:…`, `scope:repo`, or a bare `tag`). Repeatable. When set,
    /// the wake-up path floats matching capsules to the front of the
    /// recent slice so SessionStart context is about the current repo
    /// rather than whatever was globally freshest. Empty (default) keeps
    /// the legacy tenant-wide recency behavior.
    #[arg(long = "scope")]
    pub scope: Vec<String>,
}

/// Build the SessionStart hook envelope. Public so other CLI flows
/// (e.g. a future `mem hook` aggregator, or test code) can reuse the
/// exact wire shape. Returns `{}` when `body` is empty after trim,
/// matching the existing shell-hook convention for "skip injection".
pub fn session_start_envelope(body: &str) -> serde_json::Value {
    if body.trim().is_empty() {
        return serde_json::Value::Object(Default::default());
    }
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    })
}

pub async fn run(args: WakeUpArgs) -> Result<String> {
    let format = args.format;
    let body = build_body(&args).await?;
    match format {
        WakeUpFormat::Plain => Ok(body),
        WakeUpFormat::HookSessionStart => {
            // The shell hooks treated wake-up's "## Recent Context\n\n"
            // header alone (no memories, no transcripts) as content
            // worth injecting. Preserve that — emit `{}` only when the
            // entire body is whitespace, which happens only on a hard
            // failure earlier in `build_body`. Append `\n` so the
            // hook channel sees one terminated JSON record (matches
            // the heredoc behavior of the legacy shell wrapper).
            Ok(format!("{}\n", session_start_envelope(&body)))
        }
    }
}

async fn build_body(args: &WakeUpArgs) -> Result<String> {
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
        "scope_filters": args.scope,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_start_envelope_empty_body_is_skip_sentinel() {
        assert_eq!(session_start_envelope(""), serde_json::json!({}));
        assert_eq!(session_start_envelope("   \n  "), serde_json::json!({}));
    }

    #[test]
    fn session_start_envelope_wraps_non_empty_body() {
        let body = "## Recent Context\n\n- foo\n";
        let v = session_start_envelope(body);
        assert_eq!(
            v["hookSpecificOutput"]["hookEventName"].as_str().unwrap(),
            "SessionStart"
        );
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap(),
            body
        );
    }
}
