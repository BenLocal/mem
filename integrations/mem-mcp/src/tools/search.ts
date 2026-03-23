import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { z } from "zod";
import { memRequestJson } from "../mem-client.js";
import {
  memoryBootstrapInputZ,
  memorySearchContextualInputZ,
} from "../schemas.js";
import { errResult, okJson } from "../tool-result.js";
import type { ToolContext } from "./context.js";

function pickSearchSummary(data: unknown): Record<string, unknown> {
  if (data === null || typeof data !== "object") {
    return {};
  }

  const record = data as Record<string, unknown>;
  const result: Record<string, unknown> = {};

  if (Array.isArray(record.directives)) {
    result.directives = record.directives;
  }
  if (Array.isArray(record.relevant_facts)) {
    result.relevant_facts = record.relevant_facts;
  }
  if (Array.isArray(record.reusable_patterns)) {
    result.reusable_patterns = record.reusable_patterns;
  }
  if (record.suggested_workflow && typeof record.suggested_workflow === "object") {
    result.suggested_workflow = record.suggested_workflow;
  }

  return result;
}

function buildContextualScopeFilters(args: {
  project: string;
  repo?: string;
  include_repo: boolean;
  include_personal: boolean;
}): string[] {
  const scopeFilters = [`project:${args.project}`];

  if (args.include_repo && args.repo !== undefined) {
    scopeFilters.push(`repo:${args.repo}`);
  }
  if (args.include_personal) {
    scopeFilters.push("scope:workspace");
  }

  return scopeFilters;
}

export function registerMemorySearch(server: McpServer, ctx: ToolContext): void {
  const { baseUrl, fetchFn } = ctx;

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
    "memory_bootstrap",
    {
      description:
        "Lightweight project-only bootstrap search for task-start context recovery.",
      inputSchema: memoryBootstrapInputZ,
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories/search",
          {
            body: {
              query: args.query,
              intent: "bootstrap",
              scope_filters: [`project:${args.project}`],
              token_budget: args.token_budget,
              caller_agent: args.caller_agent,
              expand_graph: false,
              tenant: args.tenant,
            },
          },
        );
        return okJson(pickSearchSummary(data));
      } catch (e) {
        return errResult(e);
      }
    },
  );

  server.registerTool(
    "memory_search_contextual",
    {
      description:
        "Intent-aware search for implementation, debugging, or review. Defaults to project scope and only widens when explicitly requested.",
      inputSchema: memorySearchContextualInputZ,
    },
    async (args) => {
      try {
        const data = await memRequestJson(
          baseUrl,
          fetchFn,
          "POST",
          "memories/search",
          {
            body: {
              query: args.query,
              intent: args.intent,
              scope_filters: buildContextualScopeFilters(args),
              token_budget: args.token_budget,
              caller_agent: args.caller_agent,
              expand_graph: true,
              tenant: args.tenant,
            },
          },
        );
        return okJson(pickSearchSummary(data));
      } catch (e) {
        return errResult(e);
      }
    },
  );
}
