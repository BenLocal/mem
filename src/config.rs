use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use thiserror::Error;

use crate::domain::capability_capsule::CapabilityCapsuleType;

static APP_DB_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProviderKind {
    Fake,
    OpenAi,
    EmbedAnything,
}

impl EmbeddingProviderKind {
    /// Does this provider send capsule / transcript content off the
    /// local machine? Used by the startup privacy warning (v3 #33) —
    /// any "yes" provider gets a one-shot `tracing::warn!` at boot
    /// unless `MEM_PRIVACY_WARN_SUPPRESS=1` is set.
    ///
    /// Today only OpenAI qualifies; `Fake` is pure-Rust deterministic
    /// hashing and `EmbedAnything` runs local model inference (no
    /// network calls). New providers default to "yes" via the
    /// catch-all arm so adding a hosted provider can't silently slip
    /// past this warning — the compiler forces the author of a new
    /// variant to pick a side here.
    pub fn sends_off_machine(self) -> bool {
        match self {
            EmbeddingProviderKind::Fake | EmbeddingProviderKind::EmbedAnything => false,
            EmbeddingProviderKind::OpenAi => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddingSettings {
    pub provider: EmbeddingProviderKind,
    pub model: String,
    pub dim: usize,
    pub worker_poll_interval_ms: u64,
    pub max_retries: u32,
    pub batch_size: usize,
    pub openai_api_key: Option<String>,
    /// When `true`, `app.rs` skips spawning the transcript embedding worker.
    /// Set via `MEM_TRANSCRIPT_EMBED_DISABLED` (`"1"` or `"true"`,
    /// case-insensitive). Used by the cli/mine.rs offline pipeline and tests
    /// that want transcript ingest without a background worker.
    pub transcript_disabled: bool,
    /// O2 — write-time near-duplicate review flagging. When `true`, the
    /// embedding worker, right after it embeds a freshly-ingested
    /// `Active` capsule, checks whether a near-identical capsule already
    /// exists (cosine ≥ `neardup_threshold`); if so it flips the new
    /// capsule to `PendingConfirmation` and records a
    /// `suspected_supersede` graph edge for review. Default OFF
    /// (opt in via `MEM_INGEST_NEARDUP_ENABLED=1`) — conservative, like
    /// the other write-affecting workers.
    pub neardup_enabled: bool,
    /// Cosine threshold for O2 near-dup flagging. Default 0.92 — kept
    /// conservative (prefer missing a dup to false-flagging a distinct
    /// capsule). Tune via `MEM_INGEST_NEARDUP_THRESHOLD`.
    pub neardup_threshold: f32,
    // Vestigial usearch sidecar tuning fields (`vector_index_flush_every`,
    // `vector_index_oversample`, `vector_index_use_legacy`,
    // `transcript_vector_index_flush_every`, `transcript_search_oversample`)
    // and their `MEM_VECTOR_INDEX_*` / `MEM_TRANSCRIPT_VECTOR_INDEX_*` env vars
    // were removed in QW-4 (closes backend-coupling §6 Phase 1 QW-4) — Lance
    // 0.27 took over native ANN/FTS indexing and the sidecar code path is gone.
    // `MEM_TRANSCRIPT_OVERSAMPLE` is still live (read directly via
    // `std::env::var` in `TranscriptService::search`) but no longer round-trips
    // through this struct; invalid values fall back to default 4 in the live
    // read site instead of failing at startup.
}

/// Settings for the auto-promote sweep — promotes `PendingConfirmation`
/// capsules to `Active` after they sit idle past `age_days`, audited
/// via a `feedback_events` row with `kind=auto_promoted`. Default ON
/// (worker spawns + auto-prunes long-idle pending rows). Opt OUT with
/// `MEM_AUTO_PROMOTE_DISABLED=1`.
///
/// Why default ON: the MCP `capability_capsule_ingest` hook always
/// writes `write_mode=propose`, so every agent-proposed capsule lands
/// in PendingConfirmation. Without an auto-promote sweep, the queue
/// grows unbounded unless a human runs `review_accept` per row. The
/// guardrails (`age_days`, `decay_threshold`, type allowlist) make
/// the automatic path safe — only long-untouched, low-decay,
/// non-Preference/Workflow capsules get promoted.
#[derive(Debug, Clone)]
pub struct AutoPromoteSettings {
    /// Master switch. Worker is not spawned and HTTP endpoint refuses
    /// `dry_run=false` when this is false. Default `true`.
    pub enabled: bool,
    /// Minimum age (since `updated_at`) before a pending row qualifies.
    /// Using `updated_at` rather than `created_at` keeps in-flight
    /// human edits safe from being promoted out from under the
    /// reviewer.
    pub age_days: u64,
    /// Sweep cadence in seconds. Worker sleeps this long between
    /// passes.
    pub interval_secs: u64,
    /// Allowlist of capsule types eligible for auto-promote. Types
    /// outside this set stay in pending until a human acts. Default
    /// excludes Preference + Workflow because those embody durable
    /// commitments that warrant a human read.
    pub types: Vec<CapabilityCapsuleType>,
    /// Maximum `decay_score` a candidate may have. A capsule already
    /// flagged stale by feedback shouldn't be silently promoted; the
    /// `outdated` / `does_not_apply_here` signals push decay above
    /// this threshold.
    pub decay_threshold: f32,
}

/// Settings for the Lance vacuum sweep — periodically reclaims
/// disk space by pruning old version manifests from every Lance
/// dataset under the storage root. Default ON; opt out with
/// `MEM_VACUUM_DISABLED=1`.
///
/// Why default ON: Lance is copy-on-write, so every UPDATE writes a
/// new manifest and the old ones are never reclaimed automatically.
/// High-churn tables (`transcript_embedding_jobs`,
/// `conversation_message_embeddings`) accumulate gigabytes of
/// historical manifests within days. Vacuum is pure maintenance —
/// no semantic change to query results — so this worker mirrors
/// `decay_worker`'s always-on shape rather than the opt-in shape of
/// `auto_promote_worker`.
#[derive(Debug, Clone)]
pub struct VacuumSettings {
    /// Worker is not spawned when true. Default false (worker is ON).
    pub disabled: bool,
    /// Sweep cadence in seconds. Default 3_600 (hourly). High-churn
    /// tables (`transcript_embedding_jobs`,
    /// `conversation_message_embeddings`) accumulate manifest bloat
    /// in single-digit-GB / day for active users, so hourly tick
    /// keeps the `_versions/` directory bounded. The vacuum call
    /// itself is fast on small DBs and ~seconds on multi-GB Lance
    /// datasets — well within an hour.
    pub interval_secs: u64,
    /// Minimum age of a Lance manifest before it qualifies for
    /// pruning. Default 0 — vacuum every manifest that LanceDB's
    /// pruner deems removable. With the default non-aggressive
    /// [`Self::aggressive`], Lance still keeps the in-flight-window
    /// manifests its commit path needs; only verified-unreferenced
    /// versions are removed.
    pub older_than_days: u64,
    /// When true, vacuum calls `Prune` with `delete_unverified=true`,
    /// bypassing Lance's floor for in-flight transactions. **Default
    /// false** since 2026-06-04 — bypassing the floor deletes manifests
    /// the in-flight commit path still references, and lance 3.0.1's
    /// `conflict_resolver` `.unwrap()`s the resulting `NotFound`,
    /// core-dumping the whole serve. `mem serve` is NOT single-writer
    /// (embedding worker, auto-promote, request handlers, vacuum all
    /// write concurrently), so the floor must stay. Opt back in with
    /// `MEM_VACUUM_AGGRESSIVE=1` only when nothing writes the lance dir
    /// concurrently.
    pub aggressive: bool,
}

/// Near-duplicate sweep settings — closes mempalace-diff-v3 #30.
///
/// The dedup worker periodically scans active capsules grouped by
/// `(source_agent, project, repo)`, computes pairwise cosine on their
/// embeddings, and soft-deletes any pair-cluster member that's shorter
/// than the longest one in the cluster (via `feedback_kind=incorrect`,
/// which moves status to `Archived`). Mempalace's `dedup.py` analogue.
///
/// **Default OFF for v1** (mirrors the original `auto_promote` shape
/// before its default-flip). Dedup is destructive — it archives rows —
/// so the conservative default is opt-in. Once we have telemetry on
/// false-positive rates we can revisit.
#[derive(Debug, Clone)]
pub struct DedupSettings {
    /// Worker is not spawned when false. Default false (opt-in).
    pub enabled: bool,
    /// Sweep cadence in seconds. Default 6 hours — dedup sweeps the
    /// full active capsule set, which is more expensive than vacuum,
    /// and the duplicates it catches accumulate slowly (one extra
    /// row per redundant mining pass).
    pub interval_secs: u64,
    /// Cosine similarity threshold. Default 0.95 — pairs with cosine
    /// at or above this are treated as mirror duplicates. Was 0.92
    /// until the evolution ① merge operator went live; §12.1 of
    /// `docs/evolution-worker.md` (settled with E2) narrows dedup to
    /// mirror-duplicate duty so the two workers never make competing
    /// "archive vs merge" calls on the same pair — near-duplicates in
    /// the 0.88–0.95 band are now the merge operator's territory.
    /// (`mempalace/dedup.py` uses 0.85 as its lowest setting, but mem
    /// capsules are typically shorter / more focused.)
    pub threshold: f32,
    /// Per-sweep cap on candidate capsules pulled. Default 2_000.
    /// Bigger tenants need a higher cap (or per-scope iteration in a
    /// future revision); the cap exists to keep one sweep's worst
    /// case bounded in memory.
    pub scan_limit: usize,
}

/// Topic-tunnel auto-derivation settings — mempalace `compute_topic_tunnels`
/// analogue. The worker scans active capsules in one tenant, groups them
/// by `project`, computes shared-topic overlap between project pairs, and
/// creates `user_tunnel:topic:<topic-name>` graph edges between project
/// entities when the overlap meets `min_count`.
///
/// **Default OFF.** Topic tunnels are non-destructive (only add edges,
/// never close) but they bulk-write to the graph and a wrong min_count
/// can flood the user_tunnel namespace. Opt in via
/// `MEM_TOPIC_TUNNEL_ENABLED=1` once you've decided on the threshold.
///
/// Edges use the `user_tunnel:topic:<name>` relation prefix so they
/// surface via `kg_list_user_tunnels` (v2 #20 phase A) alongside
/// caller-curated tunnels. Operators can filter `relation LIKE
/// 'user_tunnel:topic:%'` to see auto-derived ones specifically.
#[derive(Debug, Clone)]
pub struct TopicTunnelSettings {
    /// Worker is not spawned when false. Default false.
    pub enabled: bool,
    /// Sweep cadence in seconds. Default 6h — topic overlap evolves
    /// slowly (new capsules with new topics land hourly at most), so
    /// hourly would be wasteful.
    pub interval_secs: u64,
    /// Minimum shared-topic count between two projects required to
    /// drop a tunnel. Default 2 to suppress coincidental single-topic
    /// overlaps. mempalace `compute_topic_tunnels` uses the same idea.
    pub min_count: usize,
    /// Per-sweep cap on candidate capsules pulled. Default 2_000.
    pub scan_limit: usize,
}

/// Per-session ingest throttling — closes
/// `agent-memory-strategy-readiness §4.3 #3`.
///
/// Background: transcript mining (`mem mine`) can land hundreds of
/// blocks per session in a single sweep, each enqueuing an ingest +
/// an embedding job. Without a cap, a bursty miner can flood the
/// capsule pool with single-session content, drowning out cross-
/// session signals during retrieve scoring.
///
/// The cap is **process-local** (in-memory HashMap of session_id →
/// count, reset on restart). DB-backed quotas were considered but
/// rejected for v1: the counter is purely advisory ("back off this
/// session"); fresh accounting on restart is the right semantics for
/// "current burst." Persistent quotas would need separate design.
///
/// Default: **None** (no cap). Set `MEM_MAX_INGEST_PER_SESSION=200`
/// or similar to enforce. When unset, ingest is unthrottled
/// (backwards compatible). Sessions with no `session_id` provided
/// in the ingest request are not subject to the cap — counts are
/// keyed on session_id, so missing-id ingests pass through.
#[derive(Debug, Clone, Default)]
pub struct IngestSettings {
    /// Max accepted ingests per session_id. `None` = unlimited.
    pub max_per_session: Option<usize>,
    /// Governance Step 3 — source quality gate. When true, an `experience`
    /// capsule whose content is too short, or is a bare commit subject with
    /// no supporting evidence / code_refs, is rejected at ingest with a
    /// clear reason (instead of accumulating as dead weight). **Default
    /// OFF** — opt in via `MEM_INGEST_QUALITY_GATE_ENABLED=1`. Only
    /// `experience` capsules are gated; every other type passes untouched.
    pub quality_gate_enabled: bool,
    /// Minimum trimmed content length (in chars) for a gated `experience`
    /// capsule. Also the basis for the "bare commit title" ceiling
    /// (`min_content_len * 3`): a single-line, support-free capsule longer
    /// than that is treated as a real one-line lesson, not a title.
    /// Default 40. Tune via `MEM_INGEST_MIN_CONTENT_LEN`.
    pub min_content_len: usize,
}

/// K9 edge-dynamics potentiation (closes mempalace-diff-v4 K9).
/// **Default OFF.** When enabled, retrieve enqueues graph-edge co-access
/// events to an in-memory channel and a worker batch-potentiates them
/// (Hebbian strength growth); retrieve weights the graph boost by each
/// edge's time-decayed strength. Disabled = behaviour unchanged (flat
/// graph boost, no potentiation). Opt in via `MEM_EDGE_DYNAMICS_ENABLED=1`.
#[derive(Debug, Clone)]
pub struct EdgeDynamicsSettings {
    /// Worker not spawned, and retrieve neither enqueues nor weights,
    /// when false. Default false (opt-in).
    pub enabled: bool,
    /// Cadence (seconds) at which the potentiation worker drains the
    /// access-event channel and writes batched potentiations. Default
    /// 60s. Repeated accesses to the same edge within one drain window
    /// collapse to a single potentiation (realising Cepeda anti-massing).
    pub batch_interval_secs: u64,
}

/// K10 entity co-occurrence edges (closes mempalace-diff-v4 K10).
/// **Default OFF.** When enabled, a worker scans each project's active
/// capsules and, for entity pairs that co-occur in >= `min_count`
/// capsules within that project, writes an auto-derived `cooccurs_with`
/// edge between the two entity nodes (mempalace "hallway" analogue).
/// Opt in via `MEM_COOCCURRENCE_ENABLED=1`. NB: the current retrieve
/// graph expansion is 1-hop, so these entity↔entity edges surface via
/// `kg_query` / multi-hop traversal, not the 1-hop recall boost.
#[derive(Debug, Clone)]
pub struct CooccurrenceSettings {
    /// Worker not spawned when false. Default false (opt-in).
    pub enabled: bool,
    /// Sweep cadence in seconds. Default 6h (co-occurrence evolves
    /// slowly, same cadence rationale as topic tunnels).
    pub interval_secs: u64,
    /// Minimum number of capsules (within one project) an entity pair
    /// must co-occur in before an edge is created. Default 2.
    pub min_count: usize,
    /// Per-sweep cap on candidate capsules pulled. Default 2_000.
    pub scan_limit: usize,
}

/// Capsule self-evolution worker (doc `docs/evolution-worker.md` §9).
/// **Default OFF.** When enabled, a worker periodically maps active
/// capsules in embedding space (union-find clustering over existing
/// vectors — zero LLM), aligns clusters across cycles, accumulates
/// per-candidate evidence with a K-consecutive-cycle anti-jitter gate
/// (EvoMap-inspired temporal smoothing), and proposes / executes
/// evolution operators: ① merge (keep-longest + `merged_into` lineage
/// edges) and ② generalize (episodic→semantic proposal capsule into
/// the pending-review queue; sources stay Active). Opt in via
/// `MEM_EVOLUTION_ENABLED=1`. The HTTP dry-run preview
/// (`POST /reviews/evolution {dry_run:true}`) works regardless of the
/// switch — idle-archive precedent: only the destructive path is gated.
#[derive(Debug, Clone)]
pub struct EvolutionSettings {
    /// Master switch. Worker is not spawned, and a real (non-dry-run)
    /// sweep is a no-op, when false. Default `false`.
    pub enabled: bool,
    /// Sweep cadence in seconds — one sweep is one "cycle" for the
    /// K-cycle gate. Default 24h: evolution is deliberately slow.
    pub interval_secs: u64,
    /// Anti-jitter gate: a candidate operation executes only after its
    /// signal held for this many CONSECUTIVE cycles. Default 3.
    pub k_cycles: u32,
    /// β in `E_t = β·E_{t-1} + s_t` — evidence retention across cycles.
    /// In `[0, 1)`. Default 0.7.
    pub evidence_decay: f32,
    /// Hysteresis: a pending candidate is cancelled only when its
    /// decayed evidence drops below this floor — so the cancel
    /// threshold sits below the propose threshold and borderline
    /// signals don't flap. In `(0, 1]`. Default 0.5.
    pub hysteresis: f32,
    /// Map-layer cosine threshold for cluster membership (union-find).
    /// Looser than `merge_threshold` — clusters are "same topic",
    /// merge sub-groups are "near-same content". Default 0.80.
    pub cluster_threshold: f32,
    /// Cosine threshold for the ① merge operator's sub-grouping inside
    /// a cluster. Between the 0.80 map floor and the dedup worker's
    /// 0.95 mirror-duplicate floor (§12.1). Default 0.88.
    pub merge_threshold: f32,
    /// Minimum number of episodic capsules a stable cluster needs
    /// before the ② generalize operator proposes an abstraction.
    /// Default 4 (must be ≥ 2; 1 would "generalize" a singleton).
    pub generalize_min_n: usize,
    /// Per-sweep cap on candidate capsules pulled. Default 2_000.
    pub scan_limit: usize,
    /// ⑥ Hebbian weak-edge retirement (E4): an evolution-owned
    /// `co_recalled_with` edge is closed after this many sweep cycles
    /// without co-recall evidence. Idleness is measured conservatively
    /// against max(edge `valid_from`, either endpoint's `last_used_at`)
    /// — separately-hot capsules keep their edge (see worker note).
    /// Default 3; 0 rejected.
    pub prune_idle_cycles: u32,
    /// Phase-2 synthesis backend selection (doc §6.2). E1 implements
    /// `off` (detect-only placeholders) and `review` (defer content to
    /// the pending-review queue — zero LLM in the worker). `local` /
    /// `api` are designed but unimplemented and rejected at parse.
    pub synthesis: EvolutionSynthesisMode,
}

/// `MEM_EVOLUTION_SYNTHESIS` values implemented in E1. The worker is
/// LLM-free in both modes; `Review` routes generative work to the
/// pending-review queue where the interactive agent writes content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolutionSynthesisMode {
    Off,
    Review,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub db_path: PathBuf,
    pub embedding: EmbeddingSettings,
    pub auto_promote: AutoPromoteSettings,
    pub vacuum: VacuumSettings,
    pub dedup: DedupSettings,
    pub idle_archive: IdleArchiveSettings,
    pub topic_tunnel: TopicTunnelSettings,
    pub ingest: IngestSettings,
    pub edge_dynamics: EdgeDynamicsSettings,
    pub cooccurrence: CooccurrenceSettings,
    pub evolution: EvolutionSettings,
    /// Which storage backend `mem serve` runs on. Default `Lance`
    /// (on-disk Lance datasets, read lance-native). `Postgres` requires the
    /// `postgres` cargo feature and `postgres_url`.
    pub backend: BackendKind,
    /// Connection string for the Postgres backend (`MEM_POSTGRES_URL`).
    /// `None` for the Lance backend.
    pub postgres_url: Option<String>,
    /// Connection string for the ClickHouse backend (`MEM_CLICKHOUSE_URL`,
    /// e.g. `http://localhost:8123`). `None` unless `backend = Clickhouse`.
    pub clickhouse_url: Option<String>,
}

/// Storage backend selector (`MEM_BACKEND`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    /// On-disk Lance datasets, read lance-native (the default; always built).
    #[default]
    Lance,
    /// Postgres (+ pgvector). Requires the `postgres` cargo feature.
    Postgres,
    /// ClickHouse. Requires the `clickhouse` cargo feature. UNVALIDATED
    /// scaffold (clickhouse-backend P1).
    Clickhouse,
}

/// Parse `MEM_BACKEND` (`lance` default | `postgres` | `clickhouse`) plus the
/// selected backend's connection URL (`MEM_POSTGRES_URL` / `MEM_CLICKHOUSE_URL`).
/// Returns `(kind, postgres_url, clickhouse_url)`. Selecting a non-Lance
/// backend without its URL is a loud error, not a silent fallback to Lance —
/// a backend you can't reach should fail at startup.
pub fn parse_backend(
    get: impl Fn(&str) -> Option<String>,
) -> Result<(BackendKind, Option<String>, Option<String>), ConfigError> {
    let kind = match get("MEM_BACKEND").map(|s| s.trim().to_ascii_lowercase()) {
        None => BackendKind::Lance,
        Some(s) if s == "lance" || s.is_empty() => BackendKind::Lance,
        Some(s) if s == "postgres" || s == "postgresql" || s == "pg" => BackendKind::Postgres,
        Some(s) if s == "clickhouse" || s == "ch" => BackendKind::Clickhouse,
        Some(other) => {
            return Err(ConfigError::InvalidBackend(format!(
                "{other} (expected lance, postgres, or clickhouse)"
            )))
        }
    };
    let postgres_url = get("MEM_POSTGRES_URL").filter(|s| !s.trim().is_empty());
    let clickhouse_url = get("MEM_CLICKHOUSE_URL").filter(|s| !s.trim().is_empty());
    if kind == BackendKind::Postgres && postgres_url.is_none() {
        return Err(ConfigError::InvalidBackend(
            "MEM_BACKEND=postgres requires MEM_POSTGRES_URL".to_string(),
        ));
    }
    if kind == BackendKind::Clickhouse && clickhouse_url.is_none() {
        return Err(ConfigError::InvalidBackend(
            "MEM_BACKEND=clickhouse requires MEM_CLICKHOUSE_URL".to_string(),
        ));
    }
    Ok((kind, postgres_url, clickhouse_url))
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid EMBEDDING_PROVIDER: {0} (expected fake, openai, or embedanything)")]
    InvalidEmbeddingProvider(String),
    #[error("invalid MEM_BACKEND: {0}")]
    InvalidBackend(String),
    #[error("OPENAI_API_KEY is required when EMBEDDING_PROVIDER=openai (or real alias)")]
    MissingOpenAiApiKey,
    #[error("invalid EMBEDDING_DIM: {0}")]
    InvalidEmbeddingDim(String),
    #[error("invalid EMBEDDING_WORKER_POLL_INTERVAL_MS: {0}")]
    InvalidPollInterval(String),
    #[error("invalid EMBEDDING_MAX_RETRIES: {0}")]
    InvalidMaxRetries(String),
    #[error("invalid EMBEDDING_BATCH_SIZE: {0}")]
    InvalidBatchSize(String),
    #[error("invalid MEM_AUTO_PROMOTE_AGE_DAYS: {0}")]
    InvalidAutoPromoteAgeDays(String),
    #[error("invalid MEM_AUTO_PROMOTE_INTERVAL_SECS: {0}")]
    InvalidAutoPromoteIntervalSecs(String),
    #[error("invalid MEM_AUTO_PROMOTE_DECAY_THRESHOLD: {0}")]
    InvalidAutoPromoteDecayThreshold(String),
    #[error("invalid MEM_AUTO_PROMOTE_TYPES entry: {0} (expected one of experience, implementation, episode, diary, preference, workflow)")]
    InvalidAutoPromoteType(String),
    #[error("invalid MEM_VACUUM_INTERVAL_SECS: {0}")]
    InvalidVacuumIntervalSecs(String),
    #[error("invalid MEM_VACUUM_OLDER_THAN_DAYS: {0}")]
    InvalidVacuumOlderThanDays(String),
    #[error("invalid MEM_DEDUP_INTERVAL_SECS: {0}")]
    InvalidDedupIntervalSecs(String),
    #[error("invalid MEM_DEDUP_THRESHOLD: {0} (expected float in (0, 1])")]
    InvalidDedupThreshold(String),
    #[error("invalid MEM_DEDUP_SCAN_LIMIT: {0}")]
    InvalidDedupScanLimit(String),
    #[error("invalid {var}: {value}")]
    InvalidEvolutionSetting { var: &'static str, value: String },
    #[error("invalid MEM_IDLE_ARCHIVE_INTERVAL_SECS: {0}")]
    InvalidIdleArchiveIntervalSecs(String),
    #[error("invalid MEM_IDLE_ARCHIVE_AGE_DAYS: {0}")]
    InvalidIdleArchiveAgeDays(String),
    #[error("invalid MEM_IDLE_ARCHIVE_DECAY_THRESHOLD: {0} (expected float in [0, 1])")]
    InvalidIdleArchiveDecayThreshold(String),
    #[error("invalid MEM_IDLE_ARCHIVE_CONFIDENCE: {0} (expected float in [0, 1])")]
    InvalidIdleArchiveConfidence(String),
    #[error("invalid MEM_IDLE_ARCHIVE_MIN_CONTENT_LEN: {0}")]
    InvalidIdleArchiveMinContentLen(String),
    #[error("invalid MEM_IDLE_ARCHIVE_SCAN_LIMIT: {0}")]
    InvalidIdleArchiveScanLimit(String),
    #[error("invalid MEM_TOPIC_TUNNEL_INTERVAL_SECS: {0}")]
    InvalidTopicTunnelIntervalSecs(String),
    #[error("invalid MEM_EDGE_DYNAMICS_BATCH_SECS: {0} (expected positive integer)")]
    InvalidEdgeDynamicsBatchSecs(String),
    #[error("invalid MEM_COOCCURRENCE_* setting: {0} (expected positive integer)")]
    InvalidCooccurrenceSetting(String),
    #[error("invalid MEM_TOPIC_TUNNEL_MIN_COUNT: {0}")]
    InvalidTopicTunnelMinCount(String),
    #[error("invalid MEM_TOPIC_TUNNEL_SCAN_LIMIT: {0}")]
    InvalidTopicTunnelScanLimit(String),
    #[error("invalid MEM_MAX_INGEST_PER_SESSION: {0}")]
    InvalidMaxIngestPerSession(String),
    #[error("invalid MEM_INGEST_MIN_CONTENT_LEN: {0}")]
    InvalidMinContentLen(String),
}

impl EmbeddingSettings {
    pub fn development_defaults() -> Self {
        Self {
            provider: EmbeddingProviderKind::EmbedAnything,
            model: "Qwen/Qwen3-Embedding-0.6B".to_string(),
            dim: 1024,
            // **Default 10_000 ms** (was 1_000 pre-2026-05-21). Measured
            // idle baseline of mem with workers polling at 1 Hz was
            // ~510% CPU + 800+ tokio blocking threads (spawn_blocking
            // accumulates each tick, EmbedAnything model + Rayon pool
            // pile on; under the old DuckDB read engine, futex contention
            // on its single connection mutex dominated). Dropping to 10 s
            // tick cuts the spawn-blocking cost ~9× — measured 510% → ~56% CPU and
            // 800 → 217 threads on the same workload. The trade-off is
            // worst-case 10 s latency between job enqueue and pick-up,
            // which is fine for the embedding queue (background work).
            // Set EMBEDDING_WORKER_POLL_INTERVAL_MS=1000 to restore the
            // legacy aggressive cadence if a latency-sensitive caller
            // needs sub-second job pickup.
            worker_poll_interval_ms: 10_000,
            // Failure attempts allowed before permanent `failed` (initial pending try + retries).
            max_retries: 4,
            // **Default 8** (was 1 pre-2026-05-21). Going from 1 → 8
            // amortizes EmbedAnything's CPU inference cost (the local Qwen3
            // model dominates per-batch latency once the batch is non-trivial)
            // and turns one HTTP call into N for OpenAI. Set
            // `EMBEDDING_BATCH_SIZE=1` to restore the per-job behavior if
            // a downstream cares about per-job ordering / failure isolation.
            batch_size: 8,
            openai_api_key: None,
            transcript_disabled: false,
            // O2 near-dup review flagging — opt-in, conservative threshold.
            neardup_enabled: false,
            neardup_threshold: 0.92,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();

        if let Some(value) = get("EMBEDDING_PROVIDER") {
            s.provider = match value.to_ascii_lowercase().as_str() {
                "fake" => EmbeddingProviderKind::Fake,
                "real" | "openai" => EmbeddingProviderKind::OpenAi,
                "embedanything" | "embed_anything" => EmbeddingProviderKind::EmbedAnything,
                other => return Err(ConfigError::InvalidEmbeddingProvider(other.to_string())),
            };
        }

        if let Some(model) = get("EMBEDDING_MODEL") {
            if !model.is_empty() {
                s.model = model;
            }
        }

        if let Some(raw) = get("EMBEDDING_DIM") {
            let dim: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidEmbeddingDim(raw.clone()))?;
            if dim == 0 {
                return Err(ConfigError::InvalidEmbeddingDim(raw));
            }
            s.dim = dim;
        }

        if let Some(raw) = get("EMBEDDING_WORKER_POLL_INTERVAL_MS") {
            let ms: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidPollInterval(raw.clone()))?;
            if ms == 0 {
                return Err(ConfigError::InvalidPollInterval(raw));
            }
            s.worker_poll_interval_ms = ms;
        }

        if let Some(raw) = get("EMBEDDING_MAX_RETRIES") {
            let n: u32 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidMaxRetries(raw.clone()))?;
            s.max_retries = n;
        }

        if let Some(raw) = get("EMBEDDING_BATCH_SIZE") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidBatchSize(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidBatchSize(raw));
            }
            s.batch_size = n;
        }

        if let Some(key) = get("OPENAI_API_KEY") {
            if !key.is_empty() {
                s.openai_api_key = Some(key);
            }
        }

        if s.provider == EmbeddingProviderKind::OpenAi
            && s.openai_api_key.as_deref().unwrap_or("").is_empty()
        {
            return Err(ConfigError::MissingOpenAiApiKey);
        }

        if let Some(raw) = get("MEM_TRANSCRIPT_EMBED_DISABLED") {
            s.transcript_disabled =
                matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }

        if let Some(raw) = get("MEM_INGEST_NEARDUP_ENABLED") {
            s.neardup_enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }

        if let Some(raw) = get("MEM_INGEST_NEARDUP_THRESHOLD") {
            // Invalid / out-of-range values silently fall back to the
            // default (0.92) — same lenient pattern as
            // MEM_TRANSCRIPT_OVERSAMPLE; near-dup flagging is best-effort.
            if let Ok(t) = raw.parse::<f32>() {
                if (0.0..=1.0).contains(&t) {
                    s.neardup_threshold = t;
                }
            }
        }

        Ok(s)
    }

    /// Stored on `embedding_jobs.provider` to dedupe work; matches configured backend.
    pub fn job_provider_id(&self) -> &'static str {
        match self.provider {
            EmbeddingProviderKind::Fake => "fake",
            EmbeddingProviderKind::OpenAi => "openai",
            EmbeddingProviderKind::EmbedAnything => "embedanything",
        }
    }
}

impl AutoPromoteSettings {
    /// Development / test defaults: feature ON, 3-day idle threshold,
    /// hourly cadence, default type allowlist (Experience /
    /// Implementation / Episode / Diary — Preference + Workflow
    /// excluded because they're durable commitments that warrant a
    /// human read), decay threshold 0.5. Opt OUT via
    /// `MEM_AUTO_PROMOTE_DISABLED=1`.
    ///
    /// `age_days` was 7 until 2026-06-04; lowered to 3 so the
    /// PendingConfirmation backlog (which the always-on `propose` path
    /// floods) drains in days, not a week. The decay-threshold + type
    /// allowlist guardrails still gate what promotes.
    pub fn development_defaults() -> Self {
        Self {
            enabled: true,
            age_days: 3,
            interval_secs: 3600,
            types: vec![
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleType::Implementation,
                CapabilityCapsuleType::Episode,
                CapabilityCapsuleType::Diary,
            ],
            decay_threshold: 0.5,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();

        // Canonical opt-out (mirrors `MEM_VACUUM_DISABLED`).
        if let Some(raw) = get("MEM_AUTO_PROMOTE_DISABLED") {
            if matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
                s.enabled = false;
            }
        }

        // Back-compat: the legacy `MEM_AUTO_PROMOTE_ENABLED` env var
        // (which was the opt-IN switch when this feature was default
        // OFF) still works. Truthy values are now redundant against
        // the default-on; falsy values (`0` / `false` / `no` / empty)
        // act as an opt-out alongside the canonical `_DISABLED` var.
        // This way users who had it set to either value before the
        // flip don't get surprised by the new default.
        if let Some(raw) = get("MEM_AUTO_PROMOTE_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }

        if let Some(raw) = get("MEM_AUTO_PROMOTE_AGE_DAYS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidAutoPromoteAgeDays(raw.clone()))?;
            // `0` would promote anything modified in the same tick the
            // worker fires — surprising and useless. Reject loudly.
            if n == 0 {
                return Err(ConfigError::InvalidAutoPromoteAgeDays(raw));
            }
            s.age_days = n;
        }

        if let Some(raw) = get("MEM_AUTO_PROMOTE_INTERVAL_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidAutoPromoteIntervalSecs(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidAutoPromoteIntervalSecs(raw));
            }
            s.interval_secs = n;
        }

        if let Some(raw) = get("MEM_AUTO_PROMOTE_DECAY_THRESHOLD") {
            let v: f32 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidAutoPromoteDecayThreshold(raw.clone()))?;
            if !(0.0..=1.0).contains(&v) {
                return Err(ConfigError::InvalidAutoPromoteDecayThreshold(raw));
            }
            s.decay_threshold = v;
        }

        if let Some(raw) = get("MEM_AUTO_PROMOTE_TYPES") {
            let mut out = Vec::new();
            for tok in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                let kind = match tok.to_ascii_lowercase().as_str() {
                    "experience" => CapabilityCapsuleType::Experience,
                    "implementation" => CapabilityCapsuleType::Implementation,
                    "episode" => CapabilityCapsuleType::Episode,
                    "diary" => CapabilityCapsuleType::Diary,
                    "preference" => CapabilityCapsuleType::Preference,
                    "workflow" => CapabilityCapsuleType::Workflow,
                    other => return Err(ConfigError::InvalidAutoPromoteType(other.to_string())),
                };
                out.push(kind);
            }
            // Empty list (e.g. `MEM_AUTO_PROMOTE_TYPES=""`) effectively
            // disables promotion without touching the master switch.
            // Honour that — don't silently fall back to defaults.
            s.types = out;
        }

        Ok(s)
    }
}

impl VacuumSettings {
    /// Development / test defaults: worker ON, **hourly** cadence,
    /// 0-day cutoff, prune **non-aggressively** (keep Lance's in-flight
    /// safety floor).
    ///
    /// `aggressive` was `true` until 2026-06-04, on a "single-writer
    /// local-first" assumption. That assumption was WRONG: one `mem serve`
    /// runs many CONCURRENT writer tasks (embedding worker, transcript
    /// embedding worker, auto-promote sweep, request handlers, the vacuum
    /// worker itself). Aggressive prune (`delete_unverified=true`) deletes
    /// manifests the in-flight commit path still needs, and lance 3.0.1's
    /// `conflict_resolver` does `.unwrap()` on the resulting `NotFound` →
    /// the whole serve panics + core-dumps. Observed crash:
    /// `DatasetNotFound .../capability_capsules.lance/_versions/1768.manifest`.
    /// Non-aggressive prune still reclaims the bulk of old manifests; it
    /// just keeps the recent (in-flight-window) ones. Opt back into the
    /// risky behavior with `MEM_VACUUM_AGGRESSIVE=1`.
    pub fn development_defaults() -> Self {
        Self {
            disabled: false,
            interval_secs: 3_600,
            older_than_days: 0,
            aggressive: false,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();

        if let Some(raw) = get("MEM_VACUUM_DISABLED") {
            s.disabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }

        if let Some(raw) = get("MEM_VACUUM_INTERVAL_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidVacuumIntervalSecs(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidVacuumIntervalSecs(raw));
            }
            s.interval_secs = n;
        }

        if let Some(raw) = get("MEM_VACUUM_OLDER_THAN_DAYS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidVacuumOlderThanDays(raw.clone()))?;
            // `0` is the new default — every manifest LanceDB's
            // pruner can remove gets removed. Negative values would
            // be nonsensical (`u64` rules those out already).
            s.older_than_days = n;
        }

        // Default is non-aggressive (keep Lance's in-flight floor) — the
        // safe behavior after the conflict_resolver crash. `MEM_VACUUM_AGGRESSIVE=1`
        // opts BACK IN to `delete_unverified=true` (bypass the floor). Only
        // safe when truly nothing writes the lance dir concurrently — which
        // a normal `mem serve` is NOT (it has several concurrent writer
        // tasks). The legacy `MEM_VACUUM_PRESERVE_UNVERIFIED=1` is still
        // honored (now redundant: it forces the new default) and wins over
        // `MEM_VACUUM_AGGRESSIVE` so an explicit "stay safe" always holds.
        if let Some(raw) = get("MEM_VACUUM_AGGRESSIVE") {
            if matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
                s.aggressive = true;
            }
        }
        if let Some(raw) = get("MEM_VACUUM_PRESERVE_UNVERIFIED") {
            if matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes") {
                s.aggressive = false;
            }
        }

        Ok(s)
    }
}

impl DedupSettings {
    /// Default: worker OFF, 6-hour cadence, threshold 0.95, 2_000 cap.
    /// Opt in via `MEM_DEDUP_ENABLED=1`.
    pub fn development_defaults() -> Self {
        Self {
            enabled: false,
            interval_secs: 6 * 3_600,
            threshold: 0.95,
            scan_limit: 2_000,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_DEDUP_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_DEDUP_INTERVAL_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidDedupIntervalSecs(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidDedupIntervalSecs(raw));
            }
            s.interval_secs = n;
        }
        if let Some(raw) = get("MEM_DEDUP_THRESHOLD") {
            let n: f32 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidDedupThreshold(raw.clone()))?;
            if !(0.0 < n && n <= 1.0) {
                return Err(ConfigError::InvalidDedupThreshold(raw));
            }
            s.threshold = n;
        }
        if let Some(raw) = get("MEM_DEDUP_SCAN_LIMIT") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidDedupScanLimit(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidDedupScanLimit(raw));
            }
            s.scan_limit = n;
        }
        Ok(s)
    }
}

/// Settings for the idle-archive sweep (governance Step 2) — periodically
/// archives `Active` capsules that are demonstrably dead weight: never
/// recalled since creation, aged out, never positively reinforced, and
/// decayed past a floor. Archival reuses the dedup `apply_feedback(Incorrect)`
/// path, so rows are kept verbatim — only search drops them.
///
/// **Default OFF.** Like dedup, this worker archives rows, so it is opt-in
/// (`MEM_IDLE_ARCHIVE_ENABLED=1`). The HTTP dry-run preview
/// (`POST /reviews/idle_archive {dry_run:true}`) works regardless of the
/// switch; only the destructive path is gated on it.
#[derive(Debug, Clone)]
pub struct IdleArchiveSettings {
    /// Master switch. Worker is not spawned, and a real (non-dry-run)
    /// sweep is a no-op, when false. Default `false`.
    pub enabled: bool,
    /// Sweep cadence in seconds. Default 24 hours — idle capsules age
    /// slowly, so there is no value in a tight loop.
    pub interval_secs: u64,
    /// Minimum age (since `created_at`, in days) before a capsule can be
    /// archived. `created_at` (not `updated_at`) so a recent metadata
    /// touch doesn't reset the idle clock. Default 14.
    pub age_days: u64,
    /// Minimum `decay_score` a candidate must have reached. Pairs with
    /// `age_days`: a capsule must be both old AND decayed. Default 0.15 —
    /// chosen against the decay worker's 1%/day rate so the gate is
    /// reachable within ~2 weeks rather than the ~50 days a 0.5 floor would
    /// need (0.5 is effectively dormant on a young pool).
    pub decay_threshold: f32,
    /// The ingest-default confidence. A capsule is "never positively
    /// reinforced" when its confidence is still at (or below) this value
    /// — feedback only ever raises confidence, so equality means no
    /// `useful` / `applies_here` ever landed. Default 0.6 (must match the
    /// ingest default in `pipeline::ingest::initial_status`'s sibling).
    pub default_confidence: f32,
    /// Structural-junk sub-filter floor, reused from the Step-3 ingest gate
    /// (`pipeline::ingest::low_value_experience_reason`). A candidate must
    /// be not just idle but also *structurally low-value* (too short, or a
    /// bare single-line commit subject with no evidence / code_refs) — so a
    /// long, structured, reference-carrying lesson is **never** archived no
    /// matter how idle it is. This is what stops the sweep from deleting
    /// substantive memories whose recall/feedback signals are simply blank
    /// (e.g. an existing pool predating the `last_recalled_at` column).
    /// Default 40 (matches the ingest gate's `min_content_len`).
    pub min_content_len: usize,
    /// Per-sweep cap on candidate capsules pulled. Default 2_000.
    pub scan_limit: usize,
}

impl IdleArchiveSettings {
    /// Default: worker OFF, 24-hour cadence, 14-day age floor, decay ≥ 0.15,
    /// default-confidence 0.6, structural floor 40 chars, 2_000 cap. Opt in
    /// via `MEM_IDLE_ARCHIVE_ENABLED=1`.
    pub fn development_defaults() -> Self {
        Self {
            enabled: false,
            interval_secs: 24 * 3_600,
            age_days: 14,
            decay_threshold: 0.15,
            default_confidence: 0.6,
            min_content_len: 40,
            scan_limit: 2_000,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_INTERVAL_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidIdleArchiveIntervalSecs(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidIdleArchiveIntervalSecs(raw));
            }
            s.interval_secs = n;
        }
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_AGE_DAYS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidIdleArchiveAgeDays(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidIdleArchiveAgeDays(raw));
            }
            s.age_days = n;
        }
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_DECAY_THRESHOLD") {
            let n: f32 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidIdleArchiveDecayThreshold(raw.clone()))?;
            if !(0.0..=1.0).contains(&n) {
                return Err(ConfigError::InvalidIdleArchiveDecayThreshold(raw));
            }
            s.decay_threshold = n;
        }
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_CONFIDENCE") {
            let n: f32 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidIdleArchiveConfidence(raw.clone()))?;
            if !(0.0..=1.0).contains(&n) {
                return Err(ConfigError::InvalidIdleArchiveConfidence(raw));
            }
            s.default_confidence = n;
        }
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_MIN_CONTENT_LEN") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidIdleArchiveMinContentLen(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidIdleArchiveMinContentLen(raw));
            }
            s.min_content_len = n;
        }
        if let Some(raw) = get("MEM_IDLE_ARCHIVE_SCAN_LIMIT") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidIdleArchiveScanLimit(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidIdleArchiveScanLimit(raw));
            }
            s.scan_limit = n;
        }
        Ok(s)
    }
}

impl TopicTunnelSettings {
    pub fn development_defaults() -> Self {
        Self {
            enabled: false,
            interval_secs: 6 * 3_600,
            min_count: 2,
            scan_limit: 2_000,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_TOPIC_TUNNEL_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_TOPIC_TUNNEL_INTERVAL_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidTopicTunnelIntervalSecs(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidTopicTunnelIntervalSecs(raw));
            }
            s.interval_secs = n;
        }
        if let Some(raw) = get("MEM_TOPIC_TUNNEL_MIN_COUNT") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidTopicTunnelMinCount(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidTopicTunnelMinCount(raw));
            }
            s.min_count = n;
        }
        if let Some(raw) = get("MEM_TOPIC_TUNNEL_SCAN_LIMIT") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidTopicTunnelScanLimit(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidTopicTunnelScanLimit(raw));
            }
            s.scan_limit = n;
        }
        Ok(s)
    }
}

impl EdgeDynamicsSettings {
    pub fn development_defaults() -> Self {
        Self {
            enabled: false,
            batch_interval_secs: 60,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_EDGE_DYNAMICS_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_EDGE_DYNAMICS_BATCH_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidEdgeDynamicsBatchSecs(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidEdgeDynamicsBatchSecs(raw));
            }
            s.batch_interval_secs = n;
        }
        Ok(s)
    }
}

impl CooccurrenceSettings {
    pub fn development_defaults() -> Self {
        Self {
            enabled: false,
            interval_secs: 6 * 3_600,
            min_count: 2,
            scan_limit: 2_000,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_COOCCURRENCE_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_COOCCURRENCE_INTERVAL_SECS") {
            let n: u64 = raw
                .parse()
                .map_err(|_| ConfigError::InvalidCooccurrenceSetting(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidCooccurrenceSetting(raw));
            }
            s.interval_secs = n;
        }
        if let Some(raw) = get("MEM_COOCCURRENCE_MIN_COUNT") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidCooccurrenceSetting(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidCooccurrenceSetting(raw));
            }
            s.min_count = n;
        }
        if let Some(raw) = get("MEM_COOCCURRENCE_SCAN_LIMIT") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidCooccurrenceSetting(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidCooccurrenceSetting(raw));
            }
            s.scan_limit = n;
        }
        Ok(s)
    }
}

impl EvolutionSettings {
    /// Default: worker OFF, daily cadence, K=3 gate, β=0.7, hysteresis
    /// 0.5, cluster 0.80 / merge 0.88, generalize_min_n 4, 2_000 cap,
    /// synthesis off. Opt in via `MEM_EVOLUTION_ENABLED=1`.
    pub fn development_defaults() -> Self {
        Self {
            enabled: false,
            interval_secs: 86_400,
            k_cycles: 3,
            evidence_decay: 0.7,
            hysteresis: 0.5,
            cluster_threshold: 0.80,
            merge_threshold: 0.88,
            generalize_min_n: 4,
            scan_limit: 2_000,
            prune_idle_cycles: 3,
            synthesis: EvolutionSynthesisMode::Off,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        fn invalid(var: &'static str, value: &str) -> ConfigError {
            ConfigError::InvalidEvolutionSetting {
                var,
                value: value.to_string(),
            }
        }
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_EVOLUTION_ENABLED") {
            s.enabled = matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_EVOLUTION_INTERVAL_SECS") {
            const VAR: &str = "MEM_EVOLUTION_INTERVAL_SECS";
            let n: u64 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if n == 0 {
                return Err(invalid(VAR, &raw));
            }
            s.interval_secs = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_K_CYCLES") {
            const VAR: &str = "MEM_EVOLUTION_K_CYCLES";
            let n: u32 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if n == 0 {
                return Err(invalid(VAR, &raw));
            }
            s.k_cycles = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_EVIDENCE_DECAY") {
            const VAR: &str = "MEM_EVOLUTION_EVIDENCE_DECAY";
            let n: f32 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if !(0.0..1.0).contains(&n) {
                return Err(invalid(VAR, &raw));
            }
            s.evidence_decay = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_HYSTERESIS") {
            const VAR: &str = "MEM_EVOLUTION_HYSTERESIS";
            let n: f32 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if !(0.0 < n && n <= 1.0) {
                return Err(invalid(VAR, &raw));
            }
            s.hysteresis = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_CLUSTER_THRESHOLD") {
            const VAR: &str = "MEM_EVOLUTION_CLUSTER_THRESHOLD";
            let n: f32 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if !(0.0 < n && n <= 1.0) {
                return Err(invalid(VAR, &raw));
            }
            s.cluster_threshold = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_MERGE_THRESHOLD") {
            const VAR: &str = "MEM_EVOLUTION_MERGE_THRESHOLD";
            let n: f32 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if !(0.0 < n && n <= 1.0) {
                return Err(invalid(VAR, &raw));
            }
            s.merge_threshold = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_GENERALIZE_MIN_N") {
            const VAR: &str = "MEM_EVOLUTION_GENERALIZE_MIN_N";
            let n: usize = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if n < 2 {
                return Err(invalid(VAR, &raw));
            }
            s.generalize_min_n = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_SCAN_LIMIT") {
            const VAR: &str = "MEM_EVOLUTION_SCAN_LIMIT";
            let n: usize = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if n == 0 {
                return Err(invalid(VAR, &raw));
            }
            s.scan_limit = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_PRUNE_IDLE_CYCLES") {
            const VAR: &str = "MEM_EVOLUTION_PRUNE_IDLE_CYCLES";
            let n: u32 = raw.parse().map_err(|_| invalid(VAR, &raw))?;
            if n == 0 {
                return Err(invalid(VAR, &raw));
            }
            s.prune_idle_cycles = n;
        }
        if let Some(raw) = get("MEM_EVOLUTION_SYNTHESIS") {
            const VAR: &str = "MEM_EVOLUTION_SYNTHESIS";
            s.synthesis = match raw.to_ascii_lowercase().as_str() {
                "off" => EvolutionSynthesisMode::Off,
                "review" => EvolutionSynthesisMode::Review,
                // `local` / `api` are designed (doc §6.2) but not
                // implemented — fail loudly instead of silently off.
                _ => return Err(invalid(VAR, &raw)),
            };
        }
        Ok(s)
    }
}

impl IngestSettings {
    pub fn development_defaults() -> Self {
        Self {
            max_per_session: None,
            quality_gate_enabled: false,
            min_content_len: 40,
        }
    }

    pub fn from_env_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut s = Self::development_defaults();
        if let Some(raw) = get("MEM_MAX_INGEST_PER_SESSION") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidMaxIngestPerSession(raw.clone()))?;
            // `0` is treated as "no cap" (same as unset) — saves
            // callers from a footgun where a typo'd / empty templated
            // value reads as `0` and blocks all ingest. If you really
            // want zero ingests, kill the service.
            s.max_per_session = if n == 0 { None } else { Some(n) };
        }
        if let Some(raw) = get("MEM_INGEST_QUALITY_GATE_ENABLED") {
            s.quality_gate_enabled =
                matches!(raw.to_ascii_lowercase().as_str(), "1" | "true" | "yes");
        }
        if let Some(raw) = get("MEM_INGEST_MIN_CONTENT_LEN") {
            let n: usize = raw
                .parse()
                .map_err(|_| ConfigError::InvalidMinContentLen(raw.clone()))?;
            if n == 0 {
                return Err(ConfigError::InvalidMinContentLen(raw));
            }
            s.min_content_len = n;
        }
        Ok(s)
    }
}

impl Config {
    pub fn local() -> Self {
        Self {
            bind_addr: "127.0.0.1:3000".to_string(),
            db_path: default_db_path(),
            embedding: EmbeddingSettings::development_defaults(),
            auto_promote: AutoPromoteSettings::development_defaults(),
            vacuum: VacuumSettings::development_defaults(),
            dedup: DedupSettings::development_defaults(),
            idle_archive: IdleArchiveSettings::development_defaults(),
            topic_tunnel: TopicTunnelSettings::development_defaults(),
            ingest: IngestSettings::development_defaults(),
            edge_dynamics: EdgeDynamicsSettings::development_defaults(),
            cooccurrence: CooccurrenceSettings::development_defaults(),
            evolution: EvolutionSettings::development_defaults(),
            backend: BackendKind::Lance,
            postgres_url: None,
            clickhouse_url: None,
        }
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
        let (backend, postgres_url, clickhouse_url) = parse_backend(|k| std::env::var(k).ok())?;
        Ok(Self {
            bind_addr,
            db_path: default_db_path(),
            embedding: EmbeddingSettings::from_env_vars(|k| std::env::var(k).ok())?,
            auto_promote: AutoPromoteSettings::from_env_vars(|k| std::env::var(k).ok())?,
            vacuum: VacuumSettings::from_env_vars(|k| std::env::var(k).ok())?,
            dedup: DedupSettings::from_env_vars(|k| std::env::var(k).ok())?,
            idle_archive: IdleArchiveSettings::from_env_vars(|k| std::env::var(k).ok())?,
            topic_tunnel: TopicTunnelSettings::from_env_vars(|k| std::env::var(k).ok())?,
            ingest: IngestSettings::from_env_vars(|k| std::env::var(k).ok())?,
            edge_dynamics: EdgeDynamicsSettings::from_env_vars(|k| std::env::var(k).ok())?,
            cooccurrence: CooccurrenceSettings::from_env_vars(|k| std::env::var(k).ok())?,
            evolution: EvolutionSettings::from_env_vars(|k| std::env::var(k).ok())?,
            backend,
            postgres_url,
            clickhouse_url,
        })
    }
}

fn default_db_path() -> PathBuf {
    if let Ok(path) = std::env::var("MEM_DB_PATH") {
        return PathBuf::from(path);
    }

    if let Some(home) = std::env::var_os("HOME") {
        let mem_dir = PathBuf::from(home).join(".mem");
        if std::fs::create_dir_all(&mem_dir).is_ok() {
            // Default dataset dir. Prefer the current `mem.lance` name, but
            // fall back to the legacy `mem.duckdb` name if an older install
            // still has data there (both hold Lance datasets — the DuckDB
            // read engine was removed in route-B). Keeps default-path
            // installs non-breaking across the rename.
            let current = mem_dir.join("mem.lance");
            let legacy = mem_dir.join("mem.duckdb");
            if !current.exists() && legacy.exists() {
                return legacy;
            }
            return current;
        }
    }

    let sequence = APP_DB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    // `.lance` suffix: this is a lance dataset directory, not a DuckDB file
    // (the DuckDB read engine was removed in route-B). Deep fallback used
    // only when both MEM_DB_PATH and HOME are unset.
    std::env::temp_dir().join(format!("mem-app-{}-{sequence}.lance", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(map: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k: &str| {
            map.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn provider_kind_sends_off_machine_classification() {
        // Closes mempalace-diff-v3 #33 — the startup privacy warn keys
        // off this method. If a new variant is added to
        // `EmbeddingProviderKind`, the match arm in `sends_off_machine`
        // must classify it explicitly; this test acts as the trip-wire
        // by hardcoding the expected classification for every known
        // variant.
        assert!(!EmbeddingProviderKind::Fake.sends_off_machine());
        assert!(!EmbeddingProviderKind::EmbedAnything.sends_off_machine());
        assert!(EmbeddingProviderKind::OpenAi.sends_off_machine());
    }

    #[test]
    fn embedding_defaults_when_empty() {
        // Mirrors `EmbeddingSettings::development_defaults` exactly: when the
        // closure returns no env vars the parser must hand back the in-code
        // defaults verbatim. Update both sides together when those defaults
        // change (last touched: `47aff1e` flipped to EmbedAnything;
        // 2026-05-21 flipped `batch_size` 1 → 8 to amortize the per-tick
        // refresh cost — see config doc-comment).
        let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::EmbedAnything);
        assert_eq!(s.model, "Qwen/Qwen3-Embedding-0.6B");
        assert_eq!(s.dim, 1024);
        assert_eq!(s.openai_api_key, None);
        assert_eq!(s.batch_size, 8);
        assert_eq!(s.worker_poll_interval_ms, 10_000);
    }

    #[test]
    fn embedding_real_requires_api_key() {
        let err =
            EmbeddingSettings::from_env_vars(env(&[("EMBEDDING_PROVIDER", "real")])).unwrap_err();
        assert!(matches!(err, ConfigError::MissingOpenAiApiKey));
    }

    #[test]
    fn embedding_real_with_key_ok() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "openai"),
            ("OPENAI_API_KEY", "sk-test"),
            ("EMBEDDING_MODEL", "text-embedding-3-small"),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::OpenAi);
        assert_eq!(s.model, "text-embedding-3-small");
        assert_eq!(s.openai_api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn embedding_fake_ignores_empty_openai_key() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "fake"),
            ("OPENAI_API_KEY", ""),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::Fake);
        assert_eq!(s.openai_api_key, None);
    }

    #[test]
    fn embedding_embedanything_ok_without_openai_key() {
        let s = EmbeddingSettings::from_env_vars(env(&[
            ("EMBEDDING_PROVIDER", "embedanything"),
            ("EMBEDDING_MODEL", "sentence-transformers/all-MiniLM-L6-v2"),
            ("EMBEDDING_DIM", "384"),
        ]))
        .unwrap();
        assert_eq!(s.provider, EmbeddingProviderKind::EmbedAnything);
        assert_eq!(s.openai_api_key, None);
        assert_eq!(s.job_provider_id(), "embedanything");
    }

    #[test]
    fn transcript_embed_disabled_default_false() {
        let s = EmbeddingSettings::from_env_vars(|_| None).unwrap();
        assert!(!s.transcript_disabled);
    }

    #[test]
    fn transcript_embed_disabled_accepts_truthy_values() {
        for raw in ["1", "true", "TRUE", "True", "yes", "Yes", "YES"] {
            let s =
                EmbeddingSettings::from_env_vars(env(&[("MEM_TRANSCRIPT_EMBED_DISABLED", raw)]))
                    .unwrap_or_else(|e| panic!("parse failed for {raw:?}: {e}"));
            assert!(
                s.transcript_disabled,
                "expected MEM_TRANSCRIPT_EMBED_DISABLED={raw:?} to enable transcript_disabled"
            );
        }
    }

    #[test]
    fn transcript_embed_disabled_falsy_values_stay_disabled() {
        // Anything that isn't 1/true/yes (case-insensitive) leaves the flag false.
        for raw in ["0", "false", "no", ""] {
            let s =
                EmbeddingSettings::from_env_vars(env(&[("MEM_TRANSCRIPT_EMBED_DISABLED", raw)]))
                    .unwrap();
            assert!(
                !s.transcript_disabled,
                "expected MEM_TRANSCRIPT_EMBED_DISABLED={raw:?} to leave transcript_disabled=false"
            );
        }
    }

    #[test]
    fn auto_promote_defaults_on() {
        let s = AutoPromoteSettings::from_env_vars(|_| None).unwrap();
        // Worker ON by default — the MCP propose path floods
        // PendingConfirmation, so the sweep needs to keep up.
        assert!(s.enabled);
        assert_eq!(s.age_days, 3);
        assert_eq!(s.interval_secs, 3600);
        assert_eq!(s.decay_threshold, 0.5);
        assert_eq!(
            s.types,
            vec![
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleType::Implementation,
                CapabilityCapsuleType::Episode,
                CapabilityCapsuleType::Diary,
            ],
        );
    }

    #[test]
    fn auto_promote_disabled_via_env() {
        // Canonical opt-out (mirrors `MEM_VACUUM_DISABLED`).
        for raw in ["1", "true", "yes", "TRUE"] {
            let s = AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_DISABLED", raw)]))
                .unwrap();
            assert!(!s.enabled, "{raw:?} should disable");
        }
        for raw in ["0", "false", "no", ""] {
            let s = AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_DISABLED", raw)]))
                .unwrap();
            assert!(s.enabled, "{raw:?} should leave enabled");
        }
    }

    #[test]
    fn auto_promote_enabled_back_compat() {
        // Legacy env var still parsed. Truthy is redundant against
        // the default-on; falsy opts out, matching pre-flip users
        // who had `MEM_AUTO_PROMOTE_ENABLED=0` explicitly set.
        for raw in ["1", "true", "yes", "TRUE"] {
            let s = AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_ENABLED", raw)]))
                .unwrap();
            assert!(s.enabled, "{raw:?} should leave enabled");
        }
        for raw in ["0", "false", "no", ""] {
            let s = AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_ENABLED", raw)]))
                .unwrap();
            assert!(!s.enabled, "{raw:?} should disable");
        }
    }

    #[test]
    fn auto_promote_age_zero_rejected() {
        let err = AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_AGE_DAYS", "0")]))
            .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAutoPromoteAgeDays(ref s) if s == "0"));
    }

    #[test]
    fn auto_promote_interval_zero_rejected() {
        let err =
            AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_INTERVAL_SECS", "0")]))
                .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidAutoPromoteIntervalSecs(ref s) if s == "0"
        ));
    }

    #[test]
    fn auto_promote_decay_threshold_out_of_range_rejected() {
        let err =
            AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_DECAY_THRESHOLD", "1.5")]))
                .unwrap_err();
        assert!(matches!(
            err,
            ConfigError::InvalidAutoPromoteDecayThreshold(ref s) if s == "1.5"
        ));
    }

    #[test]
    fn auto_promote_types_csv_parses() {
        let s = AutoPromoteSettings::from_env_vars(env(&[(
            "MEM_AUTO_PROMOTE_TYPES",
            "experience, workflow",
        )]))
        .unwrap();
        assert_eq!(
            s.types,
            vec![
                CapabilityCapsuleType::Experience,
                CapabilityCapsuleType::Workflow,
            ],
        );
    }

    #[test]
    fn auto_promote_types_empty_string_honoured() {
        // Empty list is a valid "disable per-type without flipping the master
        // switch" signal — don't quietly fall back to defaults.
        let s = AutoPromoteSettings::from_env_vars(env(&[("MEM_AUTO_PROMOTE_TYPES", "")])).unwrap();
        assert!(s.types.is_empty());
    }

    #[test]
    fn auto_promote_types_unknown_rejected() {
        let err = AutoPromoteSettings::from_env_vars(env(&[(
            "MEM_AUTO_PROMOTE_TYPES",
            "experience,bogus",
        )]))
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidAutoPromoteType(ref s) if s == "bogus"));
    }

    #[test]
    fn vacuum_defaults_on() {
        let s = VacuumSettings::from_env_vars(|_| None).unwrap();
        assert!(!s.disabled);
        // Hourly cadence + 0-day cutoff, but NON-aggressive prune (keep
        // Lance's in-flight floor) — aggressive bypassed the floor and
        // raced the conflict_resolver into a core dump.
        assert_eq!(s.interval_secs, 3_600);
        assert_eq!(s.older_than_days, 0);
        assert!(!s.aggressive);
    }

    #[test]
    fn vacuum_disable_via_env() {
        for raw in ["1", "true", "yes", "TRUE"] {
            let s = VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_DISABLED", raw)])).unwrap();
            assert!(s.disabled, "{raw:?} should disable");
        }
        for raw in ["0", "false", "no", ""] {
            let s = VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_DISABLED", raw)])).unwrap();
            assert!(!s.disabled, "{raw:?} should leave enabled");
        }
    }

    #[test]
    fn vacuum_interval_zero_rejected() {
        let err =
            VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_INTERVAL_SECS", "0")])).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidVacuumIntervalSecs(ref s) if s == "0"));
    }

    #[test]
    fn vacuum_older_than_zero_accepted() {
        // `0` is the new default per development_defaults — every
        // manifest LanceDB's pruner can remove gets removed. The
        // previous "reject 0" guard predated the aggressive-default
        // flip and would have made the default invalid.
        let s = VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_OLDER_THAN_DAYS", "0")])).unwrap();
        assert_eq!(s.older_than_days, 0);
    }

    #[test]
    fn vacuum_aggressive_is_opt_in() {
        // Default is non-aggressive (safe). MEM_VACUUM_AGGRESSIVE=1 opts in.
        for raw in ["1", "true", "yes", "TRUE"] {
            let s = VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_AGGRESSIVE", raw)])).unwrap();
            assert!(s.aggressive, "{raw:?} should opt INTO aggressive");
        }
        for raw in ["0", "false", "no", ""] {
            let s = VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_AGGRESSIVE", raw)])).unwrap();
            assert!(!s.aggressive, "{raw:?} should leave the safe default");
        }
    }

    #[test]
    fn vacuum_preserve_unverified_wins_over_aggressive() {
        // Back-compat: PRESERVE_UNVERIFIED forces the safe default and
        // overrides an explicit MEM_VACUUM_AGGRESSIVE so "stay safe" holds.
        let s =
            VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_PRESERVE_UNVERIFIED", "1")])).unwrap();
        assert!(!s.aggressive);
        let s = VacuumSettings::from_env_vars(env(&[
            ("MEM_VACUUM_AGGRESSIVE", "1"),
            ("MEM_VACUUM_PRESERVE_UNVERIFIED", "1"),
        ]))
        .unwrap();
        assert!(
            !s.aggressive,
            "PRESERVE_UNVERIFIED must win over AGGRESSIVE"
        );
    }

    #[test]
    fn vacuum_older_than_non_numeric_rejected() {
        let err = VacuumSettings::from_env_vars(env(&[("MEM_VACUUM_OLDER_THAN_DAYS", "soon")]))
            .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidVacuumOlderThanDays(ref s) if s == "soon"));
    }

    #[test]
    fn backend_defaults_to_lance() {
        let (kind, pg, ch) = parse_backend(|_| None).unwrap();
        assert_eq!(kind, BackendKind::Lance);
        assert_eq!(pg, None);
        assert_eq!(ch, None);
    }

    #[test]
    fn backend_postgres_requires_url() {
        // postgres selected but no URL → loud error, not a silent fallback.
        let err = parse_backend(env(&[("MEM_BACKEND", "postgres")])).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidBackend(ref s) if s.contains("MEM_POSTGRES_URL"))
        );
    }

    #[test]
    fn backend_postgres_with_url_ok() {
        let (kind, pg, ch) = parse_backend(env(&[
            ("MEM_BACKEND", "postgres"),
            ("MEM_POSTGRES_URL", "postgres://localhost/mem"),
        ]))
        .unwrap();
        assert_eq!(kind, BackendKind::Postgres);
        assert_eq!(pg.as_deref(), Some("postgres://localhost/mem"));
        assert_eq!(ch, None);
    }

    #[test]
    fn backend_clickhouse_requires_url() {
        // clickhouse selected but no URL → loud error (mirrors postgres).
        let err = parse_backend(env(&[("MEM_BACKEND", "clickhouse")])).unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidBackend(ref s) if s.contains("MEM_CLICKHOUSE_URL"))
        );
    }

    #[test]
    fn backend_clickhouse_with_url_ok() {
        // The `ch` alias also resolves to the ClickHouse backend.
        let (kind, pg, ch) = parse_backend(env(&[
            ("MEM_BACKEND", "ch"),
            ("MEM_CLICKHOUSE_URL", "http://localhost:8123"),
        ]))
        .unwrap();
        assert_eq!(kind, BackendKind::Clickhouse);
        assert_eq!(pg, None);
        assert_eq!(ch.as_deref(), Some("http://localhost:8123"));
    }

    #[test]
    fn backend_clickhouse_full_word_with_url_ok() {
        // The full `clickhouse` literal resolves identically to the `ch`
        // alias and carries MEM_CLICKHOUSE_URL into the third tuple slot.
        let (kind, pg, ch) = parse_backend(env(&[
            ("MEM_BACKEND", "clickhouse"),
            ("MEM_CLICKHOUSE_URL", "http://ch.local:8123"),
        ]))
        .unwrap();
        assert_eq!(kind, BackendKind::Clickhouse);
        assert_eq!(pg, None);
        assert_eq!(ch.as_deref(), Some("http://ch.local:8123"));
    }

    #[test]
    fn backend_unknown_value_rejected() {
        let err = parse_backend(env(&[("MEM_BACKEND", "mysql")])).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidBackend(_)));
    }

    #[test]
    fn evolution_defaults_when_empty() {
        // Mirrors `EvolutionSettings::development_defaults` — worker OFF,
        // daily cadence, K=3 anti-jitter gate, doc evolution-worker §9.
        let s = EvolutionSettings::from_env_vars(|_| None).unwrap();
        assert!(!s.enabled, "evolution must be opt-in (default OFF)");
        assert_eq!(s.interval_secs, 86_400);
        assert_eq!(s.k_cycles, 3);
        assert!((s.evidence_decay - 0.7).abs() < 1e-6);
        assert!((s.hysteresis - 0.5).abs() < 1e-6);
        assert!((s.cluster_threshold - 0.80).abs() < 1e-6);
        assert!((s.merge_threshold - 0.88).abs() < 1e-6);
        assert_eq!(s.generalize_min_n, 4);
        assert_eq!(s.scan_limit, 2_000);
        assert_eq!(s.synthesis, EvolutionSynthesisMode::Off);
    }

    #[test]
    fn evolution_env_overrides_apply() {
        let s = EvolutionSettings::from_env_vars(env(&[
            ("MEM_EVOLUTION_ENABLED", "1"),
            ("MEM_EVOLUTION_INTERVAL_SECS", "3600"),
            ("MEM_EVOLUTION_K_CYCLES", "5"),
            ("MEM_EVOLUTION_EVIDENCE_DECAY", "0.9"),
            ("MEM_EVOLUTION_HYSTERESIS", "0.4"),
            ("MEM_EVOLUTION_CLUSTER_THRESHOLD", "0.75"),
            ("MEM_EVOLUTION_MERGE_THRESHOLD", "0.9"),
            ("MEM_EVOLUTION_GENERALIZE_MIN_N", "6"),
            ("MEM_EVOLUTION_SCAN_LIMIT", "500"),
            ("MEM_EVOLUTION_SYNTHESIS", "review"),
        ]))
        .unwrap();
        assert!(s.enabled);
        assert_eq!(s.interval_secs, 3_600);
        assert_eq!(s.k_cycles, 5);
        assert!((s.evidence_decay - 0.9).abs() < 1e-6);
        assert!((s.hysteresis - 0.4).abs() < 1e-6);
        assert!((s.cluster_threshold - 0.75).abs() < 1e-6);
        assert!((s.merge_threshold - 0.9).abs() < 1e-6);
        assert_eq!(s.generalize_min_n, 6);
        assert_eq!(s.scan_limit, 500);
        assert_eq!(s.synthesis, EvolutionSynthesisMode::Review);
    }

    #[test]
    fn evolution_invalid_values_rejected() {
        // Zero / out-of-range / non-numeric inputs must error loudly
        // (dedup precedent), not silently fall back.
        for (var, bad) in [
            ("MEM_EVOLUTION_INTERVAL_SECS", "0"),
            ("MEM_EVOLUTION_K_CYCLES", "0"),
            ("MEM_EVOLUTION_EVIDENCE_DECAY", "1.5"),
            ("MEM_EVOLUTION_HYSTERESIS", "0"),
            ("MEM_EVOLUTION_CLUSTER_THRESHOLD", "1.2"),
            ("MEM_EVOLUTION_MERGE_THRESHOLD", "nope"),
            ("MEM_EVOLUTION_GENERALIZE_MIN_N", "1"),
            ("MEM_EVOLUTION_SCAN_LIMIT", "0"),
            // local / api synthesis backends are designed (doc §6.2) but
            // not implemented in E1 — selecting one must fail loudly.
            ("MEM_EVOLUTION_SYNTHESIS", "local"),
            ("MEM_EVOLUTION_SYNTHESIS", "api"),
        ] {
            let err = EvolutionSettings::from_env_vars(env(&[(var, bad)]));
            assert!(err.is_err(), "{var}={bad} must be rejected");
        }
    }

    #[test]
    fn evolution_prune_idle_cycles_default_and_override() {
        // ⑥ Hebbian weak-edge retirement (E4): default 3 idle sweep
        // cycles, env-tunable, zero rejected loudly (dedup precedent).
        let s = EvolutionSettings::from_env_vars(env(&[])).unwrap();
        assert_eq!(s.prune_idle_cycles, 3);
        let s = EvolutionSettings::from_env_vars(env(&[("MEM_EVOLUTION_PRUNE_IDLE_CYCLES", "5")]))
            .unwrap();
        assert_eq!(s.prune_idle_cycles, 5);
        assert!(
            EvolutionSettings::from_env_vars(env(&[("MEM_EVOLUTION_PRUNE_IDLE_CYCLES", "0")]))
                .is_err()
        );
    }

    #[test]
    fn dedup_default_threshold_is_mirror_duplicate_only() {
        // §12.1 decision (evolution-worker.md, settled with E2): with the
        // evolution ① merge operator live, dedup narrows to mirror-duplicate
        // duty — default cosine floor 0.95, safely above the evolution
        // merge_threshold (0.88) so the two workers never compete for the
        // same pair. MEM_DEDUP_THRESHOLD still overrides.
        let s = DedupSettings::from_env_vars(env(&[])).unwrap();
        assert!(!s.enabled, "dedup stays opt-in");
        assert!(
            (s.threshold - 0.95).abs() < 1e-6,
            "default dedup threshold must be 0.95 (mirror-only), got {}",
            s.threshold
        );
    }
}
