import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerReviewActionTools(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_review_accept",
    {
      description:
        "Accept a pending memory (activate without edits). Use after human confirms.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        memory_id: z.string().min(1),
      },
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "reviews/pending/accept",
          {
            body: { tenant: args.tenant, memory_id: args.memory_id },
          },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );

  server.registerTool(
    "memory_review_reject",
    {
      description: "Reject a pending memory (mark rejected, no successor).",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        memory_id: z.string().min(1),
      },
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "reviews/pending/reject",
          {
            body: { tenant: args.tenant, memory_id: args.memory_id },
          },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );

  server.registerTool(
    "memory_review_edit_accept",
    {
      description:
        "Edit pending memory content then accept: creates an active successor and rejects the original pending row.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        memory_id: z.string().min(1),
        summary: z.string().min(1),
        content: z.string().min(1),
        evidence: z.array(z.string()).optional().default([]),
        code_refs: z.array(z.string()).optional().default([]),
        tags: z.array(z.string()).optional().default([]),
      },
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "reviews/pending/edit_accept",
          {
            body: {
              tenant: args.tenant,
              memory_id: args.memory_id,
              summary: args.summary,
              content: args.content,
              evidence: args.evidence,
              code_refs: args.code_refs,
              tags: args.tags,
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
