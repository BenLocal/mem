import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

export const MEM_BASE_URL = process.env.MEM_BASE_URL ?? "http://127.0.0.1:3000";

const memExtension = (pi: ExtensionAPI): void => {
  pi.on("session_start", (_event, _ctx: ExtensionContext) => {
    console.warn("[mem] extension loaded");
  });
};

export default memExtension;
