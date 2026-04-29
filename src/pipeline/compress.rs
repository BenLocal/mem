use tiktoken_rs::{o200k_base_singleton, CoreBPE};

use crate::domain::{
    memory::{MemoryRecord, MemoryType},
    query::{DirectiveItem, FactItem, PatternItem, SearchMemoryResponse},
    workflow::WorkflowOutline,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Directive,
    Fact,
    Pattern,
    Workflow,
}

pub fn compress(candidates: &[MemoryRecord], budget: usize) -> SearchMemoryResponse {
    if candidates.is_empty() || budget == 0 {
        return SearchMemoryResponse::default();
    }

    let budget = budget.max(80);
    let directives_budget = budget * 30 / 100;
    let facts_budget = budget * 35 / 100;
    let patterns_budget = budget * 20 / 100;
    let workflow_budget = budget.saturating_sub(directives_budget + facts_budget + patterns_budget);

    let mut directives = Vec::new();
    let mut relevant_facts = Vec::new();
    let mut reusable_patterns = Vec::new();
    let mut suggested_workflow = None;

    for memory in candidates {
        match classify(memory) {
            Section::Directive
                if directives_budget > 0 && directives.len() < max_items(directives_budget) =>
            {
                directives.push(DirectiveItem {
                    memory_id: memory.memory_id.clone(),
                    text: compress_text(directive_text(memory), directives_budget),
                    source_summary: compress_text(&memory.summary, directives_budget / 2 + 8),
                });
            }
            Section::Fact if facts_budget > 0 && relevant_facts.len() < max_items(facts_budget) => {
                relevant_facts.push(FactItem {
                    memory_id: memory.memory_id.clone(),
                    text: compress_text(fact_text(memory), facts_budget),
                    code_refs: memory.code_refs.clone(),
                    source_summary: compress_text(&memory.summary, facts_budget / 2 + 8),
                });
            }
            Section::Pattern
                if patterns_budget > 0 && reusable_patterns.len() < max_items(patterns_budget) =>
            {
                reusable_patterns.push(PatternItem {
                    memory_id: memory.memory_id.clone(),
                    text: compress_text(pattern_text(memory), patterns_budget),
                    applicability: applicability(memory),
                    source_summary: compress_text(&memory.summary, patterns_budget / 2 + 8),
                });
            }
            Section::Workflow if suggested_workflow.is_none() && workflow_budget > 0 => {
                suggested_workflow = Some(WorkflowOutline {
                    memory_id: memory.memory_id.clone(),
                    goal: compress_text(workflow_goal(memory), workflow_budget / 3 + 8),
                    steps: workflow_steps(memory, workflow_budget),
                    success_signals: workflow_success_signals(memory, workflow_budget),
                });
            }
            _ => {}
        }
    }

    SearchMemoryResponse {
        directives,
        relevant_facts,
        reusable_patterns,
        suggested_workflow,
    }
}

fn classify(memory: &MemoryRecord) -> Section {
    if matches!(memory.memory_type, MemoryType::Preference) {
        return Section::Directive;
    }

    if matches!(memory.memory_type, MemoryType::Workflow) {
        return Section::Workflow;
    }

    let normalized = format!(
        "{} {} {} {}",
        memory.summary.to_lowercase(),
        memory.content.to_lowercase(),
        memory.tags.join(" ").to_lowercase(),
        memory.task_type.clone().unwrap_or_default().to_lowercase()
    );

    if normalized.contains("pattern")
        || normalized.contains("heuristic")
        || normalized.contains("workflow")
        || normalized.contains("repeat")
    {
        Section::Pattern
    } else {
        Section::Fact
    }
}

fn directive_text(memory: &MemoryRecord) -> &str {
    if memory.summary.trim().is_empty() {
        &memory.content
    } else {
        &memory.summary
    }
}

fn fact_text(memory: &MemoryRecord) -> &str {
    if memory.content.trim().is_empty() {
        &memory.summary
    } else {
        &memory.content
    }
}

fn pattern_text(memory: &MemoryRecord) -> &str {
    if memory.content.contains('\n') {
        &memory.content
    } else {
        &memory.summary
    }
}

fn workflow_goal(memory: &MemoryRecord) -> &str {
    if memory.summary.trim().is_empty() {
        &memory.content
    } else {
        &memory.summary
    }
}

fn workflow_steps(memory: &MemoryRecord, budget: usize) -> Vec<String> {
    let mut steps = split_steps(&memory.content);
    if steps.is_empty() {
        steps = split_steps(&memory.summary);
    }
    let limit = max_items(budget).max(2);
    steps
        .into_iter()
        .take(limit)
        .map(|step| compress_text(&step, budget / limit.max(1) + 6))
        .collect()
}

fn workflow_success_signals(memory: &MemoryRecord, budget: usize) -> Vec<String> {
    let mut signals = vec![];
    if !memory.evidence.is_empty() {
        signals.push(memory.evidence.join(", "));
    }
    if !memory.code_refs.is_empty() {
        signals.push(memory.code_refs.join(", "));
    }
    if signals.is_empty() {
        signals.push("task completed without rollback".into());
    }
    let limit = max_items(budget).max(1);
    signals
        .into_iter()
        .take(limit)
        .map(|signal| compress_text(&signal, budget / limit.max(1) + 4))
        .collect()
}

fn applicability(memory: &MemoryRecord) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(project) = memory.project.as_deref() {
        parts.push(format!("project {project}"));
    }
    if let Some(repo) = memory.repo.as_deref() {
        parts.push(format!("repo {repo}"));
    }
    if let Some(module) = memory.module.as_deref() {
        parts.push(format!("module {module}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

fn split_steps(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(|line| line.split([';', '•']))
        .map(|part| part.trim().trim_start_matches('-').trim())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

fn tokenizer() -> &'static CoreBPE {
    o200k_base_singleton()
}

fn compress_text(text: &str, budget: usize) -> String {
    let limit = budget.max(8);
    let bpe = tokenizer();
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.len() <= limit {
        return text.to_string();
    }
    let truncated = bpe.decode(&tokens[..limit]).unwrap_or_default();
    if let Some(last_space) = truncated.rfind(char::is_whitespace) {
        if last_space > truncated.len() / 2 {
            return truncated[..last_space].trim_end().to_string();
        }
    }
    truncated
}

fn max_items(budget: usize) -> usize {
    match budget {
        0..=120 => 1,
        121..=250 => 2,
        251..=450 => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_text_cjk_respects_token_budget() {
        // Long Chinese paragraph; o200k_base encodes CJK at roughly 1 token/char.
        // Pre-fix `chars × 3` heuristic returns ~3× the budget for pure CJK.
        let cjk: String =
            "机器学习是一个广泛的研究领域涵盖了从统计学到神经网络的多种方法".repeat(20);
        let budget = 50;

        let result = compress_text(&cjk, budget);

        let bpe = tokenizer();
        let result_tokens = bpe.encode_with_special_tokens(&result).len();
        assert!(
            result_tokens <= budget,
            "CJK budget violated: got {} tokens for budget {}",
            result_tokens,
            budget
        );
        assert!(!result.is_empty(), "expected nonempty truncation");
        assert!(cjk.starts_with(&result), "result must be a prefix of input");
    }
}
