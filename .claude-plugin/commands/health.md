---
description: Verify mem serve is reachable on MEM_BASE_URL and report basic counts.
allowed-tools: Bash
---

Check that the local `mem serve` process is reachable, and report a quick liveness summary.

Procedure:

1. Resolve `MEM_BASE_URL` (default `http://127.0.0.1:3000`) and `MEM_TENANT` (default `local`).
2. Hit a cheap endpoint to verify the server is up:

   ```bash
   curl -sS -o /dev/null -w "HTTP %{http_code}\n" "$MEM_BASE_URL/memories/search" \
     -H 'content-type: application/json' \
     -d '{"tenant":"'"${MEM_TENANT:-local}"'","query":"ping","limit":1}'
   ```

3. If the call returns 200, also call the MCP `mcp__mem__mem_health` tool for a richer status (provider, sidecar state).
4. If it fails to connect, tell the user `mem serve` isn't running and the canonical way to start it is `cargo run -- serve` from the mem repo.

Report a one-line status (`✦ mem · up | tenant=local | …`) plus any anomalies.
