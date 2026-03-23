import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { describe, expect, it, vi } from "vitest";
import { registerMemoryFeedback } from "./feedback.js";

describe("memory_apply_feedback", () => {
  it("maps limited feedback kinds to the existing feedback endpoint", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
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

    registerMemoryFeedback(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: fetchFn as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_apply_feedback");
    expect(tool).toBeDefined();

    const parsed = tool?.[1].inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "codex-cli",
      memory_id: "mem_123",
      kind: "useful",
      note: "keep",
    }) as Record<string, unknown>;

    const result = await tool?.[2](parsed);

    expect(fetchFn).toHaveBeenCalledTimes(1);
    const [url, init] = fetchFn.mock.calls[0];
    expect(url).toBe("http://127.0.0.1:3000/memories/feedback");
    expect(init).toMatchObject({ method: "POST" });
    expect(JSON.parse((init as RequestInit).body as string)).toMatchObject({
      tenant: "local",
      memory_id: "mem_123",
      feedback_kind: "useful",
    });
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "project",
    );
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "caller_agent",
    );
    expect(JSON.parse((init as RequestInit).body as string)).not.toHaveProperty(
      "note",
    );

    expect(result?.content[0].text).toContain("mem_123");
  });

  it("rejects feedback kinds reserved for the raw feedback tool", () => {
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

    registerMemoryFeedback(server, {
      baseUrl: "http://127.0.0.1:3000",
      defaultTenant: "local",
      fetchFn: vi.fn(async () => {
        throw new Error("unexpected fetch");
      }) as unknown as typeof fetch,
    });

    const tool = registered.find(([name]) => name === "memory_apply_feedback");
    expect(tool).toBeDefined();

    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        memory_id: "mem_123",
        kind: "applies_here",
      }),
    ).toThrow();
    expect(() =>
      tool?.[1].inputSchema?.parse({
        tenant: "local",
        project: "mem",
        caller_agent: "codex-cli",
        memory_id: "mem_123",
        kind: "does_not_apply_here",
      }),
    ).toThrow();
  });
});
