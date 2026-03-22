import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemoryGet(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_get",
    {
      description:
        "Fetch one memory by id (detail, version chain, graph links, embedding metadata).",
      inputSchema: {
        memory_id: z.string().min(1),
        tenant: z.string().optional().describe("Defaults to MEM_TENANT when omitted"),
      },
    },
    async (args) => {
      try {
        const tenant = args.tenant?.trim() || defaultTenant;
        const id = encodeURIComponent(args.memory_id);
        const data = await memRequestJson(baseUrl, fetchFn, "GET", `memories/${id}`, {
          query: { tenant },
        });
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
