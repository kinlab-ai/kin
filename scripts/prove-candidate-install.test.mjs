// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC
import test from 'node:test';
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { PLATFORMS, validateDefaultArchiveFeatures, digest, installerHostScope, validateManifest, validateArchive, verifyInstalledComponent, serveArchive, runCommand } from './prove-candidate-install.mjs';
import { matrixRows, buildJobBody } from './check-rc-build-drift.mjs';

function fixture(platform = 'win32', arch = 'x64') {
  const [artifact, target] = PLATFORMS[`${platform}:${arch}`];
  const e = { platform, arch, artifact, commit: 'a'.repeat(40), lock: 'b'.repeat(64), version: '0.8.0', repository: 'kinlab-ai/kin', runId: '123', attempt: '1' };
  const bytes = Buffer.from('the exact candidate archive');
  const identity = { schema: 'kin.update-build.v1', version: e.version, commit: e.commit, clean: true,
    source_known: true, dependency_provenance: e.lock, graph_snapshot_version: 18 };
  const suffix = platform === 'win32' ? '.exe' : '';
  const m = { schema_version: 2, release_candidate: { ref: e.commit, version: e.version }, artifact, target,
    kin: { commit: e.commit, cargo_lock_sha256: e.lock, embedded_dependency_provenance: e.lock },
    workflow: { repository: e.repository, run_id: e.runId, run_attempt: e.attempt },
    archive: { name: `${artifact}.${platform === 'win32' ? 'zip' : 'tar.gz'}`, sha256: digest(bytes), size_bytes: bytes.length },
    archive_contents: ['kin', 'kin-daemon'].map((n) => ({ name: n + suffix, sha256: 'c'.repeat(64), size_bytes: 100, build_identity: { ...identity } })) };
  return { e, m, bytes, checksum: `${m.archive.sha256}  ${m.archive.name}\n` };
}

test('all five native target/archive combinations bind both authorities', () => {
  for (const pair of Object.keys(PLATFORMS)) {
    const { e, m } = fixture(...pair.split(':'));
    assert.equal(validateManifest(m, e).components.length, 2);
  }
});

test('Windows real-installer side effects require a disposable hosted account and remain disclosed', () => {
  assert.throws(() => installerHostScope('win32', {}), /disposable/);
  assert.throws(() => installerHostScope('win32', { GITHUB_ACTIONS: 'true', RUNNER_ENVIRONMENT: 'self-hosted' }), /persistent/);
  const scope = installerHostScope('win32', { GITHUB_ACTIONS: 'true', RUNNER_ENVIRONMENT: 'github-hosted' });
  assert.equal(scope.user_path_restored, false);
  assert.match(scope.host_cleanup_owner, /not this proof driver/);
});

test('a self-hosted runner is disposable only as a single-use Depot runner the workflow declares', () => {
  const depot = { GITHUB_ACTIONS: 'true', RUNNER_ENVIRONMENT: 'self-hosted', RUNNER_NAME: 'depot-97rttqzvck',
    KIN_DISPOSABLE_RUNNER: 'depot', DEPOT_ORG_ID: 'a1b2c3d4e5' };
  const scope = installerHostScope('win32', depot);
  assert.equal(scope.host, 'disposable single-use Depot runner');
  assert.equal(scope.user_path_restored, false);
  assert.match(scope.host_cleanup_owner, /^Depot runner teardown, not this proof driver$/);
  const { KIN_DISPOSABLE_RUNNER, ...unmarked } = depot;
  assert.throws(() => installerHostScope('win32', unmarked), /persistent/, 'a Depot name without the workflow marker');
  assert.throws(() => installerHostScope('win32', { ...depot, RUNNER_NAME: 'build-box-1' }), /persistent/,
    'the workflow marker without a Depot name');
  const { DEPOT_ORG_ID, ...unorganized } = depot;
  assert.throws(() => installerHostScope('win32', unorganized), /persistent/, 'a Depot match without its organization id');
  const { GITHUB_ACTIONS, ...outside } = depot;
  assert.throws(() => installerHostScope('win32', outside), /disposable/, 'a Depot match outside Actions');
});

for (const [name, change] of [
  ['wrong platform', (m) => { m.target = 'aarch64-apple-darwin'; }],
  ['wrong artifact', (m) => { m.artifact = 'kin-macos-x86_64'; }],
  ['foreign source', (m) => { m.release_candidate.ref = 'd'.repeat(40); }],
  ['foreign lock', (m) => { m.kin.cargo_lock_sha256 = 'd'.repeat(64); }],
  ['different workflow run', (m) => { m.workflow.run_id = '124'; }],
  ['different run attempt', (m) => { m.workflow.run_attempt = '2'; }],
  ['published manifest', (m) => { m.release_tag = 'v0.8.0'; }],
  ['missing daemon', (m) => { m.archive_contents.pop(); }],
  ['duplicate CLI', (m) => { m.archive_contents.push(m.archive_contents[0]); }],
  ['dirty daemon', (m) => { m.archive_contents[1].build_identity.clean = false; }],
  ['unknown daemon source', (m) => { m.archive_contents[1].build_identity.source_known = false; }],
  ['different daemon version', (m) => { m.archive_contents[1].build_identity.version = '0.7.21'; }],
  ['different daemon graph version', (m) => { m.archive_contents[1].build_identity.graph_snapshot_version++; }],
  ['foreign archive', (m) => { m.archive.name = '../foreign.zip'; }],
]) test(`refuses ${name}`, () => {
  const { e, m } = fixture(); change(m); assert.throws(() => validateManifest(m, e));
});

test('archive and exact-name checksum must both agree; tamper and duplicate are rejected', () => {
  const { m, bytes, checksum } = fixture();
  validateArchive(bytes, checksum, m.archive);
  assert.throws(() => validateArchive(Buffer.from('different'), checksum, m.archive));
  assert.throws(() => validateArchive(bytes, checksum.replace(m.archive.name, 'other.zip'), m.archive));
  assert.throws(() => validateArchive(bytes, checksum + checksum, m.archive));
  assert.throws(() => validateArchive(bytes, '', m.archive));
});

test('installed bytes and embedded daemon identity must both match the admitted inventory', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-candidate-identity-'));
  try {
    const { m } = fixture();
    const component = m.archive_contents[1];
    const bytes = Buffer.alloc(198);
    Buffer.from('00894b494e555044415445010d0a1a0a', 'hex').copy(bytes);
    bytes.write('kin.update-build.v1', 16); bytes.write('0.8.0', 40);
    bytes.write('a'.repeat(40), 72); bytes[112] = 1; bytes[113] = 1;
    bytes.write('b'.repeat(64), 114); bytes.writeUInt32LE(18, 178);
    Buffer.from('00894b494e454e445631ff010d0a1a0a', 'hex').copy(bytes, 182);
    component.sha256 = digest(bytes); component.size_bytes = bytes.length;
    const file = path.join(root, 'kin-daemon.exe'); fs.writeFileSync(file, bytes);
    verifyInstalledComponent(file, component);
    const tampered = Buffer.from(bytes); tampered[113] = 0; fs.writeFileSync(file, tampered);
    assert.throws(() => verifyInstalledComponent(file, component), /bytes changed/);
    component.sha256 = digest(tampered);
    assert.throws(() => verifyInstalledComponent(file, component), /identity changed/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('owned loopback mirror serves exact archive/checksum and refuses other paths/methods', async () => {
  const { e, m, bytes, checksum } = fixture();
  const server = await serveArchive(e.version, m.archive.name, bytes, Buffer.from(checksum));
  try {
    assert.match(server.url, /^http:\/\/127\.0\.0\.1:\d+$/);
    const base = `${server.url}/download/v${e.version}/${m.archive.name}`;
    assert.deepEqual(Buffer.from(await (await fetch(base)).arrayBuffer()), bytes);
    assert.equal(await (await fetch(base + '.sha256')).text(), checksum);
    assert.equal((await fetch(base, { method: 'POST' })).status, 404);
    assert.equal((await fetch(server.url + '/etc/passwd')).status, 404);
    assert.deepEqual(server.requests.map((r) => r.status), [200, 200, 404, 404]);
  } finally { await server.close(); }
  await assert.rejects(fetch(server.url));
});

test('commands retain real nonzero exits and never convert stderr to success', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-candidate-command-'));
  try {
    const prefix = path.join(root, 'failed');
    await assert.rejects(runCommand(process.execPath, ['-e', 'console.error("original failure");process.exit(17)'],
      { cwd: root, env: process.env, prefix, deadline: Date.now() + 5000 }), /failed/);
    assert.equal(JSON.parse(fs.readFileSync(prefix + '.exit.json')).code, 17);
    assert.match(fs.readFileSync(prefix + '.stderr', 'utf8'), /original failure/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('absolute command deadline kills an owned stalled command and retains timeout', async () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-candidate-timeout-'));
  try {
    const prefix = path.join(root, 'timeout');
    await assert.rejects(runCommand(process.execPath, ['-e', 'setInterval(()=>{},1000)'],
      { cwd: root, env: process.env, prefix, deadline: Date.now() + 100 }), /timed out/);
    assert.equal(JSON.parse(fs.readFileSync(prefix + '.exit.json')).issue, 'candidate command timed out');
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('native install job covers every built archive and uses the real proof driver', () => {
  const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
  const text = fs.readFileSync(path.join(root, '.github/workflows/rc-build.yml'), 'utf8');
  const built = [...matrixRows(buildJobBody(text, 'RC'), 'RC').keys()].sort();
  assert.deepEqual(built, Object.values(PLATFORMS).map((p) => p[0]).sort());
  const capability = text.slice(text.indexOf('\n  capability:'));
  const probed = [...capability.matchAll(/^\s+artifact: (\S+)/gm)].map((m) => m[1]).sort();
  assert.deepEqual(probed, built);
  assert.equal((capability.match(/^\s+covered: true$/gm) || []).length, built.length);
  assert.ok(capability.includes('          node scripts/prove-candidate-install.mjs'));
  assert.ok(capability.includes("if: ${{ always() && matrix.covered }}"));
  assert.ok(capability.includes('name: candidate-install-${{ matrix.artifact }}'));
});

test('default archive feature proof rejects every missing or enabled engine dimension', () => {
  const meta = { feature_flags: ['vector', 'external-embeddings'], embeddings: {
    vector_enabled: true, embeddings_enabled: true, local_inference_enabled: false,
    external_embeddings_enabled: true, metal_enabled: false, provider: 'none' } };
  validateDefaultArchiveFeatures(meta);
  for (const [key, value] of [['vector_enabled', false], ['embeddings_enabled', false],
    ['local_inference_enabled', true], ['external_embeddings_enabled', false],
    ['metal_enabled', true], ['provider', 'external']]) {
    const changed = structuredClone(meta); changed.embeddings[key] = value;
    assert.throws(() => validateDefaultArchiveFeatures(changed), key);
    delete changed.embeddings[key];
    assert.throws(() => validateDefaultArchiveFeatures(changed), `${key} missing`);
  }
  for (const flag of ['embeddings', 'metal']) {
    assert.throws(() => validateDefaultArchiveFeatures({ ...meta, feature_flags: [...meta.feature_flags, flag] }));
  }
});
