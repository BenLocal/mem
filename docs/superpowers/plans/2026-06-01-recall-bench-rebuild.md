# Recall Ablation Bench (capsule retrieval) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rebuild the deleted recall bench as a deterministic, offline ablation harness over capsule retrieval, baselining the K9 / K10 / ③ ranking changes.

**Architecture:** Pure-additive `tests/bench/` modules driving the *real* public ranker (`retrieve::rank_with_hybrid_and_graph` + `Store::{search_candidates,hybrid_candidates}` + `eval_metrics`) with a deterministic `GeometryProvider` for embeddings. Each ablation rung is an input variation; **zero production code changes**.

**Tech Stack:** Rust 2021, tokio, LanceDB/DuckDB via `Store`, `tiktoken-rs` (already used by chunking), existing `src/pipeline/eval_metrics.rs`.

**Spec:** `docs/superpowers/specs/2026-06-01-recall-bench-rebuild-design.md`

---

## File Structure

- Create `tests/bench/mod.rs` — `pub mod fixture; pub mod geometry; pub mod synthetic; pub mod runner;`
- Create `tests/bench/geometry.rs` — `GeometryProvider` (impl `mem::embedding::EmbeddingProvider`).
- Create `tests/bench/fixture.rs` — `Topic`, `CapsuleFixture`, `QueryFixture`, `Fixture`, `Qrels`.
- Create `tests/bench/synthetic.rs` — `SyntheticConfig`, `generate(&SyntheticConfig) -> Fixture`.
- Create `tests/bench/runner.rs` — `Rung`, `RungReport`, `BenchReport`, `run_bench`, `pretty_table`, `write_json`.
- Create `tests/recall_bench.rs` — `#[ignore]`'d entrypoint: `mod bench;` + `run_bench` + print/JSON.
- Modify `Makefile` — add `bench-recall` target.

Reference files to mirror (read these at execution time, do not re-derive):
- `src/embedding/fake.rs` — `EmbeddingProvider` impl shape for Task 1.
- `tests/hybrid_search.rs:182-210` (`ingest_for_e2e`) + `:120-134` (direct embedding upsert) — ingest + embedding-upsert pattern for Task 3.
- `src/worker/cooccurrence_worker.rs` — `GraphEdge` construction + `add_edge_direct` call for Task 4 (graph rung).
- `src/pipeline/retrieve.rs:72-122` (`rank_with_hybrid_and_graph`) + `:124-135` (`EdgeDynamicsCtx`) — ranker entry for Task 3/4.

---

## Task 1: GeometryProvider (deterministic designed-geometry embeddings)

**Files:**
- Create: `tests/bench/geometry.rs`
- Create (stub): `tests/bench/mod.rs`
- Test: inline `#[cfg(test)] mod tests` in `tests/bench/geometry.rs`, exercised via `tests/recall_bench.rs` (Task 6) — for Task 1 we run the tests through a temporary `tests/bench_geometry_probe.rs` shim, deleted in Task 6. (Simpler: put a temporary `mod bench { pub mod geometry; }` probe in a throwaway test file.)

Design: `d = 16`. Topics map to basis index `0..d`. `embed_text(s)`:
1. Lowercase `s`. For each known topic term present as a substring, accumulate its basis unit vector.
2. If no topic term present, fall back to a hashed pseudo-vector (so unrelated text is far from all topics).
3. Add deterministic jitter: for each dim `i`, `+= jitter(s, i)` where `jitter` is `(hash(s,i) % 1000) as f32 / 1000.0 * 0.01` (magnitude 0.01 « inter-topic gap 1.0).
4. L2-normalize.

The provider holds the topic→index map so content and queries embed consistently.

- [ ] **Step 1: Create `tests/bench/mod.rs` with the geometry module only**

```rust
//! Test-only ablation bench (capsule recall). See
//! docs/superpowers/specs/2026-06-01-recall-bench-rebuild-design.md.
pub mod geometry;
```

- [ ] **Step 2: Write the failing test (same-topic closer than cross-topic; jitter deterministic)**

In `tests/bench/geometry.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
    }

    #[test]
    fn same_topic_closer_than_cross_topic() {
        let topics = ["tokio", "lance", "duckdb"];
        let p = GeometryProvider::new(&topics, 16);
        let q = futures::executor::block_on(p.embed_text("how to use tokio runtime")).unwrap();
        let same = futures::executor::block_on(p.embed_text("tokio async tasks")).unwrap();
        let cross = futures::executor::block_on(p.embed_text("duckdb single mutex")).unwrap();
        assert!(
            cosine(&q, &same) > cosine(&q, &cross) + 0.3,
            "same-topic cosine {} must exceed cross-topic {} by margin",
            cosine(&q, &same),
            cosine(&q, &cross)
        );
    }

    #[test]
    fn deterministic_and_unit_norm() {
        let p = GeometryProvider::new(&["tokio"], 16);
        let a = futures::executor::block_on(p.embed_text("tokio")).unwrap();
        let b = futures::executor::block_on(p.embed_text("tokio")).unwrap();
        assert_eq!(a, b);
        let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "expected unit norm, got {norm}");
        assert_eq!(a.len(), 16);
    }
}
```

Note: `futures` is already a transitive dep; if `futures::executor::block_on` is unavailable, use `tokio::runtime::Runtime::new().unwrap().block_on(...)`.

- [ ] **Step 3: Run the test, verify it fails**

Run: `cargo test --test recall_bench --no-run` is not yet wired; instead temporarily add `mod bench { pub mod geometry; }` to a scratch file. Simplest: defer running until Step 4 of Task 6 wires the entrypoint. **For Task 1 standalone**, create a temporary `tests/_bench_probe.rs` containing `#[path = "bench/geometry.rs"] mod geometry;` and run `cargo test --test _bench_probe -v`.
Expected: FAIL — `GeometryProvider` not defined.

- [ ] **Step 4: Implement `GeometryProvider`**

```rust
//! Deterministic designed-geometry embedding provider for the recall
//! bench. Each topic owns an orthogonal basis vector; a text embeds to
//! the normalized sum of the bases for the topics whose canonical term
//! appears, plus tiny deterministic jitter. Same function embeds content
//! and queries, so same-topic items are nearest neighbours by
//! construction. Mirrors the EmbeddingProvider impl shape of
//! `src/embedding/fake.rs`.
use async_trait::async_trait;
use mem::embedding::{EmbeddingError, EmbeddingProvider};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct GeometryProvider {
    dim: usize,
    topic_index: HashMap<String, usize>,
}

impl GeometryProvider {
    /// `topics` map to basis dims `0..topics.len()`. `dim` must be
    /// >= topics.len() so each topic gets its own axis.
    pub fn new(topics: &[&str], dim: usize) -> Self {
        assert!(dim >= topics.len(), "dim must be >= number of topics");
        let topic_index = topics
            .iter()
            .enumerate()
            .map(|(i, t)| (t.to_lowercase(), i))
            .collect();
        Self { dim, topic_index }
    }

    fn jitter(text: &str, i: usize) -> f32 {
        // Deterministic small per-(text,dim) perturbation, FNV-ish.
        let mut h: u64 = 1469598103934665603;
        for b in text.bytes() {
            h = (h ^ b as u64).wrapping_mul(1099511628211);
        }
        h = (h ^ i as u64).wrapping_mul(1099511628211);
        ((h % 1000) as f32 / 1000.0) * 0.01
    }

    fn raw(&self, text: &str) -> Vec<f32> {
        let lower = text.to_lowercase();
        let mut v = vec![0.0_f32; self.dim];
        let mut hit = false;
        for (term, &idx) in &self.topic_index {
            if lower.contains(term) {
                v[idx] += 1.0;
                hit = true;
            }
        }
        if !hit {
            // Unrelated text: spread weight by content hash so it is far
            // from every topic axis (no single dim dominates).
            for i in 0..self.dim {
                v[i] += Self::jitter(&lower, i + self.dim) * 50.0;
            }
        }
        for i in 0..self.dim {
            v[i] += Self::jitter(&lower, i);
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

#[async_trait]
impl EmbeddingProvider for GeometryProvider {
    fn name(&self) -> &'static str {
        "geometry"
    }
    fn model(&self) -> &str {
        "geometry-bench"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        Ok(self.raw(text))
    }
}
```

- [ ] **Step 5: Run the tests, verify they pass**

Run: `cargo test --test _bench_probe -v`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add tests/bench/mod.rs tests/bench/geometry.rs tests/_bench_probe.rs
git commit -m "test(bench): GeometryProvider — deterministic designed-geometry embeddings (refs recall-bench)"
```

---

## Task 2: Fixture types + synthetic generator

**Files:**
- Create: `tests/bench/fixture.rs`
- Create: `tests/bench/synthetic.rs`
- Modify: `tests/bench/mod.rs` (add `pub mod fixture; pub mod synthetic;`)
- Test: inline tests in `synthetic.rs`, run via `tests/_bench_probe.rs` (extend its `#[path]` mods).

- [ ] **Step 1: Define fixture types in `tests/bench/fixture.rs`**

```rust
//! Capsule-flavored bench fixtures + qrels, consumed by the runner.
use std::collections::{HashMap, HashSet};

pub type QueryId = String;
pub type CapsuleId = String;

#[derive(Debug, Clone)]
pub struct CapsuleFixture {
    pub id: CapsuleId,
    pub content: String,
    pub topics: Vec<String>,
    /// When true this is a long capsule: `content` is head-topic text
    /// followed by >DEFAULT_CHUNK_TOKENS filler then tail-topic text.
    pub long: bool,
    /// For long capsules, the tail topic (differs from topics[0]).
    pub tail_topic: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueryFixture {
    pub id: QueryId,
    pub text: String,
    pub topic: String,
    /// Drives rung 4/5: when true, query carries a graph anchor so
    /// graph expansion can fire.
    pub expand_graph: bool,
    /// True for queries that target a long capsule's *tail* topic
    /// (used by the ③ chunking rung assertion).
    pub tail_targeted: bool,
}

/// (entity_a, entity_b, strength) co-occurrence edge, strengths vary so
/// the K9 dynamics rung reorders vs the flat boost.
#[derive(Debug, Clone)]
pub struct EdgeFixture {
    pub from_topic: String,
    pub to_topic: String,
    pub strength: f32,
}

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
```

- [ ] **Step 2: Write the failing determinism + qrels test in `tests/bench/synthetic.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_for_same_seed() {
        let a = generate(&SyntheticConfig::default());
        let b = generate(&SyntheticConfig::default());
        assert_eq!(a.capsules.len(), b.capsules.len());
        assert_eq!(
            a.capsules.iter().map(|c| c.content.clone()).collect::<Vec<_>>(),
            b.capsules.iter().map(|c| c.content.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn qrels_match_topic_assignment() {
        let f = generate(&SyntheticConfig::default());
        for q in &f.queries {
            let rel = f.qrels.get(&q.id).expect("every query has qrels");
            assert!(!rel.is_empty(), "query {} has no relevant capsules", q.id);
            // Every relevant capsule carries the query topic (or, for a
            // tail-targeted query, has that topic as its tail).
            for cid in rel {
                let cap = f.capsules.iter().find(|c| &c.id == cid).unwrap();
                let carries = cap.topics.contains(&q.topic)
                    || cap.tail_topic.as_deref() == Some(q.topic.as_str());
                assert!(carries, "qrel {cid} for query {} lacks the topic", q.id);
            }
        }
    }

    #[test]
    fn has_long_capsules_and_tail_queries() {
        let f = generate(&SyntheticConfig::default());
        assert!(f.capsules.iter().any(|c| c.long), "need long capsules for ③");
        assert!(f.queries.iter().any(|q| q.tail_targeted), "need tail queries");
        assert!(!f.edges.is_empty(), "need co-occurrence edges for K10/K9");
    }
}
```

- [ ] **Step 3: Run, verify fail**

Run: extend `tests/_bench_probe.rs` with `#[path="bench/fixture.rs"] mod fixture;` + `#[path="bench/synthetic.rs"] mod synthetic;`, then `cargo test --test _bench_probe -v`.
Expected: FAIL — `generate`/`SyntheticConfig` undefined.

- [ ] **Step 4: Implement `synthetic::generate`**

```rust
//! Deterministic synthetic fixture generator. Given (seed, config) it
//! emits topics, per-topic capsules (content seeded with the topic term
//! + filler for BM25), long multi-topic capsules for ③, co-occurrence
//! edges, queries, and qrels by construction.
use crate::bench::fixture::*;
use std::collections::{HashMap, HashSet};

pub struct SyntheticConfig {
    pub seed: u64,
    pub num_topics: usize,
    pub capsules_per_topic: usize,
    pub num_long: usize,
}

impl Default for SyntheticConfig {
    fn default() -> Self {
        Self { seed: 42, num_topics: 6, capsules_per_topic: 4, num_long: 3 }
    }
}

const TERMS: &[&str] = &[
    "tokio", "lance", "duckdb", "embedding", "graph", "transcript",
    "ranking", "chunking", "entity", "session", "vector", "decay",
];

fn ts(n: usize) -> String {
    // Sortable fixed-width timestamp string, like the lance_store tests.
    format!("{:020}", 1778_000_000_000_u64 + n as u64)
}

pub fn generate(cfg: &SyntheticConfig) -> Fixture {
    assert!(cfg.num_topics <= TERMS.len());
    let topics: Vec<String> = TERMS[..cfg.num_topics].iter().map(|s| s.to_string()).collect();
    let mut capsules = Vec::new();
    let mut queries = Vec::new();
    let mut edges = Vec::new();
    let mut qrels: HashMap<QueryId, HashSet<CapsuleId>> = HashMap::new();
    let mut n = 0usize;

    // Per-topic short capsules.
    for (ti, topic) in topics.iter().enumerate() {
        for j in 0..cfg.capsules_per_topic {
            let id = format!("cap_{topic}_{j}");
            capsules.push(CapsuleFixture {
                id: id.clone(),
                content: format!("{topic} note {j}: the {topic} subsystem detail {}", cfg.seed),
                topics: vec![topic.clone()],
                long: false,
                tail_topic: None,
            });
            n += 1;
            let _ = ti;
        }
        // One query per topic.
        let qid = format!("q_{topic}");
        queries.push(QueryFixture {
            id: qid.clone(),
            text: format!("how does {topic} work"),
            topic: topic.clone(),
            expand_graph: false,
            tail_targeted: false,
        });
        let rel: HashSet<CapsuleId> =
            (0..cfg.capsules_per_topic).map(|j| format!("cap_{topic}_{j}")).collect();
        qrels.insert(qid, rel);
    }

    // Long multi-topic capsules: head = topics[i], tail = topics[i+1].
    // ~2000+ filler tokens between so head-window embedding drops the tail.
    let filler = "lorem ipsum dolor sit amet ".repeat(500);
    for i in 0..cfg.num_long {
        let head = topics[i % topics.len()].clone();
        let tail = topics[(i + 1) % topics.len()].clone();
        let id = format!("cap_long_{i}");
        capsules.push(CapsuleFixture {
            id: id.clone(),
            content: format!("{head} overview. {filler} finally the {tail} appendix."),
            topics: vec![head.clone()],
            long: true,
            tail_topic: Some(tail.clone()),
        });
        n += 1;
        // Tail-targeted query: relevant capsule is the long one (recalled
        // only when chunking is on).
        let qid = format!("q_tail_{i}");
        queries.push(QueryFixture {
            id: qid.clone(),
            text: format!("details about {tail}"),
            topic: tail.clone(),
            expand_graph: false,
            tail_targeted: true,
        });
        let mut rel = qrels.remove(&format!("q_{tail}")).unwrap_or_default();
        // The tail query's own qrel set is just the long capsule.
        let mut tail_rel = HashSet::new();
        tail_rel.insert(id.clone());
        qrels.insert(qid, tail_rel);
        qrels.insert(format!("q_{tail}"), rel.drain().collect());
    }

    // Co-occurrence edges among the first few topics, strengths varying.
    for i in 0..topics.len().saturating_sub(1) {
        edges.push(EdgeFixture {
            from_topic: topics[i].clone(),
            to_topic: topics[i + 1].clone(),
            strength: 0.2 + 0.1 * (i as f32),
        });
    }
    // A graph-anchored query: anchor topic[0], relevant = topic[1] capsules
    // reachable via the edge.
    let g_topic = topics[1].clone();
    let qid = "q_graph".to_string();
    queries.push(QueryFixture {
        id: qid.clone(),
        text: format!("how does {} work", topics[0]),
        topic: g_topic.clone(),
        expand_graph: true,
        tail_targeted: false,
    });
    let rel: HashSet<CapsuleId> =
        (0..cfg.capsules_per_topic).map(|j| format!("cap_{g_topic}_{j}")).collect();
    qrels.insert(qid, rel);

    let _ = n;
    Fixture { tenant: "bench".into(), capsules, queries, edges, qrels, topics }
}
```

Note: the `qrels` juggling for tail queries above is intentionally explicit; verify the `qrels_match_topic_assignment` test passes and tidy if the borrow checker complains (the `rel.drain().collect()` reinsert keeps `q_{tail}` pointing at its short capsules while `q_tail_i` points at the long one).

- [ ] **Step 5: Run, verify pass**

Run: `cargo test --test _bench_probe -v`
Expected: PASS (geometry 2 + synthetic 3).

- [ ] **Step 6: Commit**

```bash
git add tests/bench/mod.rs tests/bench/fixture.rs tests/bench/synthetic.rs tests/_bench_probe.rs
git commit -m "test(bench): fixture types + deterministic synthetic generator (refs recall-bench)"
```

---

## Task 3: Runner — ingest a fixture + run the hybrid baseline rung

**Files:**
- Create: `tests/bench/runner.rs`
- Modify: `tests/bench/mod.rs` (`pub mod runner;`)
- Test: inline in `runner.rs`, run via `_bench_probe`.

This task builds: (a) `ingest_fixture(store, fixture, provider)` — insert capsules via `CapabilityCapsuleService`, then set each capsule's geometry embedding directly (chunked for long, single head-window vector is added in Task 4's chunking rung); write graph edges via `add_edge_direct`; (b) `run_rung` for the **hybrid** rung only; (c) `Rung`/`RungReport`/`BenchReport` types.

- [ ] **Step 1: Define runner types + the hybrid rung; write failing test**

```rust
//! Ablation runner: ingest a Fixture into a fresh Store, run each Rung
//! through the real public ranker, aggregate metrics.
use crate::bench::fixture::*;
use crate::bench::geometry::GeometryProvider;
use mem::domain::query::SearchCapabilityCapsuleRequest;
use mem::pipeline::eval_metrics::{mrr, ndcg_at_k, precision_at_k, recall_at_k};
use mem::pipeline::retrieve;
use mem::storage::Store;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rung {
    LexicalOnly,
    SemanticOnly,
    Hybrid,
    Graph,    // K10
    Dynamics, // K9
    ChunkingOn,
    ChunkingOff,
    Oracle,
}

#[derive(Debug, Clone)]
pub struct RungReport {
    pub rung: Rung,
    pub ndcg_at_10: f64,
    pub mrr: f64,
    pub recall_at_10: f64,
    pub precision_at_10: f64,
}

#[derive(Debug, Clone)]
pub struct BenchReport {
    pub reports: Vec<RungReport>,
}
```

Failing test (full pipeline through public ranker, hybrid rung finds the topic capsules):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::synthetic::{generate, SyntheticConfig};

    #[tokio::test(flavor = "multi_thread")]
    async fn hybrid_rung_recalls_topic_capsules() {
        let f = generate(&SyntheticConfig { num_long: 0, ..SyntheticConfig::default() });
        let report = run_bench(&f, &[Rung::Hybrid]).await;
        let r = report.reports.iter().find(|r| r.rung == Rung::Hybrid).unwrap();
        assert!(r.recall_at_10 > 0.5, "hybrid recall@10 too low: {}", r.recall_at_10);
    }
}
```

- [ ] **Step 2: Run, verify fail**

Run: extend `_bench_probe.rs` with `#[path="bench/runner.rs"] mod runner;` and the `crate::bench::*` paths — NOTE the modules reference `crate::bench::...`; for the probe shim, instead make `_bench_probe.rs` declare `mod bench { pub mod geometry; ... }` so the `crate::bench::` paths resolve. Run `cargo test --test _bench_probe runner -v`.
Expected: FAIL — `run_bench` undefined.

- [ ] **Step 3: Implement `ingest_fixture` + `run_bench` (hybrid rung)**

Mirror `tests/hybrid_search.rs:182-210` for ingest and `:120-132` for the direct embedding upsert. Concretely:

```rust
async fn ingest_fixture(store: &Arc<Store>, f: &Fixture, provider: &GeometryProvider) {
    use mem::domain::capability_capsule::{
        CapabilityCapsuleType, IngestCapabilityCapsuleRequest, Scope, Visibility, WriteMode,
    };
    use mem::service::CapabilityCapsuleService;
    use mem::embedding::EmbeddingProvider;

    let svc = CapabilityCapsuleService::with_providers(
        store.clone(),
        "geometry".into(),
        Some(Arc::new(provider.clone())),
    );
    for cap in &f.capsules {
        // Ingest under a stable idempotency key so the id is predictable;
        // capture the stored record to read content_hash + updated_at.
        let resp = svc
            .ingest(IngestCapabilityCapsuleRequest {
                tenant: f.tenant.clone(),
                capability_capsule_type: CapabilityCapsuleType::Implementation,
                content: cap.content.clone(),
                summary: None,
                evidence: vec![],
                code_refs: vec![],
                scope: Scope::Global,
                visibility: Visibility::Shared,
                project: None,
                repo: None,
                module: None,
                task_type: None,
                tags: vec![],
                topics: cap.topics.clone(),
                source_agent: "bench".into(),
                idempotency_key: Some(cap.id.clone()),
                write_mode: WriteMode::Auto,
                supersedes_capability_capsule_id: None,
            })
            .await
            .expect("ingest");
        let stored = store
            .fetch_capability_capsules_by_ids(&f.tenant, &[resp.capability_capsule_id.clone()])
            .await
            .unwrap()
            .pop()
            .unwrap();
        // Designed-geometry embedding(s). Short → single vector. Long is
        // handled in Task 4 (chunked / head-window). Here: single vector.
        let vec = provider.embed_text(&cap.content).await.unwrap();
        store
            .upsert_capability_capsule_embedding_chunks(
                &stored.capability_capsule_id,
                &f.tenant,
                "geometry-bench",
                provider.dim() as i64,
                std::slice::from_ref(&vec),
                &stored.content_hash,
                &stored.updated_at,
                &stored.updated_at,
            )
            .await
            .unwrap();
        // Remember the fixture id -> stored id mapping if they differ;
        // with idempotency_key the stored id may be a generated UUID, so
        // qrels must be translated. SIMPLER: assert ingest preserves a
        // recoverable id, or build an id map. See Step 3a.
    }
    // Graph edges (Task 4 uses them; harmless to write now).
    // ... see Task 4 for GraphEdge construction.
    let _ = f.edges;
}

pub async fn run_bench(f: &Fixture, rungs: &[Rung]) -> BenchReport {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("store")).await.unwrap());
    let provider = GeometryProvider::new(
        &f.topics.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        16,
    );
    ingest_fixture(&store, f, &provider).await;

    let mut reports = Vec::new();
    for &rung in rungs {
        reports.push(run_rung(&store, f, &provider, rung).await);
    }
    BenchReport { reports }
}
```

- [ ] **Step 3a: Resolve the id-mapping concern**

`svc.ingest` returns `capability_capsule_id`. Build `id_map: HashMap<fixture_id, stored_id>` during ingest and translate `f.qrels` keys/values through it before scoring. Add to `ingest_fixture` return: `-> HashMap<CapsuleId, CapsuleId>`. Update `run_bench` to thread the map into `run_rung`. (Read the actual `IngestCapabilityCapsuleResponse` shape in `src/service/` to confirm the field name; mirror `tests/hybrid_search.rs` usage of the response.)

- [ ] **Step 3b: Implement `run_rung` for `Rung::Hybrid`**

```rust
async fn run_rung(
    store: &Arc<Store>,
    f: &Fixture,
    provider: &GeometryProvider,
    rung: Rung,
    id_map: &std::collections::HashMap<CapsuleId, CapsuleId>,
) -> RungReport {
    use mem::embedding::EmbeddingProvider;
    let mut n5 = 0.0; // unused placeholders removed in real impl
    let (mut s_ndcg, mut s_mrr, mut s_rec, mut s_prec, mut count) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for q in &f.queries {
        let (text, vec): (String, Vec<f32>) = match rung {
            Rung::LexicalOnly => (q.text.clone(), vec![]),
            Rung::SemanticOnly => (String::new(), provider.embed_text(&q.text).await.unwrap()),
            _ => (q.text.clone(), provider.embed_text(&q.text).await.unwrap()),
        };
        let pool = store.search_candidates(&f.tenant).await.unwrap();
        let hits = store.hybrid_candidates(&f.tenant, &text, &vec, 48).await.unwrap();
        let query = SearchCapabilityCapsuleRequest {
            query: q.text.clone(),
            intent: "debugging".into(),
            scope_filters: vec![],
            token_budget: 4000,
            caller_agent: "bench".into(),
            expand_graph: matches!(rung, Rung::Graph | Rung::Dynamics),
            tenant: Some(f.tenant.clone()),
        };
        let ranked = retrieve::rank_with_hybrid_and_graph(
            pool, hits, &query, store.as_ref(), None,
        )
        .await
        .unwrap();
        let run: Vec<String> = ranked.iter().map(|r| r.capability_capsule_id.clone()).collect();
        // Translate qrels fixture ids -> stored ids.
        let qrels: HashSet<String> = f.qrels[&q.id]
            .iter()
            .map(|fid| id_map.get(fid).cloned().unwrap_or_else(|| fid.clone()))
            .collect();
        s_ndcg += ndcg_at_k(&run, &qrels, 10);
        s_mrr += mrr(&run, &qrels);
        s_rec += recall_at_k(&run, &qrels, 10);
        s_prec += precision_at_k(&run, &qrels, 10);
        count += 1.0;
        let _ = n5;
    }
    RungReport {
        rung,
        ndcg_at_10: s_ndcg / count,
        mrr: s_mrr / count,
        recall_at_10: s_rec / count,
        precision_at_10: s_prec / count,
    }
}
```

Note: `store.as_ref()` must coerce to `&dyn GraphStore`; `Store` impls `GraphStore`. If the coercion needs to be explicit, use `store.as_ref() as &dyn mem::storage::GraphStore` (confirm `GraphStore` is exported from `mem::storage`).

- [ ] **Step 4: Run, verify pass**

Run: `cargo test --test _bench_probe runner -v`
Expected: PASS — `hybrid_rung_recalls_topic_capsules`.

- [ ] **Step 5: Commit**

```bash
git add tests/bench/runner.rs tests/bench/mod.rs tests/_bench_probe.rs
git commit -m "test(bench): runner — ingest fixture + hybrid baseline rung via public ranker (refs recall-bench)"
```

---

## Task 4: Full rung ladder (lexical / semantic / graph K10 / dynamics K9 / chunking ③ / oracle)

**Files:**
- Modify: `tests/bench/runner.rs`
- Test: inline in `runner.rs`.

- [ ] **Step 1: Write failing tests for the discriminating rungs**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn chunking_on_beats_off_for_tail_queries() {
    let f = generate(&SyntheticConfig::default());
    let rep = run_bench(&f, &[Rung::ChunkingOn, Rung::ChunkingOff]).await;
    let on = rep.reports.iter().find(|r| r.rung == Rung::ChunkingOn).unwrap();
    let off = rep.reports.iter().find(|r| r.rung == Rung::ChunkingOff).unwrap();
    assert!(
        on.recall_at_10 > off.recall_at_10,
        "③ chunking-on recall {} must beat off {}",
        on.recall_at_10, off.recall_at_10
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn oracle_is_an_upper_bound() {
    let f = generate(&SyntheticConfig::default());
    let rep = run_bench(&f, &[Rung::Hybrid, Rung::Oracle]).await;
    let hybrid = rep.reports.iter().find(|r| r.rung == Rung::Hybrid).unwrap();
    let oracle = rep.reports.iter().find(|r| r.rung == Rung::Oracle).unwrap();
    assert!(oracle.ndcg_at_10 >= hybrid.ndcg_at_10 - 1e-9, "oracle must dominate");
}
```

- [ ] **Step 2: Run, verify fail**

Run: `cargo test --test _bench_probe runner -v`
Expected: FAIL — `ChunkingOff` path not implemented (currently treated as hybrid); oracle not implemented.

- [ ] **Step 3: Implement remaining rungs**

In `ingest_fixture`, branch on `cap.long` and the rung. Since chunking differs per-rung, parameterize ingest by a `chunking: bool` for the long capsules:
- **ChunkingOn / default**: embed the long capsule with the real chunked path — split `cap.content` via `mem::pipeline::chunk::chunk_text(content, DEFAULT_CHUNK_TOKENS, DEFAULT_CHUNK_OVERLAP)`, geometry-embed each chunk, upsert all via `upsert_capability_capsule_embedding_chunks`.
- **ChunkingOff**: embed only the **head window** — `chunk_text(...)[0]` — as a single vector (models the embedder dropping the tail).

Because ingest state differs between ChunkingOn and ChunkingOff, `run_bench` must build a **fresh store per chunking rung**. Refactor: `run_rung` for chunking rungs calls an inner `ingest_fixture(store, f, provider, chunking_on)`; non-chunking rungs share one store ingested with `chunking_on = true`.

For `Rung::Graph` (K10): write edges (`add_edge_direct`) and set `expand_graph = true` (already wired). Mirror `src/worker/cooccurrence_worker.rs` for `GraphEdge` construction — `from_node_id` / `to_node_id` use `entity:<uuid>` or topic-derived node ids; for the bench, resolve topics to entity node ids the same way ingest does, OR write edges between the capsule ids' topic entities. **Read `cooccurrence_worker.rs` to copy the exact node-id scheme + GraphEdge fields (including the K9 `strength`/`stability`/`last_activated` Options).**

For `Rung::Dynamics` (K9): construct `EdgeDynamicsCtx { now: mem::storage::current_timestamp(), tx }` where `tx` is from `tokio::sync::mpsc::unbounded_channel()` (drop the receiver or drain it); pass `Some(&ctx)` to `rank_with_hybrid_and_graph`. Edges must carry varying `strength` (from `EdgeFixture.strength`) so decayed-strength weighting reorders vs flat.

For `Rung::Oracle`: build `run` by sorting the candidate union so qrels-relevant ids come first (then the rest), then score — yields the achievable ceiling given the candidate set.

```rust
// Oracle run construction (inside run_rung, rung == Oracle):
let pool = store.search_candidates(&f.tenant).await.unwrap();
let vec = provider.embed_text(&q.text).await.unwrap();
let hits = store.hybrid_candidates(&f.tenant, &q.text, &vec, 48).await.unwrap();
let mut ids: Vec<String> = hits.iter().map(|(m, _)| m.capability_capsule_id.clone()).collect();
for m in &pool { if !ids.contains(&m.capability_capsule_id) { ids.push(m.capability_capsule_id.clone()); } }
let qrels: HashSet<String> = /* translated as before */;
ids.sort_by_key(|id| if qrels.contains(id) { 0 } else { 1 });
let run = ids;
```

- [ ] **Step 4: Run, verify pass**

Run: `cargo test --test _bench_probe runner -v`
Expected: PASS — `chunking_on_beats_off_for_tail_queries`, `oracle_is_an_upper_bound`, plus Task 3's test.

- [ ] **Step 5: Commit**

```bash
git add tests/bench/runner.rs
git commit -m "test(bench): full rung ladder — lexical/semantic/graph(K10)/dynamics(K9)/chunking(③)/oracle (refs recall-bench)"
```

---

## Task 5: Output — pretty_table + write_json

**Files:**
- Modify: `tests/bench/runner.rs`
- Test: inline.

- [ ] **Step 1: Failing test**

```rust
#[test]
fn pretty_table_has_all_rungs_and_delta() {
    let report = BenchReport { reports: vec![
        RungReport { rung: Rung::Hybrid, ndcg_at_10: 0.5, mrr: 0.5, recall_at_10: 0.5, precision_at_10: 0.5 },
        RungReport { rung: Rung::Graph, ndcg_at_10: 0.6, mrr: 0.5, recall_at_10: 0.6, precision_at_10: 0.5 },
    ]};
    let t = pretty_table(&report);
    assert!(t.contains("Hybrid"));
    assert!(t.contains("Graph"));
    assert!(t.contains("+0.100"), "expected Δndcg vs hybrid baseline in {t}");
}

#[test]
fn write_json_is_wellformed() {
    let report = BenchReport { reports: vec![
        RungReport { rung: Rung::Hybrid, ndcg_at_10: 0.5, mrr: 0.5, recall_at_10: 0.5, precision_at_10: 0.5 },
    ]};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.json");
    write_json(&report, &path).unwrap();
    let s = std::fs::read_to_string(&path).unwrap();
    assert!(s.contains("\"ndcg_at_10\""));
    assert!(s.contains("\"Hybrid\""));
}
```

- [ ] **Step 2: Run, verify fail** — `cargo test --test _bench_probe runner -v` → FAIL (`pretty_table`/`write_json` undefined).

- [ ] **Step 3: Implement** (hand-rolled JSON to avoid a serde derive on test types):

```rust
fn rung_name(r: Rung) -> &'static str {
    match r {
        Rung::LexicalOnly => "LexicalOnly", Rung::SemanticOnly => "SemanticOnly",
        Rung::Hybrid => "Hybrid", Rung::Graph => "Graph", Rung::Dynamics => "Dynamics",
        Rung::ChunkingOn => "ChunkingOn", Rung::ChunkingOff => "ChunkingOff", Rung::Oracle => "Oracle",
    }
}

pub fn pretty_table(report: &BenchReport) -> String {
    let baseline = report.reports.iter().find(|r| r.rung == Rung::Hybrid).map(|r| r.ndcg_at_10);
    let mut out = String::from("rung          ndcg@10  mrr    recall@10  prec@10   Δndcg\n");
    for r in &report.reports {
        let delta = match baseline {
            Some(b) => format!("{:+.3}", r.ndcg_at_10 - b),
            None => String::new(),
        };
        out.push_str(&format!(
            "{:<13} {:.3}    {:.3}  {:.3}      {:.3}     {}\n",
            rung_name(r.rung), r.ndcg_at_10, r.mrr, r.recall_at_10, r.precision_at_10, delta
        ));
    }
    out
}

pub fn write_json(report: &BenchReport, path: &std::path::Path) -> std::io::Result<()> {
    let mut s = String::from("{\"rungs\":[");
    for (i, r) in report.reports.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push_str(&format!(
            "{{\"rung\":\"{}\",\"ndcg_at_10\":{},\"mrr\":{},\"recall_at_10\":{},\"precision_at_10\":{}}}",
            rung_name(r.rung), r.ndcg_at_10, r.mrr, r.recall_at_10, r.precision_at_10
        ));
    }
    s.push_str("]}");
    std::fs::write(path, s)
}
```

- [ ] **Step 4: Run, verify pass** — `cargo test --test _bench_probe runner -v` → PASS.

- [ ] **Step 5: Commit**

```bash
git add tests/bench/runner.rs
git commit -m "test(bench): pretty_table + write_json reporting (refs recall-bench)"
```

---

## Task 6: Entrypoint + wire-up + run the bench

**Files:**
- Create: `tests/recall_bench.rs`
- Delete: `tests/_bench_probe.rs`
- Modify: `Makefile`

- [ ] **Step 1: Create `tests/recall_bench.rs`**

```rust
//! Recall ablation bench (capsule retrieval). Rebuilds the bench deleted
//! in 4df527b, scoped to capsule recall. Run with:
//!   cargo test --test recall_bench -- --ignored --nocapture
//! Spec: docs/superpowers/specs/2026-06-01-recall-bench-rebuild-design.md
mod bench;

use bench::runner::{pretty_table, run_bench, write_json, Rung};
use bench::synthetic::{generate, SyntheticConfig};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "ablation bench — run with --ignored"]
async fn recall_ablation() {
    let f = generate(&SyntheticConfig::default());
    let rungs = [
        Rung::LexicalOnly, Rung::SemanticOnly, Rung::Hybrid,
        Rung::Graph, Rung::Dynamics, Rung::ChunkingOn, Rung::ChunkingOff, Rung::Oracle,
    ];
    let report = run_bench(&f, &rungs).await;
    println!("\n{}", pretty_table(&report));
    let dir = std::path::Path::new("target/recall_bench");
    std::fs::create_dir_all(dir).unwrap();
    write_json(&report, &dir.join(format!("{}.json", f.tenant))).unwrap();
}
```

- [ ] **Step 2: Confirm `tests/bench/mod.rs` declares all four modules**

```rust
//! Test-only ablation bench (capsule recall).
pub mod fixture;
pub mod geometry;
pub mod runner;
pub mod synthetic;
```

- [ ] **Step 3: Delete the probe shim**

```bash
git rm tests/_bench_probe.rs
```

- [ ] **Step 4: Move the inline `#[cfg(test)]` tests so they run under `recall_bench`**

The geometry/synthetic/runner inline tests now compile under `tests/recall_bench.rs` (because `mod bench;` pulls them in). Verify with the default run (the non-`#[ignore]` inline unit tests run; the `#[ignore]`'d `recall_ablation` does not):

Run: `cargo test --test recall_bench -v`
Expected: PASS — all inline geometry/synthetic/runner tests; `recall_ablation` shows as `ignored`.

- [ ] **Step 5: Run the full bench, eyeball the table**

Run: `cargo test --test recall_bench -- --ignored --nocapture`
Expected: PASS; prints the 8-rung table; `target/recall_bench/bench.json` written. Sanity: Oracle ndcg@10 highest; Graph ≥ Hybrid; ChunkingOn recall@10 > ChunkingOff.

- [ ] **Step 6: Add Makefile target**

Append to `Makefile`:

```makefile
.PHONY: bench-recall
bench-recall: ## Run the capsule recall ablation bench
	cargo test --test recall_bench -- --ignored --nocapture
```

- [ ] **Step 7: Gate checks + commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
git add tests/recall_bench.rs tests/bench/mod.rs Makefile
git commit -m "test(bench): recall ablation entrypoint + Makefile target; rebuild complete (refs recall-bench)"
```

---

## Self-Review

**Spec coverage:** §2 public-API/zero-prod-change → Tasks 3–4 use only `pub` fns ✓. §3.1 GeometryProvider → Task 1 ✓. §3.2 fixtures/qrels → Task 2 ✓. §3.3 ingest → Task 3 ✓. §4 7-rung ladder → Tasks 3–4 ✓ (8 `Rung` variants: lexical/semantic/hybrid/graph/dynamics/chunking-on/chunking-off/oracle — the spec's "chunking on/off" is two variants). §5 metrics+output → Task 5 ✓. §6 `#[ignore]` entrypoint + Makefile → Task 6 ✓. §7 tests (geometry/synthetic/runner incl. ③ + oracle assertions) → Tasks 1,2,4 ✓.

**Open implementation risks to resolve during execution (flagged, not placeholders):**
1. **id mapping** (Task 3a): `svc.ingest` may return a generated UUID id, not `idempotency_key`. The plan threads an `id_map` and translates qrels — confirm the response field name against `src/service/` and whether `idempotency_key` round-trips as the id.
2. **GraphStore coercion** (Task 3b): confirm `mem::storage::GraphStore` is exported and `Store: GraphStore`; adjust the `store.as_ref()` coercion if needed.
3. **GraphEdge node-id scheme** (Task 4): copy exactly from `src/worker/cooccurrence_worker.rs` (entity node ids + the K9 dynamics columns). The graph rung only produces a measurable Δ if the query's anchor resolves to an entity that has edges to the relevant capsules' entities — verify the anchor-derivation in `retrieve::graph_anchor_nodes` matches the bench's entity ids.
4. **`futures::executor::block_on`** (Task 1): if unavailable, use a `tokio::runtime::Runtime`. Or make the geometry tests `#[tokio::test]` and `.await` directly.

These are verification steps, not unknowns in the design — each has a named source file to mirror.

**Type consistency:** `Rung`, `RungReport`, `BenchReport`, `run_bench(&Fixture, &[Rung])`, `pretty_table(&BenchReport)`, `write_json(&BenchReport, &Path)` used consistently across Tasks 3–6. `Fixture`/`CapsuleFixture`/`QueryFixture`/`EdgeFixture`/`qrels` consistent across Tasks 2–4.
