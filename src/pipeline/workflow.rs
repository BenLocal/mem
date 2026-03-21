use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    episode::EpisodeRecord,
    memory::{IngestMemoryRequest, MemoryType, WriteMode},
    workflow::WorkflowCandidate,
};

#[derive(Debug, Clone)]
struct WorkflowGroup {
    representative_goal: String,
    representative_steps: Vec<String>,
    scope: crate::domain::memory::Scope,
    evidence: BTreeSet<String>,
    count: usize,
}

pub fn maybe_extract_workflow(episodes: &[EpisodeRecord]) -> Option<WorkflowCandidate> {
    let mut groups: BTreeMap<String, WorkflowGroup> = BTreeMap::new();

    for episode in episodes
        .iter()
        .filter(|episode| normalized_outcome(&episode.outcome) == "success")
    {
        let normalized_goal = normalize_text(&episode.goal);
        let normalized_steps = normalized_steps(&episode.steps);
        let key = workflow_key(&normalized_goal, &normalized_steps);

        let group = groups.entry(key).or_insert_with(|| WorkflowGroup {
            representative_goal: normalized_goal.clone(),
            representative_steps: normalized_steps.clone(),
            scope: episode.scope.clone(),
            evidence: BTreeSet::new(),
            count: 0,
        });
        group.count += 1;
        group.evidence.extend(episode.evidence.iter().cloned());
    }

    groups.into_values().find_map(|group| {
        (group.count >= 2).then(|| WorkflowCandidate {
            memory_id: None,
            goal: group.representative_goal,
            preconditions: Vec::new(),
            steps: group.representative_steps,
            decision_points: Vec::new(),
            success_signals: vec!["outcome == success".to_string()],
            failure_signals: Vec::new(),
            evidence: group.evidence.into_iter().collect(),
            scope: group.scope,
        })
    })
}

pub fn workflow_memory_request(
    episode: &EpisodeRecord,
    candidate: &WorkflowCandidate,
) -> IngestMemoryRequest {
    let mut tags = episode.tags.clone();
    if !tags.iter().any(|tag| tag == "workflow_candidate") {
        tags.push("workflow_candidate".to_string());
    }

    IngestMemoryRequest {
        tenant: episode.tenant.clone(),
        memory_type: MemoryType::Workflow,
        content: workflow_content(candidate),
        evidence: candidate.evidence.clone(),
        code_refs: Vec::new(),
        scope: candidate.scope.clone(),
        visibility: episode.visibility.clone(),
        project: episode.project.clone(),
        repo: episode.repo.clone(),
        module: episode.module.clone(),
        task_type: None,
        tags,
        source_agent: episode.source_agent.clone(),
        idempotency_key: Some(workflow_idempotency_key(&candidate.goal, &candidate.steps)),
        write_mode: WriteMode::Auto,
    }
}

fn workflow_content(candidate: &WorkflowCandidate) -> String {
    let mut content = String::new();
    content.push_str("workflow goal: ");
    content.push_str(&candidate.goal);
    content.push('\n');
    content.push_str("steps:\n");
    for step in &candidate.steps {
        content.push_str("- ");
        content.push_str(step);
        content.push('\n');
    }
    content.push_str("success signals:\n");
    for signal in &candidate.success_signals {
        content.push_str("- ");
        content.push_str(signal);
        content.push('\n');
    }
    content.trim_end().to_string()
}

fn normalized_outcome(value: &str) -> String {
    normalize_text(value)
}

fn normalize_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn normalized_steps(steps: &[String]) -> Vec<String> {
    steps.iter().map(|step| normalize_text(step)).collect()
}

fn workflow_key(goal: &str, steps: &[String]) -> String {
    format!("{goal}\u{1e}{}", steps.join("\u{1f}"))
}

fn workflow_idempotency_key(goal: &str, steps: &[String]) -> String {
    workflow_key(goal, steps)
}
