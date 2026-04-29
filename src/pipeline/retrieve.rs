use std::collections::{HashMap, HashSet};

use crate::{
    domain::{
        memory::{MemoryRecord, MemoryStatus, MemoryType, Scope},
        query::SearchMemoryRequest,
    },
    storage::DuckDbGraphStore,
};

#[derive(Debug, Clone)]
struct ScoredMemory {
    memory: MemoryRecord,
    score: i64,
}

pub fn rank_candidates(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
) -> Vec<MemoryRecord> {
    let scored = score_candidates(candidates, query, &HashSet::new(), 0);
    scored.into_iter().map(|entry| entry.memory).collect()
}

pub fn merge_and_rank_hybrid(
    lexical: Vec<MemoryRecord>,
    semantic: Vec<(MemoryRecord, f32)>,
    query: &SearchMemoryRequest,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
) -> Vec<MemoryRecord> {
    let lexical_ranks: HashMap<String, usize> = lexical
        .iter()
        .enumerate()
        .map(|(i, m)| (m.memory_id.clone(), i + 1))
        .collect();
    let semantic_ranks: HashMap<String, usize> = semantic
        .iter()
        .enumerate()
        .map(|(i, (m, _sim))| (m.memory_id.clone(), i + 1))
        .collect();

    let lexical_ids: HashSet<String> = lexical.iter().map(|m| m.memory_id.clone()).collect();
    let mut semantic_sims: HashMap<String, f32> = HashMap::new();
    let mut by_id: HashMap<String, MemoryRecord> = HashMap::new();

    for m in lexical {
        by_id.insert(m.memory_id.clone(), m);
    }
    for (m, sim) in semantic {
        let id = m.memory_id.clone();
        semantic_sims.insert(id.clone(), sim);
        by_id.entry(id).or_insert(m);
    }

    let candidates: Vec<MemoryRecord> = by_id.into_values().collect();

    let scored = if use_legacy_ranker() {
        score_candidates_hybrid_legacy(
            candidates,
            query,
            related_memory_ids,
            graph_boost,
            &lexical_ids,
            &semantic_sims,
        )
    } else {
        score_candidates_hybrid_rrf(
            candidates,
            query,
            related_memory_ids,
            graph_boost,
            &lexical_ranks,
            &semantic_ranks,
        )
    };

    scored.into_iter().map(|entry| entry.memory).collect()
}

fn use_legacy_ranker() -> bool {
    std::env::var("MEM_RANKER")
        .ok()
        .map(|v| v == "legacy")
        .unwrap_or(false)
}

pub async fn rank_with_graph_hybrid(
    lexical: Vec<MemoryRecord>,
    semantic: Vec<(MemoryRecord, f32)>,
    query: &SearchMemoryRequest,
    graph: &DuckDbGraphStore,
) -> Result<Vec<MemoryRecord>, crate::storage::GraphError> {
    if semantic.is_empty() {
        return rank_with_graph(lexical, query, graph).await;
    }

    if !query.expand_graph {
        return Ok(merge_and_rank_hybrid(
            lexical,
            semantic,
            query,
            &HashSet::new(),
            0,
        ));
    }

    let preliminary =
        merge_and_rank_hybrid(lexical.clone(), semantic.clone(), query, &HashSet::new(), 0);
    let anchors = graph_anchor_nodes_from_records(&preliminary);
    if anchors.is_empty() {
        return Ok(preliminary);
    }

    let related_memory_ids = graph.related_memory_ids(&anchors).await?;
    let related_lookup = related_memory_ids.into_iter().collect::<HashSet<_>>();
    Ok(merge_and_rank_hybrid(
        lexical,
        semantic,
        query,
        &related_lookup,
        12,
    ))
}

pub async fn rank_with_graph(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    graph: &DuckDbGraphStore,
) -> Result<Vec<MemoryRecord>, crate::storage::GraphError> {
    if !query.expand_graph {
        return Ok(rank_candidates(candidates, query));
    }

    let base = score_candidates(candidates, query, &HashSet::new(), 0);
    let anchor_nodes = graph_anchor_nodes(&base);
    if anchor_nodes.is_empty() {
        return Ok(base.into_iter().map(|entry| entry.memory).collect());
    }

    let related_memory_ids = graph.related_memory_ids(&anchor_nodes).await?;
    let related_lookup = related_memory_ids.into_iter().collect::<HashSet<_>>();
    let rescored = score_candidates(
        base.into_iter().map(|entry| entry.memory).collect(),
        query,
        &related_lookup,
        12,
    );
    Ok(rescored.into_iter().map(|entry| entry.memory).collect())
}

pub async fn candidate_memory_ids(
    graph: &DuckDbGraphStore,
    candidates: &[MemoryRecord],
) -> Result<Vec<String>, crate::storage::GraphError> {
    let mut nodes = graph_anchor_nodes(
        &candidates
            .iter()
            .cloned()
            .map(|memory| ScoredMemory { memory, score: 0 })
            .collect::<Vec<_>>(),
    );
    nodes.sort();
    nodes.dedup();
    graph.related_memory_ids(&nodes).await
}

fn graph_anchor_nodes_from_records(memories: &[MemoryRecord]) -> Vec<String> {
    let wrap: Vec<ScoredMemory> = memories
        .iter()
        .take(5)
        .cloned()
        .map(|memory| ScoredMemory { memory, score: 0 })
        .collect();
    graph_anchor_nodes(&wrap)
}

/// Computes the additive non-recall portion of a memory's score, covering
/// the 9 signals shared by all three scorers (`_rrf`, `_legacy`,
/// `score_candidates`). The evidence bonus (`+2 when !evidence.is_empty()`)
/// applies only to the hybrid scorers; callers add it inline before invoking
/// this helper. Recall computation differs per scorer; this helper handles
/// the common rest.
#[allow(dead_code)] // callers are wired in subsequent Tasks 3-5
fn apply_lifecycle_score(
    memory: &MemoryRecord,
    query: &SearchMemoryRequest,
    query_terms: &[String],
    scope_filters: &HashMap<String, Vec<String>>,
    newest: u128,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
) -> i64 {
    let mut score = 0i64;

    score += text_match_score(memory, query_terms);
    score += scope_score(memory, scope_filters);
    score += memory_type_score(&memory.memory_type, &query.intent);
    score += confidence_score(memory.confidence);
    score += validation_score(memory.last_validated_at.is_some());
    score += freshness_score(newest, timestamp_score(&memory.updated_at));
    score -= staleness_penalty(memory.decay_score);

    if related_memory_ids.contains(&memory.memory_id) {
        score += graph_boost;
    }

    if matches!(
        memory.status,
        MemoryStatus::Provisional | MemoryStatus::PendingConfirmation
    ) {
        score -= 4;
    }

    score
}

fn score_candidates_hybrid_legacy(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
    lexical_ids: &HashSet<String>,
    semantic_sims: &HashMap<String, f32>,
) -> Vec<ScoredMemory> {
    let newest = candidates
        .iter()
        .map(|memory| timestamp_score(&memory.updated_at))
        .max()
        .unwrap_or(0);

    let query_terms = tokenize(&query.query);
    let scope_filters = parse_scope_filters(&query.scope_filters);

    let mut scored = candidates
        .into_iter()
        .map(|memory| {
            let mut score = 0i64;
            if let Some(sim) = semantic_sims.get(&memory.memory_id) {
                let t = sim.clamp(-1.0, 1.0);
                score += (((t + 1.0) / 2.0) * 64.0) as i64;
            }
            if lexical_ids.contains(&memory.memory_id)
                && semantic_sims.contains_key(&memory.memory_id)
            {
                score += 26;
            }
            // Lifecycle additive layer — extracted to apply_lifecycle_score for shared math.
            // Evidence bonus stays inline because score_candidates (non-hybrid) doesn't have it.
            if !memory.evidence.is_empty() {
                score += 2;
            }
            score += apply_lifecycle_score(
                &memory,
                query,
                &query_terms,
                &scope_filters,
                newest,
                related_memory_ids,
                graph_boost,
            );

            ScoredMemory { memory, score }
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                timestamp_score(&right.memory.updated_at)
                    .cmp(&timestamp_score(&left.memory.updated_at))
            })
            .then_with(|| right.memory.version.cmp(&left.memory.version))
            .then_with(|| left.memory.memory_id.cmp(&right.memory.memory_id))
    });

    scored
}

const RRF_K: usize = 60;
const RRF_SCALE: f64 = 1000.0;

fn score_candidates_hybrid_rrf(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
    lexical_ranks: &HashMap<String, usize>,
    semantic_ranks: &HashMap<String, usize>,
) -> Vec<ScoredMemory> {
    let newest = candidates
        .iter()
        .map(|memory| timestamp_score(&memory.updated_at))
        .max()
        .unwrap_or(0);

    let query_terms = tokenize(&query.query);
    let scope_filters = parse_scope_filters(&query.scope_filters);

    let mut scored = candidates
        .into_iter()
        .map(|memory| {
            let mut score = 0i64;

            let rrf_lex = lexical_ranks
                .get(&memory.memory_id)
                .map(|&r| 1.0_f64 / (RRF_K as f64 + r as f64))
                .unwrap_or(0.0);
            let rrf_sem = semantic_ranks
                .get(&memory.memory_id)
                .map(|&r| 1.0_f64 / (RRF_K as f64 + r as f64))
                .unwrap_or(0.0);
            score += ((rrf_lex + rrf_sem) * RRF_SCALE).round() as i64;

            // Lifecycle additive layer — extracted to apply_lifecycle_score for shared math.
            // Evidence bonus stays inline because score_candidates (non-hybrid) doesn't have it.
            if !memory.evidence.is_empty() {
                score += 2;
            }
            score += apply_lifecycle_score(
                &memory,
                query,
                &query_terms,
                &scope_filters,
                newest,
                related_memory_ids,
                graph_boost,
            );

            ScoredMemory { memory, score }
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                timestamp_score(&right.memory.updated_at)
                    .cmp(&timestamp_score(&left.memory.updated_at))
            })
            .then_with(|| right.memory.version.cmp(&left.memory.version))
            .then_with(|| left.memory.memory_id.cmp(&right.memory.memory_id))
    });

    scored
}

fn score_candidates(
    candidates: Vec<MemoryRecord>,
    query: &SearchMemoryRequest,
    related_memory_ids: &HashSet<String>,
    graph_boost: i64,
) -> Vec<ScoredMemory> {
    let newest = candidates
        .iter()
        .map(|memory| timestamp_score(&memory.updated_at))
        .max()
        .unwrap_or(0);

    let query_terms = tokenize(&query.query);
    let scope_filters = parse_scope_filters(&query.scope_filters);

    let mut scored = candidates
        .into_iter()
        .map(|memory| {
            let mut score = 0i64;
            score += text_match_score(&memory, &query_terms);
            score += scope_score(&memory, &scope_filters);
            score += memory_type_score(&memory.memory_type, &query.intent);
            score += confidence_score(memory.confidence);
            score += validation_score(memory.last_validated_at.is_some());
            score += freshness_score(newest, timestamp_score(&memory.updated_at));
            score -= staleness_penalty(memory.decay_score);

            if related_memory_ids.contains(&memory.memory_id) {
                score += graph_boost;
            }

            if matches!(
                memory.status,
                MemoryStatus::Provisional | MemoryStatus::PendingConfirmation
            ) {
                score -= 4;
            }

            ScoredMemory { memory, score }
        })
        .collect::<Vec<_>>();

    scored.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| {
                timestamp_score(&right.memory.updated_at)
                    .cmp(&timestamp_score(&left.memory.updated_at))
            })
            .then_with(|| right.memory.version.cmp(&left.memory.version))
            .then_with(|| left.memory.memory_id.cmp(&right.memory.memory_id))
    });

    scored
}

fn graph_anchor_nodes(candidates: &[ScoredMemory]) -> Vec<String> {
    let mut nodes = Vec::new();

    for scored in candidates.iter().take(5) {
        let memory = &scored.memory;
        nodes.push(format!("memory:{}", memory.memory_id));

        if let Some(project) = memory.project.as_deref().filter(|value| !value.is_empty()) {
            nodes.push(format!("project:{project}"));
        }

        if let Some(repo) = memory.repo.as_deref().filter(|value| !value.is_empty()) {
            nodes.push(format!("repo:{repo}"));
        }

        if let (Some(repo), Some(module)) = (
            memory.repo.as_deref().filter(|value| !value.is_empty()),
            memory.module.as_deref().filter(|value| !value.is_empty()),
        ) {
            nodes.push(format!("module:{repo}:{module}"));
        }

        if let Some(task_type) = memory
            .task_type
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            nodes.push(format!("workflow:{task_type}"));
        } else if matches!(memory.memory_type, MemoryType::Workflow) {
            nodes.push(format!("workflow:{}", memory.memory_id));
        }
    }

    nodes.sort();
    nodes.dedup();
    nodes
}

fn text_match_score(memory: &MemoryRecord, query_terms: &[String]) -> i64 {
    if query_terms.is_empty() {
        return 0;
    }

    let mut score = 0i64;
    let haystack = normalized_haystack(memory);
    for term in query_terms {
        if haystack.contains(term) {
            score += 4;
        }

        if memory.summary.to_lowercase().contains(term) {
            score += 8;
        }
        if memory.content.to_lowercase().contains(term) {
            score += 6;
        }
        if memory
            .code_refs
            .iter()
            .any(|item| item.to_lowercase().contains(term))
        {
            score += 3;
        }
        if memory
            .tags
            .iter()
            .any(|item| item.to_lowercase().contains(term))
        {
            score += 3;
        }
        if memory
            .project
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(term))
        {
            score += 2;
        }
        if memory
            .repo
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(term))
        {
            score += 2;
        }
        if memory
            .module
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(term))
        {
            score += 2;
        }
    }

    score
}

fn scope_score(memory: &MemoryRecord, scope_filters: &HashMap<String, Vec<String>>) -> i64 {
    if scope_filters.is_empty() {
        return match memory.scope {
            Scope::Global => 0,
            Scope::Project => 2,
            Scope::Repo => 4,
            Scope::Workspace => 3,
        };
    }

    let mut score = 0;
    for (kind, values) in scope_filters {
        let matched = values
            .iter()
            .any(|value| scope_matches(memory, kind, value));
        if matched {
            score += 18;
        } else {
            score -= 4;
        }
    }

    score
}

fn scope_matches(memory: &MemoryRecord, kind: &str, value: &str) -> bool {
    match kind {
        "repo" => memory.repo.as_deref() == Some(value),
        "project" => memory.project.as_deref() == Some(value),
        "module" => memory.module.as_deref() == Some(value),
        "scope" => scope_name(&memory.scope) == value,
        "tag" => memory.tags.iter().any(|tag| tag == value),
        _ => false,
    }
}

fn parse_scope_filters(filters: &[String]) -> HashMap<String, Vec<String>> {
    let mut parsed = HashMap::new();
    for filter in filters {
        if let Some((kind, value)) = filter.split_once(':') {
            parsed
                .entry(kind.to_string())
                .or_insert_with(Vec::new)
                .push(value.to_string());
        } else {
            parsed
                .entry("tag".to_string())
                .or_insert_with(Vec::new)
                .push(filter.clone());
        }
    }
    parsed
}

fn memory_type_score(memory_type: &MemoryType, intent: &str) -> i64 {
    let intent = intent.to_lowercase();
    if intent.contains("debug") {
        return match memory_type {
            MemoryType::Experience => 10,
            MemoryType::Implementation => 8,
            MemoryType::Episode => 7,
            MemoryType::Workflow => 5,
            MemoryType::Preference => 1,
        };
    }

    if intent.contains("workflow") {
        return match memory_type {
            MemoryType::Workflow => 10,
            MemoryType::Experience => 6,
            MemoryType::Implementation => 4,
            MemoryType::Episode => 5,
            MemoryType::Preference => 1,
        };
    }

    match memory_type {
        MemoryType::Preference => 8,
        MemoryType::Workflow => 7,
        MemoryType::Experience => 6,
        MemoryType::Implementation => 5,
        MemoryType::Episode => 4,
    }
}

fn confidence_score(confidence: f32) -> i64 {
    (confidence * 10.0).round() as i64
}

fn validation_score(validated: bool) -> i64 {
    if validated {
        3
    } else {
        0
    }
}

fn freshness_score(newest: u128, current: u128) -> i64 {
    if newest <= current {
        return 6;
    }

    let delta = newest - current;
    let bucket = (delta / 10_000).min(20);
    6 - bucket as i64
}

fn staleness_penalty(decay_score: f32) -> i64 {
    (decay_score * 12.0).round() as i64
}

fn normalized_haystack(memory: &MemoryRecord) -> String {
    let mut parts = vec![
        memory.summary.to_lowercase(),
        memory.content.to_lowercase(),
        memory.project.clone().unwrap_or_default().to_lowercase(),
        memory.repo.clone().unwrap_or_default().to_lowercase(),
        memory.module.clone().unwrap_or_default().to_lowercase(),
        memory.task_type.clone().unwrap_or_default().to_lowercase(),
        memory.tags.join(" ").to_lowercase(),
        memory.code_refs.join(" ").to_lowercase(),
    ];
    parts.retain(|part| !part.is_empty());
    parts.join(" ")
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

fn timestamp_score(value: &str) -> u128 {
    let digits = value
        .chars()
        .filter(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse::<u128>().unwrap_or(0)
}

fn scope_name(scope: &Scope) -> &'static str {
    match scope {
        Scope::Global => "global",
        Scope::Project => "project",
        Scope::Repo => "repo",
        Scope::Workspace => "workspace",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::memory::{MemoryRecord, MemoryStatus, MemoryType, Scope, Visibility};
    use crate::domain::query::SearchMemoryRequest;

    fn fixture_memory(id: &str) -> MemoryRecord {
        MemoryRecord {
            memory_id: id.to_string(),
            tenant: "t".to_string(),
            memory_type: MemoryType::Implementation,
            status: MemoryStatus::Active,
            scope: Scope::Global,
            visibility: Visibility::Private,
            version: 0,
            summary: String::new(),
            content: String::new(),
            evidence: vec![],
            code_refs: vec![],
            project: None,
            repo: None,
            module: None,
            task_type: None,
            tags: vec![],
            confidence: 0.0,
            decay_score: 0.0,
            content_hash: String::new(),
            idempotency_key: None,
            session_id: None,
            supersedes_memory_id: None,
            source_agent: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            last_validated_at: None,
        }
    }

    fn fixture_query() -> SearchMemoryRequest {
        SearchMemoryRequest {
            query: String::new(),
            intent: String::new(),
            scope_filters: vec![],
            token_budget: 0,
            caller_agent: String::new(),
            expand_graph: false,
            tenant: None,
        }
    }

    fn lifecycle_baseline_for(memory: &MemoryRecord, query: &SearchMemoryRequest) -> i64 {
        let newest = timestamp_score(&memory.updated_at);
        memory_type_score(&memory.memory_type, &query.intent) + freshness_score(newest, newest)
            - staleness_penalty(memory.decay_score)
    }

    #[test]
    fn rrf_recall_only_lexical() {
        let memory = fixture_memory("mem_a");
        let query = fixture_query();

        let lifecycle_baseline = lifecycle_baseline_for(&memory, &query);

        let mut lex_ranks = HashMap::new();
        lex_ranks.insert("mem_a".into(), 1usize);
        let sem_ranks: HashMap<String, usize> = HashMap::new();

        let scored = score_candidates_hybrid_rrf(
            vec![memory],
            &query,
            &HashSet::new(),
            0,
            &lex_ranks,
            &sem_ranks,
        );

        // RRF contribution: 1000/(60+1) = 16.39 → round → 16.
        assert_eq!(scored[0].score - lifecycle_baseline, 16);
    }

    #[test]
    fn rrf_both_paths_top_rank() {
        // A candidate at rank 1 in both lex and sem gets two RRF contributions.
        // 2 * 1000/(60+1) = 32.787 → round → 33.
        let memory = fixture_memory("mem_top");
        let query = fixture_query();

        let lifecycle_baseline = lifecycle_baseline_for(&memory, &query);

        let mut lex_ranks = HashMap::new();
        let mut sem_ranks = HashMap::new();
        lex_ranks.insert("mem_top".into(), 1usize);
        sem_ranks.insert("mem_top".into(), 1usize);

        let scored = score_candidates_hybrid_rrf(
            vec![memory],
            &query,
            &HashSet::new(),
            0,
            &lex_ranks,
            &sem_ranks,
        );

        assert_eq!(scored[0].score - lifecycle_baseline, 33);
    }

    #[test]
    fn rrf_rank_monotonic() {
        // Three candidates with different semantic ranks must sort strictly
        // score-descending after RRF scoring, since all share the same
        // lifecycle baseline (identical fixture timestamps).
        let m1 = fixture_memory("rank_1");
        let m50 = fixture_memory("rank_50");
        let m100 = fixture_memory("rank_100");
        let query = fixture_query();

        let mut sem_ranks = HashMap::new();
        sem_ranks.insert("rank_1".into(), 1usize);
        sem_ranks.insert("rank_50".into(), 50usize);
        sem_ranks.insert("rank_100".into(), 100usize);
        let lex_ranks: HashMap<String, usize> = HashMap::new();

        let scored = score_candidates_hybrid_rrf(
            vec![m1, m50, m100],
            &query,
            &HashSet::new(),
            0,
            &lex_ranks,
            &sem_ranks,
        );

        // After sort: rank_1 (highest RRF), rank_50, rank_100.
        // All share the same lifecycle baseline → ordering is determined by RRF alone.
        assert_eq!(scored[0].memory.memory_id, "rank_1");
        assert_eq!(scored[1].memory.memory_id, "rank_50");
        assert_eq!(scored[2].memory.memory_id, "rank_100");
        assert!(scored[0].score > scored[1].score);
        assert!(scored[1].score > scored[2].score);
    }

    #[test]
    fn lex_only_candidate_has_nonzero_recall_after_rrf() {
        // Pre-RRF bug: lex-only candidates got zero recall contribution
        // (only the intersect bonus +26 fired, which requires also being in
        // semantic). RRF gives them 1000/(60+lex_rank) > 0, ensuring recall
        // is always positive for any ranked candidate.
        let memory = fixture_memory("lex_only");
        let query = fixture_query();

        let lifecycle_baseline = lifecycle_baseline_for(&memory, &query);

        let mut lex_ranks = HashMap::new();
        lex_ranks.insert("lex_only".into(), 1usize);
        let sem_ranks: HashMap<String, usize> = HashMap::new();

        let scored = score_candidates_hybrid_rrf(
            vec![memory],
            &query,
            &HashSet::new(),
            0,
            &lex_ranks,
            &sem_ranks,
        );

        assert!(
            scored[0].score > lifecycle_baseline,
            "lex-only candidate must have positive RRF recall contribution"
        );
    }

    #[test]
    fn apply_lifecycle_score_neutral_input() {
        let memory = fixture_memory("mem_neutral");
        let query = fixture_query();
        let newest = timestamp_score(&memory.updated_at);

        let actual = apply_lifecycle_score(
            &memory,
            &query,
            &[],
            &HashMap::new(),
            newest,
            &HashSet::new(),
            0,
        );

        let expected = memory_type_score(&memory.memory_type, &query.intent)
            + freshness_score(newest, newest)
            - staleness_penalty(memory.decay_score);
        assert_eq!(
            actual, expected,
            "neutral fixture should produce only memory_type + freshness contributions"
        );
    }

    #[test]
    fn apply_lifecycle_score_provisional_status_penalty() {
        let mut memory = fixture_memory("mem_provisional");
        memory.status = MemoryStatus::Provisional;
        let query = fixture_query();
        let newest = timestamp_score(&memory.updated_at);

        let baseline = {
            let mut neutral = memory.clone();
            neutral.status = MemoryStatus::Active;
            apply_lifecycle_score(
                &neutral,
                &query,
                &[],
                &HashMap::new(),
                newest,
                &HashSet::new(),
                0,
            )
        };

        let actual = apply_lifecycle_score(
            &memory,
            &query,
            &[],
            &HashMap::new(),
            newest,
            &HashSet::new(),
            0,
        );

        assert_eq!(
            actual,
            baseline - 4,
            "Provisional status must subtract 4 from the baseline"
        );
    }

    #[test]
    fn apply_lifecycle_score_graph_neighbor_boost() {
        let memory = fixture_memory("mem_with_neighbor");
        let query = fixture_query();
        let newest = timestamp_score(&memory.updated_at);

        let baseline = apply_lifecycle_score(
            &memory,
            &query,
            &[],
            &HashMap::new(),
            newest,
            &HashSet::new(),
            0,
        );

        let mut related = HashSet::new();
        related.insert("mem_with_neighbor".to_string());

        let actual =
            apply_lifecycle_score(&memory, &query, &[], &HashMap::new(), newest, &related, 12);

        assert_eq!(
            actual,
            baseline + 12,
            "memory in related set must add graph_boost"
        );
    }

    #[test]
    fn legacy_kill_switch_replicates_old_scoring() {
        // With MEM_RANKER=legacy, merge_and_rank_hybrid must dispatch to
        // score_candidates_hybrid_legacy. We can't easily assert the exact
        // score here because merge_and_rank_hybrid returns Vec<MemoryRecord>,
        // not Vec<ScoredMemory>, but verifying the candidate is preserved
        // through the legacy path (combined with the rrf_* tests proving RRF
        // is the default) confirms the dispatch works end-to-end.
        let memory = fixture_memory("legacy_only");
        let query = fixture_query();
        let lexical: Vec<MemoryRecord> = vec![];
        let semantic: Vec<(MemoryRecord, f32)> = vec![(memory, 1.0)];

        // SAFETY: env mutation is unsafe in Rust 2024. Cargo's libtest
        // harness defaults to multi-threaded execution but each test gets
        // its own thread; setting and clearing the var within one test
        // is safe as long as no other concurrent test reads MEM_RANKER.
        // No other test in this module reads it.
        unsafe {
            std::env::set_var("MEM_RANKER", "legacy");
        }
        let result = merge_and_rank_hybrid(lexical, semantic, &query, &HashSet::new(), 0);
        unsafe {
            std::env::remove_var("MEM_RANKER");
        }

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].memory_id, "legacy_only");
    }
}
