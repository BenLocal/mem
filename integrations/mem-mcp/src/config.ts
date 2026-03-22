export interface MemMcpConfig {
  baseUrl: string;
  defaultTenant: string;
  exposeEmbeddings: boolean;
}

function stripTrailingSlash(s: string): string {
  return s.replace(/\/+$/, "");
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): MemMcpConfig {
  const baseUrl = stripTrailingSlash(
    env.MEM_BASE_URL?.trim() || "http://127.0.0.1:3000",
  );
  const defaultTenant = env.MEM_TENANT?.trim() || "local";
  const exposeEmbeddings = env.MEM_MCP_EXPOSE_EMBEDDINGS === "1";
  return { baseUrl, defaultTenant, exposeEmbeddings };
}
