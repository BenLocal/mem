import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import {
  memoryTypeZ,
  memoryProposePreferenceInputZ,
  scopeZ,
  visibilityZ,
  writeModeZ,
} from "../schemas.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemoryIngest(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_ingest",
    {
      description:
        "Create a memory in mem. Use write_mode propose for preferences; auto is fine for implementation facts.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        memory_type: memoryTypeZ,
        content: z.string().min(1),
        evidence: z.array(z.string()).optional().default([]),
        code_refs: z.array(z.string()).optional().default([]),
        scope: scopeZ,
        visibility: visibilityZ.optional().default("private"),
        project: z.string().optional(),
        repo: z.string().optional(),
        module: z.string().optional(),
        task_type: z.string().optional(),
        tags: z.array(z.string()).optional().default([]),
        source_agent: z.string().optional().default("mem-mcp"),
        idempotency_key: z.string().optional(),
        write_mode: writeModeZ.optional().default("auto"),
      },
    },
    async (args) => {
      try {
        const body: Record<string, unknown> = {
          tenant: args.tenant,
          memory_type: args.memory_type,
          content: args.content,
          evidence: args.evidence,
          code_refs: args.code_refs,
          scope: args.scope,
          visibility: args.visibility,
          tags: args.tags,
          source_agent: args.source_agent,
          write_mode: args.write_mode,
        };
        if (args.project !== undefined) body.project = args.project;
        if (args.repo !== undefined) body.repo = args.repo;
        if (args.module !== undefined) body.module = args.module;
        if (args.task_type !== undefined) body.task_type = args.task_type;
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

export function registerMemoryProposePreference(
  server: McpServer,
  ctx: ToolContext,
): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_propose_preference",
    {
      description:
        "Propose a preference for review. Uses the standard memories endpoint with write_mode=propose.",
      inputSchema: memoryProposePreferenceInputZ.extend({
        tenant: z.string().optional().default(defaultTenant),
      }),
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories",
          {
            body: {
              tenant: args.tenant,
              memory_type: "preference",
              content: `${args.summary}\n\n${args.content}`,
              evidence: args.evidence,
              code_refs: [],
              scope: "project",
              visibility: "private",
              project: args.project,
              repo: args.repo,
              module: args.module,
              tags: [`caller_agent:${args.caller_agent}`],
              source_agent: args.source_agent,
              write_mode: "propose",
            },
          },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
