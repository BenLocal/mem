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

/// O3 per-source diversity cap: the max number of capsules from a single
/// *source* allowed in the head of a ranked result before the rest are
/// deferred to the tail (see [`diversify_by_source`]). Default **3**
/// (agentmemory's "max 3 per session"); set `MEM_RECALL_PER_SOURCE_CAP=0`
/// to disable. Read live so it can be tuned without a restart; an
/// invalid value falls back to the default.
fn per_source_cap() -> usize {
    std::env::var("MEM_RECALL_PER_SOURCE_CAP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
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

/// True when `m` carries a hard `expires_at` that `now` has reached. `None`
/// (the default) never expires. Compared as fixed-width 20-digit ms strings
/// (the `current_timestamp` format) — a malformed / wrong-width `expires_at`
/// reads as not-expired, so a bad timestamp can never silently hide a
/// capsule from recall.
fn is_expired(m: &CapabilityCapsuleRecord, now: &str) -> bool {
    match &m.expires_at {
        Some(e) => e.len() == now.len() && e.as_str() <= now,
        None => false,
    }
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
fn finalize(
    scored: Vec<ScoredMemory>,
    floor: i64,
    scope_filters: &[String],
) -> Vec<CapabilityCapsuleRecord> {
    let filtered: Vec<CapabilityCapsuleRecord> = scored
        .into_iter()
        .filter(|entry| {
            if matches!(
                entry.memory.capability_capsule_type,
                CapabilityCapsuleType::Preference | CapabilityCapsuleType::Workflow
            ) {
                guidance_in_scope(&entry.memory, scope_filters)
            } else {
                entry.score > floor
            }
        })
        .map(|entry| entry.memory)
        .collect();
    // O3: diversify the ranked head so one session's batch of near-dup
    // capsules can't dominate the top (and thus the token budget) of the
    // result.
    diversify_by_source(filtered, per_source_cap())
}

/// Whether a `Preference` / `Workflow` guidance row should pass `finalize`'s
/// floor exemption, given the active `scope_filters`. Guidance surfaces
/// regardless of textual match — BUT a **narrowly-scoped** (`Project` / `Repo`)
/// guidance row must match the active scope, so e.g. a `project:NVR-APP`
/// preference no longer leaks into a `project:mem` recall. **Broad**
/// (`Global` / `Workspace`) guidance always surfaces, and an **empty** filter
/// set (no scope requested — e.g. a raw MCP search) preserves the original
/// always-surface behavior for every guidance row.
fn guidance_in_scope(m: &CapabilityCapsuleRecord, scope_filters: &[String]) -> bool {
    match m.scope {
        Scope::Global | Scope::Workspace => true,
        Scope::Project | Scope::Repo => {
            scope_filters.is_empty() || matches_scope_filters(m, scope_filters)
        }
    }
}

/// O3 (closes oss-memory-diff O3) — per-source diversity cap. Walk the
/// already-ranked list and admit at most `cap` capsules per *source*
/// into the head, deferring any overflow (in rank order) to the tail. A
/// **soft** cap: nothing is dropped, so a token budget that reaches the
/// tail still includes the overflow — it just stops a batch of
/// near-identical capsules ingested in one session from monopolising the
/// top. Source = `session_id` when set, else the capsule's own id (so a
/// capsule with no session is its own source and is never capped).
/// `cap == 0` disables and returns the input unchanged.
///
/// Note: this is the *opposite* direction from transcript-side session
/// co-occurrence (`transcript_recall.rs`, which up-weights same-session
/// blocks) — kept as a separate function on purpose.
fn diversify_by_source(
    ranked: Vec<CapabilityCapsuleRecord>,
    cap: usize,
) -> Vec<CapabilityCapsuleRecord> {
    if cap == 0 {
        return ranked;
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut head: Vec<CapabilityCapsuleRecord> = Vec::with_capacity(ranked.len());
    let mut overflow: Vec<CapabilityCapsuleRecord> = Vec::new();
    for memory in ranked {
        let source = match memory.session_id.as_deref().filter(|s| !s.is_empty()) {
            Some(sid) => format!("session:{sid}"),
            None => format!("id:{}", memory.capability_capsule_id),
        };
        let seen = counts.entry(source).or_insert(0);
        if *seen < cap {
            *seen += 1;
            head.push(memory);
        } else {
            overflow.push(memory);
        }
    }
    head.extend(overflow);
    head
}

/// Top-level hybrid entry: take the lifecycle pool (e.g. all active
/// capsules for tenant) plus the SQL-side hybrid hits with their RRF
/// scores, and produce the user-visible ranked result.
///
/// `pool` carries the always-applicable Preference / Workflow rows
/// regardless of whether they hit the query — they pass `finalize`'s
/// floor exemption. `hybrid_hits` carries the relevance signal: an
/// (id → rrf_score) map driven by Tantivy BM25 + lance vector ANN,
/// RRF-merged in Rust (`Store::hybrid_candidates`). Items in both
/// inputs score with rrf_score + lifecycle signals; items only in the
/// pool score from lifecycle alone.
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

    // Hard expiry (Supermemory-style auto-forget): never surface a capsule
    // past its `expires_at`, even in the window before the decay worker
    // archives it. `None` expiry (the default for almost every capsule) is
    // never expired, so this is a no-op at scale. Direct get-by-id is
    // unaffected — like `archived`, expiry only hides from *recall*.
    let now = crate::storage::current_timestamp();
    let candidates: Vec<CapabilityCapsuleRecord> = candidates
        .into_iter()
        .filter(|m| !is_expired(m, &now))
        .collect();

    let floor = effective_floor(query);
    let empty_boosts: HashMap<String, i64> = HashMap::new();
    if !query.expand_graph {
        return Ok(finalize(
            score_with_hybrid(candidates, query, &hybrid_scores, &empty_boosts),
            floor,
            &query.scope_filters,
        ));
    }

    // Graph anchor derivation uses unfiltered top-N (floor is for the
    // user-visible result, not anchor selection).
    let preliminary_scored =
        score_with_hybrid(candidates.clone(), query, &hybrid_scores, &empty_boosts);
    let anchors = graph_anchor_nodes(&preliminary_scored);
    if anchors.is_empty() {
        return Ok(finalize(preliminary_scored, floor, &query.scope_filters));
    }

    let boost_by_id = compute_graph_boosts(graph, &anchors, dynamics).await?;
    Ok(finalize(
        score_with_hybrid(candidates, query, &hybrid_scores, &boost_by_id),
        floor,
        &query.scope_filters,
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

/// O4 (closes oss-memory-diff O4) — spread decay: dilute an anchor's
/// graph boost by its *fanout* (the number of capsules linked to it),
/// mem0's `1 / (1 + 0.001·(degree-1)²)`. A narrow anchor (a module, a
/// specific topic) keeps ~full boost; a high-fanout hub (`repo:` /
/// `project:` linked to hundreds of capsules) contributes almost
/// nothing — so a generic anchor can't blanket-boost everything beneath
/// it over the genuinely relevant hits. `degree ≤ 1` → no dilution.
fn spread_decay(degree: usize) -> f32 {
    let d = degree.saturating_sub(1) as f32;
    1.0 / (1.0 + 0.001 * d * d)
}

/// Build the per-capsule graph-boost map for the anchor set.
///
/// Each anchor's contribution is diluted by its fanout
/// ([`spread_decay`], O4): a 1-hop-linked capsule gets
/// `round(GRAPH_BOOST · spread(anchor_degree))`, maxed across the
/// anchors that reach it.
///
/// - **Dynamics off** (`None`, the default): one batched
///   [`GraphStore::incident_edges_for_nodes`] query for the whole anchor
///   set, then [`graph_boosts_from_edges`] computes degrees + boosts in
///   Rust. Avoids one `neighbors_within` round-trip per anchor (which
///   re-fetched a high-fanout hub's edges once per anchor).
/// - **Dynamics on** (`Some`): still walks per anchor, because it needs
///   each edge's time-decayed strength and enqueues every touched edge
///   for potentiation (best-effort — a closed channel never blocks the
///   read).
async fn compute_graph_boosts(
    graph: &dyn GraphStore,
    anchors: &[String],
    dynamics: Option<&EdgeDynamicsCtx>,
) -> Result<HashMap<String, i64>, crate::storage::GraphError> {
    let anchor_set: HashSet<&str> = anchors.iter().map(|s| s.as_str()).collect();

    let Some(ctx) = dynamics else {
        // Flat path: one query, all the math in Rust.
        let edges = graph.incident_edges_for_nodes(anchors).await?;
        return Ok(graph_boosts_from_edges(&edges, &anchor_set));
    };

    // Dynamics path: per-anchor walk (needs decayed strength + potentiation).
    let mut boosts: HashMap<String, i64> = HashMap::new();
    for anchor in anchors {
        let edges = graph.neighbors_within(anchor, 1, None).await?;
        let degree = edges
            .iter()
            .filter(|edge| {
                let other = if edge.from_node_id == *anchor {
                    &edge.to_node_id
                } else {
                    &edge.from_node_id
                };
                other.starts_with("capability_capsule:")
            })
            .count();
        let spread = spread_decay(degree);
        for edge in &edges {
            // Enqueue for potentiation off the read path; ignore send
            // errors (worker absent / channel closed) — best-effort.
            let _ = ctx.tx.send(crate::worker::potentiation_worker::EdgeAccess {
                from_node_id: edge.from_node_id.clone(),
                to_node_id: edge.to_node_id.clone(),
                relation: edge.relation.clone(),
            });
            let strength = crate::domain::edge_dynamics::decayed_strength(edge, &ctx.now);
            let boost = ((GRAPH_BOOST as f32) * spread * strength).round() as i64;
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

/// O4 flat-path core: derive per-capsule graph boosts from the raw
/// `(from, to)` edges incident to the anchor set. Behaviourally identical
/// to the per-anchor walk (just fed from one batched query): each
/// anchor's *capsule fanout degree* dilutes the boost it confers
/// ([`spread_decay`]), and each non-anchor capsule endpoint takes the max
/// over the anchors that reach it. Pure for testability.
fn graph_boosts_from_edges(
    edges: &[(String, String)],
    anchor_set: &HashSet<&str>,
) -> HashMap<String, i64> {
    // Pass 1: per-anchor capsule fanout degree (count edges whose *other*
    // endpoint is a capsule — entity↔entity edges don't inflate it).
    let mut degree: HashMap<&str, usize> = HashMap::new();
    for (from, to) in edges {
        for (endpoint, other) in [(from.as_str(), to.as_str()), (to.as_str(), from.as_str())] {
            if anchor_set.contains(endpoint) && other.starts_with("capability_capsule:") {
                *degree.entry(endpoint).or_insert(0) += 1;
            }
        }
    }
    // Pass 2: boost each non-anchor capsule endpoint by spread(degree) of
    // the anchor it links to, maxed across anchors.
    let mut boosts: HashMap<String, i64> = HashMap::new();
    for (from, to) in edges {
        for (anchor, other) in [(from.as_str(), to.as_str()), (to.as_str(), from.as_str())] {
            if !anchor_set.contains(anchor) || anchor_set.contains(other) {
                continue;
            }
            if let Some(mid) = other.strip_prefix("capability_capsule:") {
                let d = degree.get(anchor).copied().unwrap_or(0);
                let boost = ((GRAPH_BOOST as f32) * spread_decay(d)).round() as i64;
                boosts
                    .entry(mid.to_string())
                    .and_modify(|m| *m = (*m).max(boost))
                    .or_insert(boost);
            }
        }
    }
    boosts
}

/// Computes the additive non-recall portion of a memory's score (the
/// "lifecycle" stack: scope, intent, confidence, validation, freshness,
/// staleness, graph boost, status penalty). Used by `score_with_hybrid`
/// after the Rust-side RRF score has been added.
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

/// Score each candidate with the Rust-side RRF (the fused lex+sem
/// signal as a Float32) plus the lifecycle / scope / intent
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

            // The RRF score (from `rrf_merge`) is already (1/(60+lex_rank))
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
    // Diary entries are filtered out in `hybrid_candidates_compose`'s
    // Rust post-fetch filter, so they shouldn't reach this scorer.
    // Score 0 as a defense-in-depth fallback in case that filter ever
    // drifts.
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

    #[test]
    fn is_expired_respects_hard_deadline() {
        let now = "00000001781106546457".to_string(); // 20-digit ms
        let mut m = CapabilityCapsuleRecord::default();
        // No expiry → never expired (the default for almost every capsule).
        assert!(!is_expired(&m, &now));
        // 1 ms before now → expired.
        m.expires_at = Some("00000001781106546456".to_string());
        assert!(is_expired(&m, &now));
        // Exactly now → expired (<=).
        m.expires_at = Some(now.clone());
        assert!(is_expired(&m, &now));
        // Future → not expired.
        m.expires_at = Some("00000001781106546458".to_string());
        assert!(!is_expired(&m, &now));
        // Malformed (wrong width) → not expired (never hide on a bad ts).
        m.expires_at = Some("123".to_string());
        assert!(!is_expired(&m, &now));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rank_skips_expired_candidates() {
        // End of the wiring: a capsule whose `expires_at` has passed is
        // dropped from the ranked result, while a non-expiring sibling
        // survives. Preference type so the relevance floor can't be the
        // reason either one is dropped — isolating the expiry filter.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::open(&dir.path().join("exp.lance"))
            .await
            .unwrap();

        let mut live = fixture_memory("live");
        live.capability_capsule_type = CapabilityCapsuleType::Preference;
        let mut expired = fixture_memory("expired");
        expired.capability_capsule_type = CapabilityCapsuleType::Preference;
        expired.expires_at = Some("00000001000000000000".to_string()); // long past

        let out =
            rank_with_hybrid_and_graph(vec![live, expired], vec![], &fixture_query(), &store, None)
                .await
                .unwrap();

        let ids: Vec<String> = out
            .iter()
            .map(|m| m.capability_capsule_id.clone())
            .collect();
        assert!(
            ids.contains(&"live".to_string()),
            "non-expired must survive: {ids:?}"
        );
        assert!(
            !ids.contains(&"expired".to_string()),
            "expired capsule must be skipped by recall: {ids:?}"
        );
    }

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
            last_used_at: None,
            last_recalled_at: None,
            expires_at: None,
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

    /// RRF score: sum of `1.0/(60+rank)` per source — the same formula
    /// `pipeline::ranking::rrf_merge` produces. Used in tests to build
    /// the Float32 score the hybrid recall path yields.
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
        let kept = finalize(scored, 40, &[]);
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
        // Always-applicable types survive even a floor far above their score
        // (empty scope filter = no scope requested, original behavior).
        let kept = finalize(scored, 999, &[]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn finalize_scopes_narrow_guidance_to_active_scope() {
        // A Project-scoped Preference must NOT leak across projects: with a
        // non-matching scope filter it is dropped; with a matching one it
        // surfaces; a Global preference always surfaces regardless.
        let mut nvr = fixture_memory("nvr");
        nvr.capability_capsule_type = CapabilityCapsuleType::Preference;
        nvr.scope = Scope::Project;
        nvr.project = Some("NVR-APP".to_string());
        let mut global = fixture_memory("global");
        global.capability_capsule_type = CapabilityCapsuleType::Preference;
        global.scope = Scope::Global;
        let scored = || {
            vec![
                ScoredMemory {
                    memory: nvr.clone(),
                    score: 0,
                },
                ScoredMemory {
                    memory: global.clone(),
                    score: 0,
                },
            ]
        };

        // Recall scoped to project:mem → NVR-APP preference dropped, global kept.
        let kept = finalize(scored(), 999, &["project:mem".to_string()]);
        let ids: Vec<&str> = kept
            .iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["global"],
            "narrow out-of-scope guidance must be dropped"
        );

        // Recall scoped to project:NVR-APP → both surface.
        let kept = finalize(scored(), 999, &["project:NVR-APP".to_string()]);
        assert_eq!(kept.len(), 2);

        // No scope filter → original always-surface behavior preserved.
        let kept = finalize(scored(), 999, &[]);
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

    // ── O3: per-source diversity cap ────────────────────────────────
    fn sess(id: &str, session: &str) -> CapabilityCapsuleRecord {
        let mut m = fixture_memory(id);
        m.session_id = Some(session.to_string());
        m
    }

    fn ids(ms: &[CapabilityCapsuleRecord]) -> Vec<&str> {
        ms.iter()
            .map(|m| m.capability_capsule_id.as_str())
            .collect()
    }

    #[test]
    fn diversify_floats_a_distinct_source_above_same_session_overflow() {
        // a,b,c,d from session s1; x from s2. cap 2 → s1 keeps a,b in the
        // head; c,d overflow to the tail; x (originally last) floats up
        // above the s1 overflow. The whole set survives (soft cap).
        let ranked = vec![
            sess("a", "s1"),
            sess("b", "s1"),
            sess("c", "s1"),
            sess("d", "s1"),
            sess("x", "s2"),
        ];
        let out = diversify_by_source(ranked, 2);
        assert_eq!(ids(&out), vec!["a", "b", "x", "c", "d"]);
    }

    #[test]
    fn diversify_leaves_distinct_and_sessionless_capsules_untouched() {
        // Two session-less capsules (each its own source) + one in a
        // session: cap 1 caps nothing because no source repeats.
        let ranked = vec![fixture_memory("a"), fixture_memory("b"), sess("c", "s1")];
        let out = diversify_by_source(ranked, 1);
        assert_eq!(ids(&out), vec!["a", "b", "c"]);
    }

    #[test]
    fn diversify_cap_zero_is_a_noop() {
        let ranked = vec![sess("a", "s1"), sess("b", "s1"), sess("c", "s1")];
        let out = diversify_by_source(ranked, 0);
        assert_eq!(ids(&out), vec!["a", "b", "c"]);
    }

    // ── O4: graph-boost spread decay by anchor fanout ───────────────
    #[test]
    fn spread_decay_dilutes_high_fanout_monotonically() {
        // degree ≤ 1 → no dilution.
        assert!((spread_decay(0) - 1.0).abs() < 1e-6);
        assert!((spread_decay(1) - 1.0).abs() < 1e-6);
        // small fanout barely moves; large fanout heavily dilutes.
        assert!(spread_decay(2) > 0.99);
        assert!(spread_decay(50) < 0.5);
        // strictly decreasing in degree.
        assert!(spread_decay(50) < spread_decay(10));
        assert!(spread_decay(10) < spread_decay(3));
    }

    #[test]
    fn graph_boosts_from_edges_degree_max_and_anchor_skip() {
        use std::collections::HashSet;
        // anchor entities: a high-fanout hub (degree 20) + a narrow one
        // (degree 1). Capsule c1 is linked to BOTH; it must take the max
        // (the narrow anchor's higher spread). c2 sits only under the hub.
        // An anchor→anchor edge confers no boost.
        let anchors: HashSet<&str> = [
            "project:hub",
            "module:narrow",
            "capability_capsule:anchorcap",
        ]
        .into_iter()
        .collect();
        let e = |a: String, b: &str| (a, b.to_string());
        let mut edges: Vec<(String, String)> = (1..=20)
            .map(|i| e(format!("capability_capsule:c{i}"), "project:hub"))
            .collect(); // hub fanout degree = 20
        edges.push(e("capability_capsule:c1".into(), "module:narrow")); // narrow degree = 1
        edges.push(e("capability_capsule:anchorcap".into(), "module:narrow")); // anchor endpoint
        edges.reverse(); // order must not matter

        let boosts = graph_boosts_from_edges(&edges, &anchors);

        // c1 (under both) → max(spread(20), spread(1)) = narrow's full boost.
        assert_eq!(
            boosts.get("c1"),
            Some(&GRAPH_BOOST),
            "c1 takes the max over anchors = narrow's full boost"
        );
        // c2 (hub-only, degree 20) → diluted below flat.
        let c2 = *boosts.get("c2").expect("c2 boosted under hub");
        assert!(
            c2 < GRAPH_BOOST && c2 > 0,
            "c2 (hub degree 20) must dilute below flat {GRAPH_BOOST}, got {c2}"
        );
        // anchorcap is itself an anchor → never boosted.
        assert_eq!(boosts.get("anchorcap"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn graph_boost_dilutes_high_fanout_anchor() {
        use crate::domain::capability_capsule::GraphEdge;
        use crate::storage::Store;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("o4.lance")).await.unwrap();
        let edge = |capsule: &str, entity: &str| GraphEdge {
            from_node_id: format!("capability_capsule:{capsule}"),
            to_node_id: entity.to_string(),
            relation: "applies_to".into(),
            valid_from: "00000001780000000000".into(),
            valid_to: None,
            confidence: None,
            extractor: None,
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        };
        // project:hub has 20 linked capsules (high fanout); project:narrow
        // has 1.
        for i in 0..20 {
            store
                .add_edge_direct(&edge(&format!("h{i}"), "project:hub"))
                .await
                .unwrap();
        }
        store
            .add_edge_direct(&edge("n0", "project:narrow"))
            .await
            .unwrap();

        let graph: &dyn GraphStore = &store;
        let boosts = compute_graph_boosts(
            graph,
            &["project:hub".to_string(), "project:narrow".to_string()],
            None,
        )
        .await
        .unwrap();

        // Narrow anchor (degree 1) → full flat boost.
        assert_eq!(boosts.get("n0"), Some(&GRAPH_BOOST));
        // High-fanout anchor (degree 20) → diluted below flat, still > 0.
        let hub_boost = *boosts.get("h0").expect("hub capsule must be boosted");
        assert!(
            hub_boost < GRAPH_BOOST && hub_boost > 0,
            "degree-20 anchor must dilute below flat {GRAPH_BOOST}, got {hub_boost}",
        );
    }
}
