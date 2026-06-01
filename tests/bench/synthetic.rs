//! Deterministic synthetic fixture generator for the recall bench.
//!
//! `generate(&SyntheticConfig)` produces a `Fixture` whose qrels are derived
//! purely from the construction logic — no randomness, no I/O, no timestamps.
use crate::bench::fixture::{CapsuleFixture, EdgeFixture, Fixture, QueryFixture};
use std::collections::{HashMap, HashSet};

/// Fixed vocabulary; generator takes the first `num_topics` entries.
const TOPIC_TERMS: &[&str] = &[
    "tokio",
    "lance",
    "duckdb",
    "embedding",
    "graph",
    "transcript",
    "ranking",
    "chunking",
    "entity",
    "session",
    "vector",
    "decay",
];

/// Configuration for the synthetic fixture generator.
/// `seed` is reserved for future stochastic extensions; generation is currently
/// fully deterministic from the other fields.  Allow dead_code to keep the
/// public API stable across tasks.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SyntheticConfig {
    pub seed: u64,
    pub num_topics: usize,
    pub capsules_per_topic: usize,
    pub num_long: usize,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            // ≥ 12 short capsules per topic so a tail-targeted query has
            // enough exact same-topic distractors to push a chunking-OFF
            // long capsule (head-only embedding, orthogonal to the tail
            // query) out of the top-10 — making recall@10 discriminate the
            // ③ chunking effect instead of saturating at 1.0 on a tiny corpus.
            num_topics: 6,
            capsules_per_topic: 12,
            num_long: 3,
        }
    }
}

/// Fixed-width sortable timestamp helper: `1_778_000_000_000 + n` zero-padded to 20 digits.
fn ts(n: u64) -> String {
    format!("{:020}", 1_778_000_000_000u64 + n)
}

/// Generate a fully deterministic `Fixture` from `config`.
/// All qrel sets are derived independently by construction — no drain/re-insert.
pub fn generate(cfg: &SyntheticConfig) -> Fixture {
    assert!(
        cfg.num_topics <= TOPIC_TERMS.len(),
        "num_topics ({}) exceeds TOPIC_TERMS vocabulary size ({})",
        cfg.num_topics,
        TOPIC_TERMS.len()
    );

    let topics: Vec<String> = TOPIC_TERMS[..cfg.num_topics]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let mut capsules: Vec<CapsuleFixture> = Vec::new();
    let mut queries: Vec<QueryFixture> = Vec::new();
    let mut qrels: HashMap<String, HashSet<String>> = HashMap::new();

    // ── Short capsules & per-topic queries ────────────────────────────────────
    for (t_idx, topic) in topics.iter().enumerate() {
        let mut relevant_ids: HashSet<String> = HashSet::new();
        for j in 0..cfg.capsules_per_topic {
            let n = (t_idx * cfg.capsules_per_topic + j) as u64;
            let id = format!("cap_{}_{}", topic, j);
            let content = format!(
                "The {} subsystem (ts:{}) handles {} operations efficiently.",
                topic,
                ts(n),
                topic
            );
            capsules.push(CapsuleFixture {
                id: id.clone(),
                content,
                topics: vec![topic.clone()],
                long: false,
                tail_topic: None,
            });
            relevant_ids.insert(id);
        }
        let q_id = format!("q_{}", topic);
        queries.push(QueryFixture {
            id: q_id.clone(),
            text: format!("how does {} work", topic),
            topic: topic.clone(),
            expand_graph: false,
            tail_targeted: false,
        });
        qrels.insert(q_id, relevant_ids);
    }

    // ── Long capsules & tail-targeted queries ─────────────────────────────────
    let long_base_n = (cfg.num_topics * cfg.capsules_per_topic) as u64;
    for i in 0..cfg.num_long {
        let head_topic = &topics[i % cfg.num_topics];
        let tail_topic = &topics[(i + 1) % cfg.num_topics];
        let filler = "lorem ipsum dolor sit amet ".repeat(500);
        let content = format!(
            "{} overview. {}finally the {} appendix.",
            head_topic, filler, tail_topic
        );
        let id = format!("cap_long_{}", i);
        capsules.push(CapsuleFixture {
            id: id.clone(),
            content,
            topics: vec![head_topic.clone()],
            long: true,
            tail_topic: Some(tail_topic.clone()),
        });

        let q_id = format!("q_tail_{}", i);
        queries.push(QueryFixture {
            id: q_id.clone(),
            text: format!("details about {}", tail_topic),
            topic: tail_topic.clone(),
            expand_graph: false,
            tail_targeted: true,
        });
        let mut tail_rel: HashSet<String> = HashSet::new();
        tail_rel.insert(id);
        qrels.insert(q_id, tail_rel);

        let _ = long_base_n + i as u64; // keep n in scope for potential future use
    }

    // ── Co-occurrence edges ────────────────────────────────────────────────────
    let mut edges: Vec<EdgeFixture> = Vec::new();
    for i in 0..cfg.num_topics.saturating_sub(1) {
        edges.push(EdgeFixture {
            from_topic: topics[i].clone(),
            to_topic: topics[i + 1].clone(),
            strength: 0.2 + 0.1 * (i as f32),
        });
    }

    // ── Graph-anchored query ───────────────────────────────────────────────────
    // Anchor: topics[0]; target topic: topics[1] (reachable via first edge).
    // Named "q_graph_expand" to avoid collision with the per-topic "q_{T}"
    // entry generated when T = "graph" (TOPIC_TERMS[4]).
    let graph_q_id = "q_graph_expand".to_string();
    // qrel = same set as q_{topics[1]}: short capsules of topics[1]
    let graph_rel: HashSet<String> = (0..cfg.capsules_per_topic)
        .map(|j| format!("cap_{}_{}", topics[1], j))
        .collect();
    queries.push(QueryFixture {
        id: graph_q_id.clone(),
        text: format!("how does {} work", topics[0]),
        topic: topics[1].clone(),
        expand_graph: true,
        tail_targeted: false,
    });
    qrels.insert(graph_q_id, graph_rel);

    Fixture {
        tenant: "bench".to_string(),
        capsules,
        queries,
        edges,
        qrels,
        topics,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_seed() {
        let cfg = SyntheticConfig::default();
        let f1 = generate(&cfg);
        let f2 = generate(&cfg);
        let contents1: Vec<String> = f1.capsules.iter().map(|c| c.content.clone()).collect();
        let contents2: Vec<String> = f2.capsules.iter().map(|c| c.content.clone()).collect();
        assert_eq!(
            contents1, contents2,
            "generate() must be fully deterministic for the same config"
        );
    }

    #[test]
    fn qrels_match_topic_assignment() {
        let fixture = generate(&SyntheticConfig::default());
        let capsule_map: std::collections::HashMap<String, &CapsuleFixture> =
            fixture.capsules.iter().map(|c| (c.id.clone(), c)).collect();

        for query in &fixture.queries {
            let rel = fixture
                .qrels
                .get(&query.id)
                .unwrap_or_else(|| panic!("no qrel entry for query {}", query.id));
            assert!(
                !rel.is_empty(),
                "qrel set for query {} must be non-empty",
                query.id
            );
            for cap_id in rel {
                let cap = capsule_map
                    .get(cap_id)
                    .unwrap_or_else(|| panic!("qrel references unknown capsule {}", cap_id));
                let topic_match = cap.topics.contains(&query.topic);
                let tail_match = cap.tail_topic.as_deref() == Some(query.topic.as_str());
                assert!(
                    topic_match || tail_match,
                    "capsule {} in qrels for query {} neither has topic '{}' in topics {:?} nor tail_topic {:?}",
                    cap_id,
                    query.id,
                    query.topic,
                    cap.topics,
                    cap.tail_topic
                );
            }
        }
    }

    #[test]
    fn has_long_capsules_and_tail_queries() {
        let fixture = generate(&SyntheticConfig::default());
        assert!(
            fixture.capsules.iter().any(|c| c.long),
            "fixture must contain at least one long capsule"
        );
        assert!(
            fixture.queries.iter().any(|q| q.tail_targeted),
            "fixture must contain at least one tail-targeted query"
        );
        assert!(
            !fixture.edges.is_empty(),
            "fixture must contain at least one edge"
        );
    }
}
