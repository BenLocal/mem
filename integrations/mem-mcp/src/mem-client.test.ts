import { describe, expect, it, vi } from "vitest";
import { joinUrl, memRequestJson, memRequestText } from "./mem-client.js";

describe("joinUrl", () => {
  it("joins base and path", () => {
    expect(joinUrl("http://127.0.0.1:3000", "memories/search")).toBe(
      "http://127.0.0.1:3000/memories/search",
    );
    expect(joinUrl("http://127.0.0.1:3000/", "/health")).toBe(
      "http://127.0.0.1:3000/health",
    );
  });
});

describe("memRequestJson", () => {
  it("throws on non-ok with status and body snippet", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: false,
      status: 503,
      text: async () => "down",
    })) as unknown as typeof fetch;

    await expect(
      memRequestJson("http://h", fetchFn, "GET", "health"),
    ).rejects.toThrow(/mem HTTP 503/);
    await expect(
      memRequestJson("http://h", fetchFn, "GET", "health"),
    ).rejects.toThrow(/down/);
  });

  it("parses JSON body on success", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () => '{"a":1}',
    })) as unknown as typeof fetch;

    const data = await memRequestJson("http://h", fetchFn, "GET", "x");
    expect(data).toEqual({ a: 1 });
  });

  it("appends query params", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () => "[]",
    })) as unknown as typeof fetch;

    await memRequestJson("http://h", fetchFn, "GET", "reviews/pending", {
      query: { tenant: "t1" },
    });
    expect(fetchFn).toHaveBeenCalledWith(
      "http://h/reviews/pending?tenant=t1",
      expect.any(Object),
    );
  });

  it("supports graph neighbor paths with encoded colons", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () => "[]",
    })) as unknown as typeof fetch;

    const path = `graph/neighbors/${encodeURIComponent("module:mem:invoice")}`;
    await memRequestJson("http://h", fetchFn, "GET", path);
    expect(fetchFn).toHaveBeenCalledWith(
      "http://h/graph/neighbors/module%3Amem%3Ainvoice",
      expect.any(Object),
    );
  });
});

describe("memRequestText", () => {
  it("returns plain text without JSON parse", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: true,
      status: 200,
      text: async () => "ok\n",
    })) as unknown as typeof fetch;

    const t = await memRequestText("http://h", fetchFn, "GET", "health");
    expect(t).toBe("ok\n");
    expect(fetchFn).toHaveBeenCalledWith(
      "http://h/health",
      expect.objectContaining({
        headers: { Accept: "*/*" },
      }),
    );
  });

  it("throws on non-ok", async () => {
    const fetchFn = vi.fn(async () => ({
      ok: false,
      status: 502,
      text: async () => "bad gateway",
    })) as unknown as typeof fetch;

    await expect(
      memRequestText("http://h", fetchFn, "GET", "health"),
    ).rejects.toThrow(/mem HTTP 502/);
  });
});
