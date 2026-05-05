//! LongMemEval bench runner. Per-question ingest + 3-rung re-rank.
//! See docs/superpowers/specs/2026-05-05-mempalace-bench-parity-design.md.

use super::longmemeval_dataset::*;
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::{EmbeddingProvider, FakeEmbeddingProvider};
use mem::storage::{DuckDbRepository, VectorIndex};
use std::sync::Arc;

const TENANT: &str = "bench";

/// Ingest one LongMemEval question's haystack into the given repo + index.
/// Each (session, turn) pair becomes one ConversationMessage; each
/// embed-eligible message gets embedded + upserted into the VectorIndex.
pub async fn ingest_corpus(
    repo: &DuckDbRepository,
    index: &VectorIndex,
    embedder: &Arc<dyn EmbeddingProvider>,
    question: &LongMemEvalQuestion,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut count = 0usize;
    for session in &question.haystack_sessions {
        let session_started_ms = parse_started_at_to_ms(session.started_at.as_deref())
            .unwrap_or_else(|| stable_session_seed_ms(&session.session_id));
        for (turn_idx, turn) in session.turns.iter().enumerate() {
            let block_id = format!(
                "{}_{}_{}",
                question.question_id, session.session_id, turn_idx
            );
            let role = parse_role(&turn.role);
            let content = turn.content.clone();
            let created_ms = session_started_ms + (turn_idx as u64) * 60_000;
            let msg = ConversationMessage {
                message_block_id: block_id.clone(),
                tenant: TENANT.to_string(),
                session_id: Some(session.session_id.clone()),
                role,
                block_type: BlockType::Text,
                content: content.clone(),
                embed_eligible: true,
                created_at: format!("{:020}", created_ms),
                // Bench defaults for fields not used by ranking:
                caller_agent: "bench".to_string(),
                transcript_path: format!("/tmp/lme/{}.jsonl", question.question_id),
                message_uuid: Some(block_id.clone()),
                tool_use_id: None,
                tool_name: None,
                line_number: 0,
                block_index: turn_idx as u32,
            };
            repo.create_conversation_message(&msg).await?;
            let v = embedder.embed_text(&content).await?;
            index.upsert(&block_id, &v).await?;
            count += 1;
        }
    }
    Ok(count)
}

fn parse_role(s: &str) -> MessageRole {
    MessageRole::from_db_str(s).unwrap_or(MessageRole::User)
}

/// Parse an ISO-8601 timestamp like "2024-03-15T00:00:00" into ms since epoch.
/// Returns None on parse failure; caller falls back to a stable seed.
fn parse_started_at_to_ms(s: Option<&str>) -> Option<u64> {
    let s = s?;
    // Best-effort: extract YYYY-MM-DD prefix and convert to ms.
    let date = s.get(..10)?;
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: u64 = parts[0].parse().ok()?;
    let m: u64 = parts[1].parse().ok()?;
    let d: u64 = parts[2].parse().ok()?;
    // Naive: treat year/month/day as a unique offset (not a real epoch).
    Some(((y - 1970) * 365 + m * 30 + d) * 86_400_000)
}

/// Stable per-session millisecond seed (used when started_at is missing).
/// Hash the session_id to a u64 in a deterministic way.
fn stable_session_seed_ms(session_id: &str) -> u64 {
    let mut h: u64 = 14_695_981_039_346_656_037; // FNV-1a basis
    for b in session_id.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    1_700_000_000_000 + (h % (90 * 86_400_000))
}

use super::runner::SourceMix;
use mem::pipeline::transcript_recall::{score_candidates, ScoringOpts};
use std::collections::{HashMap, HashSet};

const TOP_K_CANDIDATES: usize = 50;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // rung_id + mempalace_label used by Task 8 bench harness
pub struct LongMemEvalRung {
    pub rung_id: &'static str,
    pub mempalace_label: &'static str,
    pub source: SourceMix,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
}

#[rustfmt::skip]
pub const RUNGS: &[LongMemEvalRung] = &[
    LongMemEvalRung {
        rung_id: "longmemeval_raw",
        mempalace_label: "raw",
        source: SourceMix::HnswOnly,
        disable_session_cooc: true,
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_rooms",
        mempalace_label: "rooms",
        source: SourceMix::HnswOnly,
        disable_session_cooc: false,
        disable_anchor: true,
        disable_freshness: true,
    },
    LongMemEvalRung {
        rung_id: "longmemeval_full",
        mempalace_label: "full",
        source: SourceMix::Both,
        disable_session_cooc: false,
        disable_anchor: false,
        disable_freshness: false,
    },
];

/// Retrieve and rank under the given rung's config; project to session-level
/// top-K. Caller supplies an already-populated repo + vector index for the
/// current question's corpus.
pub async fn retrieve_for_rung(
    repo: &DuckDbRepository,
    index: &VectorIndex,
    embedder: &Arc<dyn EmbeddingProvider>,
    query_text: &str,
    rung: &LongMemEvalRung,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    // 1. Get candidates per source mix.
    let bm25 = match rung.source {
        SourceMix::Bm25Only | SourceMix::Both => repo
            .bm25_transcript_candidates(TENANT, query_text, TOP_K_CANDIDATES)
            .await
            .unwrap_or_default(),
        SourceMix::HnswOnly => vec![],
    };
    let hnsw_ids: Vec<(String, f32)> = match rung.source {
        SourceMix::HnswOnly | SourceMix::Both => {
            let qv = embedder.embed_text(query_text).await?;
            index
                .search(&qv, TOP_K_CANDIDATES)
                .await
                .unwrap_or_default()
        }
        SourceMix::Bm25Only => vec![],
    };

    // 2. Build rank maps (rank starts at 1).
    let mut lex_ranks: HashMap<String, usize> = HashMap::new();
    for (i, m) in bm25.iter().enumerate() {
        lex_ranks.insert(m.message_block_id.clone(), i + 1);
    }
    let mut sem_ranks: HashMap<String, usize> = HashMap::new();
    for (i, (id, _)) in hnsw_ids.iter().enumerate() {
        sem_ranks.insert(id.clone(), i + 1);
    }

    // 3. Hydrate HNSW candidates back to ConversationMessage. BM25 already
    // returned full records; for HNSW-only ids, fetch from repo by id.
    let mut by_id: HashMap<String, mem::domain::ConversationMessage> = HashMap::new();
    for m in bm25.into_iter() {
        by_id.entry(m.message_block_id.clone()).or_insert(m);
    }
    for (id, _) in &hnsw_ids {
        if !by_id.contains_key(id) {
            if let Ok(Some(m)) = repo.get_conversation_message_by_id(TENANT, id).await {
                by_id.insert(id.clone(), m);
            }
        }
    }
    let candidates: Vec<mem::domain::ConversationMessage> = by_id.into_values().collect();

    // 4. Score via production pipeline.
    let opts = ScoringOpts {
        anchor_session_id: None, // LongMemEval has no anchor concept
        disable_session_cooc: rung.disable_session_cooc,
        disable_anchor: rung.disable_anchor,
        disable_freshness: rung.disable_freshness,
    };
    let scored = score_candidates(candidates, &lex_ranks, &sem_ranks, opts);

    // 5. Project to session-level top-K (highest-score block per session).
    let mut session_seen: HashSet<String> = HashSet::new();
    let mut run: Vec<String> = Vec::with_capacity(20);
    for sb in scored {
        if let Some(sid) = sb.message.session_id.clone() {
            if session_seen.insert(sid.clone()) {
                run.push(sid);
                if run.len() >= 20 {
                    break;
                }
            }
        }
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn make_question(
        qid: &str,
        sessions: Vec<(&str, Vec<(&str, &str)>)>,
    ) -> LongMemEvalQuestion {
        LongMemEvalQuestion {
            question_id: qid.to_string(),
            question: format!("question for {}", qid),
            haystack_sessions: sessions
                .into_iter()
                .map(|(sid, turns)| LongMemEvalSession {
                    session_id: sid.to_string(),
                    started_at: Some("2024-03-15T00:00:00".to_string()),
                    turns: turns
                        .into_iter()
                        .map(|(role, content)| LongMemEvalTurn {
                            role: role.to_string(),
                            content: content.to_string(),
                        })
                        .collect(),
                })
                .collect(),
            answer_session_ids: vec![],
            question_date: None,
        }
    }

    #[tokio::test]
    async fn ingest_corpus_creates_message_per_turn_and_embedding() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ingest.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 16);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", 64));

        let q = make_question(
            "lme_q_0001",
            vec![
                ("sess_1", vec![("user", "hello world"), ("assistant", "hi")]),
                (
                    "sess_2",
                    vec![("user", "tokio runtime"), ("assistant", "yes")],
                ),
            ],
        );
        let count = ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();
        assert_eq!(count, 4, "4 turns ingested");

        // Sanity check: HNSW finds something for a known query.
        let qv = embedder.embed_text("tokio").await.unwrap();
        let hits: Vec<(String, f32)> = index.search(&qv, 4).await.unwrap();
        assert!(!hits.is_empty(), "HNSW should return ingested blocks");
        let ids: HashSet<String> = hits.into_iter().map(|(id, _)| id).collect();
        assert!(ids.iter().all(|id: &String| id.starts_with("lme_q_0001_")));
    }

    #[tokio::test]
    async fn ingest_corpus_handles_missing_started_at() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ingest.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 8);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", 64));

        // Session with started_at = None
        let q = LongMemEvalQuestion {
            question_id: "q1".to_string(),
            question: "q".to_string(),
            haystack_sessions: vec![LongMemEvalSession {
                session_id: "s1".to_string(),
                started_at: None,
                turns: vec![LongMemEvalTurn {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
            }],
            answer_session_ids: vec![],
            question_date: None,
        };
        let count = ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn retrieve_for_rung_returns_session_level_run() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = DuckDbRepository::open(&tmp.path().join("ret.duckdb"))
            .await
            .unwrap();
        repo.set_transcript_job_provider("fake");
        let index = VectorIndex::new_in_memory(64, "fake", "fake", 16);
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(FakeEmbeddingProvider::new("fake", 64));

        let q = make_question(
            "lme_q_test",
            vec![
                ("sess_a", vec![("user", "tokio rust async runtime")]),
                ("sess_b", vec![("user", "duckdb columnar storage")]),
                ("sess_c", vec![("user", "tokio futures")]),
            ],
        );
        ingest_corpus(&repo, &index, &embedder, &q).await.unwrap();

        let rung = RUNGS[0]; // longmemeval_raw
        let run = retrieve_for_rung(&repo, &index, &embedder, "tokio runtime", &rung)
            .await
            .unwrap();
        assert!(!run.is_empty(), "raw rung should return some sessions");
        assert!(
            run.iter()
                .all(|s| ["sess_a", "sess_b", "sess_c"].contains(&s.as_str())),
            "all returned ids should be from the ingested sessions, got {:?}",
            run
        );
        // Session ids are unique (no duplicates from session-level projection).
        let unique: HashSet<&String> = run.iter().collect();
        assert_eq!(unique.len(), run.len());
    }
}
