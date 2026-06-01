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

/// All fields are part of the fixture's public shape; some are reserved for
/// future use and not read by the current runner.
///
/// `expand_graph`: populated by the synthetic generator but not read by
/// `runner.rs` (which controls `expand_graph` uniformly per rung); reserved
/// for a future per-query graph-toggle override.
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
/// Fields are part of the fixture's public shape; `edges` is populated by the
/// synthetic generator but currently unused in the runner (writing edges to
/// the store is deferred to the v1.1 fixture redesign).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct EdgeFixture {
    pub from_topic: String,
    pub to_topic: String,
    pub strength: f32,
}

/// Fields are part of the fixture's public shape; some (e.g. `edges`) are
/// reserved for the v1.1 fixture redesign and not yet consumed by the runner.
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
