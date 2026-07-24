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

  /** True once the underlying child has exited/errored (via `dispose`). A
   * closed client rejects every further call, so callers use this to decide
   * whether to reconnect rather than keep hitting a dead client. */
  get isClosed(): boolean {
    return this.closed;
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

/**
 * Wraps an McpStdioClient factory with reconnect-on-death. The spawned
 * `mem mcp` child can exit mid-session (crash, host memory pressure, an
 * external kill); once that happens the underlying client is disposed and
 * every further call on it rejects forever. Instead of surfacing "mem mcp
 * exited" for the rest of the session (fixed only by a session reload), this
 * detects the dead client and lazily re-runs the factory on the next call.
 *
 * `connect` is expected to spawn a fresh child, wire its `exit` event to
 * `dispose` the returned client, and complete the MCP `initialize` handshake.
 * Concurrent calls that arrive while a connect is in flight share it (single
 * flight) so a burst of tool calls after a death spawns exactly one child.
 */
export class ReconnectingMcp {
  private client: McpStdioClient | undefined;
  private connecting: Promise<McpStdioClient> | undefined;

  constructor(private readonly connect: () => Promise<McpStdioClient>) {}

  async call(name: string, args: unknown): Promise<McpCallResult> {
    const client = await this.ensure();
    return client.callTool(name, args);
  }

  /** Enumerates tools over a live client, connecting first if needed. Used
   * once at registration time; reconnect semantics apply here too. */
  async listTools(): Promise<McpTool[]> {
    const client = await this.ensure();
    return client.listTools();
  }

  /** Disposes the current client (rejecting its in-flight calls) and drops it.
   * A subsequent call() reconnects. Used on explicit teardown. */
  close(err: Error): void {
    this.client?.dispose(err);
    this.client = undefined;
  }

  /** Returns a live client, (re)connecting if none exists or the current one
   * has been disposed. A single in-flight connect is shared across callers. */
  private ensure(): Promise<McpStdioClient> {
    if (this.client && !this.client.isClosed) return Promise.resolve(this.client);
    if (!this.connecting) {
      const inflight = this.connect().then((c) => {
        this.client = c;
        return c;
      });
      this.connecting = inflight;
      // Clear the single-flight latch on both success and failure so a failed
      // connect doesn't wedge every future call; a rejection propagates to the
      // awaiting caller(s) and the next call retries.
      inflight.finally(() => {
        if (this.connecting === inflight) this.connecting = undefined;
      }).catch(() => {});
    }
    return this.connecting;
  }
}
