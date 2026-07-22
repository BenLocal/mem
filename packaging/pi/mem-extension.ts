import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { spawn } from "node:child_process";

export const MEM_BASE_URL = process.env.MEM_BASE_URL ?? "http://127.0.0.1:3000";

let servePid: number | undefined;
let serveStartedByUs = false;

export async function isServeUp(baseUrl: string): Promise<boolean> {
  try {
    const res = await fetch(`${baseUrl}/health`, { signal: AbortSignal.timeout(1000) });
    return res.ok;
  } catch {
    return false;
  }
}

async function ensureServe(baseUrl: string): Promise<void> {
  if (await isServeUp(baseUrl)) {
    serveStartedByUs = false;
    return;
  }
  const child = spawn("mem", ["serve"], { detached: true, stdio: "ignore", env: process.env });
  child.unref();
  servePid = child.pid;
  serveStartedByUs = true;
  // Poll /health up to ~10s.
  for (let i = 0; i < 50; i++) {
    if (await isServeUp(baseUrl)) return;
    await new Promise((r) => setTimeout(r, 200));
  }
  console.warn("[mem] serve did not become healthy within 10s");
}

function stopServe(): void {
  if (serveStartedByUs && servePid !== undefined) {
    try { process.kill(servePid, "SIGTERM"); } catch { /* already gone */ }
  }
}

const memExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", async (_event, _ctx: ExtensionContext) => {
    try {
      await ensureServe(MEM_BASE_URL);
    } catch (e) {
      console.warn("[mem] session_start lifecycle failed:", e);
    }
  });

  pi.on("session_shutdown", (_event, _ctx: ExtensionContext) => {
    stopServe();
  });
};

export default memExtension;
