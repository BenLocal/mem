//! `SynthesisBackend` — pluggable content synthesis for generative
//! evolution operators (doc `docs/evolution-worker.md` §6.2).
//!
//! E1 ships exactly one implementation: [`ReviewSynthesisBackend`],
//! which performs NO generation. It assembles a structured
//! raw-material document (member ids + summaries + shared topics)
//! that lands in the pending-review queue as a `PendingConfirmation`
//! proposal capsule; a human — or the interactive agent driving the
//! review surface — writes the actual generalized principle via
//! `review_edit_accept`. The worker stays LLM-free in every mode.
//!
//! `local` / `api` backends are designed in the doc but deliberately
//! unimplemented here; `EvolutionSettings::from_env_vars` rejects
//! them at parse time so they cannot be selected silently.

use crate::domain::capability_capsule::CapabilityCapsuleRecord;

/// One synthesis request. ① merge stays non-generative (keep-longest
/// selection, no new content); ②③④ each carry the raw material their
/// review placeholder needs.
#[derive(Debug)]
pub enum SynthesisTask<'a> {
    /// Abstract N episodic capsules into one semantic principle.
    Generalize {
        sources: &'a [&'a CapabilityCapsuleRecord],
        shared_topics: &'a [String],
    },
    /// ③ (E5): revise one contradicted-but-still-valuable capsule.
    /// `conflicts` names the signals that fired (e.g. a hanging
    /// `suspected_supersede` edge, accumulated `outdated` feedback).
    Refine {
        source: &'a CapabilityCapsuleRecord,
        conflicts: &'a [String],
    },
    /// ④ (E5): split one multi-topic capsule. `chunk_groups` is the
    /// per-group chunk-index assignment from the embedding map (group
    /// per proposed new capsule).
    Split {
        source: &'a CapabilityCapsuleRecord,
        chunk_groups: &'a [Vec<usize>],
    },
}

/// Synthesized proposal body for the placeholder capsule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SynthesizedProposal {
    /// Capsule `content` — for the review backend this is structured
    /// raw material, never generated prose.
    pub content: String,
    /// Capsule `summary` (index/hint only, verbatim rule).
    pub summary: String,
}

pub trait SynthesisBackend: Send + Sync {
    fn synthesize(&self, task: &SynthesisTask<'_>) -> SynthesizedProposal;
}

/// The E1 default: defer generation to the pending-review queue.
pub struct ReviewSynthesisBackend;

impl SynthesisBackend for ReviewSynthesisBackend {
    fn synthesize(&self, task: &SynthesisTask<'_>) -> SynthesizedProposal {
        match task {
            SynthesisTask::Generalize {
                sources,
                shared_topics,
            } => {
                let mut content = String::new();
                content.push_str(
                    "EVOLUTION PROPOSAL — generalize (episodic → semantic)\n\
                     Review task: read the source capsules below and write ONE \
                     general principle that they jointly support, then accept via \
                     review_edit_accept. Sources stay Active — the principle \
                     complements them, it does not replace them.\n\n",
                );
                content.push_str(&format!("Shared topics: {}\n\n", shared_topics.join(", ")));
                content.push_str("Source capsules (id — summary):\n");
                for s in sources.iter() {
                    content.push_str(&format!("- {} — {}\n", s.capability_capsule_id, s.summary,));
                }
                let summary = format!(
                    "[evolution:generalize] proposal over {} capsules ({})",
                    sources.len(),
                    shared_topics.join("/"),
                );
                SynthesizedProposal { content, summary }
            }
            SynthesisTask::Refine { source, conflicts } => {
                // Verbatim rule (generalize precedent): reference the
                // source by id + summary, never copy its full content —
                // the reviewer fetches it via capability_capsule_get.
                let mut content = String::new();
                content.push_str(
                    "EVOLUTION PROPOSAL — refine (contradicted but still recalled)\n\
                     Review task: fetch the source capsule below, reconcile it \
                     with the conflict evidence, and write the corrected version \
                     via review_edit_accept. To retire the outdated original, \
                     supersede it explicitly afterwards (capability_capsule_supersede) \
                     — this proposal does NOT touch the source.\n\n",
                );
                content.push_str(&format!(
                    "Source capsule: {} — {}\n\nConflict evidence:\n",
                    source.capability_capsule_id, source.summary,
                ));
                for c in conflicts.iter() {
                    content.push_str(&format!("- {c}\n"));
                }
                let summary = format!(
                    "[evolution:refine] proposal for {} ({} conflict signal(s))",
                    source.capability_capsule_id,
                    conflicts.len(),
                );
                SynthesizedProposal { content, summary }
            }
            SynthesisTask::Split {
                source,
                chunk_groups,
            } => {
                let mut content = String::new();
                content.push_str(
                    "EVOLUTION PROPOSAL — split (one capsule carries multiple topics)\n\
                     Review task: fetch the source capsule below; its embedding \
                     chunks separate into the groups listed here. Write one \
                     focused capsule per group (ingest them directly), then \
                     supersede or archive the source explicitly — this proposal \
                     does NOT touch it.\n\n",
                );
                content.push_str(&format!(
                    "Source capsule: {} — {}\n\nChunk groups (0-based chunk indices):\n",
                    source.capability_capsule_id, source.summary,
                ));
                for (i, group) in chunk_groups.iter().enumerate() {
                    content.push_str(&format!("- group {}: chunks {:?}\n", i + 1, group));
                }
                let summary = format!(
                    "[evolution:split] proposal for {} into {} parts",
                    source.capability_capsule_id,
                    chunk_groups.len(),
                );
                SynthesizedProposal { content, summary }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability_capsule::{
        CapabilityCapsuleStatus, CapabilityCapsuleType, Scope, Visibility,
    };

    fn source(id: &str, summary: &str) -> CapabilityCapsuleRecord {
        CapabilityCapsuleRecord {
            capability_capsule_id: id.into(),
            tenant: "local".into(),
            capability_capsule_type: CapabilityCapsuleType::Experience,
            status: CapabilityCapsuleStatus::Active,
            scope: Scope::Repo,
            visibility: Visibility::Shared,
            version: 1,
            summary: summary.into(),
            content: format!("verbatim content of {id}"),
            evidence: vec![],
            code_refs: vec![],
            project: Some("mem".into()),
            repo: None,
            module: None,
            task_type: None,
            tags: vec![],
            topics: vec!["rust".into(), "lance".into()],
            confidence: 0.7,
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
            expires_at: None,
        }
    }

    #[test]
    fn review_backend_emits_structured_raw_material_not_prose() {
        let a = source("mem_a", "lesson about lance writes");
        let b = source("mem_b", "lesson about duckdb refresh");
        let sources = [&a, &b];
        let topics = vec!["rust".to_string(), "lance".to_string()];
        let got = ReviewSynthesisBackend.synthesize(&SynthesisTask::Generalize {
            sources: &sources,
            shared_topics: &topics,
        });
        // Every source id and summary is present (raw material for the
        // reviewer), as are the shared topics.
        assert!(got.content.contains("mem_a"));
        assert!(got.content.contains("mem_b"));
        assert!(got.content.contains("lesson about lance writes"));
        assert!(got.content.contains("lesson about duckdb refresh"));
        assert!(got.content.contains("rust"));
        assert!(got.content.contains("lance"));
        // Verbatim rule: the placeholder must NOT copy any source's
        // full `content` (that would smell like synthesized output —
        // summaries are the index hints, content stays at the source).
        assert!(!got.content.contains("verbatim content of mem_a"));
        // Summary marks this as an evolution proposal needing review.
        assert!(got.summary.contains("[evolution:generalize]"));
        // Deterministic — same input, same output (no model in the loop).
        let again = ReviewSynthesisBackend.synthesize(&SynthesisTask::Generalize {
            sources: &sources,
            shared_topics: &topics,
        });
        assert_eq!(got, again);
    }
}
