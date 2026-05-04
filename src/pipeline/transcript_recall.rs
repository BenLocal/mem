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
///
/// Default values produce production behavior (no signals disabled). Bench
/// callers (`tests/recall_bench.rs`) toggle the `disable_*` fields per-rung
/// to measure each signal's marginal contribution.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScoringOpts<'a> {
    pub anchor_session_id: Option<&'a str>,
    pub disable_session_cooc: bool,
    pub disable_anchor: bool,
    pub disable_freshness: bool,
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
            if !opts.disable_session_cooc {
                if let Some(sid) = m.session_id.as_deref() {
                    let total = *session_counts.get(sid).unwrap_or(&0);
                    let siblings = (total - 1).clamp(0, SESSION_COOCC_CAP_SIBLINGS);
                    s += SESSION_COOCC_PER_SIBLING * siblings;
                }
            }

            // Anchor session boost.
            if !opts.disable_anchor {
                if let (Some(anchor), Some(sid)) = (opts.anchor_session_id, m.session_id.as_deref())
                {
                    if anchor == sid {
                        s += ANCHOR_SESSION_BONUS;
                    }
                }
            }

            // Freshness curve.
            let ts = timestamp_score(&m.created_at);
            if !opts.disable_freshness {
                s += freshness_score(newest, ts);
            }

            ScoredBlock {
                message: m,
                score: s,
            }
        })
        .collect();

    // Sort by score descending; break ties by message_block_id ascending so
    // that output order is deterministic regardless of HashMap iteration order
    // in callers (e.g. bench runner). Determinism matters for reproducible
    // bench runs and for stable production rankings when scores collide.
    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.message.message_block_id.cmp(&b.message.message_block_id))
    });
    scored
}

// ── Window assembly types

/// A primary hit with its hydrated context neighbors. The `primary` carries
/// its scoring context (`ScoredBlock`); `before` and `after` are
/// chronologically adjacent same-session blocks supplied by
/// [`crate::storage::DuckDbRepository::context_window_for_block`].
#[derive(Debug, Clone)]
pub struct PrimaryWithContext {
    pub primary: ScoredBlock,
    pub before: Vec<ConversationMessage>,
    pub after: Vec<ConversationMessage>,
}

/// Output of the window-merge phase: one or more primaries sharing a
/// session, surrounded by their union'd context. `score` is the maximum of
/// `primary_scores` values; `blocks` is chronologically sorted with
/// duplicates removed.
#[derive(Debug, Clone)]
pub struct MergedWindow {
    pub session_id: Option<String>,
    pub blocks: Vec<ConversationMessage>,
    pub primary_ids: Vec<String>,
    pub primary_scores: HashMap<String, i64>,
    pub score: i64,
}

/// Merge `PrimaryWithContext` items into windows. Two primaries belong in
/// the same window iff they share `session_id` AND their full
/// `(before, primary, after)` chronological ranges overlap or touch
/// (last block of the earlier item's window >= first block of the later
/// item's window in `(created_at, line_number, block_index)` order).
///
/// `session_id == None` primaries each get their own window — no merging
/// across NULL.
///
/// Output windows are sorted by `score` descending.
pub fn merge_windows(items: Vec<PrimaryWithContext>) -> Vec<MergedWindow> {
    if items.is_empty() {
        return vec![];
    }

    // Bucket by session_id; NULL gets its own bucket per primary.
    let mut by_session: HashMap<Option<String>, Vec<PrimaryWithContext>> = HashMap::new();
    for item in items {
        let key = item.primary.message.session_id.clone();
        by_session.entry(key).or_default().push(item);
    }

    let mut windows: Vec<MergedWindow> = Vec::new();
    for (session, mut group) in by_session {
        if session.is_none() {
            // No merging for NULL sessions — each item is its own window.
            for item in group {
                windows.push(single_window(None, item));
            }
            continue;
        }

        // Sort the group by primary timestamp for left-to-right merging.
        group.sort_by(|a, b| {
            timestamp_score(&a.primary.message.created_at)
                .cmp(&timestamp_score(&b.primary.message.created_at))
        });

        // Sweep: maintain a "current" merged window; if the next item's
        // window-range start <= current's window-range end, merge.
        let mut current: Option<MergedWindow> = None;
        for item in group {
            let item_window = single_window(session.clone(), item);

            match current.take() {
                None => current = Some(item_window),
                Some(existing) => {
                    if windows_overlap(&existing, &item_window) {
                        current = Some(merge_two(existing, item_window));
                    } else {
                        windows.push(existing);
                        current = Some(item_window);
                    }
                }
            }
        }
        if let Some(w) = current {
            windows.push(w);
        }
    }

    windows.sort_by(|a, b| b.score.cmp(&a.score));
    windows
}

fn single_window(session: Option<String>, item: PrimaryWithContext) -> MergedWindow {
    let mut blocks = item.before;
    blocks.push(item.primary.message.clone());
    blocks.extend(item.after);
    let primary_id = item.primary.message.message_block_id.clone();
    let mut scores = HashMap::new();
    scores.insert(primary_id.clone(), item.primary.score);
    MergedWindow {
        session_id: session,
        blocks,
        primary_ids: vec![primary_id],
        primary_scores: scores,
        score: item.primary.score,
    }
}

fn windows_overlap(a: &MergedWindow, b: &MergedWindow) -> bool {
    // Both windows are time-sorted; compare the last block of `a` to the
    // first block of `b`. Overlap = `b`'s first ts <= `a`'s last ts.
    let a_last = a.blocks.last().expect("non-empty window");
    let b_first = b.blocks.first().expect("non-empty window");
    timestamp_score(&b_first.created_at) <= timestamp_score(&a_last.created_at)
}

fn merge_two(a: MergedWindow, b: MergedWindow) -> MergedWindow {
    // Merge block lists, dedup by message_block_id, sort by
    // (created_at, line_number, block_index).
    let mut all_blocks = a.blocks;
    all_blocks.extend(b.blocks);
    all_blocks.sort_by(|x, y| {
        let tx = timestamp_score(&x.created_at);
        let ty = timestamp_score(&y.created_at);
        tx.cmp(&ty)
            .then(x.line_number.cmp(&y.line_number))
            .then(x.block_index.cmp(&y.block_index))
    });
    all_blocks.dedup_by(|x, y| x.message_block_id == y.message_block_id);

    let mut primary_ids = a.primary_ids;
    primary_ids.extend(b.primary_ids);
    let mut primary_scores = a.primary_scores;
    primary_scores.extend(b.primary_scores);
    let score = primary_scores.values().copied().max().unwrap_or(0);

    MergedWindow {
        session_id: a.session_id,
        blocks: all_blocks,
        primary_ids,
        primary_scores,
        score,
    }
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
            ..ScoringOpts::default()
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

    #[test]
    fn disable_session_cooc_zeroes_that_bonus() {
        // Two siblings in same session — without disabling, cooc bonus = 1*per_sibling
        // = 3. With disable_session_cooc=true, bonus is 0. Verify the score delta.
        let m_a = sample("a1", Some("s1"), "00000000020260503000");
        let m_b = sample("b1", Some("s1"), "00000000020260503000");

        let with_cooc = score_candidates(
            vec![m_a.clone(), m_b.clone()],
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts::default(),
        );
        let without_cooc = score_candidates(
            vec![m_a, m_b],
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts {
                disable_session_cooc: true,
                ..ScoringOpts::default()
            },
        );
        assert!(
            with_cooc[0].score > without_cooc[0].score,
            "disabling cooc must lower score (got {} vs {})",
            with_cooc[0].score,
            without_cooc[0].score
        );
        assert_eq!(
            with_cooc[0].score - without_cooc[0].score,
            SESSION_COOCC_PER_SIBLING,
            "cooc bonus delta should equal SESSION_COOCC_PER_SIBLING"
        );
    }

    #[test]
    fn disable_anchor_zeroes_anchor_bonus() {
        let m = sample("a1", Some("s_anchor"), "00000000020260503000");
        let with_anchor = score_candidates(
            vec![m.clone()],
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts {
                anchor_session_id: Some("s_anchor"),
                ..ScoringOpts::default()
            },
        );
        let disabled = score_candidates(
            vec![m],
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts {
                anchor_session_id: Some("s_anchor"),
                disable_anchor: true,
                ..ScoringOpts::default()
            },
        );
        assert_eq!(
            with_anchor[0].score - disabled[0].score,
            ANCHOR_SESSION_BONUS
        );
    }

    #[test]
    fn disable_freshness_zeroes_freshness_bonus() {
        // Two timestamps; the older one's freshness < newer one's. Both should
        // converge to the same score when disable_freshness=true.
        let m_new = sample("new", None, "00000000020260503000");
        let m_old = sample("old", None, "00000000020260403000");

        let with_fresh = score_candidates(
            vec![m_new.clone(), m_old.clone()],
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts::default(),
        );
        let without_fresh = score_candidates(
            vec![m_new, m_old],
            &HashMap::new(),
            &HashMap::new(),
            ScoringOpts {
                disable_freshness: true,
                ..ScoringOpts::default()
            },
        );
        let with_diff = with_fresh
            .iter()
            .find(|s| s.message.message_block_id == "mb-new")
            .unwrap()
            .score
            - with_fresh
                .iter()
                .find(|s| s.message.message_block_id == "mb-old")
                .unwrap()
                .score;
        let without_diff = without_fresh
            .iter()
            .find(|s| s.message.message_block_id == "mb-new")
            .unwrap()
            .score
            - without_fresh
                .iter()
                .find(|s| s.message.message_block_id == "mb-old")
                .unwrap()
                .score;
        assert!(with_diff > 0, "with freshness, newer must outrank older");
        assert_eq!(
            without_diff, 0,
            "with freshness disabled, both candidates must score equally"
        );
    }

    #[test]
    fn score_candidates_breaks_ties_deterministically() {
        // Two messages with identical scoring inputs (same session, same timestamp,
        // same RRF ranks) — only message_block_id differs ("aaa" vs "bbb").
        // After scoring, the lower message_block_id ("aaa") should come first
        // regardless of the order they are passed in. Run twice (reversed input)
        // to confirm the tiebreak is stable and not input-order-dependent.
        let m_aaa = sample("aaa", Some("s1"), "00000000020260503000");
        let m_bbb = sample("bbb", Some("s1"), "00000000020260503000");

        let mut lex = HashMap::new();
        lex.insert("mb-aaa".to_string(), 1);
        lex.insert("mb-bbb".to_string(), 1);

        let run1 = score_candidates(
            vec![m_aaa.clone(), m_bbb.clone()],
            &lex,
            &HashMap::new(),
            ScoringOpts::default(),
        );
        let run2 = score_candidates(
            vec![m_bbb.clone(), m_aaa.clone()],
            &lex,
            &HashMap::new(),
            ScoringOpts::default(),
        );

        // Both runs must produce ["mb-aaa", "mb-bbb"] — lower id first.
        assert_eq!(run1[0].message.message_block_id, "mb-aaa");
        assert_eq!(run1[1].message.message_block_id, "mb-bbb");
        assert_eq!(run2[0].message.message_block_id, "mb-aaa");
        assert_eq!(run2[1].message.message_block_id, "mb-bbb");
        // Sanity: scores are equal (the tiebreak is only on id).
        assert_eq!(run1[0].score, run1[1].score);
    }
}

#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::domain::{BlockType, MessageRole};

    fn block(suffix: &str, session: &str, created: &str, line: u64) -> ConversationMessage {
        ConversationMessage {
            message_block_id: format!("mb-{suffix}"),
            session_id: Some(session.to_string()),
            tenant: "local".to_string(),
            caller_agent: "claude-code".to_string(),
            transcript_path: "/tmp/t.jsonl".to_string(),
            line_number: line,
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

    fn pwc(
        primary_suffix: &str,
        session: &str,
        created: &str,
        line: u64,
        score: i64,
    ) -> PrimaryWithContext {
        PrimaryWithContext {
            primary: ScoredBlock {
                message: block(primary_suffix, session, created, line),
                score,
            },
            before: vec![],
            after: vec![],
        }
    }

    #[test]
    fn single_primary_no_overlap_one_window() {
        let item = pwc("p1", "S1", "00000000020260430000", 5, 30);
        let windows = merge_windows(vec![item]);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].primary_ids, vec!["mb-p1"]);
        assert_eq!(windows[0].score, 30);
        assert_eq!(windows[0].blocks.len(), 1);
    }

    #[test]
    fn two_primaries_same_session_overlapping_merge() {
        let mut a = pwc("a", "S1", "00000000020260430010", 5, 30);
        let mut b = pwc("b", "S1", "00000000020260430011", 6, 25);
        // Make their context ranges overlap: a's `after` includes b, b's `before` includes a.
        a.after = vec![block("b", "S1", "00000000020260430011", 6)];
        b.before = vec![block("a", "S1", "00000000020260430010", 5)];
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 1, "overlapping primaries should merge");
        let mut ids = windows[0].primary_ids.clone();
        ids.sort();
        assert_eq!(ids, vec!["mb-a", "mb-b"]);
        assert_eq!(windows[0].score, 30, "merged score = max(primary scores)");
    }

    #[test]
    fn two_primaries_different_session_dont_merge() {
        let a = pwc("a", "S1", "00000000020260430010", 5, 30);
        let b = pwc("b", "S2", "00000000020260430011", 6, 25);
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 2);
    }

    #[test]
    fn two_primaries_same_session_far_apart_dont_merge() {
        // Both in S1 but with no temporal overlap in their before/after ranges.
        let a = pwc("a", "S1", "00000000020260430010", 1, 30); // no after
        let b = pwc("b", "S1", "00000000020260430999", 2, 25); // no before
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 2);
    }

    #[test]
    fn merged_window_blocks_dedup_and_time_sorted() {
        // Two primaries' contexts share one block ("shared").
        let a = PrimaryWithContext {
            primary: ScoredBlock {
                message: block("a", "S1", "00000000020260430010", 1),
                score: 30,
            },
            before: vec![],
            after: vec![block("shared", "S1", "00000000020260430011", 2)],
        };
        let b = PrimaryWithContext {
            primary: ScoredBlock {
                message: block("b", "S1", "00000000020260430012", 3),
                score: 25,
            },
            before: vec![block("shared", "S1", "00000000020260430011", 2)],
            after: vec![],
        };
        let windows = merge_windows(vec![a, b]);
        assert_eq!(windows.len(), 1);
        let ids: Vec<&str> = windows[0]
            .blocks
            .iter()
            .map(|b| b.message_block_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["mb-a", "mb-shared", "mb-b"],
            "dedup'd and time-sorted"
        );
    }

    #[test]
    fn windows_sorted_by_score_descending() {
        let low = pwc("low", "S1", "00000000020260430010", 1, 10);
        let high = pwc("high", "S2", "00000000020260430011", 1, 50);
        let windows = merge_windows(vec![low, high]);
        assert_eq!(windows[0].primary_ids, vec!["mb-high"]);
        assert_eq!(windows[1].primary_ids, vec!["mb-low"]);
    }
}
