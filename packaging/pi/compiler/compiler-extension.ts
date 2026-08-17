import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

type Tool = { name: string; description?: string; inputSchema: unknown };
type CallResult = { content?: Array<{ type: string; text?: string }>; isError?: boolean };

const COMPILER_TOOLS = new Set([
  "skill_compiler_preview",
  "skill_compiler_claim",
  "skill_compiler_renew",
  "skill_compiler_publish_proposal",
  "skill_compiler_complete_decision",
  "skill_compiler_fail",
]);
const REQUEST_TIMEOUT_MS = 32_000;

class CompilerMcpClient {
  private nextId = 1;
  private buffer = "";
  private closed = false;
  private closedError: Error | undefined;
  private pending = new Map<number, {
    resolve: (value: unknown) => void;
    reject: (error: Error) => void;
    timer: NodeJS.Timeout;
  }>();

  constructor(private readonly child: ChildProcessWithoutNullStreams) {
    child.stdout.on("data", (chunk: Buffer) => this.onData(chunk.toString("utf8")));
  }

  get isClosed(): boolean {
    return this.closed;
  }

  async initialize(): Promise<void> {
    await this.request("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "pi-mem-skill-compiler", version: "0.1.0" },
    });
    this.child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
  }

  async listTools(): Promise<Tool[]> {
    const response = (await this.request("tools/list", {})) as { tools?: Tool[] };
    return response.tools ?? [];
  }

  async call(name: string, args: unknown): Promise<CallResult> {
    return (await this.request("tools/call", { name, arguments: args })) as CallResult;
  }

  close(error: Error): void {
    if (this.closed) return;
    this.closed = true;
    this.closedError = error;
    for (const { reject, timer } of this.pending.values()) {
      clearTimeout(timer);
      reject(error);
    }
    this.pending.clear();
  }

  private request(method: string, params: unknown): Promise<unknown> {
    if (this.closed) return Promise.reject(this.closedError ?? new Error("compiler MCP closed"));
    const id = this.nextId++;
    const request = JSON.stringify({ jsonrpc: "2.0", id, method, params });
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        const error = new Error(`compiler MCP ${method} timed out`);
        reject(error);
        this.close(error);
        try { this.child.kill("SIGTERM"); } catch { /* already gone */ }
      }, REQUEST_TIMEOUT_MS);
      this.pending.set(id, { resolve, reject, timer });
      this.child.stdin.write(`${request}\n`, (error) => {
        if (error) {
          this.pending.delete(id);
          clearTimeout(timer);
          reject(error);
        }
      });
    });
  }

  private onData(text: string): void {
    this.buffer += text;
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      if (!line) continue;
      let message: { id?: number; result?: unknown; error?: { message?: string } };
      try { message = JSON.parse(line); } catch { continue; }
      if (message.id === undefined) continue;
      const pending = this.pending.get(message.id);
      if (!pending) continue;
      this.pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) pending.reject(new Error(message.error.message ?? "MCP error"));
      else pending.resolve(message.result);
    }
  }
}

let child: ChildProcessWithoutNullStreams | undefined;
let client: CompilerMcpClient | undefined;
let connecting: Promise<CompilerMcpClient> | undefined;
let registered = false;
let latestCtx: ExtensionContext | undefined;

function notify(message: string, level: "info" | "warning" | "error"): void {
  try { latestCtx?.ui.notify(message, level); } catch { /* session is already closing */ }
}

function compilerEnvironment(): NodeJS.ProcessEnv {
  const environment = { ...process.env };
  delete environment.MEM_ADMIN_TOKEN;
  delete environment.MEM_SKILL_REVIEWER_TOKEN;
  delete environment.MEM_SKILL_RUNTIME_TOKEN;
  environment.MEM_AGENT_COMPILER_ID = process.env.MEM_AGENT_COMPILER_ID ?? "pi";
  return environment;
}

async function connect(): Promise<CompilerMcpClient> {
  const spawned = spawn("mem", ["mcp", "--profile", "compiler"], {
    stdio: ["pipe", "pipe", "pipe"],
    env: compilerEnvironment(),
  });
  child = spawned;
  spawned.stderr.resume();

  let connection: CompilerMcpClient | undefined;
  spawned.on("error", (error) => {
    connection?.close(error);
    if (child === spawned) {
      child = undefined;
      client = undefined;
    }
    notify(`mem compiler MCP spawn failed: ${error.message}`, "error");
  });
  connection = new CompilerMcpClient(spawned);
  spawned.on("exit", () => {
    connection?.close(new Error("mem compiler MCP exited"));
    if (child === spawned) {
      child = undefined;
      client = undefined;
    }
  });

  try {
    await connection.initialize();
    const tools = await connection.listTools();
    if (tools.some((tool) => !COMPILER_TOOLS.has(tool.name)) || tools.length !== COMPILER_TOOLS.size) {
      throw new Error("compiler MCP exposed an unexpected tool set");
    }
    if (connection.isClosed) throw new Error("mem compiler MCP exited during initialization");
    return connection;
  } catch (error) {
    connection.close(error instanceof Error ? error : new Error(String(error)));
    try { spawned.kill("SIGTERM"); } catch { /* already gone */ }
    throw error;
  }
}

function ensureClient(): Promise<CompilerMcpClient> {
  if (client && !client.isClosed) return Promise.resolve(client);
  if (!connecting) {
    const inFlight = connect().then((connected) => {
      client = connected;
      return connected;
    });
    connecting = inFlight;
    inFlight.finally(() => {
      if (connecting === inFlight) connecting = undefined;
    }).catch(() => {});
  }
  return connecting;
}

function stop(): void {
  const currentChild = child;
  const currentClient = client;
  child = undefined;
  client = undefined;
  connecting = undefined;
  currentClient?.close(new Error("compiler session stopped"));
  if (currentChild) {
    try { currentChild.kill("SIGTERM"); } catch { /* already gone */ }
  }
}

const compilerExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", async (_event, ctx: ExtensionContext) => {
    latestCtx = ctx;
    stop();
    try {
      const connected = await ensureClient();
      const tools = await connected.listTools();
      if (!registered) {
        for (const tool of tools) {
          pi.registerTool({
            name: tool.name,
            label: tool.name,
            description: tool.description ?? tool.name,
            parameters: tool.inputSchema as never,
            execute: async (_toolCallId, params) => {
              const active = await ensureClient();
              const result = await active.call(tool.name, params);
              if (result.isError) {
                const message = (result.content ?? []).map((item) => item.text ?? "").join("\n");
                throw new Error(message || "mem compiler tool failed");
              }
              return { content: result.content ?? [], details: {} } as never;
            },
          });
        }
        registered = true;
      }
      pi.setActiveTools([...COMPILER_TOOLS]);
      const activeTools = new Set(pi.getActiveTools());
      if (
        activeTools.size !== COMPILER_TOOLS.size
        || [...activeTools].some((name) => !COMPILER_TOOLS.has(name))
      ) {
        throw new Error("compiler Agent tool isolation failed");
      }
      notify("mem compiler profile ready (6 tools, no review authority)", "info");
    } catch (error) {
      notify(error instanceof Error ? error.message : String(error), "error");
      stop();
    }
  });

  pi.on("session_shutdown", async (_event, ctx: ExtensionContext) => {
    latestCtx = ctx;
    stop();
    latestCtx = undefined;
  });
};

export default compilerExtension;
