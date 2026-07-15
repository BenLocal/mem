# @shibenenen/mem

npm installer for [`mem`](https://github.com/BenLocal/mem) — a local-first Rust
memory service for multi-agent workflows.

This package ships **no binary of its own**. On install it downloads the
prebuilt `mem` binary for your platform from the matching
[GitHub Release](https://github.com/BenLocal/mem/releases), verifies its
SHA-256 against the release checksums, and installs it as the `mem` command.

## Install

```bash
npm install -g @shibenenen/mem
mem --help
```

or run without a global install:

```bash
npx @shibenenen/mem serve
```

## Platform support

Prebuilt binaries are currently published for **linux-x64 only**. The installer
pulls the statically-linked **musl** build, so it runs on any linux-x64 distro
(glibc or musl) without further setup. On any other platform `npm install`
fails fast (`os`/`cpu` are restricted in `package.json`) — build from
[source](https://github.com/BenLocal/mem) instead.

## How it works

- `install.js` (postinstall) resolves `mem-v<version>-x86_64-unknown-linux-musl`
  from `releases/download/v<version>/`, streams it to `bin/mem-linux-x64` while
  hashing, and aborts on any SHA-256 mismatch — no unverified binary is ever
  left on disk.
- `bin/mem.js` is a small launcher that execs that binary, forwarding argv,
  stdio, and exit status.

The package `version` must match a published release tag, since that is where
the binary is fetched from. In CI the version is set from the git tag at
publish time (see `.github/workflows/release.yml`).

If you installed with `--ignore-scripts`, the download never ran — re-run it
manually with `node install.js` inside the package directory.

## License

MIT
