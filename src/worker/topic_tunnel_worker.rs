//! Topic-tunnel auto-derivation worker — mempalace's
//! `compute_topic_tunnels` analogue, adapted to mem's edge-first KG.
//!
//! ## Why this exists
//!
//! mem's KG is well-populated with **caller-curated** edges (ingest
//! pipeline `tagged` / `mentions_file` / `extracted_from`, plus
//! explicit `kg_add_edge` / `user_tunnel:` from agents). What it
//! lacks until now: **auto-discovered cross-project connections**.
//! If project A and project B both write about "rust" + "lance" + "ann",
//! today the only signal is at search-time scoring; there's no
//! first-class graph edge saying "these two projects share territory."
//!
//! This worker creates that edge. Periodically, for one tenant, it:
//!
//! 1. Pulls active capsules (capped at `scan_limit`).
//! 2. Groups them by `project`; for each project, builds a set of
//!    topic entity ids (resolved via the entity registry).
//! 3. For each pair of projects, computes the topic intersection.
//! 4. If the intersection size meets `min_count`, creates a
//!    `user_tunnel:topic:<topic-name>` edge between the two project
//!    entities for each shared topic.
//!
//! Edges use the `user_tunnel:` prefix so they surface naturally via
//! `kg_list_user_tunnels` (v2 #20 phase A). The `:topic:` infix
//! signals "auto-derived" — operators can filter
//! `relation LIKE 'user_tunnel:topic:%'` to see only the worker's
//! output vs caller-curated tunnels.
//!
//! ## Idempotency
//!
//! `add_edge_direct` is idempotent on the active `(from, to, relation)`
//! triple — a second run produces zero new edges if nothing changed.
//! Safe to run repeatedly; safe to ship default-OFF (operators turn on
//! once they've decided on min_count).
//!
//! ## What this does NOT do
//!
//! - Close stale tunnels when projects stop sharing topics. (Topic
//!   tunnels accumulate; explicit invalidation via `kg_invalidate_edge`
//!   remains the only retirement path. A future enhancement could
//!   close edges whose underlying overlap has dropped below
//!   `min_count`, but the "stale tunnel" semantics need design first.)
//! - Cross-tenant tunnels. Each sweep is single-tenant.
//! - Operate on `tag` entities — only `topic` strings are used. Tags
//!   already produce `tagged` edges directly via ingest, so they have
//!   their own graph signal.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::config::TopicTunnelSettings;
use crate::domain::capability_capsule::{CapabilityCapsuleStatus, GraphEdge};
use crate::domain::EntityKind;
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, CapsuleSearchStore};

/// Long-running loop. No-op when `settings.enabled == false`.
pub async fn run(store: Arc<dyn Backend>, settings: TopicTunnelSettings, tenant: String) {
    if !settings.enabled {
        return;
    }
    let interval = Duration::from_secs(settings.interval_secs);
    info!(
        interval_secs = settings.interval_secs,
        min_count = settings.min_count,
        scan_limit = settings.scan_limit,
        tenant = %tenant,
        "topic_tunnel_worker started",
    );
    loop {
        sleep(interval).await;
        match sweep_once(&*store, &settings, &tenant).await {
            Ok(created) => {
                if !created.is_empty() {
                    info!(
                        count = created.len(),
                        tenant = %tenant,
                        "topic_tunnel: created {} new auto-tunnel(s)",
                        created.len(),
                    );
                }
            }
            Err(e) => warn!(error = %e, tenant = %tenant, "topic_tunnel sweep failed"),
        }
    }
}

/// One sweep pass. Returns the relation strings created (or that would
/// be created — see `sync` mode). Extracted so tests + a future admin
/// HTTP route can drive the same logic without spinning up the loop.
///
/// Idempotency: re-running with no underlying changes returns
/// `Vec::new()` because `add_edge_direct` short-circuits on existing
/// active edges.
pub async fn sweep_once(
    store: &dyn Backend,
    settings: &TopicTunnelSettings,
    tenant: &str,
) -> Result<Vec<String>, StorageError> {
    // 1. Pull active capsules in this tenant.
    let ids = store.list_capability_capsule_ids_for_tenant(tenant).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_slice: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .take(settings.scan_limit)
        .collect();
    let capsules =
        CapsuleSearchStore::fetch_capability_capsules_by_ids(store, tenant, &id_slice).await?;

    // 2. Build map: project_name -> Set<topic_entity_id>. Capsules
    //    without a project can't form project-pair tunnels — skip.
    //    Topics that aren't already registered as entities (no
    //    lookup_alias hit) get registered now via resolve_or_create
    //    under EntityKind::Topic so subsequent sweeps see the same id.
    let now = current_timestamp();
    let mut by_project: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    for capsule in capsules {
        if capsule.status != CapabilityCapsuleStatus::Active {
            continue;
        }
        let Some(project) = capsule.project.clone() else {
            continue;
        };
        if capsule.topics.is_empty() {
            continue;
        }
        let project_set = by_project.entry(project).or_default();
        for topic in &capsule.topics {
            if topic.trim().is_empty() {
                continue;
            }
            let topic_id = store
                .resolve_or_create(tenant, topic, EntityKind::Topic, &now)
                .await?;
            project_set.insert(topic_id);
        }
    }

    if by_project.len() < 2 {
        // Need at least two projects to form a tunnel pair.
        return Ok(Vec::new());
    }

    // 3. For each ordered project pair, compute intersection +
    //    create edges. Project names are sorted to make the
    //    `(from, to)` direction deterministic across runs (otherwise
    //    re-runs might create the same logical tunnel in both
    //    directions).
    let mut projects: Vec<&String> = by_project.keys().collect();
    projects.sort();

    // Resolve each project name to its entity id once. The ingest
    // pipeline normally creates these on first capsule write, but a
    // tenant with capsules written before the entity registry rolled
    // out might be missing — resolve_or_create is idempotent.
    let mut project_entity_ids: HashMap<String, String> = HashMap::new();
    for project in &projects {
        let id = store
            .resolve_or_create(tenant, project, EntityKind::Project, &now)
            .await?;
        project_entity_ids.insert((*project).clone(), id);
    }

    let mut created = Vec::new();
    for i in 0..projects.len() {
        for j in (i + 1)..projects.len() {
            let proj_a = projects[i];
            let proj_b = projects[j];
            let topics_a = &by_project[proj_a];
            let topics_b = &by_project[proj_b];
            let shared: Vec<&String> = topics_a.intersection(topics_b).collect();
            if shared.len() < settings.min_count {
                continue;
            }
            let from_node = format!("entity:{}", project_entity_ids[proj_a]);
            let to_node = format!("entity:{}", project_entity_ids[proj_b]);
            for topic_id in shared {
                // Resolve topic entity_id back to its canonical name
                // for a human-readable relation string. Fall back to
                // the id if lookup fails (rare — registry just wrote
                // it).
                let topic_name = match store.get_entity(tenant, topic_id).await? {
                    Some(e) => e.entity.canonical_name,
                    None => topic_id.clone(),
                };
                let relation = format!("user_tunnel:topic:{topic_name}");
                let edge = GraphEdge {
                    from_node_id: from_node.clone(),
                    to_node_id: to_node.clone(),
                    relation: relation.clone(),
                    valid_from: now.clone(),
                    valid_to: None,
                    confidence: None,
                    extractor: Some("topic_tunnel".into()),
                    strength: None,
                    stability: None,
                    last_activated: None,
                    access_count: None,
                };
                match store.add_edge_direct(&edge).await {
                    Ok(true) => created.push(relation),
                    Ok(false) => {
                        debug!(
                            from = %from_node, to = %to_node, relation = %relation,
                            "topic_tunnel: edge already active, skipping",
                        );
                    }
                    Err(e) => warn!(
                        from = %from_node, to = %to_node, relation = %relation,
                        error = %e,
                        "topic_tunnel: add_edge_direct failed, continuing",
                    ),
                }
            }
        }
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleRecord, CapabilityCapsuleType, Scope, Visibility,
    };
    use crate::storage::Store;
    use tempfile::tempdir;

    const TENANT: &str = "local";

    fn capsule(id: &str, project: &str, topics: &[&str]) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: TENANT.into(),
            capability_capsule_type: CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Repo,
            visibility: Visibility::Shared,
            version: 1,
            summary: format!("summary-{id}"),
            content: format!("content-{id}"),
            evidence: vec![],
            code_refs: vec![],
            project: Some(project.into()),
            repo: None,
            module: None,
            task_type: None,
            tags: vec![],
            topics: topics.iter().map(|t| (*t).to_string()).collect(),
            confidence: 0.9,
            decay_score: 0.0,
            content_hash: format!("hash-{id}"),
            idempotency_key: None,
            session_id: None,
            supersedes_capability_capsule_id: None,
            source_agent: "test".into(),
            created_at: "00000000000000000001".into(),
            updated_at: "00000000000000000001".into(),
            last_validated_at: None,
            last_used_at: None,
        }
    }

    fn settings(min_count: usize) -> TopicTunnelSettings {
        TopicTunnelSettings {
            enabled: true,
            interval_secs: 3_600,
            min_count,
            scan_limit: 1_000,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_projects_with_overlap_above_min_count_get_tunnels() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("tt.lance")).await.unwrap();

        // project-a topics: rust, lance, ann
        // project-b topics: rust, lance, postgres
        // shared: {rust, lance} → 2 topics, meets min_count=2
        store
            .insert_capability_capsule(capsule("a1", "phoenix", &["rust", "lance", "ann"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("b1", "atlas", &["rust", "lance", "postgres"]))
            .await
            .unwrap();

        let created = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert_eq!(
            created.len(),
            2,
            "expected 2 tunnels (one per shared topic), got {created:?}"
        );
        assert!(created.iter().any(|r| r == "user_tunnel:topic:rust"));
        assert!(created.iter().any(|r| r == "user_tunnel:topic:lance"));

        // K3 (closes mempalace-diff-v3 K3): worker-produced tunnel edges
        // carry the `topic_tunnel` provenance tag so operators can tell
        // auto-derived edges from caller-curated ones.
        let tunnels = store.list_user_tunnels(100).await.unwrap();
        assert!(
            !tunnels.is_empty()
                && tunnels
                    .iter()
                    .all(|e| e.extractor.as_deref() == Some("topic_tunnel")),
            "topic-tunnel worker edges must be tagged extractor=topic_tunnel: {tunnels:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_topic_overlap_below_default_threshold_creates_nothing() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("tt.lance")).await.unwrap();

        // Only one shared topic — below min_count=2.
        store
            .insert_capability_capsule(capsule("a1", "phoenix", &["rust", "lance"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("b1", "atlas", &["rust", "postgres"]))
            .await
            .unwrap();

        let created = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert!(
            created.is_empty(),
            "single-topic overlap should not tunnel: {created:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn min_count_one_allows_single_topic_overlap() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("tt.lance")).await.unwrap();
        store
            .insert_capability_capsule(capsule("a1", "phoenix", &["rust"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("b1", "atlas", &["rust"]))
            .await
            .unwrap();

        let created = sweep_once(&store, &settings(1), TENANT).await.unwrap();
        assert_eq!(created, vec!["user_tunnel:topic:rust".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn second_run_is_idempotent() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("tt.lance")).await.unwrap();
        store
            .insert_capability_capsule(capsule("a1", "phoenix", &["rust", "lance"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("b1", "atlas", &["rust", "lance"]))
            .await
            .unwrap();

        let first = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert_eq!(first.len(), 2);
        let second = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert!(
            second.is_empty(),
            "second sweep must create no new edges (active triple exists): {second:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn capsule_without_project_is_skipped() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("tt.lance")).await.unwrap();

        let mut a = capsule("a1", "phoenix", &["rust", "lance"]);
        let mut b_unscoped = capsule("b1", "irrelevant", &["rust", "lance"]);
        b_unscoped.project = None;

        store.insert_capability_capsule(a.clone()).await.unwrap();
        store
            .insert_capability_capsule(b_unscoped.clone())
            .await
            .unwrap();

        // Only one project → no pair → no tunnels.
        let created = sweep_once(&store, &settings(1), TENANT).await.unwrap();
        assert!(
            created.is_empty(),
            "single project should not tunnel: {created:?}"
        );

        // Add a real second project and re-sweep — now we expect tunnels.
        a.capability_capsule_id = "a2".into();
        a.content_hash = "hash-a2".into();
        a.project = Some("atlas".into());
        store.insert_capability_capsule(a).await.unwrap();
        let created2 = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert!(
            !created2.is_empty(),
            "after second project added, tunnels should fire: {created2:?}",
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn three_projects_form_all_pairs() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("tt.lance")).await.unwrap();

        // All three projects share {rust, lance} → 3 pairs × 2 topics = 6 tunnels
        store
            .insert_capability_capsule(capsule("a1", "phoenix", &["rust", "lance"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("b1", "atlas", &["rust", "lance"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("c1", "delta", &["rust", "lance"]))
            .await
            .unwrap();

        let created = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert_eq!(
            created.len(),
            6,
            "3 pairs × 2 topics = 6 tunnels, got {created:?}"
        );
    }
}
