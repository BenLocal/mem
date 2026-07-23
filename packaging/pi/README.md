# mem × pi

Install: `pi install ./packaging/pi` (or `pi install <git-source>`).

Requires the `mem` binary on `PATH`. The extension starts `mem serve` if
`MEM_BASE_URL` (default http://127.0.0.1:3000) is down, exposes all mem tools,
injects wake-up + recall context, and mines/gives feedback from pi sessions.

Env: `MEM_BASE_URL`, `MEM_TENANT` (default `local`).
