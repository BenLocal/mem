//! Map layer — pure functions for the semantic "living map"
//! (doc `docs/evolution-worker.md` §3) and the anti-jitter gate
//! (doc §3.3). No I/O: callers load embeddings / candidates, this
//! module clusters, aligns, and updates evidence.

use std::collections::{HashMap, HashSet};

use crate::storage::EvolutionCandidate;

/// Minimum member-set Jaccard for a freshly-detected proposal to be
/// considered "the same operation" as a stored candidate (doc §3.3 —
/// the participant set must stay coherent across cycles).
pub const CANDIDATE_MATCH_JACCARD: f64 = 0.8;

/// Cosine similarity. Returns 0 on length mismatch / zero vectors
/// (no info — "not similar" beats NaN). Same contract as the
/// `dedup_worker` original.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
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

/// Union-find clustering: pairs with cosine ≥ `threshold` end up in
/// the same cluster. Input carries caller-side ids (`usize` indices
/// into the caller's capsule slice); output is one `Vec` of those ids
/// per cluster. Quadratic — callers cap input via `scan_limit`.
/// (`dedup_worker::build_clusters` skeleton, doc §3.2 step 2.)
pub fn build_clusters(vectors: &[(usize, Vec<f32>)], threshold: f32) -> Vec<Vec<usize>> {
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
        by_root.entry(r).or_default().push(*orig_idx);
    }
    by_root.into_values().collect()
}

/// Member-set Jaccard — the cross-cycle alignment metric (doc §3.2
/// step 3 for clusters, §3.3 for candidate matching). 1.0 for two
/// empty sets (vacuously identical).
pub fn jaccard(a: &[String], b: &[String]) -> f64 {
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    let union = sa.union(&sb).count();
    if union == 0 {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count();
    inter as f64 / union as f64
}

/// Find the stored candidate that a freshly-detected proposal aligns
/// with: same `op_kind`, member Jaccard ≥ [`CANDIDATE_MATCH_JACCARD`],
/// best Jaccard wins. Returns the index into `candidates`.
///
/// Status is the CALLER's concern: pass a status-filtered list
/// (`list_evolution_candidates(_, Some("pending"))` for gate matching,
/// `Some("executed")` for re-proposal suppression). Filtering here
/// would silently defeat the executed-history caller (audit
/// 2026-07-03 #1 — the suppression never matched anything).
pub fn match_candidate(
    op_kind: &str,
    member_ids: &[String],
    candidates: &[EvolutionCandidate],
) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (idx, c) in candidates.iter().enumerate() {
        if c.op_kind != op_kind {
            continue;
        }
        let j = jaccard(member_ids, &c.member_ids);
        if j >= CANDIDATE_MATCH_JACCARD && best.map(|(_, bj)| j > bj).unwrap_or(true) {
            best = Some((idx, j));
        }
    }
    best.map(|(idx, _)| idx)
}

/// Anti-jitter gate parameters (subset of `EvolutionSettings`).
#[derive(Debug, Clone, Copy)]
pub struct GateSettings {
    /// Execute only after this many CONSECUTIVE signal cycles.
    pub k_cycles: u32,
    /// β in `E_t = β·E_{t-1} + s_t`.
    pub evidence_decay: f32,
    /// Cancel a silent candidate only when its decayed evidence falls
    /// below this floor — below the propose threshold (s=1 per signal
    /// cycle), so borderline candidates don't flap create/cancel.
    pub hysteresis: f32,
}

/// What the gate says about a candidate after this cycle's update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Signal held ≥ K consecutive cycles — execute the operation.
    Execute,
    /// Keep accumulating (or keep decaying, still above the floor).
    Hold,
    /// Evidence decayed below the hysteresis floor — cancel.
    Cancel,
}

/// Cycle update for a candidate whose signal IS present this sweep:
/// `E ← β·E + 1`, consecutive counter advances, `last_signal_at`
/// stamps. Returns the gate decision for the updated state.
pub fn update_on_signal(
    candidate: &mut EvolutionCandidate,
    settings: &GateSettings,
    now: &str,
) -> GateDecision {
    candidate.evidence = settings.evidence_decay * candidate.evidence + 1.0;
    candidate.consecutive_cycles += 1;
    candidate.last_signal_at = now.to_string();
    if candidate.consecutive_cycles >= i64::from(settings.k_cycles) {
        GateDecision::Execute
    } else {
        GateDecision::Hold
    }
}

/// Cycle update for a pending candidate whose signal is ABSENT this
/// sweep: `E ← β·E`, consecutive counter resets (the gate demands
/// uninterrupted persistence). Cancels once evidence falls below the
/// hysteresis floor.
pub fn update_on_silence(
    candidate: &mut EvolutionCandidate,
    settings: &GateSettings,
) -> GateDecision {
    candidate.evidence *= settings.evidence_decay;
    candidate.consecutive_cycles = 0;
    if candidate.evidence < settings.hysteresis {
        GateDecision::Cancel
    } else {
        GateDecision::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(op: &str, members: &[&str]) -> EvolutionCandidate {
        EvolutionCandidate {
            candidate_id: "c1".into(),
            tenant: "local".into(),
            op_kind: op.into(),
            member_ids: members.iter().map(|s| s.to_string()).collect(),
            params: "{}".into(),
            evidence: 0.0,
            consecutive_cycles: 0,
            status: "pending".into(),
            first_proposed_at: "t0".into(),
            last_signal_at: "t0".into(),
            executed_at: None,
            result_capsule_ids: vec![],
        }
    }

    fn gate() -> GateSettings {
        GateSettings {
            k_cycles: 3,
            evidence_decay: 0.7,
            hysteresis: 0.5,
        }
    }

    // ── clustering ──────────────────────────────────────────────

    #[test]
    fn build_clusters_groups_similar_separates_orthogonal() {
        let vectors = vec![
            (10_usize, vec![1.0_f32, 0.0]),
            (20_usize, vec![0.99, 0.01]),
            (30_usize, vec![0.0, 1.0]),
        ];
        let clusters = build_clusters(&vectors, 0.9);
        assert_eq!(clusters.len(), 2);
        let mut sizes: Vec<usize> = clusters.iter().map(|c| c.len()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![1, 2]);
        let big = clusters.iter().find(|c| c.len() == 2).unwrap();
        let mut big = big.clone();
        big.sort();
        assert_eq!(big, vec![10, 20], "original ids must round-trip");
    }

    // ── alignment ───────────────────────────────────────────────

    #[test]
    fn jaccard_basics() {
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert!((jaccard(&a, &a) - 1.0).abs() < 1e-9);
        assert!((jaccard(&a, &b) - 0.75).abs() < 1e-9);
        assert!((jaccard(&[], &[]) - 1.0).abs() < 1e-9);
        assert_eq!(jaccard(&a, &[]), 0.0);
    }

    #[test]
    fn match_candidate_requires_same_op_and_high_jaccard() {
        let stored = vec![cand("merge", &["a", "b", "c", "d"])];
        // 3/4 = 0.75 < 0.8 → no match.
        let proposal: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(match_candidate("merge", &proposal, &stored), None);
        // 4/5 = 0.8 → match.
        let proposal: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(match_candidate("merge", &proposal, &stored), Some(0));
        // Same members, different op kind → no match.
        let proposal: Vec<String> = ["a", "b", "c", "d"].iter().map(|s| s.to_string()).collect();
        assert_eq!(match_candidate("generalize", &proposal, &stored), None);
    }

    #[test]
    fn match_candidate_matches_any_status_callers_filter() {
        // Status filtering is the caller's job (they pass a
        // status-filtered list) — an executed row MUST match so the
        // executed-history re-proposal suppression works (audit
        // 2026-07-03 #1).
        let mut executed = cand("merge", &["a", "b"]);
        executed.status = "executed".into();
        let proposal: Vec<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(match_candidate("merge", &proposal, &[executed]), Some(0));
    }

    // ── anti-jitter gate (E1 acceptance: 2 cycles hold, 3rd fires) ──

    #[test]
    fn gate_holds_for_two_cycles_executes_on_third() {
        let s = gate();
        let mut c = cand("merge", &["a", "b"]);
        assert_eq!(update_on_signal(&mut c, &s, "t1"), GateDecision::Hold);
        assert_eq!(c.consecutive_cycles, 1);
        assert_eq!(update_on_signal(&mut c, &s, "t2"), GateDecision::Hold);
        assert_eq!(c.consecutive_cycles, 2);
        assert_eq!(update_on_signal(&mut c, &s, "t3"), GateDecision::Execute);
        assert_eq!(c.consecutive_cycles, 3);
        assert_eq!(c.last_signal_at, "t3");
    }

    #[test]
    fn gate_silence_resets_consecutive_counter() {
        let s = gate();
        let mut c = cand("merge", &["a", "b"]);
        update_on_signal(&mut c, &s, "t1");
        update_on_signal(&mut c, &s, "t2");
        // One missed cycle — the consecutive clock restarts; two more
        // signal cycles are NOT enough (jitter must not accumulate
        // across gaps).
        assert_eq!(update_on_silence(&mut c, &s), GateDecision::Hold);
        assert_eq!(c.consecutive_cycles, 0);
        assert_eq!(update_on_signal(&mut c, &s, "t4"), GateDecision::Hold);
        assert_eq!(update_on_signal(&mut c, &s, "t5"), GateDecision::Hold);
        assert_eq!(update_on_signal(&mut c, &s, "t6"), GateDecision::Execute);
    }

    #[test]
    fn gate_evidence_decays_to_cancel_with_hysteresis() {
        let s = gate();
        let mut c = cand("merge", &["a", "b"]);
        update_on_signal(&mut c, &s, "t1"); // E = 1.0
        assert_eq!(update_on_silence(&mut c, &s), GateDecision::Hold); // 0.7
        assert_eq!(update_on_silence(&mut c, &s), GateDecision::Cancel); // 0.49 < 0.5
    }

    #[test]
    fn gate_evidence_accumulates_under_decay() {
        let s = gate();
        let mut c = cand("merge", &["a", "b"]);
        update_on_signal(&mut c, &s, "t1");
        update_on_signal(&mut c, &s, "t2");
        update_on_signal(&mut c, &s, "t3");
        // E = 0.7·(0.7·1 + 1) + 1 = 2.19
        assert!((c.evidence - 2.19).abs() < 1e-4, "got {}", c.evidence);
    }
}
