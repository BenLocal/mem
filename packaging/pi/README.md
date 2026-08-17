# mem × pi

Install: `pi install ./packaging/pi` (or `pi install <git-source>`).

Requires the `mem` binary on `PATH`. The extension starts `mem serve` if
`MEM_BASE_URL` (default http://127.0.0.1:3000) is down, exposes all mem tools,
injects wake-up + recall context, and mines/gives feedback from pi sessions.

Env: `MEM_BASE_URL`, `MEM_TENANT` (default `local`).

For Agent-as-Compiler, install the separate `./compiler` package in a dedicated
pi session. It launches `mem mcp --profile compiler` and exposes compiler tools
only; never combine it with this ordinary memory/review extension in one Agent
context.
