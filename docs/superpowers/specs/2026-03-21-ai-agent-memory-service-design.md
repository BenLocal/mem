# AI Agent Memory Service Design

Date: 2026-03-21
Status: Draft

## 1. Overview

This project defines a personal, local-first memory service for multiple AI agents working on code engineering tasks. The service is intended to improve delivery speed, reduce repeated reasoning, and lower token consumption by retrieving compressed, reusable memory instead of replaying large amounts of prior context.

The system is optimized for:

- Reusing business implementation knowledge
- Reusing debugging and execution experience
- Persisting user preferences and long-lived constraints
- Distilling successful workflows into reusable task patterns

The storage model uses DuckDB as the factual and retrieval-oriented memory store, and IndraDB as the relationship and reasoning graph.

## 2. Goals

### Primary goals

- Build a shared memory service that can be used by multiple agents
- Store implementation knowledge from real engineering work
- Store practical experience that speeds up future work
- Store user preferences with confirmation gating
- Summarize successful workflows so they can be reused later
- Reduce token usage by returning compressed, task-relevant memory packs

### Non-goals for v1

- Distributed deployment
- Cross-user multi-tenant SaaS features
- Fully autonomous trust in all generated memory
- Rich UI beyond basic review and maintenance surfaces
- Large-scale event sourcing infrastructure

## 3. High-Level Architecture

The service is organized into five major modules.

### 3.1 Ingest API

Receives write requests from agents and maintenance tools. It accepts candidate memories, review actions, feedback, and graph queries.

### 3.2 Memory Pipeline

Processes candidate memories by performing:

- Memory classification
- Summarization and normalization
- Deduplication and versioning
- Importance and confidence scoring
- Confirmation routing
- Entity and relation extraction
- Workflow extraction from successful episodes

### 3.3 DuckDB Memory Store

DuckDB is the source of truth for memory records and retrieval-oriented data. It stores:

- Canonical memory entries
- Embeddings and retrieval metadata
- Episodes and write events
- Validation and feedback records
- Version links and lifecycle state
- References to code and source evidence

### 3.4 IndraDB Knowledge Graph

IndraDB stores high-value entities and relations used for expansion and re-ranking. It is not the canonical source for original memory text. It stores:

- Projects, repos, modules, services
- Task patterns, issue patterns, decisions
- User preferences and constraints
- Memory-to-entity links
- Contradiction and supersession edges

### 3.5 Retrieval Orchestrator

Handles search and context assembly. It retrieves candidate memories from DuckDB, expands relevant context using the graph, re-ranks results, and returns a compressed context pack sized to the caller's token budget.

## 4. Memory Model

All memory types share common metadata:

- `memory_id`
- `memory_type`
- `status`
- `scope`
- `version`
- `confidence`
- `source_agent`
- `created_at`
- `updated_at`
- `last_validated_at`
- `decay_score`
- `content_hash`
- `supersedes_memory_id`

Recommended lifecycle fields for planning:

- `status`: `pending_confirmation`, `provisional`, `active`, `archived`, `rejected`
- `confidence`: normalized score in `[0.0, 1.0]`
- `decay_score`: normalized score in `[0.0, 1.0]`, where higher means less likely to be recalled by default

### 4.1 Implementation Memory

Stores business implementation knowledge and concrete engineering facts, such as:

- How a feature was implemented
- Module-specific constraints
- Integration details
- Repeated bug-fix patterns with evidence

Key payload fields:

- `summary`
- `evidence`
- `code_refs`
- `project`
- `repo`
- `module`
- `task_type`
- `tags`

Write mode: automatic by default.

### 4.2 Experience Memory

Stores practical execution knowledge, such as:

- Effective debugging order
- Common operational shortcuts
- Repository-specific work habits
- Pitfalls and heuristics

Write mode: automatic by default, with stronger decay and deduplication than implementation memory.

### 4.3 Preference Memory

Stores user preferences and long-lived constraints, such as:

- Communication preferences
- Preferred implementation style
- Refactoring boundaries
- Repo- or project-specific constraints

Write mode: propose-only. Activation requires human confirmation.

### 4.4 Episode Memory

Stores summaries of work sessions, including decisions, attempts, results, and failures. Episode memory acts as source material from which more stable memories are distilled.

Write mode: automatic.

### 4.5 Workflow Memory

Stores reusable success flows for recurring engineering tasks. This is a first-class memory type because one of the system's goals is to capture and reuse successful working processes, not only facts and heuristics.

Example uses:

- Feature delivery workflow for a business domain
- Typical debugging flow for a service area
- Safe edit-and-verify sequence for a repository
- Investigation workflow for a recurring class of failures

Key payload fields:

- `goal`
- `preconditions`
- `steps`
- `decision_points`
- `success_signals`
- `failure_signals`
- `evidence`
- `scope`

Write mode: system-generated candidate plus human confirmation before activation.

## 5. Knowledge Graph Model

The graph should remain selective. v1 should model only high-value nodes and relationships.

### 5.1 Node types

- `Project`
- `Repo`
- `Module`
- `Service`
- `TaskPattern`
- `IssuePattern`
- `Decision`
- `UserPreference`
- `Workflow`
- `Memory`

### 5.2 Relation types

- `applies_to`
- `depends_on`
- `observed_in`
- `fixed_by`
- `derived_from`
- `prefers`
- `contradicts`
- `supersedes`
- `relevant_to`
- `uses_workflow`

The graph is used to explain why a memory is relevant in the current context. It should not attempt to represent every low-value artifact.

## 6. Write Path

v1 uses two write channels.

### 6.1 Automatic write channel

Used for:

- Implementation memory
- Experience memory
- Episode memory

Process:

1. Agent submits candidate memory
2. Pipeline classifies and normalizes it
3. System runs deduplication and conflict checks
4. System assigns confidence and lifecycle status
5. Memory enters DuckDB
6. Related entities and relations are written to IndraDB

New automatic memories may start as `provisional` when quality or evidence is weak.

Recommended state transitions:

- Automatic write: `provisional -> active -> archived`
- Confirmation write: `pending_confirmation -> active | rejected`
- Conflict or obsolescence does not require hard deletion; affected memories may remain `active` with lower rank, or move to `archived` if invalidated

Recommended score semantics:

- `confidence` increases with repeated successful reuse, positive feedback, and stronger evidence
- `confidence` decreases with explicit negative feedback, contradiction by newer evidence, or weak source quality
- `decay_score` increases with staleness, low reuse, or major related code changes
- retrieval should prefer higher `confidence` and lower `decay_score`

### 6.2 Confirmation write channel

Used for:

- Preference memory
- Workflow memory
- High-impact rule-like knowledge

Process:

1. Agent or pipeline proposes a candidate
2. Candidate enters `pending_confirmation`
3. Human accepts, rejects, or edits
4. Accepted candidate becomes `active`

## 7. Conflict, Decay, and Trust

The system must prefer correctness over recall volume.

### 7.1 Duplicate handling

When a new memory is highly similar to an existing one:

- Prefer merge or version bump over blind insertion
- Preserve source evidence
- Track supersession history

### 7.2 Conflict handling

When two memories disagree:

- Preserve both records
- Add a `contradicts` relation
- Prefer newer, better-validated, better-scoped memories during retrieval
- Surface conflict markers in compressed output when ambiguity matters

### 7.3 Decay and archival

Memories should lose weight when they are:

- Rarely reused
- Repeatedly marked unhelpful
- Tied to changed code areas
- Old and never revalidated

### 7.4 Evidence requirements

Implementation memory should include an evidence trail whenever possible:

- Code references
- Task summaries
- Validation outcomes
- Error symptoms
- Source agent identity

Memories without evidence may still be stored, but with lower default trust.

## 8. Retrieval and Context Compression

The retrieval path should optimize for usefulness per token, not raw recall volume.

### 8.1 Query understanding

Each request is classified into a task intent, such as:

- Feature implementation
- Code understanding
- Debugging
- Preference lookup
- Workflow reuse

### 8.2 Candidate retrieval from DuckDB

Candidates are gathered using a combination of:

- Text matching
- Embedding similarity
- Scope filters
- Project or repo filters
- Module and task-type filters
- Prior successful reuse signals

### 8.3 Graph expansion and re-ranking

Candidates are mapped into graph entities and expanded by a limited number of relevant relationships. Re-ranking should consider:

- Semantic similarity
- Scope match
- Memory type weight
- Confidence
- Validation count
- Freshness
- Evidence strength
- Compatibility with user preferences

### 8.4 Context compression output

The response should return a compact memory pack with four sections.

#### Directives

Short, high-priority constraints and preferences the agent should follow.

#### Relevant Facts

Facts, boundaries, dependencies, and prior implementation details directly useful to the current task.

#### Reusable Patterns

Debugging heuristics, engineering shortcuts, and reusable experience-level guidance.

#### Suggested Workflow

A compact, step-oriented success flow returned when the system detects a reusable workflow pattern for the current task.

### 8.5 Budget-aware output

The orchestrator must support explicit token budgets. When budget is tight:

- Keep directives first
- Keep only the most relevant facts
- Compress patterns aggressively
- Return workflow outlines instead of full details

Full evidence should be expanded only on demand.

## 9. API Surface

The initial API should remain small and explicit.

### 9.1 `ingest_memory`

Writes candidate memory into the system.

Representative fields:

- `memory_type`
- `content`
- `scope`
- `source_agent`
- `project`
- `repo`
- `module`
- `code_refs`
- `evidence`
- `write_mode`
- `idempotency_key`

### 9.2 `search_memory`

Returns a compressed memory pack for a task.

Representative fields:

- `query`
- `intent`
- `scope_filters`
- `token_budget`
- `caller_agent`
- `expand_graph`

### 9.3 `get_memory`

Returns a full memory record, including evidence, version chain, and graph links.

### 9.4 `feedback_memory`

Accepts feedback such as:

- Useful
- Outdated
- Incorrect
- Applies here
- Does not apply here

This feedback affects ranking, trust, and decay.

### 9.5 `review_pending_memories`

Lists pending confirmation items and supports:

- Accept
- Reject
- Edit then accept

### 9.6 `graph_neighbors`

Returns local graph context for advanced callers and maintenance tools.

## 10. Multi-Agent Sharing Model

The service is intended for multiple agents from the beginning. v1 should support these isolation and routing dimensions:

- `tenant`
- `scope`
- `visibility`
- `source_agent`

Recommended semantics:

- `tenant`: reserved top-level isolation key; v1 may default to a single local tenant, but the field should exist to avoid future schema churn
- `scope`: applicability boundary for a memory; recommended values are `global`, `project`, `repo`, and `workspace`
- `visibility`: read availability rule; recommended values are `private`, `shared`, and `system`

Recommended scope hierarchy:

- `global`
- `project`
- `repo`
- `workspace`

To reduce duplicate writes from multiple agents, the system should support idempotency via explicit keys or stable content hashing.

## 11. Error Handling

v1 should explicitly handle the following failure modes.

### 11.1 Low-quality write

Examples:

- Poor summaries
- Weak evidence
- Extraction failure
- Near-duplicate spam

Expected action:

- Reject or downgrade to `provisional`

### 11.2 Conflicting recall

Examples:

- Multiple memories recommend incompatible actions

Expected action:

- Re-rank conservatively
- Mark conflict in the output when unresolved

### 11.3 Scope pollution

Examples:

- Similar but irrelevant memory from another repo

Expected action:

- Favor stronger scope filters
- Penalize weakly scoped cross-project recall

### 11.4 Obsolete memory

Examples:

- Code layout changed
- Preference changed
- Old workflow no longer applies

Expected action:

- Revalidate, decay, or archive

## 12. Evaluation Metrics

The system should be judged by engineering usefulness, not by storage volume.

Recommended v1 metrics:

- `Precision@K`
- `Compressed Context Usefulness`
- `Token Savings`
- `Task Acceleration`
- `Memory Reuse Rate`
- `Bad Recall Rate`
- `Confirmation Burden`
- `Workflow Reuse Rate`

Minimum v1 acceptance targets for planning:

- `Precision@K`: top-5 results should contain useful memory in a majority of benchmark cases
- `Token Savings`: compressed output should materially reduce context size versus replaying raw task history
- `Bad Recall Rate`: clearly wrong or stale recall should stay rare in benchmark tasks
- `Confirmation Burden`: pending review volume should remain low enough for routine human review
- `Workflow Reuse Rate`: recurring benchmark tasks should sometimes return a usable workflow outline, not only isolated facts

## 13. v1 Test Strategy

The first validation set should focus on real engineering scenarios.

### 13.1 Business implementation reuse

Given a new task similar to prior work, verify that the system retrieves implementation patterns, constraints, and relevant code-linked knowledge.

### 13.2 Debugging reuse

Given a recurring failure mode, verify that the system retrieves prior successful debugging paths and corrective actions.

### 13.3 Preference adherence

Given a task that should trigger stored user preferences, verify that the system injects those constraints into the compressed memory pack.

### 13.4 Workflow reuse

Given a recurring engineering task class, verify that the system returns a successful reusable workflow outline instead of only isolated facts.

## 14. Recommended v1 Direction

Recommended implementation direction:

- Use DuckDB as the canonical memory and retrieval store
- Use IndraDB as a selective semantic and relationship layer
- Favor intelligent retrieval and graph-assisted ranking over minimalism
- Keep confirmation gates for preference and workflow memory
- Treat workflow extraction as a first-class objective

This gives the system a clear role: it is not just memory storage, but a shared engineering memory and workflow reuse service for multiple AI agents.
