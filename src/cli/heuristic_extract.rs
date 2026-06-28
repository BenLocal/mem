//! O7(b) — zero-LLM heuristic extraction lane.
//!
//! Scans *untagged* conversation text for high-signal sentences (decision /
//! causal / error→fix / code-reference / known-entity) and returns them as
//! candidate memory strings. Everything returned here is ingested by the miner
//! as **`PendingConfirmation`** (review-gated) — NEVER an `Active` memory.
//!
//! That review gate is the deliberate guard against the pre-2026-05-08 prose-cue
//! extractor that was *removed* (see `cli/mine.rs` header) for minting spurious
//! **active** memories off meta-mentions of the cues. The lesson applied here:
//! opt-in (default off) + high-precision cues + a hard garbage filter + every
//! hit lands in the review queue, where one `review_reject` discards a noise
//! row. Zero LLM, zero new model — pure string heuristics + the existing entity
//! normalizer.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashSet;

use crate::pipeline::entity_normalize::normalize_alias;

/// Max candidates surfaced per text block — keeps a chatty turn from flooding
/// the review queue. The reviewer sees the strongest few, not every sentence.
const MAX_PER_BLOCK: usize = 2;
/// Candidate length window (chars). Lower bound mirrors `mine::looks_like_real_memory`
/// (≥12, ≥4 substantive); upper bound drops rambling non-fact paragraphs.
const MIN_LEN: usize = 12;
const MAX_LEN: usize = 400;
const MIN_SUBSTANTIVE: usize = 4;

/// Chinese (substring) decision / causal / error→fix cues. CJK has no word
/// boundaries, so substring match is the right tool here.
const CN_CUES: &[&str] = &[
    "决定",
    "选择",
    "采用",
    "改用",
    "因为",
    "由于",
    "导致",
    "的原因",
    "修复",
    "解决了",
    "报错",
    "踩坑",
    "坑是",
    "结论是",
];

/// English (lowercased substring) cues. Trailing spaces avoid matching inside
/// longer unrelated words (e.g. `because ` not `becausexyz`).
const EN_CUES: &[&str] = &[
    "decided to",
    "we'll use",
    "we will use",
    "let's use",
    "chose ",
    "going with",
    "because ",
    "due to",
    "root cause",
    "fixed by",
    "resolved by",
    "the fix is",
    "the fix was",
    "workaround",
    "gotcha",
    "turns out",
];

/// File-path-with-extension reference (a strong "this sentence names code" cue).
static FILE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"[\w./-]+\.(rs|ts|tsx|js|jsx|py|go|toml|json|md|sql|sh|yaml|yml)\b").unwrap()
});

/// Extract the high-signal candidate sentences from one block of text.
/// `entity_aliases` (may be empty) are pre-known canonical/alias strings; a
/// sentence that names one is treated as high-signal. Deterministic, side-effect
/// free → unit-testable without a store.
pub fn heuristic_candidates(text: &str, entity_aliases: &[String]) -> Vec<String> {
    let norm_aliases: Vec<String> = entity_aliases
        .iter()
        .map(|a| normalize_alias(a))
        .filter(|a| !a.is_empty())
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in split_sentences(text) {
        let s = raw.trim();
        if !is_candidate_len(s) {
            continue;
        }
        if !has_cue(s, &norm_aliases) {
            continue;
        }
        if seen.insert(s.to_string()) {
            out.push(s.to_string());
            if out.len() >= MAX_PER_BLOCK {
                break;
            }
        }
    }
    out
}

/// Sentence boundary: any CJK terminator / newline, OR an ASCII `.!?`
/// *followed by whitespace*. The whitespace guard is load-bearing — splitting
/// on every `.` shreds file paths (`decay.rs`), versions (`0.6`), and `::`-free
/// module names, killing the code-ref cue.
static SENT_SPLIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"[。！？\n\r]+|[.!?]\s+").unwrap());

fn split_sentences(text: &str) -> impl Iterator<Item = &str> + '_ {
    SENT_SPLIT.split(text)
}

fn is_candidate_len(s: &str) -> bool {
    let n = s.chars().count();
    if !(MIN_LEN..=MAX_LEN).contains(&n) {
        return false;
    }
    s.chars().filter(|c| c.is_alphanumeric()).count() >= MIN_SUBSTANTIVE
}

fn has_cue(s: &str, norm_aliases: &[String]) -> bool {
    if CN_CUES.iter().any(|c| s.contains(c)) {
        return true;
    }
    let lower = s.to_lowercase();
    if EN_CUES.iter().any(|c| lower.contains(c)) {
        return true;
    }
    if has_code_ref(s) {
        return true;
    }
    if !norm_aliases.is_empty() {
        let norm = normalize_alias(s);
        if norm_aliases.iter().any(|a| norm.contains(a.as_str())) {
            return true;
        }
    }
    false
}

fn has_code_ref(s: &str) -> bool {
    s.contains('`') || s.contains("::") || s.contains("()") || FILE_RE.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_decision_cue() {
        let c = heuristic_candidates("我们最终决定采用 Lance 作为本地存储后端", &[]);
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("决定采用 Lance"));
    }

    #[test]
    fn detects_causal_cue() {
        let c = heuristic_candidates(
            "default vacuum stays off because aggressive prune deletes a referenced manifest",
            &[],
        );
        assert_eq!(c.len(), 1, "got {c:?}");
    }

    #[test]
    fn detects_error_fix_cue() {
        let c = heuristic_candidates("报错 Text file busy，最后改用 rm 加 cp 绕过去解决了", &[]);
        assert_eq!(c.len(), 1, "got {c:?}");
    }

    #[test]
    fn detects_code_ref_cue() {
        let c = heuristic_candidates(
            "The retry lives in src/storage/lance_store/decay.rs and runs on update",
            &[],
        );
        assert_eq!(c.len(), 1, "got {c:?}");
    }

    #[test]
    fn detects_known_entity_cue() {
        // No decision/causal/code cue, but it names a known entity.
        let c = heuristic_candidates(
            "the maintenance window for ProjectFalcon spans the whole weekend",
            &["projectfalcon".to_string()],
        );
        assert_eq!(c.len(), 1, "got {c:?}");
        // Same text with no known entities → no cue → dropped.
        assert!(heuristic_candidates(
            "the maintenance window for ProjectFalcon spans the whole weekend",
            &[]
        )
        .is_empty());
    }

    #[test]
    fn skips_low_signal_and_short() {
        // No cue.
        assert!(heuristic_candidates("the weather is nice today and i went out", &[]).is_empty());
        // Too short even with a cue.
        assert!(heuristic_candidates("因为", &[]).is_empty());
    }

    #[test]
    fn caps_per_block_and_dedups() {
        // Four distinct ≥12-char decision sentences; only MAX_PER_BLOCK surface.
        let text = "团队决定采用甲方案来处理。团队决定采用乙方案来处理。团队决定采用丙方案来处理。团队决定采用丁方案来处理";
        let c = heuristic_candidates(text, &[]);
        assert_eq!(
            c.len(),
            MAX_PER_BLOCK,
            "must cap at {MAX_PER_BLOCK}, got {c:?}"
        );

        let dup = "我们决定采用方案甲来落地实现。我们决定采用方案甲来落地实现";
        let cd = heuristic_candidates(dup, &[]);
        assert_eq!(cd.len(), 1, "identical sentences dedup, got {cd:?}");
    }
}
