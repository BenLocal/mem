import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { describe, expect, it, vi } from "vitest";
import { registerMemoryProposeExperience } from "./episode.js";
import { registerMemoryProposePreference } from "./ingest.js";

describe("memory_propose_experience", () => {
  it("uses the episode path instead of a strong auto-fact ingest path", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 201,
      text: async () =>
        JSON.stringify({ episode_id: "ep_123", status: "active" }),
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

    registerMemoryProposeExperience(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_propose_experience");
    expect(tool).toBeDefined();

    const parsed = tool?.[1].inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "codex-cli",
      source_agent: "mem-mcp",
      summary: "A review pass should record the episode, not a fact.",
      content: "We should preserve this as an episode candidate.",
      evidence: ["tests/episode.rs"],
    }) as Record<string, unknown>;

    const result = await tool?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/episodes");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      tenant: "local",
      goal: "A review pass should record the episode, not a fact.",
      steps: [],
      outcome: "We should preserve this as an episode candidate.",
      evidence: ["tests/episode.rs"],
      scope: "project",
      visibility: "private",
      project: "mem",
      source_agent: "mem-mcp",
    });
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "write_mode",
    );
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "memory_type",
    );

    expect(result?.content[0].text).toContain("ep_123");
  });

  it("rejects whitespace-only proposal fields", () => {
    const registered: Array<
      [string, { inputSchema?: { parse: (input: unknown) => unknown } }, unknown]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
      ) => {
        registered.push([name, schema, undefined]);
      },
    } as unknown as McpServer;

    registerMemoryProposeExperience(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: vi.fn(async () => {
        throw new Error("unexpected fetch");
      }) as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_propose_experience");
    expect(tool).toBeDefined();

    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "   ",
        content: "candidate",
        evidence: ["tests/episode.rs"],
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "candidate",
        content: "   ",
        evidence: ["tests/episode.rs"],
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "candidate",
        content: "episode",
        evidence: ["   "],
      }),
    ).toThrow();
  });
});

describe("memory_propose_preference", () => {
  it("always enters the review flow with write_mode=propose", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 201,
      text: async () =>
        JSON.stringify({ memory_id: "mem_123", status: "provisional" }),
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

    registerMemoryProposePreference(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_propose_preference");
    expect(tool).toBeDefined();

    const parsed = tool?.[1].inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "codex-cli",
      source_agent: "mem-mcp",
      summary: "Prefer project-scoped searches by default.",
      content: "Keep personal scope out unless explicitly requested.",
      evidence: ["docs/memory.md"],
    }) as Record<string, unknown>;

    const result = await tool?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/memories");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      tenant: "local",
      memory_type: "preference",
      content:
        "Prefer project-scoped searches by default.\n\nKeep personal scope out unless explicitly requested.",
      evidence: ["docs/memory.md"],
      code_refs: [],
      scope: "project",
      visibility: "private",
      project: "mem",
      source_agent: "mem-mcp",
      write_mode: "propose",
    });

    expect(result?.content[0].text).toContain("mem_123");
  });

  it("rejects whitespace-only proposal fields", () => {
    const registered: Array<
      [string, { inputSchema?: { parse: (input: unknown) => unknown } }, unknown]
    > = [];

    const server = {
      registerTool: (
        name: string,
        schema: { inputSchema?: { parse: (input: unknown) => unknown } },
      ) => {
        registered.push([name, schema, undefined]);
      },
    } as unknown as McpServer;

    registerMemoryProposePreference(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: vi.fn(async () => {
        throw new Error("unexpected fetch");
      }) as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_propose_preference");
    expect(tool).toBeDefined();

    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "   ",
        content: "candidate",
        evidence: ["docs/memory.md"],
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "candidate",
        content: "   ",
        evidence: ["docs/memory.md"],
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        source_agent: "mem-mcp",
        summary: "candidate",
        content: "preference",
        evidence: ["   "],
      }),
    ).toThrow();
  });
});
