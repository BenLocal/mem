#!/usr/bin/env node
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { loadConfig } from "./config.js";
import { registerMemTools } from "./register-tools.js";

async function main(): Promise<void> {
  const config = loadConfig();
  const server = new McpServer({
    name: "mem-mcp",
    version: "0.1.0",
  });
  registerMemTools(server, config, globalThis.fetch);
  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
