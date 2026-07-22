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

const memExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", async (_event, _ctx: ExtensionContext) => {
    try {
      await ensureServe(MEM_BASE_URL);
      await startMcpAndRegisterTools(pi);
    } catch (e) {
      console.warn("[mem] session_start setup failed:", e);
    }
  });

  pi.on("session_shutdown", (_event, _ctx: ExtensionContext) => {
    stopMcp();
    stopServe();
  });
};

export default memExtension;
