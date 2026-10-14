// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { cachedKinBinary } from '../../kin-mcp/src/index.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const canonicalLauncher = path.resolve(here, '../bin/kin.mjs');
const compatibilityLauncher = path.resolve(here, '../../kin-mcp/bin/kin-mcp.js');
const nativeFixture = `#!/usr/bin/env node
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const cli = process.argv[1];
const daemon = path.join(path.dirname(cli), 'kin-daemon');
const identity = JSON.parse(fs.readFileSync(path.join(path.dirname(cli), 'fixture-identity.json')));
if (process.argv[2] === '--version') { console.log('kin 99.0.0'); process.exit(0); }
if (process.argv[2] === 'runtime') {
  const component = (file, commit) => ({ path: file, version: '99.0.0', commit_sha: commit,
    source_dirty: false, target: 'fixture', build_profile: 'release',
    sha256: crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex') });
  const ready = identity.cli === identity.daemon;
  const development = process.env.KIN_RUNTIME_MODE === 'development';
  console.log(JSON.stringify({ schema: 'kin.runtime-status.v1', ready,
    mode: development ? 'development' : 'managed', kin_home: development ? path.join(process.env.HOME, '.kin-development-fixture') : path.dirname(path.dirname(cli)),
    cli: component(cli, identity.cli), daemon: component(daemon, identity.daemon),
    pair_matches: ready, channel: development ? 'development' : 'stable',
    release_kind: development ? 'development' : 'published', restart_needed: false,
    reason: ready ? null : 'pair_source_mismatch', detail: null }));
  process.exit(ready ? 0 : 1);
}
console.log(JSON.stringify({ selected: cli, mode: process.env.KIN_RUNTIME_MODE || 'managed', argv: process.argv.slice(2) }));
`;

function fixture(t) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-routing-boundary-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  const root = path.join(home, 'managed root');
  fs.mkdirSync(path.join(root, 'bin'), { recursive: true });
  const cli = path.join(root, 'bin', 'kin');
  fs.writeFileSync(cli, nativeFixture, { mode: 0o755 });
  fs.writeFileSync(path.join(root, 'bin', 'kin-daemon'), 'fixture daemon', { mode: 0o755 });
  const identityPath = path.join(root, 'bin', 'fixture-identity.json');
  fs.writeFileSync(identityPath, JSON.stringify({ cli: 'a'.repeat(40), daemon: 'a'.repeat(40) }));
  const env = { ...process.env, HOME: home, USERPROFILE: home, KIN_HOME: root };
  for (const key of ['KIN_MANAGED_BIN', 'KIN_MCP_KIN_BINARY', 'KIN_BINARY_PATH', 'KIN_MCP_CACHE_DIR', 'KIN_RUNTIME_MODE', 'KIN_LAUNCHER_ADOPT', 'KIN_NO_PROVISION']) delete env[key];
  return { home, root, cli, identityPath, env };
}

test('ordinary canonical and legacy launchers select the same managed runtime', { skip: process.platform === 'win32' }, t => {
  const { env, cli } = fixture(t);
  const ordinary = spawnSync(process.execPath, [canonicalLauncher, 'status'], { env, encoding: 'utf8' });
  assert.equal(ordinary.status, 0, ordinary.stderr);
  assert.equal(JSON.parse(ordinary.stdout).selected, cli);
  const legacy = spawnSync(process.execPath, [compatibilityLauncher, '--print-bin'], { env, encoding: 'utf8' });
  assert.equal(legacy.status, 0, legacy.stderr);
  assert.equal(legacy.stdout.trim(), cli);
});

test('KIN_DIR alone selects that same pair in both launchers', { skip: process.platform === 'win32' }, t => {
  const { env, root, cli } = fixture(t);
  delete env.KIN_HOME;
  env.KIN_DIR = root;
  for (const [launcher, args] of [[canonicalLauncher, ['status']], [compatibilityLauncher, ['--print-bin']]]) {
    const result = spawnSync(process.execPath, [launcher, ...args], { env, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
    assert.equal(launcher === canonicalLauncher ? JSON.parse(result.stdout).selected : result.stdout.trim(), cli);
  }
});

test('same-version differing source identities refuse both normal entry points', { skip: process.platform === 'win32' }, t => {
  const { env, identityPath } = fixture(t);
  fs.writeFileSync(identityPath, JSON.stringify({ cli: 'a'.repeat(40), daemon: 'b'.repeat(40) }));
  for (const [launcher, args] of [[canonicalLauncher, ['status']], [compatibilityLauncher, ['--print-bin']]]) {
    const result = spawnSync(process.execPath, [launcher, ...args], { env, encoding: 'utf8' });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /not certified/);
    assert.equal(result.stdout, '', 'an unaccepted runtime cannot execute the requested command');
  }
});

test('ordinary legacy lookup ignores retained versioned caches', async t => {
  const { env, home } = fixture(t);
  const cache = path.join(home, 'Library', 'Caches', 'kin-mcp', 'v99.0.0', 'kin-macos-aarch64');
  fs.mkdirSync(cache, { recursive: true });
  for (const name of ['kin', 'kin-daemon']) fs.writeFileSync(path.join(cache, name), 'retained legacy bytes', { mode: 0o755 });
  fs.rmSync(path.join(env.KIN_HOME, 'bin'), { recursive: true });
  assert.equal(await cachedKinBinary({ env, homeDir: home }), null);
  assert.equal(fs.readFileSync(path.join(cache, 'kin'), 'utf8'), 'retained legacy bytes');
});

test('an explicit development launcher enters the isolated lane without updating managed files', { skip: process.platform === 'win32' }, t => {
  const { env, cli } = fixture(t);
  env.KIN_MANAGED_BIN = cli;
  const result = spawnSync(process.execPath, [canonicalLauncher, 'status'], { env, encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(JSON.parse(result.stdout).mode, 'development');
  assert.equal(fs.readFileSync(cli, 'utf8'), nativeFixture);
});

test('the Unix installer cannot replace an existing pair outside its native transaction', { skip: process.platform === 'win32' }, () => {
  const script = path.resolve(here, '../../../scripts/test-managed-installer-routing.py');
  const result = spawnSync('python3', [script], { encoding: 'utf8' });
  assert.equal(result.status, 0, result.stderr);
});
