export type FetchFn = typeof fetch;

export function joinUrl(baseUrl: string, path: string): string {
  const base = baseUrl.replace(/\/+$/, "");
  const p = path.startsWith("/") ? path.slice(1) : path;
  return `${base}/${p}`;
}

export async function memRequestJson(
  baseUrl: string,
  fetchFn: FetchFn,
  method: string,
  path: string,
  opts?: {
    query?: Record<string, string | undefined>;
    body?: unknown;
  },
): Promise<unknown> {
  let url = joinUrl(baseUrl, path);
  if (opts?.query) {
    const u = new URL(url);
    for (const [k, v] of Object.entries(opts.query)) {
      if (v !== undefined && v !== "") {
        u.searchParams.set(k, v);
      }
    }
    url = u.toString();
  }

  const headers: Record<string, string> = { Accept: "application/json" };
  let body: string | undefined;
  if (opts?.body !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(opts.body);
  }

  const res = await fetchFn(url, { method, headers, body });
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`mem HTTP ${res.status}: ${text.slice(0, 2000)}`);
  }
  if (!text) {
    return null;
  }
  return JSON.parse(text) as unknown;
}
