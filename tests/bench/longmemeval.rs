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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

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
}
