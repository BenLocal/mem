import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { memoryCommitFactToolInputZ } from "../schemas.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemoryCommitFact(
  server: McpServer,
  ctx: ToolContext,
): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_commit_fact",
    {
      description:
        "Commit a verified project fact. Uses auto write mode and project scope.",
      inputSchema: memoryCommitFactToolInputZ.extend({
        tenant: z.string().trim().min(1).optional().default(defaultTenant),
      }),
    },
    async (args) => {
      try {
        const body: Record<string, unknown> = {
          tenant: args.tenant,
          memory_type: "implementation",
          content: `${args.summary}\n\n${args.content}`,
          evidence: args.evidence,
          scope: "project",
          visibility: "private",
          project: args.project,
          repo: args.repo,
          module: args.module,
          source_agent: args.source_agent,
          tags: [...args.tags, `caller_agent:${args.caller_agent}`],
          write_mode: "auto",
        };

        if (args.idempotency_key !== undefined) {
          body.idempotency_key = args.idempotency_key;
        }

        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories",
          { body },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
