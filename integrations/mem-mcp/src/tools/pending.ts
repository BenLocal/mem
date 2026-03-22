import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemoryListPendingReview(
  server: McpServer,
  ctx: ToolContext,
): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_list_pending_review",
    {
      description: "List memories awaiting human confirmation for this tenant.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
      },
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "GET",
          "reviews/pending",
          { query: { tenant: args.tenant } },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
