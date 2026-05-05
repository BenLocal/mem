//! LongMemEval dataset deserialize + env-var driven loader.
// Fields are read by future bench tasks; suppress premature dead_code noise.
#![allow(dead_code)]
//!
//! Schema notes (Task 1 probe — update if probe revealed differences):
//! - Top-level: array of question entries
//! - Per-question keys: question_id, question, haystack_sessions,
//!   answer_session_ids, question_date
//! - Per-session keys: session_id, started_at, turns
//! - Per-turn keys: role, content
//!
//! If the on-disk schema differs (e.g., snake_case -> camelCase, or
//! `id` instead of `question_id`), update the deserialize types and
//! the schema notes in this docstring atomically.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalQuestion {
    /// LongMemEval doesn't always include question_id in the JSON;
    /// loader fills with `format!("lme_q_{:04}", index)` if absent.
    #[serde(default)]
    pub question_id: String,
    pub question: String,
    pub haystack_sessions: Vec<LongMemEvalSession>,
    pub answer_session_ids: Vec<String>,
    #[serde(default)]
    pub question_date: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalSession {
    pub session_id: String,
    #[serde(default)]
    pub started_at: Option<String>,
    pub turns: Vec<LongMemEvalTurn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LongMemEvalTurn {
    pub role: String, // "user" | "assistant" | possibly "system"
    pub content: String,
}

/// Load LongMemEval questions if `MEM_LONGMEMEVAL_PATH` is set.
/// Returns `None` when env var is unset (callers skip silently).
/// Panics with a clear message if the file is missing or invalid.
pub fn load_from_env_or_skip() -> Option<Vec<LongMemEvalQuestion>> {
    let path = std::env::var("MEM_LONGMEMEVAL_PATH").ok()?;
    Some(load_from_path(Path::new(&path)).expect("load LongMemEval"))
}

pub fn load_from_path(path: &Path) -> Result<Vec<LongMemEvalQuestion>, std::io::Error> {
    let bytes = std::fs::read(path)?;
    let mut questions: Vec<LongMemEvalQuestion> =
        serde_json::from_slice(&bytes).expect("invalid LongMemEval JSON");
    // Backfill question_id where absent.
    for (i, q) in questions.iter_mut().enumerate() {
        if q.question_id.is_empty() {
            q.question_id = format!("lme_q_{:04}", i);
        }
    }
    Ok(questions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(json: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f
    }

    #[test]
    fn load_from_env_or_skip_returns_none_when_unset() {
        let original = std::env::var("MEM_LONGMEMEVAL_PATH").ok();
        std::env::remove_var("MEM_LONGMEMEVAL_PATH");
        let res = load_from_env_or_skip();
        assert!(res.is_none());
        if let Some(v) = original {
            std::env::set_var("MEM_LONGMEMEVAL_PATH", v);
        }
    }

    #[test]
    fn load_from_path_parses_minimal_valid_file() {
        let json = r#"[
            {
                "question_id": "lme_q_0001",
                "question": "favourite hike?",
                "haystack_sessions": [
                    {
                        "session_id": "sess_1",
                        "started_at": "2024-03-15T00:00:00",
                        "turns": [
                            {"role": "user", "content": "I love Yosemite trails"},
                            {"role": "assistant", "content": "great!"}
                        ]
                    }
                ],
                "answer_session_ids": ["sess_1"]
            }
        ]"#;
        let f = write_fixture(json);
        let qs = load_from_path(f.path()).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question_id, "lme_q_0001");
        assert_eq!(qs[0].haystack_sessions.len(), 1);
        assert_eq!(qs[0].haystack_sessions[0].turns.len(), 2);
        assert_eq!(qs[0].answer_session_ids, vec!["sess_1"]);
    }

    #[test]
    fn missing_question_id_gets_fallback() {
        let json = r#"[
            {
                "question": "q without id",
                "haystack_sessions": [],
                "answer_session_ids": []
            },
            {
                "question": "another q without id",
                "haystack_sessions": [],
                "answer_session_ids": []
            }
        ]"#;
        let f = write_fixture(json);
        let qs = load_from_path(f.path()).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].question_id, "lme_q_0000");
        assert_eq!(qs[1].question_id, "lme_q_0001");
    }

    #[test]
    fn missing_started_at_is_none() {
        let json = r#"[
            {
                "question_id": "q1",
                "question": "q",
                "haystack_sessions": [
                    {"session_id": "s1", "turns": [{"role": "user", "content": "hi"}]}
                ],
                "answer_session_ids": []
            }
        ]"#;
        let f = write_fixture(json);
        let qs = load_from_path(f.path()).unwrap();
        assert!(qs[0].haystack_sessions[0].started_at.is_none());
    }
}
