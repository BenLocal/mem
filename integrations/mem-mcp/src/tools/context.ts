import type { FetchFn } from "../mem-client.js";

export type ToolContext = {
  baseUrl: string;
  fetchFn: FetchFn;
  defaultTenant: string;
};
