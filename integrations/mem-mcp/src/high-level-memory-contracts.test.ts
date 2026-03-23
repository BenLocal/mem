import { describe, expect, it, vi } from "vitest";
import type { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { registerMemTools } from "./register-tools.js";

describe("high-level memory contract inventory", () => {
  it("covers the current raw mem-mcp tool surface", () => {
    const registered: string[] = [];
    const server = {
      registerTool: (name: string) => {
        registered.push(name);
      },
    } as unknown as McpServer;

    registerMemTools(
      server,
      {
        baseUrl: "http://127.0.0.1:3000",
        defaultTenant: "local",
        exposeEmbeddings: true,
      },
      vi.fn(async () => {
        throw new Error("unexpected fetch");
      }) as unknown as typeof fetch,
    );

    expect(registered.sort()).toEqual(
      [
        "embeddings_list_jobs",
        "embeddings_providers",
        "embeddings_rebuild",
        "episode_ingest",
        "mem_health",
        "memory_bootstrap",
        "memory_commit_fact",
        "memory_feedback",
        "memory_apply_feedback",
        "memory_get",
        "memory_graph_neighbors",
        "memory_ingest",
        "memory_list_pending_review",
        "memory_propose_experience",
        "memory_propose_preference",
        "memory_review_accept",
        "memory_review_edit_accept",
        "memory_review_reject",
        "memory_search",
        "memory_search_contextual",
      ].sort(),
    );
  });
});
