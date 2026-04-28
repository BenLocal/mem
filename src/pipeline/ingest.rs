use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::domain::memory::{
    GraphEdge, IngestMemoryRequest, MemoryRecord, MemoryStatus, MemoryType, WriteMode,
};

/// Length of `compute_content_hash` output (sha256 hex). Used by the storage
/// migration to identify legacy rows that still hold the old DefaultHasher
/// 16-char hash.
pub const CONTENT_HASH_LEN: usize = 64;

/// Validate that the request follows verbatim discipline: `content` must not
/// be identical to a non-empty `summary`. Returns `Err` if validation fails.
pub fn validate_verbatim(request: &IngestMemoryRequest, summary: &str) -> Result<(), String> {
    if !summary.is_empty() && summary.len() > 80 && summary == request.content {
        return Err("summary must not be identical to content (verbatim violation)".into());
    }
    Ok(())
}

pub fn initial_status(memory_type: &MemoryType, write_mode: &WriteMode) -> MemoryStatus {
    match (memory_type, write_mode) {
        (MemoryType::Preference | MemoryType::Workflow, _) => MemoryStatus::PendingConfirmation,
        (_, WriteMode::Auto) => MemoryStatus::Active,
        _ => MemoryStatus::Provisional,
    }
}

pub fn compute_content_hash(request: &IngestMemoryRequest) -> String {
    hash_canonical(&canonical_request_json(request))
}

/// Recompute the content hash from a stored `MemoryRecord`. Equivalent to
/// hashing the original `IngestMemoryRequest`, used by the migration that
/// upgrades pre-sha256 rows.
pub fn compute_content_hash_from_record(record: &MemoryRecord) -> String {
    hash_canonical(&canonical_record_json(record))
}

fn canonical_request_json(request: &IngestMemoryRequest) -> Value {
    json!({
        "tenant": request.tenant,
        "memory_type": request.memory_type,
        "content": request.content,
        "evidence": request.evidence,
        "code_refs": request.code_refs,
        "scope": request.scope,
        "visibility": request.visibility,
        "project": request.project,
        "repo": request.repo,
        "module": request.module,
        "task_type": request.task_type,
        "tags": request.tags,
        "source_agent": request.source_agent,
    })
}

fn canonical_record_json(record: &MemoryRecord) -> Value {
    json!({
        "tenant": record.tenant,
        "memory_type": record.memory_type,
        "content": record.content,
        "evidence": record.evidence,
        "code_refs": record.code_refs,
        "scope": record.scope,
        "visibility": record.visibility,
        "project": record.project,
        "repo": record.repo,
        "module": record.module,
        "task_type": record.task_type,
        "tags": record.tags,
        "source_agent": record.source_agent,
    })
}

fn hash_canonical(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn memory_node_id(memory_id: &str) -> String {
    format!("memory:{memory_id}")
}

pub fn project_node_id(project: &str) -> String {
    format!("project:{project}")
}

pub fn repo_node_id(repo: &str) -> String {
    format!("repo:{repo}")
}

pub fn module_node_id(repo: &str, module: &str) -> String {
    format!("module:{repo}:{module}")
}

pub fn workflow_node_id(workflow_id: &str) -> String {
    format!("workflow:{workflow_id}")
}

/// Extract graph edges derived from a memory's fields.
///
/// Returned `GraphEdge`s have `valid_from = String::new()` and `valid_to = None`
/// as placeholders. The storage layer (`DuckDbGraphStore::sync_memory`, added in
/// later tasks) overwrites `valid_from` with the current timestamp at write time.
/// Keeping this function pure (no clock dependency) lets us test it without
/// time mocking.
pub fn extract_graph_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    let mut edges = Vec::new();
    let from_node_id = memory_node_id(&memory.memory_id);

    if let Some(project) = memory.project.as_deref().filter(|value| !value.is_empty()) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: project_node_id(project),
            relation: "applies_to".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    if let Some(repo) = memory.repo.as_deref().filter(|value| !value.is_empty()) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: repo_node_id(repo),
            relation: "observed_in".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    if let (Some(repo), Some(module)) = (
        memory.repo.as_deref().filter(|value| !value.is_empty()),
        memory.module.as_deref().filter(|value| !value.is_empty()),
    ) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: module_node_id(repo, module),
            relation: "relevant_to".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    if let Some(workflow_id) = memory
        .task_type
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: workflow_node_id(workflow_id),
            relation: "uses_workflow".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    } else if matches!(memory.memory_type, MemoryType::Workflow) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: workflow_node_id(&memory.memory_id),
            relation: "uses_workflow".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    if let Some(previous) = memory
        .supersedes_memory_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: memory_node_id(previous),
            relation: "supersedes".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    for contradicted in memory
        .tags
        .iter()
        .filter_map(|tag| tag.strip_prefix("contradicts:"))
        .filter(|value| !value.is_empty())
    {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: memory_node_id(contradicted),
            relation: "contradicts".into(),
            valid_from: String::new(),
            valid_to: None,
        });
    }

    edges
}
