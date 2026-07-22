import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { McpStdioClient } from "./mcp-client.ts";
import { isServeUp } from "./mem-extension.ts";
import http from "node:http";

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

test("callTool correlates concurrent out-of-order replies by id, not arrival order", async () => {
  const child = fakeChild();
  const client = new McpStdioClient(child);

  // Server: capture every tools/call request's id, then reply to them in
  // REVERSED id order (second request's id first) — each reply carries a
  // distinct payload keyed to its own id. A client that resolves pending
  // promises in FIFO/arrival order (ignoring msg.id) would hand the second
  // request's result to the first caller and vice versa, failing the
  // assertions below.
  const seen: Array<{ id: number; query: string }> = [];
  child.stdin.on("data", (buf: Buffer) => {
    for (const line of buf.toString("utf8").split("\n")) {
      if (!line.trim()) continue;
      const req = JSON.parse(line);
      if (req.method !== "tools/call") {
        child.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: req.id, result: {} }) + "\n");
        continue;
      }
      seen.push({ id: req.id, query: req.params.arguments.query });
      if (seen.length < 2) continue; // wait until both requests have arrived
      // Both requests are in flight now — reply in reversed id order.
      for (const r of [...seen].reverse()) {
        const result = { content: [{ type: "text", text: `result-for-${r.query}` }] };
        child.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: r.id, result }) + "\n");
      }
    }
  });

  // Fire both requests without awaiting the first — two in flight at once.
  const first = client.callTool("capability_capsule_search", { query: "first" });
  const second = client.callTool("capability_capsule_search", { query: "second" });

  const [firstRes, secondRes] = await Promise.all([first, second]);
  assert.equal(firstRes.content[0].text, "result-for-first");
  assert.equal(secondRes.content[0].text, "result-for-second");
});

test("dispose() rejects a pending request with the given error", async () => {
  const child = fakeChild();
  const client = new McpStdioClient(child);

  // Fire a call but never let the fake child reply — the request is left
  // pending exactly like it would be if the real `mem mcp` child died
  // mid-flight (exited/crashed) before answering.
  const pending = client.callTool("capability_capsule_search", { query: "x" });

  const boom = new Error("boom");
  client.dispose(boom);

  await assert.rejects(pending, (e: unknown) => {
    assert.equal(e, boom);
    return true;
  });

  // Closed client rejects any further calls immediately too, rather than
  // hanging forever waiting on a reply that will never come.
  await assert.rejects(client.callTool("capability_capsule_search", { query: "y" }), (e: unknown) => {
    assert.equal(e, boom);
    return true;
  });
});

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
