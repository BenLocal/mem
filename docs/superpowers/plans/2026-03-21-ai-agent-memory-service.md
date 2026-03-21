# AI Agent Memory Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a local-first Rust service that lets multiple agents write, review, inspect, search, and reuse engineering memory stored in DuckDB with graph enrichment through IndraDB.

**Architecture:** The service is an `axum` HTTP API with a focused domain core, a deterministic memory pipeline, a DuckDB-backed source of truth, and an IndraDB adapter for selective graph edges. The MVP must support the full lifecycle end-to-end: ingest, pending review, detail lookup, graph diagnostics, feedback, episode capture, workflow extraction, and compressed retrieval.

**Tech Stack:** Rust, Cargo, Axum, Tokio, Serde, DuckDB, IndraDB client/integration layer, UUID, Chrono, Tracing, ThisError, Reqwest, Tempfile

---

## Scope Check

The spec covers several concerns, but they are still one implementable MVP if kept narrow:

- One local HTTP service
- One canonical DuckDB store
- One selective IndraDB integration boundary
- One deterministic retrieval and compression path
- One explicit review queue for preference and workflow memory
- One heuristic workflow extractor built from stored episodes

Do not expand this plan into distributed deployment, authentication, browser UI, asynchronous workers, or generalized process mining.

## File Structure

The implementation should start with this file layout so responsibilities remain stable and testable.

- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/app.rs`
- Create: `src/config.rs`
- Create: `src/error.rs`
- Create: `src/http/mod.rs`
- Create: `src/http/health.rs`
- Create: `src/http/memory.rs`
- Create: `src/http/review.rs`
- Create: `src/http/graph.rs`
- Create: `src/domain/mod.rs`
- Create: `src/domain/memory.rs`
- Create: `src/domain/workflow.rs`
- Create: `src/domain/episode.rs`
- Create: `src/domain/query.rs`
- Create: `src/pipeline/mod.rs`
- Create: `src/pipeline/ingest.rs`
- Create: `src/pipeline/retrieve.rs`
- Create: `src/pipeline/compress.rs`
- Create: `src/pipeline/workflow.rs`
- Create: `src/storage/mod.rs`
- Create: `src/storage/duckdb.rs`
- Create: `src/storage/graph.rs`
- Create: `src/storage/schema.rs`
- Create: `src/service/mod.rs`
- Create: `src/service/memory_service.rs`
- Create: `db/schema/001_init.sql`
- Create: `db/dev/.gitkeep`
- Create: `tests/health_api.rs`
- Create: `tests/ingest_api.rs`
- Create: `tests/review_api.rs`
- Create: `tests/search_api.rs`
- Create: `tests/workflow_pipeline.rs`
- Create: `tests/graph_adapter.rs`
- Create: `README.md`

### File responsibilities

- `src/main.rs`: process bootstrap, tracing, config loading, server startup
- `src/app.rs`: app wiring and shared state construction
- `src/config.rs`: local config defaults and path resolution
- `src/error.rs`: domain/storage/service errors mapped to HTTP responses
- `src/http/*.rs`: route registration and thin handlers
- `src/domain/memory.rs`: memory types, lifecycle enums, scope, visibility, DTOs
- `src/domain/workflow.rs`: workflow candidates and workflow response shapes
- `src/domain/episode.rs`: episode ingest payloads and stored episode records
- `src/domain/query.rs`: search requests, ranking hints, compressed response structs
- `src/pipeline/ingest.rs`: ingest routing, lifecycle initialization, dedupe hooks, graph extraction hooks
- `src/pipeline/retrieve.rs`: candidate lookup, deterministic ranking, scope biasing, staleness penalties
- `src/pipeline/compress.rs`: token-budget-aware context pack building
- `src/pipeline/workflow.rs`: heuristic workflow extraction from repeated successful episodes
- `src/storage/duckdb.rs`: repository implementation for memory, feedback, reviews, episodes, and detail lookup
- `src/storage/graph.rs`: graph port plus no-op and IndraDB adapters
- `src/storage/schema.rs`: schema bootstrap helpers and migrations
- `src/service/memory_service.rs`: orchestration layer used by HTTP handlers and tests
- `db/schema/001_init.sql`: base tables and indexes
- `tests/*.rs`: API and repository integration tests with temp DuckDB databases

## Required Contracts

The implementer should not invent the core data model during execution. Use these minimum contracts from the start.

### Memory record contract

Every stored memory record must include these fields:

- `memory_id`
- `tenant`
- `memory_type`
- `status`
- `scope`
- `visibility`
- `version`
- `summary`
- `content`
- `evidence`
- `code_refs`
- `project`
- `repo`
- `module`
- `task_type`
- `tags`
- `confidence`
- `decay_score`
- `content_hash`
- `idempotency_key`
- `supersedes_memory_id`
- `source_agent`
- `created_at`
- `updated_at`
- `last_validated_at`

### Ingest contract rules

- Writable on ingest: `memory_type`, `content`, `evidence`, `code_refs`, `scope`, `visibility`, `project`, `repo`, `module`, `task_type`, `tags`, `source_agent`, `tenant`, `idempotency_key`
- Computed on ingest: `memory_id`, `summary`, `status`, `version`, `confidence`, `decay_score`, `content_hash`, `created_at`, `updated_at`
- Updated later by lifecycle events: `last_validated_at`, `supersedes_memory_id`, `confidence`, `decay_score`, `status`, `version`

### Graph node ID conventions

Use stable string IDs from day one:

- `project:{project}`
- `repo:{repo}`
- `module:{repo}:{module}`
- `workflow:{workflow_id}`
- `memory:{memory_id}`

### Search request contract

`POST /memories/search` must accept at least:

- `query`
- `intent`
- `scope_filters`
- `token_budget`
- `caller_agent`
- `expand_graph`

### Search response contract

`SearchMemoryResponse` must use explicit DTOs:

```rust
pub struct DirectiveItem {
    pub memory_id: String,
    pub text: String,
    pub source_summary: String,
}

pub struct FactItem {
    pub memory_id: String,
    pub text: String,
    pub code_refs: Vec<String>,
    pub source_summary: String,
}

pub struct PatternItem {
    pub memory_id: String,
    pub text: String,
    pub applicability: Option<String>,
    pub source_summary: String,
}

pub struct WorkflowOutline {
    pub memory_id: String,
    pub goal: String,
    pub steps: Vec<String>,
    pub success_signals: Vec<String>,
}
```

### Review edit-accept contract

`POST /reviews/pending/edit_accept` must create a new active version instead of mutating the pending row in place.

- Request fields: `memory_id`, `summary`, `content`, `evidence`, `code_refs`, `tags`
- Lifecycle semantics:
  - keep the original pending row for auditability
  - mark the original pending row as `rejected`
  - create a new `active` row with `version = previous.version + 1`
  - set the new row `supersedes_memory_id = original.memory_id`

### Memory detail contract

`GET /memories/{id}` must return:

- the full memory record
- `version_chain`: prior and superseded versions in newest-to-oldest order
- `graph_links`: immediate graph neighbors from the graph adapter
- `feedback_summary`: counts by feedback type

## Implementation Order

Each task should leave the repo in a runnable state and end with a commit.

### Task 1: Bootstrap the Rust service skeleton

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`
- Create: `src/app.rs`
- Create: `src/config.rs`
- Create: `src/error.rs`
- Create: `src/http/mod.rs`
- Create: `src/http/health.rs`
- Test: `tests/health_api.rs`
- Modify: `README.md`

- [ ] **Step 1: Write the failing health API test**

```rust
#[tokio::test]
async fn health_endpoint_returns_ok() {
    let app = test_app().await;
    let response = app.get("/health").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await, "ok");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test health_endpoint_returns_ok -q`
Expected: FAIL because the crate and test app do not exist yet

- [ ] **Step 3: Create the crate and minimal router**

```rust
pub fn router() -> Router {
    Router::new().route("/health", get(|| async { "ok" }))
}
```

- [ ] **Step 4: Add `main.rs` bootstrapping**

```rust
#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    axum::serve(listener, app::router()).await?;
    Ok(())
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test health_endpoint_returns_ok -q`
Expected: PASS

- [ ] **Step 6: Smoke run the server**

Run: `cargo run`
Expected: service starts and serves `GET /health`

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml src/main.rs src/app.rs src/config.rs src/error.rs src/http/mod.rs src/http/health.rs tests/health_api.rs README.md
git commit -m "feat: bootstrap memory service skeleton"
```

### Task 2: Define the domain model and JSON contracts

**Files:**
- Create: `src/domain/mod.rs`
- Create: `src/domain/memory.rs`
- Create: `src/domain/workflow.rs`
- Create: `src/domain/episode.rs`
- Create: `src/domain/query.rs`
- Test: `tests/ingest_api.rs`
- Test: `tests/search_api.rs`

- [ ] **Step 1: Write failing serialization tests for status, scope, and write mode**

```rust
#[test]
fn ingest_request_serializes_expected_shape() {
    let request = IngestMemoryRequest {
        memory_type: MemoryType::Implementation,
        content: "cache invalidation rule".into(),
        scope: Scope::Repo,
        write_mode: WriteMode::Auto,
        ..Default::default()
    };
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["scope"], "repo");
    assert_eq!(value["write_mode"], "auto");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test ingest_request_serializes_expected_shape -q`
Expected: FAIL because domain types do not exist

- [ ] **Step 3: Implement the lifecycle, scope, visibility, and memory DTOs**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    PendingConfirmation,
    Provisional,
    Active,
    Archived,
    Rejected,
}
```

- [ ] **Step 4: Add the full ingest DTO with evidence, code refs, project metadata, and idempotency fields**

```rust
pub struct IngestMemoryRequest {
    pub tenant: String,
    pub memory_type: MemoryType,
    pub content: String,
    pub evidence: Vec<String>,
    pub code_refs: Vec<String>,
    pub scope: Scope,
    pub visibility: Visibility,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub module: Option<String>,
    pub task_type: Option<String>,
    pub tags: Vec<String>,
    pub source_agent: String,
    pub idempotency_key: Option<String>,
    pub write_mode: WriteMode,
}
```

- [ ] **Step 5: Add episode and compressed search response DTOs**

```rust
pub struct SearchMemoryResponse {
    pub directives: Vec<DirectiveItem>,
    pub relevant_facts: Vec<FactItem>,
    pub reusable_patterns: Vec<PatternItem>,
    pub suggested_workflow: Option<WorkflowOutline>,
}
```

- [ ] **Step 6: Run focused tests**

Run: `cargo test ingest_request_serializes_expected_shape -q`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/domain/mod.rs src/domain/memory.rs src/domain/workflow.rs src/domain/episode.rs src/domain/query.rs tests/ingest_api.rs tests/search_api.rs
git commit -m "feat: add memory domain contracts"
```

### Task 3: Implement DuckDB schema bootstrap and repository primitives

**Files:**
- Create: `db/schema/001_init.sql`
- Create: `src/storage/mod.rs`
- Create: `src/storage/schema.rs`
- Create: `src/storage/duckdb.rs`
- Test: `tests/ingest_api.rs`
- Test: `tests/review_api.rs`
- Test: `tests/workflow_pipeline.rs`

- [ ] **Step 1: Write a failing repository test for saving and fetching memory**

```rust
#[tokio::test]
async fn duckdb_repository_persists_memory_rows() {
    let repo = test_duckdb_repo().await;
    let saved = repo.insert_memory(sample_memory()).await.unwrap();
    let loaded = repo.get_memory(saved.memory_id).await.unwrap().unwrap();
    assert_eq!(loaded.summary, "cache invalidation rule");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test duckdb_repository_persists_memory_rows -q`
Expected: FAIL because schema and repository are missing

- [ ] **Step 3: Write the initial DuckDB schema with full MVP fields**

```sql
create table if not exists memories (
  memory_id text primary key,
  tenant text not null,
  memory_type text not null,
  status text not null,
  scope text not null,
  visibility text not null,
  version integer not null,
  summary text not null,
  content text not null,
  evidence_json text not null,
  code_refs_json text not null,
  project text,
  repo text,
  module text,
  task_type text,
  tags_json text not null,
  confidence double not null,
  decay_score double not null,
  content_hash text not null,
  idempotency_key text,
  supersedes_memory_id text,
  source_agent text not null,
  created_at text not null,
  updated_at text not null,
  last_validated_at text
);

create table if not exists episodes (
  episode_id text primary key,
  tenant text not null,
  goal text not null,
  steps_json text not null,
  outcome text not null,
  source_agent text not null,
  created_at text not null
);

create table if not exists feedback_events (
  feedback_id text primary key,
  memory_id text not null,
  feedback_kind text not null,
  created_at text not null
);
```

- [ ] **Step 4: Implement repository methods for memory, pending review, feedback, and episode storage**

```rust
pub async fn insert_memory(&self, memory: MemoryRecord) -> Result<MemoryRecord, StorageError> {
    self.conn.execute("insert into memories ...", params![...])?;
    Ok(memory)
}
```

- [ ] **Step 5: Run repository tests**

Run: `cargo test duckdb_repository_persists_memory_rows -q`
Expected: PASS

- [ ] **Step 6: Add a second repository test for feedback rows and episode persistence**

Run: `cargo test duckdb_repository_persists_memory_rows -q`
Expected: PASS with schema-backed memory persistence

Run: `cargo test storage_schema_bootstraps_feedback_and_episode_tables -q`
Expected: PASS for schema-backed feedback and episode tables

- [ ] **Step 7: Commit**

```bash
git add db/schema/001_init.sql src/storage/mod.rs src/storage/schema.rs src/storage/duckdb.rs tests/ingest_api.rs tests/review_api.rs tests/workflow_pipeline.rs
git commit -m "feat: add duckdb schema and repository primitives"
```

### Task 4: Build the ingest pipeline and `POST /memories`

**Files:**
- Create: `src/pipeline/mod.rs`
- Create: `src/pipeline/ingest.rs`
- Create: `src/service/mod.rs`
- Create: `src/service/memory_service.rs`
- Create: `src/http/memory.rs`
- Test: `tests/ingest_api.rs`

- [ ] **Step 1: Write a failing API test for automatic and confirmation-routed writes**

```rust
#[tokio::test]
async fn preference_memory_stays_pending_confirmation() {
    let app = test_app().await;
    let response = app.post_json("/memories", json!({
        "memory_type": "preference",
        "content": "prefer concise answers",
        "scope": "global",
        "write_mode": "propose"
    })).await;
    assert_eq!(response.status(), 201);
    assert_eq!(response.json()["status"], "pending_confirmation");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test preference_memory_stays_pending_confirmation -q`
Expected: FAIL because the ingest route does not exist

- [ ] **Step 3: Implement ingest classification and initial lifecycle routing**

```rust
pub fn initial_status(memory_type: &MemoryType, write_mode: &WriteMode) -> MemoryStatus {
    match (memory_type, write_mode) {
        (MemoryType::Preference | MemoryType::Workflow, _) => MemoryStatus::PendingConfirmation,
        (_, WriteMode::Auto) => MemoryStatus::Active,
        _ => MemoryStatus::Provisional,
    }
}
```

- [ ] **Step 4: Implement `POST /memories` through the service layer**

```rust
async fn ingest_memory(
    State(app): State<AppState>,
    Json(request): Json<IngestMemoryRequest>,
) -> Result<(StatusCode, Json<IngestMemoryResponse>), AppError> {
    let response = app.memory_service.ingest(request).await?;
    Ok((StatusCode::CREATED, Json(response)))
}
```

- [ ] **Step 5: Add idempotency and content-hash handling before persistence**

```rust
let content_hash = compute_content_hash(&request);
if let Some(existing) = repo.find_by_idempotency_or_hash(&request.idempotency_key, &content_hash).await? {
    return Ok(existing.into_response());
}
```

- [ ] **Step 6: Run focused ingest tests**

Run: `cargo test preference_memory_stays_pending_confirmation -q`
Expected: PASS

- [ ] **Step 7: Add a second test for implementation memory auto-activation**

Run: `cargo test ingest_api -q`
Expected: PASS for both memory-type paths

- [ ] **Step 8: Commit**

```bash
git add src/pipeline/mod.rs src/pipeline/ingest.rs src/service/mod.rs src/service/memory_service.rs src/http/memory.rs tests/ingest_api.rs
git commit -m "feat: add memory ingest pipeline"
```

### Task 5: Add review queue support for preference and workflow memory

**Files:**
- Create: `src/http/review.rs`
- Modify: `src/service/memory_service.rs`
- Modify: `src/storage/duckdb.rs`
- Test: `tests/review_api.rs`

- [ ] **Step 1: Write failing tests for listing, accepting, rejecting, and editing pending memories**

```rust
#[tokio::test]
async fn accepting_pending_memory_marks_it_active() {
    let app = seeded_app_with_pending_preference().await;
    let response = app.post_json("/reviews/pending/accept", json!({
        "memory_id": "mem_123"
    })).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["status"], "active");
}
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test accepting_pending_memory_marks_it_active -q`
Expected: FAIL because review endpoints do not exist

- [ ] **Step 3: Implement pending review queries plus accept, reject, and edit-accept transitions**

```rust
pub async fn reject_pending(&self, memory_id: &str) -> Result<MemoryRecord, StorageError> {
    self.update_status(memory_id, MemoryStatus::Rejected).await
}
```

- [ ] **Step 4: Implement edit-accept as versioned replacement, not in-place mutation**

```rust
pub async fn edit_and_accept_pending(
    &self,
    memory_id: &str,
    patch: EditPendingRequest,
) -> Result<MemoryRecord, ServiceError> {
    let original = self.repo.get_pending(memory_id).await?.ok_or(ServiceError::NotFound)?;
    self.repo.reject_pending(memory_id).await?;
    self.repo.create_superseding_active_version(original, patch).await
}
```

- [ ] **Step 5: Add the review endpoints**

```rust
Router::new()
    .route("/reviews/pending", get(list_pending))
    .route("/reviews/pending/accept", post(accept_pending))
    .route("/reviews/pending/reject", post(reject_pending))
    .route("/reviews/pending/edit_accept", post(edit_and_accept_pending))
```

- [ ] **Step 6: Run review API tests**

Run: `cargo test review_api -q`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/http/review.rs src/service/memory_service.rs src/storage/duckdb.rs tests/review_api.rs
git commit -m "feat: add pending memory review flow"
```

### Task 6: Add `GET /memories/{id}` and full memory detail retrieval

**Files:**
- Modify: `src/http/memory.rs`
- Modify: `src/service/memory_service.rs`
- Modify: `src/storage/duckdb.rs`
- Test: `tests/ingest_api.rs`

- [ ] **Step 1: Write a failing test for fetching a memory by id**

```rust
#[tokio::test]
async fn get_memory_returns_full_record() {
    let app = seeded_app_with_memory().await;
    let response = app.get("/memories/mem_123").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.json()["memory_id"], "mem_123");
    assert!(response.json()["content_hash"].is_string());
}
```

- [ ] **Step 2: Run the test to verify failure**

Run: `cargo test get_memory_returns_full_record -q`
Expected: FAIL because the detail endpoint does not exist

- [ ] **Step 3: Implement repository and service lookup by `memory_id`**

```rust
pub async fn get_memory(&self, memory_id: &str) -> Result<Option<MemoryRecord>, StorageError> {
    // select ... from memories where memory_id = ?
}
```

- [ ] **Step 4: Include version chain, graph links, and feedback summary in the detail response**

```rust
pub struct MemoryDetailResponse {
    pub memory: MemoryRecord,
    pub version_chain: Vec<MemoryVersionLink>,
    pub graph_links: Vec<GraphEdge>,
    pub feedback_summary: FeedbackSummary,
}
```

- [ ] **Step 5: Expose `GET /memories/{id}`**

Run: `cargo test get_memory_returns_full_record -q`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/http/memory.rs src/service/memory_service.rs src/storage/duckdb.rs tests/ingest_api.rs
git commit -m "feat: add memory detail lookup"
```

### Task 7: Add graph adapter and relation extraction hooks

**Files:**
- Create: `src/storage/graph.rs`
- Modify: `src/pipeline/ingest.rs`
- Modify: `src/service/memory_service.rs`
- Create: `src/http/graph.rs`
- Test: `tests/graph_adapter.rs`

- [ ] **Step 1: Write a failing test for graph extraction on ingest**

```rust
#[tokio::test]
async fn ingest_extracts_project_and_module_nodes() {
    let graph = test_graph_adapter();
    let memory = sample_impl_memory_for("billing", "invoice");
    graph.sync_memory(&memory).await.unwrap();
    let neighbors = graph.neighbors("module:invoice").await.unwrap();
    assert!(neighbors.iter().any(|edge| edge.relation == "applies_to"));
}
```

- [ ] **Step 2: Run the graph test to verify failure**

Run: `cargo test ingest_extracts_project_and_module_nodes -q`
Expected: FAIL because graph adapter is missing

- [ ] **Step 3: Implement a graph port with a no-op local adapter and IndraDB-backed adapter**

```rust
#[async_trait]
pub trait GraphStore: Send + Sync {
    async fn sync_memory(&self, memory: &MemoryRecord) -> Result<(), GraphError>;
    async fn neighbors(&self, node_id: &str) -> Result<Vec<GraphEdge>, GraphError>;
    async fn related_memory_ids(&self, node_ids: &[String]) -> Result<Vec<String>, GraphError>;
}
```

- [ ] **Step 4: Add deterministic relation extraction for project, repo, module, and workflow links**

```rust
fn extract_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    vec![
        GraphEdge::new(format!("memory:{}", memory.memory_id), format!("project:{}", memory.project.as_deref().unwrap_or("unknown")), "applies_to"),
        GraphEdge::new(format!("memory:{}", memory.memory_id), format!("repo:{}", memory.repo.as_deref().unwrap_or("unknown")), "observed_in"),
        GraphEdge::new(
            format!("memory:{}", memory.memory_id),
            format!("module:{}:{}", memory.repo.as_deref().unwrap_or("unknown"), memory.module.as_deref().unwrap_or("unknown")),
            "relevant_to",
        ),
    ]
}
```

- [ ] **Step 5: Add contradiction, supersession, and workflow graph edges**

```rust
fn extract_lifecycle_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    let mut edges = Vec::new();
    if let Some(previous) = &memory.supersedes_memory_id {
        edges.push(GraphEdge::new(format!("memory:{}", memory.memory_id), format!("memory:{}", previous), "supersedes"));
    }
    if memory.memory_type == MemoryType::Workflow {
        edges.push(GraphEdge::new(format!("memory:{}", memory.memory_id), format!("workflow:{}", memory.memory_id), "uses_workflow"));
    }
    edges
}
```

- [ ] **Step 6: Expose `GET /graph/neighbors/{node_id}`**

```rust
Router::new().route("/graph/neighbors/:node_id", get(graph_neighbors))
```

Run: `cargo test graph_adapter -q`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add src/storage/graph.rs src/pipeline/ingest.rs src/service/memory_service.rs src/http/graph.rs tests/graph_adapter.rs
git commit -m "feat: add graph integration adapter"
```

### Task 8: Implement search, ranking, and compressed context output

**Files:**
- Create: `src/pipeline/retrieve.rs`
- Create: `src/pipeline/compress.rs`
- Modify: `src/service/memory_service.rs`
- Modify: `src/storage/duckdb.rs`
- Modify: `src/http/memory.rs`
- Test: `tests/search_api.rs`

- [ ] **Step 1: Write a failing search API test for compressed output sections**

```rust
#[tokio::test]
async fn search_returns_compressed_memory_pack() {
    let app = seeded_app_for_search().await;
    let response = app.post_json("/memories/search", json!({
        "query": "how should I debug invoice retry failures",
        "intent": "debugging",
        "scope_filters": ["repo:billing"],
        "token_budget": 500,
        "caller_agent": "codex-worker",
        "expand_graph": true
    })).await;
    let body = response.json();
    assert_eq!(response.status(), 200);
    assert!(body["directives"].is_array());
    assert!(body["relevant_facts"].is_array());
    assert!(body["reusable_patterns"].is_array());
}
```

- [ ] **Step 2: Run the test to verify failure**

Run: `cargo test search_returns_compressed_memory_pack -q`
Expected: FAIL because search is not implemented

- [ ] **Step 3: Implement candidate lookup and deterministic ranking**

```rust
pub async fn search(&self, query: SearchMemoryRequest) -> Result<Vec<MemoryRecord>, ServiceError> {
    let candidates = self.repo.search_candidates(&query).await?;
    let expanded = maybe_expand_with_graph(candidates, &query, self.graph.as_ref()).await?;
    Ok(rank_candidates(expanded, &query))
}
```

- [ ] **Step 4: Implement graph-aware re-ranking and scope filtering**

```rust
pub async fn maybe_expand_with_graph(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    graph: &dyn GraphStore,
) -> Result<Vec<MemoryRecord>, ServiceError> {
    if !query.expand_graph {
        return Ok(candidates);
    }
    let node_ids = candidates.iter().filter_map(candidate_node_id).collect::<Vec<_>>();
    let neighbor_ids = graph.related_memory_ids(&node_ids).await?;
    merge_and_bias(candidates, neighbor_ids, query)
}
```

- [ ] **Step 5: Implement token-budget-aware compression**

```rust
pub fn compress(candidates: &[MemoryRecord], budget: usize) -> SearchMemoryResponse {
    SearchMemoryResponse {
        directives: top_directives(candidates, budget),
        relevant_facts: top_facts(candidates, budget),
        reusable_patterns: top_patterns(candidates, budget),
        suggested_workflow: top_workflow(candidates, budget),
    }
}
```

- [ ] **Step 6: Expose `POST /memories/search`**

Run: `cargo test search_api -q`
Expected: PASS

- [ ] **Step 7: Add tests for scope bias and stale-memory penalty**

Run: `cargo test search_api -q`
Expected: PASS with ranking assertions

- [ ] **Step 8: Commit**

```bash
git add src/pipeline/retrieve.rs src/pipeline/compress.rs src/service/memory_service.rs src/storage/duckdb.rs src/http/memory.rs tests/search_api.rs
git commit -m "feat: add compressed memory search"
```

### Task 9: Add feedback and memory lifecycle updates

**Files:**
- Modify: `src/http/memory.rs`
- Modify: `src/service/memory_service.rs`
- Modify: `src/storage/duckdb.rs`
- Test: `tests/search_api.rs`
- Test: `tests/review_api.rs`

- [ ] **Step 1: Write a failing test for negative feedback reducing recall priority**

```rust
#[tokio::test]
async fn negative_feedback_penalizes_future_recall() {
    let app = seeded_app_for_feedback().await;
    app.post_json("/memories/feedback", json!({
        "memory_id": "mem_old",
        "feedback": "outdated"
    })).await;
    let search = app.post_json("/memories/search", json!({
        "query": "invoice retry failure",
        "intent": "debugging",
        "token_budget": 300
    })).await;
    assert_ne!(search.json()["relevant_facts"][0]["memory_id"], "mem_old");
}
```

- [ ] **Step 2: Run the test to verify failure**

Run: `cargo test negative_feedback_penalizes_future_recall -q`
Expected: FAIL because feedback is not wired into ranking

- [ ] **Step 3: Persist feedback and update `confidence` / `decay_score`**

```rust
match feedback.kind {
    FeedbackKind::Useful => memory.confidence += 0.1,
    FeedbackKind::Outdated => memory.decay_score += 0.2,
    FeedbackKind::Incorrect => memory.status = MemoryStatus::Archived,
    FeedbackKind::AppliesHere => memory.confidence += 0.05,
    FeedbackKind::DoesNotApplyHere => memory.decay_score += 0.1,
}
```

- [ ] **Step 4: Expose `POST /memories/feedback` and re-use updated scores in retrieval**

Run: `cargo test negative_feedback_penalizes_future_recall -q`
Expected: PASS

- [ ] **Step 5: Run related API suites**

Run: `cargo test review_api search_api -q`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add src/http/memory.rs src/service/memory_service.rs src/storage/duckdb.rs tests/search_api.rs tests/review_api.rs
git commit -m "feat: add memory feedback lifecycle"
```

### Task 10: Add episode ingest and workflow memory extraction

**Files:**
- Create: `src/pipeline/workflow.rs`
- Modify: `src/domain/episode.rs`
- Modify: `src/pipeline/ingest.rs`
- Modify: `src/service/memory_service.rs`
- Modify: `src/storage/duckdb.rs`
- Modify: `src/http/memory.rs`
- Modify: `src/http/review.rs`
- Test: `tests/workflow_pipeline.rs`

- [ ] **Step 1: Write a failing API test for recording successful episodes**

```rust
#[tokio::test]
async fn ingest_episode_persists_successful_run() {
    let app = test_app().await;
    let response = app.post_json("/episodes", json!({
        "goal": "debug invoice retries",
        "steps": ["inspect logs", "trace job", "verify fix"],
        "outcome": "success"
    })).await;
    assert_eq!(response.status(), 201);
}
```

- [ ] **Step 2: Run the episode test to verify failure**

Run: `cargo test ingest_episode_persists_successful_run -q`
Expected: FAIL because episode ingest does not exist

- [ ] **Step 3: Implement episode storage and `POST /episodes`**

```rust
async fn ingest_episode(
    State(app): State<AppState>,
    Json(request): Json<IngestEpisodeRequest>,
) -> Result<(StatusCode, Json<EpisodeResponse>), AppError> {
    let response = app.memory_service.ingest_episode(request).await?;
    Ok((StatusCode::CREATED, Json(response)))
}
```

- [ ] **Step 4: Write a failing workflow extraction test from successful episodes**

```rust
#[tokio::test]
async fn repeated_successful_episodes_produce_workflow_candidate() {
    let service = test_memory_service().await;
    seed_successful_episode(&service, "debug invoice retries", vec!["inspect logs", "trace job", "verify fix"]).await;
    seed_successful_episode(&service, "debug invoice retries", vec!["inspect logs", "trace job", "verify fix"]).await;
    let pending = service.list_pending_reviews().await.unwrap();
    assert!(pending.iter().any(|m| m.memory_type == MemoryType::Workflow));
}
```

- [ ] **Step 5: Run the workflow test to verify failure**

Run: `cargo test repeated_successful_episodes_produce_workflow_candidate -q`
Expected: FAIL because workflow extraction does not exist

- [ ] **Step 6: Implement a heuristic workflow extractor**

```rust
pub fn maybe_extract_workflow(episodes: &[EpisodeRecord]) -> Option<WorkflowCandidate> {
    let stable_sequence = longest_shared_step_sequence(episodes)?;
    Some(WorkflowCandidate::from_sequence(stable_sequence))
}
```

- [ ] **Step 7: Route workflow candidates into pending confirmation**

Run: `cargo test workflow_pipeline -q`
Expected: PASS

- [ ] **Step 8: Verify workflow candidates are visible in the review queue**

Run: `cargo test review_api workflow_pipeline -q`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add src/pipeline/workflow.rs src/domain/episode.rs src/pipeline/ingest.rs src/service/memory_service.rs src/storage/duckdb.rs src/http/memory.rs src/http/review.rs tests/workflow_pipeline.rs
git commit -m "feat: add workflow memory extraction"
```

### Task 11: Polish developer workflow, docs, and full verification pass

**Files:**
- Modify: `README.md`
- Modify: `Cargo.toml`
- Modify: `tests/health_api.rs`
- Modify: `tests/ingest_api.rs`
- Modify: `tests/review_api.rs`
- Modify: `tests/search_api.rs`
- Modify: `tests/workflow_pipeline.rs`
- Modify: `tests/graph_adapter.rs`

- [ ] **Step 1: Add a final API smoke checklist to the README**

```bash
cargo run
curl localhost:3000/health
curl localhost:3000/memories/mem_123
curl localhost:3000/graph/neighbors/module:invoice
```

Expected: commands are documented with expected response shapes for local verification

- [ ] **Step 2: Add README setup instructions and example API calls**

```md
## Run locally

```bash
cargo run
curl localhost:3000/health
```
```

- [ ] **Step 3: Add dev-only helpers for temporary DuckDB files and deterministic fixtures**

```rust
pub fn temp_db_path() -> PathBuf {
    tempfile::tempdir().unwrap().into_path().join("memory.duckdb")
}
```

- [ ] **Step 4: Run the full test suite**

Run: `cargo test -q`
Expected: PASS

- [ ] **Step 5: Run formatting and linting**

Run: `cargo fmt --check`
Expected: PASS

Run: `cargo clippy --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 6: Manual smoke the core API flow**

Run: `cargo run`
Expected: `GET /health`, `POST /memories`, `GET /memories/{id}`, `GET /reviews/pending`, `POST /memories/search`, `POST /memories/feedback`, `POST /episodes`, and `GET /graph/neighbors/:node_id` all respond successfully against a local DuckDB file

- [ ] **Step 7: Commit**

```bash
git add README.md Cargo.toml tests/health_api.rs tests/ingest_api.rs tests/review_api.rs tests/search_api.rs tests/workflow_pipeline.rs tests/graph_adapter.rs
git commit -m "chore: document and verify memory service mvp"
```

## Notes for Implementers

- Keep the first retrieval path deterministic. If embeddings or LLM-assisted summarization are added later, hide them behind traits so the MVP remains testable.
- Treat IndraDB as an adapter boundary, not the source of truth. The service must still work in local development if graph sync is disabled or replaced with a no-op adapter.
- Do not add authentication, background workers, or browser UI in this implementation pass.
- Prefer integration tests that exercise the HTTP surface and service layer with a temporary DuckDB database.
- Keep workflow extraction intentionally heuristic in v1. The goal is to prove the product loop, not to solve process mining.
