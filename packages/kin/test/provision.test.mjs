// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';

import {
  archiveExtraction,
  extractorEnvironment,
  artifactName,
  releaseDownloadUrl,
  parseSha256File,
  sha256Hex,
  formatByteCount,
  formatDownloadProgress,
  createDownloadProgress,
  downloadToFile,
  provision,
  probeBinaryVersion,
  ensureProvisioned,
  resolveReadyManagedBinary,
  probeManagedRuntime,
  developmentLaunchEnvironment,
  updateManagedInstall,
  bootstrapPublishedInstall,
  resolvePublishedRelease,
  resolveLatestPublishedRelease,
  releaseRustTarget,
  isTruthyEnv,
  persistentPathAdvice,
  setupFollows,
} from '../lib/provision.mjs';
import { binaryName, readLauncherStamp, writeLauncherStamp } from '../lib/resolve.mjs';

// The native Windows job must execute the package suite, but NTFS does not
// expose the POSIX execute bits required to falsify a macOS app bundle. Those
// bundle-only assertions remain authoritative on macOS/Linux; Windows runs the
// native ZIP path and all platform-neutral provisioning policy below.
const macArchiveModeTest = process.platform === 'win32' ? test.skip : test;

// The archive layout is a property of the target; the extractor is a property
// of the host. Reading `process.platform` inside the target branch conflated
// them, and the native Windows leg went red on three tests that unpack a darwin
// archive on a Windows runner, where /usr/bin/tar does not exist. All four
// combinations are asserted here rather than only the two a single runner can
// reach, so neither leg has to be the place this is discovered.
test('the extractor is chosen by the host and the layout by the target', () => {
  const winEnv = { SystemRoot: 'C:\\Windows' };
  const sys32Tar = path.win32.join('C:\\Windows', 'System32', 'tar.exe');

  assert.deepEqual(archiveExtraction('win32', winEnv, 'a.zip', 'win32'), {
    executable: sys32Tar,
    args: ['-xf', 'a.zip', '-C', '.'],
  });
  assert.deepEqual(archiveExtraction('win32', {}, 'a.zip', 'darwin'), {
    executable: '/usr/bin/unzip',
    args: ['-q', 'a.zip', '-d', '.'],
  });

  // Unix target on a Windows host: the cross-target case, which must not reach
  // for /usr/bin.
  assert.deepEqual(archiveExtraction('darwin', winEnv, 'a.tar.gz', 'win32'), {
    executable: sys32Tar,
    args: ['-xf', 'a.tar.gz', '-C', '.'],
  });

  if (process.platform !== 'win32') {
    const unix = archiveExtraction('darwin', {}, 'a.tar.gz', 'darwin');
    assert.ok(
      unix.executable === '/usr/bin/tar' || unix.executable === '/bin/tar',
      `expected an absolute system tar, got ${unix.executable}`,
    );
    assert.deepEqual(unix.args, ['-xf', 'a.tar.gz', '-C', '.']);
  }
});

// GNU tar starts gzip through PATH. An empty or unusual PATH once broke
// extraction on a Linux runner ("gzip: Cannot exec"), and a gzip earlier on
// PATH would run in the system one's place, so the trusted directories lead.
test('the extractor runs with the trusted system directories leading PATH', () => {
  assert.equal(extractorEnvironment({ PATH: '' }, 'linux').PATH, '/usr/bin:/bin');
  assert.equal(extractorEnvironment({}, 'darwin').PATH, '/usr/bin:/bin');
  assert.equal(
    extractorEnvironment({ PATH: '/opt/evil:/usr/bin:/home/me/bin', HOME: '/h' }, 'linux').PATH,
    '/usr/bin:/bin:/opt/evil:/home/me/bin',
  );
  assert.equal(extractorEnvironment({ PATH: '/x', HOME: '/h' }, 'linux').HOME, '/h');
  const winEnv = { Path: 'C:\\Windows\\System32' };
  assert.equal(extractorEnvironment(winEnv, 'win32'), winEnv);
});

test('artifactName maps every released host and matches release.yml naming', () => {
  assert.equal(artifactName('darwin', 'arm64'), 'kin-macos-aarch64.tar.gz');
  assert.equal(artifactName('darwin', 'x64'), 'kin-macos-x86_64.tar.gz');
  assert.equal(artifactName('linux', 'arm64'), 'kin-linux-aarch64.tar.gz');
  assert.equal(artifactName('linux', 'x64'), 'kin-linux-x86_64.tar.gz');
  assert.equal(artifactName('win32', 'x64'), 'kin-windows-x86_64.zip');
});

test('launcher booleans accept the complete generated env-contract vocabulary', () => {
  for (const token of ['1', 'true', 'TRUE', 'TrUe', 'yes', 'YES', 'on', 'ON', ' on ']) {
    assert.equal(isTruthyEnv(token), true, token);
  }
  for (const token of ['', '0', 'false', 'no', 'off', 'truthy']) {
    assert.equal(isTruthyEnv(token), false, token);
  }
});

test('artifactName is honest about windows-aarch64 having no artifact', () => {
  assert.throws(() => artifactName('win32', 'arm64'), /no native windows-aarch64/);
});

test('releaseDownloadUrl pins the tag and file', () => {
  assert.equal(
    releaseDownloadUrl('1.2.3', 'kin-macos-aarch64.tar.gz'),
    'https://github.com/kinlab-ai/kin/releases/download/v1.2.3/kin-macos-aarch64.tar.gz',
  );
});

test('parseSha256File accepts shasum output and rejects junk', () => {
  const hex = 'a'.repeat(64);
  assert.equal(parseSha256File(`${hex}  kin-macos-aarch64.tar.gz\n`), hex);
  assert.equal(parseSha256File(`${hex.toUpperCase()}  x`), hex);
  assert.throws(() => parseSha256File('not-a-digest  file'), /malformed \.sha256/);
  assert.throws(() => parseSha256File(''), /malformed \.sha256/);
});

test('download progress reports stable bytes and percent', () => {
  assert.equal(formatByteCount(0), '0 B');
  assert.equal(formatByteCount(1536), '1.5 KiB');
  assert.equal(
    formatDownloadProgress('kin-test.tar.gz', 524288, 1048576),
    'kin: downloading kin-test.tar.gz: 50% (512.0 KiB / 1.0 MiB)',
  );
  assert.equal(
    formatDownloadProgress('kin-test.tar.gz', 1536),
    'kin: downloading kin-test.tar.gz: 1.5 KiB received',
  );
});

test('downloadToFile writes each chunk before pulling the next and never buffers the archive', async () => {
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-download-bounded-'));
  try {
    const destination = path.join(work, 'archive.tar.gz');
    const chunkSize = 64 * 1024;
    const chunkCount = 16;
    const expectedHash = crypto.createHash('sha256');
    const fetchImpl = async () => ({
      ok: true,
      status: 200,
      headers: { get: () => String(chunkSize * chunkCount) },
      body: (async function* boundedFixture() {
        for (let index = 0; index < chunkCount; index += 1) {
          const stagedBytes = fs.existsSync(destination) ? fs.statSync(destination).size : 0;
          assert.equal(
            stagedBytes,
            index * chunkSize,
            'the prior chunk must be flushed before the next pull',
          );
          const chunk = Buffer.alloc(chunkSize, index);
          expectedHash.update(chunk);
          yield chunk;
        }
      })(),
      arrayBuffer: async () => {
        throw new Error('streaming archive must not be accumulated through arrayBuffer()');
      },
    });

    const originalBufferFrom = Buffer.from;
    let result;
    try {
      Buffer.from = function rejectStreamChunkCopy(value, ...args) {
        if (ArrayBuffer.isView(value)) {
          throw new Error('stream chunks must be staged without an archive-sized copy');
        }
        return Reflect.apply(originalBufferFrom, this, [value, ...args]);
      };
      result = await downloadToFile('https://example.test/archive', destination, fetchImpl);
    } finally {
      Buffer.from = originalBufferFrom;
    }
    assert.equal(result.bytes, chunkSize * chunkCount);
    assert.equal(result.sha256, expectedHash.digest('hex'));
    assert.equal(fs.statSync(destination).size, chunkSize * chunkCount);
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
});

// ── offline provision fixture ──────────────────────────────────────────────
//
// Builds a real tar.gz in a tempdir (via the same `tar` the provisioner uses)
// containing the archive layout install.sh documents: a kin-* subdirectory
// with kin + kin-daemon. The fake fetch serves those bytes; no network.

function environmentValue(env, name) {
  const key = Object.keys(env).find(
    (candidate) => candidate.toLowerCase() === name.toLowerCase(),
  );
  return key === undefined ? undefined : env[key];
}

function windowsSystemTarPath(env = process.env) {
  const systemRoot = environmentValue(env, 'SystemRoot');
  assert.ok(systemRoot, 'native Windows ZIP fixtures require SystemRoot');
  return path.win32.join(systemRoot, 'System32', 'tar.exe');
}

function environmentWithHostileTar(work, overrides = {}) {
  const hostileBin = path.join(work, 'hostile-path');
  fs.mkdirSync(hostileBin, { recursive: true });
  const hostileTar = path.join(hostileBin, process.platform === 'win32' ? 'tar.exe' : 'tar');
  if (process.platform === 'win32') {
    fs.copyFileSync(process.execPath, hostileTar);
  } else {
    fs.writeFileSync(hostileTar, '#!/bin/sh\nexit 97\n', { mode: 0o755 });
  }

  const env = { ...process.env, ...overrides };
  const originalPath = environmentValue(env, 'PATH') || '';
  for (const name of Object.keys(env)) {
    if (name.toLowerCase() === 'path') delete env[name];
  }
  env.PATH = [hostileBin, originalPath].filter(Boolean).join(path.delimiter);
  return env;
}

function makeFixture({
  withDaemon = true,
  streamArchive = false,
  notifier = 'complete',
  notifierBody = 'notifier-v1',
  kinScript = '#!/bin/sh\necho "kin 9.9.9 (test)"\n',
} = {}) {
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-prov-fixture-'));
  const stage = path.join(work, 'kin-macos-aarch64');
  fs.mkdirSync(stage, { recursive: true });
  fs.writeFileSync(path.join(stage, 'kin'), kinScript);
  if (withDaemon) {
    fs.writeFileSync(path.join(stage, 'kin-daemon'), '#!/bin/sh\necho daemon\n');
  }
  if (notifier !== 'absent') {
    const bundle = path.join(stage, 'KinNotifier.app');
    fs.mkdirSync(path.join(bundle, 'Contents', 'MacOS'), { recursive: true });
    fs.mkdirSync(path.join(bundle, 'Contents', 'Resources'), { recursive: true });
    const executable = path.join(bundle, 'Contents', 'MacOS', 'KinNotifier');
    fs.writeFileSync(executable, `#!/bin/sh\necho ${notifierBody}\n`);
    fs.chmodSync(executable, 0o755);
    fs.writeFileSync(path.join(bundle, 'Contents', 'Resources', 'Kin.icns'), 'icns');
    if (notifier === 'complete') {
      fs.writeFileSync(path.join(bundle, 'Contents', 'Info.plist'), '<plist/>');
    }
  }
  const archivePath = path.join(work, 'kin-macos-aarch64.tar.gz');
  const tar = spawnSync('tar', ['-czf', path.basename(archivePath), 'kin-macos-aarch64'], {
    cwd: work,
    encoding: 'utf8',
  });
  assert.equal(tar.status, 0, tar.stderr);
  const bytes = fs.readFileSync(archivePath);
  const sha = `${sha256Hex(bytes)}  kin-macos-aarch64.tar.gz\n`;
  const fetchImpl = async (url) => {
    const body = url.endsWith('.sha256') ? Buffer.from(sha) : bytes;
    if (streamArchive && !url.endsWith('.sha256')) {
      const split = Math.floor(body.length / 2);
      return {
        ok: true,
        status: 200,
        headers: {
          get: (name) => (name.toLowerCase() === 'content-length' ? String(body.length) : null),
        },
        body: (async function* streamFixture() {
          yield body.subarray(0, split);
          yield body.subarray(split);
        })(),
        arrayBuffer: async () => {
          throw new Error('streaming archive must not be buffered through arrayBuffer()');
        },
      };
    }
    return {
      ok: true,
      status: 200,
      arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
    };
  };
  return { work, bytes, sha, fetchImpl };
}

function makeWindowsFixture({ streamArchive = false } = {}) {
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-prov-windows-fixture-'));
  const stage = path.join(work, 'stage');
  const archiveName = 'kin-windows-x86_64.zip';
  const archivePath = path.join(work, archiveName);
  const kinBytes = Buffer.from('native windows kin fixture 9.9.9');
  const daemonBytes = Buffer.from('native windows daemon fixture');
  fs.mkdirSync(stage, { recursive: true });
  fs.writeFileSync(path.join(stage, 'kin.exe'), kinBytes);
  fs.writeFileSync(path.join(stage, 'kin-daemon.exe'), daemonBytes);

  if (process.platform === 'win32') {
    const zipped = spawnSync(
      windowsSystemTarPath(),
      ['-a', '-c', '-f', `../${archiveName}`, 'kin.exe', 'kin-daemon.exe'],
      { cwd: stage, encoding: 'utf8' },
    );
    assert.equal(zipped.status, 0, zipped.stderr);
  } else {
    const zipped = spawnSync(
      '/usr/bin/zip',
      ['-q', `../${archiveName}`, 'kin.exe', 'kin-daemon.exe'],
      { cwd: stage, encoding: 'utf8' },
    );
    assert.equal(zipped.status, 0, zipped.stderr);
  }

  const bytes = fs.readFileSync(archivePath);
  assert.equal(bytes.subarray(0, 4).toString('hex'), '504b0304');
  const sha = `${sha256Hex(bytes)}  ${archiveName}\n`;
  const fetchImpl = async (url) => {
    const body = url.endsWith('.sha256') ? Buffer.from(sha) : bytes;
    if (streamArchive && !url.endsWith('.sha256')) {
      const split = Math.floor(body.length / 2);
      return {
        ok: true,
        status: 200,
        headers: {
          get: (name) => (name.toLowerCase() === 'content-length' ? String(body.length) : null),
        },
        body: (async function* streamFixture() {
          yield body.subarray(0, split);
          yield body.subarray(split);
        })(),
        arrayBuffer: async () => {
          throw new Error('streaming archive must not be buffered through arrayBuffer()');
        },
      };
    }
    return {
      ok: true,
      status: 200,
      arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
    };
  };
  return { work, bytes, fetchImpl, kinBytes, daemonBytes };
}

function makeHostProvisionFixture(options = {}) {
  if (process.platform === 'win32') {
    return {
      ...makeWindowsFixture(options),
      platform: 'win32',
      arch: 'x64',
    };
  }
  return {
    ...makeFixture(options),
    platform: 'darwin',
    arch: 'arm64',
  };
}

test('provision verifies, installs kin + kin-daemon, and stamps the version', async () => {
  const { work, fetchImpl, platform, arch } = makeHostProvisionFixture();
  const home = path.join(work, 'kin-home');
  const env = { KIN_HOME: home };
  const installed = await provision('9.9.9', {
    env,
    platform,
    arch,
    fetchImpl,
    log: () => {},
  });
  assert.equal(installed, path.join(home, 'bin', binaryName('kin', platform)));
  assert.ok(fs.existsSync(path.join(home, 'bin', binaryName('kin', platform))));
  assert.ok(fs.existsSync(path.join(home, 'bin', binaryName('kin-daemon', platform))));
  if (process.platform !== 'win32') {
    assert.equal(fs.statSync(installed).mode & 0o111 && true, true);
  }
  assert.equal(readLauncherStamp(env), '9.9.9');
  fs.rmSync(work, { recursive: true, force: true });
});

test('provision uses deterministic Windows ZIP extraction under a hostile PATH', async () => {
  const { work, fetchImpl, kinBytes, daemonBytes } = makeWindowsFixture();
  const home = path.join(work, 'kin-home');
  const env = environmentWithHostileTar(work, { KIN_HOME: home });
  try {
    const installed = await provision('9.9.9', {
      env,
      platform: 'win32',
      arch: 'x64',
      fetchImpl,
      log: () => {},
    });
    assert.equal(installed, path.join(home, 'bin', 'kin.exe'));
    assert.deepEqual(fs.readFileSync(installed), kinBytes);
    assert.deepEqual(fs.readFileSync(path.join(home, 'bin', 'kin-daemon.exe')), daemonBytes);
    assert.equal(readLauncherStamp(env), '9.9.9');
  } finally {
    fs.rmSync(work, { recursive: true, force: true });
  }
});

// The Unix arm of the rule the Windows test above proves. `tar` was resolved
// through PATH, so a planted `tar` unpacked the archive whose SHA-256 had just
// been verified: the integrity check protected bytes an attacker's program
// then read. The hostile `tar` here exits 97, so this test is red against a
// PATH lookup and green against an absolute one.
macArchiveModeTest(
  'provision unpacks the Unix archive with an absolute tar under a hostile PATH',
  async () => {
    const { work, fetchImpl } = makeFixture();
    const home = path.join(work, 'kin-home');
    const env = environmentWithHostileTar(work, { KIN_HOME: home });
    try {
      const installed = await provision('9.9.9', {
        env,
        platform: 'darwin',
        arch: 'arm64',
        fetchImpl,
        log: () => {},
      });
      assert.equal(installed, path.join(home, 'bin', 'kin'));
      assert.ok(fs.existsSync(path.join(home, 'bin', 'kin-daemon')));
      assert.equal(readLauncherStamp(env), '9.9.9');
    } finally {
      fs.rmSync(work, { recursive: true, force: true });
    }
  },
);

test('provision streams live archive byte and percent progress without touching checksum semantics', async () => {
  const { work, bytes, fetchImpl, platform, arch } = makeHostProvisionFixture({
    streamArchive: true,
  });
  const home = path.join(work, 'kin-home');
  const progress = [];
  await provision('9.9.9', {
    env: { KIN_HOME: home },
    platform,
    arch,
    fetchImpl,
    log: () => {},
    onProgress: (event) => progress.push({ ...event }),
  });

  assert.ok(
    progress.some((event) => !event.done && event.received > 0 && event.received < bytes.length),
  );
  assert.deepEqual(progress.at(-1), {
    received: bytes.length,
    total: bytes.length,
    done: true,
  });
  fs.rmSync(work, { recursive: true, force: true });
});

test('a mid-stream failure clears and terminates the active TTY progress line', async () => {
  const { work, bytes, sha } = makeFixture();
  const home = path.join(work, 'kin-home');
  const writes = [];
  const progress = createDownloadProgress('kin-macos-aarch64.tar.gz', {
    write: (value) => writes.push(String(value)),
  });
  const fetchImpl = async (url) => {
    if (url.endsWith('.sha256')) {
      const body = Buffer.from(sha);
      return {
        ok: true,
        status: 200,
        arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
      };
    }
    return {
      ok: true,
      status: 200,
      headers: { get: () => String(bytes.length) },
      body: (async function* failingFixture() {
        yield bytes.subarray(0, Math.max(1, Math.floor(bytes.length / 2)));
        throw new Error('simulated connection reset');
      })(),
      arrayBuffer: async () => {
        throw new Error('failing stream must not use arrayBuffer()');
      },
    };
  };

  await assert.rejects(
    provision('9.9.9', {
      env: { KIN_HOME: home },
      platform: 'darwin',
      arch: 'arm64',
      fetchImpl,
      log: () => {},
      onProgress: progress,
    }),
    /simulated connection reset/,
  );

  assert.match(writes[0], /kin: downloading kin-macos-aarch64\.tar\.gz:/);
  assert.equal(writes.at(-1), '\r\x1b[2K\n');
  const launcherOutput = `${writes.join('')}kin: provisioning failed: simulated connection reset\n`;
  assert.match(launcherOutput, /\r\x1b\[2K\nkin: provisioning failed:/);
  assert.ok(!fs.existsSync(path.join(home, 'bin', 'kin')));
  fs.rmSync(work, { recursive: true, force: true });
});

macArchiveModeTest('provision installs the macOS notification bundle so notifications post as Kin', async () => {
  const { work, fetchImpl } = makeFixture();
  const home = path.join(work, 'kin-home');
  await provision('9.9.9', {
    env: { KIN_HOME: home },
    platform: 'darwin',
    arch: 'arm64',
    fetchImpl,
    log: () => {},
  });
  const executable = path.join(home, 'lib', 'KinNotifier.app', 'Contents', 'MacOS', 'KinNotifier');
  assert.ok(fs.existsSync(executable), 'the bundle must reach $KIN_HOME/lib');
  assert.ok(fs.existsSync(path.join(home, 'lib', 'KinNotifier.app', 'Contents', 'Info.plist')));
  assert.ok(fs.statSync(executable).mode & 0o111, 'a notifier without +x cannot be launched');
  fs.rmSync(work, { recursive: true, force: true });
});

macArchiveModeTest('provision replaces a stale bundle whole rather than merging into it', async () => {
  const { work, fetchImpl } = makeFixture({ notifierBody: 'notifier-v2' });
  const home = path.join(work, 'kin-home');
  // A previous release's bundle, carrying a file the new one does not have.
  const dest = path.join(home, 'lib', 'KinNotifier.app');
  fs.mkdirSync(path.join(dest, 'Contents', 'MacOS'), { recursive: true });
  fs.writeFileSync(path.join(dest, 'Contents', 'MacOS', 'KinNotifier'), 'stale');
  fs.writeFileSync(path.join(dest, 'Contents', 'Stale.txt'), 'left over');

  await provision('9.9.9', {
    env: { KIN_HOME: home },
    platform: 'darwin',
    arch: 'arm64',
    fetchImpl,
    log: () => {},
  });
  assert.match(
    fs.readFileSync(path.join(dest, 'Contents', 'MacOS', 'KinNotifier'), 'utf8'),
    /notifier-v2/,
  );
  assert.ok(
    !fs.existsSync(path.join(dest, 'Contents', 'Stale.txt')),
    'a merged bundle keeps files the new release removed, breaking its signature seal',
  );
  fs.rmSync(work, { recursive: true, force: true });
});

function seedExistingManagedInstall(home) {
  const env = { KIN_HOME: home };
  const binDir = path.join(home, 'bin');
  const bundle = path.join(home, 'lib', 'KinNotifier.app');
  fs.mkdirSync(path.join(bundle, 'Contents', 'MacOS'), { recursive: true });
  fs.mkdirSync(binDir, { recursive: true });
  fs.writeFileSync(path.join(binDir, 'kin'), 'old-kin');
  fs.writeFileSync(path.join(binDir, 'kin-daemon'), 'old-daemon');
  fs.writeFileSync(path.join(bundle, 'Contents', 'Info.plist'), '<plist>old</plist>');
  fs.writeFileSync(path.join(bundle, 'Contents', 'MacOS', 'KinNotifier'), 'old-notifier');
  writeLauncherStamp('8.8.8', env);
  return env;
}

function refusingCurrentNative(binary, args) {
  return { status: 1, stdout: args[0] === 'runtime'
    ? JSON.stringify({ schema: 'kin.runtime-status.v1', ready: false, reason: 'repair_required' })
    : '', stderr: 'native transaction refused' };
}

async function assertMalformedBundleUpgradeIsPreflightOnly(notifier, expectedError) {
  const { work, fetchImpl } = makeFixture({ notifier });
  const home = path.join(work, 'kin-home');
  const env = seedExistingManagedInstall(home);

  await assert.rejects(
    provision('9.9.9', {
      env,
      platform: 'darwin',
      arch: 'arm64',
      fetchImpl,
      spawnImpl: refusingCurrentNative,
      log: () => {},
    }),
    /native transactional update failed/,
  );

  assert.equal(fs.readFileSync(path.join(home, 'bin', 'kin'), 'utf8'), 'old-kin');
  assert.equal(fs.readFileSync(path.join(home, 'bin', 'kin-daemon'), 'utf8'), 'old-daemon');
  assert.equal(
    fs.readFileSync(
      path.join(home, 'lib', 'KinNotifier.app', 'Contents', 'MacOS', 'KinNotifier'),
      'utf8',
    ),
    'old-notifier',
  );
  assert.equal(readLauncherStamp(env), '8.8.8');
  fs.rmSync(work, { recursive: true, force: true });
}

test('a reinstall cannot bypass native authority to replace an existing pair', async () => {
  await assertMalformedBundleUpgradeIsPreflightOnly('absent', /carries no KinNotifier\.app/);
});

macArchiveModeTest('a malformed reinstall also cannot bypass native authority', async () => {
  await assertMalformedBundleUpgradeIsPreflightOnly('no-plist', /missing Contents\/Info\.plist/);
});

test('provision installs no bundle on a platform that has no notification bundle', async () => {
  const { work, fetchImpl } = makeFixture();
  const home = path.join(work, 'kin-home');
  const lines = [];
  await provision('9.9.9', {
    env: { KIN_HOME: home },
    platform: 'linux',
    arch: 'x64',
    fetchImpl,
    log: (line) => lines.push(line),
  });
  assert.ok(!fs.existsSync(path.join(home, 'lib', 'KinNotifier.app')));
  assert.ok(
    !lines.some((line) => line.includes('KinNotifier.app')),
    'a platform with no bundle concept has nothing to warn about',
  );
  fs.rmSync(work, { recursive: true, force: true });
});

// A stand-in for the release kin that keeps the one rule this lane depends on:
// `registry authority --initialize` refuses a Kin home its group or others can
// write, exactly as the real binary's registry check does. It records every
// registry call so a test can see the installer made it.
const REGISTRY_CHECKING_KIN = `#!/bin/sh
if [ "$1" = registry ]; then
  printf '%s\\n' "$*" >> "$KIN_HOME/../registry-calls.log"
  if [ -n "$(find "$KIN_HOME" -prune \\( -perm -020 -o -perm -002 \\) -print)" ]; then
    echo "Error: registry parent $KIN_HOME must not be group/world writable" >&2
    exit 1
  fi
  exit 0
fi
echo "kin 9.9.9 (test)"
`;

function directoryMode(target) {
  return fs.statSync(target).mode & 0o777;
}

// Ubuntu, Debian and every distribution with USERGROUPS_ENAB give a login a
// umask of 0002. mkdirSync's default 0777 then made KIN_HOME group writable,
// the release kin refused its own registry directory, and the npm lane ended
// with a registry nothing had initialized.
macArchiveModeTest('provision creates Kin directories at 0755 and initializes the registry under umask 0002', async () => {
  const { work, fetchImpl } = makeFixture({ kinScript: REGISTRY_CHECKING_KIN });
  const home = path.join(work, 'fresh', 'kin-home');
  const previous = process.umask(0o002);
  try {
    await provision('9.9.9', {
      env: { KIN_HOME: home },
      platform: 'darwin',
      arch: 'arm64',
      fetchImpl,
      log: () => {},
    });
  } finally {
    process.umask(previous);
  }
  for (const directory of [
    path.join(work, 'fresh'),
    home,
    path.join(home, 'bin'),
    path.join(home, 'lib'),
    path.join(home, 'lib', 'KinNotifier.app'),
    path.join(home, 'lib', 'KinNotifier.app', 'Contents'),
  ]) {
    assert.equal(directoryMode(directory), 0o755, `${directory} under umask 0002`);
  }
  assert.equal(
    fs.readFileSync(path.join(work, 'fresh', 'registry-calls.log'), 'utf8'),
    'registry authority --initialize\n',
    'the npm lane initializes the registry the way the shell installer does',
  );
  assert.ok(fs.existsSync(path.join(home, 'bin', 'kin')));
  fs.rmSync(work, { recursive: true, force: true });
});

// A Kin home an earlier install left group writable is not repaired silently.
// The refusal names the directory and the command that repairs it, and the
// installed binaries stay as they were.
macArchiveModeTest('refused native update preserves an existing group-writable home', async () => {
  const { work, fetchImpl } = makeFixture({ kinScript: REGISTRY_CHECKING_KIN });
  const home = path.join(work, 'kin home');
  fs.mkdirSync(path.join(home, 'bin'), { recursive: true });
  fs.writeFileSync(path.join(home, 'bin', 'kin'), 'old kin');
  fs.chmodSync(home, 0o775);
  await assert.rejects(
    provision('9.9.9', {
      env: { KIN_HOME: home },
      platform: 'darwin',
      arch: 'arm64',
      fetchImpl,
      spawnImpl: refusingCurrentNative,
      log: () => {},
    }),
    (error) => {
      assert.match(error.message, /native transactional update failed/);
      assert.match(error.message, /native updater recovery instructions/);
      return true;
    },
  );
  assert.equal(fs.readFileSync(path.join(home, 'bin', 'kin'), 'utf8'), 'old kin');
  assert.equal(directoryMode(home), 0o775, 'a refusal never repairs');

  // Consent is what turns the refusal into a repair request.
  const calls = path.join(work, 'registry-calls.log');
  fs.rmSync(calls, { force: true });
  fs.chmodSync(home, 0o755);
  await assert.rejects(provision('9.9.9', {
    env: { KIN_HOME: home, KIN_REGISTRY_REPAIR: '1' },
    platform: 'darwin', arch: 'arm64', fetchImpl, log: () => {}, spawnImpl: refusingCurrentNative,
  }), /native transactional update failed/);
  assert.equal(fs.existsSync(calls), false, 'npm did not replace the unsupported authority with a staged binary');
  assert.equal(fs.readFileSync(path.join(home, 'bin', 'kin'), 'utf8'), 'old kin');
  fs.rmSync(work, { recursive: true, force: true });
});

macArchiveModeTest('fresh provisioning refuses symlinked component directories before downloading', async t => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-fresh-route-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  const outside = path.join(home, 'outside');
  const root = path.join(home, 'managed');
  fs.mkdirSync(outside);
  fs.mkdirSync(root);
  fs.symlinkSync(outside, path.join(root, 'bin'));
  await assert.rejects(provision('9.9.9', {
    env: { KIN_HOME: root }, log: () => {},
    fetchImpl: async () => { throw new Error('no download before directory authority'); },
  }), /real non-symlink directory/);
  assert.deepEqual(fs.readdirSync(outside), [], 'no component was written outside the managed root');
});

test('provision refuses a checksum mismatch without installing', async () => {
  const { work, bytes } = makeFixture();
  const home = path.join(work, 'kin-home');
  const badSha = `${'0'.repeat(64)}  kin-macos-aarch64.tar.gz\n`;
  const fetchImpl = async (url) => {
    const body = url.endsWith('.sha256') ? Buffer.from(badSha) : bytes;
    return {
      ok: true,
      status: 200,
      arrayBuffer: async () => body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength),
    };
  };
  await assert.rejects(
    provision('9.9.9', { env: { KIN_HOME: home }, platform: 'darwin', arch: 'arm64', fetchImpl, log: () => {} }),
    /SHA-256 mismatch/,
  );
  assert.ok(!fs.existsSync(path.join(home, 'bin', 'kin')));
  fs.rmSync(work, { recursive: true, force: true });
});

test('provision refuses a daemon-less archive before moving anything', async () => {
  const { work, fetchImpl } = makeFixture({ withDaemon: false });
  const home = path.join(work, 'kin-home');
  await assert.rejects(
    provision('9.9.9', { env: { KIN_HOME: home }, platform: 'darwin', arch: 'arm64', fetchImpl, log: () => {} }),
    /daemon-less/,
  );
  assert.ok(!fs.existsSync(path.join(home, 'bin', 'kin')));
  fs.rmSync(work, { recursive: true, force: true });
});

test('provision surfaces a failed download loudly', async () => {
  const fetchImpl = async () => ({ ok: false, status: 404, arrayBuffer: async () => new ArrayBuffer(0) });
  await assert.rejects(
    provision('9.9.9', { env: { KIN_HOME: '/nonexistent-home' }, platform: 'darwin', arch: 'arm64', fetchImpl, log: () => {} }),
    /download failed \(404\)/,
  );
});

test('probeBinaryVersion parses the kin version line and null on failure', () => {
  const okSpawn = (binary, args, options) => {
    assert.equal(binary, '/any');
    assert.deepEqual(args, ['--version']);
    assert.equal(options.timeout, 5000);
    assert.equal(options.killSignal, 'SIGKILL');
    assert.equal(options.maxBuffer, 16 * 1024);
    return { status: 0, stdout: 'kin 1.2.3 (abc detached)\n' };
  };
  const badSpawn = () => ({ status: 1, stdout: '', stderr: 'boom' });
  assert.equal(probeBinaryVersion('/any', okSpawn), '1.2.3');
  assert.equal(probeBinaryVersion('/any', badSpawn), null);
  assert.equal(probeBinaryVersion('/any', () => ({ status: 0, stdout: 'kin unknown\n' })), null);
  assert.equal(probeBinaryVersion('/any', () => ({ status: null, error: new Error('timeout') })), null);
  assert.equal(probeBinaryVersion('/any', () => ({ status: 0, stdout: 'kin 1.2.3-rc.1+build\n' })), '1.2.3-rc.1+build');
});

test('an explicit development pair is isolated and never provisioned', async t => {
  const { cliPath, report, env } = managedFixture(t);
  report.mode = 'development';
  report.cli.source_dirty = true;
  report.daemon.source_dirty = true;
  const result = await ensureProvisioned({
    env: { ...env, KIN_MANAGED_BIN: cliPath },
    fetchImpl: async () => { throw new Error('network must not be touched'); },
    spawnImpl: (binary, args, options) => {
      assert.equal(binary, cliPath);
      assert.deepEqual(args, ['runtime', 'status', '--json', '--development']);
      assert.equal(options.env.KIN_RUNTIME_MODE, 'development');
      return { status: 0, stdout: JSON.stringify(report) };
    },
  });
  assert.equal(result, cliPath);
});

test('an explicit old development binary cannot silently attach without isolation', async t => {
  const { cliPath, env } = managedFixture(t);
  await assert.rejects(ensureProvisioned({
    env: { ...env, KIN_MANAGED_BIN: cliPath },
    spawnImpl: () => ({ status: 1, stdout: '', stderr: 'unknown subcommand runtime' }),
    fetchImpl: async () => { throw new Error('no install repair permitted'); },
  }), /development build.*runtime-isolation contract/);
});

test('ensureProvisioned is resolve-only under KIN_NO_PROVISION', async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-noprov-'));
  const result = await ensureProvisioned({
    env: { KIN_HOME: home, KIN_NO_PROVISION: '1' },
    fetchImpl: async () => {
      throw new Error('network must not be touched');
    },
    log: () => {},
  });
  assert.equal(result, null);
  fs.rmSync(home, { recursive: true, force: true });
});

function managedFixture(t, version = '99.0.0', platform = process.platform) {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-paired-runtime-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  const env = { KIN_HOME: home };
  const cliPath = path.join(home, 'bin', binaryName('kin', platform));
  const daemonPath = path.join(home, 'bin', binaryName('kin-daemon', platform));
  fs.mkdirSync(path.dirname(cliPath), { recursive: true });
  fs.writeFileSync(cliPath, 'preserve-cli');
  fs.writeFileSync(daemonPath, 'preserve-daemon');
  const component = candidate => ({
    path: candidate, version, commit_sha: 'a'.repeat(40), source_dirty: false,
    target: 'test-target', build_profile: 'release', sha256: sha256Hex(fs.readFileSync(candidate)),
  });
  const report = {
    schema: 'kin.runtime-status.v1', ready: true, mode: 'managed', kin_home: home,
    cli: component(cliPath), daemon: component(daemonPath), pair_matches: true,
    channel: 'stable', release_kind: 'published', reason: null, detail: null,
    restart_needed: false,
  };
  return { home, env, cliPath, daemonPath, report };
}

function publishedFixtureFetch(fixture, version, { commitSha = 'c'.repeat(40), mutateRelease = () => {} } = {}) {
  const archiveName = artifactName(fixture.platform, fixture.arch);
  const url = releaseDownloadUrl(version, archiveName);
  const release = { tag_name: `v${version}`, draft: false, prerelease: version.includes('-'), assets: [
    { name: archiveName, browser_download_url: url },
    { name: `${archiveName}.sha256`, browser_download_url: `${url}.sha256` },
  ] };
  mutateRelease(release);
  return async address => {
    if (!address.startsWith('https://api.github.com/')) return fixture.fetchImpl(address);
    const value = address.includes('/git/ref/') ? { ref: `refs/tags/v${version}`, object: { type: 'commit', sha: commitSha } } : release;
    const bytes = Buffer.from(JSON.stringify(value));
    return { ok: true, status: 200, arrayBuffer: async () => bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) };
  };
}

for (const oldVersion of ['0.8.1', '0.8.6']) {
  test(`legacy ${oldVersion} uses a private incoming helper and awaits exact canonical proof`, async t => {
    const fixture = makeHostProvisionFixture();
    t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
    const { home, env, cliPath, daemonPath, report } = managedFixture(t, '99.0.0', fixture.platform);
    const expectedTarget = releaseRustTarget(fixture.platform, fixture.arch);
    report.cli.commit_sha = report.daemon.commit_sha = 'c'.repeat(40);
    report.cli.target = report.daemon.target = expectedTarget;
    const calls = [];
    let migrated = false;
    let incoming;
    const selected = await ensureProvisioned({
      env: { ...env, KIN_DIR: '/conflicting-root', KIN_DAEMON_BIN: daemonPath, KIN_DAEMON_URL: 'http://old-daemon.test' },
      platform: fixture.platform, arch: fixture.arch, minimumVersion: '99.0.0',
      fetchImpl: publishedFixtureFetch(fixture, '99.0.0'), log: () => {},
      spawnImpl: (binary, args, options) => {
        calls.push({ binary, args });
        if (binary === cliPath && args[0] === '--version') return { status: 0, stdout: `kin ${oldVersion}\n` };
        if (args.includes('--bootstrap-published')) {
          incoming = binary;
          assert.notEqual(binary, cliPath, 'the old installed executable never receives new flags');
          assert.equal(options.cwd, path.dirname(binary), 'incoming Windows/native helper has a private working directory');
          assert.equal(options.env.KIN_HOME, home);
          assert.equal(options.env.KIN_DIR, undefined);
          assert.equal(options.env.KIN_DAEMON_BIN, undefined);
          assert.equal(options.env.KIN_DAEMON_URL, undefined);
          assert.equal(options.env.KIN_RUNTIME_MODE, 'managed');
          assert.deepEqual(fs.readdirSync(home), ['bin'], 'staging did not initialize live registry or directories');
          assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
          assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
          assert.deepEqual(args, ['update', '--bootstrap-published', '--channel', 'stable',
            '--expect-version', '99.0.0', '--expect-sha', 'c'.repeat(40),
            '--expect-archive-sha256', sha256Hex(fixture.bytes), '--expect-target', expectedTarget,
            '--expect-archive-name', artifactName(fixture.platform, fixture.arch), '--apply']);
          migrated = true;
          return { status: 0, stdout: 'fixture native transaction result\n' };
        }
        assert.equal(binary, cliPath);
        return migrated ? { status: 0, stdout: JSON.stringify(report) } : { status: 2, stdout: 'unknown runtime command' };
      },
    });
    assert.equal(selected, cliPath);
    assert.equal(readLauncherStamp(env), '99.0.0', 'stamp follows exact canonical proof');
    assert.equal(fs.existsSync(incoming), false, 'successful stage is removed');
    assert.equal(calls.filter(call => call.binary === cliPath && call.args[0] === 'update').length, 0);
    assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli', 'only native fixture authority could replace installed bytes');
  });
}

test('bootstrap preflight refuses unofficial metadata without invoking an installed or incoming updater', async t => {
  const fixture = makeHostProvisionFixture();
  t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
  const { env, cliPath } = managedFixture(t, '0.8.1', fixture.platform);
  await assert.rejects(bootstrapPublishedInstall('99.0.0', {
    env, platform: fixture.platform, arch: fixture.arch, log: () => {},
    fetchImpl: publishedFixtureFetch(fixture, '99.0.0', { mutateRelease: release => { release.draft = true; } }),
    spawnImpl: () => { throw new Error('no executable before official preflight'); },
  }), /official.*(metadata|published)/);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  assert.equal(readLauncherStamp(env), null);
});

test('bootstrap archive checksum failure does not invoke native migration or change the managed home', async t => {
  const fixture = makeHostProvisionFixture();
  t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
  const { home, env, cliPath } = managedFixture(t, '0.8.6', fixture.platform);
  const publishedFetch = publishedFixtureFetch(fixture, '99.0.0');
  await assert.rejects(bootstrapPublishedInstall('99.0.0', {
    env, platform: fixture.platform, arch: fixture.arch, log: () => {},
    fetchImpl: async address => {
      if (!address.endsWith('.sha256')) return publishedFetch(address);
      const bytes = Buffer.from(`${'0'.repeat(64)}  ${artifactName(fixture.platform, fixture.arch)}\n`);
      return { ok: true, status: 200, arrayBuffer: async () => bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) };
    },
    spawnImpl: () => { throw new Error('no executable before archive verification'); },
  }), /archive checksum/);
  assert.deepEqual(fs.readdirSync(home), ['bin']);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
});

test('an older compatible npm wrapper migrates legacy Kin with the exact latest stable tuple', async t => {
  const fixture = makeHostProvisionFixture();
  t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
  const { env, cliPath, report } = managedFixture(t, '0.8.8', fixture.platform);
  report.cli.target = report.daemon.target = releaseRustTarget(fixture.platform, fixture.arch);
  report.cli.commit_sha = report.daemon.commit_sha = 'c'.repeat(40);
  let migrated = false;
  const requested = [];
  const publishedFetch = publishedFixtureFetch(fixture, '0.8.8');
  const selected = await ensureProvisioned({
    env, platform: fixture.platform, arch: fixture.arch, minimumVersion: '0.8.7', log: () => {},
    fetchImpl: address => { requested.push(address); return publishedFetch(address); },
    spawnImpl: (binary, args) => {
      if (args[0] === '--version') return { status: 0, stdout: 'kin 0.8.1\n' };
      if (args.includes('--bootstrap-published')) {
        assert.notEqual(binary, cliPath);
        assert.deepEqual(args, ['update', '--bootstrap-published', '--channel', 'stable',
          '--expect-version', '0.8.8', '--expect-sha', 'c'.repeat(40),
          '--expect-archive-sha256', sha256Hex(fixture.bytes),
          '--expect-target', releaseRustTarget(fixture.platform, fixture.arch),
          '--expect-archive-name', artifactName(fixture.platform, fixture.arch), '--apply']);
        migrated = true;
        return { status: 0, stdout: '' };
      }
      return migrated ? { status: 0, stdout: JSON.stringify(report) } : { status: 2, stdout: 'unsupported runtime command' };
    },
  });
  assert.equal(selected, cliPath);
  assert.equal(readLauncherStamp(env), '0.8.8');
  assert.ok(requested.some(address => address.endsWith('/releases/latest')));
  assert.ok(requested.some(address => address.endsWith('/git/ref/tags/v0.8.8')));
  assert.equal(requested.some(address => address.includes('v0.8.7')), false);
});

test('latest stable below the npm minimum refuses before staging or managed writes', async t => {
  const fixture = makeHostProvisionFixture();
  t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
  const { home, env, cliPath } = managedFixture(t, '0.8.1', fixture.platform);
  const requested = [];
  const publishedFetch = publishedFixtureFetch(fixture, '0.8.6');
  await assert.rejects(ensureProvisioned({
    env, platform: fixture.platform, arch: fixture.arch, minimumVersion: '0.8.7', log: () => {},
    fetchImpl: address => { requested.push(address); return publishedFetch(address); },
    spawnImpl: (binary, args) => {
      assert.equal(binary, cliPath);
      assert.notEqual(args[0], 'update', 'legacy installed CLI is never invoked to migrate');
      return { status: args[0] === '--version' ? 0 : 2, stdout: args[0] === '--version' ? 'kin 0.8.1\n' : 'unsupported runtime command' };
    },
  }), /latest stable Kin 0\.8\.6 is below.*minimum 0\.8\.7/);
  assert.equal(requested.length, 1, 'no tag or archive is staged after an incompatible latest version');
  assert.deepEqual(fs.readdirSync(home), ['bin']);
  assert.equal(readLauncherStamp(env), null);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  await assert.rejects(resolveLatestPublishedRelease('0.8.7', {
    platform: fixture.platform, arch: fixture.arch, fetchImpl: publishedFetch,
  }), /below.*minimum/);
});

for (const refusal of ['native', 'restart', 'wrong-source']) {
  test(`bootstrap ${refusal} refusal retains stage and never claims installed success`, async t => {
    const fixture = makeHostProvisionFixture();
    t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
    const { home, env, cliPath, report } = managedFixture(t, '99.0.0', fixture.platform);
    report.cli.target = report.daemon.target = releaseRustTarget(fixture.platform, fixture.arch);
    report.cli.commit_sha = report.daemon.commit_sha = refusal === 'wrong-source' ? 'b'.repeat(40) : 'c'.repeat(40);
    if (refusal === 'restart') { report.ready = false; report.restart_needed = true; report.reason = 'restart_pending'; }
    let incoming;
    await assert.rejects(bootstrapPublishedInstall('99.0.0', {
      env, platform: fixture.platform, arch: fixture.arch, log: () => {},
      fetchImpl: publishedFixtureFetch(fixture, '99.0.0'),
      spawnImpl: (binary, args) => {
        if (args.includes('--bootstrap-published')) {
          incoming = binary;
          return { status: refusal === 'native' ? 1 : 0, stdout: '', stderr: 'fixture native refusal' };
        }
        assert.equal(binary, cliPath);
        return { status: refusal === 'restart' ? 1 : 0, stdout: JSON.stringify(report) };
      },
    }), /Bootstrap stage retained/);
    assert.ok(fs.existsSync(incoming));
    assert.deepEqual(fs.readdirSync(home), ['bin']);
    assert.equal(readLauncherStamp(env), null);
    assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
    // Fixture cleanup owns only the private stage named by the retained helper.
    const stage = path.dirname(incoming);
    fs.rmSync(path.basename(stage).startsWith('kin-published-bootstrap-') ? stage : path.dirname(stage), { recursive: true, force: true });
  });
}

test('published tag resolution rejects unknown commit identity and binds the platform release assets', async t => {
  const fixture = makeHostProvisionFixture();
  t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
  const release = await resolvePublishedRelease('99.0.0', {
    platform: fixture.platform, arch: fixture.arch, fetchImpl: publishedFixtureFetch(fixture, '99.0.0'),
  });
  assert.equal(release.commitSha, 'c'.repeat(40));
  assert.equal(release.target, releaseRustTarget(fixture.platform, fixture.arch));
  await assert.rejects(resolvePublishedRelease('99.0.0', {
    platform: fixture.platform, arch: fixture.arch,
    fetchImpl: publishedFixtureFetch(fixture, '99.0.0', { commitSha: 'unknown' }),
  }), /published release metadata/);
});

test('published source resolution peels the exact annotated tag instead of a same-named branch', async t => {
  const fixture = makeHostProvisionFixture();
  t.after(() => fs.rmSync(fixture.work, { recursive: true, force: true }));
  const original = publishedFixtureFetch(fixture, '99.0.0');
  const calls = [];
  const fetchImpl = async address => {
    calls.push(address);
    let record;
    if (address.includes('/git/ref/')) record = { ref: 'refs/tags/v99.0.0', object: { type: 'tag', sha: 'd'.repeat(40) } };
    else if (address.includes('/git/tags/')) record = { sha: 'd'.repeat(40), object: { type: 'commit', sha: 'e'.repeat(40) } };
    else return original(address);
    const bytes = Buffer.from(JSON.stringify(record));
    return { ok: true, status: 200, arrayBuffer: async () => bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) };
  };
  const release = await resolvePublishedRelease('99.0.0', { platform: fixture.platform, arch: fixture.arch, fetchImpl });
  assert.equal(release.commitSha, 'e'.repeat(40));
  assert.ok(calls.some(address => address.endsWith(`/git/tags/${'d'.repeat(40)}`)));
  assert.equal(calls.some(address => address.includes('/commits/')), false);
});

test('normal resolution requires paired source identity even when versions and stamp agree', t => {
  const { env, report } = managedFixture(t);
  writeLauncherStamp(report.cli.version, env);
  for (const mutate of [
    r => { r.daemon = null; },
    r => { r.daemon.commit_sha = 'b'.repeat(40); },
    r => { r.daemon.target = 'different-target'; },
    r => { r.cli.source_dirty = true; },
    r => { r.daemon.path = '/different/runtime/kin-daemon'; },
    r => { r.schema = 'unknown.v2'; },
    r => { r.ready = false; r.reason = 'restart_pending'; },
  ]) {
    const changed = structuredClone(report);
    mutate(changed);
    assert.throws(() => resolveReadyManagedBinary({
      env, spawnImpl: () => ({ status: 0, stdout: JSON.stringify(changed) }),
    }), /not certified/);
  }
});

test('matching paired identity is reused without touching network or launcher stamp', async t => {
  const { env, report, cliPath, daemonPath } = managedFixture(t);
  writeLauncherStamp('0.0.1', env);
  const calls = [];
  const selected = await ensureProvisioned({
    env,
    fetchImpl: async () => { throw new Error('no download expected'); },
    spawnImpl: (binary, args) => {
      calls.push(args);
      return { status: 0, stdout: args[0] === '--version' ? `kin ${report.cli.version}\n` : JSON.stringify(report) };
    },
  });
  assert.equal(selected, cliPath);
  assert.deepEqual(calls, [['--version'], ['runtime', 'status', '--json']]);
  assert.equal(readLauncherStamp(env), '0.0.1', 'receipt is not rewritten as runtime proof');
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
});

test('an old managed runtime delegates upgrade exclusively to the native updater', async t => {
  const { env, report, cliPath, daemonPath } = managedFixture(t, '99.0.0', 'linux');
  const calls = [];
  let updated = false;
  const selected = await ensureProvisioned({
    env, platform: 'linux',
    fetchImpl: async () => { throw new Error('launcher must not download an upgrade'); },
    log: () => {},
    spawnImpl: (binary, args) => {
      assert.equal(binary, cliPath);
      calls.push(args);
      if (args[0] === '--version') return { status: 0, stdout: 'kin 0.0.1\n' };
      if (args[0] === 'update') { updated = true; return { status: 0, stdout: 'native transaction completed\n' }; }
      return { status: updated ? 0 : 1, stdout: JSON.stringify(updated ? report : {
        schema: 'kin.runtime-status.v1', ready: false, reason: 'upgrade_required',
      }) };
    },
  });
  assert.equal(selected, cliPath);
  assert.deepEqual(calls, [['--version'], ['runtime', 'status', '--json'], ['update', '--apply'], ['runtime', 'status', '--json']]);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
});

test('a refused native upgrade preserves both existing files and never falls back to provisioning', async t => {
  const { env, cliPath, daemonPath } = managedFixture(t, '99.0.0', 'linux');
  await assert.rejects(ensureProvisioned({
    env, platform: 'linux',
    fetchImpl: async () => { throw new Error('must not download'); },
    spawnImpl: (binary, args) => ({ status: args[0] === '--version' ? 0 : 1,
      stdout: args[0] === '--version' ? 'kin 0.0.1\n' : args[0] === 'runtime'
        ? JSON.stringify({ schema: 'kin.runtime-status.v1', ready: false, reason: 'upgrade_required' }) : '',
      stderr: 'managed runtime is active' }),
    log: () => {},
  }), /transactional update failed/);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
});

test('Windows updates prepare a private native worker and await its exit before status', t => {
  const { env, cliPath, report, home, daemonPath } = managedFixture(t, '99.0.0', 'win32');
  const copied = copiedWindowsWorker(home);
  const calls = [];
  const selected = updateManagedInstall({
    env, platform: 'win32', log: () => {},
    spawnImpl: (binary, args, options) => {
      calls.push({ binary, args });
      if (args.includes('--prepare-windows-worker')) return { status: 0, stdout: JSON.stringify(copied.prepared) };
      if (args.includes('--windows-worker')) {
        assert.equal(binary, copied.worker);
        assert.equal(options.cwd, copied.directory);
        assert.deepEqual(args, ['update', '--windows-worker', copied.record, '--apply']);
        recordMockWorkerConsumption(copied);
        return { status: 0, pid: 4312, stdout: 'native transaction complete' };
      }
      if (args.includes('--cleanup-windows-worker')) {
        assert.equal(binary, cliPath);
        assert.equal(options.cwd, path.dirname(cliPath), 'cleanup runs outside the mapped worker directory');
        assert.deepEqual(args, ['update', '--cleanup-windows-worker', copied.record]);
        assert.ok(fs.existsSync(copied.worker), 'wrapper has not deleted runtime-owned scratch');
        return { status: 0, stdout: 'native cleanup complete' };
      }
      assert.equal(binary, cliPath);
      assert.deepEqual(args, ['runtime', 'status', '--json']);
      assert.ok(fs.existsSync(copied.directory), 'only native cleanup owns scratch deletion');
      return { status: 0, stdout: JSON.stringify(report) };
    },
  });
  assert.equal(selected, cliPath);
  assert.equal(calls.length, 4);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
});

function copiedWindowsWorker(home) {
  const directory = path.join(home, `.update-worker-${crypto.randomUUID()}`);
  fs.mkdirSync(directory);
  const worker = path.join(directory, 'kin-update-worker.exe');
  const record = path.join(directory, 'handoff.json');
  fs.writeFileSync(worker, 'copied worker bytes');
  fs.writeFileSync(record, 'private handoff bytes');
  return { directory, worker, record, prepared: {
    schema: 'kin.windows-update-worker.v1', worker_executable: worker, handoff_record: record,
    source_commit_sha: 'a'.repeat(40), version: '99.0.0', worker_sha256: sha256Hex(fs.readFileSync(worker)),
  } };
}

// These injected records test consumer dispatch only. Native Windows controls
// execute the actual worker and its process/object-bound consumption proof.
function recordMockWorkerConsumption(copied, overrides = {}) {
  const consumed = {
    schema: 'kin.windows-update-worker-consumed.v1',
    worker_process: { pid: 4312, boot_id: 'mock boot', birth_token: 'mock birth' },
    directory_binding: { mock: true }, handoff_binding: { mock: true },
    handoff_sha256: sha256Hex(fs.readFileSync(copied.record)),
    ...overrides,
  };
  fs.writeFileSync(path.join(copied.directory, 'claimed'), JSON.stringify(consumed));
}

for (const outcome of ['no-update', 'failure']) {
  test(`consumed Windows worker ${outcome} requests native cleanup after exit and preserves its result`, t => {
    const { home, env, cliPath, report, daemonPath } = managedFixture(t, '99.0.0', 'win32');
    const copied = copiedWindowsWorker(home);
    const calls = [];
    const run = () => updateManagedInstall({
      env, platform: 'win32', log: () => {},
      spawnImpl: (binary, args) => {
        calls.push(args);
        if (args.includes('--prepare-windows-worker')) return { status: 0, stdout: JSON.stringify(copied.prepared) };
        if (args.includes('--windows-worker')) {
          assert.ok(fs.existsSync(copied.worker));
          recordMockWorkerConsumption(copied);
          return { status: outcome === 'failure' ? 17 : 0, pid: 4312,
            stdout: outcome === 'no-update' ? 'already current' : '', stderr: 'fixture failure' };
        }
        if (args.includes('--cleanup-windows-worker')) {
          assert.equal(binary, cliPath);
          return { status: 0, stdout: '' };
        }
        return { status: 0, stdout: JSON.stringify(report) };
      },
    });
    if (outcome === 'failure') assert.throws(run, /transactional update failed \(17\)/);
    else assert.equal(run(), cliPath);
    assert.deepEqual(calls.slice(0, 3), [
      ['update', '--prepare-windows-worker'], ['update', '--windows-worker', copied.record, '--apply'],
      ['update', '--cleanup-windows-worker', copied.record],
    ]);
    assert.ok(fs.existsSync(copied.directory), 'the mock native cleanup did not delete; the wrapper must not do so');
    assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
    assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
  });
}

for (const cleanupOutcome of ['refusal', 'throw']) {
  test(`native Windows cleanup ${cleanupOutcome} never replaces the worker exit result`, t => {
    const { home, env, cliPath } = managedFixture(t, '99.0.0', 'win32');
    const copied = copiedWindowsWorker(home);
    const logs = [];
    assert.throws(() => updateManagedInstall({
      env, platform: 'win32', log: line => logs.push(line),
      spawnImpl: (binary, args) => {
        if (args.includes('--prepare-windows-worker')) return { status: 0, stdout: JSON.stringify(copied.prepared) };
        if (args.includes('--windows-worker')) {
          recordMockWorkerConsumption(copied);
          return { status: 17, pid: 4312, stdout: '', stderr: 'original worker failure' };
        }
        assert.equal(binary, cliPath);
        assert.deepEqual(args, ['update', '--cleanup-windows-worker', copied.record]);
        if (cleanupOutcome === 'throw') throw new Error('native cleanup invocation failed');
        return { status: 19, stdout: '', stderr: 'native recovery fence retained scratch' };
      },
    }), /transactional update failed \(17\)/);
    assert.ok(logs.some(line => line.includes('cleanup deferred')));
    assert.ok(fs.existsSync(copied.worker));
    assert.ok(fs.existsSync(copied.record));
    assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  });
}

test('native cleanup refusal does not turn a successful worker into a failed update', t => {
  const { home, env, cliPath, report } = managedFixture(t, '99.0.0', 'win32');
  const copied = copiedWindowsWorker(home);
  assert.equal(updateManagedInstall({
    env, platform: 'win32', log: () => {},
    spawnImpl: (binary, args) => {
      if (args.includes('--prepare-windows-worker')) return { status: 0, stdout: JSON.stringify(copied.prepared) };
      if (args.includes('--windows-worker')) {
        recordMockWorkerConsumption(copied);
        return { status: 0, pid: 4312, stdout: '' };
      }
      if (args.includes('--cleanup-windows-worker')) return { status: 1, stderr: 'scratch preserved', stdout: '' };
      return { status: 0, stdout: JSON.stringify(report) };
    },
  }), cliPath);
  assert.ok(fs.existsSync(copied.worker));
});

for (const consumption of ['unclaimed', 'empty', 'wrong-schema', 'wrong-pid', 'wrong-handoff-hash', 'linked-marker']) {
  test(`Windows wrapper refuses native cleanup dispatch for ${consumption} consumption proof`, {
    skip: consumption === 'linked-marker' && process.platform === 'win32',
  }, t => {
    const { home, env, cliPath, report } = managedFixture(t, '99.0.0', 'win32');
    const copied = copiedWindowsWorker(home);
    let cleanupCalls = 0;
    assert.equal(updateManagedInstall({
      env, platform: 'win32', log: () => {},
      spawnImpl: (binary, args) => {
        if (args.includes('--prepare-windows-worker')) return { status: 0, stdout: JSON.stringify(copied.prepared) };
        if (args.includes('--windows-worker')) {
          const marker = path.join(copied.directory, 'claimed');
          if (consumption === 'empty') fs.writeFileSync(marker, '');
          else if (consumption !== 'unclaimed') {
            const overrides = consumption === 'wrong-schema' ? { schema: 'unknown' }
              : consumption === 'wrong-pid' ? { worker_process: { pid: 9999 } }
                : consumption === 'wrong-handoff-hash' ? { handoff_sha256: '0'.repeat(64) } : {};
            recordMockWorkerConsumption(copied, overrides);
            if (consumption === 'linked-marker') {
              const retained = path.join(home, 'retained-claim');
              fs.renameSync(marker, retained);
              fs.symlinkSync(retained, marker);
            }
          }
          return { status: 0, pid: 4312, stdout: '' };
        }
        if (args.includes('--cleanup-windows-worker')) cleanupCalls++;
        return { status: 0, stdout: JSON.stringify(report) };
      },
    }), cliPath);
    assert.equal(cleanupCalls, 0);
    assert.ok(fs.existsSync(copied.worker));
    assert.ok(fs.existsSync(copied.record));
    assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  });
}

test('Windows prepare cannot direct wrapper cleanup outside the exact managed worker directory', t => {
  const { home, env, cliPath } = managedFixture(t, '99.0.0', 'win32');
  const otherHome = path.join(home, 'unrelated-data');
  fs.mkdirSync(otherHome);
  const copied = copiedWindowsWorker(otherHome);
  let calls = 0;
  assert.throws(() => updateManagedInstall({
    env, platform: 'win32',
    spawnImpl: () => { calls++; return { status: 0, stdout: JSON.stringify(copied.prepared) }; },
  }), /scratch is not an unclaimed owned directory/);
  assert.equal(calls, 1, 'worker outside managed direct child was never launched');
  assert.ok(fs.existsSync(copied.worker));
  assert.ok(fs.existsSync(copied.record));
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
});

test('a preclaimed Windows handoff remains intact and is never replayed by the wrapper', t => {
  const { home, env } = managedFixture(t, '99.0.0', 'win32');
  const copied = copiedWindowsWorker(home);
  fs.writeFileSync(path.join(copied.directory, 'claimed'), '');
  let calls = 0;
  assert.throws(() => updateManagedInstall({
    env, platform: 'win32',
    spawnImpl: () => { calls++; return { status: 0, stdout: JSON.stringify(copied.prepared) }; },
  }), /scratch is not an unclaimed owned directory/);
  assert.equal(calls, 1);
  assert.ok(fs.existsSync(copied.worker));
  assert.ok(fs.existsSync(copied.record));
});

for (const uncertain of ['no-exit-status', 'no-child-pid', 'wait-threw']) {
  test(`Windows worker scratch is retained when synchronous completion is uncertain: ${uncertain}`, t => {
    const { home, env, cliPath } = managedFixture(t, '99.0.0', 'win32');
    const copied = copiedWindowsWorker(home);
    let cleanupCalls = 0;
    assert.throws(() => updateManagedInstall({
      env, platform: 'win32', log: () => {},
      spawnImpl: (binary, args) => {
        if (args.includes('--prepare-windows-worker')) return { status: 0, stdout: JSON.stringify(copied.prepared) };
        if (args.includes('--cleanup-windows-worker')) cleanupCalls++;
        assert.equal(binary, copied.worker);
        recordMockWorkerConsumption(copied);
        if (uncertain === 'wait-threw') throw new Error('synchronous worker wait failed');
        if (uncertain === 'no-child-pid') return { status: 1, stdout: '' };
        return { status: null, error: new Error('no confirmed worker exit'), stdout: '' };
      },
    }), /worker wait failed|no confirmed worker exit|transactional update failed \(1\)/);
    assert.equal(cleanupCalls, 0);
    assert.ok(fs.existsSync(copied.worker));
    assert.ok(fs.existsSync(copied.record));
    assert.ok(fs.existsSync(path.join(copied.directory, 'claimed')));
    assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  });
}

test('a changed Windows worker is refused before launch or installed-file mutation', t => {
  const { env, home, cliPath } = managedFixture(t, '99.0.0', 'win32');
  const worker = path.join(home, 'worker.exe');
  const handoff = path.join(home, 'handoff.json');
  fs.writeFileSync(worker, 'changed bytes');
  fs.writeFileSync(handoff, 'handoff fixture');
  let calls = 0;
  assert.throws(() => updateManagedInstall({
    env, platform: 'win32',
    spawnImpl: () => {
      calls += 1;
      return { status: 0, stdout: JSON.stringify({
        schema: 'kin.windows-update-worker.v1', worker_executable: worker, handoff_record: handoff,
        source_commit_sha: 'a'.repeat(40), version: '99.0.0', worker_sha256: '0'.repeat(64),
      }) };
    },
  }), /worker identity changed/);
  assert.equal(calls, 1);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
});

test('development launch strips an inherited normal root and uses the admitted isolated home', t => {
  const { env, cliPath, report, home } = managedFixture(t);
  report.mode = 'development';
  report.kin_home = path.join(home, 'isolated-home');
  const inherited = { ...env, KIN_DIR: env.KIN_HOME };
  const launched = developmentLaunchEnvironment(cliPath, {
    env: inherited,
    spawnImpl: (binary, args, options) => {
      assert.equal(options.env.KIN_RUNTIME_MODE, 'development');
      assert.equal(options.env.KIN_HOME, undefined);
      assert.equal(options.env.KIN_DIR, undefined);
      return { status: 0, stdout: JSON.stringify(report) };
    },
  });
  assert.equal(launched.KIN_HOME, report.kin_home);
  assert.equal(launched.KIN_DIR, report.kin_home);
  assert.equal(inherited.KIN_HOME, env.KIN_HOME, 'the caller environment was not mutated');
});

test('old status API cannot be certified by stamp when official migration metadata is unavailable', async t => {
  const { env, cliPath } = managedFixture(t);
  writeLauncherStamp('99.0.0', env);
  for (const version of ['kin 99.0.0\n', 'unknown binary']) {
    await assert.rejects(ensureProvisioned({
      env, spawnImpl: (binary, args) => ({ status: 0, stdout: args[0] === '--version' ? version : 'old CLI' }),
      fetchImpl: async () => { throw new Error('official release unavailable'); },
    }), /official release unavailable/);
  }
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
});

test('an explicit development selection cannot use direct managed provisioning', async t => {
  const { env, cliPath } = managedFixture(t);
  await assert.rejects(provision('99.0.0', {
    env: { ...env, KIN_MANAGED_BIN: cliPath },
    fetchImpl: async () => { throw new Error('no download permitted'); },
    spawnImpl: () => { throw new Error('no managed repair permitted'); },
  }), /development selections cannot run managed provisioning/);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
});

test('independent adoption cannot overwrite an existing managed pair', async t => {
  const { env, cliPath, daemonPath } = managedFixture(t);
  await assert.rejects(ensureProvisioned({
    env: { ...env, KIN_LAUNCHER_ADOPT: '1' },
    spawnImpl: () => { throw new Error('must not launch'); },
    fetchImpl: async () => { throw new Error('must not download'); },
  }), /cannot replace a managed pair independently/);
  assert.equal(fs.readFileSync(cliPath, 'utf8'), 'preserve-cli');
  assert.equal(fs.readFileSync(daemonPath, 'utf8'), 'preserve-daemon');
});

// FIR-2628. `npm install -g` dies with EACCES on a root-owned global prefix,
// and the death happens inside npm before this package is unpacked, so nothing
// here can intercept it. The recovery Kin can offer is the install that needs
// no prefix at all, and this is the one moment its own code runs to say so.
test('a first provision names the persistent no-root install when PATH cannot see it', async () => {
  const { work, fetchImpl, platform, arch } = makeHostProvisionFixture();
  const home = path.join(work, 'kin-home');
  const lines = [];
  await provision('9.9.9', {
    env: { KIN_HOME: home, PATH: '/usr/bin:/bin' },
    platform,
    arch,
    fetchImpl,
    log: (line) => lines.push(line),
  });
  const said = lines.join('\n');
  const binDir = path.join(home, 'bin');

  // Positive control: the install line itself must be there, or this test is
  // asserting about a provision that never ran.
  assert.match(said, /managed kin 9\.9\.9 installed at/);

  assert.ok(said.includes(binDir), `the advice must name the directory: ${said}`);
  assert.match(said, /export PATH=/);
  assert.match(said, /needs no root and no writable npm prefix/);
  assert.match(said, /kin setup/);
  fs.rmSync(work, { recursive: true, force: true });
});

test('a provision into a directory already on PATH says nothing extra', async () => {
  const { work, fetchImpl, platform, arch } = makeHostProvisionFixture();
  const home = path.join(work, 'kin-home');
  const binDir = path.join(home, 'bin');
  const lines = [];
  await provision('9.9.9', {
    env: { KIN_HOME: home, PATH: `/usr/bin${path.delimiter}${binDir}` },
    platform,
    arch,
    fetchImpl,
    log: (line) => lines.push(line),
  });
  const said = lines.join('\n');
  assert.match(said, /managed kin 9\.9\.9 installed at/);
  assert.equal(/export PATH=/.test(said), false, `no advice was needed: ${said}`);
  fs.rmSync(work, { recursive: true, force: true });
});

test('persistentPathAdvice compares resolved paths, not spellings', () => {
  const dir = path.join(os.tmpdir(), 'kin-advice-fixture', 'bin');
  assert.deepEqual(persistentPathAdvice(dir, { PATH: dir }), []);
  assert.deepEqual(
    persistentPathAdvice(dir, { PATH: path.join(dir, '..', 'bin') }),
    [],
    'a path that resolves to the same directory is the same directory',
  );
  assert.ok(
    persistentPathAdvice(dir, { PATH: '/usr/bin' }).length > 0,
    'a PATH that does not carry it must produce advice',
  );
  assert.ok(
    persistentPathAdvice(dir, {}).length > 0,
    'an absent PATH is not evidence the directory is reachable',
  );
});

test('setupFollows reads the forwarded command, not an option or a later word', () => {
  assert.equal(setupFollows(['setup']), true);
  assert.equal(setupFollows(['--no-color', 'setup', '--verbose']), true);
  assert.equal(setupFollows(['init', 'setup']), false);
  assert.equal(setupFollows(['--version']), false);
  assert.equal(setupFollows([]), false);
});

test('provision says only the download when kin setup runs next', async () => {
  for (const [follows, expectNotes] of [
    [true, false],
    [false, true],
  ]) {
    const { work, fetchImpl, platform, arch } = makeHostProvisionFixture();
    const home = path.join(work, 'kin-home');
    const lines = [];
    await provision('9.9.9', {
      env: { KIN_HOME: home, PATH: '' },
      platform,
      arch,
      fetchImpl,
      log: (line) => lines.push(line),
      setupFollows: follows,
    });
    const noted = lines.some((line) => line.includes('installed at'));
    assert.equal(noted, expectNotes, lines.join('\n'));
    if (follows) {
      assert.deepEqual(lines, [], 'setup reports the rest itself');
    }
    fs.rmSync(work, { recursive: true, force: true });
  }
});
