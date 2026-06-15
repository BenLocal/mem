//! Postgres backend integration tests (postgres-backend.md P1).
//!
//! Gated on the `postgres` cargo feature AND a reachable test database:
//! every test reads `MEM_TEST_POSTGRES_URL` and **skips** (prints +
//! returns) when it is unset, so the default `cargo test` (no feature,
//! no DB) stays green and CI's `rust` job is unaffected. To run:
//!
//! ```bash
//! docker run -d --name mem-pg -e POSTGRES_PASSWORD=mem -e POSTGRES_DB=mem \
//!   -p 5433:5432 pgvector/pgvector:pg16
//! MEM_TEST_POSTGRES_URL=postgres://postgres:mem@127.0.0.1:5433/mem \
//!   cargo test --features postgres --test postgres_backend
//! ```
//!
//! P1 validates the existing `PostgresCapsuleStore` scaffold (the
//! `CapsuleStore` trait) against a real Postgres — the Phase-4 spike
//! validation that the doc said "needs Docker + testcontainers" and
//! never ran. Cases mirror `capsule_store_parity.rs`.
#![cfg(feature = "postgres")]

use std::sync::Arc;

use mem::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
};
use mem::storage::{
    current_timestamp, CapsuleSearchStore, CapsuleStore, EmbeddingVectorStore, FeedbackEvent,
    PostgresCapsuleStore,
};

/// `Some(store)` on a fresh schema when `MEM_TEST_POSTGRES_URL` is set,
/// else `None` (caller skips). Each call drops + re-applies the schema
/// so tests are order-independent.
async fn backend() -> Option<Arc<dyn CapsuleStore>> {
    let url = std::env::var("MEM_TEST_POSTGRES_URL").ok()?;
    let store = PostgresCapsuleStore::connect_fresh(&url)
        .await
        .expect("connect + migrate test postgres");
    Some(Arc::new(store))
}

/// Like [`backend`] but returns the concrete store so we can both call
/// `EmbeddingVectorStore` methods AND reach `pool()` for raw pgvector
/// ANN queries. `None` (skip) when `MEM_TEST_POSTGRES_URL` is unset.
async fn embedding_backend() -> Option<PostgresCapsuleStore> {
    let url = std::env::var("MEM_TEST_POSTGRES_URL").ok()?;
    Some(
        PostgresCapsuleStore::connect_fresh(&url)
            .await
            .expect("connect + migrate test postgres"),
    )
}

/// f32 vector of length `dim` with `v[i] = vals[i]` and zeros after.
fn vec_of(dim: usize, vals: &[f32]) -> Vec<f32> {
    let mut v = vec![0.0_f32; dim];
    for (i, x) in vals.iter().enumerate() {
        v[i] = *x;
    }
    v
}

/// Native-endian f32 blob (matches `crate::embedding::wire::encode_f32_blob`).
fn blob_of(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_ne_bytes());
    }
    out
}

macro_rules! pg_test {
    ($name:ident, $backend:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some($backend) = backend().await else {
                eprintln!("skip {}: MEM_TEST_POSTGRES_URL unset", stringify!($name));
                return;
            };
            $body
        }
    };
}

fn fixture(id: &str, status: CapabilityCapsuleStatus) -> CapabilityCapsuleRecord {
    CapabilityCapsuleRecord {
        capability_capsule_id: id.into(),
        tenant: "t".into(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status,
        scope: Scope::Repo,
        visibility: Visibility::Private,
        version: 1,
        summary: format!("summary-{id}"),
        content: format!("content-{id}"),
        content_hash: format!("{id:0>64}"),
        confidence: 0.5,
        decay_score: 0.0,
        source_agent: "test".into(),
        created_at: "00000000000000000000".into(),
        updated_at: "00000000000000000000".into(),
        ..Default::default()
    }
}

pg_test!(insert_and_get_round_trip, backend, {
    let row = fixture("a", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "a")
        .await
        .unwrap()
        .expect("tenant-scoped get should find the row");
    assert_eq!(got.capability_capsule_id, "a");
    assert_eq!(got.tenant, "t");
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    assert_eq!(got.content, "content-a");
});

pg_test!(get_for_other_tenant_returns_none, backend, {
    backend
        .insert_capability_capsule(fixture("a", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("other", "a")
        .await
        .unwrap();
    assert!(got.is_none(), "cross-tenant get must not leak");
});

pg_test!(accept_pending_transitions_status, backend, {
    backend
        .insert_capability_capsule(fixture("p", CapabilityCapsuleStatus::PendingConfirmation))
        .await
        .unwrap();
    backend.accept_pending("t", "p").await.unwrap();
    let got = backend
        .get_capability_capsule_for_tenant("t", "p")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, CapabilityCapsuleStatus::Active);
});

pg_test!(list_pending_review_filters_status, backend, {
    backend
        .insert_capability_capsule(fixture("act", CapabilityCapsuleStatus::Active))
        .await
        .unwrap();
    backend
        .insert_capability_capsule(fixture(
            "pend",
            CapabilityCapsuleStatus::PendingConfirmation,
        ))
        .await
        .unwrap();
    let pending = backend.list_pending_review("t").await.unwrap();
    let ids: Vec<&str> = pending
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(ids, vec!["pend"], "only PendingConfirmation rows listed");
});

pg_test!(find_by_idempotency_dedups_on_hash, backend, {
    let mut row = fixture("h", CapabilityCapsuleStatus::Active);
    row.content_hash = "deadbeef".into();
    backend.insert_capability_capsule(row).await.unwrap();
    let hit = backend
        .find_by_idempotency_or_hash("t", &None, "deadbeef")
        .await
        .unwrap();
    assert!(hit.is_some(), "existing content_hash should dedup");
    let miss = backend
        .find_by_idempotency_or_hash("t", &None, "00000000")
        .await
        .unwrap();
    assert!(miss.is_none(), "unknown hash is not a dup");
});

pg_test!(apply_feedback_moves_confidence, backend, {
    let row = fixture("f", CapabilityCapsuleStatus::Active);
    backend
        .insert_capability_capsule(row.clone())
        .await
        .unwrap();
    let before = backend
        .get_capability_capsule_for_tenant("t", "f")
        .await
        .unwrap()
        .unwrap()
        .confidence;
    let event = FeedbackEvent {
        feedback_id: "fb_1".into(),
        capability_capsule_id: "f".into(),
        feedback_kind: "useful".into(),
        created_at: current_timestamp(),
        note: None,
    };
    backend.apply_feedback(&row, event).await.unwrap();
    let after = backend
        .get_capability_capsule_for_tenant("t", "f")
        .await
        .unwrap()
        .unwrap()
        .confidence;
    assert!(
        after > before,
        "useful feedback raises confidence ({before} -> {after})"
    );
});

/// P2 smoke test: `mem serve` boots on the Postgres backend. Builds a
/// `Config` with `backend = Postgres` + a Fake embedding provider (so no
/// model download) and asserts `AppState::from_config` assembles cleanly
/// — i.e. the Postgres arm of `app.rs` connects, migrates, wires up the
/// services/workers, and never hits an `unimplemented!()` stub on the
/// startup path. Skips when `MEM_TEST_POSTGRES_URL` is unset.
#[tokio::test(flavor = "multi_thread")]
async fn serve_boots_on_postgres() {
    let Some(url) = std::env::var("MEM_TEST_POSTGRES_URL").ok() else {
        eprintln!("skip serve_boots_on_postgres: MEM_TEST_POSTGRES_URL unset");
        return;
    };
    let mut config = mem::config::Config::local();
    config.backend = mem::config::BackendKind::Postgres;
    config.postgres_url = Some(url);
    config.embedding.provider = mem::config::EmbeddingProviderKind::Fake;
    config.embedding.model = "fake".to_string();
    config.embedding.dim = 64;

    let state = mem::app::AppState::from_config(config).await;
    assert!(
        state.is_ok(),
        "AppState::from_config should assemble on the Postgres backend: {:?}",
        state.err()
    );
}

// ───────────────────────── EmbeddingVectorStore (P3) ──────────────────────

const DIM: usize = 8;

macro_rules! emb_test {
    ($name:ident, $store:ident, $body:block) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            let Some($store) = embedding_backend().await else {
                eprintln!("skip {}: MEM_TEST_POSTGRES_URL unset", stringify!($name));
                return;
            };
            $body
        }
    };
}

emb_test!(embedding_upsert_get_roundtrip, store, {
    let v = vec_of(DIM, &[0.1, -0.2, 0.3, 0.4, -0.5, 0.6, 0.7, -0.8]);
    store
        .upsert_capability_capsule_embedding(
            "cap1",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&v),
            "hash1",
            "src-ts",
            "now-ts",
        )
        .await
        .unwrap();
    let got = store
        .get_capability_capsule_embedding_vector("cap1")
        .await
        .unwrap()
        .expect("vector should exist after upsert");
    assert_eq!(got.len(), DIM);
    for (a, b) in got.iter().zip(v.iter()) {
        assert!((a - b).abs() < 1e-6, "elementwise: {a} vs {b}");
    }
    // metadata triple = (model, content_hash, created_at==now)
    let row = store
        .get_capability_capsule_embedding_row("cap1")
        .await
        .unwrap()
        .expect("row should exist");
    assert_eq!(row, ("fake".into(), "hash1".into(), "now-ts".into()));
});

emb_test!(embedding_chunks_replace, store, {
    let three = vec![
        vec_of(DIM, &[1.0]),
        vec_of(DIM, &[0.0, 1.0]),
        vec_of(DIM, &[0.0, 0.0, 1.0]),
    ];
    store
        .upsert_capability_capsule_embedding_chunks(
            "capc", "t", "fake", DIM as i64, &three, "h", "s", "n",
        )
        .await
        .unwrap();
    let cnt = store.count_capsule_embedding_rows("capc").await.unwrap();
    assert_eq!(cnt, 3, "first upsert writes 3 chunk rows");

    let two = vec![vec_of(DIM, &[1.0]), vec_of(DIM, &[0.0, 1.0])];
    store
        .upsert_capability_capsule_embedding_chunks(
            "capc", "t", "fake", DIM as i64, &two, "h2", "s", "n",
        )
        .await
        .unwrap();
    let cnt2 = store.count_capsule_embedding_rows("capc").await.unwrap();
    assert_eq!(cnt2, 2, "second upsert fully replaces — only 2 rows remain");
});

emb_test!(embedding_delete, store, {
    let v = vec_of(DIM, &[0.5, 0.5]);
    store
        .upsert_capability_capsule_embedding(
            "capd",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&v),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    store
        .delete_capability_capsule_embedding("capd")
        .await
        .unwrap();
    let got = store
        .get_capability_capsule_embedding_vector("capd")
        .await
        .unwrap();
    assert!(got.is_none(), "vector gone after delete");
});

emb_test!(embedding_ann_cosine_orders_by_distance, store, {
    // [1,0,..], [0.9,0.1,..], [0,1,..]; query [1,0,..]. Nearest two by
    // cosine distance are cap_a (identical dir) then cap_b (0.9,0.1).
    store
        .upsert_capability_capsule_embedding(
            "cap_a",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[1.0, 0.0])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    store
        .upsert_capability_capsule_embedding(
            "cap_b",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[0.9, 0.1])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    store
        .upsert_capability_capsule_embedding(
            "cap_c",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[0.0, 1.0])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();

    let rows = store
        .ann_nearest_capsule_ids(&vec_of(DIM, &[1.0, 0.0]), 2)
        .await
        .unwrap();
    assert_eq!(
        rows,
        vec!["cap_a".to_string(), "cap_b".to_string()],
        "pgvector cosine orders cap_a (identical) then cap_b (0.9,0.1)"
    );
});

emb_test!(conversation_embedding_roundtrip, store, {
    let v = vec_of(DIM, &[0.2, 0.4, 0.6, 0.8, -0.1, -0.3, -0.5, -0.7]);
    store
        .upsert_conversation_message_embedding(
            "msg1",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&v),
            "mh",
            "s",
            "n",
        )
        .await
        .unwrap();
    let got = store
        .get_message_embedding_vector("msg1")
        .await
        .unwrap()
        .expect("message vector should exist after upsert");
    assert_eq!(got.len(), DIM);
    for (a, b) in got.iter().zip(v.iter()) {
        assert!((a - b).abs() < 1e-6, "elementwise: {a} vs {b}");
    }
    store
        .delete_conversation_message_embedding("msg1")
        .await
        .unwrap();
    let cnt = store.count_message_embedding_rows("msg1").await.unwrap();
    assert_eq!(cnt, 0, "conversation embedding gone after delete");
});

// ───────────────────── CapsuleSearchStore — hybrid (P4) ───────────────────
//
// Verifies the Postgres lexical (tsvector) / semantic (pgvector) / RRF
// channels behave-align with the Lance backend. These assert PG's own
// behaviour (cross-backend parity is hard to scaffold; the contract is
// "same top hit set"). One case directly checks the RRF ordering against
// `1/(60+rank)` summed — the exact formula in `pipeline::retrieve::sql_rrf`.
//
// CHINESE-TOKENIZATION NOTE: the 'simple' tsvector config splits on
// whitespace/punctuation and has NO CJK segmenter, so a run of Han
// characters is one token — sub-phrase lexical recall over Chinese is weak.
// That is an accepted, documented limitation (see 0003_search.sql); pgvector
// semantic recall + RRF cover Chinese. These tests therefore exercise
// lexical recall with ASCII tokens only.

/// Capsule fixture with explicit content + type, tenant "t".
fn capsule_with(id: &str, content: &str, ty: CapabilityCapsuleType) -> CapabilityCapsuleRecord {
    let mut c = fixture(id, CapabilityCapsuleStatus::Active);
    c.capability_capsule_type = ty;
    c.content = content.into();
    c.content_hash = format!("{id:0>64}");
    c
}

/// Insert a capsule + its (single-vector) embedding so the hybrid path has
/// both channels to draw on.
async fn seed(
    store: &PostgresCapsuleStore,
    id: &str,
    content: &str,
    ty: CapabilityCapsuleType,
    vec: &[f32],
) {
    store
        .insert_capability_capsule(capsule_with(id, content, ty))
        .await
        .unwrap();
    store
        .upsert_capability_capsule_embedding(
            id,
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, vec)),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
}

emb_test!(bm25_finds_lexical_match, store, {
    store
        .insert_capability_capsule(capsule_with(
            "lex_hit",
            "configure the embedding batch size knob",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule_with(
            "lex_miss",
            "completely unrelated transcript archive note",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    let hits = store
        .bm25_candidate_ids("t", "embedding batch", 10)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    assert!(
        ids.contains(&"lex_hit"),
        "keyword capsule is recalled: {ids:?}"
    );
    assert!(
        !ids.contains(&"lex_miss"),
        "unrelated capsule not recalled: {ids:?}"
    );
    // ranks are 1-based and contiguous.
    assert_eq!(hits[0].1, 1, "top hit has rank 1");
});

emb_test!(bm25_empty_query_is_empty, store, {
    store
        .insert_capability_capsule(capsule_with(
            "x",
            "anything",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    assert!(store
        .bm25_candidate_ids("t", "   ", 10)
        .await
        .unwrap()
        .is_empty());
});

emb_test!(ann_finds_semantic_neighbor, store, {
    // [1,0,..] nearest to query [1,0,..]; [0.9,0.1,..] second; [0,1,..] last.
    seed(
        &store,
        "near",
        "alpha",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;
    seed(
        &store,
        "mid",
        "beta",
        CapabilityCapsuleType::Experience,
        &[0.9, 0.1],
    )
    .await;
    seed(
        &store,
        "far",
        "gamma",
        CapabilityCapsuleType::Experience,
        &[0.0, 1.0],
    )
    .await;
    let hits = store
        .ann_candidate_ids("t", &vec_of(DIM, &[1.0, 0.0]), 3)
        .await
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["near", "mid", "far"],
        "ANN orders by cosine distance"
    );
    assert_eq!(hits[0].1, 1, "1-based rank");
    assert_eq!(hits[1].1, 2);
});

emb_test!(ann_missing_embeddings_table_is_empty, store, {
    // No embedding ever upserted → table not lazy-created → empty, no error.
    let hits = store
        .ann_candidate_ids("t", &vec_of(DIM, &[1.0, 0.0]), 5)
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "missing embeddings table short-circuits to empty"
    );
});

emb_test!(hybrid_fuses_both, store, {
    // lex_only: matches the text query, far embedding.
    // sem_only: no keyword, embedding identical to query.
    // both:     matches text AND near embedding → must rank first by RRF.
    seed(
        &store,
        "lex_only",
        "rust async runtime tokio scheduler",
        CapabilityCapsuleType::Experience,
        &[0.0, 1.0],
    )
    .await;
    seed(
        &store,
        "sem_only",
        "an entirely different unrelated topic",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;
    seed(
        &store,
        "both",
        "rust async runtime done right",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;

    let fused = store
        .hybrid_candidates("t", "rust async runtime", &vec_of(DIM, &[1.0, 0.0]), 10)
        .await
        .unwrap();
    let ids: Vec<&str> = fused
        .iter()
        .map(|(c, _)| c.capability_capsule_id.as_str())
        .collect();
    assert_eq!(
        ids.first(),
        Some(&"both"),
        "dual-channel hit ranks first: {ids:?}"
    );
    assert!(
        ids.contains(&"lex_only") && ids.contains(&"sem_only"),
        "single-channel hits still surface: {ids:?}"
    );
});

emb_test!(hybrid_rrf_score_matches_formula, store, {
    // Two capsules, distinct embeddings, no shared lexical token, query that
    // matches NEITHER lexically → the fused score is purely the ANN channel:
    // each capsule's rrf == 1/(60+rank_sem). Assert that exact value.
    seed(
        &store,
        "v1",
        "qqqqq",
        CapabilityCapsuleType::Experience,
        &[1.0, 0.0],
    )
    .await;
    seed(
        &store,
        "v2",
        "wwwww",
        CapabilityCapsuleType::Experience,
        &[0.0, 1.0],
    )
    .await;
    let fused = store
        .hybrid_candidates("t", "zzzzz", &vec_of(DIM, &[1.0, 0.0]), 10)
        .await
        .unwrap();
    let by_id: std::collections::HashMap<&str, f32> = fused
        .iter()
        .map(|(c, s)| (c.capability_capsule_id.as_str(), *s))
        .collect();
    // v1 is the nearest neighbor (rank_sem 1), v2 is rank_sem 2.
    let expect_rank1 = 1.0_f32 / (60.0 + 1.0);
    let expect_rank2 = 1.0_f32 / (60.0 + 2.0);
    assert!(
        (by_id["v1"] - expect_rank1).abs() < 1e-6,
        "v1 rrf == 1/(60+1): {}",
        by_id["v1"]
    );
    assert!(
        (by_id["v2"] - expect_rank2).abs() < 1e-6,
        "v2 rrf == 1/(60+2): {}",
        by_id["v2"]
    );
    // and the order respects the score.
    assert_eq!(fused[0].0.capability_capsule_id, "v1");
});

emb_test!(search_candidates_respects_status_and_diary, store, {
    store
        .insert_capability_capsule(capsule_with(
            "active1",
            "live capsule",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    store
        .insert_capability_capsule(fixture("arch1", CapabilityCapsuleStatus::Archived))
        .await
        .unwrap();
    store
        .insert_capability_capsule(fixture("rej1", CapabilityCapsuleStatus::Rejected))
        .await
        .unwrap();
    store
        .insert_capability_capsule(capsule_with(
            "diary1",
            "diary entry",
            CapabilityCapsuleType::Diary,
        ))
        .await
        .unwrap();

    let pool = store.search_candidates("t").await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert!(ids.contains(&"active1"), "active capsule in pool: {ids:?}");
    assert!(!ids.contains(&"arch1"), "archived excluded: {ids:?}");
    assert!(!ids.contains(&"rej1"), "rejected excluded: {ids:?}");
    assert!(!ids.contains(&"diary1"), "diary excluded: {ids:?}");
});

emb_test!(search_candidates_dedups_superseded, store, {
    // old <- new(supersedes old, active). search_candidates must drop `old`.
    store
        .insert_capability_capsule(capsule_with(
            "old",
            "version one",
            CapabilityCapsuleType::Experience,
        ))
        .await
        .unwrap();
    let mut new = capsule_with("new", "version two", CapabilityCapsuleType::Experience);
    new.supersedes_capability_capsule_id = Some("old".into());
    store.insert_capability_capsule(new).await.unwrap();

    let pool = store.search_candidates("t").await.unwrap();
    let ids: Vec<&str> = pool
        .iter()
        .map(|c| c.capability_capsule_id.as_str())
        .collect();
    assert!(ids.contains(&"new"), "superseder present: {ids:?}");
    assert!(
        !ids.contains(&"old"),
        "superseded-by-active row dropped: {ids:?}"
    );
});

emb_test!(hybrid_excludes_archived_vec_only_hit, store, {
    // Archived capsule with an embedding that matches the query: the ANN
    // channel finds it, but the outer hydration filter must drop it (same as
    // the Lance hybrid outer WHERE).
    let mut arch = capsule_with("arch_vec", "qqq", CapabilityCapsuleType::Experience);
    arch.status = CapabilityCapsuleStatus::Archived;
    store.insert_capability_capsule(arch).await.unwrap();
    store
        .upsert_capability_capsule_embedding(
            "arch_vec",
            "t",
            "fake",
            DIM as i64,
            &blob_of(&vec_of(DIM, &[1.0, 0.0])),
            "h",
            "s",
            "n",
        )
        .await
        .unwrap();
    let fused = store
        .hybrid_candidates("t", "", &vec_of(DIM, &[1.0, 0.0]), 10)
        .await
        .unwrap();
    let ids: Vec<&str> = fused
        .iter()
        .map(|(c, _)| c.capability_capsule_id.as_str())
        .collect();
    assert!(
        !ids.contains(&"arch_vec"),
        "archived vec-only hit dropped post-fusion: {ids:?}"
    );
});
