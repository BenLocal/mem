use reqwest::Method;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
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
pub struct MemorySearchArgs {
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
pub struct MemoryBootstrapArgs {
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
pub struct MemorySearchContextualArgs {
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
pub struct MemoryIngestArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    /// One of: implementation | experience | preference | episode | workflow
    pub memory_type: String,
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
pub struct MemoryCommitFactArgs {
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
pub struct MemoryProposePreferenceArgs {
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
pub struct MemoryProposeExperienceArgs {
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
pub struct MemoryGetArgs {
    pub memory_id: String,
    /// Defaults to MEM_TENANT when omitted.
    #[serde(default)]
    pub tenant: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryFeedbackArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub memory_id: String,
    /// One of: useful | outdated | incorrect | applies_here | does_not_apply_here
    pub feedback_kind: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MemoryApplyFeedbackArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub project: String,
    pub caller_agent: String,
    pub memory_id: String,
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
    pub memory_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewEditAcceptArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    pub memory_id: String,
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbeddingsListJobsArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub memory_id: Option<String>,
    /// 1..=10000, default 200
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmbeddingsRebuildArgs {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub memory_ids: Option<Vec<String>>,
    #[serde(default)]
    pub force: Option<bool>,
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

    // ------------------- memory_search -------------------
    #[tool(
        description = "Search the shared mem service for compressed directives, facts, and patterns. Call early in a task; use scope_filters like repo:<name> to narrow results."
    )]
    async fn memory_search(
        &self,
        Parameters(args): Parameters<MemorySearchArgs>,
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
        self.post_json("memories/search", &body).await
    }

    // ------------------- memory_bootstrap -------------------
    #[tool(
        description = "Lightweight project-only bootstrap search for task-start context recovery."
    )]
    async fn memory_bootstrap(
        &self,
        Parameters(args): Parameters<MemoryBootstrapArgs>,
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
            .request_json(Method::POST, "memories/search", Some(&body))
            .await
        {
            Ok(v) => Ok(ok_json(&pick_search_summary(&v))),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- memory_search_contextual -------------------
    #[tool(
        description = "Intent-aware search for implementation, debugging, or review. Defaults to project scope and only widens when explicitly requested."
    )]
    async fn memory_search_contextual(
        &self,
        Parameters(args): Parameters<MemorySearchContextualArgs>,
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
            .request_json(Method::POST, "memories/search", Some(&body))
            .await
        {
            Ok(v) => Ok(ok_json(&pick_search_summary(&v))),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- memory_ingest -------------------
    #[tool(
        description = "Create a memory in mem. Use write_mode propose for preferences; auto is fine for implementation facts."
    )]
    async fn memory_ingest(
        &self,
        Parameters(args): Parameters<MemoryIngestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("memory_type".into(), json!(args.memory_type));
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
            .request_json(Method::POST, "memories", Some(&Value::Object(body)))
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Memory saved", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- memory_commit_fact -------------------
    #[tool(description = "Commit a verified project fact. Uses auto write mode and project scope.")]
    async fn memory_commit_fact(
        &self,
        Parameters(args): Parameters<MemoryCommitFactArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut tags: Vec<String> = args.tags.unwrap_or_default();
        tags.push(format!("caller_agent:{}", args.caller_agent));
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("memory_type".into(), json!("implementation"));
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
            .request_json(Method::POST, "memories", Some(&Value::Object(body)))
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Fact committed", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- memory_propose_preference -------------------
    #[tool(
        description = "Propose a preference for review. Uses the standard memories endpoint with write_mode=propose."
    )]
    async fn memory_propose_preference(
        &self,
        Parameters(args): Parameters<MemoryProposePreferenceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = Map::new();
        body.insert(
            "tenant".into(),
            json!(self.resolve_tenant(args.tenant.as_ref())),
        );
        body.insert("memory_type".into(), json!("preference"));
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
            .request_json(Method::POST, "memories", Some(&Value::Object(body)))
            .await
        {
            Ok(v) => Ok(ok_json_with_content("✓ Preference proposed", &content, &v)),
            Err(e) => Ok(err_text(e.to_string())),
        }
    }

    // ------------------- memory_propose_experience -------------------
    #[tool(
        description = "Propose a candidate experience by recording it as an episode instead of a strong fact."
    )]
    async fn memory_propose_experience(
        &self,
        Parameters(args): Parameters<MemoryProposeExperienceArgs>,
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
    async fn memory_get(
        &self,
        Parameters(args): Parameters<MemoryGetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        let path = format!("memories/{}", encode_segment(&args.memory_id));
        self.get_with_query(&path, &[("tenant", tenant)]).await
    }

    // ------------------- memory_feedback -------------------
    #[tool(description = "Record feedback on a memory to adjust future ranking.")]
    async fn memory_feedback(
        &self,
        Parameters(args): Parameters<MemoryFeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "memory_id": args.memory_id,
            "feedback_kind": args.feedback_kind,
        });
        self.post_json("memories/feedback", &body).await
    }

    // ------------------- memory_apply_feedback -------------------
    #[tool(
        description = "Apply limited feedback on a memory while keeping the existing POST /memories/feedback backend contract."
    )]
    async fn memory_apply_feedback(
        &self,
        Parameters(args): Parameters<MemoryApplyFeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "memory_id": args.memory_id,
            "feedback_kind": args.kind,
        });
        self.post_json("memories/feedback", &body).await
    }

    // ------------------- memory_list_pending_review -------------------
    #[tool(description = "List memories awaiting human confirmation for this tenant.")]
    async fn memory_list_pending_review(
        &self,
        Parameters(args): Parameters<TenantOnlyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tenant = self.resolve_tenant(args.tenant.as_ref());
        self.get_with_query("reviews/pending", &[("tenant", tenant)])
            .await
    }

    // ------------------- memory_review_accept -------------------
    #[tool(
        description = "Accept a pending memory (activate without edits). Use after human confirms."
    )]
    async fn memory_review_accept(
        &self,
        Parameters(args): Parameters<ReviewSimpleArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "memory_id": args.memory_id,
        });
        self.post_json("reviews/pending/accept", &body).await
    }

    // ------------------- memory_review_reject -------------------
    #[tool(description = "Reject a pending memory (mark rejected, no successor).")]
    async fn memory_review_reject(
        &self,
        Parameters(args): Parameters<ReviewSimpleArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "memory_id": args.memory_id,
        });
        self.post_json("reviews/pending/reject", &body).await
    }

    // ------------------- memory_review_edit_accept -------------------
    #[tool(
        description = "Edit pending memory content then accept: creates an active successor and rejects the original pending row."
    )]
    async fn memory_review_edit_accept(
        &self,
        Parameters(args): Parameters<ReviewEditAcceptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "tenant": self.resolve_tenant(args.tenant.as_ref()),
            "memory_id": args.memory_id,
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

    // ------------------- memory_graph_neighbors -------------------
    #[tool(
        description = "List graph edges adjacent to a node id (e.g. module:mem:billing, project:acme). Complements memory_search when expand_graph is not enough."
    )]
    async fn memory_graph_neighbors(
        &self,
        Parameters(args): Parameters<GraphNeighborsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("graph/neighbors/{}", encode_segment(&args.node_id));
        self.get_with_query(&path, &[]).await
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
        if let Some(m) = args.memory_id.filter(|s| !s.is_empty()) {
            query.push(("memory_id", m));
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
            "memory_ids": args.memory_ids.unwrap_or_default(),
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
