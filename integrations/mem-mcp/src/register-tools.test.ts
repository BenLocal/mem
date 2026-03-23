import { describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { registerMemTools } from "./register-tools.js";

describe("shared memory tool contracts", () => {
  it("memory_bootstrap defaults to project-only scope", () => {
    const registered: Array<[string, unknown]> = [];
    const server = {
      registerTool: (name: string, schema: unknown) => {
        registered.push([name, schema]);
      },
    } as unknown as McpServer;

    registerMemTools(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        exposeEmbeddings: false,
      },
      vi.fn(async () => {
        throw new Error("unexpected fetch");
      }) as unknown as typeof fetch,
    );

    const bootstrap = registered.find(([name]) => name === "memory_bootstrap");
    expect(bootstrap, "memory_bootstrap should be registered").toBeDefined();

    const inputSchema = (bootstrap?.[1] as {
      inputSchema?: { parse: (input: unknown) => { scope?: string } };
    })?.inputSchema;
    expect(inputSchema, "memory_bootstrap input schema should exist").toBeDefined();

    const parsed = inputSchema?.parse({
      tenant: "local",
      project: "mem",
      caller_agent: "vitest",
      query: "bootstrap",
    });
    expect(parsed?.scope).toBe("project");
    expect(parsed).not.toHaveProperty("repo");
    expect(parsed).not.toHaveProperty("source_agent");
  });
});
