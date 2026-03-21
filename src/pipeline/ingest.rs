use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
};

use serde_json::json;

use crate::domain::memory::{GraphEdge, IngestMemoryRequest, MemoryRecord, MemoryStatus, MemoryType, WriteMode};

pub fn initial_status(memory_type: &MemoryType, write_mode: &WriteMode) -> MemoryStatus {
    match (memory_type, write_mode) {
        (MemoryType::Preference | MemoryType::Workflow, _) => MemoryStatus::PendingConfirmation,
        (_, WriteMode::Auto) => MemoryStatus::Active,
        _ => MemoryStatus::Provisional,
    }
}

pub fn compute_content_hash(request: &IngestMemoryRequest) -> String {
    let mut hasher = DefaultHasher::new();
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
    .to_string()
    .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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

pub fn extract_graph_edges(memory: &MemoryRecord) -> Vec<GraphEdge> {
    let mut edges = Vec::new();
    let from_node_id = memory_node_id(&memory.memory_id);

    if let Some(project) = memory.project.as_deref().filter(|value| !value.is_empty()) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: project_node_id(project),
            relation: "applies_to".into(),
        });
    }

    if let Some(repo) = memory.repo.as_deref().filter(|value| !value.is_empty()) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: repo_node_id(repo),
            relation: "observed_in".into(),
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
        });
    }

    if let Some(workflow_id) = memory.task_type.as_deref().filter(|value| !value.is_empty()) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: workflow_node_id(workflow_id),
            relation: "uses_workflow".into(),
        });
    } else if matches!(memory.memory_type, MemoryType::Workflow) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: workflow_node_id(&memory.memory_id),
            relation: "uses_workflow".into(),
        });
    }

    if let Some(previous) = memory.supersedes_memory_id.as_deref().filter(|value| !value.is_empty()) {
        edges.push(GraphEdge {
            from_node_id: from_node_id.clone(),
            to_node_id: memory_node_id(previous),
            relation: "supersedes".into(),
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
        });
    }

    edges
}
