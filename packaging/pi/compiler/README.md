# mem Skill compiler × pi

Run this package in a dedicated Pi process so the ordinary mem extension is
not loaded into the compiler context:

```bash
pi --no-extensions --no-builtin-tools \
  --extension ./packaging/pi/compiler/compiler-extension.ts \
  --no-prompt-templates \
  --prompt-template ./packaging/pi/compiler/prompts
```

Requires `mem` on `PATH`, a running `mem serve`, and a compiler token supplied
as `MEM_SKILL_COMPILER_TOKEN` or through mem's permission-checked Unix
`$HOME/.mem/config.env` fallback (directory `0700`, file `0600`). It starts
`mem mcp --profile compiler`, registers
exactly six `skill_compiler_*` tools, and never starts review, recall, mining or
feedback hooks. The extension also calls `setActiveTools` on every session and
fails closed unless the active set is exactly those six tools. Do not install
the ordinary `@shibenenen/pi-mem` extension in the same pi Agent context.
