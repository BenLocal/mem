# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
bd dolt push          # Push beads data to remote
```

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:ca08a54f -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds
<!-- END BEADS INTEGRATION -->

## Codebase Orientation (Rust / mem)

### Common commands

```bash
cargo run                # default = `mem serve` (axum HTTP on 127.0.0.1:3000)
cargo run -- serve       # explicit HTTP server mode
cargo run -- mcp         # stdio MCP server; forwards to MEM_BASE_URL

cargo test -q                          # full suite (integration tests in tests/)
cargo test --test search_api           # single integration test file
cargo test ingest::compute_content     # single test fn (path or substring)
cargo fmt --check                      # required by CI
cargo clippy --all-targets -- -D warnings   # required by CI

cross build --release                  # cross-compile (reads ./Cross.toml)
```

Key env vars: `MEM_DB_PATH` (DuckDB file), `BIND_ADDR` (HTTP bind), `MEM_BASE_URL` / `MEM_TENANT` (MCP forwarder), `MEM_MCP_EXPOSE_EMBEDDINGS=1` (admin tools).

### Architecture (non-obvious bits)

- **Single binary, two modes**: `mem serve` (HTTP) and `mem mcp` (stdio). MCP is a thin forwarder over `MEM_BASE_URL` — *not* a separate process talking to DuckDB directly. Two HTTP services pointed at the same DB will fight; one writer.
- **Storage layer (`src/storage/`)**: DuckDB (bundled, single file) wrapped in `Arc<Mutex<Connection>>` — every DB call serializes through one mutex. This is the de-facto concurrency boundary; treat it as the equivalent of "one transaction at a time." Graph layer is dual: `IndraDbGraphAdapter` (in-memory `MemoryDatastore`) + `LocalGraphAdapter` fallback.
- **Pipeline (`src/pipeline/`)** is the heart of behavior, not `service/`. Four stages: `ingest.rs` (status assignment, content_hash via sha2, graph edge extraction) → `retrieve.rs` (additive integer scoring: semantic + lexical + scope + intent + confidence + freshness − decay + graph) → `compress.rs` (token-budgeted four-section output: directives / relevant_facts / reusable_patterns / suggested_workflow) → `workflow.rs` (episode → workflow extraction).
- **Embedding pipeline is async + persistent**: writes enqueue rows in `embedding_jobs` (DuckDB table), `service/embedding_worker.rs` consumes with retry/backoff. Provider trait under `src/embedding/` (embed_anything local / OpenAI / fake). Failure does not block ingest — jobs go `pending → processing → completed | failed | stale`.
- **Memory has lifecycle, not just CRUD**: `MemoryStatus = Provisional | Active | PendingConfirmation`, `supersedes_memory_id` forms version chains, `feedback_events` mutate `confidence`/`decay_score`. New code touching memories must respect status transitions — see `domain/memory.rs`.
- **Schema lives in `db/schema/*.sql`** and is applied at startup by `src/storage/schema.rs`. Migrations are append-only files; do not edit historical ones in-place.

### Design discipline

- **Verbatim rule**: `memories.content` is the **fact source** — never rewrite, never truncate at storage. `memories.summary` is **index/hint only** — never use it as the basis for an answer or quote. Output-layer compression (`pipeline/compress.rs`) operates on `content`, never replaces it.
- **Two-axis layering** (see `docs/mempalace-diff.md` §8): 📦 storage stays verbatim, 🔍 indexing/ranking/lifecycle is where structured signals live, ⚙️ infra/bug-fix is its own track. Before touching ranking, ingest, or output, name which layer you're in.

### Where to find design context

- **`docs/mempalace-diff.md`** — comparison with MemPalace, roadmap (§8 numbered items #1–#13). Commit messages reference these (e.g. `closes mempalace-diff §8 #1`); read it before non-trivial design changes.
- **`docs/superpowers/plans/`** — historical implementation plans (TDD-style steps); useful for "why is it like this" archaeology.
- **`docs/superpowers/skills/mem-mcp-codex/SKILL.md`** — how caller agents (Codex/Cursor) are expected to use the MCP tools; check before changing tool surface.
