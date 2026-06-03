use std::collections::{HashMap, HashSet};

use crate::{
    domain::{
        capability_capsule::{
            CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope,
        },
        query::SearchCapabilityCapsuleRequest,
    },
    pipeline::ranking::{freshness_score, timestamp_score, RRF_SCALE},
    storage::GraphStore,
};

#[derive(Debug, Clone)]
struct ScoredMemory {
    memory: CapabilityCapsuleRecord,
    score: i64,
}

/// Default relevance floor: a memory must reach this score to surface in
/// results. Sized to let a pure semantic-rank-1 hit through (~16 RRF +
/// modest lifecycle baseline), while filtering candidates that have no
/// textual or semantic signal at all. Tunable via `MEM_MIN_SCORE`. Raise
/// it to be more aggressive about filtering scope-only matches.
const DEFAULT_MIN_RELEVANCE_SCORE: i64 = 25;

fn min_relevance_score() -> i64 {
    std::env::var("MEM_MIN_SCORE")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_MIN_RELEVANCE_SCORE)
}

/// Resolve the relevance floor for a request: an explicit per-request
/// `min_score` wins; otherwise fall back to the process-wide
/// `MEM_MIN_SCORE` / default. Keeps the override decision in one place.
fn effective_floor(query: &SearchCapabilityCapsuleRequest) -> i64 {
    query.min_score.unwrap_or_else(min_relevance_score)
}

/// Does `memory` satisfy the given scope filters? Uses the same
/// `kind:value` grammar as search ranking (`repo:`, `project:`,
/// `module:`, `scope:`, bare `tag`). Empty filters → always true (no
/// scoping requested). Public so the wake-up path can float in-scope
/// capsules to the front of the recent slice.
pub fn matches_scope_filters(memory: &CapabilityCapsuleRecord, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    parse_scope_filters(filters).iter().any(|(kind, values)| {
        values
            .iter()
            .any(|value| scope_matches(memory, kind, value))
    })
}

/// Filters scored candidates by the relevance floor and strips scores.
/// Used at every user-visible exit point so empty results bubble up to
/// `compress`, which renders them as empty sections.
///
/// `Preference` (rendered as Directive) and `Workflow` (rendered as
/// Suggested Workflow) are "always-applicable" memory types: they describe
/// background guidance / procedural defaults that should surface regardless
/// of textual match with the current query. The relevance floor only gates
/// the relevance-driven sections (Facts, Patterns).
fn finalize(scored: Vec<ScoredMemory>, floor: i64) -> Vec<CapabilityCapsuleRecord> {
    scored
        .into_iter()
        .filter(|entry| {
            matches!(
                entry.memory.capability_capsule_type,
                CapabilityCapsuleType::Preference | CapabilityCapsuleType::Workflow
            ) || entry.score > floor
        })
        .map(|entry| entry.memory)
        .collect()
}

/// Top-level hybrid entry: take the lifecycle pool (e.g. all active
/// capsules for tenant) plus the SQL-side hybrid hits with their RRF
/// scores, and produce the user-visible ranked result.
///
/// `pool` carries the always-applicable Preference / Workflow rows
/// regardless of whether they hit the query — they pass `finalize`'s
/// floor exemption. `hybrid_hits` carries the relevance signal: an
/// (id → rrf_score) map driven by `lance_fts` + `lance_vector_search`
/// joined and RRF-fused inline in DuckDB SQL. Items in both inputs
/// score with rrf_score + lifecycle signals; items only in the pool
/// score from lifecycle alone.
///
/// Graph expansion follows the same two-pass shape as the pre-hybrid
/// path: derive anchors from the unfiltered top-N, fetch related
/// capsule ids, rescore with `graph_boost = 12` on matching items.
pub async fn rank_with_hybrid_and_graph(
    pool: Vec<CapabilityCapsuleRecord>,
    hybrid_hits: Vec<(CapabilityCapsuleRecord, f32)>,
    query: &SearchCapabilityCapsuleRequest,
    graph: &dyn GraphStore,
    dynamics: Option<&EdgeDynamicsCtx>,
) -> Result<Vec<CapabilityCapsuleRecord>, crate::storage::GraphError> {
    let hybrid_scores: HashMap<String, f32> = hybrid_hits
        .iter()
        .map(|(m, s)| (m.capability_capsule_id.clone(), *s))
        .collect();

    // Merge: pool acts as the lifecycle-applicable cohort; any
    // hybrid hits not already in pool (rare — pool is the full active
    // tenant set) are folded in so they can be scored.
    let mut by_id: HashMap<String, CapabilityCapsuleRecord> = HashMap::new();
    for m in pool {
        by_id.insert(m.capability_capsule_id.clone(), m);
    }
    for (m, _) in hybrid_hits {
        by_id.entry(m.capability_capsule_id.clone()).or_insert(m);
    }
    let candidates: Vec<CapabilityCapsuleRecord> = by_id.into_values().collect();

    let floor = effective_floor(query);
    let empty_boosts: HashMap<String, i64> = HashMap::new();
    if !query.expand_graph {
        return Ok(finalize(
            score_with_hybrid(candidates, query, &hybrid_scores, &empty_boosts),
            floor,
        ));
    }

    // Graph anchor derivation uses unfiltered top-N (floor is for the
    // user-visible result, not anchor selection).
    let preliminary_scored =
        score_with_hybrid(candidates.clone(), query, &hybrid_scores, &empty_boosts);
    let anchors = graph_anchor_nodes(&preliminary_scored);
    if anchors.is_empty() {
        return Ok(finalize(preliminary_scored, floor));
    }

    let boost_by_id = compute_graph_boosts(graph, &anchors, dynamics).await?;
    Ok(finalize(
        score_with_hybrid(candidates, query, &hybrid_scores, &boost_by_id),
        floor,
    ))
}

/// K9 context for edge-dynamics-aware graph ranking, threaded in by the
/// service only when `MEM_EDGE_DYNAMICS_ENABLED`. `None` everywhere else
/// → flat graph boost + no potentiation (the pre-K9 behaviour).
pub struct EdgeDynamicsCtx {
    /// `current_timestamp()` for read-time decay.
    pub now: String,
    /// Channel to the potentiation worker; co-access events are sent here
    /// (best-effort, non-blocking) — never written on the read path.
    pub tx: tokio::sync::mpsc::UnboundedSender<crate::worker::potentiation_worker::EdgeAccess>,
}

/// Base graph-expansion boost: a capsule reachable from an anchor in one
/// hop gets this added to its score. K9 scales it by the connecting
/// edge's time-decayed strength.
const GRAPH_BOOST: i64 = 12;

/// Build the per-capsule graph-boost map for the anchor set.
///
/// - Dynamics off (`None`): every 1-hop-related capsule gets the flat
///   [`GRAPH_BOOST`] — byte-for-byte the pre-K9 behaviour.
/// - Dynamics on (`Some`): walk each anchor's incident edges, scale the
///   non-anchor capsule endpoint's boost by the edge's decayed strength
///   (`round(GRAPH_BOOST * decayed_strength)`, max over connecting
///   edges), and enqueue every touched edge for potentiation
///   (best-effort — a closed channel never blocks or fails the read).
async fn compute_graph_boosts(
    graph: &dyn GraphStore,
    anchors: &[String],
    dynamics: Option<&EdgeDynamicsCtx>,
) -> Result<HashMap<String, i64>, crate::storage::GraphError> {
    let Some(ctx) = dynamics else {
        let related = graph.related_capability_capsule_ids(anchors).await?;
        return Ok(related.into_iter().map(|id| (id, GRAPH_BOOST)).collect());
    };
    let anchor_set: HashSet<&str> = anchors.iter().map(|s| s.as_str()).collect();
    let mut boosts: HashMap<String, i64> = HashMap::new();
    for anchor in anchors {
        let edges = graph.neighbors_within(anchor, 1, None).await?;
        for edge in &edges {
            // Enqueue for potentiation off the read path; ignore send
            // errors (worker absent / channel closed) — best-effort.
            let _ = ctx.tx.send(crate::worker::potentiation_worker::EdgeAccess {
                from_node_id: edge.from_node_id.clone(),
                to_node_id: edge.to_node_id.clone(),
                relation: edge.relation.clone(),
            });
            let strength = crate::domain::edge_dynamics::decayed_strength(edge, &ctx.now);
            let boost = ((GRAPH_BOOST as f32) * strength).round() as i64;
            for endpoint in [&edge.from_node_id, &edge.to_node_id] {
                if anchor_set.contains(endpoint.as_str()) {
                    continue;
                }
                if let Some(mid) = endpoint.strip_prefix("capability_capsule:") {
                    boosts
                        .entry(mid.to_string())
                        .and_modify(|m| *m = (*m).max(boost))
                        .or_insert(boost);
                }
            }
        }
    }
    Ok(boosts)
}

/// Computes the additive non-recall portion of a memory's score (the
/// "lifecycle" stack: scope, intent, confidence, validation, freshness,
/// staleness, graph boost, status penalty). Used by `score_with_hybrid`
/// after the SQL-side RRF score has been added.
/// handles the common rest.
#[allow(dead_code)] // callers are wired in subsequent Tasks 3-5
fn apply_lifecycle_score(
    memory: &CapabilityCapsuleRecord,
    query: &SearchCapabilityCapsuleRequest,
    query_terms: &[String],
    scope_filters: &HashMap<String, Vec<String>>,
    newest: u128,
    graph_boost_by_id: &HashMap<String, i64>,
) -> i64 {
    let mut score = 0i64;

    score += text_match_score(memory, query_terms);
    score += scope_score(memory, scope_filters);
    score += memory_type_score(&memory.capability_capsule_type, &query.intent);
    score += confidence_score(memory.confidence);
    score += validation_score(memory.last_validated_at.is_some());
    score += freshness_score(newest, timestamp_score(&memory.updated_at));
    score -= staleness_penalty(memory.decay_score);

    score += graph_boost_by_id
        .get(&memory.capability_capsule_id)
        .copied()
        .unwrap_or(0);

    if matches!(
        memory.status,
        CapabilityCapsuleStatus::Provisional | CapabilityCapsuleStatus::PendingConfirmation
    ) {
        score -= 4;
    }

    score
}

/// Score each candidate with the SQL-side RRF (already-fused
/// lex+sem signal as a Float32) plus the lifecycle / scope / intent
/// / freshness / decay / graph stack. Items not in `hybrid_scores`
/// score zero on relevance — they survive only via the lifecycle
/// stack and the always-applicable `Preference` / `Workflow` floor
/// exemption in `finalize`.
fn score_with_hybrid(
    candidates: Vec<CapabilityCapsuleRecord>,
    query: &SearchCapabilityCapsuleRequest,
    hybrid_scores: &HashMap<String, f32>,
    graph_boost_by_id: &HashMap<String, i64>,
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

            // SQL-side RRF score is already (1/(60+lex_rank))
            // + (1/(60+sem_rank)) ∈ ~[0, 0.033]. Scale it into the
            // i64 score domain via `RRF_SCALE` so a rank-1 dual hit
            // contributes about the same as the legacy manual RRF
            // path (~32 score points).
            if let Some(rrf) = hybrid_scores.get(&memory.capability_capsule_id) {
                score += ((*rrf as f64) * RRF_SCALE).round() as i64;
            }

            if !memory.evidence.is_empty() {
                score += 2;
            }
            score += apply_lifecycle_score(
                &memory,
                query,
                &query_terms,
                &scope_filters,
                newest,
                graph_boost_by_id,
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
            .then_with(|| {
                left.memory
                    .capability_capsule_id
                    .cmp(&right.memory.capability_capsule_id)
            })
    });

    scored
}

fn graph_anchor_nodes(candidates: &[ScoredMemory]) -> Vec<String> {
    let mut nodes = Vec::new();

    for scored in candidates.iter().take(5) {
        let memory = &scored.memory;
        nodes.push(format!(
            "capability_capsule:{}",
            memory.capability_capsule_id
        ));

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
        } else if matches!(
            memory.capability_capsule_type,
            CapabilityCapsuleType::Workflow
        ) {
            nodes.push(format!("workflow:{}", memory.capability_capsule_id));
        }
    }

    nodes.sort();
    nodes.dedup();
    nodes
}

fn text_match_score(memory: &CapabilityCapsuleRecord, query_terms: &[String]) -> i64 {
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

fn scope_score(
    memory: &CapabilityCapsuleRecord,
    scope_filters: &HashMap<String, Vec<String>>,
) -> i64 {
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

fn scope_matches(memory: &CapabilityCapsuleRecord, kind: &str, value: &str) -> bool {
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

fn memory_type_score(capability_capsule_type: &CapabilityCapsuleType, intent: &str) -> i64 {
    // Diary entries are filtered out at SQL level (see
    // `hybrid_candidates` outer WHERE), so they shouldn't reach this
    // scorer. Score 0 as a defense-in-depth fallback in case the SQL
    // filter ever drifts.
    if matches!(capability_capsule_type, CapabilityCapsuleType::Diary) {
        return 0;
    }

    let intent = intent.to_lowercase();
    if intent.contains("debug") {
        return match capability_capsule_type {
            CapabilityCapsuleType::Experience => 10,
            CapabilityCapsuleType::Implementation => 8,
            CapabilityCapsuleType::Episode => 7,
            CapabilityCapsuleType::Workflow => 5,
            CapabilityCapsuleType::Preference => 1,
            CapabilityCapsuleType::Diary => 0,
        };
    }

    if intent.contains("workflow") {
        return match capability_capsule_type {
            CapabilityCapsuleType::Workflow => 10,
            CapabilityCapsuleType::Experience => 6,
            CapabilityCapsuleType::Implementation => 4,
            CapabilityCapsuleType::Episode => 5,
            CapabilityCapsuleType::Preference => 1,
            CapabilityCapsuleType::Diary => 0,
        };
    }

    match capability_capsule_type {
        CapabilityCapsuleType::Preference => 8,
        CapabilityCapsuleType::Workflow => 7,
        CapabilityCapsuleType::Experience => 6,
        CapabilityCapsuleType::Implementation => 5,
        CapabilityCapsuleType::Episode => 4,
        CapabilityCapsuleType::Diary => 0,
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

fn staleness_penalty(decay_score: f32) -> i64 {
    (decay_score * 12.0).round() as i64
}

fn normalized_haystack(memory: &CapabilityCapsuleRecord) -> String {
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
    use crate::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use crate::domain::query::SearchCapabilityCapsuleRequest;

    fn fixture_memory(id: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.to_string(),
            tenant: "t".to_string(),
            capability_capsule_type: CapabilityCapsuleType::Implementation,
            status: CapabilityCapsuleStatus::Active,
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
            topics: vec![],
            confidence: 0.0,
            decay_score: 0.0,
            content_hash: String::new(),
            idempotency_key: None,
            session_id: None,
            supersedes_capability_capsule_id: None,
            source_agent: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            last_validated_at: None,
        }
    }

    fn fixture_query() -> SearchCapabilityCapsuleRequest {
        SearchCapabilityCapsuleRequest {
            query: String::new(),
            intent: String::new(),
            scope_filters: vec![],
            token_budget: 0,
            caller_agent: String::new(),
            expand_graph: false,
            tenant: None,
            min_score: None,
        }
    }

    fn lifecycle_baseline_for(
        memory: &CapabilityCapsuleRecord,
        query: &SearchCapabilityCapsuleRequest,
    ) -> i64 {
        let newest = timestamp_score(&memory.updated_at);
        memory_type_score(&memory.capability_capsule_type, &query.intent)
            + freshness_score(newest, newest)
            - staleness_penalty(memory.decay_score)
    }

    /// RRF score equivalent to `lance_fts`/`lance_vector_search`'s SQL
    /// output: sum of `1.0/(60+rank)` per source. Used in tests to
    /// build the same Float32 score the SQL hybrid produces.
    fn sql_rrf(lex_rank: Option<usize>, sem_rank: Option<usize>) -> f32 {
        let lex = lex_rank.map(|r| 1.0 / (60.0 + r as f32)).unwrap_or(0.0);
        let sem = sem_rank.map(|r| 1.0 / (60.0 + r as f32)).unwrap_or(0.0);
        lex + sem
    }

    #[test]
    fn effective_floor_prefers_request_min_score() {
        let mut query = fixture_query();
        query.min_score = Some(77);
        assert_eq!(effective_floor(&query), 77);
        // None delegates to the process-wide default / MEM_MIN_SCORE.
        query.min_score = None;
        assert_eq!(effective_floor(&query), min_relevance_score());
    }

    #[test]
    fn finalize_filters_relevance_types_by_floor() {
        let mut low = fixture_memory("low");
        low.capability_capsule_type = CapabilityCapsuleType::Implementation;
        let mut high = fixture_memory("high");
        high.capability_capsule_type = CapabilityCapsuleType::Implementation;
        let scored = vec![
            ScoredMemory {
                memory: low,
                score: 30,
            },
            ScoredMemory {
                memory: high,
                score: 50,
            },
        ];
        // A higher per-request floor (40) drops the 30-scorer.
        let kept = finalize(scored, 40);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].capability_capsule_id, "high");
    }

    #[test]
    fn finalize_exempts_preference_and_workflow() {
        let mut pref = fixture_memory("pref");
        pref.capability_capsule_type = CapabilityCapsuleType::Preference;
        let mut wf = fixture_memory("wf");
        wf.capability_capsule_type = CapabilityCapsuleType::Workflow;
        let scored = vec![
            ScoredMemory {
                memory: pref,
                score: 0,
            },
            ScoredMemory {
                memory: wf,
                score: 0,
            },
        ];
        // Always-applicable types survive even a floor far above their score.
        let kept = finalize(scored, 999);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn matches_scope_filters_repo_project_any() {
        let mut m = fixture_memory("m");
        m.repo = Some("mem".into());
        m.project = Some("mem".into());
        assert!(matches_scope_filters(&m, &[])); // no scoping → always in
        assert!(matches_scope_filters(&m, &["repo:mem".to_string()]));
        assert!(matches_scope_filters(&m, &["project:mem".to_string()]));
        assert!(!matches_scope_filters(&m, &["repo:other".to_string()]));
        // Any-match across multiple filters.
        assert!(matches_scope_filters(
            &m,
            &["repo:other".to_string(), "project:mem".to_string()]
        ));
    }

    #[test]
    fn rrf_recall_only_lexical() {
        let memory = fixture_memory("mem_a");
        let query = fixture_query();

        let lifecycle_baseline = lifecycle_baseline_for(&memory, &query);

        let mut hybrid = HashMap::new();
        hybrid.insert("mem_a".into(), sql_rrf(Some(1), None));

        let scored = score_with_hybrid(vec![memory], &query, &hybrid, &HashMap::new());

        // RRF contribution: 1000 * 1/(60+1) = 16.39 → round → 16.
        assert_eq!(scored[0].score - lifecycle_baseline, 16);
    }

    #[test]
    fn rrf_both_paths_top_rank() {
        // A candidate at rank 1 in both lex and sem gets two RRF contributions.
        // 2 * 1000/(60+1) = 32.787 → round → 33.
        let memory = fixture_memory("mem_top");
        let query = fixture_query();

        let lifecycle_baseline = lifecycle_baseline_for(&memory, &query);

        let mut hybrid = HashMap::new();
        hybrid.insert("mem_top".into(), sql_rrf(Some(1), Some(1)));

        let scored = score_with_hybrid(vec![memory], &query, &hybrid, &HashMap::new());

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

        let mut hybrid = HashMap::new();
        hybrid.insert("rank_1".into(), sql_rrf(None, Some(1)));
        hybrid.insert("rank_50".into(), sql_rrf(None, Some(50)));
        hybrid.insert("rank_100".into(), sql_rrf(None, Some(100)));

        let scored = score_with_hybrid(vec![m1, m50, m100], &query, &hybrid, &HashMap::new());

        // After sort: rank_1 (highest RRF), rank_50, rank_100.
        // All share the same lifecycle baseline → ordering is determined by RRF alone.
        assert_eq!(scored[0].memory.capability_capsule_id, "rank_1");
        assert_eq!(scored[1].memory.capability_capsule_id, "rank_50");
        assert_eq!(scored[2].memory.capability_capsule_id, "rank_100");
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

        let mut hybrid = HashMap::new();
        hybrid.insert("lex_only".into(), sql_rrf(Some(1), None));

        let scored = score_with_hybrid(vec![memory], &query, &hybrid, &HashMap::new());

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
            &HashMap::new(),
        );

        let expected = memory_type_score(&memory.capability_capsule_type, &query.intent)
            + freshness_score(newest, newest)
            - staleness_penalty(memory.decay_score);
        assert_eq!(
            actual, expected,
            "neutral fixture should produce only capability_capsule_type + freshness contributions"
        );
    }

    #[test]
    fn apply_lifecycle_score_provisional_status_penalty() {
        let mut memory = fixture_memory("mem_provisional");
        memory.status = CapabilityCapsuleStatus::Provisional;
        let query = fixture_query();
        let newest = timestamp_score(&memory.updated_at);

        let baseline = {
            let mut neutral = memory.clone();
            neutral.status = CapabilityCapsuleStatus::Active;
            apply_lifecycle_score(
                &neutral,
                &query,
                &[],
                &HashMap::new(),
                newest,
                &HashMap::new(),
            )
        };

        let actual = apply_lifecycle_score(
            &memory,
            &query,
            &[],
            &HashMap::new(),
            newest,
            &HashMap::new(),
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
            &HashMap::new(),
        );

        let mut boosts = HashMap::new();
        boosts.insert("mem_with_neighbor".to_string(), 12i64);

        let actual = apply_lifecycle_score(&memory, &query, &[], &HashMap::new(), newest, &boosts);

        assert_eq!(
            actual,
            baseline + 12,
            "memory's per-id graph boost must be added"
        );
    }

    /// K9 phase 4: with dynamics on, `compute_graph_boosts` scales each
    /// related capsule's boost by the connecting edge's decayed strength
    /// (`round(GRAPH_BOOST * strength)`) and enqueues every touched edge.
    #[tokio::test(flavor = "multi_thread")]
    async fn compute_graph_boosts_weights_by_strength_and_enqueues() {
        use crate::domain::capability_capsule::GraphEdge;
        use crate::storage::Store;
        use crate::worker::potentiation_worker::EdgeAccess;
        use tempfile::tempdir;
        use tokio::sync::mpsc;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("cgb.lance")).await.unwrap();
        let mk = |to: &str, strength: f32| GraphEdge {
            from_node_id: "capability_capsule:A".into(),
            to_node_id: to.into(),
            relation: "rel".into(),
            valid_from: "00000001780000000000".into(),
            valid_to: None,
            confidence: None,
            extractor: None,
            strength: Some(strength),
            stability: Some(1.0),
            // last_activated == now below → no decay, so boost is a clean
            // round(GRAPH_BOOST * strength).
            last_activated: Some("00000001780000000000".into()),
            access_count: Some(1),
        };
        store
            .add_edge_direct(&mk("capability_capsule:strong", 4.0))
            .await
            .unwrap();
        store
            .add_edge_direct(&mk("capability_capsule:weak", 0.5))
            .await
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<EdgeAccess>();
        let ctx = EdgeDynamicsCtx {
            now: "00000001780000000000".into(),
            tx,
        };
        let graph: &dyn GraphStore = &store;
        let boosts = compute_graph_boosts(graph, &["capability_capsule:A".to_string()], Some(&ctx))
            .await
            .unwrap();

        assert_eq!(
            boosts.get("strong"),
            Some(&48),
            "strength 4.0 → round(12*4.0)=48 (stronger than the flat 12)"
        );
        assert_eq!(
            boosts.get("weak"),
            Some(&6),
            "strength 0.5 → round(12*0.5)=6 (weaker than the flat 12)"
        );

        let mut seen = std::collections::HashSet::new();
        while let Ok(e) = rx.try_recv() {
            seen.insert(e.to_node_id);
        }
        assert!(seen.contains("capability_capsule:strong"));
        assert!(seen.contains("capability_capsule:weak"));
    }
}
