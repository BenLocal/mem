import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemorySearch(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn } = ctx;

  server.registerTool(
    "memory_search",
    {
      description:
        "Search the shared mem service for compressed directives, facts, and patterns. Call early in a task; use scope_filters like repo:<name> to narrow results.",
      inputSchema: {
        query: z.string().min(1),
        intent: z.string().optional().default("general"),
        scope_filters: z.array(z.string()).optional().default([]),
        token_budget: z.number().int().positive().optional().default(400),
        caller_agent: z
          .string()
          .min(1)
          .describe("Identify this runtime, e.g. codex-cli, cursor, ci:job-123"),
        expand_graph: z.boolean().optional().default(true),
        tenant: z
          .string()
          .optional()
          .describe("Override MEM_TENANT; omit to use server default (local)"),
      },
    },
    async (args) => {
      try {
        const body: Record<string, unknown> = {
          query: args.query,
          intent: args.intent,
          scope_filters: args.scope_filters,
          token_budget: args.token_budget,
          caller_agent: args.caller_agent,
          expand_graph: args.expand_graph,
        };
        if (args.tenant !== undefined && args.tenant !== "") {
          body.tenant = args.tenant;
        }
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories/search",
          { body },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
