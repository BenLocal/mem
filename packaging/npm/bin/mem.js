#!/usr/bin/env node
'use strict';

// Thin launcher: exec the native `mem` binary that install.js fetched into
// this directory, forwarding argv, stdio, and the exit status. Committed to
// the package (unlike the native binary) so `bin` resolves even before the
// postinstall download runs.

const path = require('path');
const fs = require('fs');
const { spawnSync } = require('child_process');

const TARGETS = { 'linux-x64': 'mem-linux-x64' };
const name = TARGETS[`${process.platform}-${process.arch}`];
const binPath = name && path.join(__dirname, name);

if (!binPath || !fs.existsSync(binPath)) {
  console.error(
    '[@shibenenen/mem] native binary missing — the postinstall download may ' +
      'have failed or been skipped (e.g. --ignore-scripts). Reinstall the ' +
      'package, or run `node ' +
      path.join(__dirname, '..', 'install.js') +
      '`.'
  );
  process.exit(1);
}

const res = spawnSync(binPath, process.argv.slice(2), { stdio: 'inherit' });
if (res.error) {
  console.error(`[@shibenenen/mem] ${res.error.message}`);
  process.exit(1);
}
// Propagate signal-kills as the conventional 128+signal code.
if (res.signal) {
  process.exit(1);
}
process.exit(res.status == null ? 1 : res.status);
