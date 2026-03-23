import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import {
  feedbackKindZ,
  memoryApplyFeedbackInputZ,
} from "../schemas.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

export function registerMemoryFeedback(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn, defaultTenant } = ctx;

  server.registerTool(
    "memory_feedback",
    {
      description: "Record feedback on a memory to adjust future ranking.",
      inputSchema: {
        tenant: z.string().optional().default(defaultTenant),
        memory_id: z.string().min(1),
        feedback_kind: feedbackKindZ,
      },
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories/feedback",
          {
            body: {
              tenant: args.tenant,
              memory_id: args.memory_id,
              feedback_kind: args.feedback_kind,
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
    "memory_apply_feedback",
    {
      description:
        "Apply limited feedback on a memory while keeping the existing POST /memories/feedback backend contract.",
      inputSchema: memoryApplyFeedbackInputZ.extend({
        tenant: z.string().optional().default(defaultTenant),
      }),
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories/feedback",
          {
            body: {
              tenant: args.tenant,
              memory_id: args.memory_id,
              feedback_kind: args.kind,
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
