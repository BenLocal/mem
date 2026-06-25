# `mem sync` — store-to-store sync CLI (design)

**Date:** 2026-06-25
**Status:** approved design, pending implementation plan
**Scope tag:** `feat(cli)` — closes a follow-up to the multi-backend work (postgres/clickhouse now default deps)

## Goal

A CLI that copies data from one storage backend to another — primarily migrating
an existing Lance store into ClickHouse or Postgres, but **any → any** since all
three backends implement the `Backend` umbrella trait. The copy is **verbatim**:
rows are copied through the target backend's `insert_*` / `upsert_*` / `add_edge_*`
methods with their original ids, timestamps, version chains, and lifecycle state
(`confidence` / `decay_score` / status), **not** re-ingested through the pipeline.

## Decisions (locked during brainstorming)

| Axis | Decision |
|---|---|
| **Scope** | Full domain, **zero new trait methods** — use only existing reads/writes. |
| **Direction** | **Any → any** (`--from <spec> --to <spec>`). |
| **Embeddings** | **Rebuild all on target** — copy rows only, enqueue embedding jobs; the target's `mem serve` embedding worker fills vectors. Provider/dim-agnostic. |
| **Approach** | **Live streaming copy**, in-process, trait-only, idempotent (re-run = resume). |
| **Fidelity** | Verbatim row copy (preserve ids / timestamps / lifecycle state). |

## Non-goals (v1)

- Tenant enumeration / `--all-tenants` (no trait read for it).
- Entity alias migration (not enumerable through the trait — see Gaps).
- Closed/historical graph edge reconstruction.
- Migrating operational/transient tables: `embedding_jobs`, `mine_cursors`, raw `sessions`.
- An on-disk interchange format / offline transfer (rejected approach ②).

## CLI surface

```bash
mem sync --from <spec> --to <spec> --tenant <t> [--tenant <t> …] \
         [--domains capsules,transcripts,entities,episodes,graph] \
         [--batch-size 200] [--dry-run] [--verbose]
```

- **`<spec>`** = `<kind>:<locator>`:
  - `lance:/path/to/dataset-dir`
  - `postgres:postgres://user:pass@host:5432/db`
  - `clickhouse:http://user:pass@host:8123`
- **`--tenant`** — **required, repeatable** (≥1). No tenant enumeration exists.
- **`--domains`** — optional subset; default = all five, run in dependency order.
- **`--dry-run`** — read + count only, zero writes.
- **`--verbose`** — per-batch / per-session progress lines.
- Handler returns `i32` exit code: `0` clean, `1` if any batch/domain failed.

## Architecture

Single in-process command. It opens **two** `Arc<dyn Backend>` handles directly
(bypassing `app::from_config`, which would also spawn the worker fleet):

```
open_backend(spec) -> Arc<dyn Backend>
  lance:<dir>        -> Store::open_with_provider(dir, provider)
  postgres:<url>     -> PostgresCapsuleStore::connect(url)
  clickhouse:<url>   -> ClickHouseBackend::connect(url).apply_migrations()
```

- The **source** is read-only; it needs an embedding provider only because
  `Store::open_with_provider` requires one — a no-op/fake provider is fine (the
  source never embeds).
- The **target** needs the process's embedding config (`Config::from_env`) so the
  embedding-job rows it enqueues carry the right provider id; the target's own
  `mem serve` worker drains them later.

## Components (one fn each, single responsibility)

- `open_backend(&str) -> Result<Arc<dyn Backend>>` — spec parse + connect/migrate.
- `parse_spec(&str) -> Result<(BackendKind, String)>` — pure, unit-tested.
- Per-domain copiers, run **in dependency order** per tenant:
  `copy_entities` → `copy_capsules` → `copy_episodes` → `copy_transcripts` → `copy_graph_edges`.
  Uniform signature `(src, dst, tenant, opts) -> DomainReport { copied, skipped, failed }`.
- `DomainReport` aggregation + a final printed summary per tenant + grand total.

## Data flow (verbatim, idempotent)

General shape per domain: **read source → subtract ids already in target (resume)
→ write target in batches**.

- **capsules** — `list_capability_capsule_ids_for_tenant` → per id
  `list_capability_capsule_versions_for_tenant` to collect *every* version id
  (guarantees all versions + all statuses regardless of whether the head-list
  dedups) → `fetch_capability_capsules_by_ids` in batches → target
  `insert_capability_capsules` (original id / timestamps / confidence / decay /
  `supersedes_memory_id` chain preserved) → target `enqueue_embedding_jobs` for the
  newly-written capsules (vectors rebuilt on the target).
- **transcripts** — `list_transcript_sessions` → per session
  `get_conversation_messages_by_session_paged` → target `create_conversation_messages`
  (its create path auto-enqueues transcript embedding jobs for `embed_eligible` blocks).
- **entities** — `list_entities(tenant, None, None, large_limit)` → target `resolve_or_create`.
- **episodes** — `list_successful_episodes_for_tenant` → target `insert_episode`.
- **graph edges** — per node (all capsule ids + all entity node ids) `neighbors`
  to read **active** edges → dedupe by `(from, predicate, to)` → target
  `add_edge_direct` (preserves the edge's `valid_from`).

### Resume / idempotency

- Before each domain, read the target's existing ids for that tenant (e.g.
  `dst.list_capability_capsule_ids_for_tenant`) into a skip-set; skip ids already
  present. Verbatim ids make this exact.
- No checkpoint file — **re-running the command is the resume mechanism**.

## Error handling

- Per-domain, per-batch `try`/`continue`: a failing batch is counted in
  `DomainReport.failed` and does not abort the rest of the domain or other domains.
- Domains are independent — an edge-copy failure never loses already-copied capsules.
- Any non-zero `failed` total → process exit code `1` (so re-run picks up the rest).

## ⚠️ Known gaps (all stem from "zero new trait methods")

1. **Tenant enumeration** — must pass `--tenant` explicitly; no read lists tenants.
2. **Entity aliases not migrated** — `Entity { entity_id, tenant, canonical_name, kind, created_at }`
   carries no aliases, and there is no "list aliases" read. Canonical entities migrate;
   the alias→`entity_id` mappings are lost. Bounded impact: graph edges reference
   `entity:<uuid>` and are copied directly (so edges stay intact), and the target
   re-creates alias mappings as topics recur during future ingest.
3. **Active edges only** — `add_edge_direct` writes active edges; closed/historical
   (`valid_to` set) edges are not faithfully reconstructed.
4. **Async embedding tail** — target rows land first; vectors are filled later by the
   target's embedding worker (capsule jobs enqueued here; transcript jobs auto-enqueued
   by the create path).
5. **Operational/transient tables skipped** — `embedding_jobs`, `mine_cursors`, raw `sessions`.

> Gaps 1–3 are each closeable by adding one small read method in a future v2
> (`list_tenants`, `list_aliases_for_tenant`, a full-edge bulk read). v1 honors the
> "zero new trait" decision and documents them honestly.

## Testing

- **Unit (pure):** `parse_spec` (each kind + malformed), batching boundaries, skip-set
  difference logic.
- **Integration:**
  - `lance → lance` round-trip — seed a temp source `Store`, sync to a temp target
    `Store`, assert parity (capsule ids + version chains + lifecycle fields, transcript
    blocks, entities, episodes, active edges). Uses existing test infra; always runs.
  - `lance → clickhouse` / `lance → postgres` — real-DB parity, runs only when
    `MEM_TEST_CLICKHOUSE_URL` / `MEM_TEST_POSTGRES_URL` is set, self-skips otherwise
    (mirrors `tests/{clickhouse,postgres}_backend.rs`).

## Open items to confirm at implementation time

- Whether `list_capability_capsules_for_tenant` returns all rows or active heads
  (drives whether the ids+versions+fetch path is strictly necessary — designed
  defensively to use it regardless).
- Exact node-id formats to enumerate for the edge walk (`mem:<id>` capsules +
  `entity:<uuid>` entities).
- Whether `insert_capability_capsules` is append vs upsert on each backend (the
  skip-set makes re-runs safe either way, but informs single-run dup behavior).
