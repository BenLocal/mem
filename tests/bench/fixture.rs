//! Capsule-flavored bench fixtures + qrels, consumed by the runner.
use std::collections::{HashMap, HashSet};

pub type QueryId = String;
pub type CapsuleId = String;

#[derive(Debug, Clone)]
pub struct CapsuleFixture {
    pub id: CapsuleId,
    pub content: String,
    pub topics: Vec<String>,
    /// True => long capsule: head-topic text + >DEFAULT_CHUNK_TOKENS filler + tail-topic text.
    pub long: bool,
    /// For long capsules, the tail topic (differs from topics[0]).
    pub tail_topic: Option<String>,
}

/// All fields are part of the runner API consumed by Tasks 3+; allow dead_code
/// for the ones not yet used in this task's tests.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct QueryFixture {
    pub id: QueryId,
    pub text: String,
    pub topic: String,
    pub expand_graph: bool,
    pub tail_targeted: bool,
}

/// (from_topic, to_topic, strength) co-occurrence edge; strengths vary across
/// edges so the K9 dynamics rung reorders vs the flat boost.
/// Fields consumed by the runner in Tasks 3+; allow dead_code for now.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct EdgeFixture {
    pub from_topic: String,
    pub to_topic: String,
    pub strength: f32,
}

/// Fields consumed by the runner in Tasks 3+; allow dead_code for now.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Fixture {
    pub tenant: String,
    pub capsules: Vec<CapsuleFixture>,
    pub queries: Vec<QueryFixture>,
    pub edges: Vec<EdgeFixture>,
    /// query_id -> relevant capsule ids, derived by construction.
    pub qrels: HashMap<QueryId, HashSet<CapsuleId>>,
    /// Canonical topic terms (for GeometryProvider::new).
    pub topics: Vec<String>,
}
