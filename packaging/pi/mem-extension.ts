import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { spawn } from "node:child_process";
import { McpStdioClient, ReconnectingMcp, type McpCallResult } from "./mcp-client.ts";

export const MEM_BASE_URL = process.env.MEM_BASE_URL ?? "http://127.0.0.1:3000";

let servePid: number | undefined;
// Write-once: only the spawn branch of `ensureServe` may set this to `true`.
// A later session's health-hit reuse branch must never clear it back to
// `false` — that would let a later session's "serve was already up" finding
// clobber the ownership flag set by the session that actually spawned it,
// causing `stopServe` to skip killing a process it owns.
let serveStartedByUs = false;

let mcpChild: ReturnType<typeof spawn> | undefined;
// Reconnecting wrapper over the `mem mcp` child's client. The registered
// tool `execute` closures forward to this at call time, so if the child dies
// mid-session it transparently respawns on the next call instead of failing
// the rest of the session with "mem mcp exited" until a reload.
let mcpConn: ReconnectingMcp | undefined;
// Guards `pi.registerTool` so tools are registered exactly once per process.
// pi re-fires `session_start` on reload/newSession/switchSession; registering
// again would duplicate every tool. The registered `execute` closures forward
// to the module-level `mcpClient` read at call time, so a reload that spawns
// a fresh mcp child (via `startMcpAndRegisterTools`'s `stopMcp()` + respawn)
// is picked up by the already-registered tools without re-registering.
let toolsRegistered = false;

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
    // Reuse branch: serve is already up (possibly spawned by an earlier
    // session in this same pi process). Do NOT touch `serveStartedByUs`
    // here — see the write-once note at its declaration.
    return;
  }
  const child = spawn("mem", ["serve"], { detached: true, stdio: "ignore", env: process.env });
  child.on("error", (e) => console.warn("[mem] mem serve spawn error:", e));
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

// Maps an MCP CallToolResult to pi's AgentToolResult shape: { content, details, terminate? }.
// NOTE: AgentToolResult (from @earendil-works/pi-agent-core, re-exported via
// pi-coding-agent) has no `isError` field — pi tracks error-ness at the agent-loop
// layer based on whether `execute` throws, not on a field inside the returned
// result. So an MCP-reported error (`res.isError`) must become a thrown Error
// for pi to treat the tool call as failed; a normal result maps straight through.
function mapResult(r: McpCallResult): { content: McpCallResult["content"]; details: unknown } {
  return { content: r.content ?? [], details: {} };
}

// Spawns a fresh `mem mcp` child, wires its lifecycle, completes the MCP
// handshake, and returns the ready client. Used both for the first connect
// and — via ReconnectingMcp — to respawn after the child dies mid-session.
// Updates module-level `mcpChild` so `stopMcp` can kill the current child.
async function connectMcp(): Promise<McpStdioClient> {
  const child = spawn("mem", ["mcp"], {
    stdio: ["pipe", "pipe", "ignore"],
    env: { ...process.env, MEM_BASE_URL },
  });
  mcpChild = child;
  // Attach BEFORE anything else can let an async 'error' event (most
  // commonly ENOENT — `mem` not on PATH) fire unhandled, which would
  // otherwise crash the whole pi host process instead of just degrading
  // this extension's tool registration.
  child.on("error", (e) => console.warn("[mem] mem mcp spawn error:", e));
  if (!child.stdin || !child.stdout) throw new Error("mem mcp: no stdio pipes");

  const client = new McpStdioClient({ stdin: child.stdin, stdout: child.stdout });
  // On child exit, dispose THIS client (not a module ref) so a late exit from
  // an old child can't dispose a newer reconnected client. The disposed client
  // is what ReconnectingMcp detects as dead to trigger a respawn.
  child.on("exit", () => client.dispose(new Error("mem mcp exited")));
  await client.initialize();
  return client;
}

async function startMcpAndRegisterTools(pi: ExtensionAPI): Promise<void> {
  // Idempotent teardown FIRST: pi re-fires `session_start` on reload/
  // newSession/switchSession, and without this a re-fire would spawn a new
  // `mem mcp` child while the previous one (and its client) leaked, never
  // killed.
  stopMcp();

  const conn = new ReconnectingMcp(connectMcp);
  mcpConn = conn;
  const tools = await conn.listTools(); // triggers the first connect

  // Register tools only ONCE per process. Each `execute` reads the
  // module-level `mcpConn` at CALL time (not the `conn` captured here), so
  // after a reload rebuilds it above, already-registered tools transparently
  // pick up the fresh connection without re-registering — and mid-session,
  // `mcpConn.call` respawns a dead child on its own.
  if (!toolsRegistered) {
    for (const tool of tools) {
      pi.registerTool({
        name: tool.name,
        label: tool.name,
        description: tool.description ?? tool.name,
        parameters: tool.inputSchema as never,
        execute: async (_toolCallId, params) => {
          if (!mcpConn) throw new Error("mem mcp not connected");
          const res = await mcpConn.call(tool.name, params);
          if (res.isError) {
            const text = (res.content ?? []).map((c) => c.text ?? "").join("\n") || "mem tool error";
            throw new Error(text);
          }
          return mapResult(res) as never;
        },
      });
    }
    toolsRegistered = true;
    console.warn(`[mem] registered ${tools.length} tools via mem mcp`);
  } else {
    console.warn(`[mem] mem mcp reconnected (${tools.length} tools already registered)`);
  }
}

// Kills the mem mcp child (if any) and disposes its client. Idempotent —
// safe to call when nothing is running. Deliberately does NOT reset
// `toolsRegistered`: tools stay registered in pi across a reload: they
// forward to the module-level `mcpConn`, which this function clears and
// `startMcpAndRegisterTools` refreshes on the next call.
function stopMcp(): void {
  if (mcpChild) {
    try { mcpChild.kill("SIGTERM"); } catch { /* gone */ }
    mcpChild = undefined;
  }
  mcpConn?.close(new Error("mem mcp stopped"));
  mcpConn = undefined;
}

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

// Returns the active session's JSONL path, or undefined for an ephemeral
// (--no-session) session — undefined-safe against a missing/throwing
// sessionManager.
function sessionFileOf(ctx: ExtensionContext): string | undefined {
  try {
    return ctx.sessionManager.getSessionFile();
  } catch {
    return undefined;
  }
}

// Mines the current session's transcript into mem (Plan 1's pi-format
// parser tags rows source_agent="pi"). No-op for ephemeral sessions.
async function runMine(pi: ExtensionAPI, ctx: ExtensionContext): Promise<void> {
  const file = sessionFileOf(ctx);
  if (!file) return; // --no-session ephemeral: nothing to mine
  await pi.exec("mem", ["mine", file], { timeout: 60000 });
}

// Scans the current session's transcript and posts applies_here feedback
// for memories referenced in subsequent assistant blocks. No-op for
// ephemeral sessions.
async function runFeedback(pi: ExtensionAPI, ctx: ExtensionContext): Promise<void> {
  const file = sessionFileOf(ctx);
  if (!file) return;
  await pi.exec("mem", ["feedback-from-transcript", file], { timeout: 30000 });
}

// One recall hit as returned in the directives/relevant_facts/reusable_patterns
// arrays of `POST /capability_capsules/search` — see `src/domain/query.rs`
// `DirectiveItem`/`FactItem`/`PatternItem` (all three carry `capability_capsule_id`
// + `text` + `source_summary`; `text` is kept optional here for callers that
// only have a narrower shape).
type RecallHit = { capability_capsule_id: string; source_summary?: string; text?: string };

// Renders the mem "index"-style auto-recall banner. COUPLING: this text is
// round-trip-parsed by `src/cli/feedback.rs::scan_transcript` (via
// `push_codex_banner_ids` / `extract_injected_ids`) to close the feedback
// loop — it MUST contain the marker substring "mem auto-recall" and render
// each hit as a `` `[mem_<id>]` `` bullet token. Mirrors the shape emitted by
// the real Claude Code hook renderer (`cli/hook.rs::format_prompt_recall_styled`,
// RecallStyle::Index: `- {headline}  \`[{id}]\``) so the same parser code path
// that already round-trips the Claude Code banner also round-trips this one.
export function buildRecallBanner(hits: RecallHit[]): string {
  const preamble =
    "🧠 mem auto-recall (index) — hits relevant to this prompt, headlines only. " +
    "To USE one, `capability_capsule_get` its id FIRST for the verbatim content, " +
    "then send feedback for it — silence freezes ranking. Ignore if irrelevant.";
  const lines = hits.map((h) => {
    const headline = (h.source_summary ?? h.text ?? "").split("\n")[0].slice(0, 80);
    return `- ${headline}  \`[${h.capability_capsule_id}]\``;
  });
  return [preamble, ...lines].join("\n");
}

// Searches mem for capsules relevant to the upcoming prompt and renders them
// as a recall banner. Fail-safe: any fetch error, non-ok response, or empty
// hit set resolves to undefined so the caller can silently skip injection.
async function recallForPrompt(prompt: string): Promise<string | undefined> {
  const res = await fetch(`${MEM_BASE_URL}/capability_capsules/search`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      query: prompt,
      intent: "general",
      scope_filters: [],
      token_budget: 1200,
      caller_agent: "pi",
      expand_graph: false,
      tenant: process.env.MEM_TENANT ?? "local",
    }),
    signal: AbortSignal.timeout(5000),
  });
  if (!res.ok) return undefined;
  const body = (await res.json()) as {
    directives?: RecallHit[];
    relevant_facts?: RecallHit[];
    reusable_patterns?: RecallHit[];
  };
  const hits = [
    ...(body.directives ?? []),
    ...(body.relevant_facts ?? []),
    ...(body.reusable_patterns ?? []),
  ];
  if (hits.length === 0) return undefined;
  return buildRecallBanner(hits);
}

const memExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", async (_event, _ctx: ExtensionContext) => {
    try {
      await ensureServe(MEM_BASE_URL);
      await startMcpAndRegisterTools(pi);
      try { await injectWakeUp(pi); } catch (e) { console.warn("[mem] wake-up failed:", e); }
    } catch (e) {
      console.warn("[mem] session_start setup failed:", e);
    }
  });

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

  pi.on("agent_end", async (_event, ctx) => {
    try { await runFeedback(pi, ctx); } catch (e) { console.warn("[mem] feedback failed:", e); }
  });

  pi.on("session_before_compact", async (_event, ctx) => {
    try { await runMine(pi, ctx); } catch (e) { console.warn("[mem] mine (pre-compact) failed:", e); }
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    try { await runMine(pi, ctx); } catch (e) { console.warn("[mem] mine (shutdown) failed:", e); }
    stopMcp();
    stopServe();
  });
};

export default memExtension;
