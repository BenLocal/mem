//! Near-duplicate sweep — closes mempalace-diff-v3 #30.
//!
//! Periodically scans active capsules in one tenant, groups them by
//! `(source_agent, project, repo)`, computes pairwise cosine on their
//! embeddings within each group, and soft-deletes redundant members.
//!
//! Why this exists: transcript mining (`mem mine`) can land the same
//! block twice if a session is mined a second time without
//! incremental cursor support. Exact-content dedup via `content_hash`
//! and `idempotency_key` catches the bit-identical case; this worker
//! catches near-identical variants (whitespace edits, model
//! re-summarizations) that share embedding space but not hash.
//!
//! Default OFF — see [`DedupSettings`] — because the worker archives
//! rows (irreversible without manual undo). Opt in via
//! `MEM_DEDUP_ENABLED=1`. Mempalace's `dedup.py` analogue.
//!
//! **Tenant scope** mirrors `auto_promote_worker`: single tenant per
//! worker instance (env-driven). Multi-tenant fan-out is a future
//! revision when the use case shows up.
//!
//! ## Algorithm (one sweep)
//!
//! 1. List active capsule ids for `tenant` (capped at `scan_limit`).
//! 2. Hydrate metadata via [`CapsuleSearchStore::fetch_capability_capsules_by_ids`].
//! 3. For each capsule, fetch its embedding vector
//!    ([`EmbeddingVectorStore::get_capability_capsule_embedding_vector`]).
//!    Capsules without an embedding yet (worker hasn't processed them)
//!    are skipped.
//! 4. Group by `(source_agent, project, repo)`. Within each group:
//!    - Build clusters by union-find on pair cosine ≥ `threshold`.
//!    - In each cluster with ≥2 members, keep the one with the
//!      longest `content` (ties broken by oldest `created_at`) and
//!      archive the rest via `apply_feedback(FeedbackKind::Incorrect)`.
//! 5. Return the archived ids (or, when `dry_run=true`, the would-be
//!    archived ids without writing).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::DedupSettings;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, FeedbackKind,
};
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, CapsuleSearchStore, FeedbackEvent};

/// Long-running loop. Returns immediately when
/// `settings.enabled == false`. Spawned by `app::AppState::from_config`
/// only when `enabled` is true; the guard is here too so a future
/// caller spawning unconditionally still no-ops.
pub async fn run(store: Arc<dyn Backend>, settings: DedupSettings, tenant: String) {
    if !settings.enabled {
        return;
    }
    let interval = Duration::from_secs(settings.interval_secs);
    info!(
        interval_secs = settings.interval_secs,
        threshold = settings.threshold,
        scan_limit = settings.scan_limit,
        tenant = %tenant,
        "dedup_worker started",
    );
    loop {
        sleep(interval).await;
        match sweep_once(&*store, &settings, &tenant, /* dry_run */ false).await {
            Ok(archived) => {
                if !archived.is_empty() {
                    info!(
                        count = archived.len(),
                        tenant = %tenant,
                        "dedup: archived {} near-duplicate capsule(s)",
                        archived.len(),
                    );
                }
            }
            Err(e) => warn!(error = %e, tenant = %tenant, "dedup sweep failed"),
        }
    }
}

/// One sweep pass. Returns the ids that were (or, when `dry_run`,
/// would be) archived. Extracted from [`run`] so tests + a future
/// admin HTTP route can drive the same logic.
pub async fn sweep_once(
    store: &dyn Backend,
    settings: &DedupSettings,
    tenant: &str,
    dry_run: bool,
) -> Result<Vec<String>, StorageError> {
    // 1. Candidate ids — capsule status is filtered after hydration
    //    because the id-only listing doesn't carry status.
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
    let active: Vec<CapabilityCapsuleRecord> = capsules
        .into_iter()
        .filter(|c| c.status == CapabilityCapsuleStatus::Active)
        .collect();
    if active.len() < 2 {
        return Ok(Vec::new());
    }

    // 2. Group by (source_agent, project, repo). project / repo may be
    //    None — we treat None like a distinct value so capsules with
    //    no scope don't accidentally cluster with scoped ones.
    let mut groups: HashMap<GroupKey, Vec<usize>> = HashMap::new();
    for (idx, c) in active.iter().enumerate() {
        let key = GroupKey {
            source_agent: c.source_agent.clone(),
            project: c.project.clone(),
            repo: c.repo.clone(),
        };
        groups.entry(key).or_default().push(idx);
    }

    // 3. Per-group cluster + archive.
    let mut archived = Vec::new();
    let now = current_timestamp();
    for (_, idxs) in groups {
        if idxs.len() < 2 {
            continue;
        }
        // Embedding fetch only for members of multi-element groups —
        // singletons can't have duplicates.
        let mut vectors: Vec<(usize, Vec<f32>)> = Vec::with_capacity(idxs.len());
        for idx in &idxs {
            let id = &active[*idx].capability_capsule_id;
            match store.get_capability_capsule_embedding_vector(id).await? {
                Some(v) if !v.is_empty() => vectors.push((*idx, v)),
                _ => {} // skip — no embedding yet, nothing to compare
            }
        }
        if vectors.len() < 2 {
            continue;
        }

        // Pairwise cosine — small groups so quadratic is fine.
        let clusters = build_clusters(&vectors, settings.threshold);
        for cluster in clusters {
            if cluster.len() < 2 {
                continue;
            }
            // Pick survivor: longest content first, oldest created_at
            // as tiebreaker (oldest = first user encountered this fact).
            let survivor = cluster
                .iter()
                .copied()
                .max_by(|&a, &b| {
                    let ca = &active[a];
                    let cb = &active[b];
                    ca.content
                        .len()
                        .cmp(&cb.content.len())
                        .then_with(|| cb.created_at.cmp(&ca.created_at))
                })
                .expect("cluster non-empty");
            for &loser in &cluster {
                if loser == survivor {
                    continue;
                }
                let capsule = &active[loser];
                let id = capsule.capability_capsule_id.clone();
                if dry_run {
                    archived.push(id);
                    continue;
                }
                let survivor_id = &active[survivor].capability_capsule_id;
                let event = FeedbackEvent {
                    feedback_id: format!("fb_{}", uuid::Uuid::now_v7()),
                    capability_capsule_id: id.clone(),
                    feedback_kind: FeedbackKind::Incorrect.as_str().to_string(),
                    created_at: now.clone(),
                    note: Some(format!(
                        "dedup: near-duplicate of {survivor_id} (cosine ≥ {})",
                        settings.threshold,
                    )),
                };
                match store.apply_feedback(capsule, event).await {
                    Ok(_) => archived.push(id),
                    Err(e) => warn!(
                        capability_capsule_id = %id,
                        error = %e,
                        "dedup: archive failed, continuing",
                    ),
                }
            }
        }
    }
    Ok(archived)
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
struct GroupKey {
    source_agent: String,
    project: Option<String>,
    repo: Option<String>,
}

/// Build clusters of indices whose pair cosine ≥ `threshold`. Simple
/// union-find: pair-wise scan, union on hit, return one Vec per root.
fn build_clusters(vectors: &[(usize, Vec<f32>)], threshold: f32) -> Vec<Vec<usize>> {
    let n = vectors.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] != i {
            let r = find(parent, parent[i]);
            parent[i] = r;
        }
        parent[i]
    }
    for (i, (_, vi)) in vectors.iter().enumerate() {
        for (j, (_, vj)) in vectors.iter().enumerate().skip(i + 1) {
            if cosine(vi, vj) >= threshold {
                let ri = find(&mut parent, i);
                let rj = find(&mut parent, j);
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, (orig_idx, _)) in vectors.iter().enumerate() {
        let r = find(&mut parent, i);
        // Map vector position back to the original `active` index.
        by_root.entry(r).or_default().push(*orig_idx);
    }
    by_root.into_values().collect()
}

/// Standard cosine: dot / (|a| * |b|). Returns 0 on either-vector zero
/// length (no info — treat as "not similar" rather than NaN).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_vectors_is_one() {
        let v = vec![1.0_f32, 2.0, 3.0];
        let c = cosine(&v, &v);
        assert!((c - 1.0).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert_eq!(cosine(&a, &b), 0.0);
    }

    #[test]
    fn cosine_mismatched_or_empty_returns_zero() {
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn build_clusters_groups_pairs_above_threshold() {
        // Three vectors: two nearly identical, one orthogonal.
        let vectors = vec![
            (10_usize, vec![1.0_f32, 0.0]),
            (20_usize, vec![0.99, 0.01]),
            (30_usize, vec![0.0, 1.0]),
        ];
        let clusters = build_clusters(&vectors, 0.9);
        // Three indices, two clusters: {10,20} and {30}.
        assert_eq!(clusters.len(), 2);
        let mut sizes: Vec<usize> = clusters.iter().map(|c| c.len()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![1, 2]);
    }

    #[test]
    fn build_clusters_threshold_separates_pairs() {
        // Same three vectors, but threshold demanding near-perfect match:
        // (1,0) vs (0.99,0.01) cosine ≈ 0.9999 — still grouped.
        let vectors = vec![(1_usize, vec![1.0_f32, 0.0]), (2_usize, vec![0.99, 0.01])];
        let clusters = build_clusters(&vectors, 0.999);
        assert_eq!(clusters.len(), 1, "tight threshold still groups close pair");
    }

    #[test]
    fn build_clusters_loose_threshold_lumps_everything() {
        let vectors = vec![
            (1_usize, vec![1.0_f32, 0.0]),
            (2_usize, vec![0.99, 0.01]),
            (3_usize, vec![0.0, 1.0]),
        ];
        // Threshold 0.0 lumps all three together.
        let clusters = build_clusters(&vectors, -1.0);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 3);
    }
}
