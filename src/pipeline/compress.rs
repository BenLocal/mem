use tiktoken_rs::{o200k_base_singleton, CoreBPE};

use crate::domain::{
    capability_capsule::{CapabilityCapsuleRecord, CapabilityCapsuleType},
    query::{
        ConversationHighlight, ConversationSnippet, DirectiveItem, FactItem, PatternItem,
        SearchCapabilityCapsuleResponse,
    },
    workflow::WorkflowOutline,
};
use crate::service::RecentSession;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Directive,
    Fact,
    Pattern,
    Workflow,
}

pub fn compress(
    candidates: &[CapabilityCapsuleRecord],
    budget: usize,
) -> SearchCapabilityCapsuleResponse {
    if candidates.is_empty() || budget == 0 {
        return SearchCapabilityCapsuleResponse::default();
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
                    capability_capsule_id: memory.capability_capsule_id.clone(),
                    text: compress_text(directive_text(memory), directives_budget),
                    source_summary: compress_text(&memory.summary, directives_budget / 2 + 8),
                });
            }
            Section::Fact if facts_budget > 0 && relevant_facts.len() < max_items(facts_budget) => {
                relevant_facts.push(FactItem {
                    capability_capsule_id: memory.capability_capsule_id.clone(),
                    text: compress_text(fact_text(memory), facts_budget),
                    code_refs: memory.code_refs.clone(),
                    source_summary: compress_text(&memory.summary, facts_budget / 2 + 8),
                });
            }
            Section::Pattern
                if patterns_budget > 0 && reusable_patterns.len() < max_items(patterns_budget) =>
            {
                reusable_patterns.push(PatternItem {
                    capability_capsule_id: memory.capability_capsule_id.clone(),
                    text: compress_text(pattern_text(memory), patterns_budget),
                    applicability: applicability(memory),
                    source_summary: compress_text(&memory.summary, patterns_budget / 2 + 8),
                });
            }
            Section::Workflow if suggested_workflow.is_none() && workflow_budget > 0 => {
                suggested_workflow = Some(WorkflowOutline {
                    capability_capsule_id: memory.capability_capsule_id.clone(),
                    goal: compress_text(workflow_goal(memory), workflow_budget / 3 + 8),
                    steps: workflow_steps(memory, workflow_budget),
                    success_signals: workflow_success_signals(memory, workflow_budget),
                });
            }
            _ => {}
        }
    }

    SearchCapabilityCapsuleResponse {
        directives,
        relevant_facts,
        reusable_patterns,
        suggested_workflow,
        recent_conversations: Vec::new(),
    }
}

fn classify(memory: &CapabilityCapsuleRecord) -> Section {
    if matches!(
        memory.capability_capsule_type,
        CapabilityCapsuleType::Preference
    ) {
        return Section::Directive;
    }

    if matches!(
        memory.capability_capsule_type,
        CapabilityCapsuleType::Workflow
    ) {
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

fn directive_text(memory: &CapabilityCapsuleRecord) -> &str {
    if memory.summary.trim().is_empty() {
        &memory.content
    } else {
        &memory.summary
    }
}

fn fact_text(memory: &CapabilityCapsuleRecord) -> &str {
    if memory.content.trim().is_empty() {
        &memory.summary
    } else {
        &memory.content
    }
}

fn pattern_text(memory: &CapabilityCapsuleRecord) -> &str {
    if memory.content.contains('\n') {
        &memory.content
    } else {
        &memory.summary
    }
}

fn workflow_goal(memory: &CapabilityCapsuleRecord) -> &str {
    if memory.summary.trim().is_empty() {
        &memory.content
    } else {
        &memory.summary
    }
}

fn workflow_steps(memory: &CapabilityCapsuleRecord, budget: usize) -> Vec<String> {
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

fn workflow_success_signals(memory: &CapabilityCapsuleRecord, budget: usize) -> Vec<String> {
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

fn applicability(memory: &CapabilityCapsuleRecord) -> Option<String> {
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
    // O5: redact high-confidence secrets at the output layer (single choke point
    // for all compressed prose). Storage stays verbatim; only this derived text
    // is masked. Redact BEFORE the token-budget trim so a masked key is what the
    // budget shapes.
    let redacted = crate::pipeline::redact::redact_secrets(text);
    let text: &str = redacted.as_ref();
    let limit = budget.max(8);
    let bpe = tokenizer();
    let tokens = bpe.encode_with_special_tokens(text);
    if tokens.len() <= limit {
        return text.to_string();
    }
    // tiktoken's `decode` does a strict `String::from_utf8` and ERRORS when the
    // token cut lands inside a multi-byte char (routine for emoji / uncommon CJK,
    // which o200k_base splits into byte-fragment tokens). The old
    // `.unwrap_or_default()` silently turned that error into an empty snippet —
    // blanking facts / patterns / `source_summary` whenever a budget forced such
    // a cut. Decode the raw bytes and recover lossily, dropping only the dangling
    // partial char at the cut (one trailing U+FFFD) rather than the whole string.
    let bytes = bpe.decode_bytes(&tokens[..limit]).unwrap_or_default();
    let truncated = String::from_utf8_lossy(&bytes)
        .trim_end_matches('\u{FFFD}')
        .to_string();
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

/// Compress a wake-up `Vec<RecentSession>` into the
/// `ConversationSnippet` shape on `SearchCapabilityCapsuleResponse`.
/// Each session's per-block budget is `total / sessions / blocks`,
/// rounded down. Empty input → empty Vec; zero `total_budget` short-
/// circuits to empty (the `recent_conversations` field is then
/// omitted from the JSON via `serde(skip_serializing_if = Vec::is_empty)`).
pub fn compress_recent_sessions(
    sessions: Vec<RecentSession>,
    total_budget: usize,
) -> Vec<ConversationSnippet> {
    if sessions.is_empty() || total_budget == 0 {
        return Vec::new();
    }
    let n_sessions = sessions.len();
    let per_session = (total_budget / n_sessions).max(40);
    sessions
        .into_iter()
        .map(|s| {
            let n_blocks = s.highlights.len().max(1);
            let per_block = (per_session / n_blocks).max(20);
            let highlights = s
                .highlights
                .into_iter()
                .map(|m| ConversationHighlight {
                    message_block_id: m.message_block_id,
                    role: format!("{:?}", m.role).to_lowercase(),
                    block_type: format!("{:?}", m.block_type).to_lowercase(),
                    text: compress_text(&m.content, per_block),
                    created_at: m.created_at,
                })
                .collect();
            ConversationSnippet {
                session_id: s.session_id,
                last_at: s.last_at,
                block_count: s.block_count,
                caller_agent: s.caller_agent,
                highlights,
            }
        })
        .collect()
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

    #[test]
    fn compress_text_ascii_within_budget() {
        // Short input well under budget: must be returned verbatim.
        let input = "Hello world";
        let result = compress_text(input, 100);
        assert_eq!(result, input);
    }

    #[test]
    fn compress_text_redacts_secrets_o5() {
        // O5: a high-confidence secret in output prose is masked at this choke
        // point (default-on). Large budget → no truncation, so we test redaction
        // in isolation.
        let input = "we set the key sk-abcdEFGH1234ijklMNOP in the deploy config";
        let result = compress_text(input, 1000);
        assert!(
            result.contains("[redacted:sk]"),
            "secret not masked: {result}"
        );
        assert!(!result.contains("sk-abcdEFGH"), "secret leaked: {result}");
    }

    #[test]
    fn compress_text_ascii_exceeds_budget_breaks_at_whitespace() {
        // ~600 tokens of English; budget of 30 forces truncation.
        // The whitespace backstep in compress_text must ensure the result
        // does not end mid-word.
        let input: String = "The quick brown fox jumps over the lazy dog. ".repeat(60);
        let budget = 30;

        let result = compress_text(&input, budget);

        let bpe = tokenizer();
        let result_tokens = bpe.encode_with_special_tokens(&result).len();
        assert!(
            result_tokens <= budget,
            "ASCII budget violated: got {} tokens for budget {}",
            result_tokens,
            budget
        );
        // The whitespace backstep should ensure the result ends at a word/space
        // boundary: either the input contains "<result-trimmed> " immediately
        // after the result, or the result is a suffix of the input.
        let trimmed = result.trim_end();
        let space_marker = format!("{} ", trimmed);
        assert!(
            input.contains(&space_marker) || input.ends_with(trimmed),
            "result should end at a word boundary; got tail: {:?}",
            &result[result.len().saturating_sub(40)..]
        );
    }

    #[test]
    fn compress_text_mixed_cjk_ascii() {
        // Mixed CJK + ASCII content; token count should still be respected
        // and the result must be a prefix of the input (no reordering or
        // content insertion).
        let input: String =
            "项目 X uses HNSW for ANN queries 实现细节: see vector_index.rs. ".repeat(20);
        let budget = 25;

        let result = compress_text(&input, budget);

        let bpe = tokenizer();
        let result_tokens = bpe.encode_with_special_tokens(&result).len();
        assert!(
            result_tokens <= budget,
            "mixed budget violated: got {} tokens for budget {}",
            result_tokens,
            budget
        );
        // Result must be a prefix of the input (no content fabricated).
        assert!(
            input.starts_with(&result),
            "result must be a prefix of input"
        );
    }

    #[test]
    fn compress_text_zero_or_empty() {
        // Empty input: should never panic and must return empty.
        assert_eq!(compress_text("", 100), "");

        // budget=0 is clamped to budget.max(8) == 8 internally; the result
        // must still respect that effective budget.
        let result = compress_text("hello world this is a longer test sentence", 0);
        let bpe = tokenizer();
        let tokens = bpe.encode_with_special_tokens(&result).len();
        assert!(
            tokens <= 8,
            "budget=0 must clamp to 8; got {} tokens",
            tokens
        );
    }

    #[test]
    fn compress_text_exact_budget() {
        // When the input's token count exactly equals the budget, no truncation
        // should occur — the early-return branch (`tokens.len() <= limit`)
        // must fire and return the input verbatim.
        let input = "Hello, world!";
        let bpe = tokenizer();
        let n = bpe.encode_with_special_tokens(input).len();

        let result = compress_text(input, n);
        assert_eq!(result, input, "no truncation when token count == budget");
    }

    #[test]
    fn compress_text_truncation_never_blanks_on_split_char() {
        // tiktoken's strict `decode` errors when the token cut lands inside a
        // multi-byte char: o200k_base splits emoji and uncommon CJK into
        // byte-fragment tokens, so a prefix cut can end mid-char. The old
        // `.unwrap_or_default()` turned that into a silently EMPTY snippet (the
        // whitespace backstep can't help — no spaces at the cut). This content
        // (emoji + CJK, ubiquitous in real transcripts) has many such split
        // points; at every one, compress_text must still return non-empty,
        // prefix-valid prose within budget. (Common repeated Chinese maps
        // 1 token/char and never splits, which is why the budget=50 case above
        // passed and hid this bug.)
        let text: String = "任务✅完成了🚀部署到生产🔥环境🎉成功".repeat(6);
        let bpe = tokenizer();
        let toks = bpe.encode_with_special_tokens(text.as_str());

        // The fixture must actually contain mid-char splits, else the test is
        // vacuous and proves nothing.
        let split_budgets: Vec<usize> = (8..toks.len())
            .filter(|&n| bpe.decode(&toks[..n]).is_err())
            .collect();
        assert!(
            !split_budgets.is_empty(),
            "fixture must contain mid-char token splits to be a real regression"
        );

        for budget in split_budgets {
            let out = compress_text(&text, budget);
            assert!(
                !out.is_empty(),
                "compress_text blanked at mid-char split budget {budget}"
            );
            assert!(
                text.starts_with(&out),
                "result must stay a prefix of input at budget {budget}: {out:?}"
            );
            let out_tokens = bpe.encode_with_special_tokens(out.as_str()).len();
            assert!(
                out_tokens <= budget,
                "budget {budget} violated: {out_tokens} tokens"
            );
        }
    }
}
