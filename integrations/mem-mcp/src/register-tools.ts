import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import type { MemMcpConfig } from "./config.js";
import type { FetchFn } from "./mem-client.js";
import { registerEpisodeIngest } from "./tools/episode.js";
import { registerEmbeddingsTools } from "./tools/embeddings.js";
import { registerMemoryFeedback } from "./tools/feedback.js";
import { registerMemoryGraphNeighbors } from "./tools/graph.js";
import { registerMemHealth } from "./tools/health.js";
import { registerMemoryIngest } from "./tools/ingest.js";
import { registerMemoryGet } from "./tools/memory-get.js";
import { registerMemoryListPendingReview } from "./tools/pending.js";
import { registerMemorySearch } from "./tools/search.js";
import type { ToolContext } from "./tools/context.js";

export function registerMemTools(
  server: McpServer,
  config: MemMcpConfig,
  fetchFn: FetchFn,
): void {
  const { baseUrl, defaultTenant, exposeEmbeddings } = config;
  const ctx: ToolContext = { baseUrl, fetchFn, defaultTenant };

  registerMemHealth(server, ctx);
  registerMemorySearch(server, ctx);
  registerMemoryIngest(server, ctx);
  registerMemoryGet(server, ctx);
  registerMemoryFeedback(server, ctx);
  registerMemoryListPendingReview(server, ctx);
  registerEpisodeIngest(server, ctx);
  registerMemoryGraphNeighbors(server, ctx);

  if (exposeEmbeddings) {
    registerEmbeddingsTools(server, ctx);
  }
}
