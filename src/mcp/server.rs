use reqwest::Method;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::client::{encode_segment, MemHttpClient};
use super::config::McpConfig;
use super::result::{err_text, ok_json, ok_json_with_content};

#[derive(Clone)]
pub struct MemMcpServer {
    client: MemHttpClient,
    default_tenant: String,
    expose_embeddings: bool,
    #[allow(dead_code)] // read by macro-generated code in #[tool_handler]
    tool_router: ToolRouter<MemMcpServer>,
}

impl MemMcpServer {
    pub fn new(config: McpConfig) -> Self {
        Self {
            client: MemHttpClient::new(config.base_url),
            default_tenant: config.default_tenant,
            expose_embeddings: config.expose_embeddings,
            tool_router: Self::tool_router(),
        }
    }

    fn resolve_tenant(&self, override_value: Option<&String>) -> String {
        override_value
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.default_tenant.clone())
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<CallToolResult, McpError> {
        match self
            .client
            .request_json(Method::POST, path, Some(body))
            .await
        {
            Ok(v) => Ok(ok_json(&v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    async fn get_with_query(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<CallToolResult, McpError> {
        match self
            .client
            .request_json_with_query::<Value>(Method::GET, path, None, query)
            .await
        {
            Ok(v) => Ok(ok_json(&v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmptyArgs {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleSearchArgs {
    pub query: String,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub scope_filters: Option<Vec<String>>,
    #[serde(default)]
    pub token_budget: Option<u32>,
    pub caller_agent: String,
    #[serde(default)]
    pub expand_graph: Option<bool>,
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleBootstrapArgs {
    pub tenant: String,
    pub project: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    pub caller_agent: String,
    pub query: String,
    #[serde(default)]
    pub token_budget: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleSearchContextualArgs {
    pub tenant: String,
    pub project: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    pub caller_agent: String,
    pub query: String,
    /// One of: "implementation" | "debugging" | "review"
    pub intent: String,
    #[serde(default)]
    pub include_repo: Option<bool>,
    #[serde(default)]
    pub include_personal: Option<bool>,
    #[serde(default)]
    pub token_budget: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleIngestArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    /// One of: implementation | experience | preference | episode | workflow
    pub capability_capsule_type: String,
    pub content: String,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    pub code_refs: Option<Vec<String>>,
    /// One of: global | project | repo | workspace
    pub scope: String,
    /// One of: private | shared | system. Default "private".
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub task_type: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Default "mem-mcp"
    #[serde(default)]
    pub source_agent: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// One of: auto | propose. Default "auto".
    #[serde(default)]
    pub write_mode: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleCommitFactArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub project: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    pub caller_agent: String,
    pub source_agent: String,
    pub summary: String,
    pub content: String,
    pub evidence: Vec<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleProposePreferenceArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub project: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    pub caller_agent: String,
    pub source_agent: String,
    pub summary: String,
    pub content: String,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleProposeExperienceArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub project: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    pub caller_agent: String,
    pub source_agent: String,
    pub summary: String,
    pub content: String,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleGetArgs {
    pub capability_capsule_id: String,
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscriptSessionGetArgs {
    /// Claude Code session id, as exposed on
    /// `SearchCapabilityCapsuleResponse.recent_conversations[].session_id`
    /// from the wake-up call.
    pub session_id: String,
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Block-page size. Defaults to 200 (the admin web's default);
    /// max 1000 (server-side cap in `TranscriptService::get_by_session_paged`).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Optional filter — one of: user | assistant | system. When set,
    /// only blocks with the matching role come back.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional filter — one of: text | tool_use | tool_result | thinking.
    /// When set, only blocks of the matching kind come back.
    #[serde(default)]
    pub block_type: Option<String>,
    /// Lexicographic lower bound on `created_at` (inclusive). Same 20-digit
    /// millisecond string encoding as `current_timestamp`.
    #[serde(default)]
    pub since: Option<String>,
    /// Lexicographic upper bound on `created_at` (exclusive).
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleFeedbackArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub capability_capsule_id: String,
    /// One of: useful | outdated | incorrect | applies_here | does_not_apply_here
    pub feedback_kind: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleApplyFeedbackArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub project: String,
    pub caller_agent: String,
    pub capability_capsule_id: String,
    /// One of: useful | outdated | incorrect
    pub kind: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TenantOnlyArgs {
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewSimpleArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub capability_capsule_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewEditAcceptArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub capability_capsule_id: String,
    pub summary: String,
    pub content: String,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    pub code_refs: Option<Vec<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EpisodeIngestArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub goal: String,
    pub steps: Vec<String>,
    pub outcome: String,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    /// Default "workspace"
    #[serde(default)]
    pub scope: Option<String>,
    /// Default "private"
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Default "mem-mcp"
    #[serde(default)]
    pub source_agent: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GraphNeighborsArgs {
    /// Graph node id as returned by mem APIs (e.g. module:mem:billing).
    pub node_id: String,
    /// Walk depth. Default 1 (single-hop neighbors). Storage layer
    /// caps at 3 to prevent dense-graph blow-up.
    #[serde(default)]
    pub max_hops: Option<u32>,
    /// Point-in-time edge filter (20-digit ms string). When set, only
    /// edges with `valid_from <= as_of AND (valid_to IS NULL OR
    /// valid_to > as_of)` are returned. Omit for "active now".
    #[serde(default)]
    pub as_of: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KgTimelineArgs {
    /// Node id whose full edge history to surface (active + closed,
    /// ordered chronologically by `valid_from`).
    pub node_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KgAddEdgeArgs {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    /// Optional caller-supplied `valid_from` (20-digit ms string).
    /// Empty / missing → server stamps `current_timestamp()`.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Optional pre-set `valid_to` for inserting an already-closed
    /// historical edge in one shot.
    #[serde(default)]
    pub valid_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KgInvalidateEdgeArgs {
    pub from_node_id: String,
    pub to_node_id: String,
    pub relation: String,
    /// Optional `valid_to` stamp. Empty / missing → server stamps
    /// `current_timestamp()`.
    #[serde(default)]
    pub ended_at: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListInScopeArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    /// Filter `project = ?` when set.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    /// One of: implementation | experience | preference | episode | workflow | diary.
    #[serde(default)]
    pub capability_capsule_type: Option<String>,
    /// One of: provisional | active | pending_confirmation | archived | rejected.
    #[serde(default)]
    pub status: Option<String>,
    /// Page cursor from a prior response's `next_cursor` (`{updated_at, capability_capsule_id}`).
    #[serde(default)]
    pub cursor: Option<ListInScopeCursorArg>,
    /// Default 50, capped at 200 server-side.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema, Serialize)]
pub struct ListInScopeCursorArg {
    pub updated_at: String,
    pub capability_capsule_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentDiaryWriteArgs {
    /// Owning agent. Diary entries are scoped to this string — only
    /// `agent_diary_read(caller_agent=<same string>)` surfaces them.
    pub caller_agent: String,
    /// Verbatim entry text. Must be ≥12 chars.
    pub content: String,
    /// Optional one-line topic / headline (becomes the capsule's
    /// `summary`). Defaults to content[:80] when omitted.
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AgentDiaryReadArgs {
    /// Owning agent — only entries written by this agent come back.
    pub caller_agent: String,
    /// Default 20, max 200.
    #[serde(default)]
    pub last_n: Option<usize>,
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct KgListUserTunnelsArgs {
    /// Default 50, max 200.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbeddingsListJobsArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub capability_capsule_id: Option<String>,
    /// 1..=10000, default 200
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbeddingsRebuildArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub capability_capsule_ids: Option<Vec<String>>,
    #[serde(default)]
    pub force: Option<bool>,
}

// ─── batch ingest ──────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleBatchIngestArgs {
    /// Default tenant for every item; per-item value overrides.
    #[serde(default)]
    pub tenant: Option<String>,
    /// One row per capsule. Each item carries the same fields as
    /// `capability_capsule_ingest`, minus the per-item `tenant`
    /// override (passed at the outer struct).
    pub items: Vec<CapabilityCapsuleBatchIngestItem>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CapabilityCapsuleBatchIngestItem {
    /// One of: implementation | experience | preference | episode | workflow
    pub capability_capsule_type: String,
    pub content: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub evidence: Option<Vec<String>>,
    #[serde(default)]
    pub code_refs: Option<Vec<String>>,
    /// One of: global | project | repo | workspace
    pub scope: String,
    /// One of: private | shared | system. Default "private".
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub module: Option<String>,
    #[serde(default)]
    pub task_type: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Default "mem-mcp"
    #[serde(default)]
    pub source_agent: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// One of: auto | propose. Default "auto".
    #[serde(default)]
    pub write_mode: Option<String>,
}

// ─── transcript search ─────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscriptRangeArgs {
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Lexicographic lower bound on `created_at` (inclusive). Same
    /// 20-digit millisecond encoding as `current_timestamp`. Omit for
    /// "from the beginning of the archive".
    #[serde(default)]
    pub time_from: Option<String>,
    /// Lexicographic upper bound on `created_at` (exclusive). Omit for
    /// "up to now".
    #[serde(default)]
    pub time_to: Option<String>,
    /// Optional filter — one of: user | assistant | system.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional filter — one of: text | tool_use | tool_result | thinking.
    #[serde(default)]
    pub block_type: Option<String>,
    /// Page cursor from a prior response's `next_cursor`. Pass back
    /// verbatim to continue scrolling.
    #[serde(default)]
    pub cursor: Option<TranscriptCursor>,
    /// Page size. Defaults to 200; capped at 1000 server-side.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema, Serialize)]
pub struct TranscriptCursor {
    pub created_at: String,
    pub line_number: i64,
    pub block_index: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscriptSearchArgs {
    /// Free-text BM25 + semantic query. Empty string falls through to
    /// the recent-time browse path.
    pub query: String,
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
    /// Restrict to one session (optional).
    #[serde(default)]
    pub session_id: Option<String>,
    /// One of: user | assistant | system
    #[serde(default)]
    pub role: Option<String>,
    /// One of: text | tool_use | tool_result | thinking
    #[serde(default)]
    pub block_type: Option<String>,
    /// Lexicographic compare against `created_at` (ISO-8601). Inclusive.
    #[serde(default)]
    pub time_from: Option<String>,
    #[serde(default)]
    pub time_to: Option<String>,
    /// 1..=100, default 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Inject this session's blocks as candidates regardless of query.
    #[serde(default)]
    pub anchor_session_id: Option<String>,
    /// ±N blocks of context around each primary; capped at 10.
    #[serde(default)]
    pub context_window: Option<usize>,
    /// Include tool_use / tool_result blocks in the context window.
    #[serde(default)]
    pub include_tool_blocks_in_context: Option<bool>,
}

// ─── entities ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityCreateArgs {
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
    pub canonical_name: String,
    /// One of: topic | project | repo | module | workflow
    pub kind: String,
    /// Optional list of additional aliases that should resolve to this
    /// entity (the canonical_name is implicitly an alias). Re-POSTing
    /// with the same canonical name is idempotent on alias hit.
    #[serde(default)]
    pub aliases: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityGetArgs {
    pub entity_id: String,
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityAddAliasArgs {
    pub entity_id: String,
    pub alias: String,
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EntityListArgs {
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
    /// One of: topic | project | repo | module | workflow
    #[serde(default)]
    pub kind: Option<String>,
    /// Substring match against `canonical_name` (case-sensitive).
    #[serde(default)]
    pub q: Option<String>,
    /// 1..=100, default 50.
    #[serde(default)]
    pub limit: Option<usize>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pick_search_summary(value: &Value) -> Value {
    let mut out = Map::new();
    if let Some(obj) = value.as_object() {
        for key in ["directives", "relevant_facts", "reusable_patterns"] {
            if let Some(v) = obj.get(key) {
                if v.is_array() {
                    out.insert(key.to_string(), v.clone());
                }
            }
        }
        if let Some(v) = obj.get("suggested_workflow") {
            if v.is_object() {
                out.insert("suggested_workflow".to_string(), v.clone());
            }
        }
    }
    Value::Object(out)
}

fn embeddings_disabled_error() -> CallToolResult {
    err_text("embeddings tools are disabled; set MEM_MCP_EXPOSE_EMBEDDINGS=1 to enable")
}

// ---------------------------------------------------------------------------
// Tool router
// ---------------------------------------------------------------------------

#[tool_router]
impl MemMcpServer {
    // ------------------- mem_health -------------------
    #[tool(
        description = "Check that the mem HTTP server is reachable (GET /health). Use when MCP tools fail to see if the service is up."
    )]
    async fn mem_health(
        &self,
        Parameters(_): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.client.get_text("health").await {
            Ok(body) => Ok(ok_json(&json!({
                "reachable": true,
                "health_body": body.trim(),
            }))),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_search -------------------
    #[tool(
        description = "Search the shared mem service for compressed directives, facts, and patterns. Call early in a task; use scope_filters like repo:<name> to narrow results."
    )]
    async fn capability_capsule_search(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "query": args.query,
            "intent": args.intent.unwrap_or_else(|| "general".to_string()),
            "scope_filters": args.scope_filters.unwrap_or_default(),
            "token_budget": args.token_budget.unwrap_or(400),
            "caller_agent": args.caller_agent,
            "expand_graph": args.expand_graph.unwrap_or(true),
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
        });
        self.post_json("capability_capsules/search", &body).await
    }

    // ------------------- capability_capsule_bootstrap -------------------
    #[tool(
        description = "Lightweight project-only bootstrap search for task-start context recovery."
    )]
    async fn capability_capsule_bootstrap(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleBootstrapArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "query": args.query,
            "intent": "bootstrap",
            "scope_filters": [format!("project:{}", args.project)],
            "token_budget": args.token_budget.unwrap_or(120),
            "caller_agent": args.caller_agent,
            "expand_graph": false,
            "tenant": args.tenant,
        });
        match self
            .client
            .request_json(Method::POST, "capability_capsules/search", Some(&body))
            .await
        {
            Ok(v) => Ok(ok_json(&pick_search_summary(&v))),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_search_contextual -------------------
    #[tool(
        description = "Intent-aware search for implementation, debugging, or review. Defaults to project scope and only widens when explicitly requested."
    )]
    async fn capability_capsule_search_contextual(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleSearchContextualArgs>,
    ) -> Result<CallToolResult, McpError> {
        let include_repo = args.include_repo.unwrap_or(false);
        let include_personal = args.include_personal.unwrap_or(false);
        if include_repo && args.repo.is_none() {
            return Ok(err_text("repo is required when include_repo is true"));
        }
        let mut scope_filters = vec![format!("project:{}", args.project)];
        if include_repo {
            if let Some(r) = &args.repo {
                scope_filters.push(format!("repo:{r}"));
            }
        }
        if include_personal {
            scope_filters.push("scope:workspace".to_string());
        }
        let body = json!({
            "query": args.query,
            "intent": args.intent,
            "scope_filters": scope_filters,
            "token_budget": args.token_budget.unwrap_or(400),
            "caller_agent": args.caller_agent,
            "expand_graph": true,
            "tenant": args.tenant,
        });
        match self
            .client
            .request_json(Method::POST, "capability_capsules/search", Some(&body))
            .await
        {
            Ok(v) => Ok(ok_json(&pick_search_summary(&v))),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_ingest -------------------
    #[tool(
        description = "Create a memory in mem. Use write_mode propose for preferences; auto is fine for implementation facts."
    )]
    async fn capability_capsule_ingest(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleIngestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert(
            "capability_capsule_type".into(),
            json!(args.capability_capsule_type),
        );
        body.insert("content".into(), json!(args.content));
        body.insert("evidence".into(), json!(args.evidence.unwrap_or_default()));
        body.insert(
            "code_refs".into(),
            json!(args.code_refs.unwrap_or_default()),
        );
        body.insert("scope".into(), json!(args.scope));
        body.insert(
            "visibility".into(),
            json!(args.visibility.unwrap_or_else(|| "private".to_string())),
        );
        body.insert("tags".into(), json!(args.tags.unwrap_or_default()));
        body.insert(
            "source_agent".into(),
            json!(args.source_agent.unwrap_or_else(|| "mem-mcp".to_string())),
        );
        body.insert(
            "write_mode".into(),
            json!(args.write_mode.unwrap_or_else(|| "auto".to_string())),
        );
        if let Some(v) = args.project {
            body.insert("project".into(), json!(v));
        }
        if let Some(v) = args.repo {
            body.insert("repo".into(), json!(v));
        }
        if let Some(v) = args.module {
            body.insert("module".into(), json!(v));
        }
        if let Some(v) = args.task_type {
            body.insert("task_type".into(), json!(v));
        }
        if let Some(v) = args.idempotency_key {
            body.insert("idempotency_key".into(), json!(v));
        }
        let content = args.content.clone();
        match self
            .client
            .request_json(
                Method::POST,
                "capability_capsules",
                Some(&Value::Object(body)),
            )
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Memory saved", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_commit_fact -------------------
    #[tool(description = "Commit a verified project fact. Uses auto write mode and project scope.")]
    async fn capability_capsule_commit_fact(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleCommitFactArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut tags: Vec<String> = args.tags.unwrap_or_default();
        tags.push(format!("caller_agent:{}", args.caller_agent));
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("capability_capsule_type".into(), json!("implementation"));
        body.insert(
            "content".into(),
            json!(format!("{}\n\n{}", args.summary, args.content)),
        );
        body.insert("evidence".into(), json!(args.evidence));
        body.insert("scope".into(), json!("project"));
        body.insert("visibility".into(), json!("private"));
        body.insert("project".into(), json!(args.project));
        if let Some(v) = args.repo {
            body.insert("repo".into(), json!(v));
        }
        if let Some(v) = args.module {
            body.insert("module".into(), json!(v));
        }
        body.insert("source_agent".into(), json!(args.source_agent));
        body.insert("tags".into(), json!(tags));
        body.insert("write_mode".into(), json!("auto"));
        if let Some(v) = args.idempotency_key {
            body.insert("idempotency_key".into(), json!(v));
        }
        let content = format!("{}\n\n{}", args.summary, args.content);
        match self
            .client
            .request_json(
                Method::POST,
                "capability_capsules",
                Some(&Value::Object(body)),
            )
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Fact committed", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_propose_preference -------------------
    #[tool(
        description = "Propose a preference for review. Uses the standard memories endpoint with write_mode=propose."
    )]
    async fn capability_capsule_propose_preference(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleProposePreferenceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("capability_capsule_type".into(), json!("preference"));
        body.insert(
            "content".into(),
            json!(format!("{}\n\n{}", args.summary, args.content)),
        );
        body.insert("evidence".into(), json!(args.evidence.unwrap_or_default()));
        body.insert("code_refs".into(), json!(Vec::<String>::new()));
        body.insert("scope".into(), json!("project"));
        body.insert("visibility".into(), json!("private"));
        body.insert("project".into(), json!(args.project));
        if let Some(v) = args.repo {
            body.insert("repo".into(), json!(v));
        }
        if let Some(v) = args.module {
            body.insert("module".into(), json!(v));
        }
        body.insert(
            "tags".into(),
            json!(vec![format!("caller_agent:{}", args.caller_agent)]),
        );
        body.insert("source_agent".into(), json!(args.source_agent));
        body.insert("write_mode".into(), json!("propose"));
        let content = format!("{}\n\n{}", args.summary, args.content);
        match self
            .client
            .request_json(
                Method::POST,
                "capability_capsules",
                Some(&Value::Object(body)),
            )
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Preference proposed", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_propose_experience -------------------
    #[tool(
        description = "Propose a candidate experience by recording it as an episode instead of a strong fact."
    )]
    async fn capability_capsule_propose_experience(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleProposeExperienceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("goal".into(), json!(args.summary));
        body.insert("steps".into(), json!(Vec::<String>::new()));
        body.insert("outcome".into(), json!(args.content));
        body.insert("evidence".into(), json!(args.evidence.unwrap_or_default()));
        body.insert("scope".into(), json!("project"));
        body.insert("visibility".into(), json!("private"));
        body.insert("project".into(), json!(args.project));
        if let Some(v) = args.repo {
            body.insert("repo".into(), json!(v));
        }
        if let Some(v) = args.module {
            body.insert("module".into(), json!(v));
        }
        body.insert(
            "tags".into(),
            json!(vec![format!("caller_agent:{}", args.caller_agent)]),
        );
        body.insert("source_agent".into(), json!(args.source_agent));
        let content = args.content.clone();
        match self
            .client
            .request_json(Method::POST, "episodes", Some(&Value::Object(body)))
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Experience proposed", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- memory_get -------------------
    #[tool(
        description = "Fetch one memory by id (detail, version chain, graph links, embedding metadata)."
    )]
    async fn capability_capsule_get(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        let path = format!(
            "capability_capsules/{}",
            encode_segment(&args.capability_capsule_id)
        );
        self.get_with_query(&path, &[("tenant", tenant)]).await
    }

    // ------------------- capability_capsule_feedback -------------------
    #[tool(description = "Record feedback on a memory to adjust future ranking.")]
    async fn capability_capsule_feedback(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleFeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_id": args.capability_capsule_id,
            "feedback_kind": args.feedback_kind,
        });
        self.post_json("capability_capsules/feedback", &body).await
    }

    // ------------------- capability_capsule_apply_feedback -------------------
    #[tool(
        description = "Apply limited feedback on a memory while keeping the existing POST /capability_capsules/feedback backend contract. Optional `note` is persisted verbatim on the resulting feedback_events row."
    )]
    async fn capability_capsule_apply_feedback(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleApplyFeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert(
            "capability_capsule_id".into(),
            json!(args.capability_capsule_id),
        );
        body.insert("feedback_kind".into(), json!(args.kind));
        if let Some(note) = args.note.filter(|s| !s.is_empty()) {
            body.insert("note".into(), json!(note));
        }
        self.post_json("capability_capsules/feedback", &Value::Object(body))
            .await
    }

    // ------------------- capability_capsule_list_pending_review -------------------
    #[tool(description = "List memories awaiting human confirmation for this tenant.")]
    async fn capability_capsule_list_pending_review(
        &self,
        Parameters(args): Parameters<TenantOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        self.get_with_query("reviews/pending", &[("tenant", tenant)])
            .await
    }

    // ------------------- capability_capsule_review_accept -------------------
    #[tool(
        description = "Accept a pending memory (activate without edits). Use after human confirms."
    )]
    async fn capability_capsule_review_accept(
        &self,
        Parameters(args): Parameters<ReviewSimpleArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_id": args.capability_capsule_id,
        });
        self.post_json("reviews/pending/accept", &body).await
    }

    // ------------------- capability_capsule_review_reject -------------------
    #[tool(description = "Reject a pending memory (mark rejected, no successor).")]
    async fn capability_capsule_review_reject(
        &self,
        Parameters(args): Parameters<ReviewSimpleArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_id": args.capability_capsule_id,
        });
        self.post_json("reviews/pending/reject", &body).await
    }

    // ------------------- capability_capsule_review_edit_accept -------------------
    #[tool(
        description = "Edit pending memory content then accept: creates an active successor and rejects the original pending row."
    )]
    async fn capability_capsule_review_edit_accept(
        &self,
        Parameters(args): Parameters<ReviewEditAcceptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_id": args.capability_capsule_id,
            "summary": args.summary,
            "content": args.content,
            "evidence": args.evidence.unwrap_or_default(),
            "code_refs": args.code_refs.unwrap_or_default(),
            "tags": args.tags.unwrap_or_default(),
        });
        self.post_json("reviews/pending/edit_accept", &body).await
    }

    // ------------------- episode_ingest -------------------
    #[tool(
        description = "Record a successful multi-step episode; may produce workflow candidates."
    )]
    async fn episode_ingest(
        &self,
        Parameters(args): Parameters<EpisodeIngestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("goal".into(), json!(args.goal));
        body.insert("steps".into(), json!(args.steps));
        body.insert("outcome".into(), json!(args.outcome));
        body.insert("evidence".into(), json!(args.evidence.unwrap_or_default()));
        body.insert(
            "scope".into(),
            json!(args.scope.unwrap_or_else(|| "workspace".to_string())),
        );
        body.insert(
            "visibility".into(),
            json!(args.visibility.unwrap_or_else(|| "private".to_string())),
        );
        body.insert("tags".into(), json!(args.tags.unwrap_or_default()));
        body.insert(
            "source_agent".into(),
            json!(args.source_agent.unwrap_or_else(|| "mem-mcp".to_string())),
        );
        if let Some(v) = args.project {
            body.insert("project".into(), json!(v));
        }
        if let Some(v) = args.repo {
            body.insert("repo".into(), json!(v));
        }
        if let Some(v) = args.module {
            body.insert("module".into(), json!(v));
        }
        if let Some(v) = args.idempotency_key {
            body.insert("idempotency_key".into(), json!(v));
        }
        let content = format!("Goal: {}\nOutcome: {}", args.goal, args.outcome);
        match self
            .client
            .request_json(Method::POST, "episodes", Some(&Value::Object(body)))
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Episode recorded", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_graph_neighbors -------------------
    #[tool(
        description = "List graph edges adjacent to a node id (e.g. module:mem:billing, project:acme). Optional `max_hops` (default 1, cap 3) performs a BFS walk; `as_of` filters to edges active at a point in time (20-digit ms string). Complements capability_capsule_search when expand_graph is not enough."
    )]
    async fn capability_capsule_graph_neighbors(
        &self,
        Parameters(args): Parameters<GraphNeighborsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("graph/neighbors/{}", encode_segment(&args.node_id));
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(h) = args.max_hops {
            query.push(("max_hops", h.to_string()));
        }
        if let Some(t) = args.as_of {
            query.push(("as_of", t));
        }
        self.get_with_query(&path, &query).await
    }

    // ------------------- capability_capsule_kg_timeline -------------------
    #[tool(
        description = "Full edge history for one node (active + closed), ordered `valid_from ASC, relation ASC`. Use to see how an entity / project / topic evolved over time — which capsules referenced it, which relations have been invalidated, which are still active. Pairs with `capability_capsule_kg_invalidate_edge` for the write side."
    )]
    async fn capability_capsule_kg_timeline(
        &self,
        Parameters(args): Parameters<KgTimelineArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("graph/timeline/{}", encode_segment(&args.node_id));
        self.get_with_query(&path, &[]).await
    }

    // ------------------- capability_capsule_graph_stats -------------------
    #[tool(
        description = "Whole-graph aggregate counts: node_count, total/active/closed edge counts, top-N relation kinds. Tenant-less (graph_edges has no tenant column — all tenants share one graph by design). Use for observability and to spot KG health regressions (e.g. closed_edges growing without bound)."
    )]
    async fn capability_capsule_graph_stats(
        &self,
        Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.get_with_query("graph/stats", &[]).await
    }

    // ------------------- capability_capsule_kg_add_edge -------------------
    #[tool(
        description = "Write a caller-supplied edge directly. Use when an agent learns a new fact (`X depends on Y`, `project Z owns module W`) that the auto-extractor wouldn't catch. Idempotent on the active `(from, to, relation)` triple. Pass `valid_from` to backdate; omit for `now`. Pass `valid_to` only for the rare pre-closed historical edge case."
    )]
    async fn capability_capsule_kg_add_edge(
        &self,
        Parameters(args): Parameters<KgAddEdgeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert("from_node_id".into(), json!(args.from_node_id));
        body.insert("to_node_id".into(), json!(args.to_node_id));
        body.insert("relation".into(), json!(args.relation));
        if let Some(v) = args.valid_from {
            body.insert("valid_from".into(), json!(v));
        }
        if let Some(v) = args.valid_to {
            body.insert("valid_to".into(), json!(v));
        }
        self.post_json("graph/edges", &Value::Object(body)).await
    }

    // ------------------- capability_capsule_kg_invalidate_edge -------------------
    #[tool(
        description = "Close (invalidate) one specific active edge by `(from, predicate, to)` triple — stamps `valid_to = ended_at` (defaults to `current_timestamp()`). Idempotent: a triple with no active edge returns `{closed: 0}` without error. Use when you learn a previously-true fact is no longer true; the closed row stays in the table for audit / timeline reconstruction."
    )]
    async fn capability_capsule_kg_invalidate_edge(
        &self,
        Parameters(args): Parameters<KgInvalidateEdgeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert("from_node_id".into(), json!(args.from_node_id));
        body.insert("to_node_id".into(), json!(args.to_node_id));
        body.insert("relation".into(), json!(args.relation));
        if let Some(v) = args.ended_at {
            body.insert("ended_at".into(), json!(v));
        }
        self.post_json("graph/edges/invalidate", &Value::Object(body))
            .await
    }

    // ------------------- capability_capsule_list_in_scope -------------------
    #[tool(
        description = "Browse capsules by scope (project / repo / module / type / status) without an embedding query. Use when the caller wants 'show me everything under project X' rather than 'find the most relevant capsules for query Y'. Paginated by `(updated_at, capability_capsule_id)` cursor. Default limit 50, max 200. Each filter is optional and AND-combined. Returns `{capability_capsules, next_cursor, has_more}`."
    )]
    async fn capability_capsule_list_in_scope(
        &self,
        Parameters(args): Parameters<ListInScopeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        if let Some(v) = args.project {
            body.insert("project".into(), json!(v));
        }
        if let Some(v) = args.repo {
            body.insert("repo".into(), json!(v));
        }
        if let Some(v) = args.module {
            body.insert("module".into(), json!(v));
        }
        if let Some(v) = args.capability_capsule_type {
            body.insert("capability_capsule_type".into(), json!(v));
        }
        if let Some(v) = args.status {
            body.insert("status".into(), json!(v));
        }
        if let Some(v) = args.cursor {
            body.insert("cursor".into(), json!(v));
        }
        if let Some(v) = args.limit {
            body.insert("limit".into(), json!(v));
        }
        self.post_json("capability_capsules/list", &Value::Object(body))
            .await
    }

    // ------------------- capability_capsule_agent_diary_write -------------------
    #[tool(
        description = "Append an entry to the calling agent's private diary. Diary capsules use `capability_capsule_type=diary` and are excluded from `capability_capsule_search` results by default — they're for the writing agent's self-notes (tried-but-failed approaches, intermediate observations, scratchpad thoughts) without polluting the shared capsule pool. Each agent reads only its own diary via `capability_capsule_agent_diary_read(caller_agent=<same>)`. Content ≥12 chars."
    )]
    async fn capability_capsule_agent_diary_write(
        &self,
        Parameters(args): Parameters<AgentDiaryWriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.content.trim().chars().count() < 12 {
            return Ok(err_text(
                "agent diary content must be at least 12 characters".to_string(),
            ));
        }
        let summary = args
            .topic
            .clone()
            .unwrap_or_else(|| args.content.chars().take(80).collect());
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_type": "diary",
            "content": args.content,
            "summary": summary,
            "scope": "workspace",
            "visibility": "private",
            "source_agent": args.caller_agent,
            "write_mode": "auto",
        });
        self.post_json("capability_capsules", &body).await
    }

    // ------------------- capability_capsule_agent_diary_read -------------------
    #[tool(
        description = "Read back the calling agent's own diary entries (where `capability_capsule_type=diary` AND `source_agent=caller_agent`), most-recent first. Other agents' diaries are not accessible. Default `last_n=20`, max 200. Use this instead of `capability_capsule_search` when you want your scratchpad, not the shared pool."
    )]
    async fn capability_capsule_agent_diary_read(
        &self,
        Parameters(args): Parameters<AgentDiaryReadArgs>,
    ) -> Result<CallToolResult, McpError> {
        // diary entries are stored as regular capsules but filtered
        // out of search by SQL — list_in_scope intentionally has no
        // diary exclusion, so passing capability_capsule_type=diary
        // is what surfaces them. source_agent must match server-side;
        // we encode it via the (yet to be added) source_agent filter.
        // For now, pull everything of type=diary and filter
        // source_agent client-side from the response list.
        let limit = args.last_n.unwrap_or(20).clamp(1, 200);
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_type": "diary",
            "limit": limit,
        });
        // Use post_json_with_filter so we can drop entries that
        // belong to a different caller_agent. The base list endpoint
        // doesn't (yet) take `source_agent` as a filter — that's a
        // follow-up. Until then, we over-fetch and filter.
        match self
            .client
            .request_json::<Value>(Method::POST, "capability_capsules/list", Some(&body))
            .await
        {
            Ok(mut v) => {
                if let Some(arr) = v
                    .get_mut("capability_capsules")
                    .and_then(|x| x.as_array_mut())
                {
                    arr.retain(|c| {
                        c.get("source_agent")
                            .and_then(|s| s.as_str())
                            .map(|s| s == args.caller_agent)
                            .unwrap_or(false)
                    });
                }
                Ok(ok_json(&v))
            }
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- capability_capsule_kg_list_user_tunnels -------------------
    #[tool(
        description = "List caller-curated graph edges (`relation` starts with `user_tunnel:`). Mem's auto-extracted edges use plain relation strings (`mentions`, `tagged`, `supersedes`); caller-supplied bridges between scopes/topics by convention prefix the relation with `user_tunnel:<label>`. Use `capability_capsule_kg_add_edge` to create one (`relation=user_tunnel:related_to_billing` etc.); `capability_capsule_kg_invalidate_edge` to close. This endpoint is the read side. Default limit 50, max 200."
    )]
    async fn capability_capsule_kg_list_user_tunnels(
        &self,
        Parameters(args): Parameters<KgListUserTunnelsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let limit = args.limit.unwrap_or(50).clamp(1, 200);
        self.get_with_query("graph/tunnels", &[("limit", limit.to_string())])
            .await
    }

    // ------------------- transcript_session_get -------------------
    #[tool(
        description = "Fetch the full block sequence for one Claude Code transcript session, identified by `session_id` (as exposed on the wake-up response's `recent_conversations[].session_id`). Returns chronological text/thinking/tool blocks. Optional `role` / `block_type` / `since` / `until` narrow the page to specific speakers, block kinds, or time windows — useful for 'show only assistant text from session X' style queries."
    )]
    async fn transcript_session_get(
        &self,
        Parameters(args): Parameters<TranscriptSessionGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("session_id".into(), json!(args.session_id));
        body.insert("limit".into(), json!(args.limit.unwrap_or(200)));
        if let Some(v) = args.role {
            body.insert("role".into(), json!(v));
        }
        if let Some(v) = args.block_type {
            body.insert("block_type".into(), json!(v));
        }
        if let Some(v) = args.since {
            body.insert("since".into(), json!(v));
        }
        if let Some(v) = args.until {
            body.insert("until".into(), json!(v));
        }
        self.post_json("transcripts", &Value::Object(body)).await
    }

    // ------------------- transcripts_list_sessions -------------------
    #[tool(
        description = "List all Claude Code transcript sessions for a tenant as `{session_id, block_count, first_at, last_at, caller_agent}` summaries, ordered newest-first by `last_at`. Discovery entry point — pair with `transcript_session_get` to drill into a specific session, or with `transcripts_search` when looking for content rather than recent activity."
    )]
    async fn transcripts_list_sessions(
        &self,
        Parameters(args): Parameters<TenantOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        self.get_with_query("transcripts/sessions", &[("tenant", tenant)])
            .await
    }

    // ------------------- transcripts_range -------------------
    #[tool(
        description = "Cross-session time-window scan over the transcript archive. Returns every block for the tenant inside `[time_from, time_to)` (each bound optional), chronologically ordered and paginated by a composite cursor. Optional `role` / `block_type` narrow the result. Use when you want 'everything between time X and Y across all sessions' or 'recent activity since cursor Z' rather than per-session drill-down (`transcript_session_get`) or content search (`transcripts_search`)."
    )]
    async fn transcripts_range(
        &self,
        Parameters(args): Parameters<TranscriptRangeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        if let Some(v) = args.time_from {
            body.insert("time_from".into(), json!(v));
        }
        if let Some(v) = args.time_to {
            body.insert("time_to".into(), json!(v));
        }
        if let Some(v) = args.role {
            body.insert("role".into(), json!(v));
        }
        if let Some(v) = args.block_type {
            body.insert("block_type".into(), json!(v));
        }
        if let Some(v) = args.cursor {
            body.insert("cursor".into(), json!(v));
        }
        body.insert("limit".into(), json!(args.limit.unwrap_or(200)));
        self.post_json("transcripts/range", &Value::Object(body))
            .await
    }

    // ------------------- capability_capsule_batch_ingest -------------------
    #[tool(
        description = "Bulk-insert multiple capsules in one call (server folds N rows into one Lance write + one DuckDB refresh; bench shows 9-227x speedup over looping `capability_capsule_ingest`). Returns 201 with per-item {result: ok | err} preserving input order, or 207 if any item failed."
    )]
    async fn capability_capsule_batch_ingest(
        &self,
        Parameters(args): Parameters<CapabilityCapsuleBatchIngestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        let items: Vec<Value> = args
            .items
            .into_iter()
            .map(|i| {
                let mut body = Map::new();
                body.insert("tenant".into(), json!(tenant));
                body.insert(
                    "capability_capsule_type".into(),
                    json!(i.capability_capsule_type),
                );
                body.insert("content".into(), json!(i.content));
                body.insert("evidence".into(), json!(i.evidence.unwrap_or_default()));
                body.insert("code_refs".into(), json!(i.code_refs.unwrap_or_default()));
                body.insert("scope".into(), json!(i.scope));
                body.insert(
                    "visibility".into(),
                    json!(i.visibility.unwrap_or_else(|| "private".to_string())),
                );
                body.insert("tags".into(), json!(i.tags.unwrap_or_default()));
                body.insert(
                    "source_agent".into(),
                    json!(i.source_agent.unwrap_or_else(|| "mem-mcp".to_string())),
                );
                body.insert(
                    "write_mode".into(),
                    json!(i.write_mode.unwrap_or_else(|| "auto".to_string())),
                );
                if let Some(v) = i.summary {
                    body.insert("summary".into(), json!(v));
                }
                if let Some(v) = i.project {
                    body.insert("project".into(), json!(v));
                }
                if let Some(v) = i.repo {
                    body.insert("repo".into(), json!(v));
                }
                if let Some(v) = i.module {
                    body.insert("module".into(), json!(v));
                }
                if let Some(v) = i.task_type {
                    body.insert("task_type".into(), json!(v));
                }
                if let Some(v) = i.idempotency_key {
                    body.insert("idempotency_key".into(), json!(v));
                }
                Value::Object(body)
            })
            .collect();
        self.post_json("capability_capsules/batch", &Value::Array(items))
            .await
    }

    // ------------------- transcripts_search -------------------
    #[tool(
        description = "Hybrid (BM25 + semantic) search over the verbatim transcript archive. Returns merged context windows around each primary hit. Use to recall earlier conversations beyond what wake-up surfaces; pair with `transcript_session_get` to fetch full sessions."
    )]
    async fn transcripts_search(
        &self,
        Parameters(args): Parameters<TranscriptSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("query".into(), json!(args.query));
        body.insert("limit".into(), json!(args.limit.unwrap_or(20)));
        body.insert(
            "include_tool_blocks_in_context".into(),
            json!(args.include_tool_blocks_in_context.unwrap_or(false)),
        );
        if let Some(v) = args.session_id {
            body.insert("session_id".into(), json!(v));
        }
        if let Some(v) = args.role {
            body.insert("role".into(), json!(v));
        }
        if let Some(v) = args.block_type {
            body.insert("block_type".into(), json!(v));
        }
        if let Some(v) = args.time_from {
            body.insert("time_from".into(), json!(v));
        }
        if let Some(v) = args.time_to {
            body.insert("time_to".into(), json!(v));
        }
        if let Some(v) = args.anchor_session_id {
            body.insert("anchor_session_id".into(), json!(v));
        }
        if let Some(v) = args.context_window {
            body.insert("context_window".into(), json!(v));
        }
        self.post_json("transcripts/search", &Value::Object(body))
            .await
    }

    // ------------------- entity_create -------------------
    #[tool(
        description = "Create or resolve a canonical entity in the registry. Idempotent on alias hit (re-POSTing the same canonical_name returns the existing entity_id). Returns 201 / 409 (alias already bound to a different entity)."
    )]
    async fn entity_create(
        &self,
        Parameters(args): Parameters<EntityCreateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "canonical_name": args.canonical_name,
            "kind": args.kind,
            "aliases": args.aliases.unwrap_or_default(),
        });
        self.post_json("entities", &body).await
    }

    // ------------------- entity_get -------------------
    #[tool(
        description = "Fetch one entity (canonical_name, kind, aliases) by entity_id. Returns 404 when the id is unknown."
    )]
    async fn entity_get(
        &self,
        Parameters(args): Parameters<EntityGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("entities/{}", encode_segment(&args.entity_id));
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        self.get_with_query(&path, &[("tenant", tenant)]).await
    }

    // ------------------- entity_add_alias -------------------
    #[tool(
        description = "Declare an additional alias for an existing entity. Returns 200 (inserted / already_on_same_entity) or 409 (conflict_with_different_entity)."
    )]
    async fn entity_add_alias(
        &self,
        Parameters(args): Parameters<EntityAddAliasArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "alias": args.alias,
        });
        let path = format!("entities/{}/aliases", encode_segment(&args.entity_id));
        self.post_json(&path, &body).await
    }

    // ------------------- entity_list -------------------
    #[tool(
        description = "List entities for the tenant, ordered by created_at desc. Supports filtering by `kind` and substring `q` on canonical_name. Default limit 50, server-side cap 100."
    )]
    async fn entity_list(
        &self,
        Parameters(args): Parameters<EntityListArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut query: Vec<(&str, String)> = vec![
            ("tenant", self.resolve_tenant(args.tenant.as_ref())),
            ("limit", args.limit.unwrap_or(50).to_string()),
        ];
        if let Some(s) = args.kind.filter(|s| !s.is_empty()) {
            query.push(("kind", s));
        }
        if let Some(s) = args.q.filter(|s| !s.is_empty()) {
            query.push(("q", s));
        }
        self.get_with_query("entities", &query).await
    }

    // ------------------- embeddings_list_jobs (admin) -------------------
    #[tool(description = "Admin: list embedding jobs (requires MEM_MCP_EXPOSE_EMBEDDINGS=1).")]
    async fn embeddings_list_jobs(
        &self,
        Parameters(args): Parameters<EmbeddingsListJobsArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !self.expose_embeddings {
            return Ok(embeddings_disabled_error());
        }
        let mut query: Vec<(&str, String)> = vec![
            ("tenant", self.resolve_tenant(args.tenant.as_ref())),
            ("limit", args.limit.unwrap_or(200).to_string()),
        ];
        if let Some(s) = args.status.filter(|s| !s.is_empty()) {
            query.push(("status", s));
        }
        if let Some(m) = args.capability_capsule_id.filter(|s| !s.is_empty()) {
            query.push(("capability_capsule_id", m));
        }
        self.get_with_query("embeddings/jobs", &query).await
    }

    // ------------------- embeddings_rebuild (admin) -------------------
    #[tool(
        description = "Admin: enqueue embedding rebuild; force clears vector row and stale live jobs server-side."
    )]
    async fn embeddings_rebuild(
        &self,
        Parameters(args): Parameters<EmbeddingsRebuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !self.expose_embeddings {
            return Ok(embeddings_disabled_error());
        }
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "capability_capsule_ids": args.capability_capsule_ids.unwrap_or_default(),
            "force": args.force.unwrap_or(false),
        });
        self.post_json("embeddings/rebuild", &body).await
    }

    // ------------------- embeddings_providers (admin) -------------------
    #[tool(description = "Admin: describe configured embedding provider and dimension.")]
    async fn embeddings_providers(
        &self,
        Parameters(_): Parameters<EmptyArgs>,
    ) -> Result<CallToolResult, McpError> {
        if !self.expose_embeddings {
            return Ok(embeddings_disabled_error());
        }
        self.get_with_query("embeddings/providers", &[]).await
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for MemMcpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::V_2024_11_05;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut server_info = Implementation::default();
        server_info.name = "mem-mcp".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.server_info = server_info;
        info.instructions = Some(
            "Memory MCP server (Rust). Tools forward to the local mem HTTP service \
             configured by MEM_BASE_URL (default http://127.0.0.1:3000). The default \
             tenant comes from MEM_TENANT (default \"local\"). Set \
             MEM_MCP_EXPOSE_EMBEDDINGS=1 to enable admin embeddings_* tools."
                .to_string(),
        );
        info
    }
}
