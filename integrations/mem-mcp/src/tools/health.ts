import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestText } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemHealth(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn } = ctx;

  server.registerTool(
    "mem_health",
    {
      description:
        "Check that the mem HTTP server is reachable (GET /health). Use when MCP tools fail to see if the service is up.",
      inputSchema: z.object({}),
    },
    async () => {
      try {
        const body = (await memRequestText(baseUrl, fetchFn, "GET", "health")).trim();
        return okJson({ reachable: true, health_body: body });
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
