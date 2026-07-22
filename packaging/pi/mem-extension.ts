import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { spawn } from "node:child_process";
import { McpStdioClient, type McpCallResult } from "./mcp-client.ts";

export const MEM_BASE_URL = process.env.MEM_BASE_URL ?? "http://127.0.0.1:3000";

let servePid: number | undefined;
let serveStartedByUs = false;

let mcpChild: ReturnType<typeof spawn> | undefined;
let mcpClient: McpStdioClient | undefined;

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

async function startMcpAndRegisterTools(pi: ExtensionAPI): Promise<void> {
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
  child.on("exit", () => mcpClient?.dispose(new Error("mem mcp exited")));
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
        const res = await client.callTool(tool.name, params);
        if (res.isError) {
          const text = (res.content ?? []).map((c) => c.text ?? "").join("\n") || "mem tool error";
          throw new Error(text);
        }
        return mapResult(res) as never;
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
