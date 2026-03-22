import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { scopeZ, visibilityZ } from "../schemas.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerEpisodeIngest(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "episode_ingest",
    {
      description:
        "Record a successful multi-step episode; may produce workflow candidates.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        goal: z.string().min(1),
        steps: z.array(z.string()),
        outcome: z.string().min(1),
        evidence: z.array(z.string()).optional().default([]),
        scope: scopeZ.optional().default("workspace"),
        visibility: visibilityZ.optional().default("private"),
        project: z.string().optional(),
        repo: z.string().optional(),
        module: z.string().optional(),
        tags: z.array(z.string()).optional().default([]),
        source_agent: z.string().optional().default("mem-mcp"),
        idempotency_key: z.string().optional(),
      },
    },
    async (args) => {
      try {
        const body: Record<string, unknown> = {
          tenant: args.tenant,
          goal: args.goal,
          steps: args.steps,
          outcome: args.outcome,
          evidence: args.evidence,
          scope: args.scope,
          visibility: args.visibility,
          tags: args.tags,
          source_agent: args.source_agent,
        };
        if (args.project !== undefined) body.project = args.project;
        if (args.repo !== undefined) body.repo = args.repo;
        if (args.module !== undefined) body.module = args.module;
        if (args.idempotency_key !== undefined) {
          body.idempotency_key = args.idempotency_key;
        }
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "episodes",
          { body },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
