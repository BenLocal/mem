//! K10 entity co-occurrence worker — mempalace's within-wing "hallway"
//! analogue, adapted to mem's edge-first KG (closes mempalace-diff-v4 K10).
//!
//! ## Why this exists
//!
//! `topic_tunnel_worker` (K2) links *projects* that share topics. This
//! worker links *entities* that travel together: within one project, if
//! two entities co-occur in `>= min_count` of that project's active
//! capsules, it writes a `cooccurs_with` edge between them. That makes
//! "what concepts cluster together inside this project" a first-class
//! graph fact instead of an implicit search-time signal.
//!
//! Per sweep, for one tenant:
//! 1. Pull active capsules (capped at `scan_limit`).
//! 2. Group by `project`; for each capsule resolve its `topics` to entity
//!    ids (via the registry, `EntityKind::Topic`), forming a per-capsule
//!    entity set.
//! 3. Per project, count co-occurrence of every entity pair across its
//!    capsules' sets.
//! 4. For pairs at/above `min_count`, write a `cooccurs_with` edge
//!    (`extractor = "cooccurrence"`) between the two entity nodes,
//!    direction fixed by sorted id for determinism.
//!
//! ## Recall note
//!
//! The current retrieve graph expansion is **1-hop** (capsule → entity →
//! capsule), so these entity↔entity edges are not traversed by the 1-hop
//! recall boost. They enrich the KG for `kg_query` / multi-hop traversal;
//! turning them into a retrieve-recall lift needs multi-hop expansion.
//!
//! ## Idempotency / safety
//!
//! `add_edge_direct` short-circuits on an existing active
//! `(from, to, relation)` triple, so re-runs produce zero new edges when
//! nothing changed. Default OFF (`MEM_COOCCURRENCE_ENABLED`); single
//! tenant per sweep; does not close edges whose overlap later drops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::config::CooccurrenceSettings;
use crate::domain::capability_capsule::{CapabilityCapsuleStatus, GraphEdge};
use crate::domain::EntityKind;
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, CapsuleSearchStore};

/// Long-running loop. No-op when `settings.enabled == false`.
pub async fn run(store: Arc<dyn Backend>, settings: CooccurrenceSettings, tenant: String) {
    if !settings.enabled {
        return;
    }
    let interval = Duration::from_secs(settings.interval_secs.max(1));
    info!(
        interval_secs = settings.interval_secs,
        min_count = settings.min_count,
        "cooccurrence worker started"
    );
    loop {
        sleep(interval).await;
        match sweep_once(store.as_ref(), &settings, &tenant).await {
            Ok(created) if !created.is_empty() => {
                debug!(count = created.len(), "cooccurrence: created edges")
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "cooccurrence sweep failed"),
        }
    }
}

/// One sweep. Returns the `from->to` descriptors of the edges created
/// (empty when nothing crossed `min_count` or everything already existed).
/// Pulled out of [`run`] so a test / HTTP route can drive it directly.
pub async fn sweep_once(
    store: &dyn Backend,
    settings: &CooccurrenceSettings,
    tenant: &str,
) -> Result<Vec<String>, StorageError> {
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

    // project -> one entity-id set per active capsule (need >= 2 entities
    // to form a pair, so smaller capsules are skipped).
    let now = current_timestamp();
    let mut by_project: HashMap<String, Vec<HashSet<String>>> = HashMap::new();
    for capsule in capsules {
        if capsule.status != CapabilityCapsuleStatus::Active {
            continue;
        }
        let Some(project) = capsule.project.clone() else {
            continue;
        };
        if capsule.topics.len() < 2 {
            continue;
        }
        let mut ent_set: HashSet<String> = HashSet::new();
        for topic in &capsule.topics {
            if topic.trim().is_empty() {
                continue;
            }
            let id = store
                .resolve_or_create(tenant, topic, EntityKind::Topic, &now)
                .await?;
            ent_set.insert(id);
        }
        if ent_set.len() >= 2 {
            by_project.entry(project).or_default().push(ent_set);
        }
    }

    let mut created = Vec::new();
    for capsule_sets in by_project.values() {
        // Count co-occurrence of every entity pair across this project's
        // capsules.
        let mut pair_counts: HashMap<(String, String), usize> = HashMap::new();
        for ent_set in capsule_sets {
            let mut ents: Vec<&String> = ent_set.iter().collect();
            ents.sort();
            for i in 0..ents.len() {
                for j in (i + 1)..ents.len() {
                    *pair_counts
                        .entry((ents[i].clone(), ents[j].clone()))
                        .or_insert(0) += 1;
                }
            }
        }
        for ((a, b), count) in pair_counts {
            if count < settings.min_count {
                continue;
            }
            let from_node = format!("entity:{a}");
            let to_node = format!("entity:{b}");
            let edge = GraphEdge {
                from_node_id: from_node.clone(),
                to_node_id: to_node.clone(),
                relation: "cooccurs_with".into(),
                valid_from: now.clone(),
                valid_to: None,
                confidence: None,
                extractor: Some("cooccurrence".into()),
                strength: None,
                stability: None,
                last_activated: None,
                access_count: None,
            };
            match store.add_edge_direct(&edge).await {
                Ok(true) => created.push(format!("{from_node}->{to_node}")),
                Ok(false) => debug!(
                    from = %from_node, to = %to_node,
                    "cooccurrence: edge already active, skipping",
                ),
                Err(e) => warn!(
                    from = %from_node, to = %to_node, error = %e,
                    "cooccurrence: add_edge_direct failed, continuing",
                ),
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
            last_recalled_at: None,
        }
    }

    fn settings(min_count: usize) -> CooccurrenceSettings {
        CooccurrenceSettings {
            enabled: true,
            interval_secs: 3_600,
            min_count,
            scan_limit: 1_000,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn entity_pair_co_occurring_above_min_count_gets_edge() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("co.lance")).await.unwrap();
        // Two capsules in the same project, both tagging {rust, lance}
        // → the (rust, lance) pair co-occurs in 2 capsules ≥ min_count 2.
        store
            .insert_capability_capsule(capsule("c1", "phoenix", &["rust", "lance"]))
            .await
            .unwrap();
        store
            .insert_capability_capsule(capsule("c2", "phoenix", &["rust", "lance"]))
            .await
            .unwrap();

        let created = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert_eq!(
            created.len(),
            1,
            "one (rust,lance) cooccurrence edge expected, got {created:?}"
        );
        assert!(created[0].starts_with("entity:") && created[0].contains("->entity:"));

        // Idempotent: a second sweep with no changes creates nothing.
        let again = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert!(
            again.is_empty(),
            "second sweep must be idempotent: {again:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn single_capsule_pair_below_min_count_creates_nothing() {
        let dir = tempdir().unwrap();
        let store = Store::open(&dir.path().join("co.lance")).await.unwrap();
        // Only one capsule has {rust, lance} → pair count 1 < min_count 2.
        store
            .insert_capability_capsule(capsule("c1", "phoenix", &["rust", "lance"]))
            .await
            .unwrap();

        let created = sweep_once(&store, &settings(2), TENANT).await.unwrap();
        assert!(
            created.is_empty(),
            "single-capsule pair is below threshold: {created:?}"
        );
    }
}
