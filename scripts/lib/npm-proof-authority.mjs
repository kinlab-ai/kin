// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Assertions for the real first-run proof, also exercised with offline refusal
// controls. Product discovery and status supply authority; directory existence
// alone cannot establish which repository a store serves.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';

function canonical(candidate) {
  // Rust records Windows paths with the extended-length prefix. The native
  // resolver asks the OS for directory identity; the JS walker can retain a
  // caller's prefixed spelling and compare it unequal to the same directory.
  const resolved = fs.realpathSync.native(candidate);
  return process.platform === 'win32' ? resolved.toLowerCase() : resolved;
}

export function assertCentralAuthority(answer, { repoDir, storesDir }) {
  assert.equal(answer.schema, 'kin.store-path.v1');
  assert.equal(typeof answer.working_dir, 'string');
  assert.equal(typeof answer.store, 'string');
  assert.ok(path.isAbsolute(answer.working_dir), 'working tree path must be absolute');
  assert.ok(path.isAbsolute(answer.store), 'store path must be absolute');
  assert.equal(canonical(answer.working_dir), canonical(repoDir), 'store serves another working tree');
  assert.equal(answer.external, true, 'default admission must use a central store');
  assert.equal(canonical(path.dirname(answer.store)), canonical(storesDir), 'store is outside the isolated census');
  const manifest = fs.statSync(path.join(answer.store, 'manifest.json'));
  assert.ok(manifest.isFile() && manifest.size > 0, 'admitted store needs a non-empty manifest');
  const stores = fs.readdirSync(storesDir, { withFileTypes: true }).filter((entry) =>
    entry.isDirectory() && fs.existsSync(path.join(storesDir, entry.name, 'manifest.json')),
  );
  assert.equal(stores.length, 1, 'fixture must publish exactly one central store');
  assert.equal(canonical(path.join(storesDir, stores[0].name)), canonical(answer.store));
  assert.equal(fs.existsSync(path.join(repoDir, '.kin')), false, 'default admission must keep the working tree clean');
}

export function assertDurableAuthority(status) {
  assert.equal(status.schema, 'kin.status.v3');
  assert.equal(status.authority, 'repository-v6');
  assert.equal(status.repository?.source_cas_verified, true);
}
