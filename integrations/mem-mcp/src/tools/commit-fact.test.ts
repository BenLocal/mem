import { describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { registerMemoryCommitFact } from "./commit-fact.js";

describe("memory_commit_fact", () => {
  it("maps fact payload to an auto ingest request", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 201,
      text: async () => JSON.stringify({ memory_id: "mem_123", status: "active" }),
    }));

    const registered: Array<
      [
        string,
        { inputSchema?: { parse: (input: unknown) => unknown } },
        (args: Record<string, unknown>) => Promise<{
          content: Array<{ type: "text"; text: string }>;
        }>,
      ]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
        handler: (args: Record<string, unknown>) => Promise<{
          content: Array<{ type: "text"; text: string }>;
        }>,
      ) => {
        registered.push([name, schema, handler]);
      },
    } as unknown as McpServer;

    registerMemoryCommitFact(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        fetchFn: fetchFn as unknown as typeof fetch,
      },
    );

    const tool = registered.find(([name]) => name === "memory_commit_fact");
    expect(tool).toBeDefined();

    const parsed = tool?.[1].inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "codex-cli",
      source_agent: "mem-mcp",
      summary: "POST /memories keeps project facts active",
      content: "Verified facts use the existing /memories endpoint.",
      evidence: ["tests/ingest_api.rs"],
    }) as Record<string, unknown>;

    const result = await tool?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/memories");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      tenant: "local",
      memory_type: "implementation",
      content:
        "POST /memories keeps project facts active\n\nVerified facts use the existing /memories endpoint.",
      evidence: ["tests/ingest_api.rs"],
      scope: "project",
      visibility: "private",
      project: "mem",
      source_agent: "mem-mcp",
      tags: ["caller_agent:codex-cli"],
      write_mode: "auto",
    });
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "summary",
    );
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "caller_agent",
    );

    expect(result?.content[0].text).toContain("mem_123");
  });

  it("rejects whitespace-only summary, content, and evidence values", () => {
    const fetchFn = vi.fn(async () => {
      throw new Error("unexpected fetch");
    });

    const registered: Array<
      [
        string,
        { inputSchema?: { parse: (input: unknown) => unknown } },
        unknown,
      ]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
      ) => {
        registered.push([name, schema, undefined]);
      },
    } as unknown as McpServer;

    registerMemoryCommitFact(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_commit_fact");
    expect(tool).toBeDefined();

    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "   ",
        content: "facts",
        evidence: ["tests/ingest_api.rs"],
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "summary",
        content: "   ",
        evidence: ["tests/ingest_api.rs"],
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "summary",
        content: "facts",
        evidence: ["   "],
      }),
    ).toThrow();
  });

  it("uses defaultTenant when tenant is omitted", () => {
    const fetchFn = vi.fn(async () => {
      throw new Error("unexpected fetch");
    });

    const registered: Array<
      [
        string,
        { inputSchema?: { parse: (input: unknown) => unknown } },
        unknown,
      ]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
      ) => {
        registered.push([name, schema, undefined]);
      },
    } as unknown as McpServer;

    registerMemoryCommitFact(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "default-tenant",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_commit_fact");
    expect(tool).toBeDefined();

    const parsed = tool?.[1].inputSchema?.parse({
      project: "mem",
      caller_agent: "codex-cli",
      source_agent: "mem-mcp",
      summary: "summary",
      content: "facts",
      evidence: ["tests/ingest_api.rs"],
    }) as Record<string, unknown>;

    expect(parsed.tenant).toBe("default-tenant");
  });

  it("rejects blank tenant", () => {
    const fetchFn = vi.fn(async () => {
      throw new Error("unexpected fetch");
    });

    const registered: Array<
      [
        string,
        { inputSchema?: { parse: (input: unknown) => unknown } },
        unknown,
      ]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
      ) => {
        registered.push([name, schema, undefined]);
      },
    } as unknown as McpServer;

    registerMemoryCommitFact(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "default-tenant",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_commit_fact");
    expect(tool).toBeDefined();

    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "   ",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "summary",
        content: "facts",
        evidence: ["tests/ingest_api.rs"],
      }),
    ).toThrow();
  });

  it("forwards optional supported fields while preserving caller-agent tagging", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 201,
      text: async () => JSON.stringify({ memory_id: "mem_456", status: "active" }),
    }));

    const registered: Array<
      [
        string,
        { inputSchema?: { parse: (input: unknown) => unknown } },
        (args: Record<string, unknown>) => Promise<{
          content: Array<{ type: "text"; text: string }>;
        }>,
      ]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
        handler: (args: Record<string, unknown>) => Promise<{
          content: Array<{ type: "text"; text: string }>;
        }>,
      ) => {
        registered.push([name, schema, handler]);
      },
    } as unknown as McpServer;

    registerMemoryCommitFact(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_commit_fact");
    expect(tool).toBeDefined();

    const parsed = tool?.[1].inputSchema?.parse({
      tenant: "local",
      project: "mem",
      repo: "mem-mcp",
      module: "tools",
      caller_agent: "codex-ci",
      source_agent: "mem-mcp",
      summary: "fact summary",
      content: "fact content",
      evidence: ["tests/ingest_api.rs"],
      tags: ["shared-fact"],
      idempotency_key: "fact-123",
    }) as Record<string, unknown>;

    await tool?.[2](parsed);

    const [, init] = fetchFn.mock.calls[0];
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      repo: "mem-mcp",
      module: "tools",
      idempotency_key: "fact-123",
      tags: ["shared-fact", "caller_agent:codex-ci"],
    });
  });
});
