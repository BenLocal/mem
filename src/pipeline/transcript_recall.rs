//! Transcript candidate scoring and window assembly. Separate from
//! pipeline/retrieve.rs (memories scoring) — zero shared state. Shares
//! only the pure helpers in pipeline/ranking.rs.
//!
//! Task 5 (this commit) lands `score_candidates`. Task 6 will append
//! `merge_windows` and the window-assembly types.

use std::collections::HashMap;

use crate::domain::ConversationMessage;
use crate::pipeline::ranking::{freshness_score, rrf_contribution, timestamp_score};

// ── Scoring magnitude constants (tunable; documented next to constants).

/// Per-sibling boost when a candidate shares its `session_id` with another
/// candidate in the same scoring batch. Encourages multi-block matches from
/// a single conversation to surface together.
pub const SESSION_COOCC_PER_SIBLING: i64 = 3;

/// Cap on co-occurrence siblings counted (avoids runaway on long sessions
/// that happen to be massively over-represented in candidates).
pub const SESSION_COOCC_CAP_SIBLINGS: i64 = 4;

/// Bonus applied to candidates whose `session_id` matches the caller-supplied
/// `anchor_session_id`. Set above the rank-1 RRF value (~16) so an explicit
/// anchor reliably bumps moderate matches up; *never* high enough to flood
/// irrelevant blocks above strong topical matches (rank-1 RRF×2 = ~32).
///
/// Magnitude invariant guarded by `magnitude_anchor_dominates_cooccurrence`
/// test below: `ANCHOR_SESSION_BONUS > SESSION_COOCC_PER_SIBLING * SESSION_COOCC_CAP_SIBLINGS`.
pub const ANCHOR_SESSION_BONUS: i64 = 20;

/// Optional per-call options.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoringOpts<'a> {
    pub anchor_session_id: Option<&'a str>,
}

/// One candidate annotated with its final composite score.
#[derive(Debug, Clone)]
pub struct ScoredBlock {
    pub message: ConversationMessage,
    pub score: i64,
}

/// Score the given candidate set. Pure function: no I/O, no allocation
/// beyond the result `Vec`.
///
/// Final score = `rrf_contribution(lex_rank) + rrf_contribution(sem_rank)`
///              `+ session_co_occurrence_bonus(this, all)`
///              `+ anchor_session_bonus(this.session_id, opts.anchor)`
///              `+ freshness_score(newest_in_pool, this.created_at)`.
///
/// Returned vector is sorted by score descending.
pub fn score_candidates(
    candidates: Vec<ConversationMessage>,
    lexical_ranks: &HashMap<String, usize>,
    semantic_ranks: &HashMap<String, usize>,
    opts: ScoringOpts<'_>,
) -> Vec<ScoredBlock> {
    if candidates.is_empty() {
        return vec![];
    }

    // Pre-compute newest timestamp for the freshness curve.
    let newest = candidates
        .iter()
        .map(|m| timestamp_score(&m.created_at))
        .max()
        .unwrap_or(0);

    // Pre-compute session sibling counts. Counts include self so we
    // subtract 1 below (siblings = others in same session). Owned `String`
    // keys so the table doesn't borrow from `candidates` (we move `candidates`
    // into the scoring closure below).
    let mut session_counts: HashMap<String, i64> = HashMap::new();
    for m in &candidates {
        if let Some(sid) = m.session_id.as_deref() {
            *session_counts.entry(sid.to_string()).or_insert(0) += 1;
        }
    }

    let mut scored: Vec<ScoredBlock> = candidates
        .into_iter()
        .map(|m| {
            let mut s: i64 = 0;

            // RRF (lex + sem).
            s += lexical_ranks
                .get(&m.message_block_id)
                .map(|&r| rrf_contribution(r))
                .unwrap_or(0);
            s += semantic_ranks
                .get(&m.message_block_id)
                .map(|&r| rrf_contribution(r))
                .unwrap_or(0);

            // Session co-occurrence.
            if let Some(sid) = m.session_id.as_deref() {
                let total = *session_counts.get(sid).unwrap_or(&0);
                let siblings = (total - 1).clamp(0, SESSION_COOCC_CAP_SIBLINGS);
                s += SESSION_COOCC_PER_SIBLING * siblings;
            }

            // Anchor session boost.
            if let (Some(anchor), Some(sid)) = (opts.anchor_session_id, m.session_id.as_deref()) {
                if anchor == sid {
                    s += ANCHOR_SESSION_BONUS;
                }
            }

            // Freshness curve.
            let ts = timestamp_score(&m.created_at);
            s += freshness_score(newest, ts);

            ScoredBlock {
                message: m,
                score: s,
            }
        })
        .collect();

    scored.sort_by(|a, b| b.score.cmp(&a.score));
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BlockType, MessageRole};

    fn sample(suffix: &str, session: Option<&str>, created: &str) -> ConversationMessage {
        ConversationMessage {
            message_block_id: format!("mb-{suffix}"),
            session_id: session.map(String::from),
            tenant: "local".to_string(),
            caller_agent: "claude-code".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            line_number: 1,
            block_index: 0,
            message_uuid: None,
            role: MessageRole::Assistant,
            block_type: BlockType::Text,
            content: format!("c-{suffix}"),
            tool_name: None,
            tool_use_id: None,
            embed_eligible: true,
            created_at: created.to_string(),
        }
    }

    #[test]
    fn rrf_only_no_session_no_anchor() {
        // Single candidate with NULL session, lex rank 1 only.
        let m = sample("a", None, "00000000020260430000");
        let mut lex = HashMap::new();
        lex.insert("mb-a".to_string(), 1);
        let scored = score_candidates(vec![m], &lex, &HashMap::new(), ScoringOpts::default());
        assert_eq!(scored.len(), 1);
        // Expected = rrf(1) [16] + rrf(absent) [0] + cooccurrence [0; no session] + anchor [0] + freshness [6]
        assert_eq!(scored[0].score, 16 + 6);
    }

    #[test]
    fn session_cooccurrence_caps_at_4() {
        // 6 candidates in same session. Each should see up to 4 siblings (cap), not 5.
        let candidates: Vec<_> = (0..6)
            .map(|i| sample(&format!("s{i}"), Some("S"), "00000000020260430000"))
            .collect();
        let scored = score_candidates(
            candidates,
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts::default(),
        );
        // No RRF, but each gets cap*per_sibling = 4*3 = 12 + freshness (all equal → 6)
        for sb in &scored {
            assert_eq!(sb.score, 12 + 6, "co-occ should cap at 4 siblings");
        }
    }

    #[test]
    fn anchor_session_boost_applies_only_when_match() {
        let a1 = sample("a1", Some("A"), "00000000020260430000");
        let b1 = sample("b1", Some("B"), "00000000020260430000");
        let opts = ScoringOpts {
            anchor_session_id: Some("A"),
        };
        let scored = score_candidates(vec![a1, b1], &HashMap::new(), &HashMap::new(), opts);
        // a1 gets anchor bonus; b1 does not. Both have 0 co-occ (only 1 in their session).
        let a_score = scored
            .iter()
            .find(|s| s.message.message_block_id == "mb-a1")
            .unwrap()
            .score;
        let b_score = scored
            .iter()
            .find(|s| s.message.message_block_id == "mb-b1")
            .unwrap()
            .score;
        assert_eq!(a_score, 20 + 6); // anchor + freshness
        assert_eq!(b_score, 6); // freshness only
    }

    #[test]
    // Tripwire test: the assertion is intentionally over `pub const` values.
    // If a future maintainer rebalances the constants and breaks the
    // invariant, this test must fail at compile/run time — that's the whole
    // point. Suppressing `assertions_on_constants` here is deliberate.
    #[allow(clippy::assertions_on_constants)]
    fn magnitude_anchor_dominates_cooccurrence() {
        // Invariant: ANCHOR_SESSION_BONUS > SESSION_COOCC_PER_SIBLING * SESSION_COOCC_CAP_SIBLINGS
        // (i.e., a single anchor hit outweighs the maximum co-occurrence boost).
        // Changing constants without updating each other will fail this test.
        assert!(
            ANCHOR_SESSION_BONUS > SESSION_COOCC_PER_SIBLING * SESSION_COOCC_CAP_SIBLINGS,
            "ANCHOR_SESSION_BONUS ({}) must exceed max co-occ ({}*{})",
            ANCHOR_SESSION_BONUS,
            SESSION_COOCC_PER_SIBLING,
            SESSION_COOCC_CAP_SIBLINGS,
        );
    }

    #[test]
    fn freshness_decays_old_below_new_at_equal_rrf() {
        // Two candidates with identical lex rank; older one scores lower.
        let new = sample("new", None, "00000000020260430000");
        let mut old = sample("old", None, "00000000020260420000"); // 10 buckets earlier
        old.line_number = 2;
        let mut lex = HashMap::new();
        lex.insert("mb-new".to_string(), 1);
        lex.insert("mb-old".to_string(), 1);
        let scored = score_candidates(
            vec![new, old],
            &lex,
            &HashMap::new(),
            ScoringOpts::default(),
        );
        let new_s = scored
            .iter()
            .find(|s| s.message.message_block_id == "mb-new")
            .unwrap()
            .score;
        let old_s = scored
            .iter()
            .find(|s| s.message.message_block_id == "mb-old")
            .unwrap()
            .score;
        assert!(
            new_s > old_s,
            "newer candidate must outrank older at equal RRF"
        );
    }
}
