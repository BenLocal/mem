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
    ] {
        repo.add_edge_direct(&e).await.unwrap();
    }

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

fn sorted_ids<I: IntoIterator<Item = String>>(ids: I) -> serde_json::Value {
    let mut v: Vec<String> = ids.into_iter().collect();
    v.sort();
    json!(v)
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
    let repo = Arc::new(Store::open(&db).await.unwrap());
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

    // ── ann ── (postfilter parity: top-k across tenants, then filter t1)
    let q_vec = deterministic_embedding("new decay formula anchors on last_used_at", DIM);
    let ann = repo.ann_candidate_ids("t1", &q_vec, 5).await.unwrap();
    assert_golden("ann", ranked(ann));

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
}
