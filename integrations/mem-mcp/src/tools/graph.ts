import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

/**
 * Graph node ids often contain colons (e.g. module:mem:invoice); encode for the path segment.
 */
export function registerMemoryGraphNeighbors(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn } = ctx;

  server.registerTool(
    "memory_graph_neighbors",
    {
      description:
        "List graph edges adjacent to a node id (e.g. module:mem:billing, project:acme). Complements memory_search when expand_graph is not enough.",
      inputSchema: {
        node_id: z
          .string()
          .min(1)
          .describe("Graph node id as returned by mem APIs or README examples"),
      },
    },
    async (args) => {
      try {
        const segment = encodeURIComponent(args.node_id);
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "GET",
          `graph/neighbors/${segment}`,
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
