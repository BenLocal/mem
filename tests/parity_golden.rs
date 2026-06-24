//! Parity golden scaffold for the "remove DuckDB, keep Lance" plan
//! (`docs/remove-duckdb-keep-lance.md` §5 Phase 0 / §7).
//!
//! Captures the CURRENT DuckDB read engine's output, on a fixed
//! deterministic fixture, across every capability bucket the lance-native
//! read engine will have to replace:
//!
//!   filter · ann · fts · hybrid (fused + compose) · stats · taxonomy ·
//!   graph · transcript (fts + ann) · version-chain.
//!
//! These goldens are the baseline for the per-bucket parity diff in
//! Phase 1: once a bucket is reimplemented on lancedb-native + Tantivy,
//! its output is diffed against the same golden (see §7).
//!
//! REPEATABLE / REFRESHABLE:
//!   - `cargo test --test parity_golden`                 → verifies current
//!     DuckDB output still matches the committed `tests/golden/*.json`
//!     (a determinism guard on the fixture + queries + serialization).
//!   - `REFRESH_GOLDEN=1 cargo test --test parity_golden` → regenerates the
//!     golden files from the current engine.
//!
//! Determinism rules: fixed ids / timestamps / content-hashes; deterministic
//! embeddings (`deterministic_embedding`); floats captured as rounded
//! integers; non-semantic orderings sorted, semantic (ranking) orderings
//! preserved (the DuckDB queries break ties by id, so ranking order is
//! itself deterministic).
//!
//! NON-DESTRUCTIVE: this only READS through the existing engine + seeds a
//! throwaway tempdir store. It does not touch any production read path.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, GraphEdge, Scope,
    Visibility,
};
use mem::domain::{BlockType, ConversationMessage, MessageRole};
use mem::embedding::deterministic_embedding;
use mem::storage::{MaintenanceStore, Store};
use serde_json::json;
use tempfile::tempdir;

const DIM: usize = 64;

fn f32_to_blob(values: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_ne_bytes());
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn cap(
    id: &str,
    tenant: &str,
    ctype: CapabilityCapsuleType,
    status: CapabilityCapsuleStatus,
    scope: Scope,
    content: &str,
    topics: &[&str],
    version: i64,
    supersedes: Option<&str>,
    stamp: &str,
) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: tenant.into(),
        capability_capsule_type: ctype,
        status,
        scope,
        visibility: Visibility::Shared,
        version,
        summary: format!("sum-{id}"),
        content: content.into(),
        evidence: vec![],
        code_refs: vec![],
        project: Some("mem".into()),
        repo: Some("mem".into()),
        module: None,
        task_type: None,
        tags: vec![],
        topics: topics.iter().map(|t| t.to_string()).collect(),
        confidence: 0.8,
        decay_score: 0.0,
        content_hash: format!("hash-{id}"),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: supersedes.map(String::from),
        source_agent: "test".into(),
        created_at: stamp.into(),
        updated_at: stamp.into(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    }
}

fn tmsg(id: &str, tenant: &str, line: u64, content: &str, stamp: &str) -> ConversationMessage {
    ConversationMessage {
        message_block_id: id.into(),
        session_id: Some("S1".into()),
        tenant: tenant.into(),
        caller_agent: "claude-code".into(),
        transcript_path: "/tmp/parity.jsonl".into(),
        line_number: line,
        block_index: 0,
        message_uuid: None,
        role: MessageRole::Assistant,
        block_type: BlockType::Text,
        content: content.into(),
        tool_name: None,
        tool_use_id: None,
        embed_eligible: true,
        created_at: stamp.into(),
        meta_json: None,
    }
}

fn edge(from: &str, to: &str, rel: &str, valid_from: &str) -> GraphEdge {
    GraphEdge {
        from_node_id: from.into(),
        to_node_id: to.into(),
        relation: rel.into(),
        valid_from: valid_from.into(),
        valid_to: None,
        confidence: None,
        extractor: None,
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    }
}

/// Like [`edge`] but pre-closed at `valid_to` — exercises the
/// closed/historical path for `kg_timeline` (which surfaces closed
/// edges) vs `neighbors` / `list_user_tunnels` / `find_tunnels` /
/// `follow_tunnels` (active-only).
fn closed_edge(from: &str, to: &str, rel: &str, valid_from: &str, valid_to: &str) -> GraphEdge {
    GraphEdge {
        valid_to: Some(valid_to.into()),
        ..edge(from, to, rel, valid_from)
    }
}

/// Build the fixed deterministic corpus on a fresh tempdir store, embeddings
/// and FTS/ANN indexes included. Same inputs every run.
async fn seed(repo: &Store) {
    // create_conversation_message enqueues a transcript embedding job, which
    // needs the provider name configured (mirrors `mem serve` startup).
    repo.set_transcript_job_provider("fake");

    // ── Capsules: tenant t1 (main) + t2 (isolation). Fixed ids/stamps. ──
    let caps = vec![
        // content chosen so FTS terms are distinguishable per bucket.
        cap(
            "cap_a",
            "t1",
            CapabilityCapsuleType::Implementation,
            CapabilityCapsuleStatus::Active,
            Scope::Repo,
            "duckdb attaches the lance dataset as a sql read engine",
            &["storage"],
            1,
            None,
            "00000000000000000001",
        ),
        cap(
            "cap_b",
            "t1",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::Active,
            Scope::Project,
            "transcript ragged batch fts scanner bug recurs",
            &["transcript", "fts"],
            1,
            None,
            "00000000000000000002",
        ),
        cap(
            "cap_c",
            "t1",
            CapabilityCapsuleType::Preference,
            CapabilityCapsuleStatus::Active,
            Scope::Repo,
            "always run fmt and clippy before every commit",
            &["ci"],
            1,
            None,
            "00000000000000000003",
        ),
        cap(
            "cap_d_v1",
            "t1",
            CapabilityCapsuleType::Implementation,
            CapabilityCapsuleStatus::Active,
            Scope::Repo,
            "old decay formula uses updated_at only",
            &["decay"],
            1,
            None,
            "00000000000000000004",
        ),
        cap(
            "cap_d_v2",
            "t1",
            CapabilityCapsuleType::Implementation,
            CapabilityCapsuleStatus::Active,
            Scope::Repo,
            "new decay formula anchors on last_used_at",
            &["decay"],
            2,
            Some("cap_d_v1"),
            "00000000000000000005",
        ),
        cap(
            "cap_e_pending",
            "t1",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::PendingConfirmation,
            Scope::Repo,
            "proposed experience capsule awaiting review",
            &["review"],
            1,
            None,
            "00000000000000000006",
        ),
        cap(
            "cap_f_archived",
            "t1",
            CapabilityCapsuleType::Experience,
            CapabilityCapsuleStatus::Archived,
            Scope::Repo,
            "archived incorrect fact",
            &["review"],
            1,
            None,
            "00000000000000000007",
        ),
        cap(
            "cap_t2_x",
            "t2",
            CapabilityCapsuleType::Implementation,
            CapabilityCapsuleStatus::Active,
            Scope::Repo,
            "tenant two isolation capsule should never leak into t1",
            &["storage"],
            1,
            None,
            "00000000000000000008",
        ),
    ];
    for c in &caps {
        repo.insert_capability_capsule(c.clone()).await.unwrap();
        repo.upsert_capability_capsule_embedding(
            &c.capability_capsule_id,
            &c.tenant,
            "fake",
            DIM as i64,
            &f32_to_blob(&deterministic_embedding(&c.content, DIM)),
            &c.content_hash,
            &c.updated_at,
            "00000000000000000010",
        )
        .await
        .unwrap();
    }

    // ── Transcript blocks (t1, session S1) + embeddings. ──
    let msgs = vec![
        tmsg(
            "mb_1",
            "t1",
            1,
            "we discussed the rust storage layer design",
            "00000000000000000020",
        ),
        tmsg(
            "mb_2",
            "t1",
            2,
            "duckdb attaches the lance dataset for reads",
            "00000000000000000021",
        ),
        tmsg(
            "mb_3",
            "t1",
            3,
            "tantivy is a full text search alternative",
            "00000000000000000022",
        ),
    ];
    for m in &msgs {
        repo.create_conversation_message(m).await.unwrap();
        repo.upsert_conversation_message_embedding(
            &m.message_block_id,
            &m.tenant,
            "fake",
            DIM as i64,
            &f32_to_blob(&deterministic_embedding(&m.content, DIM)),
            &format!("hash-{}", m.message_block_id),
            &m.created_at,
            "00000000000000000030",
        )
        .await
        .unwrap();
    }

    // ── Graph edges: two capsules + a 2-hop chain off a shared node. ──
    // Capsule nodes are encoded `capability_capsule:<id>` in graph_edges (the
    // form ingest writes); `related_capability_capsule_ids` only recognises
    // that prefix, so bare ids would read back empty.
    for e in [
        edge(
            "capability_capsule:cap_a",
            "entity:proj-mem",
            "mentions",
            "00000000000000000001",
        ),
        edge(
            "capability_capsule:cap_b",
            "entity:proj-mem",
            "mentions",
            "00000000000000000002",
        ),
        edge(
            "entity:proj-mem",
            "entity:org-x",
            "part_of",
            "00000000000000000003",
        ),
        // ── batch-C graph-tunnel reads need more edge variety: ──
        // Two ACTIVE user-tunnel edges (caller-curated bridges) for
        // list_user_tunnels / find_tunnels / follow_tunnels. The
        // `user_tunnel:` relation prefix is what those reads filter on.
        edge(
            "capability_capsule:cap_a",
            "capability_capsule:cap_b",
            "user_tunnel:link_ab",
            "00000000000000000050",
        ),
        edge(
            "capability_capsule:cap_b",
            "entity:proj-mem",
            "user_tunnel:topic_bridge",
            "00000000000000000051",
        ),
        // A CLOSED user-tunnel edge — must be excluded by the active-only
        // tunnel listings but surface in cap_a's kg_timeline (history).
        closed_edge(
            "capability_capsule:cap_a",
            "entity:org-x",
            "user_tunnel:archived",
            "00000000000000000052",
            "00000000000000000060",
        ),
    ] {
        repo.add_edge_direct(&e).await.unwrap();
    }

    // ── Entity registry (batch-B): one entity with two aliases. ──
    // `resolve_or_create` mints a UUIDv7 entity_id (non-deterministic), so
    // golden projections must NOT embed the raw id — they project canonical
    // name / kind / aliases and id-consistency flags instead.
    let entity_id = repo
        .resolve_or_create(
            "t1",
            "Rust Async",
            mem::domain::EntityKind::Topic,
            "00000000000000000040",
        )
        .await
        .unwrap();
    repo.add_alias("t1", &entity_id, "Tokio", "00000000000000000041")
        .await
        .unwrap();

    // ── Embedding jobs (batch-B): one capsule-side + one transcript-side,
    //    each with a FIXED job_id so the status reads are deterministic. ──
    repo.try_enqueue_embedding_job(mem::storage::types::EmbeddingJobInsert {
        job_id: "job_cap_b".into(),
        tenant: "t1".into(),
        capability_capsule_id: "cap_a".into(),
        target_content_hash: "hash-cap_a".into(),
        provider: "fake".into(),
        available_at: "00000000000000000042".into(),
        created_at: "00000000000000000042".into(),
        updated_at: "00000000000000000042".into(),
    })
    .await
    .unwrap();
    repo.try_enqueue_transcript_embedding_job(
        "job_tr_b".into(),
        "t1".into(),
        "mb_1".into(),
        "fake".into(),
        "00000000000000000043".into(),
    )
    .await
    .unwrap();

    // Force-build FTS + (where eligible) ANN indexes over all seeded rows so
    // the bm25 buckets see a fully-covering index (deterministic).
    repo.rebuild_query_indexes().await.unwrap();
}

/// Verify-or-refresh one bucket's golden under `tests/golden/<name>.json`.
fn check_or_write(name: &str, value: serde_json::Value) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let path = dir.join(format!("{name}.json"));
    let mut actual = serde_json::to_string_pretty(&value).unwrap();
    actual.push('\n');
    if std::env::var("REFRESH_GOLDEN").as_deref() == Ok("1") {
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, &actual).unwrap();
    } else {
        let expected = fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!("golden {name}.json missing — run `REFRESH_GOLDEN=1 cargo test --test parity_golden`")
        });
        assert_eq!(
            actual, expected,
            "parity golden drift for `{name}` (run REFRESH_GOLDEN=1 to refresh)"
        );
    }
}

/// Phase-1 parity assertion: a MIGRATED bucket's lance-engine output must
/// byte-match the committed DuckDB golden (never writes — the DuckDB side
/// owns the golden via `check_or_write`).
fn assert_golden(name: &str, value: serde_json::Value) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let mut actual = serde_json::to_string_pretty(&value).unwrap();
    actual.push('\n');
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("golden {name}.json missing — generate it via the DuckDB test"));
    assert_eq!(
        actual, expected,
        "lance-engine parity drift for `{name}` vs DuckDB golden"
    );
}

/// SOFT parity assertion for a different-engine ranked result (§7): a
/// migrated bucket whose engine is NOT byte-compatible with DuckDB (a
/// different BM25 implementation won't reproduce the exact scores /
/// rank order). The golden is read for its ordered id list, and the
/// lance result is asserted "acceptably close" on two axes — lenient on
/// exact rank order, strict on which docs come back:
///
/// 1. `overlap@10 ≥ 0.8` (overlap of the top-10 id sets:
///    `|intersection| / |golden top-10|`), AND
/// 2. the lance id set is equal-or-superset of the golden id set within
///    that tolerance — every golden id must appear in the lance result.
///
/// A wrong-docs result fails #1; a superset that still contains all
/// golden docs passes — a stricter engine returning a few extra true
/// matches is fine, dropping a golden doc is not.
fn assert_golden_soft(name: &str, lance_ranked: Vec<(String, i64)>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("golden {name}.json missing — generate it via the DuckDB test"));
    // Golden shape is `[[id, rank], …]` in rank order.
    let golden: Vec<(String, i64)> = serde_json::from_str::<Vec<(String, i64)>>(&raw)
        .unwrap_or_else(|e| panic!("golden {name}.json not a [[id,rank],…] array: {e}"));

    let golden_ids: Vec<&str> = golden.iter().map(|(id, _)| id.as_str()).collect();
    let lance_ids: Vec<&str> = lance_ranked.iter().map(|(id, _)| id.as_str()).collect();
    let golden_set: std::collections::HashSet<&str> = golden_ids.iter().copied().collect();
    let lance_set: std::collections::HashSet<&str> = lance_ids.iter().copied().collect();

    // overlap@10: |golden_top10 ∩ lance_top10| / |golden_top10|.
    let g10: std::collections::HashSet<&str> = golden_ids.iter().take(10).copied().collect();
    let l10: std::collections::HashSet<&str> = lance_ids.iter().take(10).copied().collect();
    let inter = g10.intersection(&l10).count();
    let overlap = if g10.is_empty() {
        1.0
    } else {
        inter as f64 / g10.len() as f64
    };
    assert!(
        overlap >= 0.8,
        "soft parity `{name}`: overlap@10 = {overlap:.3} < 0.8\n  golden={golden_ids:?}\n  lance ={lance_ids:?}"
    );
    // Equal-or-superset: every golden id must be present in the lance set.
    let missing: Vec<&str> = golden_set.difference(&lance_set).copied().collect();
    assert!(
        missing.is_empty(),
        "soft parity `{name}`: lance result is not a superset of golden — missing {missing:?}\n  golden={golden_ids:?}\n  lance ={lance_ids:?}"
    );
}

/// SOFT parity assertion for a different-engine result whose golden is a
/// **plain ordered id array** (`["mb_2", …]`), not `[[id, rank], …]`. Used
/// for the `transcript_fts` bucket — Tantivy is a different BM25 engine
/// than DuckDB's `lance_fts`, so it won't byte-match the golden, but the
/// same honesty bar applies as [`assert_golden_soft`]:
///
/// 1. `overlap@10 ≥ 0.8` (overlap of the top-10 id sets:
///    `|intersection| / |golden top-10|`), AND
/// 2. the lance id set is equal-or-superset of the golden id set — every
///    golden id must appear in the lance result.
fn assert_golden_soft_ids(name: &str, lance_ids: Vec<String>) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("golden {name}.json missing — generate it via the DuckDB test"));
    // Golden shape is a plain ordered id array `["id", …]`.
    let golden_ids: Vec<String> = serde_json::from_str::<Vec<String>>(&raw)
        .unwrap_or_else(|e| panic!("golden {name}.json not a [\"id\", …] array: {e}"));

    let golden_refs: Vec<&str> = golden_ids.iter().map(|s| s.as_str()).collect();
    let lance_refs: Vec<&str> = lance_ids.iter().map(|s| s.as_str()).collect();
    let golden_set: std::collections::HashSet<&str> = golden_refs.iter().copied().collect();
    let lance_set: std::collections::HashSet<&str> = lance_refs.iter().copied().collect();

    // overlap@10: |golden_top10 ∩ lance_top10| / |golden_top10|.
    let g10: std::collections::HashSet<&str> = golden_refs.iter().take(10).copied().collect();
    let l10: std::collections::HashSet<&str> = lance_refs.iter().take(10).copied().collect();
    let inter = g10.intersection(&l10).count();
    let overlap = if g10.is_empty() {
        1.0
    } else {
        inter as f64 / g10.len() as f64
    };
    assert!(
        overlap >= 0.8,
        "soft parity `{name}`: overlap@10 = {overlap:.3} < 0.8\n  golden={golden_refs:?}\n  lance ={lance_refs:?}"
    );
    // Equal-or-superset: every golden id must be present in the lance set.
    let missing: Vec<&str> = golden_set.difference(&lance_set).copied().collect();
    assert!(
        missing.is_empty(),
        "soft parity `{name}`: lance result is not a superset of golden — missing {missing:?}\n  golden={golden_refs:?}\n  lance ={lance_refs:?}"
    );
}

fn sorted_ids<I: IntoIterator<Item = String>>(ids: I) -> serde_json::Value {
    let mut v: Vec<String> = ids.into_iter().collect();
    v.sort();
    json!(v)
}

/// Project a `Vec<GraphEdge>` to a stable JSON value, sorting by
/// `(from, to, relation, valid_from)` so non-load-bearing scan order
/// can't drift the golden. The engine-side ordering is asserted
/// separately by the unit tests in `duckdb_query/graph.rs`; here we
/// only need a deterministic set projection for cross-engine parity.
fn graph_edges_json(mut edges: Vec<GraphEdge>) -> serde_json::Value {
    edges.sort_by(|a, b| {
        (&a.from_node_id, &a.to_node_id, &a.relation, &a.valid_from).cmp(&(
            &b.from_node_id,
            &b.to_node_id,
            &b.relation,
            &b.valid_from,
        ))
    });
    serde_json::to_value(&edges).unwrap()
}

/// Project `Vec<(String, String)>` endpoint pairs to a stable JSON
/// value (sorted) — `incident_edges_for_nodes` has no load-bearing
/// order, so we sort the pairs deterministically.
fn sorted_pairs(mut pairs: Vec<(String, String)>) -> serde_json::Value {
    pairs.sort();
    json!(pairs
        .into_iter()
        .map(|(a, b)| json!([a, b]))
        .collect::<Vec<_>>())
}

/// `Vec<(id, rank)>` in engine order (ranking is semantic; ties broken by id
/// in SQL → deterministic). Score floats are not in this shape.
fn ranked(pairs: Vec<(String, i64)>) -> serde_json::Value {
    json!(pairs
        .into_iter()
        .map(|(id, r)| json!([id, r]))
        .collect::<Vec<_>>())
}

/// `Vec<(record, score_f32)>` → `[[id, round(score*1e6)], …]` in engine order.
fn scored(pairs: Vec<(CapabilityCapsuleRecord, f32)>) -> serde_json::Value {
    json!(pairs
        .into_iter()
        .map(|(rec, s)| json!([rec.capability_capsule_id, (s * 1_000_000.0).round() as i64]))
        .collect::<Vec<_>>())
}

#[tokio::test]
async fn duckdb_parity_golden() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("parity.duckdb");
    // The default read engine is now `Lance` (route-B Phase 3), so pin the
    // DuckDb engine explicitly here — this test GENERATES / verifies the
    // goldens from the DuckDb side (the `check_or_write` exact-match owner),
    // which must keep exercising DuckDb until the engine is deleted.
    let repo = Arc::new(
        Store::open_with_read_engine(&db, mem::config::ReadEngine::DuckDb)
            .await
            .unwrap(),
    );
    seed(&repo).await;

    let q_text = "decay formula";
    let q_vec = deterministic_embedding("new decay formula anchors on last_used_at", DIM);

    // ── filter ──
    let listed = repo
        .list_capability_capsules_for_tenant("t1")
        .await
        .unwrap();
    check_or_write(
        "filter",
        sorted_ids(listed.into_iter().map(|c| c.capability_capsule_id)),
    );

    // ── batch-A: list_wings ── (distinct projects, alpha order; set-stable)
    let wings = repo.list_wings("t1").await.unwrap();
    check_or_write("list_wings", sorted_ids(wings));

    // ── batch-A: get_pending ── (the one PendingConfirmation row, + a miss)
    let pending = repo.get_pending("t1", "cap_e_pending").await.unwrap();
    let pending_miss = repo.get_pending("t1", "cap_a").await.unwrap();
    check_or_write(
        "get_pending",
        json!({
            "hit": pending.map(|c| c.capability_capsule_id),
            "miss_active": pending_miss.map(|c| c.capability_capsule_id),
        }),
    );

    // ── batch-A: list_pending_review ── (created_at DESC; ordered ids)
    let pending_review = repo.list_pending_review("t1").await.unwrap();
    check_or_write(
        "list_pending_review",
        json!(pending_review
            .into_iter()
            .map(|c| c.capability_capsule_id)
            .collect::<Vec<_>>()),
    );

    // ── batch-A: recent_active ── (updated_at DESC, version DESC, id ASC;
    //    excludes rejected/archived + diary; ORDER is load-bearing → ordered)
    let recent = repo
        .recent_active_capability_capsules("t1", 10)
        .await
        .unwrap();
    check_or_write(
        "recent_active",
        json!(recent
            .into_iter()
            .map(|c| c.capability_capsule_id)
            .collect::<Vec<_>>()),
    );

    // ── batch-A: list_ids ── (updated_at DESC; ALL statuses; ordered ids)
    let ids = repo
        .list_capability_capsule_ids_for_tenant("t1")
        .await
        .unwrap();
    check_or_write("list_ids", json!(ids));

    // ── batch-A: list_in_scope ── (project=mem, repo=mem; limit=3 forces a
    //    has_more=true page; ORDER updated_at DESC, id ASC is load-bearing)
    let (scope_p1, scope_more) = repo
        .list_capability_capsules_in_scope(
            "t1",
            Some("mem"),
            Some("mem"),
            None,
            None,
            None,
            None,
            None,
            3,
        )
        .await
        .unwrap();
    check_or_write(
        "list_in_scope",
        json!({
            "page1_ids": scope_p1
                .into_iter()
                .map(|c| c.capability_capsule_id)
                .collect::<Vec<_>>(),
            "has_more": scope_more,
        }),
    );

    // ── ann ──
    let ann = repo.ann_candidate_ids("t1", &q_vec, 5).await.unwrap();
    check_or_write("ann", ranked(ann));

    // ── fts ──
    let fts = repo.bm25_candidate_ids("t1", q_text, 5).await.unwrap();
    check_or_write("fts", ranked(fts));

    // ── hybrid (fused-SQL fast path) ──
    let hybrid = repo
        .hybrid_candidates("t1", q_text, &q_vec, 5)
        .await
        .unwrap();
    check_or_write("hybrid", scored(hybrid));

    // ── hybrid (portable compose path) — captured separately so Phase 1 can
    //    diff lance-native compose against BOTH the fused golden and this. ──
    let compose = repo
        .hybrid_candidates_compose("t1", q_text, &q_vec, 5)
        .await
        .unwrap();
    check_or_write("hybrid_compose", scored(compose));

    // ── stats ──
    let stats = repo.capsule_stats("t1").await.unwrap();
    check_or_write("stats", serde_json::to_value(stats).unwrap());

    // ── taxonomy ── (sort outer + inner for stability)
    let mut tax = repo.get_taxonomy("t1").await.unwrap();
    for (_, vs) in tax.iter_mut() {
        vs.sort();
    }
    tax.sort();
    check_or_write("taxonomy", serde_json::to_value(tax).unwrap());

    // ── version-chain ── version links + the NOT-EXISTS-deduped candidate pool
    let mut versions = repo
        .list_capability_capsule_versions_for_tenant("t1", "cap_d_v2")
        .await
        .unwrap();
    versions.sort_by(|a, b| {
        a.capability_capsule_id
            .cmp(&b.capability_capsule_id)
            .then(a.version.cmp(&b.version))
    });
    let pool = repo.search_candidates("t1").await.unwrap();
    check_or_write(
        "version_chain",
        json!({
            "versions": serde_json::to_value(&versions).unwrap(),
            "search_candidates_ids": sorted_ids(pool.into_iter().map(|c| c.capability_capsule_id)),
        }),
    );

    // ── graph ──
    let mut neighbors = repo
        .neighbors_within("entity:proj-mem", 2, None)
        .await
        .unwrap();
    neighbors.sort_by(|a, b| {
        (&a.from_node_id, &a.to_node_id, &a.relation).cmp(&(
            &b.from_node_id,
            &b.to_node_id,
            &b.relation,
        ))
    });
    let mut related = repo
        .related_capability_capsule_ids(&["entity:proj-mem".to_string()])
        .await
        .unwrap();
    related.sort();
    let gstats = repo.graph_stats().await.unwrap();
    check_or_write(
        "graph",
        json!({
            "neighbors_within_2hops": serde_json::to_value(&neighbors).unwrap(),
            "related_capsule_ids": json!(related),
            "graph_stats": serde_json::to_value(gstats).unwrap(),
        }),
    );

    // ── batch-C graph-tunnel reads ──
    // neighbors (1-hop active, on the shared entity node)
    let neighbors_1hop = repo.neighbors("entity:proj-mem").await.unwrap();
    // kg_timeline (cap_a is on a mentions edge + a closed user_tunnel) —
    // closed edges MUST surface here.
    let timeline = repo.kg_timeline("capability_capsule:cap_a").await.unwrap();
    // query_predicate: every `mentions` assertion (active + closed), and a
    // point-in-time as_of probe that excludes future-dated edges.
    let predicate_all = repo.query_predicate("mentions", None).await.unwrap();
    let predicate_as_of = repo
        .query_predicate("mentions", Some("00000000000000000001"))
        .await
        .unwrap();
    // list_user_tunnels: the two ACTIVE user-tunnel edges (closed excluded).
    let user_tunnels = repo.list_user_tunnels(100).await.unwrap();
    // find_tunnels: active user-tunnels between capability_capsule:* nodes
    // (both prefixes equal → dedup path exercised) and a broad any↔any scan.
    let tunnels_caps = repo
        .find_tunnels("capability_capsule:", "capability_capsule:", 100)
        .await
        .unwrap();
    let tunnels_any = repo.find_tunnels("", "", 100).await.unwrap();
    // follow_tunnels: BFS from cap_b following only user-tunnel edges.
    let followed = repo
        .follow_tunnels("capability_capsule:cap_b", 3)
        .await
        .unwrap();
    // incident_edges_for_nodes: active 1-hop endpoint pairs around the
    // shared entity (no load-bearing order → sorted).
    let incident = repo
        .incident_edges_for_nodes(&["entity:proj-mem".to_string()])
        .await
        .unwrap();
    check_or_write(
        "graph_tunnel",
        json!({
            "neighbors_1hop": graph_edges_json(neighbors_1hop),
            "kg_timeline_cap_a": graph_edges_json(timeline),
            "query_predicate_mentions": graph_edges_json(predicate_all),
            "query_predicate_mentions_as_of": graph_edges_json(predicate_as_of),
            "list_user_tunnels": graph_edges_json(user_tunnels),
            "find_tunnels_caps": graph_edges_json(tunnels_caps),
            "find_tunnels_any": graph_edges_json(tunnels_any),
            "follow_tunnels_cap_b": graph_edges_json(followed),
            "incident_edges_proj_mem": sorted_pairs(incident),
        }),
    );

    // ── transcript: fts + ann ──
    let t_fts = repo
        .bm25_transcript_candidates("t1", "lance", 5)
        .await
        .unwrap();
    check_or_write(
        "transcript_fts",
        json!(t_fts
            .into_iter()
            .map(|m| m.message_block_id)
            .collect::<Vec<_>>()),
    );

    let t_vec = deterministic_embedding("duckdb attaches the lance dataset for reads", DIM);
    let t_ann = repo
        .semantic_search_transcripts("t1", &t_vec, 5)
        .await
        .unwrap();
    check_or_write(
        "transcript_ann",
        json!(t_ann
            .into_iter()
            .map(|(m, _)| m.message_block_id)
            .collect::<Vec<_>>()),
    );

    // ── batch-B: get_embedding_job_status ── (hit = seeded job, miss = gone)
    let job_hit = repo.get_embedding_job_status("job_cap_b").await.unwrap();
    let job_miss = repo.get_embedding_job_status("nope").await.unwrap();
    check_or_write(
        "embedding_job_status",
        json!({ "hit": job_hit, "miss": job_miss }),
    );

    // ── batch-B: get_transcript_embedding_job_status ── (hit + miss)
    let tjob_hit = repo
        .get_transcript_embedding_job_status("job_tr_b")
        .await
        .unwrap();
    let tjob_miss = repo
        .get_transcript_embedding_job_status("nope")
        .await
        .unwrap();
    check_or_write(
        "transcript_embedding_job_status",
        json!({ "hit": tjob_hit, "miss": tjob_miss }),
    );

    // ── batch-B: get_entity ── (entity_id is a non-deterministic UUIDv7, so
    //    project a STABLE shape: canonical name / kind / ordered aliases +
    //    whether the fetched id matches the alias lookup, never the raw id)
    let looked_up = repo.lookup_alias("t1", "rust async").await.unwrap();
    let entity = repo
        .get_entity("t1", looked_up.as_deref().unwrap())
        .await
        .unwrap()
        .expect("seeded entity exists");
    let entity_miss = repo.get_entity("t1", "does-not-exist").await.unwrap();
    check_or_write(
        "get_entity",
        json!({
            "canonical_name": entity.entity.canonical_name,
            "tenant": entity.entity.tenant,
            "kind": serde_json::to_value(entity.entity.kind).unwrap(),
            "aliases": entity.aliases,
            "id_matches_lookup": Some(&entity.entity.entity_id) == looked_up.as_ref(),
            "miss_is_none": entity_miss.is_none(),
        }),
    );

    // ── batch-B: lookup_alias ── (id is a UUIDv7 → project consistency, not
    //    the raw id: both aliases resolve to the SAME entity; miss is None)
    let look_a = repo.lookup_alias("t1", "rust async").await.unwrap();
    let look_b = repo.lookup_alias("t1", "Tokio").await.unwrap();
    let look_ws = repo.lookup_alias("t1", "  RUST   ASYNC  ").await.unwrap();
    let look_miss = repo.lookup_alias("t1", "unknown").await.unwrap();
    check_or_write(
        "lookup_alias",
        json!({
            "both_aliases_same_entity": look_a.is_some() && look_a == look_b,
            "normalized_ws_same_entity": look_a == look_ws,
            "miss_is_none": look_miss.is_none(),
        }),
    );

    // ── batch-B: list_transcript_sessions ── (per-session aggregate;
    //    last_at DESC; fully deterministic values)
    let sessions = repo.list_transcript_sessions("t1").await.unwrap();
    check_or_write(
        "list_transcript_sessions",
        serde_json::to_value(&sessions).unwrap(),
    );

    // ── batch-B: recent_conversation_messages ── (created_at DESC,
    //    line_number DESC, block_index DESC; ordered ids are load-bearing)
    let recent_msgs = repo.recent_conversation_messages("t1", 10).await.unwrap();
    check_or_write(
        "recent_conversation_messages",
        json!(recent_msgs
            .into_iter()
            .map(|m| m.message_block_id)
            .collect::<Vec<_>>()),
    );
}

/// Phase-1 parity double-run: seed the same fixture, read each MIGRATED
/// bucket under `ReadEngine::Lance`, and assert it byte-matches the DuckDB
/// golden (= lance == duckdb). Buckets are added here as they pass; an
/// unmigrated bucket is simply absent. See `docs/remove-duckdb-keep-lance.md`
/// §5 Phase 1.
#[tokio::test]
async fn lance_parity_matches_golden() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("parity_lance.duckdb");
    let repo = Arc::new(
        Store::open_with_read_engine(&db, mem::config::ReadEngine::Lance)
            .await
            .unwrap(),
    );
    seed(&repo).await;

    // ── filter ──
    let listed = repo
        .list_capability_capsules_for_tenant("t1")
        .await
        .unwrap();
    assert_golden(
        "filter",
        sorted_ids(listed.into_iter().map(|c| c.capability_capsule_id)),
    );

    // ── batch-A: list_wings ── (non-FTS → exact byte-match)
    let wings = repo.list_wings("t1").await.unwrap();
    assert_golden("list_wings", sorted_ids(wings));

    // ── batch-A: get_pending ── (exact byte-match)
    let pending = repo.get_pending("t1", "cap_e_pending").await.unwrap();
    let pending_miss = repo.get_pending("t1", "cap_a").await.unwrap();
    assert_golden(
        "get_pending",
        json!({
            "hit": pending.map(|c| c.capability_capsule_id),
            "miss_active": pending_miss.map(|c| c.capability_capsule_id),
        }),
    );

    // ── batch-A: list_pending_review ── (created_at DESC; exact byte-match)
    let pending_review = repo.list_pending_review("t1").await.unwrap();
    assert_golden(
        "list_pending_review",
        json!(pending_review
            .into_iter()
            .map(|c| c.capability_capsule_id)
            .collect::<Vec<_>>()),
    );

    // ── batch-A: recent_active ── (updated_at DESC, version DESC, id ASC;
    //    exact byte-match — order is load-bearing)
    let recent = repo
        .recent_active_capability_capsules("t1", 10)
        .await
        .unwrap();
    assert_golden(
        "recent_active",
        json!(recent
            .into_iter()
            .map(|c| c.capability_capsule_id)
            .collect::<Vec<_>>()),
    );

    // ── batch-A: list_ids ── (updated_at DESC; exact byte-match)
    let ids = repo
        .list_capability_capsule_ids_for_tenant("t1")
        .await
        .unwrap();
    assert_golden("list_ids", json!(ids));

    // ── batch-A: list_in_scope ── (limit=3 → has_more page; exact byte-match)
    let (scope_p1, scope_more) = repo
        .list_capability_capsules_in_scope(
            "t1",
            Some("mem"),
            Some("mem"),
            None,
            None,
            None,
            None,
            None,
            3,
        )
        .await
        .unwrap();
    assert_golden(
        "list_in_scope",
        json!({
            "page1_ids": scope_p1
                .into_iter()
                .map(|c| c.capability_capsule_id)
                .collect::<Vec<_>>(),
            "has_more": scope_more,
        }),
    );

    // ── ann ── (postfilter parity: top-k across tenants, then filter t1)
    let q_vec = deterministic_embedding("new decay formula anchors on last_used_at", DIM);
    let ann = repo.ann_candidate_ids("t1", &q_vec, 5).await.unwrap();
    assert_golden("ann", ranked(ann));

    // ── fts ── SOFT parity: Tantivy is a different BM25 engine than
    //    DuckDB's lance_fts, so it won't byte-match the golden's exact
    //    score-derived ranks. We assert overlap@10 ≥ 0.8 + superset of the
    //    golden id set (§7). The fixture's golden set is {cap_d_v1,
    //    cap_d_v2} — Tantivy must return both.
    let fts = repo
        .bm25_candidate_ids("t1", "decay formula", 5)
        .await
        .unwrap();
    assert_golden_soft("fts", fts);

    // ── stats ──
    let stats = repo.capsule_stats("t1").await.unwrap();
    assert_golden("stats", serde_json::to_value(stats).unwrap());

    // ── taxonomy ── (sort outer + inner for stability — same as duckdb side)
    let mut tax = repo.get_taxonomy("t1").await.unwrap();
    for (_, vs) in tax.iter_mut() {
        vs.sort();
    }
    tax.sort();
    assert_golden("taxonomy", serde_json::to_value(tax).unwrap());

    // ── graph ── (mirrors the duckdb block verbatim: BFS neighbors +
    //    related capsule ids + whole-graph stats)
    let mut neighbors = repo
        .neighbors_within("entity:proj-mem", 2, None)
        .await
        .unwrap();
    neighbors.sort_by(|a, b| {
        (&a.from_node_id, &a.to_node_id, &a.relation).cmp(&(
            &b.from_node_id,
            &b.to_node_id,
            &b.relation,
        ))
    });
    let mut related = repo
        .related_capability_capsule_ids(&["entity:proj-mem".to_string()])
        .await
        .unwrap();
    related.sort();
    let gstats = repo.graph_stats().await.unwrap();
    assert_golden(
        "graph",
        json!({
            "neighbors_within_2hops": serde_json::to_value(&neighbors).unwrap(),
            "related_capsule_ids": json!(related),
            "graph_stats": serde_json::to_value(gstats).unwrap(),
        }),
    );

    // ── batch-C graph-tunnel reads ── (mirrors the duckdb block verbatim:
    //    neighbors / kg_timeline / query_predicate / list_user_tunnels /
    //    find_tunnels / follow_tunnels / incident_edges_for_nodes — all
    //    exact byte-match against the DuckDb-owned golden)
    let neighbors_1hop = repo.neighbors("entity:proj-mem").await.unwrap();
    let timeline = repo.kg_timeline("capability_capsule:cap_a").await.unwrap();
    let predicate_all = repo.query_predicate("mentions", None).await.unwrap();
    let predicate_as_of = repo
        .query_predicate("mentions", Some("00000000000000000001"))
        .await
        .unwrap();
    let user_tunnels = repo.list_user_tunnels(100).await.unwrap();
    let tunnels_caps = repo
        .find_tunnels("capability_capsule:", "capability_capsule:", 100)
        .await
        .unwrap();
    let tunnels_any = repo.find_tunnels("", "", 100).await.unwrap();
    let followed = repo
        .follow_tunnels("capability_capsule:cap_b", 3)
        .await
        .unwrap();
    let incident = repo
        .incident_edges_for_nodes(&["entity:proj-mem".to_string()])
        .await
        .unwrap();
    assert_golden(
        "graph_tunnel",
        json!({
            "neighbors_1hop": graph_edges_json(neighbors_1hop),
            "kg_timeline_cap_a": graph_edges_json(timeline),
            "query_predicate_mentions": graph_edges_json(predicate_all),
            "query_predicate_mentions_as_of": graph_edges_json(predicate_as_of),
            "list_user_tunnels": graph_edges_json(user_tunnels),
            "find_tunnels_caps": graph_edges_json(tunnels_caps),
            "find_tunnels_any": graph_edges_json(tunnels_any),
            "follow_tunnels_cap_b": graph_edges_json(followed),
            "incident_edges_proj_mem": sorted_pairs(incident),
        }),
    );

    // ── version-chain ── (mirrors the duckdb block verbatim: version links
    //    walked off cap_d_v2 + the NOT-EXISTS-deduped candidate pool)
    let mut versions = repo
        .list_capability_capsule_versions_for_tenant("t1", "cap_d_v2")
        .await
        .unwrap();
    versions.sort_by(|a, b| {
        a.capability_capsule_id
            .cmp(&b.capability_capsule_id)
            .then(a.version.cmp(&b.version))
    });
    let pool = repo.search_candidates("t1").await.unwrap();
    assert_golden(
        "version_chain",
        json!({
            "versions": serde_json::to_value(&versions).unwrap(),
            "search_candidates_ids": sorted_ids(pool.into_iter().map(|c| c.capability_capsule_id)),
        }),
    );

    // ── transcript: ann ── (mirrors the duckdb block verbatim: lance-native
    //    nearest_to + chunk-collapse, postfilter tenant + embed_eligible,
    //    distance-ordered ids)
    let t_vec = deterministic_embedding("duckdb attaches the lance dataset for reads", DIM);
    let t_ann = repo
        .semantic_search_transcripts("t1", &t_vec, 5)
        .await
        .unwrap();
    assert_golden(
        "transcript_ann",
        json!(t_ann
            .into_iter()
            .map(|(m, _)| m.message_block_id)
            .collect::<Vec<_>>()),
    );

    // ── transcript: fts ── SOFT parity: Tantivy is a different BM25 engine
    //    than DuckDB's lance_fts, so it won't byte-match the golden's exact
    //    id order. We assert overlap@10 ≥ 0.8 + superset of the golden id
    //    set (§7). The fixture's golden set is {mb_2} (the only block whose
    //    content mentions "lance") — Tantivy must return it.
    let t_fts = repo
        .bm25_transcript_candidates("t1", "lance", 5)
        .await
        .unwrap();
    assert_golden_soft_ids(
        "transcript_fts",
        t_fts
            .into_iter()
            .map(|m| m.message_block_id)
            .collect::<Vec<_>>(),
    );

    // ── hybrid ── On the lance engine the fused-SQL fast path is dropped for
    //    the portable compose (Tantivy BM25 + lance ANN + Rust RRF), so BOTH
    //    `hybrid_candidates` and `hybrid_candidates_compose` soft-match the
    //    COMPOSE golden (`hybrid_compose.json`). The fused `hybrid.json`
    //    (version-chain-deduped) is intentionally NOT reproduced — that dedup
    //    is redundant downstream (the `search_candidates` recall pool already
    //    excludes superseded rows). See docs/remove-duckdb-keep-lance.md §3.
    let q_vec_h = deterministic_embedding("new decay formula anchors on last_used_at", DIM);
    let lance_hybrid = repo
        .hybrid_candidates("t1", "decay formula", &q_vec_h, 5)
        .await
        .unwrap();
    assert_golden_soft(
        "hybrid_compose",
        lance_hybrid
            .into_iter()
            .map(|(r, _)| (r.capability_capsule_id, 0_i64))
            .collect(),
    );
    let lance_compose = repo
        .hybrid_candidates_compose("t1", "decay formula", &q_vec_h, 5)
        .await
        .unwrap();
    assert_golden_soft(
        "hybrid_compose",
        lance_compose
            .into_iter()
            .map(|(r, _)| (r.capability_capsule_id, 0_i64))
            .collect(),
    );

    // ── batch-B: get_embedding_job_status ── (exact byte-match)
    let job_hit = repo.get_embedding_job_status("job_cap_b").await.unwrap();
    let job_miss = repo.get_embedding_job_status("nope").await.unwrap();
    assert_golden(
        "embedding_job_status",
        json!({ "hit": job_hit, "miss": job_miss }),
    );

    // ── batch-B: get_transcript_embedding_job_status ── (exact byte-match)
    let tjob_hit = repo
        .get_transcript_embedding_job_status("job_tr_b")
        .await
        .unwrap();
    let tjob_miss = repo
        .get_transcript_embedding_job_status("nope")
        .await
        .unwrap();
    assert_golden(
        "transcript_embedding_job_status",
        json!({ "hit": tjob_hit, "miss": tjob_miss }),
    );

    // ── batch-B: get_entity ── (stable shape, mirrors the duckdb block)
    let looked_up = repo.lookup_alias("t1", "rust async").await.unwrap();
    let entity = repo
        .get_entity("t1", looked_up.as_deref().unwrap())
        .await
        .unwrap()
        .expect("seeded entity exists");
    let entity_miss = repo.get_entity("t1", "does-not-exist").await.unwrap();
    assert_golden(
        "get_entity",
        json!({
            "canonical_name": entity.entity.canonical_name,
            "tenant": entity.entity.tenant,
            "kind": serde_json::to_value(entity.entity.kind).unwrap(),
            "aliases": entity.aliases,
            "id_matches_lookup": Some(&entity.entity.entity_id) == looked_up.as_ref(),
            "miss_is_none": entity_miss.is_none(),
        }),
    );

    // ── batch-B: lookup_alias ── (consistency shape, mirrors the duckdb block)
    let look_a = repo.lookup_alias("t1", "rust async").await.unwrap();
    let look_b = repo.lookup_alias("t1", "Tokio").await.unwrap();
    let look_ws = repo.lookup_alias("t1", "  RUST   ASYNC  ").await.unwrap();
    let look_miss = repo.lookup_alias("t1", "unknown").await.unwrap();
    assert_golden(
        "lookup_alias",
        json!({
            "both_aliases_same_entity": look_a.is_some() && look_a == look_b,
            "normalized_ws_same_entity": look_a == look_ws,
            "miss_is_none": look_miss.is_none(),
        }),
    );

    // ── batch-B: list_transcript_sessions ── (per-session aggregate;
    //    last_at DESC; exact byte-match)
    let sessions = repo.list_transcript_sessions("t1").await.unwrap();
    assert_golden(
        "list_transcript_sessions",
        serde_json::to_value(&sessions).unwrap(),
    );

    // ── batch-B: recent_conversation_messages ── (created_at DESC,
    //    line_number DESC, block_index DESC; exact byte-match on ordered ids)
    let recent_msgs = repo.recent_conversation_messages("t1", 10).await.unwrap();
    assert_golden(
        "recent_conversation_messages",
        json!(recent_msgs
            .into_iter()
            .map(|m| m.message_block_id)
            .collect::<Vec<_>>()),
    );
}
