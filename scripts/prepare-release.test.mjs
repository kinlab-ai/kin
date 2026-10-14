// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import {
  removeChangelogSection,
  stampServerVersion,
  stampNpmVersion,
  stampCompatibilityNpmLock,
  updateWorkspaceLock,
  upsertChangelogSection,
} from './prepare-release.mjs';

test('compatibility package upgrades cannot retain an older canonical resolver dependency', () => {
  const source = {
    name: '@kinlab/kin-mcp', version: '0.8.6',
    dependencies: { '@kinlab/kin': '0.8.6', preserved: '2.0.0' },
  };
  const upgraded = stampNpmVersion(source, '0.8.6', '0.8.7');
  assert.equal(upgraded.version, '0.8.7');
  assert.equal(upgraded.dependencies['@kinlab/kin'], '0.8.7');
  assert.equal(upgraded.dependencies.preserved, '2.0.0');
  assert.equal(source.dependencies['@kinlab/kin'], '0.8.6');
  assert.deepEqual(stampNpmVersion(upgraded, '0.8.6', '0.8.7'), upgraded);
});

test('an already bumped compatibility manifest repairs its stale resolver pin', () => {
  const manifest = { name: '@kinlab/kin-mcp', version: '0.8.7', dependencies: { '@kinlab/kin': '0.8.6' } };
  assert.equal(stampNpmVersion(manifest, '0.8.6', '0.8.7').dependencies['@kinlab/kin'], '0.8.7');
  assert.throws(() => stampNpmVersion({ ...manifest, version: 'unrelated' }, '0.8.6', '0.8.7'), /expected/);
});

test('compatibility source lock advances both manifests while preserving the local development link', () => {
  const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
  const source = JSON.parse(fs.readFileSync(path.join(root, 'packages/kin-mcp/package-lock.json'), 'utf8'));
  const from = source.version;
  const to = '99.0.0';
  const updated = stampCompatibilityNpmLock(source, from, to);
  assert.equal(updated.version, to);
  assert.equal(updated.packages[''].version, to);
  assert.equal(updated.packages[''].dependencies['@kinlab/kin'], to);
  assert.equal(updated.packages['../kin'].version, to);
  assert.equal(updated.packages[''].devDependencies['@kinlab/kin'], 'file:../kin');
  assert.deepEqual(updated.packages['node_modules/@kinlab/kin'], source.packages['node_modules/@kinlab/kin']);
  assert.equal(updated.dependencies, undefined, 'v3 lock must not gain invented registry dependency records');
  assert.equal(source.version, from);
  assert.deepEqual(stampCompatibilityNpmLock(updated, from, to), updated);
});

test('compatibility lock stamping refuses registry-byte claims and unrelated local versions', () => {
  const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
  const source = JSON.parse(fs.readFileSync(path.join(root, 'packages/kin-mcp/package-lock.json'), 'utf8'));
  for (const property of ['integrity', 'resolved']) {
    const changed = structuredClone(source);
    changed.packages['../kin'][property] = 'untrusted-registry-claim';
    assert.throws(() => stampCompatibilityNpmLock(changed, source.version, '99.0.0'), /generated local/);
  }
  const changed = structuredClone(source);
  changed.packages['../kin'].version = 'unrelated';
  assert.throws(() => stampCompatibilityNpmLock(changed, source.version, '99.0.0'), /expected/);
});

test('a future source version installs offline through the generated lock before registry publication', () => {
  const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
  const canonical = path.join(root, 'packages/kin');
  const compatibility = path.join(root, 'packages/kin-mcp');
  const lock = JSON.parse(fs.readFileSync(path.join(compatibility, 'package-lock.json'), 'utf8'));
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-source-lock-'));
  const sibling = path.join(fixture, 'kin');
  const project = path.join(fixture, 'kin-mcp');
  const version = '99.0.0';
  try {
    fs.mkdirSync(sibling);
    fs.mkdirSync(project);
    for (const directory of ['bin', 'lib']) fs.cpSync(path.join(canonical, directory), path.join(sibling, directory), { recursive: true });
    for (const [source, destination] of [[canonical, sibling], [compatibility, project]]) {
      const manifest = JSON.parse(fs.readFileSync(path.join(source, 'package.json'), 'utf8'));
      fs.writeFileSync(path.join(destination, 'package.json'), JSON.stringify(stampNpmVersion(manifest, lock.version, version)));
    }
    fs.writeFileSync(path.join(project, 'package-lock.json'), JSON.stringify(stampCompatibilityNpmLock(lock, lock.version, version)));
    const env = { ...process.env, npm_config_cache: path.join(fixture, 'cache'), npm_config_registry: 'http://127.0.0.1:9' };
    execFileSync(process.platform === 'win32' ? 'npm.cmd' : 'npm', ['ci', '--ignore-scripts', '--no-audit', '--no-fund', '--offline'], {
      cwd: project, env, encoding: 'utf8', shell: process.platform === 'win32',
    });
    assert.equal(fs.realpathSync(path.join(project, 'node_modules/@kinlab/kin')), fs.realpathSync(sibling));
    const output = execFileSync(process.execPath, ['--input-type=module', '--eval',
      "import { targetKinVersion } from '@kinlab/kin/lib/resolve.mjs'; process.stdout.write(targetKinVersion());"], {
      cwd: project, env, encoding: 'utf8',
    });
    assert.equal(output, version);
    const installed = JSON.parse(fs.readFileSync(path.join(project, 'package.json'), 'utf8'));
    assert.equal(installed.dependencies['@kinlab/kin'], version, 'published dependency remains an exact registry version');
  } finally { fs.rmSync(fixture, { recursive: true, force: true }); }
});

test('updates only local Kin workspace packages in Cargo.lock', () => {
  const source = `version = 4

[[package]]
name = "kin-cli"
version = "0.3.6"

[[package]]
name = "async-stream"
version = "0.3.6"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "keep"

[[package]]
name = "local-helper"
version = "0.3.6"
`;
  const result = updateWorkspaceLock(source, '0.3.6', '0.4.0');
  assert.equal(result.replacements, 1);
  assert.match(result.lock, /name = "kin-cli"\nversion = "0.4.0"/);
  assert.match(result.lock, /name = "async-stream"\nversion = "0.3.6"/);
  assert.match(result.lock, /name = "local-helper"\nversion = "0.3.6"/);
  assert.match(result.lock, /checksum = "keep"/);
});

test('validates an already prepared workspace lock without double counting', () => {
  const source = `version = 4

[[package]]
name = "kin-cli"
version = "0.4.0"

[[package]]
name = "kin-core"
version = "0.4.0"
`;
  const result = updateWorkspaceLock(source, '0.4.0', '0.4.0');
  assert.equal(result.replacements, 0);
  assert.equal(result.targetEntries, 2);
  assert.equal(result.lock, source);
});

test('inserts a nonempty release section after Unreleased', () => {
  const changelog = '# Changelog\n\n## [Unreleased]\n\n## [0.3.6] - 2026-07-26\n\n- old\n';
  const updated = upsertChangelogSection(
    changelog,
    '0.4.0',
    '2026-07-28',
    ['Add repository authority (#475)'],
  );
  assert.match(
    updated,
    /## \[Unreleased\]\n\n## \[0\.4\.0\] - 2026-07-28\n\n### Changed\n\n- Add repository authority \(#475\)/,
  );
  assert.match(updated, /## \[0\.3\.6\] - 2026-07-26/);
});

test('replaces an existing generated section when the train coalesces', () => {
  const changelog = [
    '# Changelog',
    '',
    '## [Unreleased]',
    '',
    '## [0.4.0] - 2026-07-28',
    '',
    '### Changed',
    '',
    '- old note',
    '',
    '## [0.3.6] - 2026-07-26',
    '',
    '- prior',
    '',
  ].join('\n');
  const updated = upsertChangelogSection(
    changelog,
    '0.4.0',
    '2026-07-29',
    ['new note', 'another note'],
  );
  assert.doesNotMatch(updated, /old note/);
  assert.match(updated, /## \[0\.4\.0\] - 2026-07-29/);
  assert.match(updated, /- another note/);
  assert.equal((updated.match(/## \[0\.4\.0\]/g) ?? []).length, 1);
});

test('removes a superseded generated section when bump intent escalates', () => {
  const changelog = [
    '# Changelog',
    '',
    '## [Unreleased]',
    '',
    '## [0.3.7] - 2026-07-28',
    '',
    '### Changed',
    '',
    '- stale patch train',
    '',
    '## [0.3.6] - 2026-07-26',
    '',
    '- prior',
    '',
  ].join('\n');
  const updated = removeChangelogSection(changelog, '0.3.7');
  assert.doesNotMatch(updated, /0\.3\.7|stale patch train/);
  assert.match(updated, /## \[0\.3\.6\] - 2026-07-26/);
});

test('the fuzz lockfile moves with the workspace version', () => {
  const source = `version = 4

[[package]]
name = "kin-parser"
version = "0.3.6"

[[package]]
name = "kin-parser-fuzz"
version = "0.0.0"

[[package]]
name = "kin-model"
version = "0.7.1"
source = "sparse+https://example.invalid/"
checksum = "keep"
`;
  const result = updateWorkspaceLock(source, '0.3.6', '0.4.0');
  assert.equal(result.replacements, 1);
  assert.match(result.lock, /name = "kin-parser"\nversion = "0.4.0"/);
  // A fuzz target pinned at 0.0.0 and a registry dependency are untouched.
  assert.match(result.lock, /name = "kin-parser-fuzz"\nversion = "0.0.0"/);
  assert.match(result.lock, /name = "kin-model"\nversion = "0.7.1"/);
});

test('the generator runs from a copy reached through a symlinked directory', () => {
  // release-train.yml runs this file from a copy in $RUNNER_TEMP/release-policy.
  // The entry-point test used to compare `import.meta.url` against
  // `pathToFileURL(process.argv[1])`, and Node resolves symlinks for the first
  // and not the second, so a copy invoked through one generated nothing, wrote
  // no $GITHUB_OUTPUT, and exited 0. The train would then read an empty version.
  //
  // Run with no arguments and no repository, so `main()` is reached and fails on
  // its own missing input. The distinction being drawn is between a process that
  // ran and one that never started, and only the second is silent.
  const generator = path.join(
    path.dirname(fileURLToPath(import.meta.url)),
    'prepare-release.mjs',
  );
  const real = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-prepare-policy-'));
  const link = `${real}-link`;
  fs.symlinkSync(real, link, 'dir');
  const policy = path.join(link, 'prepare-release.mjs');
  fs.copyFileSync(generator, policy);
  // The sibling this file imports has to come too, exactly as release-train.yml
  // copies all three policy scripts into one directory. Without it node fails on
  // module resolution and writes to stderr, and this test would then pass on
  // that error no matter what the entry point did.
  fs.copyFileSync(
    path.join(path.dirname(generator), 'check-release-version.mjs'),
    path.join(link, 'check-release-version.mjs'),
  );
  const empty = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-prepare-cwd-'));

  let output = '';
  try {
    output = execFileSync(process.execPath, [policy], {
      cwd: empty,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
  } catch (error) {
    output = `${error.stdout ?? ''}${error.stderr ?? ''}`;
  }
  assert.notEqual(output, '', 'the generator produced no output, so it never ran');
});

test('the MCP Registry entry moves with the release', () => {
  const server = {
    name: 'ai.kinlab/kin',
    version: '0.7.21',
    packages: [{ identifier: '@kinlab/kin', version: '0.7.21', transport: { type: 'stdio' } }],
  };
  const stamped = stampServerVersion(server, '0.7.21', '0.8.0');
  assert.equal(stamped.version, '0.8.0');
  assert.equal(stamped.packages[0].version, '0.8.0');
  assert.equal(stamped.packages[0].identifier, '@kinlab/kin');
  assert.equal(stampServerVersion(stamped, '0.7.21', '0.8.0'), stamped, 'already stamped is left alone');
  assert.throws(
    () => stampServerVersion({ ...server, version: '0.5.27' }, '0.7.21', '0.8.0'),
    /server.json names 0.5.27, 0.7.21/,
  );
});

test('the checked-in MCP Registry entry names the workspace version', () => {
  const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
  const cargo = fs.readFileSync(path.join(root, 'Cargo.toml'), 'utf8');
  const workspace = /\[workspace\.package\][^[]*?\nversion = "([^"]+)"/.exec(cargo)?.[1];
  const server = JSON.parse(fs.readFileSync(path.join(root, 'server.json'), 'utf8'));
  assert.ok(workspace, 'Cargo.toml names no workspace version');
  assert.equal(server.version, workspace);
  for (const pkg of server.packages) {
    assert.equal(pkg.version, workspace, pkg.identifier);
  }
  // The entry installs the canonical package and runs its MCP server, and that
  // package carries the registry name the registry validates it against.
  const canonical = JSON.parse(fs.readFileSync(path.join(root, 'packages/kin/package.json'), 'utf8'));
  assert.equal(server.packages[0].identifier, canonical.name);
  assert.equal(canonical.mcpName, server.name);
  assert.deepEqual(
    server.packages[0].packageArguments.map((argument) => argument.value),
    ['mcp', 'start'],
  );
});
