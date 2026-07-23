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

  private child: { stdin: Writable; stdout: Readable };
  private closed = false;
  private closedErr: Error | undefined;

  constructor(child: { stdin: Writable; stdout: Readable }) {
    this.child = child;
    this.child.stdout.on("data", (chunk: Buffer) => this.onData(chunk));
  }

  /**
   * Reject every pending request with `err`, clear the pending map, and mark
   * the client closed so any subsequent request()/callTool() rejects
   * immediately instead of hanging forever. Call this when the underlying
   * child process exits/errors so in-flight (and future) calls fail fast
   * rather than waiting on a reply that will never arrive.
   */
  dispose(err: Error): void {
    if (this.closed) return;
    this.closed = true;
    this.closedErr = err;
    for (const p of this.pending.values()) p.reject(err);
    this.pending.clear();
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
    if (this.closed) return Promise.reject(this.closedErr ?? new Error("mcp client closed"));
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
      clientInfo: { name: "pi-mem", version: "0.1.0" },
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
