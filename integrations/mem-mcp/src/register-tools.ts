import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import type { MemMcpConfig } from "./config.js";
import type { FetchFn } from "./mem-client.js";
import { memRequestJson } from "./mem-client.js";

const memoryTypeZ = z.enum([
  "implementation",
  "experience",
  "preference",
  "episode",
  "workflow",
]);
const scopeZ = z.enum(["global", "project", "repo", "workspace"]);
const visibilityZ = z.enum(["private", "shared", "system"]);
const writeModeZ = z.enum(["auto", "propose"]);
const feedbackKindZ = z.enum([
  "useful",
  "outdated",
  "incorrect",
  "applies_here",
  "does_not_apply_here",
]);

function okJson(data: unknown) {
  return {
    content: [
      {
        type: "text" as const,
        text: JSON.stringify(data, null, 2),
      },
    ],
  };
}

function errResult(err: unknown) {
  const message = err instanceof Error ? err.message : String(err);
  return {
    isError: true as const,
    content: [{ type: "text" as const, text: message }],
  };
}

export function registerMemTools(
  server: McpServer,
  config: MemMcpConfig,
  fetchFn: FetchFn,
): void {
  const { baseUrl, defaultTenant, exposeEmbeddings } = config;

  server.registerTool(
    "memory_search",
    {
      description:
        "Search the shared mem service for compressed directives, facts, and patterns. Call early in a task; use scope_filters like repo:<name> to narrow results.",
      inputSchema: {
        query: z.string().min(1),
        intent: z.string().optional().default("general"),
        scope_filters: z.array(z.string()).optional().default([]),
        token_budget: z.number().int().positive().optional().default(400),
        caller_agent: z
          .string()
          .min(1)
          .describe("Identify this runtime, e.g. codex-cli, cursor, ci:job-123"),
        expand_graph: z.boolean().optional().default(true),
        tenant: z
          .string()
          .optional()
          .describe("Override MEM_TENANT; omit to use server default (local)"),
      },
    },
    async (args) => {
      try {
        const body: Record<string, unknown> = {
          query: args.query,
          intent: args.intent,
          scope_filters: args.scope_filters,
          token_budget: args.token_budget,
          caller_agent: args.caller_agent,
          expand_graph: args.expand_graph,
        };
        if (args.tenant !== undefined && args.tenant !== "") {
          body.tenant = args.tenant;
        }
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories/search",
          { body },
        );
        return okJson(data);
      } catch (e) {
        return errResult(e);
      }
    },
  );

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

  if (exposeEmbeddings) {
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
}
