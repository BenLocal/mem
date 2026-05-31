//! Pre-ingest sanity check against the entity registry + KG.
//!
//! mempalace's `fact_checker.py` analogue — minus the LLM. Three
//! structured signals, all derived from `EntityRegistry::list_entities`
//! and `graph_edges`, no model calls:
//!
//! 1. **`similar_names`** — token in caller input (a `topics` entry,
//!    or a verbatim token from `content`) is normalized + checked
//!    against existing canonical names / aliases. A near-miss
//!    (Levenshtein ≤ 2, token length ≥ 4) that's *not* already an
//!    exact alias hit is surfaced as a probable typo.
//! 2. **`relationship_conflicts`** — caller asserts `(S, P, O)`; KG
//!    has an *active* `(O, P, S)` with the same predicate →
//!    direction mismatch. (Mem doesn't carry an inverse-predicate
//!    map, so we only flag verbatim-same-predicate reversals to
//!    keep false positives low.)
//! 3. **`kg_contradictions`** — caller asserts `(S, P, O)`; KG has
//!    an active `(S, P, X)` with `X ≠ O` → value-changed
//!    contradiction. Closed `(S, P, O)` (i.e. caller restates a
//!    previously-invalidated fact) is also flagged here.
//!
//! Pure read; no writes; caller decides whether to act on the
//! report (typically: surface to the human reviewer, or accept).
//!
//! ## Why ≤ 2 / len ≥ 4
//!
//! Single-char-edit thresholds blow up on short tokens — `"a"` is
//! Levenshtein-1 from `"b"`, etc. Restricting to tokens of length 4+
//! and edit distance ≤ 2 strikes the usual balance for human-name
//! typo detection without flooding the report with trivia.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::capability_capsule::GraphEdge;
use crate::domain::EntityKind;
use crate::pipeline::entity_normalize::normalize_alias;
use crate::storage::{Backend, GraphError, StorageError};

#[derive(Debug, Error)]
pub enum FactCheckError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Graph(#[from] GraphError),
}

/// Lowest token length we'll subject to fuzzy-typo matching.
/// Anything shorter has too many collisions at distance ≤ 2.
const TYPO_MIN_TOKEN_LEN: usize = 4;

/// Max Levenshtein distance we treat as "probably the same name
/// misspelled." Two edits covers the common cases (`Alic` ↔ `Alice`,
/// `Pheonix` ↔ `Phoenix`) without straying into unrelated words.
const TYPO_MAX_DISTANCE: usize = 2;

/// Cap on `list_entities` calls — keeps the per-request worst case
/// bounded on tenants with very large registries. Tunable but
/// hardcoded for v1 (no env knob until a real corpus needs it).
const ENTITY_SCAN_LIMIT: usize = 1000;

/// Caller-supplied check input. `content` is read for token-level
/// typo scanning; `topics` are checked separately (and weighted
/// higher implicitly because they're explicit caller intent).
/// `relationships` carry the structured triples the caller wants
/// cross-checked against KG state — mem has no LLM to extract them
/// from prose, so the contract is "caller passes what they want
/// verified."
#[derive(Debug, Clone, Deserialize)]
pub struct FactCheckRequest {
    pub tenant: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub relationships: Vec<RelationshipTriple>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RelationshipTriple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FactCheckReport {
    pub similar_names: Vec<SimilarNameMatch>,
    pub relationship_conflicts: Vec<RelationshipConflict>,
    pub kg_contradictions: Vec<KgContradiction>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SimilarNameMatch {
    /// The verbatim caller token that looked typo-ish.
    pub in_input: String,
    pub matches: Vec<EntitySuggestion>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EntitySuggestion {
    pub entity_id: String,
    pub canonical_name: String,
    pub kind: EntityKind,
    pub edit_distance: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RelationshipConflict {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub existing_edge: GraphEdge,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KgContradiction {
    pub claim: String,
    pub existing: GraphEdge,
    pub note: String,
}

#[derive(Clone)]
pub struct FactCheckService {
    store: Arc<dyn Backend>,
}

impl FactCheckService {
    pub fn new(store: Arc<dyn Backend>) -> Self {
        Self { store }
    }

    /// Run all three checks. Each list is empty when nothing fires;
    /// the overall call always returns `Ok` (storage errors propagate
    /// — they're not "check found nothing," they're "couldn't run the
    /// check").
    pub async fn check(&self, req: FactCheckRequest) -> Result<FactCheckReport, FactCheckError> {
        let similar_names = self.check_similar_names(&req).await?;
        let (relationship_conflicts, kg_contradictions) = self.check_relationships(&req).await?;
        Ok(FactCheckReport {
            similar_names,
            relationship_conflicts,
            kg_contradictions,
        })
    }

    // ────────────────────────── similar names ──────────────────────────

    async fn check_similar_names(
        &self,
        req: &FactCheckRequest,
    ) -> Result<Vec<SimilarNameMatch>, FactCheckError> {
        let candidates = collect_typo_candidates(&req.content, &req.topics);
        if candidates.is_empty() {
            return Ok(Vec::new());
        }

        // Pre-filter: any candidate that's already an exact alias hit
        // is fine — drop it before scanning.
        let mut needs_scan = Vec::with_capacity(candidates.len());
        for cand in candidates {
            let hit = self.store.lookup_alias(&req.tenant, &cand).await?;
            if hit.is_none() {
                needs_scan.push(cand);
            }
        }
        if needs_scan.is_empty() {
            return Ok(Vec::new());
        }

        // One full entity list; we filter / compare in Rust. v1 caps
        // at ENTITY_SCAN_LIMIT (caller-visible behavior: tenants past
        // that need to upgrade the limit when we add the knob).
        let entities = self
            .store
            .list_entities(&req.tenant, None, None, ENTITY_SCAN_LIMIT)
            .await?;

        let mut out = Vec::new();
        for cand in needs_scan {
            let norm_cand = normalize_alias(&cand);
            if norm_cand.is_empty() {
                continue;
            }
            let mut matches = Vec::new();
            for ent in &entities {
                let norm_canon = normalize_alias(&ent.canonical_name);
                if norm_canon.is_empty() || norm_canon == norm_cand {
                    continue;
                }
                let dist = levenshtein(&norm_cand, &norm_canon);
                if dist <= TYPO_MAX_DISTANCE && dist > 0 {
                    matches.push(EntitySuggestion {
                        entity_id: ent.entity_id.clone(),
                        canonical_name: ent.canonical_name.clone(),
                        kind: ent.kind,
                        edit_distance: dist,
                    });
                }
            }
            if !matches.is_empty() {
                matches.sort_by(|a, b| {
                    a.edit_distance
                        .cmp(&b.edit_distance)
                        .then_with(|| a.canonical_name.cmp(&b.canonical_name))
                });
                out.push(SimilarNameMatch {
                    in_input: cand,
                    matches,
                });
            }
        }
        Ok(out)
    }

    // ────────────────────────── relationships ──────────────────────────

    async fn check_relationships(
        &self,
        req: &FactCheckRequest,
    ) -> Result<(Vec<RelationshipConflict>, Vec<KgContradiction>), FactCheckError> {
        if req.relationships.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        let mut conflicts = Vec::new();
        let mut contradictions = Vec::new();

        for triple in &req.relationships {
            let subj_id = self
                .store
                .lookup_alias(&req.tenant, &triple.subject)
                .await?;
            let obj_id = self.store.lookup_alias(&req.tenant, &triple.object).await?;
            // Both endpoints must resolve to known entities — otherwise
            // there's no KG row that could conflict.
            let (Some(subj_id), Some(obj_id)) = (subj_id, obj_id) else {
                continue;
            };
            let subj_node = format!("entity:{subj_id}");
            let obj_node = format!("entity:{obj_id}");

            // One BFS pull from the subject node covers everything we
            // need: outbound edges (S → *) and inbound edges (* → S).
            // GraphStore::neighbors_within(node, 1, None) returns only
            // active edges by spec, which is exactly the "currently
            // believed" KG view we want to check against. (Closed-
            // edge restatement requires the full timeline view; we do
            // that with a second call only when we have a candidate.)
            let active_neighbors = self.store.neighbors_within(&subj_node, 1, None).await?;

            for edge in &active_neighbors {
                // Direction-reversed identical predicate?
                if edge.relation == triple.predicate
                    && edge.from_node_id == obj_node
                    && edge.to_node_id == subj_node
                {
                    conflicts.push(RelationshipConflict {
                        subject: triple.subject.clone(),
                        predicate: triple.predicate.clone(),
                        object: triple.object.clone(),
                        existing_edge: edge.clone(),
                        note: format!(
                            "direction mismatch: KG has ({}, {}, {}) active",
                            edge.from_node_id, edge.relation, edge.to_node_id,
                        ),
                    });
                }
                // Value-changed contradiction: same subject + predicate,
                // different object.
                if edge.relation == triple.predicate
                    && edge.from_node_id == subj_node
                    && edge.to_node_id != obj_node
                {
                    contradictions.push(KgContradiction {
                        claim: format!("{} {} {}", triple.subject, triple.predicate, triple.object),
                        existing: edge.clone(),
                        note: format!(
                            "active edge ({}, {}, {}) — value differs from claim",
                            edge.from_node_id, edge.relation, edge.to_node_id,
                        ),
                    });
                }
            }

            // Closed-fact restatement: scan the full timeline of the
            // subject node for a *closed* `(S, P, O)` that matches the
            // claim verbatim. (Only run when we have a registered
            // subject — keeps the timeline pull out of the hot path
            // for unknown-subject claims.)
            let timeline = self.store.kg_timeline(&subj_node).await?;
            for edge in timeline {
                if edge.relation == triple.predicate
                    && edge.from_node_id == subj_node
                    && edge.to_node_id == obj_node
                    && edge.valid_to.is_some()
                {
                    contradictions.push(KgContradiction {
                        claim: format!("{} {} {}", triple.subject, triple.predicate, triple.object),
                        existing: edge,
                        note: "restates a previously-invalidated edge".into(),
                    });
                }
            }
        }

        Ok((conflicts, contradictions))
    }
}

// ────────────────────────── helpers ──────────────────────────

/// Pull the set of tokens worth fuzzy-matching from `content` + an
/// explicit `topics` list. Tokens normalize via the same rule used
/// for alias lookup (lowercase + whitespace collapse); duplicates
/// drop out. Tokens shorter than [`TYPO_MIN_TOKEN_LEN`] are skipped.
///
/// Content tokenization is intentionally crude — split on whitespace,
/// strip common punctuation. We're not building an NLP pipeline;
/// we're catching `"Alic Smith"` ≈ `"Alice Smith"`.
fn collect_typo_candidates(content: &str, topics: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let push_if_new = |raw: &str, seen: &mut HashSet<String>, out: &mut Vec<String>| {
        let cleaned = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-');
        if cleaned.len() < TYPO_MIN_TOKEN_LEN {
            return;
        }
        let key = normalize_alias(cleaned);
        if key.is_empty() || key.len() < TYPO_MIN_TOKEN_LEN {
            return;
        }
        if seen.insert(key) {
            out.push(cleaned.to_string());
        }
    };

    for t in topics {
        push_if_new(t, &mut seen, &mut out);
    }
    for token in content.split_whitespace() {
        push_if_new(token, &mut seen, &mut out);
    }
    out
}

/// Classic iterative two-row Levenshtein. Operates on chars (not
/// bytes) so multi-byte UTF-8 counts as one edit, not several.
///
/// `pub(crate)` so the K5 graph-neighbors fuzzy-suggestion path
/// (`CapabilityCapsuleService::graph_neighbor_suggestions`) can reuse
/// it without duplicating the algorithm.
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.is_empty() {
        return b_chars.len();
    }
    if b_chars.is_empty() {
        return a_chars.len();
    }
    let mut prev: Vec<usize> = (0..=b_chars.len()).collect();
    let mut curr: Vec<usize> = vec![0; b_chars.len() + 1];
    for (i, ca) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic_cases() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("a", ""), 1);
        assert_eq!(levenshtein("", "a"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("alice", "alic"), 1);
        assert_eq!(levenshtein("phoenix", "pheonix"), 2);
        assert_eq!(levenshtein("same", "same"), 0);
    }

    #[test]
    fn levenshtein_handles_multibyte() {
        // 中文 vs 中国 = 1 char edit (single replacement), not 3 bytes.
        assert_eq!(levenshtein("中文", "中国"), 1);
        assert_eq!(levenshtein("café", "cafe"), 1);
    }

    #[test]
    fn typo_candidates_dedup_and_length_filter() {
        let content = "Alic Smith met with Bob the cat";
        let topics = vec!["alic".to_string(), "BOB".to_string()];
        let cands = collect_typo_candidates(content, &topics);
        // "alic" appears in both topics and content — should dedup to one.
        // "Bob" / "BOB" / "the" / "cat" all under length 4 → dropped.
        // "Smith" and "with" qualify; "Alic" qualifies via topics.
        let normalized: Vec<String> = cands.iter().map(|c| normalize_alias(c)).collect();
        assert!(normalized.contains(&"alic".to_string()));
        assert!(normalized.contains(&"smith".to_string()));
        assert!(normalized.contains(&"with".to_string()));
        assert!(
            !normalized.contains(&"bob".to_string()),
            "bob should be too short"
        );
        assert!(!normalized.contains(&"the".to_string()));
        assert!(!normalized.contains(&"cat".to_string()));
        // No duplicates after normalization.
        let mut sorted = normalized.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), normalized.len());
    }

    #[test]
    fn typo_candidates_strip_trailing_punct() {
        let content = "Alice, who works on Phoenix.";
        let cands = collect_typo_candidates(content, &[]);
        let normalized: Vec<String> = cands.iter().map(|c| normalize_alias(c)).collect();
        assert!(normalized.contains(&"alice".to_string()));
        assert!(normalized.contains(&"phoenix".to_string()));
    }
}
