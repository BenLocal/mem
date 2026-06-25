//! ClickHouse backend parity smoke — **UNVALIDATED scaffold**
//! (clickhouse-backend P1).
//!
//! Gated twice over:
//! - It only does real work under `--features clickhouse` (the default
//!   build compiles a no-op stub so the test binary still exists).
//! - Even with the feature, it requires `MEM_TEST_CLICKHOUSE_URL`
//!   (e.g. `http://localhost:8123`); when unset every case prints a skip
//!   line and returns `Ok` — there is no local ClickHouse in CI/dev, so
//!   the suite must never fail merely for being un-runnable.
//!
//! When a real ClickHouse is reachable it applies
//! `migrations/clickhouse/0001_capsule_store.sql` and runs a representative
//! subset of the `tests/capsule_store_parity.rs` scenarios against
//! `ClickHouseBackend` (full reuse of that crate's scenarios needs a shared
//! test-helper module — a P-later cleanup).

#[cfg(feature = "clickhouse")]
mod ch {
    use std::sync::Arc;

    use mem::domain::capability_capsule::GraphEdge;
    use mem::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };
    use mem::domain::EntityKind;
    use mem::storage::types::EmbeddingJobInsert;
    use mem::storage::{
        current_timestamp, CapsuleStore, ClickHouseBackend, EmbeddingJobStore,
        EmbeddingVectorStore, EntityRegistry, FeedbackEvent, GraphStore,
    };

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
            content_hash: format!("{:0>64}", id),
            confidence: 0.5,
            decay_score: 0.0,
            source_agent: "test".into(),
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
            ..Default::default()
        }
    }

    /// Connect + migrate, or `None` to skip (URL unset). A fresh
    /// per-run database name keeps parallel test runs isolated would be
    /// ideal; P1 just targets whatever DB the URL names.
    async fn store() -> Option<Arc<dyn CapsuleStore>> {
        let url = std::env::var("MEM_TEST_CLICKHOUSE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(url) = url else {
            eprintln!("MEM_TEST_CLICKHOUSE_URL unset — skipping ClickHouse parity");
            return None;
        };
        let backend = ClickHouseBackend::connect(&url)
            .await
            .expect("clickhouse connect");
        backend
            .apply_migrations()
            .await
            .expect("clickhouse migrate");
        Some(Arc::new(backend))
    }

    #[tokio::test]
    async fn insert_and_get_round_trip() {
        let Some(store) = store().await else { return };
        store
            .insert_capability_capsule(fixture("ch_a", CapabilityCapsuleStatus::Active))
            .await
            .unwrap();
        let got = store
            .get_capability_capsule_for_tenant("t", "ch_a")
            .await
            .unwrap()
            .expect("round-trip row");
        assert_eq!(got.capability_capsule_id, "ch_a");
        assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    }

    #[tokio::test]
    async fn accept_pending_transitions_status() {
        let Some(store) = store().await else { return };
        store
            .insert_capability_capsule(fixture(
                "ch_pending",
                CapabilityCapsuleStatus::PendingConfirmation,
            ))
            .await
            .unwrap();
        let updated = store.accept_pending("t", "ch_pending").await.unwrap();
        assert_eq!(updated.status, CapabilityCapsuleStatus::Active);
        // The latest version read should also reflect Active (ReplacingMergeTree).
        let got = store
            .get_capability_capsule_for_tenant("t", "ch_pending")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.status, CapabilityCapsuleStatus::Active);
    }

    #[tokio::test]
    async fn find_by_idempotency_dedups_on_hash() {
        let Some(store) = store().await else { return };
        let row = fixture("ch_hash", CapabilityCapsuleStatus::Active);
        let hash = row.content_hash.clone();
        store.insert_capability_capsule(row).await.unwrap();
        let found = store
            .find_by_idempotency_or_hash("t", &None, &hash)
            .await
            .unwrap();
        assert_eq!(
            found.map(|r| r.capability_capsule_id),
            Some("ch_hash".to_string())
        );
    }

    /// Concrete backend (impls every sub-trait) so a test can reach methods
    /// off `CapsuleStore` — e.g. `EmbeddingVectorStore` (P3). `None` to skip.
    async fn ch_backend() -> Option<ClickHouseBackend> {
        let url = std::env::var("MEM_TEST_CLICKHOUSE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let Some(url) = url else {
            eprintln!("MEM_TEST_CLICKHOUSE_URL unset — skipping ClickHouse embedding parity");
            return None;
        };
        let backend = ClickHouseBackend::connect(&url)
            .await
            .expect("clickhouse connect");
        backend
            .apply_migrations()
            .await
            .expect("clickhouse migrate");
        Some(backend)
    }

    /// P3: a capsule embedding upserts and round-trips its vector + metadata,
    /// and `delete` clears it.
    #[tokio::test]
    async fn embedding_vector_round_trip_and_delete() {
        let Some(be) = ch_backend().await else { return };
        let v = vec![0.1_f32, 0.2, 0.3, 0.4];
        be.upsert_capability_capsule_embedding_chunks(
            "ch_emb",
            "t",
            "test-model",
            4,
            std::slice::from_ref(&v),
            "hash_emb",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();
        assert_eq!(
            be.get_capability_capsule_embedding_vector("ch_emb")
                .await
                .unwrap(),
            Some(v)
        );
        assert_eq!(
            be.get_capability_capsule_embedding_row("ch_emb")
                .await
                .unwrap()
                .map(|(m, h, _)| (m, h)),
            Some(("test-model".to_string(), "hash_emb".to_string()))
        );
        be.delete_capability_capsule_embedding("ch_emb")
            .await
            .unwrap();
        // Mutations are async in ClickHouse; the delete is best-effort here —
        // we only assert it doesn't error. (A post-delete absence check would
        // need to wait out the mutation; deferred to the validation pass.)
    }

    /// P3: a 2-chunk conversation embedding upserts without error (the
    /// chunk_index discriminator lets both rows coexist under
    /// ReplacingMergeTree; the full chunk-set read is P5's search).
    #[tokio::test]
    async fn conversation_chunks_upsert_ok() {
        let Some(be) = ch_backend().await else { return };
        let vs = vec![vec![1.0_f32, 0.0], vec![0.0_f32, 1.0]];
        be.upsert_conversation_message_embedding_chunks(
            "msg_x",
            "t",
            "test-model",
            2,
            &vs,
            "hash_msg",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();
        be.delete_conversation_message_embedding("msg_x")
            .await
            .unwrap();
    }

    /// P4: seed 2 capsules + embeddings → `ann_candidate_ids` ranks by the
    /// query vector (chunk-collapse + tenant postfilter), `bm25_candidate_ids`
    /// substring-matches the coarse lexical channel, and
    /// `hybrid_candidates_compose` fuses both via Rust RRF.
    #[tokio::test]
    async fn hybrid_search_ann_bm25_compose() {
        use mem::storage::CapsuleSearchStore;
        let Some(be) = ch_backend().await else { return };

        let mut a = fixture("ch_s_a", CapabilityCapsuleStatus::Active);
        a.content = "alpha vector database".into();
        let mut b = fixture("ch_s_b", CapabilityCapsuleStatus::Active);
        b.content = "beta graph store".into();
        be.insert_capability_capsule(a).await.unwrap();
        be.insert_capability_capsule(b).await.unwrap();

        let va = vec![1.0_f32, 0.0, 0.0];
        let vb = vec![0.0_f32, 1.0, 0.0];
        be.upsert_capability_capsule_embedding_chunks(
            "ch_s_a",
            "t",
            "m",
            3,
            std::slice::from_ref(&va),
            "ha",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();
        be.upsert_capability_capsule_embedding_chunks(
            "ch_s_b",
            "t",
            "m",
            3,
            std::slice::from_ref(&vb),
            "hb",
            &current_timestamp(),
            &current_timestamp(),
        )
        .await
        .unwrap();

        // ANN: query near `va` → ch_s_a ranks first.
        let ann = be.ann_candidate_ids("t", &va, 5).await.unwrap();
        assert_eq!(ann.first().map(|(id, _)| id.as_str()), Some("ch_s_a"));

        // BM25 (coarse substring): "graph" → ch_s_b is a candidate.
        let bm25 = be.bm25_candidate_ids("t", "graph", 5).await.unwrap();
        assert!(bm25.iter().any(|(id, _)| id == "ch_s_b"));

        // Hybrid compose: text + vec → non-empty fused result, ch_s_a on top.
        let hybrid = be
            .hybrid_candidates_compose("t", "vector", &va, 5)
            .await
            .unwrap();
        assert_eq!(
            hybrid
                .first()
                .map(|(r, _)| r.capability_capsule_id.as_str()),
            Some("ch_s_a")
        );
    }

    #[tokio::test]
    async fn feedback_summary_counts_kinds() {
        let Some(store) = store().await else { return };
        let row = fixture("ch_fb", CapabilityCapsuleStatus::Active);
        store.insert_capability_capsule(row.clone()).await.unwrap();
        store
            .apply_feedback(
                &row,
                FeedbackEvent {
                    feedback_id: "fb1".into(),
                    capability_capsule_id: "ch_fb".into(),
                    feedback_kind: "useful".into(),
                    created_at: current_timestamp(),
                    note: None,
                },
            )
            .await
            .unwrap();
        let summary = store.feedback_summary("ch_fb").await.unwrap();
        assert_eq!(summary.useful, 1);
        assert_eq!(summary.total, 1);
    }

    fn edge(from: &str, to: &str, rel: &str) -> GraphEdge {
        GraphEdge {
            from_node_id: from.into(),
            to_node_id: to.into(),
            relation: rel.into(),
            valid_from: current_timestamp(),
            valid_to: None,
            confidence: Some(1.0),
            extractor: Some("test".into()),
            strength: None,
            stability: None,
            last_activated: None,
            access_count: None,
        }
    }

    /// P5 GraphStore: sync edges, then `neighbors` finds the incident active edge.
    #[tokio::test]
    async fn graph_sync_and_neighbors() {
        let Some(be) = ch_backend().await else { return };
        let now = current_timestamp();
        be.sync_memory_edges(&[edge("a", "b", "rel:x")], &now)
            .await
            .unwrap();
        let n = be.neighbors("a").await.unwrap();
        assert!(n
            .iter()
            .any(|e| e.to_node_id == "b" && e.relation == "rel:x"));
    }

    /// P5 EntityRegistry: resolve creates an entity; a second resolve of the
    /// same alias returns the same id; get_entity surfaces it.
    #[tokio::test]
    async fn entity_resolve_is_idempotent() {
        let Some(be) = ch_backend().await else { return };
        let now = current_timestamp();
        let id1 = be
            .resolve_or_create("t", "Rust", EntityKind::Topic, &now)
            .await
            .unwrap();
        let id2 = be
            .resolve_or_create("t", "rust", EntityKind::Topic, &now)
            .await
            .unwrap();
        assert_eq!(id1, id2);
        let got = be.get_entity("t", &id1).await.unwrap().unwrap();
        assert_eq!(got.entity.canonical_name, "Rust");
    }

    /// P5 EmbeddingJobStore: enqueue → claim → complete moves the status.
    #[tokio::test]
    async fn embedding_job_enqueue_claim_complete() {
        let Some(be) = ch_backend().await else { return };
        let now = current_timestamp();
        let ok = be
            .try_enqueue_embedding_job(EmbeddingJobInsert {
                job_id: "j1".into(),
                tenant: "t".into(),
                capability_capsule_id: "cap1".into(),
                target_content_hash: "h1".into(),
                provider: "fake".into(),
                available_at: now.clone(),
                created_at: now.clone(),
                updated_at: now.clone(),
            })
            .await
            .unwrap();
        assert!(ok);
        let claimed = be.claim_next_n_embedding_jobs(&now, 3, 10).await.unwrap();
        assert!(claimed.iter().any(|c| c.job_id == "j1"));
        be.complete_embedding_job("j1", &now).await.unwrap();
        assert_eq!(
            be.get_embedding_job_status("j1").await.unwrap(),
            Some("completed".to_string())
        );
    }
}

/// Default build (no `clickhouse` feature): keep a test binary present so
/// `cargo test` lists the file, and explain how to run the real suite.
#[cfg(not(feature = "clickhouse"))]
#[test]
fn clickhouse_parity_requires_feature() {
    if std::env::var("MEM_TEST_CLICKHOUSE_URL").is_ok() {
        eprintln!(
            "MEM_TEST_CLICKHOUSE_URL is set but mem was built without --features clickhouse — \
             skipping ClickHouse parity (rebuild with the feature to run it)"
        );
    }
}
