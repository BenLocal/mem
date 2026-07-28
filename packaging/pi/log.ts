export type NotifyLevel = "info" | "warning" | "error";

/** The slice of `ExtensionContext` that logging actually needs. */
export interface LogSink {
  hasUI: boolean;
  ui: { notify(message: string, level?: NotifyLevel): void };
}

export type LogFn = (msg: string, level?: NotifyLevel) => void;

/**
 * Extension logs MUST go through pi's UI channel, never bare stderr: pi's TUI
 * does not take over stderr, so a `console.warn` prints straight into the
 * render area at the cursor — i.e. over the input box — corrupting the frame.
 * A handful of them (mem serve unhealthy, mcp respawn, mine/feedback failures)
 * is enough to leave the session unusable. pi's own extension runner follows
 * the same rule: it only falls back to `console.warn` when `!hasUI()`.
 *
 * `getSink` is a getter rather than a stored object because ExtensionContext's
 * properties are lazy getters — holding the ctx and reading it later still
 * yields the UI live at that moment. But reading one after the runner is torn
 * down throws, and logging is called from catch blocks and from detached child
 * `error` handlers, so the whole read is wrapped. With no UI (headless `pi -p`)
 * or a dead runner there is no TUI to corrupt and stderr is the right sink.
 */
export function createLogger(
  getSink: () => LogSink | undefined,
  fallback: (line: string) => void = (line) => console.warn(line),
): LogFn {
  return (msg, level = "info") => {
    const line = `[mem] ${msg}`;
    try {
      const sink = getSink();
      if (sink?.hasUI) {
        sink.ui.notify(line, level);
        return;
      }
    } catch {
      // Reading ctx or notify itself threw — fall through, the message must not be lost.
    }
    fallback(line);
  };
}

/**
 * Formats a caught `unknown` for a log line. Every call site is a `catch (e)`,
 * so this must never be the thing that throws — a hostile `toString` would
 * turn a logged warning into a crash.
 */
export function errText(e: unknown): string {
  if (e instanceof Error) return e.message;
  try {
    return String(e);
  } catch {
    return "<unprintable error>";
  }
}
