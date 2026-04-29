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
    score_candidates_hybrid_legacy(
        candidates,
        query,
        related_memory_ids,
        graph_boost,
        &lexical_ids,
        &semantic_sims,
    )
    .into_iter()
    .map(|entry| entry.memory)
    .collect()
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
            if !memory.evidence.is_empty() {
                score += 2;
            }
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
