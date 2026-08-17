use std::fs;
use std::path::{Path, PathBuf};

use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::mcp::{client::MCP_HTTP_TIMEOUT_MS, compiler::COMPILER_IN_FLIGHT_RESERVATION_MS};
use std::collections::BTreeSet;

fn repo_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn typescript_numeric_constant(content: &str, name: &str) -> u64 {
    let prefix = format!("const {name} = ");
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix(&prefix))
        .and_then(|value| value.strip_suffix(';'))
        .map(|value| value.replace('_', ""))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("missing numeric TypeScript constant {name}"))
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
fn crystallize_candidates_command_allows_only_compiler_tools() {
    let relative = ".claude-plugin/compiler/commands/crystallize-candidates.md";
    let content = fs::read_to_string(repo_path(relative))
        .expect("read crystallize-candidates plugin command")
        .replace("\r\n", "\n");
    let frontmatter = content
        .strip_prefix("---\n")
        .and_then(|body| body.split_once("\n---\n"))
        .map(|(yaml, _)| yaml)
        .expect("crystallize-candidates command has closed YAML frontmatter");
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(frontmatter).expect("command frontmatter is valid YAML");
    let allowed = yaml
        .as_mapping()
        .and_then(|mapping| mapping.get(serde_yaml_ng::Value::String("allowed-tools".to_owned())))
        .and_then(serde_yaml_ng::Value::as_str)
        .expect("allowed-tools is a comma-separated scalar");
    let actual: BTreeSet<_> = allowed.split(',').map(str::trim).collect();
    let expected: BTreeSet<_> = [
        "mcp__mem__skill_compiler_preview",
        "mcp__mem__skill_compiler_claim",
        "mcp__mem__skill_compiler_renew",
        "mcp__mem__skill_compiler_publish_proposal",
        "mcp__mem__skill_compiler_complete_decision",
        "mcp__mem__skill_compiler_fail",
    ]
    .into_iter()
    .collect();

    assert_eq!(actual, expected);
    assert!(!allowed.contains("accept"));
    assert!(!content.contains("skill_proposal_accept"));
    assert!(!content.contains("capability_capsule_review_accept"));
}

#[test]
fn compiler_plugin_launches_only_the_dedicated_mcp_profile() {
    let marketplace: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_path(".claude-plugin/marketplace.json"))
            .expect("read marketplace"),
    )
    .expect("marketplace JSON");
    let plugins = marketplace["plugins"]
        .as_array()
        .expect("marketplace plugins");
    assert!(plugins.iter().any(|plugin| {
        plugin["name"] == "mem-skill-compiler" && plugin["source"] == "./.claude-plugin/compiler"
    }));

    let manifest: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_path(
            ".claude-plugin/compiler/.claude-plugin/plugin.json",
        ))
        .expect("read compiler plugin manifest"),
    )
    .expect("compiler plugin manifest JSON");
    assert_eq!(manifest["name"], "mem-skill-compiler");
    assert_eq!(manifest["mcpServers"], "./.mcp.json");

    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_path(".claude-plugin/compiler/.mcp.json"))
            .expect("read compiler MCP config"),
    )
    .expect("compiler MCP JSON");
    let server = &config["mcpServers"]["mem"];
    assert_eq!(server["command"], "mem");
    assert_eq!(
        server["args"],
        serde_json::json!(["mcp", "--profile", "compiler"])
    );
    let rendered = server.to_string();
    assert!(!rendered.contains("MEM_ADMIN_TOKEN"));
    assert!(!rendered.contains("MEM_SKILL_REVIEWER_TOKEN"));
}

#[test]
fn pi_compiler_package_is_separate_and_uses_the_compiler_profile() {
    let package: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_path("packaging/pi/compiler/package.json"))
            .expect("read pi compiler package"),
    )
    .expect("pi compiler package JSON");
    assert_eq!(
        package["pi"]["extensions"],
        serde_json::json!(["./compiler-extension.ts"])
    );
    assert_eq!(
        package["pi"]["prompts"],
        serde_json::json!(["./prompts/crystallize-candidates.md"])
    );
    let extension = fs::read_to_string(repo_path("packaging/pi/compiler/compiler-extension.ts"))
        .expect("read pi compiler extension");
    assert!(extension.contains("[\"mcp\", \"--profile\", \"compiler\"]"));
    assert!(extension.contains("delete environment.MEM_ADMIN_TOKEN"));
    assert!(extension.contains("delete environment.MEM_SKILL_REVIEWER_TOKEN"));
    assert!(extension.contains("delete environment.MEM_SKILL_RUNTIME_TOKEN"));
    assert!(extension.contains("spawned.on(\"error\""));
    assert!(extension.contains("pi.setActiveTools([...COMPILER_TOOLS])"));
    assert!(extension.contains("compiler Agent tool isolation failed"));
    assert!(!extension.contains("wake-up"));
    assert!(!extension.contains("feedback-from-transcript"));
    assert!(!extension.contains("mem mine"));
    let pi_timeout = typescript_numeric_constant(&extension, "REQUEST_TIMEOUT_MS");
    assert!(
        MCP_HTTP_TIMEOUT_MS < pi_timeout
            && u128::from(pi_timeout) < COMPILER_IN_FLIGHT_RESERVATION_MS,
        "timeouts must satisfy HTTP < Pi RPC < reservation"
    );

    let prompt = fs::read_to_string(repo_path(
        "packaging/pi/compiler/prompts/crystallize-candidates.md",
    ))
    .expect("read pi compiler prompt");
    assert!(prompt.contains("skill_compiler_preview"));
    assert!(prompt.contains("skill_compiler_publish_proposal"));
    assert!(prompt.contains("PendingConfirmation"));
    assert!(!prompt.contains("skill_proposal_accept"));

    let pi_readme = fs::read_to_string(repo_path("packaging/pi/compiler/README.md"))
        .expect("read pi compiler README");
    assert!(pi_readme.contains("--no-builtin-tools"));
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
