# mem pi Extension Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pi (`@earendil-works/pi-coding-agent`) extension that (1) manages `mem serve` + `mem mcp` process lifecycle, (2) exposes all ~40 mem tools in pi by proxying to a `mem mcp` subprocess, and (3) replaces mem's Claude-Code hooks with pi events (wake-up, auto-recall, feedback, mining).

**Architecture:** A single ExtensionFactory (`mem-extension.ts`) wires pi events. Tools come from a minimal MCP-stdio client (`mcp-client.ts`) that talks to a spawned `mem mcp` child: `tools/list` at startup drives one `registerTool()` per tool, and each tool's `execute` forwards a `tools/call`. `mem serve` is a shared HTTP daemon (started only if the port is down; killed only if we started it); `mem mcp` is a per-session child (killed unconditionally on shutdown). Wake-up, auto-recall, feedback, and mining call the mem CLI / HTTP.

**Tech Stack:** TypeScript (run by pi via `node --experimental-strip-types`), Node built-ins (`node:child_process`, `node:http`/global `fetch`), `node:test` for unit tests. pi ExtensionAPI v0.74.x.

This is **Plan 2 of 2** (spec: `docs/superpowers/specs/2026-07-22-mem-pi-extension-design.md`). It depends on **Plan 1** (pi transcript parser) being live, so `mem mine` / `mem feedback-from-transcript` can parse the pi session files this extension points them at.

## Global Constraints

- pi ExtensionAPI facts (verified in `…/pi-coding-agent/dist/core/extensions/types.d.ts`, v0.74.2):
  - Entry: `export type ExtensionFactory = (pi: ExtensionAPI) => void | Promise<void>` — the extension's **default export is a function taking `pi`**.
  - `pi.on(event, handler)` where `handler: (event, ctx: ExtensionContext) => …`. Events used: `session_start`, `session_shutdown`, `session_before_compact`, `agent_end`, `before_agent_start`.
  - `pi.registerTool({ name, label, description, parameters, execute })`; `execute(toolCallId, params, signal, onUpdate, ctx) => Promise<AgentToolResult>`. `parameters` is a TypeBox `TSchema` (a JSON-Schema superset).
  - `pi.exec(command, args, options?) => Promise<ExecResult>` where `ExecResult = { stdout, stderr, code, killed }`. **`exec` blocks to completion — never use it for a daemon.**
  - `pi.sendMessage(msg, { triggerTurn, deliverAs })` — inject without forcing a turn (use `triggerTurn:false, deliverAs:"nextTurn"` for wake-up). `pi.sendUserMessage(content)` **always triggers a turn** — do NOT use it for session_start injection.
  - `before_agent_start` handler may return `BeforeAgentStartEventResult = { message?: Pick<CustomMessage,"customType"|"content"|"display"|"details">, systemPrompt?: string }` — the auto-recall injection channel.
  - `ctx.sessionManager.getSessionFile(): string` — current session JSONL path (may be undefined for `--no-session`). Also `getCwd()`, `getSessionId()`.
- **All handlers are fail-safe**: wrap every mem call in try/catch; a mem failure must `console.warn` and return, never throw out of a pi handler (would disrupt the user's session).
- Files live in `packaging/pi/` (mirrors `packaging/npm/`). No build step — pi strips types at load.
- **Test runner (environment fact, discovered during execution):** the default `node` on PATH is v20.19.2, which lacks `--experimental-strip-types` (needs Node ≥22.6). Run all `.ts` unit tests with the Node 24 binary explicitly: `/root/.nvm/versions/node/v24.16.0/bin/node --test packaging/pi/<file>.test.ts` (Node 24 strips TS types natively). pi itself still runs on v20.19.2 and loads the extension via its own internal type-stripping — that path is unaffected; only standalone `node --test` needs Node 24.
- No real client/company names anywhere.
- **`mem serve` singleton rule** (CLAUDE.md): one `mem serve` per Lance dir. This extension enforces "start only if `/health` is down".

## File Structure

- `packaging/pi/package.json` — pi package manifest (`pi.extensions`, keyword `pi-package`).
- `packaging/pi/mcp-client.ts` — minimal MCP-stdio JSON-RPC client (`McpStdioClient`). One responsibility: frame/correlate JSON-RPC over a child's stdio. Pure enough to unit-test with a fake child.
- `packaging/pi/mem-extension.ts` — the ExtensionFactory: process lifecycle, tool registration, event wiring. Imports `McpStdioClient`.
- `packaging/pi/mcp-client.test.ts` — `node:test` unit tests for the client + banner builder.
- `packaging/pi/README.md` — install/usage.

---

### Task 1: pi package skeleton + loadable extension

**Files:**
- Create: `packaging/pi/package.json`, `packaging/pi/mem-extension.ts`, `packaging/pi/README.md`.

**Interfaces:**
- Produces: a default-exported `ExtensionFactory` that pi can load; a `MEM_BASE_URL` constant (`process.env.MEM_BASE_URL ?? "http://127.0.0.1:3000"`) reused by later tasks.

- [ ] **Step 1: Write `package.json`**

`packaging/pi/package.json`:

```json
{
  "name": "@shibenenen/mem-pi",
  "version": "0.1.0",
  "description": "mem memory service extension for the pi coding agent",
  "keywords": ["pi-package"],
  "pi": {
    "extensions": ["./mem-extension.ts"]
  },
  "license": "MIT"
}
```

- [ ] **Step 2: Write the skeleton extension**

`packaging/pi/mem-extension.ts`:

```ts
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

export const MEM_BASE_URL = process.env.MEM_BASE_URL ?? "http://127.0.0.1:3000";

const memExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", (_event, _ctx: ExtensionContext) => {
    console.warn("[mem] extension loaded");
  });
};

export default memExtension;
```

- [ ] **Step 3: Smoke-test that pi loads it**

Run (non-interactive, prints and exits):

```bash
pi -e ./packaging/pi/mem-extension.ts -p "say hi" 2>&1 | grep -F "[mem] extension loaded"
```

Expected: the `[mem] extension loaded` line appears (proves the factory ran on session_start).
If `pi -e` is not the correct load flag on this pi version, confirm via `pi --help` (look for the extension/`-e` load flag) and adjust; `pi install ./packaging/pi` then a normal `pi -p` run is the fallback.

- [ ] **Step 4: Write README**

`packaging/pi/README.md` — a short install doc:

```markdown
# mem × pi

Install: `pi install ./packaging/pi` (or `pi install <git-source>`).

Requires the `mem` binary on `PATH`. The extension starts `mem serve` if
`MEM_BASE_URL` (default http://127.0.0.1:3000) is down, exposes all mem tools,
injects wake-up + recall context, and mines/gives feedback from pi sessions.

Env: `MEM_BASE_URL`, `MEM_TENANT` (default `local`).
```

- [ ] **Step 5: Commit**

```bash
git add packaging/pi/
git commit -m "feat(pi): loadable mem extension skeleton"
```

---

### Task 2: Minimal MCP-stdio client

**Files:**
- Create: `packaging/pi/mcp-client.ts`, `packaging/pi/mcp-client.test.ts`.

**Interfaces:**
- Produces:
  - `interface McpTool { name: string; description?: string; inputSchema: unknown }`
  - `class McpStdioClient { constructor(child: { stdin: Writable; stdout: Readable }); initialize(): Promise<void>; listTools(): Promise<McpTool[]>; callTool(name: string, args: unknown): Promise<McpCallResult>; }`
  - `interface McpCallResult { content: Array<{ type: string; text?: string }>; isError?: boolean }`
- Consumes: nothing (Node built-ins only).

> **Confirm at impl:** rmcp `transport-io` framing. MCP's stdio transport spec is **newline-delimited JSON** (one JSON-RPC object per line, no embedded newlines). This plan implements newline framing. Before Step 3, run `mem mcp` manually and pipe one `{"jsonrpc":"2.0","id":1,"method":"initialize",...}` line to confirm the reply is a single newline-terminated JSON line (not `Content-Length:`-framed). If it is Content-Length-framed, swap the `splitFrames` logic for header parsing — the rest of the client is unchanged.

- [ ] **Step 1: Write the failing test**

`packaging/pi/mcp-client.test.ts` (uses a fake child = two `PassThrough` streams; the test plays the "server" by reading requests off `toChild` and writing framed replies to `fromChild`):

```ts
import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { McpStdioClient } from "./mcp-client.ts";

function fakeChild() {
  const stdin = new PassThrough(); // extension writes here (requests)
  const stdout = new PassThrough(); // extension reads here (replies)
  return { stdin, stdout };
}

test("listTools returns the tools from a tools/list reply", async () => {
  const child = fakeChild();
  const client = new McpStdioClient(child);

  // Server: read each request line, reply by id.
  child.stdin.on("data", (buf: Buffer) => {
    for (const line of buf.toString("utf8").split("\n")) {
      if (!line.trim()) continue;
      const req = JSON.parse(line);
      let result: unknown;
      if (req.method === "initialize") result = { protocolVersion: "2024-11-05", capabilities: {} };
      else if (req.method === "tools/list") result = { tools: [{ name: "capability_capsule_search", description: "search", inputSchema: { type: "object" } }] };
      else result = {};
      child.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: req.id, result }) + "\n");
    }
  });

  await client.initialize();
  const tools = await client.listTools();
  assert.equal(tools.length, 1);
  assert.equal(tools[0].name, "capability_capsule_search");
});

test("callTool correlates the reply by id", async () => {
  const child = fakeChild();
  const client = new McpStdioClient(child);
  child.stdin.on("data", (buf: Buffer) => {
    for (const line of buf.toString("utf8").split("\n")) {
      if (!line.trim()) continue;
      const req = JSON.parse(line);
      const result = req.method === "tools/call"
        ? { content: [{ type: "text", text: "ok" }] }
        : {};
      child.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: req.id, result }) + "\n");
    }
  });
  const res = await client.callTool("capability_capsule_search", { query: "x" });
  assert.equal(res.content[0].text, "ok");
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `node --experimental-strip-types --test packaging/pi/mcp-client.test.ts`
Expected: FAIL — `Cannot find module './mcp-client.ts'` / `McpStdioClient is not a constructor`.

- [ ] **Step 3: Write the client**

`packaging/pi/mcp-client.ts`:

```ts
import type { Readable, Writable } from "node:stream";

export interface McpTool {
  name: string;
  description?: string;
  inputSchema: unknown;
}

export interface McpCallResult {
  content: Array<{ type: string; text?: string }>;
  isError?: boolean;
}

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
}

/** Minimal MCP JSON-RPC client over a child's stdio (newline-delimited). */
export class McpStdioClient {
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private buf = "";

  constructor(private child: { stdin: Writable; stdout: Readable }) {
    this.child.stdout.on("data", (chunk: Buffer) => this.onData(chunk));
  }

  private onData(chunk: Buffer): void {
    this.buf += chunk.toString("utf8");
    let nl: number;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (!line) continue;
      let msg: { id?: number; result?: unknown; error?: { message?: string } };
      try {
        msg = JSON.parse(line);
      } catch {
        continue; // ignore non-JSON (e.g. stray log lines)
      }
      if (typeof msg.id !== "number") continue; // notification — ignore
      const p = this.pending.get(msg.id);
      if (!p) continue;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new Error(msg.error.message ?? "mcp error"));
      else p.resolve(msg.result);
    }
  }

  private request(method: string, params?: unknown): Promise<unknown> {
    const id = this.nextId++;
    const line = JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child.stdin.write(line, (err) => {
        if (err) {
          this.pending.delete(id);
          reject(err);
        }
      });
    });
  }

  async initialize(): Promise<void> {
    await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "mem-pi", version: "0.1.0" },
    });
  }

  async listTools(): Promise<McpTool[]> {
    const res = (await this.request("tools/list")) as { tools?: McpTool[] };
    return res.tools ?? [];
  }

  async callTool(name: string, args: unknown): Promise<McpCallResult> {
    const res = (await this.request("tools/call", { name, arguments: args })) as McpCallResult;
    return res;
  }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --experimental-strip-types --test packaging/pi/mcp-client.test.ts`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add packaging/pi/mcp-client.ts packaging/pi/mcp-client.test.ts
git commit -m "feat(pi): minimal MCP stdio client for tool proxying"
```

---

### Task 3: `mem serve` lifecycle (health-gated start, ownership-tracked stop)

**Files:**
- Modify: `packaging/pi/mem-extension.ts`.
- Modify: `packaging/pi/mcp-client.test.ts` (add health-check unit test) OR add `packaging/pi/lifecycle.test.ts`.

**Interfaces:**
- Produces (exported from `mem-extension.ts` for testing): `async function isServeUp(baseUrl: string): Promise<boolean>`.
- Internal module state: `let servePid: number | undefined; let serveStartedByUs = false;`.

- [ ] **Step 1: Write the failing test**

Add to `packaging/pi/mcp-client.test.ts`:

```ts
import { isServeUp } from "./mem-extension.ts";
import http from "node:http";

test("isServeUp is true when /health returns 200", async () => {
  const server = http.createServer((req, res) => {
    if (req.url === "/health") { res.writeHead(200); res.end("ok"); }
    else { res.writeHead(404); res.end(); }
  });
  await new Promise<void>((r) => server.listen(0, r));
  const port = (server.address() as import("node:net").AddressInfo).port;
  try {
    assert.equal(await isServeUp(`http://127.0.0.1:${port}`), true);
    assert.equal(await isServeUp(`http://127.0.0.1:1`), false); // nothing listening
  } finally {
    server.close();
  }
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `node --experimental-strip-types --test packaging/pi/mcp-client.test.ts`
Expected: FAIL — `isServeUp` is not exported.

- [ ] **Step 3: Implement lifecycle in `mem-extension.ts`**

Add imports and the serve helpers; wire into `session_start`/`session_shutdown`:

```ts
import { spawn } from "node:child_process";

let servePid: number | undefined;
let serveStartedByUs = false;

export async function isServeUp(baseUrl: string): Promise<boolean> {
  try {
    const res = await fetch(`${baseUrl}/health`, { signal: AbortSignal.timeout(1000) });
    return res.ok;
  } catch {
    return false;
  }
}

async function ensureServe(baseUrl: string): Promise<void> {
  if (await isServeUp(baseUrl)) {
    serveStartedByUs = false;
    return;
  }
  const child = spawn("mem", ["serve"], { detached: true, stdio: "ignore", env: process.env });
  child.unref();
  servePid = child.pid;
  serveStartedByUs = true;
  // Poll /health up to ~10s.
  for (let i = 0; i < 50; i++) {
    if (await isServeUp(baseUrl)) return;
    await new Promise((r) => setTimeout(r, 200));
  }
  console.warn("[mem] serve did not become healthy within 10s");
}

function stopServe(): void {
  if (serveStartedByUs && servePid !== undefined) {
    try { process.kill(servePid, "SIGTERM"); } catch { /* already gone */ }
  }
}
```

Replace the skeleton `session_start` handler and add `session_shutdown`:

```ts
const memExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", async (_event, _ctx) => {
    try {
      await ensureServe(MEM_BASE_URL);
    } catch (e) {
      console.warn("[mem] session_start lifecycle failed:", e);
    }
  });

  pi.on("session_shutdown", (_event, _ctx) => {
    stopServe();
  });
};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --experimental-strip-types --test packaging/pi/mcp-client.test.ts`
Expected: PASS (health test + prior client tests).

- [ ] **Step 5: Commit**

```bash
git add packaging/pi/mem-extension.ts packaging/pi/mcp-client.test.ts
git commit -m "feat(pi): health-gated mem serve lifecycle with ownership tracking"
```

---

### Task 4: `mem mcp` subprocess + register all tools

**Files:**
- Modify: `packaging/pi/mem-extension.ts`.

**Interfaces:**
- Consumes: `McpStdioClient`, `McpCallResult` from `./mcp-client.ts`.
- Internal state: `let mcpChild: ReturnType<typeof spawn> | undefined; let mcpClient: McpStdioClient | undefined;`.

> **Confirm at impl:** (a) `AgentToolResult` exact shape — the type is imported from `@earendil-works/pi-agent-core` (not readable as installed source here). Read its definition via the pi package types, or infer from a working built-in tool. It is content-block shaped; the mapping below (`{ content: [...], isError }`) is the expected form — adjust field names if the real type differs. (b) whether `pi.registerTool`'s `parameters` accepts a raw JSON Schema object directly; TypeBox `TSchema` is structurally a JSON Schema, so passing `inputSchema` as-is should typecheck at runtime (types stripped). If pi validates the schema shape strictly, wrap with `{ ...inputSchema }` or `Type.Unsafe(inputSchema)`.

- [ ] **Step 1: Implement mcp spawn + registration (called from `session_start` after `ensureServe`)**

Add to `mem-extension.ts`:

```ts
import { McpStdioClient, type McpCallResult } from "./mcp-client.ts";

let mcpChild: ReturnType<typeof spawn> | undefined;
let mcpClient: McpStdioClient | undefined;

function mapResult(r: McpCallResult) {
  // Map an MCP CallToolResult to pi's AgentToolResult. Both are
  // content-block shaped; pass content through and carry the error flag.
  return { content: r.content ?? [], isError: r.isError ?? false };
}

async function startMcpAndRegisterTools(pi: ExtensionAPI): Promise<void> {
  const child = spawn("mem", ["mcp"], {
    stdio: ["pipe", "pipe", "ignore"],
    env: { ...process.env, MEM_BASE_URL },
  });
  mcpChild = child;
  if (!child.stdin || !child.stdout) throw new Error("mem mcp: no stdio pipes");

  const client = new McpStdioClient({ stdin: child.stdin, stdout: child.stdout });
  mcpClient = client;
  await client.initialize();
  const tools = await client.listTools();

  for (const tool of tools) {
    pi.registerTool({
      name: tool.name,
      label: tool.name,
      description: tool.description ?? tool.name,
      parameters: tool.inputSchema as never,
      execute: async (_toolCallId, params) => {
        try {
          const res = await client.callTool(tool.name, params);
          return mapResult(res) as never;
        } catch (e) {
          return {
            content: [{ type: "text", text: `mem tool error: ${String(e)}` }],
            isError: true,
          } as never;
        }
      },
    });
  }
  console.warn(`[mem] registered ${tools.length} tools via mem mcp`);
}

function stopMcp(): void {
  if (mcpChild) {
    try { mcpChild.kill("SIGTERM"); } catch { /* gone */ }
    mcpChild = undefined;
    mcpClient = undefined;
  }
}
```

Wire into the handlers (extend `session_start`, extend `session_shutdown`):

```ts
  pi.on("session_start", async (_event, _ctx) => {
    try {
      await ensureServe(MEM_BASE_URL);
      await startMcpAndRegisterTools(pi);
    } catch (e) {
      console.warn("[mem] session_start setup failed:", e);
    }
  });

  pi.on("session_shutdown", (_event, _ctx) => {
    stopMcp();
    stopServe();
  });
```

- [ ] **Step 2: Manual integration smoke test**

With a real `mem` binary on PATH:

```bash
pi -e ./packaging/pi/mem-extension.ts -p "list your available tools" 2>&1 | grep -F "registered"
```

Expected: `[mem] registered N tools via mem mcp` with N ≈ 40. If tool schemas are rejected by pi, apply the §Confirm-at-impl (b) wrapping and retry.

- [ ] **Step 3: Verify a tool actually round-trips**

```bash
pi -e ./packaging/pi/mem-extension.ts -p "use the capability_capsule_search tool to search for 'test' and show the raw result"
```

Expected: the agent invokes `capability_capsule_search`, the extension forwards to `mem mcp` → `mem serve`, and a result comes back (empty results are fine — the point is no transport error).

- [ ] **Step 4: Commit**

```bash
git add packaging/pi/mem-extension.ts
git commit -m "feat(pi): proxy all mem tools via mem mcp subprocess"
```

---

### Task 5: Wake-up injection on session_start

**Files:**
- Modify: `packaging/pi/mem-extension.ts`.

**Interfaces:**
- Consumes: `pi.exec`, `pi.sendMessage`.

- [ ] **Step 1: Implement wake-up (append after tool registration in `session_start`)**

Add a helper and call it at the end of the `session_start` try-block:

```ts
async function injectWakeUp(pi: ExtensionAPI): Promise<void> {
  const res = await pi.exec("mem", ["wake-up"], { timeout: 15000 });
  const text = res.stdout.trim();
  if (!text) return;
  // Inject as context WITHOUT forcing a turn (sendUserMessage would trigger
  // one immediately, before the user has typed).
  pi.sendMessage(
    { customType: "mem-wakeup", content: text, display: true },
    { triggerTurn: false, deliverAs: "nextTurn" },
  );
}
```

In `session_start`, after `startMcpAndRegisterTools(pi)`:

```ts
      await startMcpAndRegisterTools(pi);
      try { await injectWakeUp(pi); } catch (e) { console.warn("[mem] wake-up failed:", e); }
```

> **Confirm at impl:** the exact `sendMessage` payload shape pi persists/displays. `CustomMessage` fields are `customType | content | display | details`. If `content` must be structured (not a bare string), wrap as the pi `CustomMessage.content` type expects (read `CustomMessage` in types). The goal: wake-up text is visible next turn and lands in the session file so `feedback-from-transcript` can later see it.

- [ ] **Step 2: Manual smoke test**

```bash
pi -e ./packaging/pi/mem-extension.ts -p "what did we work on recently?" 2>&1 | head -40
```

Expected: `mem wake-up` output is present in the session context (recent memories / diary). No crash if `mem wake-up` returns empty.

- [ ] **Step 3: Commit**

```bash
git add packaging/pi/mem-extension.ts
git commit -m "feat(pi): inject mem wake-up context on session start"
```

---

### Task 6: Mining + feedback events

**Files:**
- Modify: `packaging/pi/mem-extension.ts`.

**Interfaces:**
- Consumes: `ctx.sessionManager.getSessionFile()`, `pi.exec`. Relies on **Plan 1** (pi transcript parsing in `mem mine` / `mem feedback-from-transcript`).

- [ ] **Step 1: Implement mine + feedback helpers**

```ts
function sessionFileOf(ctx: ExtensionContext): string | undefined {
  try { return ctx.sessionManager.getSessionFile(); } catch { return undefined; }
}

async function runMine(pi: ExtensionAPI, ctx: ExtensionContext): Promise<void> {
  const file = sessionFileOf(ctx);
  if (!file) return; // --no-session ephemeral: nothing to mine
  await pi.exec("mem", ["mine", file], { timeout: 60000 });
}

async function runFeedback(pi: ExtensionAPI, ctx: ExtensionContext): Promise<void> {
  const file = sessionFileOf(ctx);
  if (!file) return;
  await pi.exec("mem", ["feedback-from-transcript", file], { timeout: 30000 });
}
```

- [ ] **Step 2: Wire the events**

```ts
  pi.on("agent_end", async (_event, ctx) => {
    try { await runFeedback(pi, ctx); } catch (e) { console.warn("[mem] feedback failed:", e); }
  });

  pi.on("session_before_compact", async (_event, ctx) => {
    try { await runMine(pi, ctx); } catch (e) { console.warn("[mem] mine (pre-compact) failed:", e); }
  });
```

Extend `session_shutdown` to mine before stopping the processes (it now needs `ctx`):

```ts
  pi.on("session_shutdown", async (_event, ctx) => {
    try { await runMine(pi, ctx); } catch (e) { console.warn("[mem] mine (shutdown) failed:", e); }
    stopMcp();
    stopServe();
  });
```

> **Confirm at impl:** whether `session_shutdown` awaits async handlers before the runtime tears down (some hosts fire shutdown fire-and-forget). If pi does not await it, move the shutdown mine to `agent_end`-style timing or accept that the final partial turn is mined next session. `session_before_compact` reliably fires before context loss, which is the important one.

- [ ] **Step 3: Manual smoke test**

Run a session that emits a `<mem-save>` tag, exit, then check the row landed:

```bash
pi -e ./packaging/pi/mem-extension.ts -p "note this: <mem-save>pi extension mines on shutdown</mem-save>"
# after exit:
curl -s -X POST http://127.0.0.1:3000/capability_capsules/search -H 'content-type: application/json' \
  -d '{"query":"pi extension mines on shutdown","tenant":"local"}' | grep -F "mines on shutdown"
```

Expected: the mined memory is retrievable (proves `mem mine` parsed the pi session file — Plan 1 working end-to-end through the extension). Its `source_agent` should be `pi`.

- [ ] **Step 4: Commit**

```bash
git add packaging/pi/mem-extension.ts
git commit -m "feat(pi): mine on compact/shutdown, feedback on agent_end"
```

---

### Task 7: `before_agent_start` auto-recall banner

**Files:**
- Modify: `packaging/pi/mem-extension.ts`, `packaging/pi/mcp-client.test.ts` (banner-builder unit test).

**Interfaces:**
- Produces: `function buildRecallBanner(hits: Array<{ capability_capsule_id: string; source_summary?: string; text?: string }>): string` — renders the mem `index`-style recall banner (must match what `cli/feedback.rs::scan_transcript` parses: contains the marker `mem auto-recall` and one `[mem_…]` id per line).
- `before_agent_start` handler returns `{ message: { customType, content, display } }` when there are hits.

> **Coupling (memory `feedback-loop-transcript-format-dep`):** the banner text is round-trip-parsed by `scan_transcript`. It MUST contain a recall marker string (`mem auto-recall`) and render each hit's id as `[mem_<id>]`. Verify the exact marker + id token against `push_codex_banner_ids` / `extract_injected_ids` in `src/cli/feedback.rs` before finalizing the format.

- [ ] **Step 1: Write the failing test**

Add to `packaging/pi/mcp-client.test.ts`:

```ts
import { buildRecallBanner } from "./mem-extension.ts";

test("recall banner carries the marker and [mem_id] tokens", () => {
  const banner = buildRecallBanner([
    { capability_capsule_id: "mem_abc", source_summary: "pi stores sessions as jsonl" },
  ]);
  assert.match(banner, /mem auto-recall/);
  assert.match(banner, /\[mem_abc\]/);
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `node --experimental-strip-types --test packaging/pi/mcp-client.test.ts`
Expected: FAIL — `buildRecallBanner` is not exported.

- [ ] **Step 3: Implement banner + handler**

```ts
export function buildRecallBanner(
  hits: Array<{ capability_capsule_id: string; source_summary?: string; text?: string }>,
): string {
  const lines = hits.map((h) => {
    const summary = (h.source_summary ?? h.text ?? "").split("\n")[0].slice(0, 80);
    return `[${h.capability_capsule_id}] ${summary}`;
  });
  return `<mem auto-recall>\n${lines.join("\n")}\n</mem auto-recall>`;
}

async function recallForPrompt(prompt: string): Promise<string | undefined> {
  const res = await fetch(`${MEM_BASE_URL}/capability_capsules/search`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ query: prompt, tenant: process.env.MEM_TENANT ?? "local" }),
    signal: AbortSignal.timeout(5000),
  });
  if (!res.ok) return undefined;
  const body = (await res.json()) as {
    directives?: unknown[]; relevant_facts?: unknown[]; reusable_patterns?: unknown[];
  };
  const hits = [
    ...(body.directives ?? []),
    ...(body.relevant_facts ?? []),
    ...(body.reusable_patterns ?? []),
  ] as Array<{ capability_capsule_id: string; source_summary?: string; text?: string }>;
  if (hits.length === 0) return undefined;
  return buildRecallBanner(hits);
}
```

Wire the handler (returns a `BeforeAgentStartEventResult`):

```ts
  pi.on("before_agent_start", async (event, _ctx) => {
    try {
      const banner = await recallForPrompt(event.prompt);
      if (!banner) return;
      return { message: { customType: "mem-recall", content: banner, display: true } };
    } catch (e) {
      console.warn("[mem] auto-recall failed:", e);
      return;
    }
  });
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --experimental-strip-types --test packaging/pi/mcp-client.test.ts`
Expected: PASS (banner test + all prior).

- [ ] **Step 5: Verify the round-trip end-to-end**

```bash
# 1. ensure a searchable memory exists (from Task 6's smoke test).
# 2. run a prompt that should recall it, then exit:
pi -e ./packaging/pi/mem-extension.ts -p "how does the pi extension handle shutdown?"
# 3. feedback ran on agent_end; confirm the recalled capsule got a feedback event
#    (its confidence/decay moved). Inspect via the admin/search path or /metrics feedback_* counters.
curl -s http://127.0.0.1:3000/metrics | grep -F feedback_
```

Expected: `feedback_*` counters are non-zero — proving the banner was injected, landed in the pi session file, and `feedback-from-transcript` (Plan 1's pi branch) credited the recalled capsule. This closes the loop.

- [ ] **Step 6: Commit**

```bash
git add packaging/pi/mem-extension.ts packaging/pi/mcp-client.test.ts
git commit -m "feat(pi): before_agent_start auto-recall banner injection"
```

---

## Self-Review

**Spec coverage:**
- §4.1 serve lifecycle (start-if-down / kill-if-we-started) → Task 3. ✅
- §4.1 mem mcp child (per-session, unconditional kill) → Task 4 + Task 6 shutdown wiring. ✅
- §4.3 ~40 tools via MCP subprocess proxy (runtime tools/list) → Task 4. ✅
- §4.2 wake-up injection (sendMessage, not sendUserMessage) → Task 5. ✅
- §4.4 agent_end→feedback, before_compact+shutdown→mine, getSessionFile → Task 6. ✅
- §4.5 before_agent_start auto-recall banner (round-trip coupling) → Task 7. ✅
- §5 packaging (`packaging/pi/`, `pi.extensions`, keyword `pi-package`, `pi install`) → Task 1. ✅
- §6 fail-safe error handling → every handler try/catch across Tasks 3–7. ✅
- §7 tests: MCP client framing/correlation (Task 2), health check (Task 3), banner (Task 7). ✅

**Placeholder scan:** No TBD/TODO. The four "Confirm at impl" callouts are concrete verification steps (a named symbol to read + a stated fallback), not vague requirements — each is bounded and has a default the code already implements.

**Type consistency:** `MEM_BASE_URL`, `isServeUp`, `servePid`/`serveStartedByUs`, `mcpChild`/`mcpClient`, `McpStdioClient`/`McpCallResult`, `buildRecallBanner`, `sessionFileOf` are defined once and referenced consistently. `session_start` handler is progressively extended (Tasks 3→4→5) — final form: `ensureServe` → `startMcpAndRegisterTools` → `injectWakeUp`. `session_shutdown` final form (Task 6): `runMine` → `stopMcp` → `stopServe`. Each task shows the full handler body at the point it changes it, so out-of-order readers see the current shape.

---

## Execution Handoff

After all seven tasks land: run the full `node --experimental-strip-types --test packaging/pi/*.test.ts`, then a real `pi install ./packaging/pi` + interactive session as the acceptance test (tools present, wake-up shown, memory mined on exit, feedback counters move). Plan 1 must be merged first (feedback/mine pi-format parsing).
