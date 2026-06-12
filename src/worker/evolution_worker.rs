//! Capsule self-evolution sweep — E1 MVP of `docs/evolution-worker.md`
//! (map layer §3 + operators ① merge / ② generalize §4 + anti-jitter
//! gate §3.3).
//!
//! One sweep (= one "cycle" for the K-gate), zero LLM:
//!
//! 1. Load active capsules (capped at `scan_limit`) and their EXISTING
//!    embeddings — capsules not yet embedded, or expired, are skipped.
//! 2. Cluster in embedding space (union-find on pairwise cosine ≥
//!    `cluster_threshold` — the `dedup_worker` skeleton via
//!    `evolution::map::build_clusters`).
//! 3. Detect operator proposals:
//!    - **merge**: within a cluster, members of the same
//!      `(source_agent, project, repo)` group whose pairwise cosine ≥
//!      `merge_threshold`. `Preference` / `Workflow` guidance capsules
//!      are excluded (auto-promote precedent).
//!    - **generalize**: a cluster with ≥ `generalize_min_n` episodic
//!      capsules (`Experience` / `Episode`) sharing ≥ 2 topics with
//!      mean confidence ≥ 0.6.
//! 4. Reconcile proposals against the durable `evolution_candidates`
//!    table (member-set Jaccard ≥ 0.8 = same operation): signal cycles
//!    accumulate evidence, silent cycles decay it, and an operation
//!    executes only after `k_cycles` CONSECUTIVE signal cycles — the
//!    EvoMap-inspired anti-jitter gate. Proposals matching an already
//!    `executed` candidate are suppressed (no flapping re-proposal).
//! 5. Execute gated operations:
//!    - **merge** keeps the longest-content member as canonical (tie:
//!      oldest `created_at`), flips every other member to `Archived`
//!      via `set_capsule_status` (verbatim-safe — the row is kept and
//!      recoverable; deliberately NOT the dedup `Incorrect` path,
//!      which semantically means "wrong"), and writes one
//!      `merged_into` lineage edge per loser (extractor=`evolution`).
//!      NOTE (doc §4① deviation): `supersedes_capability_capsule_id`
//!      is single-valued, so an N-member merge cannot be expressed as
//!      a supersede chain — `merged_into` edges ARE the lineage.
//!    - **generalize** inserts ONE `PendingConfirmation` proposal
//!      capsule whose content is structured raw material from the
//!      `review` synthesis backend (doc §6.2 — no generated prose),
//!      writes `generalizes` lineage edges proposal → each source,
//!      and leaves every source `Active` (a principle complements its
//!      episodes, it never replaces them).
//!
//! **Default OFF** (`MEM_EVOLUTION_ENABLED=1` to opt in). A real
//! (non-dry-run) sweep is a no-op while disabled; the dry-run preview
//! (`POST /reviews/evolution {dry_run:true}`) works regardless and
//! writes NOTHING — no candidates, no status flips, no edges.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::EvolutionSettings;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, GraphEdge,
};
use crate::evolution::map::{
    build_clusters, match_candidate, update_on_signal, update_on_silence, GateDecision,
    GateSettings,
};
use crate::evolution::synthesis::{ReviewSynthesisBackend, SynthesisBackend, SynthesisTask};
use crate::pipeline::ingest::compute_content_hash_from_record;
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, EvolutionCandidate};

/// Mean-confidence floor for ② generalize sources (doc §4② — fixed in
/// E1, not env-tunable).
const GENERALIZE_MIN_CONFIDENCE: f32 = 0.6;
/// Minimum shared (lowercased) topics across a generalize cluster.
const GENERALIZE_MIN_SHARED_TOPICS: usize = 2;

/// Sweep result — serialized verbatim as the HTTP response of
/// `POST /reviews/evolution`.
#[derive(Debug, Serialize)]
pub struct EvolutionReport {
    pub dry_run: bool,
    /// Active capsules considered (post scan-limit).
    pub scanned: usize,
    /// Subset that had a usable embedding (the map population).
    pub embedded: usize,
    /// Cluster count over the map population.
    pub clusters: usize,
    pub proposals: Vec<ProposalPreview>,
    pub executed: Vec<ExecutedOp>,
    /// Candidate ids cancelled this sweep (evidence decayed below the
    /// hysteresis floor).
    pub cancelled: Vec<String>,
}

/// One detected proposal with its gate state after this sweep.
#[derive(Debug, Serialize)]
pub struct ProposalPreview {
    pub op_kind: String,
    pub member_ids: Vec<String>,
    pub members: Vec<MemberPreview>,
    pub evidence: f32,
    pub consecutive_cycles: i64,
    /// `new` (first sighting) | `hold` (gate accumulating) | `execute`.
    pub decision: String,
}

/// Enough capsule context to eyeball a cluster from the HTTP response.
#[derive(Debug, Clone, Serialize)]
pub struct MemberPreview {
    pub capability_capsule_id: String,
    pub summary: String,
    pub project: Option<String>,
    pub capability_capsule_type: String,
}

#[derive(Debug, Serialize)]
pub struct ExecutedOp {
    pub candidate_id: String,
    pub op_kind: String,
    pub member_ids: Vec<String>,
    pub result_capsule_ids: Vec<String>,
}

/// Long-running loop. Returns immediately when `settings.enabled ==
/// false` (spawn guard in `app` checks too — redundancy is deliberate,
/// dedup precedent).
pub async fn run(store: Arc<dyn Backend>, settings: EvolutionSettings, tenant: String) {
    if !settings.enabled {
        return;
    }
    let interval = Duration::from_secs(settings.interval_secs);
    info!(
        interval_secs = settings.interval_secs,
        k_cycles = settings.k_cycles,
        cluster_threshold = settings.cluster_threshold,
        merge_threshold = settings.merge_threshold,
        tenant = %tenant,
        "evolution_worker started",
    );
    loop {
        sleep(interval).await;
        match sweep_once(&*store, &settings, &tenant, /* dry_run */ false).await {
            Ok(report) => {
                if !report.executed.is_empty() || !report.proposals.is_empty() {
                    info!(
                        proposals = report.proposals.len(),
                        executed = report.executed.len(),
                        cancelled = report.cancelled.len(),
                        tenant = %tenant,
                        "evolution sweep done",
                    );
                }
            }
            Err(e) => warn!(error = %e, tenant = %tenant, "evolution sweep failed"),
        }
    }
}

/// One sweep pass. `dry_run=true` computes and reports but writes
/// NOTHING. `dry_run=false` requires `settings.enabled` (no-op
/// otherwise — idle-archive precedent: only the preview bypasses the
/// switch).
pub async fn sweep_once(
    store: &dyn Backend,
    settings: &EvolutionSettings,
    tenant: &str,
    dry_run: bool,
) -> Result<EvolutionReport, StorageError> {
    let mut report = EvolutionReport {
        dry_run,
        scanned: 0,
        embedded: 0,
        clusters: 0,
        proposals: Vec::new(),
        executed: Vec::new(),
        cancelled: Vec::new(),
    };
    if !dry_run && !settings.enabled {
        return Ok(report);
    }
    let now = current_timestamp();

    // 1. Map population: active, unexpired, embedded.
    let live = store.list_capability_capsules_for_tenant(tenant).await?;
    let active: Vec<CapabilityCapsuleRecord> = live
        .into_iter()
        .filter(|c| c.status == CapabilityCapsuleStatus::Active)
        .filter(|c| !matches!(&c.expires_at, Some(e) if e.as_str() <= now.as_str()))
        .take(settings.scan_limit)
        .collect();
    report.scanned = active.len();
    let mut vectors: Vec<(usize, Vec<f32>)> = Vec::with_capacity(active.len());
    for (idx, c) in active.iter().enumerate() {
        match store
            .get_capability_capsule_embedding_vector(&c.capability_capsule_id)
            .await?
        {
            Some(v) if !v.is_empty() => vectors.push((idx, v)),
            _ => {} // not embedded yet — joins the map next sweep
        }
    }
    report.embedded = vectors.len();

    // 2. Cluster.
    let clusters = build_clusters(&vectors, settings.cluster_threshold);
    report.clusters = clusters.len();
    let vector_by_idx: HashMap<usize, &Vec<f32>> = vectors.iter().map(|(i, v)| (*i, v)).collect();

    // 3. Detect proposals.
    let mut proposals: Vec<Proposal> = Vec::new();
    for cluster in &clusters {
        proposals.extend(detect_merge(cluster, &active, &vector_by_idx, settings));
        if let Some(p) = detect_generalize(cluster, &active, settings) {
            proposals.push(p);
        }
    }

    // 4. Reconcile with durable candidates (anti-jitter gate).
    let pending = store
        .list_evolution_candidates(tenant, Some("pending"))
        .await?;
    let executed_history = store
        .list_evolution_candidates(tenant, Some("executed"))
        .await?;
    let gate = GateSettings {
        k_cycles: settings.k_cycles,
        evidence_decay: settings.evidence_decay,
        hysteresis: settings.hysteresis,
    };
    let mut matched_pending = vec![false; pending.len()];

    for proposal in proposals {
        // Suppress re-proposals of already-executed operations (the
        // generalize sources stay Active, so without this the same
        // cluster would re-propose forever).
        if match_candidate(&proposal.op_kind, &proposal.member_ids, &executed_history).is_some() {
            continue;
        }
        let (mut candidate, decision_label, decision) =
            match match_candidate(&proposal.op_kind, &proposal.member_ids, &pending) {
                Some(i) => {
                    matched_pending[i] = true;
                    let mut c = pending[i].clone();
                    let d = update_on_signal(&mut c, &gate, &now);
                    (c, gate_label(d), d)
                }
                None => {
                    let c = EvolutionCandidate {
                        candidate_id: format!("evo_{}", uuid::Uuid::now_v7()),
                        tenant: tenant.to_string(),
                        op_kind: proposal.op_kind.clone(),
                        member_ids: proposal.member_ids.clone(),
                        params: proposal.params.clone(),
                        evidence: 1.0,
                        consecutive_cycles: 1,
                        status: "pending".to_string(),
                        first_proposed_at: now.clone(),
                        last_signal_at: now.clone(),
                        executed_at: None,
                        result_capsule_ids: Vec::new(),
                    };
                    let d = if i64::from(settings.k_cycles) <= 1 {
                        GateDecision::Execute
                    } else {
                        GateDecision::Hold
                    };
                    (c, "new", d)
                }
            };

        report.proposals.push(ProposalPreview {
            op_kind: proposal.op_kind.clone(),
            member_ids: proposal.member_ids.clone(),
            members: proposal.members.clone(),
            evidence: candidate.evidence,
            consecutive_cycles: candidate.consecutive_cycles,
            decision: decision_label.to_string(),
        });

        if dry_run {
            continue;
        }
        if decision == GateDecision::Execute {
            let result_ids = match proposal.op_kind.as_str() {
                "merge" => {
                    execute_merge(store, tenant, &proposal.member_ids, &active, &now).await?
                }
                "generalize" => {
                    execute_generalize(store, tenant, &proposal.member_ids, &active, &now).await?
                }
                other => {
                    warn!(op_kind = other, "unknown evolution op kind — skipping");
                    Vec::new()
                }
            };
            candidate.status = "executed".to_string();
            candidate.executed_at = Some(now.clone());
            candidate.result_capsule_ids = result_ids.clone();
            report.executed.push(ExecutedOp {
                candidate_id: candidate.candidate_id.clone(),
                op_kind: candidate.op_kind.clone(),
                member_ids: candidate.member_ids.clone(),
                result_capsule_ids: result_ids,
            });
        }
        store.upsert_evolution_candidate(candidate).await?;
    }

    // 5. Silent candidates: decay evidence, reset the consecutive
    //    clock, cancel below the hysteresis floor.
    for (i, was_matched) in matched_pending.iter().enumerate() {
        if *was_matched {
            continue;
        }
        let mut c = pending[i].clone();
        let d = update_on_silence(&mut c, &gate);
        if d == GateDecision::Cancel {
            c.status = "cancelled".to_string();
            report.cancelled.push(c.candidate_id.clone());
        }
        if !dry_run {
            store.upsert_evolution_candidate(c).await?;
        }
    }

    Ok(report)
}

fn gate_label(d: GateDecision) -> &'static str {
    match d {
        GateDecision::Execute => "execute",
        GateDecision::Hold => "hold",
        GateDecision::Cancel => "cancel",
    }
}

/// Detected-but-not-yet-gated operation.
struct Proposal {
    op_kind: String,
    member_ids: Vec<String>,
    members: Vec<MemberPreview>,
    params: String,
}

fn preview(c: &CapabilityCapsuleRecord) -> MemberPreview {
    MemberPreview {
        capability_capsule_id: c.capability_capsule_id.clone(),
        summary: c.summary.clone(),
        project: c.project.clone(),
        capability_capsule_type: format!("{:?}", c.capability_capsule_type),
    }
}

fn is_guidance(t: &CapabilityCapsuleType) -> bool {
    matches!(
        t,
        CapabilityCapsuleType::Preference | CapabilityCapsuleType::Workflow
    )
}

fn is_episodic(t: &CapabilityCapsuleType) -> bool {
    matches!(
        t,
        CapabilityCapsuleType::Experience | CapabilityCapsuleType::Episode
    )
}

/// ① merge detection inside one map cluster: same
/// `(source_agent, project, repo)` group, sub-clustered at
/// `merge_threshold`, guidance types excluded, members already in a
/// supersede relation with each other excluded.
fn detect_merge(
    cluster: &[usize],
    active: &[CapabilityCapsuleRecord],
    vector_by_idx: &HashMap<usize, &Vec<f32>>,
    settings: &EvolutionSettings,
) -> Vec<Proposal> {
    let mut groups: HashMap<(String, Option<String>, Option<String>), Vec<usize>> = HashMap::new();
    for &idx in cluster {
        let c = &active[idx];
        if is_guidance(&c.capability_capsule_type) {
            continue;
        }
        groups
            .entry((c.source_agent.clone(), c.project.clone(), c.repo.clone()))
            .or_default()
            .push(idx);
    }
    let mut out = Vec::new();
    for idxs in groups.into_values() {
        if idxs.len() < 2 {
            continue;
        }
        let vectors: Vec<(usize, Vec<f32>)> = idxs
            .iter()
            .filter_map(|i| vector_by_idx.get(i).map(|v| (*i, (*v).clone())))
            .collect();
        for sub in build_clusters(&vectors, settings.merge_threshold) {
            if sub.len() < 2 {
                continue;
            }
            // Members already chained by supersede must not re-merge.
            let ids: BTreeSet<&str> = sub
                .iter()
                .map(|&i| active[i].capability_capsule_id.as_str())
                .collect();
            let members: Vec<usize> = sub
                .iter()
                .copied()
                .filter(|&i| {
                    active[i]
                        .supersedes_capability_capsule_id
                        .as_deref()
                        .map(|s| !ids.contains(s))
                        .unwrap_or(true)
                })
                .collect();
            if members.len() < 2 {
                continue;
            }
            let mut member_ids: Vec<String> = members
                .iter()
                .map(|&i| active[i].capability_capsule_id.clone())
                .collect();
            member_ids.sort();
            out.push(Proposal {
                op_kind: "merge".to_string(),
                members: members.iter().map(|&i| preview(&active[i])).collect(),
                member_ids,
                params: format!("{{\"merge_threshold\":{}}}", settings.merge_threshold),
            });
        }
    }
    out
}

/// One capsule's theme set for the ② generalize shared signal:
/// lowercased `topics ∪ tags` (E1.5, refs evolution-worker E1.5).
/// Rationale: the live corpus carries empty `topics` everywhere
/// (mine/hook ingest never fills it) but ≥2 meaningful `tags` on
/// 255/265 capsules — topics-only made ② structurally silent.
/// Entity-registry resolution stays a doc §4② refinement.
fn member_themes(c: &CapabilityCapsuleRecord) -> BTreeSet<String> {
    c.topics
        .iter()
        .chain(c.tags.iter())
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Shared themes across a member set — the intersection of each
/// member's `topics ∪ tags` (lowercased). Empty input → empty set.
fn shared_themes(members: &[&CapabilityCapsuleRecord]) -> Vec<String> {
    let mut shared: Option<BTreeSet<String>> = None;
    for c in members {
        let themes = member_themes(c);
        shared = Some(match shared {
            None => themes,
            Some(prev) => prev.intersection(&themes).cloned().collect(),
        });
    }
    shared.unwrap_or_default().into_iter().collect()
}

/// ② generalize detection over one map cluster: ≥ `generalize_min_n`
/// episodic members sharing ≥ 2 themes (`topics ∪ tags`, lowercased)
/// with mean confidence ≥ 0.6.
fn detect_generalize(
    cluster: &[usize],
    active: &[CapabilityCapsuleRecord],
    settings: &EvolutionSettings,
) -> Option<Proposal> {
    let members: Vec<usize> = cluster
        .iter()
        .copied()
        .filter(|&i| is_episodic(&active[i].capability_capsule_type))
        .collect();
    if members.len() < settings.generalize_min_n {
        return None;
    }
    let member_refs: Vec<&CapabilityCapsuleRecord> = members.iter().map(|&i| &active[i]).collect();
    let shared: Vec<String> = shared_themes(&member_refs);
    if shared.len() < GENERALIZE_MIN_SHARED_TOPICS {
        return None;
    }
    let mean_confidence: f32 =
        members.iter().map(|&i| active[i].confidence).sum::<f32>() / members.len() as f32;
    if mean_confidence < GENERALIZE_MIN_CONFIDENCE {
        return None;
    }
    let mut member_ids: Vec<String> = members
        .iter()
        .map(|&i| active[i].capability_capsule_id.clone())
        .collect();
    member_ids.sort();
    Some(Proposal {
        op_kind: "generalize".to_string(),
        members: members.iter().map(|&i| preview(&active[i])).collect(),
        member_ids,
        params: format!(
            "{{\"shared_topics\":{}}}",
            serde_json::to_string(&shared).unwrap_or_else(|_| "[]".to_string()),
        ),
    })
}

/// Execute ① merge: keep-longest canonical, archive the rest
/// (verbatim-safe status flip, NOT the dedup `Incorrect` path), write
/// `merged_into` lineage edges. Returns `[canonical_id]`.
async fn execute_merge(
    store: &dyn Backend,
    tenant: &str,
    member_ids: &[String],
    active: &[CapabilityCapsuleRecord],
    now: &str,
) -> Result<Vec<String>, StorageError> {
    let members: Vec<&CapabilityCapsuleRecord> = active
        .iter()
        .filter(|c| member_ids.contains(&c.capability_capsule_id))
        .collect();
    if members.len() < 2 {
        return Ok(Vec::new());
    }
    let survivor = members
        .iter()
        .copied()
        .max_by(|a, b| {
            a.content
                .len()
                .cmp(&b.content.len())
                .then_with(|| b.created_at.cmp(&a.created_at))
        })
        .expect("members non-empty");
    let survivor_id = survivor.capability_capsule_id.clone();
    for loser in members {
        if loser.capability_capsule_id == survivor_id {
            continue;
        }
        store
            .set_capsule_status(
                tenant,
                &loser.capability_capsule_id,
                CapabilityCapsuleStatus::Archived,
            )
            .await?;
        let edge = lineage_edge(
            &loser.capability_capsule_id,
            &survivor_id,
            "merged_into",
            now,
        );
        if let Err(e) = store.add_edge_direct(&edge).await {
            warn!(error = %e, "merged_into lineage edge write failed");
        }
    }
    Ok(vec![survivor_id])
}

/// Execute ② generalize: insert ONE `PendingConfirmation` proposal
/// capsule (review-backend raw material, doc §6.2) + `generalizes`
/// lineage edges. Sources are NOT touched. Returns `[proposal_id]`.
async fn execute_generalize(
    store: &dyn Backend,
    tenant: &str,
    member_ids: &[String],
    active: &[CapabilityCapsuleRecord],
    now: &str,
) -> Result<Vec<String>, StorageError> {
    let sources: Vec<&CapabilityCapsuleRecord> = active
        .iter()
        .filter(|c| member_ids.contains(&c.capability_capsule_id))
        .collect();
    if sources.is_empty() {
        return Ok(Vec::new());
    }
    let shared_topics: Vec<String> = shared_themes(&sources);
    let synthesized = ReviewSynthesisBackend.synthesize(&SynthesisTask::Generalize {
        sources: &sources,
        shared_topics: &shared_topics,
    });

    let project = common_value(sources.iter().map(|s| s.project.clone()));
    let repo = common_value(sources.iter().map(|s| s.repo.clone()));
    let proposal_id = format!("mem_{}", uuid::Uuid::now_v7());
    let mut record = CapabilityCapsuleRecord {
        capability_capsule_id: proposal_id.clone(),
        tenant: tenant.to_string(),
        capability_capsule_type: CapabilityCapsuleType::Experience,
        status: CapabilityCapsuleStatus::PendingConfirmation,
        scope: sources[0].scope.clone(),
        visibility: sources[0].visibility.clone(),
        version: 1,
        summary: synthesized.summary,
        content: synthesized.content,
        // Source ids double as evidence refs — auditable from the row
        // itself even without the graph.
        evidence: member_ids.to_vec(),
        code_refs: vec![],
        project,
        repo,
        module: None,
        task_type: None,
        tags: vec!["evolution:generalize".to_string()],
        topics: shared_topics,
        // PendingConfirmation ingest default (service::default_confidence).
        confidence: 0.6,
        decay_score: 0.0,
        content_hash: String::new(),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: "evolution_worker".to_string(),
        created_at: now.to_string(),
        updated_at: now.to_string(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    };
    record.content_hash = compute_content_hash_from_record(&record);
    store.insert_capability_capsule(record).await?;
    for s in &sources {
        let edge = lineage_edge(&proposal_id, &s.capability_capsule_id, "generalizes", now);
        if let Err(e) = store.add_edge_direct(&edge).await {
            warn!(error = %e, "generalizes lineage edge write failed");
        }
    }
    Ok(vec![proposal_id])
}

fn lineage_edge(from_capsule: &str, to_capsule: &str, relation: &str, now: &str) -> GraphEdge {
    GraphEdge {
        from_node_id: format!("capability_capsule:{from_capsule}"),
        to_node_id: format!("capability_capsule:{to_capsule}"),
        relation: relation.to_string(),
        valid_from: now.to_string(),
        valid_to: None,
        confidence: None,
        extractor: Some("evolution".to_string()),
        strength: None,
        stability: None,
        last_activated: None,
        access_count: None,
    }
}

/// `Some(v)` when every element equals `Some(v)`, else `None`.
fn common_value(mut iter: impl Iterator<Item = Option<String>>) -> Option<String> {
    let first = iter.next().flatten()?;
    for item in iter {
        if item.as_deref() != Some(first.as_str()) {
            return None;
        }
    }
    Some(first)
}
