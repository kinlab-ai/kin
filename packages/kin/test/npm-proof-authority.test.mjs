// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { assertCentralAuthority, assertDurableAuthority } from '../../../scripts/lib/npm-proof-authority.mjs';

function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'kin-npm-authority-control-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const repoDir = path.join(root, 'repo');
  const storesDir = path.join(root, 'stores');
  const store = path.join(storesDir, 'fixture-store');
  fs.mkdirSync(repoDir);
  fs.mkdirSync(store, { recursive: true });
  fs.writeFileSync(path.join(store, 'manifest.json'), '{}');
  return { root, repoDir, storesDir, store, answer: {
    schema: 'kin.store-path.v1', working_dir: repoDir, store, external: true,
  } };
}

test('central discovery accepts the exact fixture without an in-tree .kin', (t) => {
  const f = fixture(t);
  assertCentralAuthority(f.answer, f);
});


test('Windows extended paths identify the same store and exact working tree', {
  skip: process.platform !== 'win32',
}, (t) => {
  const f = fixture(t);
  const namespaced = {
    ...f.answer,
    working_dir: path.toNamespacedPath(f.repoDir),
    store: path.toNamespacedPath(f.store),
  };
  assertCentralAuthority(namespaced, f);
  assert.throws(() => assertCentralAuthority({
    ...namespaced, working_dir: path.toNamespacedPath(f.root),
  }, f), /another working tree/);
});

test('an enclosing repository cannot qualify the fixture', (t) => {
  const f = fixture(t);
  assert.throws(() => assertCentralAuthority({ ...f.answer, working_dir: f.root }, f), /another working tree/);
});

test('a store outside the isolated census cannot qualify the fixture', (t) => {
  const f = fixture(t);
  assert.throws(() => assertCentralAuthority({ ...f.answer, store: f.repoDir }, f), /outside the isolated census/);
});

test('relative discovery paths and unknown schemas are refused', (t) => {
  const f = fixture(t);
  assert.throws(() => assertCentralAuthority({ ...f.answer, store: 'stores/fixture-store' }, f), /absolute/);
  assert.throws(() => assertCentralAuthority({ ...f.answer, schema: 'other' }, f));
});

test('missing, empty and directory manifests cannot qualify admission', (t) => {
  const f = fixture(t);
  const manifest = path.join(f.store, 'manifest.json');
  fs.unlinkSync(manifest);
  assert.throws(() => assertCentralAuthority(f.answer, f), /ENOENT/);
  fs.writeFileSync(manifest, '');
  assert.throws(() => assertCentralAuthority(f.answer, f), /non-empty manifest/);
  fs.unlinkSync(manifest);
  fs.mkdirSync(manifest);
  assert.throws(() => assertCentralAuthority(f.answer, f), /non-empty manifest/);
});

test('multiple admitted stores cannot qualify a fresh fixture', (t) => {
  const f = fixture(t);
  const other = path.join(f.storesDir, 'unexpected');
  fs.mkdirSync(other);
  fs.writeFileSync(path.join(other, 'manifest.json'), '{}');
  assert.throws(() => assertCentralAuthority(f.answer, f), /exactly one/);
});

test('in-tree placement cannot stand in for default central admission', (t) => {
  const f = fixture(t);
  assert.throws(() => assertCentralAuthority({ ...f.answer, external: false }, f), /central store/);
  fs.mkdirSync(path.join(f.repoDir, '.kin'));
  assert.throws(() => assertCentralAuthority(f.answer, f), /working tree clean/);
});

test('status must attest the current repository format and verified source CAS', () => {
  const status = { schema: 'kin.status.v3', authority: 'repository-v6', repository: { source_cas_verified: true } };
  assertDurableAuthority(status);
  for (const bad of [
    { ...status, schema: 'other' },
    { ...status, authority: 'directory' },
    { ...status, repository: { source_cas_verified: false } },
    { ...status, repository: undefined },
  ]) assert.throws(() => assertDurableAuthority(bad));
});
