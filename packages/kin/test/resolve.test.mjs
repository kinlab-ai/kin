// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import {
  packageVersion,
  targetKinVersion,
  compareVersions,
  platformTarget,
  binaryName,
  kinHome,
  managedBinaryPath,
  resolveManagedBinary,
  launcherStampPath,
  readLauncherStamp,
  writeLauncherStamp,
  notProvisionedMessage,
} from '../lib/resolve.mjs';

test('packageVersion reads a semver from the manifest', () => {
  assert.match(packageVersion(), /^\d+\.\d+\.\d+/);
});

test('targetKinVersion tracks the package version', () => {
  assert.equal(targetKinVersion(), packageVersion());
});

test('compareVersions orders numeric cores', () => {
  assert.equal(compareVersions('0.2.0', '0.1.0'), 1);
  assert.equal(compareVersions('1.0.0', '0.9.9'), 1);
  assert.equal(compareVersions('0.1.0', '0.1.0'), 0);
  assert.equal(compareVersions('0.1.0', '0.2.0'), -1);
});

test('compareVersions understands prereleases', () => {
  // A pre-release of a higher core beats a lower stable release.
  assert.equal(compareVersions('0.2.7-alpha.1', '0.2.6'), 1);
  // A newer alpha beats an older alpha of the same core.
  assert.equal(compareVersions('0.2.7-alpha.2', '0.2.7-alpha.1'), 1);
  // A stable release outranks its own pre-release.
  assert.equal(compareVersions('0.2.7', '0.2.7-alpha.9'), 1);
  // A pre-release never outranks its released version (no downgrade).
  assert.equal(compareVersions('0.2.7-alpha.1', '0.2.7'), -1);
  // Equal pre-releases compare equal.
  assert.equal(compareVersions('0.2.7-alpha.1', '0.2.7-alpha.1'), 0);
  // Alphanumeric identifiers rank above numeric; 'beta' > 'alpha' lexically.
  assert.equal(compareVersions('0.2.7-beta', '0.2.7-alpha.1'), 1);
  // A leading 'v' is tolerated on either side.
  assert.equal(compareVersions('v0.2.7', 'v0.2.6'), 1);
});

test('compareVersions treats unparseable segments as 0 instead of throwing', () => {
  assert.equal(compareVersions('unknown', '0.0.0'), 0);
  assert.equal(compareVersions('1.x.0', '1.0.0'), 0);
});

test('compareVersions ignores build metadata for release and prerelease precedence', () => {
  assert.equal(compareVersions('1.2.3+build.2', '1.2.3+build.1'), 0);
  assert.equal(compareVersions('1.2.3-rc.1+build', '1.2.3-rc.1'), 0);
  assert.equal(compareVersions('1.2.3-rc.1', '1.2.3-rc.1+build'), 0);
  assert.equal(compareVersions('1.2.3-rc.1+build.99', '1.2.3-rc.2'), -1);
});

test('platformTarget maps every supported host to a Rust triple', () => {
  assert.equal(platformTarget('darwin', 'arm64'), 'aarch64-apple-darwin');
  assert.equal(platformTarget('darwin', 'x64'), 'x86_64-apple-darwin');
  assert.equal(platformTarget('linux', 'arm64'), 'aarch64-unknown-linux-gnu');
  assert.equal(platformTarget('linux', 'x64'), 'x86_64-unknown-linux-gnu');
  assert.equal(platformTarget('win32', 'x64'), 'x86_64-pc-windows-msvc');
});

test('platformTarget throws loudly on an unsupported host', () => {
  assert.throws(() => platformTarget('sunos', 'sparc'), /unsupported platform/);
});

test('binaryName adds .exe only on Windows', () => {
  assert.equal(binaryName('kin', 'linux'), 'kin');
  assert.equal(binaryName('kin', 'darwin'), 'kin');
  assert.equal(binaryName('kin', 'win32'), 'kin.exe');
});

test('kinHome honors $KIN_HOME then falls back to ~/.kin', () => {
  assert.equal(kinHome({ KIN_HOME: '/custom/kin' }, '/home/u'), '/custom/kin');
  assert.equal(kinHome({}, '/home/u'), path.join('/home/u', '.kin'));
  assert.equal(kinHome({ KIN_HOME: '' }, '/home/u'), path.join('/home/u', '.kin'));
  assert.equal(kinHome({ KIN_HOME: '   ' }, '/home/u'), path.resolve('   '), 'nonempty path spelling matches the shell and native resolver');
});

test('managedBinaryPath defaults under kinHome and is overridable', () => {
  const def = managedBinaryPath('kin', { HOME: '/home/u' }, 'linux');
  assert.ok(def.endsWith(path.join('.kin', 'bin', 'kin')));
  assert.equal(
    managedBinaryPath('kin', { KIN_MANAGED_BIN: '/opt/kin/bin/kin' }, 'linux'),
    '/opt/kin/bin/kin',
  );
});

test('resolveManagedBinary returns null when nothing is installed', () => {
  assert.equal(
    resolveManagedBinary('kin', { KIN_MANAGED_BIN: '/nonexistent/kin-binary-xyz' }, 'linux'),
    null,
  );
});

test('resolveManagedBinary returns the path when the binary exists', () => {
  // Any real file proves existence-resolution without shipping a fake binary.
  const real = process.execPath;
  assert.equal(resolveManagedBinary('kin', { KIN_MANAGED_BIN: real }, process.platform), real);
});

test('launcher stamp round-trips under kinHome and reads null when absent', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-stamp-'));
  const env = { KIN_HOME: home };
  assert.equal(readLauncherStamp(env), null);
  fs.mkdirSync(path.join(home, 'bin'), { recursive: true });
  writeLauncherStamp('9.9.9', env);
  assert.equal(readLauncherStamp(env), '9.9.9');
  assert.ok(launcherStampPath(env).startsWith(home));
  fs.rmSync(home, { recursive: true, force: true });
});

test('launcher stamp is replaced by rename and leaves no temporary file behind', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-stamp-atomic-'));
  try {
    const env = { KIN_HOME: home };
    const bin = path.join(home, 'bin');
    fs.mkdirSync(bin, { recursive: true });
    fs.writeFileSync(launcherStampPath(env), '0.6.6\n');
    const before = fs.statSync(launcherStampPath(env));

    writeLauncherStamp('0.8.7', env);

    // An in-place write keeps the inode; a rename installs a new one, which is
    // what lets a concurrent reader see only the whole old or the whole new
    // stamp.
    const after = fs.statSync(launcherStampPath(env));
    assert.notEqual(after.ino, before.ino);
    assert.equal(fs.readFileSync(launcherStampPath(env), 'utf8'), '0.8.7\n');
    assert.deepEqual(fs.readdirSync(bin), ['.kinlab-kin-version']);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a launcher stamp write that cannot rename removes its temporary file', () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-stamp-refused-'));
  try {
    const env = { KIN_HOME: home };
    const bin = path.join(home, 'bin');
    // A directory where the stamp belongs refuses the rename.
    fs.mkdirSync(path.join(launcherStampPath(env), 'occupied'), { recursive: true });

    assert.throws(() => writeLauncherStamp('0.8.7', env));

    assert.deepEqual(fs.readdirSync(bin), ['.kinlab-kin-version']);
    assert.ok(fs.statSync(launcherStampPath(env)).isDirectory());
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('notProvisionedMessage names the expected path and stays honest', () => {
  const msg = notProvisionedMessage('kin', { KIN_MANAGED_BIN: '/opt/kin/bin/kin' }, 'linux');
  assert.match(msg, /managed "kin" binary not found/);
  assert.match(msg, /\/opt\/kin\/bin\/kin/);
  assert.match(msg, /provisioning failed or was disabled/);
});

test('KIN_DIR is the install-root alias and KIN_HOME takes precedence', () => {
  assert.equal(kinHome({ KIN_DIR: '/legacy-root' }, '/home/fixture'), path.resolve('/legacy-root'));
  assert.equal(kinHome({ KIN_HOME: '/preferred', KIN_DIR: '/legacy-root' }, '/home/fixture'), path.resolve('/preferred'));
  assert.equal(kinHome({ KIN_HOME: '', KIN_DIR: '/legacy-root' }, '/home/fixture'), path.resolve('/legacy-root'));
});
