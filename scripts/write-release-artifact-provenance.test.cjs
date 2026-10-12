// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

"use strict";

const assert = require("node:assert/strict");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const test = require("node:test");

const { SENTINEL_BYTES } = require("./read-update-build-identity.cjs");
const { buildManifest } = require("./write-release-artifact-provenance.cjs");

const SHA = "a".repeat(40);
const LOCK = "lock bytes\n";
const LOCK_SHA = crypto.createHash("sha256").update(LOCK).digest("hex");
const sha256 = (bytes) => crypto.createHash("sha256").update(bytes).digest("hex");

// A component carrying the static build identity the updater reads, in the
// layout read-update-build-identity.cjs parses.
function component(label, { version = "0.8.4", commit = SHA } = {}) {
  const sentinel = Buffer.alloc(SENTINEL_BYTES);
  Buffer.from("00894b494e555044415445010d0a1a0a", "hex").copy(sentinel, 0);
  sentinel.write("kin.update-build.v1", 16, "ascii");
  sentinel.write(version, 40, "ascii");
  sentinel.write(commit, 72, "ascii");
  sentinel[112] = 1;
  sentinel[113] = 1;
  sentinel.write(LOCK_SHA, 114, "ascii");
  sentinel.writeUInt32LE(7, 178);
  Buffer.from("00894b494e454e445631ff010d0a1a0a", "hex").copy(sentinel, 182);
  return Buffer.concat([Buffer.from(`${label} prefix`), sentinel, Buffer.from("suffix")]);
}

const TARGETS = {
  "kin-linux-x86_64": { target: "x86_64-unknown-linux-musl", names: ["kin", "kin-daemon"], archives: ["kin-linux-x86_64.tar.gz"] },
  "kin-macos-aarch64": { target: "aarch64-apple-darwin", names: ["kin", "kin-daemon"], archives: ["kin-macos-aarch64.tar.gz"] },
  "kin-windows-x86_64": {
    target: "x86_64-pc-windows-msvc",
    names: ["kin.exe", "kin-daemon.exe"],
    archives: ["kin-windows-x86_64.zip", "kin-windows-x86_64.tar.gz"],
  },
};

// The RC Build side: a content root, its archives, and the candidate manifest
// RC Build wrote, all consistent. `ship` then edits what the release saw.
function fixture(artifact, { componentOptions = {}, rcEdit = () => {}, ship = () => {} } = {}) {
  const spec = TARGETS[artifact];
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-release-provenance-"));
  const root = path.join(dir, "root");
  const archives = path.join(dir, "archives");
  fs.mkdirSync(root);
  fs.mkdirSync(archives);
  for (const name of spec.names) fs.writeFileSync(path.join(root, name), component(name, componentOptions));
  for (const doc of ["README.md", "INSTALL.md", "checksums-sha256.txt"]) fs.writeFileSync(path.join(root, doc), doc);
  if (spec.target.endsWith("-apple-darwin")) {
    fs.mkdirSync(path.join(root, "KinNotifier.app", "Contents", "MacOS"), { recursive: true });
    fs.writeFileSync(path.join(root, "KinNotifier.app", "Contents", "MacOS", "KinNotifier"), "notifier", { mode: 0o755 });
    fs.writeFileSync(path.join(root, "KinNotifier.app", "Contents", "Info.plist"), "plist");
  }
  for (const name of spec.archives) fs.writeFileSync(path.join(archives, name), `archive ${name}`);
  fs.writeFileSync(path.join(dir, "Cargo.lock"), LOCK);
  fs.writeFileSync(
    path.join(dir, "build-meta.json"),
    JSON.stringify({ kin_commit: SHA, dependency_provenance: LOCK_SHA }),
  );
  const record = (name) => ({ name, sha256: sha256(fs.readFileSync(path.join(archives, name))), size_bytes: fs.statSync(path.join(archives, name)).size });
  const contents = spec.names
    .map((name) => {
      const bytes = fs.readFileSync(path.join(root, name));
      return {
        name,
        sha256: sha256(bytes),
        size_bytes: bytes.length,
        build_identity: {
          schema: "kin.update-build.v1",
          version: componentOptions.version ?? "0.8.4",
          commit: componentOptions.commit ?? SHA,
          clean: true,
          source_known: true,
          dependency_provenance: LOCK_SHA,
          graph_snapshot_version: 7,
        },
      };
    })
    .sort((left, right) => left.name.localeCompare(right.name));
  const [primary, ...additional] = spec.archives;
  const rc = {
    schema_version: 2,
    release_candidate: { ref: SHA, version: "0.8.4" },
    artifact,
    target: spec.target,
    kin: { commit: SHA, cargo_lock_sha256: LOCK_SHA, embedded_dependency_provenance: LOCK_SHA },
    archive: record(primary),
    ...(additional.length ? { additional_archives: additional.map(record) } : {}),
    archive_contents: contents,
    workflow: { repository: "kinlab-ai/kin", run_id: "4242", run_attempt: "1" },
  };
  rcEdit(rc);
  fs.writeFileSync(path.join(dir, "rc.json"), JSON.stringify(rc));
  ship({ root, archives, dir });
  return {
    artifact,
    target: spec.target,
    contentRoot: root,
    archiveDir: archives,
    buildMeta: path.join(dir, "build-meta.json"),
    rcManifest: path.join(dir, "rc.json"),
    rcRunId: "4242",
    commit: SHA,
    cargoLock: path.join(dir, "Cargo.lock"),
    tag: "v0.8.4",
  };
}

const env = { GITHUB_REPOSITORY: "kinlab-ai/kin", GITHUB_RUN_ID: "9001", GITHUB_RUN_ATTEMPT: "1" };

test("a Linux archive ships as RC Build made it, with the release tag and its RC Build run", () => {
  const manifest = buildManifest(fixture("kin-linux-x86_64"), env);
  assert.equal(manifest.schema_version, 2);
  assert.equal(manifest.release_tag, "v0.8.4");
  assert.equal(manifest.workflow.run_id, "9001");
  assert.deepEqual(Object.keys(manifest), [
    "schema_version", "release_tag", "artifact", "target", "kin", "archive", "archive_contents", "workflow", "rc_build",
  ]);
  assert.equal(manifest.rc_build.workflow, ".github/workflows/rc-build.yml");
  assert.equal(manifest.rc_build.run_id, "4242");
  assert.equal(manifest.rc_build.archive.sha256, manifest.archive.sha256);
  assert.deepEqual(manifest.archive_contents.map((record) => record.name), ["kin", "kin-daemon"]);
});

test("a Windows archive keeps the zip primary and inventories the tarball beside it", () => {
  const manifest = buildManifest(fixture("kin-windows-x86_64"), env);
  assert.equal(manifest.archive.name, "kin-windows-x86_64.zip");
  assert.deepEqual(manifest.additional_archives.map((record) => record.name), ["kin-windows-x86_64.tar.gz"]);
  assert.deepEqual(manifest.rc_build.additional_archives.map((record) => record.name), ["kin-windows-x86_64.tar.gz"]);
  assert.throws(
    () => buildManifest(fixture("kin-windows-x86_64", { ship: ({ archives }) => fs.rmSync(path.join(archives, "kin-windows-x86_64.tar.gz")) }), env),
    /expected final archives kin-windows-x86_64\.tar\.gz, kin-windows-x86_64\.zip/,
  );
});

test("a byte that differs from RC Build's record of an unsigned platform refuses", () => {
  assert.throws(
    () => buildManifest(fixture("kin-linux-x86_64", { ship: ({ archives }) => fs.writeFileSync(path.join(archives, "kin-linux-x86_64.tar.gz"), "rebuilt") }), env),
    /names other bytes for kin-linux-x86_64\.tar\.gz/,
  );
  assert.throws(
    () => buildManifest(fixture("kin-windows-x86_64", { ship: ({ archives }) => fs.writeFileSync(path.join(archives, "kin-windows-x86_64.tar.gz"), "rebuilt") }), env),
    /names other bytes for kin-windows-x86_64\.tar\.gz/,
  );
  assert.throws(
    () => buildManifest(fixture("kin-linux-x86_64", { rcEdit: (rc) => { rc.archive_contents[0].sha256 = "0".repeat(64); } }), env),
    /inventories other component bytes/,
  );
});

test("a signed macOS archive may differ in bytes but never in static build identity", () => {
  const signed = fixture("kin-macos-aarch64", {
    ship: ({ root, archives }) => {
      fs.appendFileSync(path.join(root, "kin"), "signature");
      fs.writeFileSync(path.join(archives, "kin-macos-aarch64.tar.gz"), "signed archive");
    },
  });
  const manifest = buildManifest(signed, env);
  assert.notEqual(manifest.archive.sha256, manifest.rc_build.archive.sha256);
  assert.throws(
    () => buildManifest(fixture("kin-macos-aarch64", { rcEdit: (rc) => { rc.archive_contents[0].build_identity.graph_snapshot_version = 8; } }), env),
    /records another static build identity for kin/,
  );
});

test("a release manifest about another run, commit, version or lock refuses", () => {
  for (const [label, options, override, needle] of [
    ["another RC Build run", { rcEdit: (rc) => { rc.workflow.run_id = "1"; } }, {}, /written by run 1, not the located run 4242/],
    ["another candidate commit", { rcEdit: (rc) => { rc.release_candidate.ref = "b".repeat(40); } }, {}, /is about another commit/],
    ["a release manifest as the RC record", { rcEdit: (rc) => { rc.release_tag = "v0.8.4"; } }, {}, /is not a candidate manifest/],
    ["another artifact", { rcEdit: (rc) => { rc.artifact = "kin-linux-aarch64"; } }, {}, /another artifact or target/],
    ["another tag version", {}, { tag: "v0.8.5" }, /static build identity does not match the tagged release source/],
    ["binaries from another commit", { componentOptions: { commit: "b".repeat(40) } }, {}, /static build identity does not match/],
  ]) {
    assert.throws(() => buildManifest({ ...fixture("kin-linux-x86_64", options), ...override }, env), needle, label);
  }
  const meta = fixture("kin-linux-x86_64");
  fs.writeFileSync(meta.buildMeta, JSON.stringify({ kin_commit: SHA, dependency_provenance: "0".repeat(64) }));
  assert.throws(() => buildManifest(meta, env), /embedded build provenance does not match/);
});
