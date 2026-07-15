#!/usr/bin/env node
'use strict';

// postinstall: fetch the prebuilt `mem` binary matching this package's
// version from the GitHub Release, verify its SHA-256 against the release's
// SHA256SUMS, and drop it next to the shim in bin/. No binary is shipped in
// the npm tarball — it is downloaded here, once, at install time.
//
// Only linux-x64 is published today (release.yml builds gnu + musl for that
// target only). We pull the MUSL build: it is statically linked, so it runs
// on any linux-x64 distro (glibc or musl) without a libc-version guess.

const fs = require('fs');
const path = require('path');
const https = require('https');
const crypto = require('crypto');

const pkg = require('./package.json');
const REPO = 'BenLocal/mem';
const VERSION = pkg.version;
const TAG = `v${VERSION}`;

// process.platform-process.arch -> { asset (release filename), out (local name) }
const TARGETS = {
  'linux-x64': {
    asset: `mem-${TAG}-x86_64-unknown-linux-musl`,
    out: 'mem-linux-x64',
  },
};

function log(msg) {
  console.log(`[@shibenenen/mem] ${msg}`);
}
function die(msg) {
  console.error(`[@shibenenen/mem] ${msg}`);
  process.exit(1);
}

const platformKey = `${process.platform}-${process.arch}`;
const target = TARGETS[platformKey];
if (!target) {
  die(
    `no prebuilt binary for ${platformKey}. Prebuilt binaries are published ` +
      `for linux-x64 only; build from source instead: https://github.com/${REPO}`
  );
}

const base = `https://github.com/${REPO}/releases/download/${TAG}`;
const assetUrl = `${base}/${target.asset}`;
const sumsUrl = `${base}/mem-${TAG}-SHA256SUMS`;
const binDir = path.join(__dirname, 'bin');
const outPath = path.join(binDir, target.out);

// Follow up to 5 redirects (GitHub release assets 302 to a CDN host).
function get(url, redirectsLeft, cb) {
  https
    .get(url, { headers: { 'user-agent': 'shibenenen-mem-npm-installer' } }, (res) => {
      const { statusCode, headers } = res;
      if (statusCode >= 300 && statusCode < 400 && headers.location) {
        if (redirectsLeft <= 0) {
          res.resume();
          return cb(new Error(`too many redirects for ${url}`));
        }
        res.resume();
        return get(headers.location, redirectsLeft - 1, cb);
      }
      if (statusCode !== 200) {
        res.resume();
        return cb(new Error(`HTTP ${statusCode} for ${url}`));
      }
      cb(null, res);
    })
    .on('error', cb);
}

function fetchText(url) {
  return new Promise((resolve, reject) => {
    get(url, 5, (err, res) => {
      if (err) return reject(err);
      let data = '';
      res.setEncoding('utf8');
      res.on('data', (c) => (data += c));
      res.on('end', () => resolve(data));
      res.on('error', reject);
    });
  });
}

// Stream the asset to disk while hashing it, so we never hold the whole
// binary in memory. Resolves with the hex sha256 of the bytes written.
function downloadToFile(url, dest) {
  return new Promise((resolve, reject) => {
    get(url, 5, (err, res) => {
      if (err) return reject(err);
      const hash = crypto.createHash('sha256');
      const file = fs.createWriteStream(dest, { mode: 0o755 });
      res.on('data', (chunk) => hash.update(chunk));
      res.pipe(file);
      file.on('finish', () => file.close(() => resolve(hash.digest('hex'))));
      file.on('error', (e) => {
        fs.unlink(dest, () => reject(e));
      });
      res.on('error', (e) => {
        fs.unlink(dest, () => reject(e));
      });
    });
  });
}

// Parse `sha256sum`-style output: "<hex>  <filename>" per line.
function expectedSumFor(sumsText, assetName) {
  for (const line of sumsText.split('\n')) {
    const m = line.trim().match(/^([0-9a-fA-F]{64})\s+\*?(.+)$/);
    if (m && path.basename(m[2]) === assetName) return m[1].toLowerCase();
  }
  return null;
}

async function main() {
  // Idempotent: a prior successful install leaves the binary in place.
  if (fs.existsSync(outPath) && fs.statSync(outPath).size > 0) {
    log(`binary already present (${target.out}); skipping download.`);
    return;
  }
  fs.mkdirSync(binDir, { recursive: true });

  log(`downloading ${target.asset} (${TAG})...`);
  const sumsText = await fetchText(sumsUrl);
  const expected = expectedSumFor(sumsText, target.asset);
  if (!expected) {
    die(`no SHA-256 entry for ${target.asset} in the release checksums.`);
  }

  const actual = await downloadToFile(assetUrl, outPath);
  if (actual !== expected) {
    fs.unlinkSync(outPath);
    die(
      `checksum mismatch for ${target.asset}\n  expected ${expected}\n  actual   ${actual}`
    );
  }
  fs.chmodSync(outPath, 0o755);
  log(`installed ${target.out} (sha256 ok).`);
}

main().catch((e) => {
  // Never leave a partial/unverified binary behind.
  try {
    if (fs.existsSync(outPath)) fs.unlinkSync(outPath);
  } catch (_) {
    /* ignore */
  }
  die(`install failed: ${e.message}`);
});
