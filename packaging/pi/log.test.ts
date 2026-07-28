import { test } from "node:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createLogger, errText, type LogSink } from "./log.ts";

interface Notified {
  message: string;
  level: string | undefined;
}

/** Fake ExtensionContext slice that records notify calls; `hasUI` is controllable. */
function fakeSink(hasUI: boolean, notified: Notified[]): LogSink {
  return {
    get hasUI() {
      return hasUI;
    },
    ui: {
      notify(message, level) {
        notified.push({ message, level });
      },
    },
  };
}

test("logs go through notify, never stderr, when the TUI is up", () => {
  const notified: Notified[] = [];
  const stderr: string[] = [];
  const log = createLogger(() => fakeSink(true, notified), (line) => stderr.push(line));

  log("registered 30 tools via mem mcp");

  assert.deepEqual(notified, [
    { message: "[mem] registered 30 tools via mem mcp", level: "info" },
  ]);
  assert.deepEqual(stderr, []);
});

test("the notify level is caller-controlled", () => {
  const notified: Notified[] = [];
  const log = createLogger(() => fakeSink(true, notified), () => {});

  log("serve did not become healthy within 10s", "warning");

  assert.equal(notified[0]?.level, "warning");
});

// Headless (print/RPC mode, `pi -p`): there is no TUI to corrupt, so stderr is
// the correct destination — that is where an operator looks for extension logs.
test("falls back to stderr when there is no UI", () => {
  const notified: Notified[] = [];
  const stderr: string[] = [];
  const log = createLogger(() => fakeSink(false, notified), (line) => stderr.push(line));

  log("mem serve spawn error: ENOENT", "error");

  assert.deepEqual(stderr, ["[mem] mem serve spawn error: ENOENT"]);
  assert.deepEqual(notified, []);
});

// `ensureServe` / `connectMcp` run inside the very first `session_start`, and
// the spawn `error` handlers can fire before any handler has stored a ctx.
test("falls back to stderr before any ctx has been captured", () => {
  const stderr: string[] = [];
  const log = createLogger(() => undefined, (line) => stderr.push(line));

  log("mem mcp spawn error: ENOENT", "error");

  assert.deepEqual(stderr, ["[mem] mem mcp spawn error: ENOENT"]);
});

// ExtensionContext properties are lazy getters guarded by assertActive(); reading
// one after the runner is torn down throws. Logging is called from catch blocks
// and detached child `error`/`exit` handlers, so it must never rethrow.
test("does not throw when reading hasUI throws (runner torn down)", () => {
  const stderr: string[] = [];
  const dead: LogSink = {
    get hasUI(): boolean {
      throw new Error("Extension runner is no longer active");
    },
    ui: {
      notify() {
        throw new Error("should not be reached");
      },
    },
  };
  const log = createLogger(() => dead, (line) => stderr.push(line));

  assert.doesNotThrow(() => log("mine (shutdown) failed: timeout", "warning"));
  assert.deepEqual(stderr, ["[mem] mine (shutdown) failed: timeout"]);
});

test("keeps the message when notify itself throws", () => {
  const stderr: string[] = [];
  const racing: LogSink = {
    hasUI: true,
    ui: {
      notify() {
        throw new Error("TUI is tearing down");
      },
    },
  };
  const log = createLogger(() => racing, (line) => stderr.push(line));

  assert.doesNotThrow(() => log("feedback failed: exec timeout", "warning"));
  assert.deepEqual(stderr, ["[mem] feedback failed: exec timeout"]);
});

// Every call site is `catch (e)` with an `unknown`. Formatting must not be the
// thing that throws — that would replace a logged warning with a crash.
test("errText unwraps an Error to its message", () => {
  assert.equal(errText(new Error("spawn mem ENOENT")), "spawn mem ENOENT");
});

test("errText stringifies a non-Error throw", () => {
  assert.equal(errText("plain string throw"), "plain string throw");
  assert.equal(errText(undefined), "undefined");
});

test("errText survives a value whose String() throws", () => {
  const hostile = {
    toString() {
      throw new Error("boom");
    },
  };

  assert.doesNotThrow(() => errText(hostile));
  assert.ok(errText(hostile).length > 0);
});

// The whole point of this module: a bare `console.*` in the extension prints
// into pi's TUI render area (over the input box) and corrupts the session.
// This guards the fix from regressing when a future edit adds "just one" log.
// `log.ts` itself is exempt — its stderr fallback is the sanctioned escape.
for (const file of ["mem-extension.ts", "mcp-client.ts"]) {
  test(`${file} contains no bare console calls`, async () => {
    const src = await readFile(new URL(`./${file}`, import.meta.url), "utf8");
    const code = src
      // Blank the comments but keep their newlines, so reported line numbers match the file.
      .replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, ""))
      .replace(/^\s*\/\/.*$/gm, "");

    const offenders = code
      .split("\n")
      .map((line, i) => ({ line: line.trim(), n: i + 1 }))
      .filter(({ line }) => /\bconsole\s*\./.test(line));

    assert.deepEqual(
      offenders,
      [],
      `${file}: use the createLogger logger instead of console.*:\n${offenders
        .map((o) => `  line ${o.n}: ${o.line}`)
        .join("\n")}`,
    );
  });
}
