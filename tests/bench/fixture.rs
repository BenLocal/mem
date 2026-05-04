//! Fixture data structures shared by synthetic + real loaders and consumed
//! by the bench runner.

use std::collections::{HashMap, HashSet};

pub type QueryId = String;
pub type SessionId = String;
pub type JudgmentMap = HashMap<QueryId, HashSet<SessionId>>;

#[derive(Debug, Clone)]
pub struct BlockFixture {
    pub block_id: String,
    pub role: String,       // "user" or "assistant"
    pub block_type: String, // "text" / "thinking" — bench skips tool blocks
    pub content: String,
    pub created_at: String, // sortable timestamp string ("00000000020260503000")
}

#[derive(Debug, Clone)]
pub struct SessionFixture {
    pub session_id: SessionId,
    pub started_at: String,
    pub blocks: Vec<BlockFixture>,
}

#[derive(Debug, Clone)]
pub struct QueryFixture {
    pub query_id: QueryId,
    pub text: String,
    pub anchor_session_id: Option<SessionId>,
    /// Aliases this query is "about". Used by judgment derivation.
    pub anchor_entities: Vec<String>,
    /// Pre-computed (synthetic) judgments. `None` for real fixtures
    /// (judgment.rs will derive via entity registry).
    pub synthetic_judgments: Option<HashSet<SessionId>>,
}

#[derive(Debug, Clone)]
pub struct Fixture {
    pub kind: FixtureKind,
    pub tenant: String,
    pub sessions: Vec<SessionFixture>,
    pub queries: Vec<QueryFixture>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureKind {
    Synthetic { seed: u64 },
    Real,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_can_be_constructed_empty() {
        let f = Fixture {
            kind: FixtureKind::Synthetic { seed: 0 },
            tenant: "t".to_string(),
            sessions: vec![],
            queries: vec![],
        };
        assert_eq!(f.tenant, "t");
        assert_eq!(f.sessions.len(), 0);
    }
}
