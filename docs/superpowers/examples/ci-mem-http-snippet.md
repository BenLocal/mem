# CI: call mem over HTTP (no MCP)

Use the same `MEM_BASE_URL` and `tenant` as interactive agents so CI and Codex share one store.

## Prerequisites

- `mem` reachable from the job (often `localhost` if the job starts it, or an internal URL).
- `curl` and `jq` (optional, for parsing).

## Search before work

```bash
curl -sS -X POST "${MEM_BASE_URL:-http://127.0.0.1:3000}/memories/search" \
  -H 'content-type: application/json' \
  -d "{
    \"query\": \"${MEM_CI_QUERY:-invoice retry debugging}\",
    \"intent\": \"ci\",
    \"scope_filters\": [\"repo:${MEM_REPO_NAME:-unknown}\"],
    \"token_budget\": 300,
    \"caller_agent\": \"ci:${CI_JOB_ID:-local}\",
    \"expand_graph\": false,
    \"tenant\": \"${MEM_TENANT:-local}\"
  }"
```

## Record a successful episode (optional)

```bash
curl -sS -X POST "${MEM_BASE_URL:-http://127.0.0.1:3000}/episodes" \
  -H 'content-type: application/json' \
  -d "{
    \"tenant\": \"${MEM_TENANT:-local}\",
    \"goal\": \"${MEM_EPISODE_GOAL:-ci green}\",
    \"steps\": [\"lint\", \"test\", \"build\"],
    \"outcome\": \"success\",
    \"repo\": \"${MEM_REPO_NAME:-}\",
    \"source_agent\": \"ci:${CI_JOB_ID:-local}\"
  }"
```

Adjust fields to match `IngestEpisodeRequest` in the mem codebase.
