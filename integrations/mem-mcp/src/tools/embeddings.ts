import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerEmbeddingsTools(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "embeddings_list_jobs",
    {
      description: "Admin: list embedding jobs (requires MEM_MCP_EXPOSE_EMBEDDINGS=1).",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        status: z.string().optional(),
        memory_id: z.string().optional(),
        limit: z.number().int().positive().max(10_000).optional().default(200),
      },
    },
    async (args) => {
      try {
        const query: Record<string, string | undefined> = {
          tenant: args.tenant,
          limit: String(args.limit),
        };
        if (args.status) query.status = args.status;
        if (args.memory_id) query.memory_id = args.memory_id;
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "GET",
          "embeddings/jobs",
          { query },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );

  server.registerTool(
    "embeddings_rebuild",
    {
      description:
        "Admin: enqueue embedding rebuild; force clears vector row and stale live jobs server-side.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        memory_ids: z.array(z.string()).optional().default([]),
        force: z.boolean().optional().default(false),
      },
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "embeddings/rebuild",
          {
            body: {
              tenant: args.tenant,
              memory_ids: args.memory_ids,
              force: args.force,
            },
          },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );

  server.registerTool(
    "embeddings_providers",
    {
      description: "Admin: describe configured embedding provider and dimension.",
      inputSchema: z.object({}),
    },
    async () => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "GET",
          "embeddings/providers",
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
