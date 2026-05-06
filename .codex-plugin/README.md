# mem — Codex plugin

Local-first Rust memory service packaged as a Codex CLI plugin.

## What you get

- **Hooks** (auto-fire, no user action):
  - `SessionStart` → injects ~800-token wake-up summary from prior memories.
  - `Stop` → every ~15 user exchanges, fires `mem mine` in background to extract memories from the transcript.
  - `PreCompact` → final mine before context compression so nothing is lost.
- **MCP tools** (configured separately, see below): `memory_search`, `memory_ingest`, `memory_get`, `memory_graph_neighbors`, `memory_feedback`, …

## Prerequisites

1. `mem` binary on `PATH`. Build with `cargo build --release` from the repo root and symlink `target/release/mem` into a `PATH` directory, or `cargo install --path .`.
2. `mem serve` running on `http://127.0.0.1:3000` (the hooks talk to it via `mem mine` / `mem wake-up`, which go through the HTTP API).
3. `jq` available on `PATH` (used by hook scripts to parse the JSON payload Codex pipes on stdin).

## Install

From a Codex CLI session:

```
/plugin marketplace add /path/to/mem
/plugin install mem@mem
```

`/plugin marketplace add` registers this repo as a local marketplace (writes `~/.codex/plugins/known_marketplaces.json`). `/plugin install` caches the plugin to `~/.codex/plugins/cache/mem/mem/0.1.0/` and wires the hooks. Restart your Codex session for the hooks to take effect.

## MCP server registration (manual, one-time)

Codex plugins do not auto-register MCP servers in `~/.codex/config.toml`. Add this section yourself:

```toml
[mcp_servers.mem]
command = "mem"
args = ["mcp"]

[mcp_servers.mem.env]
MEM_BASE_URL = "http://127.0.0.1:3000"
MEM_TENANT = "local"
```

Optional: set `MEM_MCP_EXPOSE_EMBEDDINGS = "1"` to expose admin `embeddings_*` tools.

## Verify

```
# In a fresh Codex session, look for these signals:
# 1. SessionStart wake-up appears as injected context (if any memories exist).
# 2. After ~15 exchanges, ~/.mem/codex_last_save updates.
# 3. Memories show up at http://127.0.0.1:3000/memories?tenant=local.
```

## Layout

```
.codex-plugin/
├── plugin.json        — Codex plugin manifest
├── marketplace.json   — Single-plugin marketplace registry
├── hooks.json         — Hook bindings (SessionStart / Stop / PreCompact)
├── hooks/
│   ├── session_start.sh   — Wake-up injection
│   ├── stop.sh            — Throttled background mine (every 15 exchanges)
│   └── precompact.sh      — Final mine before context compression
└── README.md          — This file
```

## Notes / known caveats

- `mem mine` was written for Claude Code transcripts. Codex JSONL schema may differ; if extraction is empty, file an issue with a sample transcript line.
- Hook payload field names (`transcriptPath` vs `transcript_path`) differ across Codex versions. The scripts try both, then fall back to the latest `.jsonl` under `~/.codex/sessions/`.
- The Claude Code variant of this plugin lives in `.claude-plugin/` at the repo root; the two plugins share `mem mine` / `mem wake-up` infrastructure but ship separate hook scripts so each runtime tags its memories with the correct `--agent` value.
