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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::EvolutionSettings;
use crate::domain::capability_capsule::{
    CapabilityCapsuleRecord, CapabilityCapsuleStatus, CapabilityCapsuleType, FeedbackKind,
    GraphEdge,
};
use crate::evolution::map::{
    build_clusters, cosine, match_candidate, update_on_signal, update_on_silence, GateDecision,
    GateSettings,
};
use crate::evolution::synthesis::{ReviewSynthesisBackend, SynthesisBackend, SynthesisTask};
use crate::pipeline::ingest::compute_content_hash_from_record;
use crate::storage::types::StorageError;
use crate::storage::{current_timestamp, Backend, EvolutionCandidate, FeedbackEvent};

/// `source_agent` stamped on every capsule the evolution worker
/// creates. Load-bearing beyond provenance: `auto_promote_worker`
/// excludes capsules carrying this agent from its sweep, because
/// evolution proposals are review-gated BY DESIGN (doc §6.2 — products
/// are forced through PendingConfirmation, never directly Active) and
/// auto-promote would silently bypass that gate (E1.6).
pub const EVOLUTION_SOURCE_AGENT: &str = "evolution_worker";

/// Mean-confidence floor for ② generalize sources (doc §4② — fixed in
/// E1, not env-tunable).
const GENERALIZE_MIN_CONFIDENCE: f32 = 0.6;
/// Minimum shared (lowercased) topics across a generalize cluster.
const GENERALIZE_MIN_SHARED_TOPICS: usize = 2;
/// ⑤ reweight-up stops emitting once a member reaches this confidence
/// (doc §4⑤'s 0.9 cap — enforced at emission because `apply_feedback`'s
/// generic clamp sits at 1.0).
const REWEIGHT_UP_CONFIDENCE_CAP: f32 = 0.9;
/// ⑤ reweight-up fires only when MORE than this share of a cluster was
/// recalled within the last sweep interval (doc §4⑤ "活跃占比 > 0.5").
const REWEIGHT_ACTIVE_SHARE_FLOOR: f32 = 0.5;
/// §5 dying filter for ⑤: capsules already decayed past this floor are
/// left to decay/idle-archive — don't spend evolution signal on them.
const DYING_DECAY_FLOOR: f32 = 0.8;
/// ⑥ skips co-recall batches larger than this — one giant search
/// response fanning out is weak evidence for PAIRWISE association, and
/// the pair count is quadratic in group size.
const CORECALL_MAX_GROUP: usize = 12;
/// ③ refine value gate (E5, doc §4③): only capsules recalled within
/// this window are worth revising — a contradicted capsule nobody
/// recalls is decay/idle-archive's problem, not refine's.
const REFINE_RECENT_WINDOW_SECS: u64 = 30 * 86_400;
/// ③ refine contradiction floor: accumulated `outdated` feedback
/// events at/above this count as a conflict signal.
const REFINE_MIN_OUTDATED: u64 = 2;

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
    /// ⑥ weak-edge retirement: evolution-owned `co_recalled_with`
    /// edges closed this sweep for lack of co-recall evidence inside
    /// the idle window (E4). Always 0 on dry-run.
    pub pruned_edges: usize,
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
        pruned_edges: 0,
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
    // One embeddings read per capsule (same query count as the old
    // single-vector read, all rows instead of LIMIT 1): the first chunk
    // feeds the map exactly as before; capsules with ≥2 chunks also
    // feed ④ split detection (E5).
    let mut vectors: Vec<(usize, Vec<f32>)> = Vec::with_capacity(active.len());
    let mut chunk_sets: HashMap<usize, Vec<Vec<f32>>> = HashMap::new();
    for (idx, c) in active.iter().enumerate() {
        let chunks: Vec<Vec<f32>> = store
            .get_capability_capsule_embedding_chunks(&c.capability_capsule_id)
            .await?
            .into_iter()
            .filter(|v| !v.is_empty())
            .collect();
        match chunks.first() {
            Some(first) => vectors.push((idx, first.clone())),
            None => continue, // not embedded yet — joins the map next sweep
        }
        if chunks.len() >= 2 {
            chunk_sets.insert(idx, chunks);
        }
    }
    report.embedded = vectors.len();

    // 2. Cluster.
    let clusters = build_clusters(&vectors, settings.cluster_threshold);
    report.clusters = clusters.len();
    let vector_by_idx: HashMap<usize, &Vec<f32>> = vectors.iter().map(|(i, v)| (*i, v)).collect();

    // 3. Detect proposals.
    let mut proposals: Vec<Proposal> = Vec::new();
    let recent_cutoff = ms_cutoff(&now, settings.interval_secs);
    for cluster in &clusters {
        proposals.extend(detect_merge(cluster, &active, &vector_by_idx, settings));
        if let Some(p) = detect_generalize(cluster, &active, settings) {
            proposals.push(p);
        }
        // E4 ⑤ reweight: recurring signal adjustments per cluster shape.
        if let Some(p) = detect_reweight_up(cluster, &active, &recent_cutoff) {
            proposals.push(p);
        }
        if let Some(p) = detect_reweight_decay(cluster, &active) {
            proposals.push(p);
        }
    }
    // E5 ④ split: multi-chunk capsules whose chunks separate into
    // well-apart groups (multi-topic in embedding space).
    for (&idx, chunks) in &chunk_sets {
        if let Some(p) = detect_split(idx, chunks, &active, settings) {
            proposals.push(p);
        }
    }
    // E5 ③ refine: contradiction signal (hanging suspected_supersede
    // edge, or accumulated `outdated` feedback) AND value signal
    // (recalled within the last 30 days). The value gate runs first so
    // the per-capsule feedback reads stay bounded to the hot set.
    let recall_cutoff = ms_cutoff(&now, REFINE_RECENT_WINDOW_SECS);
    let suspect_targets: HashSet<String> = match store
        .query_predicate("suspected_supersede", None)
        .await
    {
        Ok(edges) => edges
            .into_iter()
            .filter(|e| e.valid_to.is_none())
            .filter_map(|e| {
                e.to_node_id
                    .strip_prefix("capability_capsule:")
                    .map(str::to_string)
            })
            .collect(),
        Err(e) => {
            warn!(error = %e, "suspected_supersede read failed — refine signal degraded this sweep");
            HashSet::new()
        }
    };
    for c in &active {
        if is_guidance(&c.capability_capsule_type) || c.decay_score > DYING_DECAY_FLOOR {
            continue;
        }
        if c.last_recalled_at
            .as_deref()
            .is_none_or(|t| t < recall_cutoff.as_str())
        {
            continue;
        }
        let mut conflicts: Vec<String> = Vec::new();
        if suspect_targets.contains(&c.capability_capsule_id) {
            conflicts.push(
                "active suspected_supersede edge points at this capsule \
                 (unresolved O2/O7a near-duplicate proposal)"
                    .to_string(),
            );
        }
        match store.feedback_summary(&c.capability_capsule_id).await {
            Ok(s) if s.outdated >= REFINE_MIN_OUTDATED => {
                conflicts.push(format!(
                    "{} accumulated `outdated` feedback event(s)",
                    s.outdated
                ));
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "feedback summary read failed during refine detection"),
        }
        if conflicts.is_empty() {
            continue;
        }
        let params = serde_json::to_string(&conflicts)
            .map(|s| format!("{{\"conflicts\":{s}}}"))
            .unwrap_or_else(|_| "{}".to_string());
        proposals.push(Proposal {
            op_kind: "refine".to_string(),
            member_ids: vec![c.capability_capsule_id.clone()],
            members: vec![preview(c)],
            params,
        });
    }
    // E4 ⑥ co-recall: pairs sharing a last_used_worker flush batch.
    // Pairs already carrying an active edge are skipped at detection —
    // on a failed edge read we skip the whole channel this sweep
    // (proposing anyway would only add candidate noise).
    match store.query_predicate("co_recalled_with", None).await {
        Ok(edges) => {
            let existing: HashSet<(String, String)> = edges
                .into_iter()
                .filter(|e| e.valid_to.is_none())
                .map(|e| (e.from_node_id, e.to_node_id))
                .collect();
            proposals.extend(detect_corecall(&active, &existing));
        }
        Err(e) => {
            warn!(error = %e, "co_recalled_with read failed — corecall detection skipped this sweep")
        }
    }

    // 4. Reconcile with durable candidates (anti-jitter gate).
    let pending = store
        .list_evolution_candidates(tenant, Some("pending"))
        .await?;
    let mut executed_history = store
        .list_evolution_candidates(tenant, Some("executed"))
        .await?;
    // ⑦: an ACCEPTED candidate is a settled one-shot — the reviewer's
    // accept flips `executed` → `accepted`, and suppression must
    // survive that flip or the settled cluster re-proposes forever
    // (its sources stay Active by design).
    executed_history.extend(
        store
            .list_evolution_candidates(tenant, Some("accepted"))
            .await?,
    );
    // ⑪ churn guard input: a cancelled corecall candidate remembers the
    // batch_ts it was cancelled over (audit 2026-07-03 ⑪ — without
    // this, one stale pair re-minted a fresh candidate row every
    // hysteresis window forever).
    let cancelled_history = store
        .list_evolution_candidates(tenant, Some("cancelled"))
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
        // cluster would re-propose forever). One-shot ops only:
        // recurring ⑤ reweight candidates never park in `executed`,
        // and a ⑥ corecall pair must be re-earnable after its edge is
        // pruned (or rolled back).
        if matches!(
            proposal.op_kind.as_str(),
            "merge" | "generalize" | "refine" | "split"
        ) && match_candidate(&proposal.op_kind, &proposal.member_ids, &executed_history)
            .is_some()
        {
            continue;
        }
        // ⑥ churn guard: the SAME batch_ts that already ran its full
        // create→silence→cancel arc is not new evidence — only a FRESH
        // batch may re-open a cancelled pair.
        if proposal.op_kind == "corecall"
            && cancelled_history.iter().any(|c| {
                c.op_kind == "corecall"
                    && c.params == proposal.params
                    && crate::evolution::map::jaccard(&proposal.member_ids, &c.member_ids)
                        >= crate::evolution::map::CANDIDATE_MATCH_JACCARD
            })
        {
            continue;
        }
        let (mut candidate, decision_label, decision) =
            match match_candidate(&proposal.op_kind, &proposal.member_ids, &pending) {
                Some(i) => {
                    // ⑥ freshness guard: the SAME flush-batch timestamp
                    // re-observed is not new co-recall evidence — leave
                    // the candidate unmatched so silence decay applies.
                    if proposal.op_kind == "corecall" && pending[i].params == proposal.params {
                        continue;
                    }
                    matched_pending[i] = true;
                    let mut c = pending[i].clone();
                    if proposal.op_kind == "corecall" {
                        // Carry the newest batch ts for the next guard.
                        c.params = proposal.params.clone();
                    }
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
            // I2 P2 merge veto (docs/offline-reranker-lane.md §4). Fail
            // posture on a rerank ERROR is fail-closed-HOLD: the
            // operator asked for the gate, so executing past a broken
            // gate would bypass it — and cancelling would settle a
            // candidate no model ever scored. Held candidates stay
            // `pending` and retry next sweep.
            if proposal.op_kind == "merge" && crate::rerank::offline_enabled() {
                match merge_rerank_veto(&proposal.member_ids, &active).await {
                    Ok(None) => {}
                    Ok(Some(reason)) => {
                        crate::metrics::metrics().inc_rerank_merge_veto();
                        warn!(
                            candidate_id = %candidate.candidate_id,
                            %reason,
                            "rerank veto: merge candidate cancelled"
                        );
                        candidate.status = "cancelled".to_string();
                        report.cancelled.push(candidate.candidate_id.clone());
                        store.upsert_evolution_candidate(candidate).await?;
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            candidate_id = %candidate.candidate_id,
                            error = %e,
                            "rerank veto check failed; holding merge candidate (fail-closed)"
                        );
                        store.upsert_evolution_candidate(candidate).await?;
                        continue;
                    }
                }
            }
            let result_ids = match proposal.op_kind.as_str() {
                "merge" => {
                    execute_merge(store, tenant, &proposal.member_ids, &active, &now).await?
                }
                "generalize" => {
                    execute_generalize(store, tenant, &proposal.member_ids, &active, &now).await?
                }
                "reweight_up" => {
                    execute_reweight(
                        store,
                        &proposal.member_ids,
                        &active,
                        FeedbackKind::SystemReweightUp,
                        &candidate.candidate_id,
                        &now,
                    )
                    .await?
                }
                "reweight_decay" => {
                    execute_reweight(
                        store,
                        &proposal.member_ids,
                        &active,
                        FeedbackKind::SystemReweightDecay,
                        &candidate.candidate_id,
                        &now,
                    )
                    .await?
                }
                "corecall" => execute_corecall(store, &proposal.member_ids, &now).await?,
                "refine" => {
                    execute_refine(
                        store,
                        tenant,
                        &proposal.member_ids,
                        &active,
                        &proposal.params,
                        &now,
                    )
                    .await?
                }
                "split" => {
                    execute_split(
                        store,
                        tenant,
                        &proposal.member_ids,
                        &active,
                        &proposal.params,
                        &now,
                    )
                    .await?
                }
                other => {
                    warn!(op_kind = other, "unknown evolution op kind — skipping");
                    Vec::new()
                }
            };
            // Recurring ⑤ ops stay `pending`: the doc's "+δ per signal
            // cycle" means the open gate re-fires every cycle the
            // signal holds (and silence still decays/cancels them).
            // One-shot ops park in `executed` and its re-proposal
            // suppression.
            if !matches!(proposal.op_kind.as_str(), "reweight_up" | "reweight_decay") {
                candidate.status = "executed".to_string();
            }
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

    // 6. ⑥ weak-edge retirement (E4): close evolution-owned
    //    co_recalled_with edges that show no evidence inside the idle
    //    window. Dry-run never prunes. A prune failure is logged and
    //    skipped — retirement retries next sweep.
    if !dry_run {
        match prune_idle_corecall_edges(store, &active, settings, &now).await {
            Ok(n) => report.pruned_edges = n,
            Err(e) => warn!(error = %e, "co_recalled_with prune failed — skipped this sweep"),
        }
    }

    Ok(report)
}

/// Result of rolling back one executed candidate — §11's audit unit.
/// Serialized verbatim as the HTTP response of
/// `POST /reviews/evolution/rollback`.
#[derive(Debug, Serialize)]
pub struct RollbackReport {
    pub candidate_id: String,
    pub op_kind: String,
    /// Capsules whose status was written back (merge: losers → Active;
    /// generalize: the proposal capsule → Archived).
    pub restored: Vec<String>,
    /// Lineage edges closed (`valid_to` stamped — never deleted).
    pub closed_edges: usize,
}

/// Roll back one executed candidate (doc §11). The rollback unit is a
/// whole executed candidate row; the inverse is exact because ① merge
/// only flips loser status + writes lineage edges (E1's documented
/// deviation from the supersede-chain design) and ② generalize only
/// inserts a proposal capsule + lineage edges:
///
/// - `merge`: losers → `Active` again, their `merged_into` edges get
///   `valid_to` stamped (closed, never deleted — §11 audit semantics).
/// - `generalize`: the proposal capsule → `Archived`, ALL its edges
///   closed; sources were never touched so rollback doesn't touch them.
///
/// Edge-close failures are warned and skipped (same tolerance as the
/// forward path's lineage writes) — the status writes are the
/// retrieval-semantics restore and DO propagate errors. The candidate
/// row is kept as a `rolled_back` tombstone, never deleted.
pub async fn rollback_candidate(
    store: &dyn Backend,
    tenant: &str,
    candidate_id: &str,
) -> Result<RollbackReport, StorageError> {
    let all = store.list_evolution_candidates(tenant, None).await?;
    let mut candidate = all
        .into_iter()
        .find(|c| c.candidate_id == candidate_id)
        .ok_or_else(|| {
            StorageError::InvalidInput(format!("unknown evolution candidate: {candidate_id}"))
        })?;
    if candidate.status != "executed" {
        return Err(StorageError::InvalidInput(format!(
            "candidate {candidate_id} is '{}' — only executed candidates can be rolled back",
            candidate.status
        )));
    }

    let now = current_timestamp();
    let mut restored = Vec::new();
    let mut closed_edges = 0usize;
    match candidate.op_kind.as_str() {
        "merge" => {
            let canonical =
                candidate
                    .result_capsule_ids
                    .first()
                    .cloned()
                    .ok_or(StorageError::InvalidData(
                        "executed merge candidate has no canonical id",
                    ))?;
            for loser in candidate.member_ids.iter().filter(|m| **m != canonical) {
                store
                    .set_capsule_status(tenant, loser, CapabilityCapsuleStatus::Active)
                    .await?;
                restored.push(loser.clone());
                match store
                    .invalidate_edge(
                        &format!("capability_capsule:{loser}"),
                        "merged_into",
                        &format!("capability_capsule:{canonical}"),
                        &now,
                    )
                    .await
                {
                    Ok(n) => closed_edges += n,
                    Err(e) => {
                        warn!(error = %e, loser = %loser, "rollback: merged_into edge close failed")
                    }
                }
            }
        }
        // ②③④ share the placeholder shape — rollback archives the
        // placeholder and closes its lineage edges; sources were never
        // touched by any of the three (§11).
        "generalize" | "refine" | "split" => {
            let proposal =
                candidate
                    .result_capsule_ids
                    .first()
                    .cloned()
                    .ok_or(StorageError::InvalidData(
                        "executed placeholder candidate has no proposal id",
                    ))?;
            store
                .set_capsule_status(tenant, &proposal, CapabilityCapsuleStatus::Archived)
                .await?;
            restored.push(proposal.clone());
            match store.close_edges_for_capability_capsule(&proposal).await {
                Ok(n) => closed_edges += n,
                Err(e) => {
                    warn!(error = %e, proposal = %proposal, "rollback: lineage edge close failed")
                }
            }
        }
        "corecall" => {
            // §11 ⑥: the rollback of an earned edge is closing it
            // (point-in-time reads keep the history).
            let (Some(a), Some(b)) = (candidate.member_ids.first(), candidate.member_ids.get(1))
            else {
                return Err(StorageError::InvalidData(
                    "executed corecall candidate is missing its pair",
                ));
            };
            match store
                .invalidate_edge(
                    &format!("capability_capsule:{a}"),
                    "co_recalled_with",
                    &format!("capability_capsule:{b}"),
                    &now,
                )
                .await
            {
                Ok(n) => closed_edges += n,
                Err(e) => warn!(error = %e, "rollback: co_recalled_with edge close failed"),
            }
        }
        "reweight_up" | "reweight_decay" => {
            // Recurring ⑤ candidates never park in `executed`; their
            // rollback path is §11's note-based inversion of the
            // feedback_events rows (note = "evolution:<kind>:candidate=…"),
            // not a candidate-unit rollback.
            return Err(StorageError::InvalidInput(format!(
                "candidate {candidate_id} is a recurring reweight op — invert its \
                 feedback_events rows (note 'evolution:*:candidate={candidate_id}') instead"
            )));
        }
        other => {
            return Err(StorageError::InvalidInput(format!(
                "candidate {candidate_id} has unrecognized op_kind '{other}'"
            )));
        }
    }

    candidate.status = "rolled_back".to_string();
    store.upsert_evolution_candidate(candidate.clone()).await?;
    Ok(RollbackReport {
        candidate_id: candidate.candidate_id,
        op_kind: candidate.op_kind,
        restored,
        closed_edges,
    })
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

/// Keep-longest merge canonical (ties → earlier `created_at`) — the
/// single source of truth shared by [`execute_merge`] and the I2 P2
/// rerank veto, so the gate always scores against the capsule that
/// would actually survive.
fn merge_survivor<'a>(members: &[&'a CapabilityCapsuleRecord]) -> &'a CapabilityCapsuleRecord {
    members
        .iter()
        .copied()
        .max_by(|a, b| {
            a.content
                .len()
                .cmp(&b.content.len())
                .then_with(|| b.created_at.cmp(&a.created_at))
        })
        .expect("members non-empty")
}

/// I2 P2 (docs/offline-reranker-lane.md §4): cross-encoder floor for a
/// merge candidate. Each would-be loser is scored against the survivor
/// in BOTH directions; a bidirectional geometric mean below
/// `MEM_RERANK_MERGE_FLOOR` returns `Some(reason)` — same-topic but
/// different-fact capsules (which cosine clustering happily lumps
/// together) score low, true restatements score high. Scoring runs in
/// `spawn_blocking` (model load + forward are seconds of CPU) and the
/// provider is dropped when the batch is done — never resident.
async fn merge_rerank_veto(
    member_ids: &[String],
    active: &[CapabilityCapsuleRecord],
) -> Result<Option<String>, crate::rerank::RerankError> {
    let members: Vec<&CapabilityCapsuleRecord> = active
        .iter()
        .filter(|c| member_ids.contains(&c.capability_capsule_id))
        .collect();
    if members.len() < 2 {
        return Ok(None);
    }
    let survivor = merge_survivor(&members);
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut losers: Vec<String> = Vec::new();
    for m in &members {
        if m.capability_capsule_id == survivor.capability_capsule_id {
            continue;
        }
        let s = crate::rerank::truncate_for_rerank(&survivor.content).to_string();
        let l = crate::rerank::truncate_for_rerank(&m.content).to_string();
        pairs.push((s.clone(), l.clone()));
        pairs.push((l, s));
        losers.push(m.capability_capsule_id.clone());
    }
    let n_pairs = pairs.len() as u64;
    let scores = tokio::task::spawn_blocking(move || {
        let provider = crate::rerank::provider_from_env()?;
        provider.score_pairs(&pairs)
    })
    .await
    .map_err(|e| crate::rerank::RerankError::Internal(format!("rerank task join: {e}")))??;
    crate::metrics::metrics().add_rerank_pairs(n_pairs);
    let floor = crate::rerank::merge_floor();
    for (i, loser_id) in losers.iter().enumerate() {
        let geo = (scores[2 * i] * scores[2 * i + 1]).max(0.0).sqrt();
        if geo < floor {
            return Ok(Some(format!(
                "loser {loser_id} bidirectional score {geo:.3} < floor {floor:.2}"
            )));
        }
    }
    Ok(None)
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
    let survivor = merge_survivor(&members);
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
        source_agent: EVOLUTION_SOURCE_AGENT.to_string(),
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

/// ⑤ reweight-up detection (E4): a cluster of ≥2 whose recently-
/// recalled share EXCEEDS the 0.5 floor ("recent" = `last_recalled_at`
/// within one sweep interval — NOT `last_used_at`, which the hourly
/// decay sweep stamps on every active row as its clock; audit
/// 2026-07-03 #2a). Guidance capsules are excluded (curated, not
/// evolved — ①④⑤-decay precedent; audit ⑫). Recurring — the gate
/// re-fires every signal cycle after it first opens.
fn detect_reweight_up(
    cluster: &[usize],
    active: &[CapabilityCapsuleRecord],
    recent_cutoff: &str,
) -> Option<Proposal> {
    let members: Vec<usize> = cluster
        .iter()
        .copied()
        .filter(|&i| !is_guidance(&active[i].capability_capsule_type))
        .collect();
    if members.len() < 2 {
        return None;
    }
    let recalled = members
        .iter()
        .filter(|&&i| {
            active[i]
                .last_recalled_at
                .as_deref()
                .is_some_and(|t| t >= recent_cutoff)
        })
        .count();
    if (recalled as f32) / (members.len() as f32) <= REWEIGHT_ACTIVE_SHARE_FLOOR {
        return None;
    }
    let mut member_ids: Vec<String> = members
        .iter()
        .map(|&i| active[i].capability_capsule_id.clone())
        .collect();
    member_ids.sort();
    Some(Proposal {
        op_kind: "reweight_up".to_string(),
        members: members.iter().map(|&i| preview(&active[i])).collect(),
        member_ids,
        params: "{}".to_string(),
    })
}

/// ⑤ reweight-decay detection (E4): a map singleton (no cluster
/// membership) with ZERO recalls ever. Guidance capsules are excluded
/// (curated, not evolved — merge precedent) and so are dying capsules
/// (§5: decay > 0.8 gets no evolution budget; decay/idle-archive
/// already own them).
fn detect_reweight_decay(
    cluster: &[usize],
    active: &[CapabilityCapsuleRecord],
) -> Option<Proposal> {
    if cluster.len() != 1 {
        return None;
    }
    let c = &active[cluster[0]];
    // `last_recalled_at` is the durable recall signal — `last_used_at`
    // is the decay clock the hourly sweep advances on every row, which
    // made this lane dead after a capsule's first hour (audit
    // 2026-07-03 #2b).
    if c.last_recalled_at.is_some()
        || c.decay_score > DYING_DECAY_FLOOR
        || is_guidance(&c.capability_capsule_type)
    {
        return None;
    }
    Some(Proposal {
        op_kind: "reweight_decay".to_string(),
        member_ids: vec![c.capability_capsule_id.clone()],
        members: vec![preview(c)],
        params: "{}".to_string(),
    })
}

/// ⑥ co-recall detection (E4): capsules sharing one exact
/// `last_recalled_at` value were stamped by the same
/// `last_used_worker` flush — one co-recall batch (the signal source
/// doc §4⑥ names). Only `bump_last_used_at` writes that column, so it
/// is immune to the hourly decay sweep homogenizing `last_used_at`
/// across the whole corpus (audit 2026-07-03 #2c — that shared stamp
/// minted all-pairs Hebbian edges in small tenants).
/// `params` carries the batch timestamp so the reconcile freshness
/// guard can tell a NEW batch from the same one re-observed. Pairs
/// already holding an active edge are skipped; oversized batches are
/// skipped whole (weak pairwise evidence, quadratic pair count).
fn detect_corecall(
    active: &[CapabilityCapsuleRecord],
    existing_pairs: &HashSet<(String, String)>,
) -> Vec<Proposal> {
    let mut by_batch: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, c) in active.iter().enumerate() {
        if let Some(ts) = c.last_recalled_at.as_deref() {
            by_batch.entry(ts).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (ts, group) in by_batch {
        if group.len() < 2 {
            continue;
        }
        if group.len() > CORECALL_MAX_GROUP {
            warn!(
                size = group.len(),
                "co-recall batch above pair cap — skipped"
            );
            continue;
        }
        for (x, &i) in group.iter().enumerate() {
            for &j in &group[x + 1..] {
                let (a, b) = {
                    let (ai, bi) = (
                        &active[i].capability_capsule_id,
                        &active[j].capability_capsule_id,
                    );
                    if ai <= bi {
                        (ai.clone(), bi.clone())
                    } else {
                        (bi.clone(), ai.clone())
                    }
                };
                if existing_pairs.contains(&(
                    format!("capability_capsule:{a}"),
                    format!("capability_capsule:{b}"),
                )) {
                    continue;
                }
                out.push(Proposal {
                    op_kind: "corecall".to_string(),
                    members: vec![preview(&active[i]), preview(&active[j])],
                    member_ids: vec![a, b],
                    params: format!("{{\"batch_ts\":\"{ts}\"}}"),
                });
            }
        }
    }
    out
}

/// ④ split detection (E5): a capsule's chunk vectors separate into ≥2
/// groups (union-find at `cluster_threshold`, the map's own geometry)
/// AND every cross-group chunk pair sits at/below `split_threshold` —
/// well-separated topics, not mild internal drift. Guidance and dying
/// capsules are excluded (⑤ precedent).
fn detect_split(
    idx: usize,
    chunks: &[Vec<f32>],
    active: &[CapabilityCapsuleRecord],
    settings: &EvolutionSettings,
) -> Option<Proposal> {
    let c = &active[idx];
    if is_guidance(&c.capability_capsule_type) || c.decay_score > DYING_DECAY_FLOOR {
        return None;
    }
    let indexed: Vec<(usize, Vec<f32>)> = chunks.iter().cloned().enumerate().collect();
    let mut groups = build_clusters(&indexed, settings.cluster_threshold);
    if groups.len() < 2 {
        return None;
    }
    for g in &mut groups {
        g.sort_unstable();
    }
    groups.sort();
    for (gi, ga) in groups.iter().enumerate() {
        for gb in groups.iter().skip(gi + 1) {
            for &a in ga {
                for &b in gb {
                    if cosine(&chunks[a], &chunks[b]) > settings.split_threshold {
                        return None;
                    }
                }
            }
        }
    }
    let params = serde_json::to_string(&groups)
        .map(|s| format!("{{\"chunk_groups\":{s}}}"))
        .unwrap_or_else(|_| "{}".to_string());
    Some(Proposal {
        op_kind: "split".to_string(),
        member_ids: vec![c.capability_capsule_id.clone()],
        members: vec![preview(c)],
        params,
    })
}

/// Insert ONE `PendingConfirmation` review placeholder + its lineage
/// edges (placeholder → each source). Shared by ③ refine and ④ split;
/// ② generalize predates this helper and builds the same shape inline.
async fn insert_review_placeholder(
    store: &dyn Backend,
    tenant: &str,
    sources: &[&CapabilityCapsuleRecord],
    synthesized: crate::evolution::synthesis::SynthesizedProposal,
    op_tag: &str,
    lineage_relation: &str,
    now: &str,
) -> Result<Vec<String>, StorageError> {
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
        // itself even without the graph (generalize precedent; also the
        // handle `edit_and_accept_pending` uses to re-own lineage).
        evidence: sources
            .iter()
            .map(|s| s.capability_capsule_id.clone())
            .collect(),
        code_refs: vec![],
        project,
        repo,
        module: None,
        task_type: None,
        tags: vec![format!("evolution:{op_tag}")],
        topics: vec![],
        confidence: 0.6,
        decay_score: 0.0,
        content_hash: String::new(),
        idempotency_key: None,
        session_id: None,
        supersedes_capability_capsule_id: None,
        source_agent: EVOLUTION_SOURCE_AGENT.to_string(),
        created_at: now.to_string(),
        updated_at: now.to_string(),
        last_validated_at: None,
        last_used_at: None,
        last_recalled_at: None,
        expires_at: None,
    };
    record.content_hash = compute_content_hash_from_record(&record);
    store.insert_capability_capsule(record).await?;
    for s in sources {
        let edge = lineage_edge(
            &proposal_id,
            &s.capability_capsule_id,
            lineage_relation,
            now,
        );
        if let Err(e) = store.add_edge_direct(&edge).await {
            warn!(error = %e, relation = lineage_relation, "lineage edge write failed");
        }
    }
    Ok(vec![proposal_id])
}

/// Execute ③ refine: ONE `PendingConfirmation` placeholder +
/// `refined_from` lineage. The source is NOT touched — Phase 1 review
/// form; the reviewer writes the correction via `review_edit_accept`
/// and supersedes the original explicitly.
async fn execute_refine(
    store: &dyn Backend,
    tenant: &str,
    member_ids: &[String],
    active: &[CapabilityCapsuleRecord],
    params: &str,
    now: &str,
) -> Result<Vec<String>, StorageError> {
    let Some(source) = active
        .iter()
        .find(|c| member_ids.contains(&c.capability_capsule_id))
    else {
        return Ok(Vec::new());
    };
    let mut conflicts: Vec<String> = serde_json::from_str::<serde_json::Value>(params)
        .ok()
        .and_then(|v| serde_json::from_value(v.get("conflicts")?.clone()).ok())
        .unwrap_or_default();
    // H2 (oss-memory-diff §9): surface the verbatim notes riding
    // `outdated` feedback as conflict evidence — the reviewer sees WHY
    // the capsule is stale, not just how many times it was flagged
    // (review-gated version of MemOS's natural-language correction).
    // A failed read degrades to the count-only evidence, never aborts.
    match store
        .list_feedback_for_memory(&source.capability_capsule_id)
        .await
    {
        Ok(events) => {
            for ev in events {
                if ev.feedback_kind != "outdated" {
                    continue;
                }
                if let Some(note) = ev.note.filter(|n| !n.is_empty()) {
                    conflicts.push(format!("outdated note: {note}"));
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "feedback events read failed — refine note evidence degraded")
        }
    }
    let synthesized = ReviewSynthesisBackend.synthesize(&SynthesisTask::Refine {
        source,
        conflicts: &conflicts,
    });
    insert_review_placeholder(
        store,
        tenant,
        &[source],
        synthesized,
        "refine",
        "refined_from",
        now,
    )
    .await
}

/// Execute ④ split: ONE `PendingConfirmation` placeholder +
/// `split_from` lineage carrying the chunk-group plan. Source untouched
/// (same Phase 1 review form as ③).
async fn execute_split(
    store: &dyn Backend,
    tenant: &str,
    member_ids: &[String],
    active: &[CapabilityCapsuleRecord],
    params: &str,
    now: &str,
) -> Result<Vec<String>, StorageError> {
    let Some(source) = active
        .iter()
        .find(|c| member_ids.contains(&c.capability_capsule_id))
    else {
        return Ok(Vec::new());
    };
    let chunk_groups: Vec<Vec<usize>> = serde_json::from_str::<serde_json::Value>(params)
        .ok()
        .and_then(|v| serde_json::from_value(v.get("chunk_groups")?.clone()).ok())
        .unwrap_or_default();
    let synthesized = ReviewSynthesisBackend.synthesize(&SynthesisTask::Split {
        source,
        chunk_groups: &chunk_groups,
    });
    insert_review_placeholder(
        store,
        tenant,
        &[source],
        synthesized,
        "split",
        "split_from",
        now,
    )
    .await
}

/// Execute ⑤ reweight over one candidate's members: one auditable
/// `feedback_events` row per touched capsule (the SAME channel human
/// feedback uses — §4⑤'s "走可审计通道"), additive delta applied by
/// `apply_feedback`. Per-member guards: dying capsules skipped, and
/// reweight-up stops at the 0.9 cap. Returns the touched ids.
async fn execute_reweight(
    store: &dyn Backend,
    member_ids: &[String],
    active: &[CapabilityCapsuleRecord],
    kind: FeedbackKind,
    candidate_id: &str,
    now: &str,
) -> Result<Vec<String>, StorageError> {
    let mut touched = Vec::new();
    for c in active
        .iter()
        .filter(|c| member_ids.contains(&c.capability_capsule_id))
    {
        if c.decay_score > DYING_DECAY_FLOOR {
            continue;
        }
        if kind == FeedbackKind::SystemReweightUp && c.confidence >= REWEIGHT_UP_CONFIDENCE_CAP {
            continue;
        }
        let event = FeedbackEvent {
            feedback_id: format!("fb_{}", uuid::Uuid::now_v7()),
            capability_capsule_id: c.capability_capsule_id.clone(),
            feedback_kind: kind.as_str().to_string(),
            created_at: now.to_string(),
            // §11 ⑤: the note is the rollback handle — events carrying
            // this candidate id can be inverted later.
            note: Some(format!(
                "evolution:{}:candidate={candidate_id}",
                kind.as_str()
            )),
        };
        store.apply_feedback(c, event).await?;
        crate::metrics::metrics().record_feedback(&kind);
        touched.push(c.capability_capsule_id.clone());
    }
    Ok(touched)
}

/// Execute ⑥: write the earned `co_recalled_with` edge (sorted pair,
/// one direction, `extractor="evolution"` — cooccurrence precedent).
/// The edge feeds O4's 1-hop graph boost immediately.
async fn execute_corecall(
    store: &dyn Backend,
    member_ids: &[String],
    now: &str,
) -> Result<Vec<String>, StorageError> {
    let (Some(a), Some(b)) = (member_ids.first(), member_ids.get(1)) else {
        return Ok(Vec::new());
    };
    let mut edge = lineage_edge(a, b, "co_recalled_with", now);
    edge.last_activated = Some(now.to_string());
    edge.access_count = Some(1);
    if let Err(e) = store.add_edge_direct(&edge).await {
        warn!(error = %e, "co_recalled_with edge write failed");
    }
    Ok(member_ids.to_vec())
}

/// ⑥ weak-edge retirement: close evolution-owned `co_recalled_with`
/// edges with no evidence inside `prune_idle_cycles × interval_secs`.
/// Idleness is measured CONSERVATIVELY against the latest of edge
/// birth, its potentiation stamp, and EITHER endpoint's own
/// `last_recalled_at` (the durable recall signal — `last_used_at` is
/// the decay clock and is always fresh; audit 2026-07-03 #2d) —
/// without a per-pair co-access event log we cannot distinguish "pair
/// co-recalled" from "members individually hot", so individually-hot
/// pairs keep their edge (erring toward keeping a strengthening
/// signal). Edges with NO endpoint inside this sweep's scanned active
/// set are skipped outright: `graph_edges` is tenant-less, so another
/// tenant's (or an out-of-`scan_limit`-window) edge is invisible to
/// this scan and we have no basis to judge its idleness (audit ⑧ —
/// the fallback-to-edge-birth used to close other tenants' live
/// edges). Caller edges (`extractor != "evolution"`) and
/// non-`co_recalled_with` relations (incl. `user_tunnel:*`) are never
/// touched.
async fn prune_idle_corecall_edges(
    store: &dyn Backend,
    active: &[CapabilityCapsuleRecord],
    settings: &EvolutionSettings,
    now: &str,
) -> Result<usize, crate::storage::GraphError> {
    let cutoff = ms_cutoff(
        now,
        u64::from(settings.prune_idle_cycles).saturating_mul(settings.interval_secs),
    );
    // Every scanned id is present (endpoint-ownership check); the value
    // is its durable recall stamp when it has one.
    let recalled_by_id: HashMap<&str, Option<&str>> = active
        .iter()
        .map(|c| {
            (
                c.capability_capsule_id.as_str(),
                c.last_recalled_at.as_deref(),
            )
        })
        .collect();
    let mut pruned = 0usize;
    for edge in store.query_predicate("co_recalled_with", None).await? {
        if edge.valid_to.is_some() || edge.extractor.as_deref() != Some("evolution") {
            continue;
        }
        let mut latest = edge.valid_from.as_str();
        if let Some(t) = edge.last_activated.as_deref() {
            if t > latest {
                latest = t;
            }
        }
        let mut known_endpoint = false;
        for node in [&edge.from_node_id, &edge.to_node_id] {
            if let Some(id) = node.strip_prefix("capability_capsule:") {
                if let Some(stamp) = recalled_by_id.get(id) {
                    known_endpoint = true;
                    if let Some(t) = stamp {
                        if *t > latest {
                            latest = t;
                        }
                    }
                }
            }
        }
        if !known_endpoint || latest >= cutoff.as_str() {
            continue;
        }
        pruned += store
            .invalidate_edge(&edge.from_node_id, &edge.relation, &edge.to_node_id, now)
            .await?;
    }
    Ok(pruned)
}

/// Millisecond-string timestamp `secs` before `now`, zero-padded to
/// the storage width so lexicographic compare == numeric compare.
fn ms_cutoff(now: &str, secs: u64) -> String {
    let now_ms: u64 = now.parse().unwrap_or(0);
    format!("{:020}", now_ms.saturating_sub(secs.saturating_mul(1000)))
}

/// Evolution lineage edge (`extractor="evolution"`). `pub(crate)` so the
/// review-accept path (`capability_capsule_service::edit_and_accept_pending`)
/// re-writes the SAME edge shape when it moves generalize lineage onto
/// the accepted successor (E3 "accept 时写边").
pub(crate) fn lineage_edge(
    from_capsule: &str,
    to_capsule: &str,
    relation: &str,
    now: &str,
) -> GraphEdge {
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
