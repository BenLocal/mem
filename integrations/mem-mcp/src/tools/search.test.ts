import { describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { registerMemorySearch } from "./search.js";

describe("memory_bootstrap", () => {
  it("sends low-budget project-scoped search and returns a trimmed result", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () =>
        JSON.stringify({
          directives: [{ memory_id: "m1" }],
          relevant_facts: [{ memory_id: "m2" }],
          reusable_patterns: [{ memory_id: "m3" }],
          suggested_workflow: { steps: ["inspect"] },
          raw_http_status: 200,
          raw_headers: { x: "leak" },
        }),
    }));

    const registered: Array<[
      string,
      { inputSchema?: { parse: (input: unknown) => unknown } },
      (args: Record<string, unknown>) => Promise<{
        content: Array<{ type: "text"; text: string }>;
      }>,
    ]> = [];

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

    registerMemorySearch(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        fetchFn: fetchFn as unknown as typeof fetch,
      },
    );

    const bootstrap = registered.find(([name]) => name === "memory_bootstrap");
    expect(bootstrap).toBeDefined();

    const inputSchema = bootstrap?.[1].inputSchema;
    expect(inputSchema).toBeDefined();

    const parsed = inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "vitest",
      source_agent: "vitest",
      query: "bootstrap",
    }) as Record<string, unknown>;

    const result = await bootstrap?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/memories/search");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      query: "bootstrap",
      intent: "bootstrap",
      scope_filters: ["project:mem"],
      token_budget: 120,
      caller_agent: "vitest",
      expand_graph: false,
      tenant: "local",
    });

    const payload = JSON.parse(result?.content[0].text ?? "{}") as Record<
      string,
      unknown
    >;
    expect(payload).toMatchObject({
      directives: [{ memory_id: "m1" }],
      relevant_facts: [{ memory_id: "m2" }],
      reusable_patterns: [{ memory_id: "m3" }],
      suggested_workflow: { steps: ["inspect"] },
    });
    expect(payload).not.toHaveProperty("raw_http_status");
    expect(payload).not.toHaveProperty("raw_headers");
  });
});

describe("memory_search_contextual", () => {
  it("defaults to project-only scope", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () =>
        JSON.stringify({
          directives: [],
          relevant_facts: [],
          reusable_patterns: [],
        }),
    }));

    const registered: Array<[
      string,
      { inputSchema?: { parse: (input: unknown) => unknown } },
      (args: Record<string, unknown>) => Promise<{
        content: Array<{ type: "text"; text: string }>;
      }>,
    ]> = [];

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

    registerMemorySearch(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        fetchFn: fetchFn as unknown as typeof fetch,
      },
    );

    const contextual = registered.find(([name]) => name === "memory_search_contextual");
    expect(contextual).toBeDefined();

    const inputSchema = contextual?.[1].inputSchema;
    expect(inputSchema).toBeDefined();

    const parsed = inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "vitest",
      query: "debug cache",
      intent: "debugging",
    }) as Record<string, unknown>;

    await contextual?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/memories/search");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      query: "debug cache",
      intent: "debugging",
      scope_filters: ["project:mem"],
      token_budget: 400,
      caller_agent: "vitest",
      expand_graph: true,
      tenant: "local",
    });
  });

  it("trims contextual results through pickSearchSummary", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () =>
        JSON.stringify({
          directives: [{ memory_id: "m1" }],
          relevant_facts: [{ memory_id: "m2" }],
          reusable_patterns: [{ memory_id: "m3" }],
          suggested_workflow: { steps: ["inspect"] },
          raw_http_status: 200,
          raw_headers: { x: "leak" },
        }),
    }));

    const registered: Array<[
      string,
      { inputSchema?: { parse: (input: unknown) => unknown } },
      (args: Record<string, unknown>) => Promise<{
        content: Array<{ type: "text"; text: string }>;
      }>,
    ]> = [];

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

    registerMemorySearch(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        fetchFn: fetchFn as unknown as typeof fetch,
      },
    );

    const contextual = registered.find(([name]) => name === "memory_search_contextual");
    expect(contextual).toBeDefined();

    const inputSchema = contextual?.[1].inputSchema;
    expect(inputSchema).toBeDefined();

    const parsed = inputSchema?.parse({
      tenant: "local",
      project: "mem",
      repo: "mem",
      caller_agent: "vitest",
      query: "debug cache",
      intent: "debugging",
      include_repo: true,
      include_personal: true,
    }) as Record<string, unknown>;

    const result = await contextual?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/memories/search");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      query: "debug cache",
      intent: "debugging",
      scope_filters: ["project:mem", "repo:mem", "scope:workspace"],
      token_budget: 400,
      caller_agent: "vitest",
      expand_graph: true,
      tenant: "local",
    });

    const payload = JSON.parse(result?.content[0].text ?? "{}") as Record<
      string,
      unknown
    >;
    expect(payload).toMatchObject({
      directives: [{ memory_id: "m1" }],
      relevant_facts: [{ memory_id: "m2" }],
      reusable_patterns: [{ memory_id: "m3" }],
      suggested_workflow: { steps: ["inspect"] },
    });
    expect(payload).not.toHaveProperty("raw_http_status");
    expect(payload).not.toHaveProperty("raw_headers");
  });

  it("rejects include_repo without repo", () => {
    const registered: Array<[
      string,
      { inputSchema?: { parse: (input: unknown) => unknown } },
      (args: Record<string, unknown>) => Promise<{
        content: Array<{ type: "text"; text: string }>;
      }>,
    ]> = [];

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

    registerMemorySearch(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        fetchFn: vi.fn(async () => {
          throw new Error("unexpected fetch");
        }) as unknown as typeof fetch,
      },
    );

    const contextual = registered.find(([name]) => name === "memory_search_contextual");
    expect(contextual).toBeDefined();

    expect(() =>
      contextual?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "vitest",
        query: "debug cache",
        intent: "debugging",
        include_repo: true,
      }),
    ).toThrowError(/repo is required when include_repo is true/);
  });
});
