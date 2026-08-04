use std::fs;
use std::path::{Path, PathBuf};

use mem::domain::query::SearchCapabilityCapsuleRequest;

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn parse_frontmatter(relative: &str) {
    let content = fs::read_to_string(repo_path(relative))
        .expect("read plugin markdown")
        .replace("\r\n", "\n");
    let frontmatter = content
        .strip_prefix("---\n")
        .and_then(|body| body.split_once("\n---\n"))
        .map(|(yaml, _)| yaml)
        .expect("plugin markdown must have closed YAML frontmatter");

    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(frontmatter)
        .unwrap_or_else(|error| panic!("{relative} has invalid YAML frontmatter: {error}"));
}

fn documented_search_request(relative: &str) -> SearchCapabilityCapsuleRequest {
    let content = fs::read_to_string(repo_path(relative)).expect("read plugin documentation");
    let data_argument = content
        .lines()
        .find_map(|line| line.trim().strip_prefix("-d '"))
        .expect("find curl data argument");
    let payload_end = data_argument
        .find("' |")
        .unwrap_or_else(|| data_argument.len() - 1);
    let payload = data_argument[..payload_end].replace("'\"${MEM_TENANT:-local}\"'", "local");

    serde_json::from_str(&payload)
        .unwrap_or_else(|error| panic!("{relative} has invalid search request JSON: {error}"))
}

#[test]
fn plugin_markdown_frontmatter_is_valid_yaml() {
    let commands_dir = repo_path(".claude-plugin/commands");
    for entry in fs::read_dir(commands_dir).expect("read plugin commands") {
        let path = entry.expect("read command entry").path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("md") {
            let relative = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .expect("command is inside repository")
                .to_string_lossy();
            parse_frontmatter(&relative);
        }
    }

    parse_frontmatter(".claude-plugin/skills/mem/SKILL.md");
}

#[test]
fn documented_raw_capsule_search_requests_include_required_fields() {
    for relative in [
        ".claude-plugin/commands/health.md",
        ".claude-plugin/skills/mem/SKILL.md",
    ] {
        let request = documented_search_request(relative);
        assert_eq!(request.query, "ping");
        assert_eq!(request.intent, "debugging");
        assert!(request.scope_filters.is_empty());
        assert_eq!(request.token_budget, 200);
        assert_eq!(request.caller_agent, "mem-health");
        assert!(!request.expand_graph);
        assert_eq!(request.tenant.as_deref(), Some("local"));
    }
}

#[test]
fn agents_instructions_fit_codex_limit_and_keep_feedback_contract() {
    const CODEX_AGENT_INSTRUCTION_LIMIT: usize = 32_768;
    let content = fs::read_to_string(repo_path("AGENTS.md")).expect("read AGENTS.md");

    assert!(
        content.len() < CODEX_AGENT_INSTRUCTION_LIMIT,
        "AGENTS.md is {} bytes; it must leave room below Codex's {}-byte limit",
        content.len(),
        CODEX_AGENT_INSTRUCTION_LIMIT
    );
    for required in [
        "## Feedback discipline (calling agent → MCP)",
        "mcp__mem__capability_capsule_feedback",
        "at most one",
        "`incorrect` is destructive",
    ] {
        assert!(
            content.contains(required),
            "AGENTS.md must retain feedback contract text: {required}"
        );
    }
}
