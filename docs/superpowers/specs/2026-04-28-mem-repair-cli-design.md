# `mem repair` Subcommand — Design

> Closes mempalace-diff §8 #4 (with deviation: subcommand on the existing `mem` binary, not a separate `bin/mem-repair`).

## Summary

Adds a `mem repair` subcommand that gives operators a way to actively diagnose and rebuild the `usearch` HNSW sidecar created by §8 #3. Two modes:

- `mem repair --check` (default) — read-only health check, returns structured diagnostic + exit code
- `mem repair --rebuild` — destructively recreates the sidecar from DuckDB

Both modes are **offline** operations because DuckDB's bundled mode is single-writer and locks the file; a running `mem serve` will cause both modes to fail with a clear "stop the service first" message.

## Goals

- Surface index health proactively instead of waiting for the next `mem serve` startup
- Give operators a manual repair lever for cases where the automatic `open_or_rebuild` path can't reach (e.g., need to force-rebuild without restarting the service immediately)
- Provide structured JSON output suitable for cron / dashboards / CI
- Reuse the existing `VectorIndex::open_or_rebuild` rebuild path — zero new core logic

## Non-Goals

- Online (concurrent-with-`mem serve`) diagnostics — physically blocked by DuckDB's single-writer lock; would require an architecture change (DuckDB attach mode or shared-memory primitive) that is far beyond #4
- Sample-query / semantic-regression checks — out of scope, would couple `--check` to embedding-provider availability
- Rebuilding from a different source than the configured `MEM_DB_PATH` — operators should run the command in the same env as `mem serve`
- Rebuilding partial state (per-tenant, per-time-range) — full rebuild is the only flow
- Daemon / service mode for the repair command — one-shot CLI only

## Decisions (resolved during brainstorming)

- **Q1**: two modes (`--check` / `--rebuild`) instead of single auto-mode — operators want explicit control over destructive action
- **Q2**: `--check` includes opening the binary index file (not just metadata) — catches sidecar corruption before users hit it
- **Q3**: text default + `--json` flag for structured output
- **Q4 (revised)**: both modes are offline; the original Q1 framing implied `--check` could run online but DuckDB's lock makes that physically impossible

## CLI Surface

In `src/main.rs` add to the `Command` enum:

```rust
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the HTTP memory service (default).
    Serve,
    /// Run the MCP (Model Context Protocol) stdio server.
    Mcp,
    /// Diagnose or rebuild the vector index sidecar.
    Repair(RepairArgs),
}

#[derive(Debug, Args)]
struct RepairArgs {
    #[command(flatten)]
    mode: RepairMode,
    /// Output structured JSON instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[group(multiple = false)]
struct RepairMode {
    /// Read-only health check (default).
    #[arg(long)]
    check: bool,
    /// Force rebuild from DuckDB (requires `mem serve` to be stopped).
    #[arg(long)]
    rebuild: bool,
}
```

Invocations:

- `mem repair` → implicit `--check`
- `mem repair --check`
- `mem repair --rebuild`
- `mem repair --check --json`
- `mem repair --rebuild --json`

Configuration is loaded via the existing `Config::from_env()` — same env vars as `mem serve` and `mem mcp` (`MEM_DB_PATH`, `EMBEDDING_*`, etc.). No new env vars introduced.

## Diagnostic Flow (`--check`)

```
diagnose(config) -> DiagnosticReport:
  1. open DuckDbRepository at config.db_path
     ├─ IO/lock error → DiagnosticStatus::DbUnavailable { reason }
     └─ ok → continue

  2. live_count = repo.count_total_memory_embeddings()

  3. (idx_path, meta_path) = sidecar_paths(&config.db_path)

  4. file existence:
     ├─ either missing → DiagnosticStatus::SidecarMissing { which }
     └─ both present → continue

  5. read meta.json:
     ├─ parse fail → DiagnosticStatus::MetaCorrupt { reason }
     └─ ok → meta

  6. fingerprint match:
     ├─ meta.{provider, model, dim} != config fingerprint
        → DiagnosticStatus::FingerprintMismatch { stored, current }
     └─ ok → continue

  7. dim guard:
     ├─ meta.dim == 0 → DiagnosticStatus::FingerprintMismatch (zero-dim corruption)
     └─ ok → continue

  8. load index file (the §3 启示 #2 carryover — actually open the binary):
     ├─ usearch load fail → DiagnosticStatus::IndexCorrupt { reason }
     └─ ok → loaded_size

  9. parity:
     ├─ loaded_size != meta.row_count
        → DiagnosticStatus::IndexMetaDrift { index_size, meta_count }
     ├─ meta.row_count != live_count
        → DiagnosticStatus::DbDrift { meta_count, db_count }
     └─ all equal → DiagnosticStatus::Healthy { rows: live_count }
```

Each step short-circuits on failure — later steps only make sense once earlier ones pass.

## Types

```rust
pub struct DiagnosticReport {
    pub status: DiagnosticStatus,
    pub paths: PathInfo,
    pub elapsed_ms: u64,
}

pub struct PathInfo {
    pub db: PathBuf,
    pub index: PathBuf,
    pub meta: PathBuf,
}

pub enum SidecarFile { Index, Meta }

pub enum DiagnosticStatus {
    Healthy { rows: i64 },
    SidecarMissing { which: SidecarFile },
    MetaCorrupt { reason: String },
    FingerprintMismatch { stored: VectorIndexFingerprint, current: VectorIndexFingerprint },
    IndexCorrupt { reason: String },
    IndexMetaDrift { index_size: usize, meta_count: usize },
    DbDrift { meta_count: usize, db_count: i64 },
    DbUnavailable { reason: String },
}
```

`DiagnosticReport`, `DiagnosticStatus`, `PathInfo`, `SidecarFile`, and `VectorIndexFingerprint` (already exists from §3) all derive `Serialize` for JSON output. `DiagnosticStatus` uses `#[serde(tag = "kind")]` so the variant name appears as the `kind` field and the variant payload is flattened into siblings — this matches the JSON examples below where each `details` object has `"kind": "DbDrift"` plus the variant's named fields.

## Rebuild Flow (`--rebuild`)

```
rebuild(config):
  1. open DuckDbRepository
     ├─ lock error → DbUnavailable + exit 2
     └─ ok → continue
  2. (idx_path, meta_path) = sidecar_paths(...)
  3. best-effort delete (NotFound is OK):
       fs::remove_file(idx_path);
       fs::remove_file(meta_path);
  4. fp = VectorIndexFingerprint from config.embedding
  5. let idx = VectorIndex::open_or_rebuild(&repo, &config.db_path, &fp).await?;
     // because files were deleted, open_or_rebuild MUST take the rebuild branch
  6. report rows = idx.size(), elapsed_ms
```

Reuses the existing `open_or_rebuild` end-to-end. No new core logic; the file deletion is the only "force" mechanism.

## Exit Code Matrix

| Status | exit | Meaning |
|---|---|---|
| `Healthy` | 0 | Index is consistent with DuckDB; no action needed |
| `SidecarMissing` / `MetaCorrupt` / `FingerprintMismatch` / `IndexCorrupt` / `IndexMetaDrift` / `DbDrift` | 1 | Drift or corruption detected; run `mem repair --rebuild` |
| `DbUnavailable` | 2 | Cannot complete diagnosis (service running, path wrong, permission, etc.) |
| Rebuild succeeded | 0 | New sidecar landed |
| Rebuild failed (write error, FFI error) | 2 | Surface error details; existing files may already be deleted |

## Output Formats

### Text (default)

`--check`:

```
$ mem repair --check
✅ Healthy: 1247 rows. Sidecar at /data/mem.duckdb.usearch
   (db_count=1247, meta.row_count=1247, index.size=1247) elapsed=18ms

$ mem repair --check     # drift case
❌ Drift detected: meta.row_count=1247 but db has 1250.
   → Run `mem repair --rebuild` to reconcile.
   elapsed=12ms

$ mem repair --check     # corruption case
❌ Index file is corrupt: usearch load failed: <error>
   → Run `mem repair --rebuild` to recreate from DuckDB.
   elapsed=8ms

$ mem repair --check     # service running / lock
❌ Could not open DB at /data/mem.duckdb: file is locked.
   Is `mem serve` running? Stop the service before running this command.
   elapsed=2ms
```

`--rebuild`:

```
$ mem repair --rebuild
🔨 Rebuilding vector index from /data/mem.duckdb...
✅ Rebuilt: 1247 rows in 832ms.
   New sidecar at /data/mem.duckdb.usearch
```

Each text output ends with `elapsed=<N>ms` so operators can spot pathological slowness without needing `--json`.

### JSON (`--json`)

`--check`:

```json
{
  "command": "check",
  "status": "drift",
  "exit_code": 1,
  "details": {
    "kind": "DbDrift",
    "meta_count": 1247,
    "db_count": 1250
  },
  "paths": {
    "db": "/data/mem.duckdb",
    "index": "/data/mem.duckdb.usearch",
    "meta": "/data/mem.duckdb.usearch.meta.json"
  },
  "elapsed_ms": 12
}
```

`status` enum values: `"healthy"`, `"drift"`, `"corrupt"`, `"db_unavailable"`, `"config_error"`. The first four map onto the `DiagnosticStatus` variants below; `"config_error"` is emitted when `Config::from_env` itself fails before `diagnose` can run (exit 2).

The variants map as follows:

- `Healthy` → `"healthy"`
- `DbDrift` / `IndexMetaDrift` → `"drift"`
- `SidecarMissing` / `MetaCorrupt` / `FingerprintMismatch` / `IndexCorrupt` → `"corrupt"`
- `DbUnavailable` → `"db_unavailable"`

`details.kind` carries the full enum variant name; `details` carries the variant's payload fields. This gives operators a coarse three-state filter (`jq '.status'`) plus full structure when needed.

`--rebuild`:

```json
{ "command": "rebuild", "status": "rebuilt", "exit_code": 0, "rows": 1247, "elapsed_ms": 832 }
{ "command": "rebuild", "status": "db_unavailable", "exit_code": 2, "details": {...} }
{ "command": "rebuild", "status": "rebuild_failed", "exit_code": 2, "details": {"reason": "..."} }
```

## Error Handling

| Scenario | Exit | Handling |
|---|---|---|
| Config parse failure (bad env var) | 2 | Fail before `diagnose` runs; in JSON mode emit `{"status": "config_error", "exit_code": 2, "details": {"reason": "..."}}`; in text mode print the underlying error |
| `mem repair --check --rebuild` (mutually exclusive flags) | 2 | clap's `#[group(multiple = false)]` rejects this at parse time with its own error and exit 2 — no extra handling needed |
| DuckDB lock contention | 2 | Translate to `DbUnavailable { reason: "file is locked: ..." }` and the "stop mem serve" hint in text mode |
| Sidecar read permission deny | 2 | Same `DbUnavailable` channel (path-not-readable variant of message) |
| `--rebuild` write failure (disk full, permission) | 2 | New `rebuild_failed` JSON status; text output prints the underlying error |
| usearch FFI panic | abort | Process exits; user can re-run; not caught |

## Module Layout

**New file**: `src/storage/vector_index_diagnose.rs` (or extend `vector_index.rs` if it stays under ~600 LOC; the file was 530 LOC after #3, this would push to ~700, so split is justified)

```rust
// vector_index_diagnose.rs
pub fn diagnose(repo: &DuckDbRepository, db_path: &Path, fp: &VectorIndexFingerprint)
    -> Result<DiagnosticReport, VectorIndexError>;
```

**New file**: `src/cli/repair.rs` — the CLI handler that:
1. Builds `Config` from env
2. Opens `DuckDbRepository` (catches lock errors → `DbUnavailable`)
3. Dispatches to `diagnose(...)` or `rebuild(...)`
4. Formats output (text or JSON) and exits with the right code

**Modify** `src/main.rs` — add the `Repair` subcommand variant + dispatch arm.

**Modify** `src/lib.rs` — re-export anything `main.rs` needs (`cli::repair::run`).

## Testing

New file `tests/repair_cli.rs` (integration tests that call the diagnostic functions directly, not via `std::process::Command`):

1. **Healthy**: ingest one row, run worker, attach index, save → `diagnose` returns `Healthy { rows: 1 }`
2. **SidecarMissing**: as above, then delete one sidecar file → `diagnose` returns `SidecarMissing { which: ... }`
3. **MetaCorrupt**: as above, then truncate `.meta.json` → `diagnose` returns `MetaCorrupt`
4. **IndexCorrupt**: as above, then truncate `.usearch` → `diagnose` returns `IndexCorrupt`
5. **FingerprintMismatch**: build with dim=256, call `diagnose` with a fingerprint having dim=128 → `FingerprintMismatch`
6. **DbDrift**: build sidecar with N rows, then bypass service to add 1 more row to DuckDB → `diagnose` returns `DbDrift { meta_count: N, db_count: N+1 }`
7. **rebuild happy path**: trigger any drift case, call `rebuild` function, then call `diagnose` → expect `Healthy` with the new count
8. **JSON serialization**: for each `DiagnosticStatus` variant, build a `DiagnosticReport`, serialize via `serde_json::to_value`, assert: top-level `status` string is the expected coarse enum (`"healthy"` / `"drift"` / `"corrupt"` / `"db_unavailable"`); `details.kind` is the exact variant name (`"DbDrift"`, etc.); `paths.db` / `paths.index` / `paths.meta` are present and absolute
9. (Skipped — high friction, low value) Surface CLI test via `std::process::Command` — exit codes match. Mark `#[ignore]` if added; not required.

`DbUnavailable` is hard to test deterministically (would need to actually hold a DuckDB lock). Skip — text-output path covers it for manual verification, and the `Result<DiagnosticReport>` wrapping makes the unit tests around `diagnose` itself sufficient.

## Change Inventory

**New files**:
- `src/storage/vector_index_diagnose.rs` (~150 LOC) — pure logic, no I/O orchestration
- `src/cli/mod.rs` + `src/cli/repair.rs` (~200 LOC) — CLI handler + output formatting
- `tests/repair_cli.rs` (~250 LOC)

**Modified files**:
- `src/main.rs` — add `Repair(RepairArgs)` variant + dispatch
- `src/lib.rs` — `pub mod cli;` and re-export
- `src/storage/mod.rs` — re-export `DiagnosticReport`, `DiagnosticStatus`, `diagnose`
- `docs/mempalace-diff.md` — once landed, mark §8 row #4 ✅

## Out of Scope

- A `--force` flag on `--rebuild` to also rebuild when current state is healthy — not needed since `--rebuild` is destructive by definition; if user runs it, they want it
- Rebuild progress reporting (e.g. `1247/5000 rows...`) — current scale doesn't justify it; tracing already logs `rebuilt vector index: N rows in T ms`
- Logging integration with the `mem serve` log file — repair is a one-shot, stderr is fine
- Auto-detection of running `mem serve` via PID file or socket probe — DuckDB's lock surfaces the contention naturally; no need to duplicate

## References

- mempalace-diff §3 启示 #2 (HNSW health-check pattern from MemPalace `chroma.py::hnsw_capacity_status`)
- mempalace-diff §8 row #4 (the roadmap entry being closed)
- `docs/superpowers/specs/2026-04-27-vector-index-sidecar-design.md` — the §3 implementation this builds on
- `src/storage/vector_index.rs::open_or_rebuild` — reused verbatim by the rebuild path
- `src/storage/vector_index.rs::sidecar_paths` — reused for the file deletion step
