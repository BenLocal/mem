//! Synthetic fixture generator. Deterministic given (seed, config).

use super::fixture::*;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use std::collections::HashSet;

pub struct TopicSeed {
    pub canonical: &'static str,
    pub aliases: &'static [&'static str],
}

pub const DEFAULT_TOPICS: &[TopicSeed] = &[
    TopicSeed {
        canonical: "Rust async",
        aliases: &["tokio", "futures", "await"],
    },
    TopicSeed {
        canonical: "DuckDB",
        aliases: &["duckdb", "olap", "columnar"],
    },
    TopicSeed {
        canonical: "HNSW",
        aliases: &["usearch", "ann", "vector index"],
    },
    TopicSeed {
        canonical: "BM25",
        aliases: &["fts", "tantivy", "lexical"],
    },
    TopicSeed {
        canonical: "session window",
        aliases: &["sliding", "bucket", "auto-bucket"],
    },
    TopicSeed {
        canonical: "ranking",
        aliases: &["rrf", "fusion", "reranker"],
    },
    TopicSeed {
        canonical: "embedding",
        aliases: &["vector", "encoder", "dense"],
    },
    TopicSeed {
        canonical: "MCP",
        aliases: &["model context protocol", "stdio", "json-rpc"],
    },
    TopicSeed {
        canonical: "axum",
        aliases: &["http", "router", "tokio runtime"],
    },
    TopicSeed {
        canonical: "schema migration",
        aliases: &["alter table", "ddl", "ddl drift"],
    },
    TopicSeed {
        canonical: "graph edges",
        aliases: &["valid_from", "supersedes", "bitemporal"],
    },
    TopicSeed {
        canonical: "cross-encoder",
        aliases: &["bge-reranker", "ms-marco", "rerank model"],
    },
];

const NOISE_WORDS: &[&str] = &[
    "the",
    "in",
    "and",
    "to",
    "that",
    "with",
    "for",
    "is",
    "are",
    "we",
    "this",
    "they",
    "their",
    "from",
    "after",
    "before",
    "should",
    "would",
    "could",
    "discuss",
    "consider",
    "regarding",
    "notes",
    "context",
];

pub struct SyntheticConfig {
    pub seed: u64,
    pub session_count: usize,
    pub blocks_per_session: usize,
    pub topic_pool: &'static [TopicSeed],
    pub query_count: usize,
    pub noise_words_per_block: usize,
    pub tenant: &'static str,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self {
            seed: 42,
            session_count: 30,
            blocks_per_session: 8,
            topic_pool: DEFAULT_TOPICS,
            query_count: 24,
            noise_words_per_block: 30,
            tenant: "local",
        }
    }
}

pub fn generate(config: &SyntheticConfig) -> Fixture {
    let mut rng = StdRng::seed_from_u64(config.seed);

    // Step 1: Generate sessions, each tagged with 1-2 topic indices.
    let mut sessions: Vec<SessionFixture> = Vec::with_capacity(config.session_count);
    let mut session_topics: Vec<Vec<usize>> = Vec::with_capacity(config.session_count);

    for s_idx in 0..config.session_count {
        let topics_n = if rng.gen_bool(0.5) { 1 } else { 2 };
        let mut chosen: Vec<usize> = (0..config.topic_pool.len()).collect();
        chosen.shuffle(&mut rng);
        let topics: Vec<usize> = chosen.into_iter().take(topics_n).collect();
        session_topics.push(topics.clone());

        // 90 days span → each session gets a base offset; blocks within session monotonic.
        let base_day = rng.gen_range(0..90u64);
        let session_id = format!("synth_session_{:03}", s_idx);
        let started_at = format_timestamp(2026, 5, 3, base_day, 0);

        let mut blocks: Vec<BlockFixture> = Vec::with_capacity(config.blocks_per_session);
        for b_idx in 0..config.blocks_per_session {
            // Pick topic for this block (round-robin from session's topics).
            let topic_idx = topics[b_idx % topics.len()];
            let topic = &config.topic_pool[topic_idx];
            let term = if rng.gen_bool(0.4) {
                topic.canonical.to_string()
            } else {
                topic.aliases[rng.gen_range(0..topic.aliases.len())].to_string()
            };

            // Build content: shuffled mix of noise words + the term.
            let mut content_words: Vec<String> = (0..config.noise_words_per_block)
                .map(|_| NOISE_WORDS[rng.gen_range(0..NOISE_WORDS.len())].to_string())
                .collect();
            let insert_pos = rng.gen_range(0..=content_words.len());
            content_words.insert(insert_pos, term);
            let content = content_words.join(" ");

            let role = if b_idx % 2 == 0 { "user" } else { "assistant" };
            let created_at = format_timestamp(2026, 5, 3, base_day, b_idx as u64);

            blocks.push(BlockFixture {
                block_id: format!("synth_{:03}_{:02}", s_idx, b_idx),
                role: role.to_string(),
                block_type: "text".to_string(),
                content,
                created_at,
            });
        }

        sessions.push(SessionFixture {
            session_id,
            started_at,
            blocks,
        });
    }

    // Step 2: Generate queries. Each picks a topic (uniform from topics that
    // appear in at least one session). The query text is
    // "how do I use <alias> for <canonical>?".
    let covered_topics: Vec<usize> = {
        let mut set: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        for ts in &session_topics {
            for &t in ts {
                set.insert(t);
            }
        }
        set.into_iter().collect()
    };
    let mut queries: Vec<QueryFixture> = Vec::with_capacity(config.query_count);
    for q_idx in 0..config.query_count {
        let topic_idx = covered_topics[rng.gen_range(0..covered_topics.len())];
        let topic = &config.topic_pool[topic_idx];
        let alias = topic.aliases[rng.gen_range(0..topic.aliases.len())];
        let text = format!(
            "how do I use {} for {} in production?",
            alias, topic.canonical
        );

        // Synthetic judgments: any session whose topic list includes this topic.
        let synthetic_judgments: HashSet<String> = session_topics
            .iter()
            .enumerate()
            .filter(|(_, topics)| topics.contains(&topic_idx))
            .map(|(s_idx, _)| format!("synth_session_{:03}", s_idx))
            .collect();

        queries.push(QueryFixture {
            query_id: format!("synth_q_{:03}", q_idx),
            text,
            anchor_session_id: None,
            anchor_entities: vec![topic.canonical.to_string()],
            synthetic_judgments: Some(synthetic_judgments),
        });
    }

    Fixture {
        kind: FixtureKind::Synthetic { seed: config.seed },
        tenant: config.tenant.to_string(),
        sessions,
        queries,
    }
}

/// Compose a sortable timestamp string compatible with the project's
/// `timestamp_score` parser (numeric prefix, microsecond resolution).
fn format_timestamp(_year: u64, _month: u64, _day: u64, day_offset: u64, seq: u64) -> String {
    // Project format: "00000000<unix-ms>". Generate a synthetic monotonic
    // millisecond by combining day_offset and seq.
    let ms = 1_700_000_000_000_u64 + day_offset * 86_400_000 + seq * 1_000;
    format!("{:020}", ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_produces_30_sessions_240_blocks_24_queries() {
        let f = generate(&SyntheticConfig::default());
        assert_eq!(f.sessions.len(), 30);
        assert_eq!(
            f.sessions.iter().map(|s| s.blocks.len()).sum::<usize>(),
            240
        );
        assert_eq!(f.queries.len(), 24);
        assert_eq!(f.tenant, "local");
        assert!(matches!(f.kind, FixtureKind::Synthetic { seed: 42 }));
    }

    #[test]
    fn generation_is_deterministic_for_same_seed() {
        let f1 = generate(&SyntheticConfig::default());
        let f2 = generate(&SyntheticConfig::default());
        assert_eq!(
            f1.sessions[0].blocks[0].content,
            f2.sessions[0].blocks[0].content
        );
        assert_eq!(f1.queries[0].text, f2.queries[0].text);
    }

    #[test]
    fn different_seeds_produce_different_content() {
        let f1 = generate(&SyntheticConfig::default());
        let f2 = generate(&SyntheticConfig {
            seed: 999,
            ..SyntheticConfig::default()
        });
        // At least one of these should differ.
        assert!(
            f1.sessions[0].blocks[0].content != f2.sessions[0].blocks[0].content
                || f1.queries[0].text != f2.queries[0].text
        );
    }

    #[test]
    fn synthetic_judgments_are_populated() {
        let f = generate(&SyntheticConfig::default());
        for q in &f.queries {
            let j = q
                .synthetic_judgments
                .as_ref()
                .expect("synthetic judgments must be Some");
            assert!(
                !j.is_empty(),
                "every synthetic query should have ≥1 relevant session"
            );
        }
    }

    #[test]
    fn anchor_entities_match_topic_canonical_names() {
        let f = generate(&SyntheticConfig::default());
        let canonicals: Vec<&str> = DEFAULT_TOPICS.iter().map(|t| t.canonical).collect();
        for q in &f.queries {
            assert_eq!(q.anchor_entities.len(), 1);
            assert!(canonicals.contains(&q.anchor_entities[0].as_str()));
        }
    }

    #[test]
    fn block_content_contains_topic_term() {
        // Take the first session's first block; its topic is session_topics[0][0],
        // and the content must contain canonical OR one of the aliases.
        let f = generate(&SyntheticConfig::default());
        let first_block = &f.sessions[0].blocks[0];
        let any_topic_hit = DEFAULT_TOPICS.iter().any(|t| {
            first_block.content.contains(t.canonical)
                || t.aliases.iter().any(|a| first_block.content.contains(a))
        });
        assert!(
            any_topic_hit,
            "content should embed at least one topic term"
        );
    }
}
