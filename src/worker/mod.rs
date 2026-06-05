//! Background workers spawned at process startup by `app::AppState`.
//!
//! Each worker holds an `Arc<Store>` and ticks at its own cadence.
//! They sit alongside `service::*` (request-driven HTTP handlers):
//! services run synchronously inside a request future, workers run
//! forever in their own tokio tasks.
//!
//! - `auto_promote_worker` — periodic sweep of long-idle
//!   `PendingConfirmation` rows → `Active`, audited via a
//!   `feedback_events` row with `kind=auto_promoted`. Default ON;
//!   opt out via `MEM_AUTO_PROMOTE_DISABLED=1`.
//! - `dedup_worker` — periodic near-duplicate sweep. Groups active
//!   capsules by `(source_agent, project, repo)`, computes pairwise
//!   cosine on embeddings, archives shorter members of near-dup
//!   clusters via `feedback_kind=incorrect`. Default OFF (destructive);
//!   opt in via `MEM_DEDUP_ENABLED=1`. Mempalace `dedup.py` analogue.
//! - `topic_tunnel_worker` — periodic auto-derivation of cross-project
//!   `user_tunnel:topic:<X>` edges. Groups active capsules by project,
//!   finds projects that share ≥ `min_count` topics, creates one
//!   tunnel edge per shared topic between the two project entities.
//!   Default OFF; opt in via `MEM_TOPIC_TUNNEL_ENABLED=1`. Mempalace
//!   `compute_topic_tunnels` analogue, adapted to mem's edge-first KG.
//! - `cooccurrence_worker` — K10 entity co-occurrence. Per project,
//!   entity pairs co-occurring in ≥ `min_count` active capsules get a
//!   `cooccurs_with` edge between the entity nodes. Default OFF; opt in
//!   via `MEM_COOCCURRENCE_ENABLED=1`. Mempalace within-wing "hallway"
//!   analogue. (Entity↔entity edges; surfaces via kg_query / multi-hop,
//!   not the 1-hop retrieve boost.)
//! - `potentiation_worker` — K9 edge dynamics (strategy B). Drains an
//!   in-memory channel of graph-edge co-access events that retrieve
//!   enqueues, dedups bursts, and applies one Hebbian potentiation per
//!   unique edge via `Store::potentiate_edge` — off the read path.
//!   Default OFF; opt in via `MEM_EDGE_DYNAMICS_ENABLED=1`. Mempalace
//!   `dynamics.py` analogue.
//! - `last_used_worker` — roadmap O1 retrieval reinforcement. Drains an
//!   in-memory channel of capsule-used events that `search` enqueues for
//!   every capsule emitted into a response, coalesces bursts, and stamps
//!   `last_used_at = now` via `Store::bump_last_used_at` — off the read
//!   path. `last_used_at` anchors the decay clock so retrieved capsules
//!   decay slower than untouched ones. Always on.
//! - `vacuum_worker` — daily Lance manifest pruning across every
//!   managed table. Always-on maintenance (reclaims accumulated
//!   copy-on-write history); opt out with `MEM_VACUUM_DISABLED=1`.
//! - `decay_worker` — bulk SQL UPDATE of `memories.decay_score`
//!   (active rows only, capped at 1.0). Goes through
//!   `Store::apply_time_decay` (DuckDB SQL via the lance extension).
//! - `embedding_worker` — drains `embedding_jobs`, calls
//!   `embed_batch`, upserts to `capability_capsule_embeddings`. Lance handles
//!   vector indexing internally — no separate HNSW sidecar to
//!   update. Also hosts roadmap O2 near-dup review flagging
//!   (`flag_if_near_duplicate`): right after embedding a fresh `Active`
//!   capsule it checks the nearest neighbor and, if cosine ≥
//!   `neardup_threshold`, flips it to `PendingConfirmation` + writes a
//!   `suspected_supersede` edge. Opt-in (`MEM_INGEST_NEARDUP_ENABLED`).
//! - `transcript_embedding_worker` — same shape for
//!   `transcript_embedding_jobs` → `conversation_message_embeddings`.
//!
//! There is no `fts_worker` — BM25 index is built once at
//! `LanceStore::open` time on `(memories, content)` and
//! `(conversation_messages, content)` via the lance extension's
//! native FTS. Writes update the inverted index automatically.

pub mod auto_promote_worker;
pub mod cooccurrence_worker;
pub mod decay_worker;
pub mod dedup_worker;
pub mod embedding_worker;
pub mod last_used_worker;
pub mod potentiation_worker;
pub mod topic_tunnel_worker;
pub mod transcript_embedding_worker;
pub mod vacuum_worker;
