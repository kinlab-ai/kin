// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The vendored proof tooling is a byte-identical copy of the umbrella's, and
// this is the check that keeps the manifest honest: an edit to a vendored file
// that does not move VENDORED.json fails here, so a local fix cannot fork the
// copy silently. The lock shim beside them is written for this tree and is
// held to its contract instead.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const MANIFEST = JSON.parse(fs.readFileSync(path.join(HERE, 'VENDORED.json'), 'utf8'));
const SHIM = path.join(HERE, 'bin', 'kin-lane');

const sha256 = (file) => createHash('sha256').update(fs.readFileSync(file)).digest('hex');

test('the manifest names the umbrella commit and every vendored file', () => {
  assert.equal(MANIFEST.source_repository, 'firelock-ai/kin-ecosystem');
  assert.match(MANIFEST.source_commit, /^[0-9a-f]{40}$/);
  const expected = [
    'bin/kin-acceptance-suite',
    'bin/kin-evidence-publish',
    'bin/kin-release-preflight',
    'bin/kin-release-preflight.d/Dockerfile',
    'bin/kin-release-preflight.d/proof-flow.sh',
    'bin/kin-release-preflight.d/validate.mjs',
  ];
  assert.deepEqual(Object.keys(MANIFEST.files).sort(), expected);
});

test('every vendored file carries the bytes the manifest records', () => {
  for (const [relative, entry] of Object.entries(MANIFEST.files)) {
    const file = path.join(HERE, relative);
    assert.ok(fs.existsSync(file), `${relative} is missing`);
    assert.match(entry.sha256, /^[0-9a-f]{64}$/);
    assert.equal(sha256(file), entry.sha256, `${relative} drifted from VENDORED.json`);
    assert.equal(entry.source, relative);
  }
});

test('the executables are executable', () => {
  for (const relative of ['bin/kin-release-preflight', 'bin/kin-evidence-publish', 'bin/kin-acceptance-suite', 'bin/kin-release-preflight.d/proof-flow.sh', 'bin/kin-lane']) {
    const mode = fs.statSync(path.join(HERE, relative)).mode;
    assert.ok(mode & 0o111, `${relative} is not executable`);
  }
});

test('the lock shim keeps the acquire, holder and release contract the preflight relies on', () => {
  const locks = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-shim-'));
  const env = { ...process.env, KIN_RELEASE_PROOF_LOCKS: locks };
  const run = (...args) => spawnSync('bash', [SHIM, ...args], { env, encoding: 'utf8' });

  // Acquire, then read the holder back: this is the read the preflight refuses
  // to run a leg without.
  assert.equal(run('acquire', 'daemon', 'release-proof', '--pid', '123', 'release preflight').status, 0);
  assert.equal(run('holder', 'daemon').stdout.trim(), 'release-proof');
  // The same lane may re-acquire; another lane may not.
  assert.equal(run('acquire', 'daemon', 'release-proof').status, 0);
  const contended = run('acquire', 'daemon', 'other-lane');
  assert.notEqual(contended.status, 0);
  assert.match(contended.stderr, /daemon is held by release-proof/);
  // A release by the wrong lane is refused; by the holder it clears the table.
  const wrong = run('release', 'daemon', 'other-lane');
  assert.notEqual(wrong.status, 0);
  assert.match(wrong.stderr, /held by 'release-proof', not 'other-lane'/);
  assert.equal(run('release', 'daemon', 'release-proof').status, 0);
  assert.equal(run('holder', 'daemon').stdout.trim(), '');
  assert.equal(run('release', 'daemon', 'release-proof').status, 0);
  assert.notEqual(run('frobnicate').status, 0);
});

for (const suite of ['magic', 'brownfield']) {
  test(`a staged ${suite} suite carries its exact sibling modules in a clean environment`, (t) => {
    const python = spawnSync('python3', ['--version'], { encoding: 'utf8' });
    if (python.error) return t.skip('python3 unavailable');
    const root = path.join(HERE, '..', '..');
    const staged = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-suite-'));
    t.after(() => fs.rmSync(staged, { recursive: true, force: true }));
    const dest = path.join(staged, `${suite}_repro.py`);
    const env = { PATH: process.env.PATH, HOME: staged, LANG: 'C.UTF-8' };
    // The resolver exit also carries its drift tripwire verdict, which is not
    // what this test grades; the staging writes land before that verdict.
    spawnSync('python3', [path.join(HERE, 'bin', 'kin-acceptance-suite'), suite,
      '--checkout', root, '--dest', dest, '--no-fetch'], { env, encoding: 'utf8' });
    assert.ok(fs.existsSync(dest), 'the suite was not staged');
    for (const name of ['trace_pages.py', 'runtime_fixture.py']) {
      const sibling = path.join(staged, name);
      assert.ok(fs.existsSync(sibling), `${name} was not staged beside the suite`);
      assert.equal(sha256(sibling), sha256(path.join(root, 'scripts', 'acceptance', name)));
    }
    const help = spawnSync('python3', [dest, '--help'], { cwd: staged, env, encoding: 'utf8' });
    assert.equal(help.status, 0, help.stderr);
    assert.doesNotMatch(help.stderr, /ModuleNotFoundError/);
  });
}

test('rehearsal receipts and candidate staging bind the exact runtime helper bytes', (t) => {
  const python = spawnSync('python3', ['--version'], { encoding: 'utf8' });
  if (python.error) return t.skip('python3 unavailable');
  const root = path.join(HERE, '..', '..');
  const staged = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-rehearsal-inputs-'));
  t.after(() => fs.rmSync(staged, { recursive: true, force: true }));
  const fixture = String.raw`
import hashlib, importlib.util, json, os, pathlib, shutil, subprocess, sys, types, zipfile
upstream, root = map(pathlib.Path, sys.argv[1:])
spec = importlib.util.spec_from_file_location('rehearsal_policy', upstream / 'scripts/release-rehearsal.py')
policy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy)
source = root / 'source'
source.mkdir()
for name in policy.INPUTS:
    target = source / name
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(upstream / name, target)
subprocess.run(['git', 'init', '-q', '-b', 'main', str(source)], check=True)
subprocess.run(['git', 'add', '.'], cwd=source, check=True)
subprocess.run(['git', '-c', 'user.name=Rehearsal fixture', '-c', 'user.email=fixture@kin.invalid',
                '-c', 'commit.gpgsign=false', 'commit', '-q', '-s', '-m', 'Create rehearsal input fixture'], cwd=source, check=True)
candidate = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=source, text=True).strip()
helper_name = 'scripts/acceptance/runtime_fixture.py'
helper = source / helper_name
helper_bytes = helper.read_bytes()
helper_hash = hashlib.sha256(helper_bytes).hexdigest()
assert policy.identity(source, candidate, checked_out=True)['inputs_sha256'][helper_name] == helper_hash
receipt_path = root / 'imports.json'
policy.imports(types.SimpleNamespace(source=source, candidate=candidate, output=receipt_path))
receipt = json.loads(receipt_path.read_text())
assert receipt['status'] == 'PASS' and receipt['candidate'] == candidate
assert set(receipt['staged']) == {'magic', 'brownfield'}
for suite, entry in receipt['staged'].items():
    assert entry['runtime_fixture_sha256'] == helper_hash
    assert entry['sibling_sha256'] == hashlib.sha256((source / 'scripts/acceptance/trace_pages.py').read_bytes()).hexdigest()
    assert (root / 'imports' / suite / 'runtime_fixture.py').read_bytes() == helper_bytes

# The archive contains inert fixture bytes; only staging and provenance run.
directory = root / 'archive'
directory.mkdir()
archive = directory / 'kin-windows-x86_64.zip'
components = {'kin.exe': b'fixture CLI', 'kin-daemon.exe': b'fixture daemon'}
with zipfile.ZipFile(archive, 'w') as bundle:
    for name, body in components.items():
        bundle.writestr(name, body)
archive_hash = hashlib.sha256(archive.read_bytes()).hexdigest()
pathlib.Path(str(archive) + '.sha256').write_text(archive_hash + '  ' + archive.name + '\n')
lock = hashlib.sha256((source / 'Cargo.lock').read_bytes()).hexdigest()
version = json.loads((source / 'packages/kin/package.json').read_text())['version']
os.environ.update(GITHUB_RUN_ID='123', GITHUB_REPOSITORY='fixture/kin')
manifest = {'schema_version': 2, 'artifact': 'kin-windows-x86_64',
            'kin': {'commit': candidate, 'cargo_lock_sha256': lock, 'embedded_dependency_provenance': lock},
            'release_candidate': {'version': version},
            'archive': {'name': archive.name, 'sha256': archive_hash},
            'workflow': {'run_id': 123, 'repository': 'fixture/kin'},
            'archive_contents': [{'name': name, 'sha256': hashlib.sha256(body).hexdigest()} for name, body in components.items()]}
(directory / 'kin-windows-x86_64.provenance.json').write_text(json.dumps(manifest))
args = types.SimpleNamespace(source=source, candidate=candidate, directory=directory)
policy.stage_candidate(args)
assert (directory / 'install.ps1').read_bytes() == (source / 'scripts/install.ps1').read_bytes()
identity = json.loads((directory / 'candidate-identity.json').read_text())
assert identity['commit'] == candidate and identity['archive_sha256'] == archive_hash
assert identity['components'] == {name: hashlib.sha256(body).hexdigest() for name, body in components.items()}
helper.write_bytes(helper_bytes + b'\n# changed fixture helper\n')
try:
    policy.stage_candidate(args)
except ValueError as error:
    assert helper_name in str(error) and 'committed candidate bytes' in str(error)
else:
    raise AssertionError('candidate staging accepted changed runtime helper bytes')
`;
  const result = spawnSync('python3', ['-c', fixture, root, staged], { encoding: 'utf8', timeout: 30000 });
  assert.equal(result.status, 0, result.stderr || result.stdout);
});

test('the preflight resolves the shim beside itself rather than the fleet tool', () => {
  const preflight = fs.readFileSync(path.join(HERE, 'bin', 'kin-release-preflight'), 'utf8');
  assert.match(preflight, /KIN_LANE="\$BIN_DIR\/kin-lane"/);
  assert.match(preflight, /\[ -x "\$KIN_LANE" \] \|\| die/);
  // And it copies itself before running, which is why `bash -n` on the vendored
  // copy is a meaningful check here rather than a formality.
  assert.equal(execFileSync('bash', ['-n', path.join(HERE, 'bin', 'kin-release-preflight')], { encoding: 'utf8' }), '');
  assert.equal(execFileSync('bash', ['-n', path.join(HERE, 'bin', 'kin-release-preflight.d', 'proof-flow.sh')], { encoding: 'utf8' }), '');
  assert.equal(execFileSync('bash', ['-n', SHIM], { encoding: 'utf8' }), '');
});

test('the standalone rehearsal policy pins legacy preparation and preserves candidate proof controls', (t) => {
  const python = spawnSync('python3', ['--version'], { encoding: 'utf8' });
  if (python.error) return t.skip('python3 unavailable');
  const root = path.join(HERE, '..', '..');
  const staged = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-rehearsal-receipts-'));
  t.after(() => fs.rmSync(staged, { recursive: true, force: true }));
  const fixture = String.raw`
import copy, importlib.util, json, os, pathlib, sys, types
from unittest import mock
upstream, root = map(pathlib.Path, sys.argv[1:])
def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, upstream / 'scripts' / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
policy = load('rehearsal_policy', 'release-rehearsal.py')
compat = load('compatibility_policy', 'release-rehearsal-compat.py')
assert policy.BASELINE_IDENTITY == compat.BASELINE
assert policy.BASELINE_ARCHIVE == compat.BASELINE_ARCHIVE
preparation = {'schema': 'kin.legacy-baseline-preparation.v1', 'status': 'PASS',
               'coverage': 'verified-legacy-archive-materialization',
               'source_archive': dict(copy.deepcopy(policy.BASELINE_ARCHIVE), checksum_sha256='d' * 64),
               'installed': {}, 'candidate_installer_invoked': False, 'modern_managed_admission': False}
installed = {}
for filename, pin in policy.BASELINE_ARCHIVE['components'].items():
    entry = dict(pin, path='C:/ci/.kin/bin/' + filename)
    preparation['installed'][filename] = entry
    installed[filename.removesuffix('.exe')] = dict(entry, version_text=filename.removesuffix('.exe') + ' 0.8.6')
compatibility = {'schema': 'kin.release-rehearsal-compat.v1', 'status': 'PASS',
                 'coverage': 'compatibility-baseline', 'baseline': copy.deepcopy(policy.BASELINE_IDENTITY),
                 'preparation': preparation, 'installed': installed}
source = {'candidate': 'a' * 40, 'source_tree': 'b' * 40, 'inputs_sha256': {'Cargo.lock': 'c' * 64}}
common = dict(source, schema=policy.SCHEMA, status='PASS', run_id=123, repository='fixture/kin',
              mode='compatibility-baseline', baseline_tag=policy.BASELINE, event='push', ref='refs/heads/main')
tooling = dict(common, leg='tooling', native_handoff='PASS',
               imports={'status': 'PASS', 'candidate': source['candidate']},
               environment={'status': 'PASS', 'candidate': source['candidate']})
def receipt_for(value):
    return dict(common, legs={'windows': dict(common, leg='windows', baseline=value['baseline'], compatibility=value),
                              'tooling': copy.deepcopy(tooling)})
policy.validate_compatibility_baseline(compatibility)
policy.validate_receipt(receipt_for(compatibility), source, 123, 'fixture/kin', 'compatibility-baseline')
output = root / 'windows.json'
(root / 'compatibility').mkdir()
def record(value):
    (root / 'compatibility/result.json').write_text(json.dumps(value))
    args = types.SimpleNamespace(source=upstream, candidate=source['candidate'], leg='windows',
                                 mode='compatibility-baseline', release_tag=policy.BASELINE, output=output)
    with mock.patch.object(policy, 'identity', return_value=source), mock.patch.dict(os.environ,
            GITHUB_RUN_ID='123', GITHUB_REPOSITORY='fixture/kin', GITHUB_EVENT_NAME='push', GITHUB_REF='refs/heads/main'):
        policy.record(args)
record(compatibility)
assert json.loads(output.read_text())['compatibility'] == compatibility
output.unlink()
def refuse(call):
    try:
        call()
    except ValueError:
        return
    raise AssertionError('receipt policy accepted a substituted legacy proof')
cases = [
    (('preparation',), None),
    (('baseline', 'commit'), '1' * 40),
    (('baseline', 'lock_sha256'), '2' * 64),
    (('preparation', 'source_archive', 'sha256'), '3' * 64),
    (('preparation', 'source_archive', 'checksum_sha256'), ''),
    (('preparation', 'source_archive', 'components', 'kin-daemon.exe', 'sha256'), '4' * 64),
    (('preparation', 'source_archive', 'components', 'kin.exe', 'size_bytes'), float(105574912)),
    (('preparation', 'installed', 'kin.exe', 'path'), 'C:/elsewhere/kin.exe'),
    (('preparation', 'installed', 'kin-daemon.exe', 'sha256'), '5' * 64),
    (('installed', 'kin-daemon', 'size_bytes'), 1),
    (('preparation', 'candidate_installer_invoked'), True),
    (('preparation', 'modern_managed_admission'), True),
    (('preparation', 'modern_managed_admission'), 0),
    (('preparation', 'schema'), 'other.preparation.v1'),
    (('preparation', 'status'), 'FAIL'),
    (('preparation', 'coverage'), 'candidate-full'),
]
for keys, value in cases:
    poisoned = copy.deepcopy(compatibility)
    target = poisoned
    for key in keys[:-1]:
        target = target[key]
    target[keys[-1]] = value
    refuse(lambda: policy.validate_receipt(receipt_for(poisoned), source, 123, 'fixture/kin', 'compatibility-baseline'))
    refuse(lambda: record(poisoned))
    assert not output.exists()
missing_component = copy.deepcopy(compatibility)
del missing_component['preparation']['source_archive']['components']['kin-daemon.exe']
refuse(lambda: policy.validate_receipt(receipt_for(missing_component), source, 123, 'fixture/kin', 'compatibility-baseline'))
outer = receipt_for(compatibility)
outer['legs']['windows']['baseline'] = dict(policy.BASELINE_IDENTITY, commit='6' * 40)
refuse(lambda: policy.validate_receipt(outer, source, 123, 'fixture/kin', 'compatibility-baseline'))

# Candidate qualification remains its separate exact archive proof.
full_common = dict(common, mode='candidate-full', baseline_tag=None)
full_tooling = dict(tooling, mode='candidate-full', baseline_tag=None)
binary = {'commit': source['candidate'], 'lock_sha256': source['inputs_sha256']['Cargo.lock'],
          'archive_sha256': '7' * 64, 'provenance_sha256': '8' * 64, 'version': '0.8.7',
          'components': {'kin.exe': '9' * 64, 'kin-daemon.exe': 'e' * 64}}
full = dict(full_common, legs={'tooling': full_tooling,
            'windows': dict(full_common, leg='windows', candidate_binary=binary, full_install_proof='PASS')})
policy.validate_receipt(full, source, 123, 'fixture/kin')
full['legs']['windows']['candidate_binary']['commit'] = 'f' * 40
refuse(lambda: policy.validate_receipt(full, source, 123, 'fixture/kin'))
`;
  const result = spawnSync('python3', ['-c', fixture, root, staged], { encoding: 'utf8', timeout: 30000 });
  assert.equal(result.status, 0, result.stderr || result.stdout);
});

test('legacy archive preparation and setup contract controls run in the release proof gate', (t) => {
  const python = spawnSync('python3', ['--version'], { encoding: 'utf8' });
  if (python.error) return t.skip('python3 unavailable');
  const root = path.join(HERE, '..', '..');
  const result = spawnSync('python3', [path.join(root, 'scripts', 'test-release-rehearsal-compat.py')],
    { env: process.env, encoding: 'utf8', timeout: 30000 });
  assert.equal(result.status, 0, result.stderr || result.stdout);
});
