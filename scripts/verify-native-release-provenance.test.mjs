// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  executionPlan,
  hostArch,
  judgeBuildMeta,
  judgeVersionLines,
  machoArch,
  parseArgs,
  verify,
} from "./verify-native-release-provenance.mjs";

const SCRIPT = fileURLToPath(new URL("./verify-native-release-provenance.mjs", import.meta.url));
const SHA = "a".repeat(40);
const LOCK = "lock bytes\n";
const LOCK_SHA = createHash("sha256").update(LOCK).digest("hex");

function meta(over = {}) {
  return {
    kin_version: "0.8.3",
    kin_commit: SHA,
    kin_dirty: false,
    kin_source_known: true,
    dependency_provenance: LOCK_SHA,
    feature_flags: ["vector", "external-embeddings", "http"],
    embeddings: {
      vector_enabled: true,
      embeddings_enabled: true,
      local_inference_enabled: false,
      external_embeddings_enabled: true,
      metal_enabled: false,
      provider: "none",
    },
    ...over,
  };
}

const judge = (record, over = {}) =>
  judgeBuildMeta(record, {
    expectedCommit: SHA,
    expectedVersion: "0.8.3",
    lockSha: LOCK_SHA,
    target: "x86_64-unknown-linux-musl",
    ...over,
  });

test("a native host executes its own architecture, Rosetta covers x86_64 macOS, and the rest is a skip", () => {
  assert.equal(executionPlan("X64", "x86_64-unknown-linux-musl"), "native");
  assert.equal(executionPlan("ARM64", "aarch64-apple-darwin"), "native");
  assert.equal(executionPlan("X64", "x86_64-pc-windows-msvc"), "native");
  assert.equal(executionPlan("ARM64", "x86_64-apple-darwin"), "rosetta");
  assert.equal(executionPlan("ARM64", "x86_64-unknown-linux-musl"), "skip");
  assert.equal(executionPlan("X64", "aarch64-apple-darwin"), "skip");
  assert.equal(machoArch("aarch64-apple-darwin"), "arm64");
  assert.equal(machoArch("x86_64-apple-darwin"), "x86_64");
  assert.equal(hostArch({ RUNNER_ARCH: "ARM64" }, "x64"), "ARM64");
  assert.equal(hostArch({}, "x64"), "X64");
  assert.equal(hostArch({}, "arm64"), "ARM64");
});

test("both version lines must name the expected clean commit", () => {
  assert.equal(judgeVersionLines([`kin 0.8.3 (${SHA} 2026-10-12)`, `kin-daemon 0.8.3 (${SHA} 2026-10-12)`], SHA), SHA);
  assert.throws(() => judgeVersionLines(["kin 0.8.3", `kin-daemon 0.8.3 (${SHA} x)`], SHA), /lacks a full build SHA/);
  assert.throws(() => judgeVersionLines([`kin 0.8.3 (${SHA}-dirty x)`, `kin-daemon (${SHA} x)`], SHA), /built dirty/);
  assert.throws(
    () => judgeVersionLines([`kin (${"b".repeat(40)} x)`, `kin-daemon (${"b".repeat(40)} x)`], SHA),
    /do not match checkout/,
  );
  assert.throws(() => judgeVersionLines([`kin (${SHA} x)`, `kin-daemon (${"b".repeat(40)} x)`], SHA), /do not match/);
});

test("bench-meta must prove the version, a clean known source, the lock and the default profile", () => {
  judge(meta());
  judge(meta({ kin_version: "9.9.9" }), { expectedVersion: undefined });
  assert.throws(() => judge(meta({ kin_version: "0.8.2" })), /does not match release tag 0\.8\.3/);
  assert.throws(() => judge(meta({ kin_commit: "b".repeat(40) })), /clean, known release source/);
  assert.throws(() => judge(meta({ kin_dirty: true })), /clean, known release source/);
  assert.throws(() => judge(meta({ kin_source_known: false })), /clean, known release source/);
  assert.throws(() => judge(meta({ dependency_provenance: "c".repeat(64) })), /does not match tagged Cargo\.lock/);
  assert.throws(() => judge(meta({ feature_flags: ["vector"] })), /did not enable external-embeddings/);
  assert.throws(() => judge(meta({ feature_flags: ["vector", "external-embeddings", "metal"] })), /unexpectedly enabled metal/);
  assert.throws(
    () => judge(meta({ embeddings: { ...meta().embeddings, local_inference_enabled: true } })),
    /engine-free default archive profile/,
  );
});

function fakeExec(outputs) {
  const calls = [];
  const exec = (file, args) => {
    calls.push([file, ...args]);
    const key = [path.basename(file), ...args].join(" ");
    if (!(key in outputs)) throw new Error(`unexpected call ${key}`);
    const value = outputs[key];
    if (value instanceof Error) throw value;
    return value;
  };
  return { exec, calls };
}

function fixture() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-native-provenance-"));
  fs.writeFileSync(path.join(dir, "Cargo.lock"), LOCK);
  return dir;
}

const goodOutputs = (over = {}) => ({
  "kin --version": `kin 0.8.3 (${SHA} 2026-10-12)\n`,
  "kin-daemon --version": `kin-daemon 0.8.3 (${SHA} 2026-10-12)\n`,
  "kin bench-meta --json": JSON.stringify(meta()),
  ...over,
});

function argsFor(dir, over = {}) {
  return {
    kin: path.join(dir, "kin"),
    daemon: path.join(dir, "kin-daemon"),
    target: "aarch64-apple-darwin",
    expectCommit: SHA,
    expectVersion: "0.8.3",
    cargoLock: path.join(dir, "Cargo.lock"),
    out: path.join(dir, "out"),
    requireExecution: true,
    ...over,
  };
}

test("a macOS leg checks both binaries are thin Mach-O of the target and records the evidence it ran", () => {
  const dir = fixture();
  const { exec, calls } = fakeExec(
    goodOutputs({
      [`lipo -archs ${path.join(dir, "kin")}`]: "arm64\n",
      [`lipo -archs ${path.join(dir, "kin-daemon")}`]: "arm64\n",
    }),
  );
  const result = verify(argsFor(dir), { env: { RUNNER_ARCH: "ARM64" }, exec, log: () => {} });
  assert.equal(result.executed, true);
  assert.equal(result.lockSha, LOCK_SHA);
  assert.deepEqual(
    calls.filter((call) => call[0] === "lipo").map((call) => call[2]),
    [path.join(dir, "kin"), path.join(dir, "kin-daemon")],
  );
  assert.equal(JSON.parse(fs.readFileSync(path.join(dir, "out", "kin-build-meta.json"), "utf8")).kin_commit, SHA);
  assert.match(fs.readFileSync(path.join(dir, "out", "kin-version.txt"), "utf8"), /kin 0\.8\.3/);
});

test("a fat or wrong-architecture Mach-O is refused before anything runs", () => {
  const dir = fixture();
  const { exec, calls } = fakeExec({ [`lipo -archs ${path.join(dir, "kin")}`]: "x86_64 arm64\n" });
  assert.throws(
    () => verify(argsFor(dir), { env: { RUNNER_ARCH: "ARM64" }, exec, log: () => {} }),
    /is a x86_64 arm64 Mach-O, expected arm64/,
  );
  assert.ok(calls.every((call) => call[0] === "lipo"));
});

test("a host that cannot execute the archive skips only when the caller allows it", () => {
  const dir = fixture();
  const { exec, calls } = fakeExec({});
  const skipped = verify(argsFor(dir, { target: "aarch64-unknown-linux-musl", requireExecution: false }), {
    env: { RUNNER_ARCH: "X64" },
    exec,
    log: () => {},
  });
  assert.equal(skipped.executed, false);
  assert.equal(calls.length, 0);
  assert.throws(
    () =>
      verify(argsFor(dir, { target: "aarch64-unknown-linux-musl" }), {
        env: { RUNNER_ARCH: "X64" },
        exec,
        log: () => {},
      }),
    /cannot execute aarch64-unknown-linux-musl/,
  );
});

test("an x86_64 macOS archive on Apple Silicon needs Rosetta 2 and refuses without it", () => {
  const dir = fixture();
  const { exec } = fakeExec({ "arch -x86_64 /usr/bin/true": new Error("Bad CPU type") });
  assert.throws(
    () =>
      verify(argsFor(dir, { target: "x86_64-apple-darwin" }), {
        env: { RUNNER_ARCH: "ARM64" },
        exec,
        log: () => {},
      }),
    /Rosetta 2 cannot run x86_64-apple-darwin/,
  );
});

test("a binary from another commit or a different lock fails the leg", () => {
  const dir = fixture();
  const other = "b".repeat(40);
  for (const [outputs, expected] of [
    [goodOutputs({ "kin --version": `kin 0.8.3 (${other} x)\n` }), /do not match checkout/],
    [goodOutputs({ "kin bench-meta --json": JSON.stringify(meta({ dependency_provenance: "d".repeat(64) })) }), /Cargo\.lock/],
    [goodOutputs({ "kin bench-meta --json": "not json" }), /did not print JSON/],
  ]) {
    const { exec } = fakeExec(outputs);
    assert.throws(
      () =>
        verify(argsFor(dir, { target: "x86_64-unknown-linux-musl" }), {
          env: { RUNNER_ARCH: "X64" },
          exec,
          log: () => {},
        }),
      expected,
    );
  }
});

test("the argument parser refuses a loose commit and missing paths", () => {
  const exit = process.exit;
  const write = process.stderr.write;
  process.exit = (code) => {
    throw new Error(`exit ${code}`);
  };
  process.stderr.write = () => true;
  try {
    assert.throws(() => parseArgs(["--kin", "k"]), /exit 2/);
    assert.throws(
      () =>
        parseArgs([
          "--kin", "k", "--daemon", "d", "--target", "t", "--expect-commit", "HEAD",
          "--cargo-lock", "c", "--out", "o",
        ]),
      /exit 2/,
    );
  } finally {
    process.exit = exit;
    process.stderr.write = write;
  }
  const parsed = parseArgs([
    "--kin", "k", "--daemon", "d", "--target", "t", "--expect-commit", SHA,
    "--cargo-lock", "c", "--out", "o", "--require-execution",
  ]);
  assert.equal(parsed.requireExecution, true);
  assert.equal(parsed.expectVersion, undefined);
});

// One end-to-end run through real processes, so the CLI wiring is exercised and
// not only the functions it calls.
test("the CLI runs real executables and exits non-zero on a wrong commit", { skip: process.platform === "win32" }, () => {
  const dir = fixture();
  const script = (name, version) =>
    `#!/bin/sh\nif [ "$1" = bench-meta ]; then cat "${path.join(dir, "meta.json")}"; else echo "${name} 0.8.3 (${version} 2026-10-12)"; fi\n`;
  fs.writeFileSync(path.join(dir, "meta.json"), JSON.stringify(meta()));
  fs.writeFileSync(path.join(dir, "kin"), script("kin", SHA), { mode: 0o755 });
  fs.writeFileSync(path.join(dir, "kin-daemon"), script("kin-daemon", SHA), { mode: 0o755 });
  const arch = process.arch === "arm64" ? "ARM64" : "X64";
  const target = process.arch === "arm64" ? "aarch64-unknown-linux-musl" : "x86_64-unknown-linux-musl";
  const cli = (commit) =>
    spawnSync(
      process.execPath,
      [
        SCRIPT,
        "--kin", path.join(dir, "kin"),
        "--daemon", path.join(dir, "kin-daemon"),
        "--target", target,
        "--expect-commit", commit,
        "--expect-version", "0.8.3",
        "--cargo-lock", path.join(dir, "Cargo.lock"),
        "--out", path.join(dir, "out"),
        "--require-execution",
      ],
      // A Linux target keeps lipo out of the run on a macOS developer host,
      // because the Mach-O check is keyed on the target.
      { encoding: "utf8", env: { ...process.env, RUNNER_ARCH: arch } },
    );
  const good = cli(SHA);
  assert.equal(good.status, 0, good.stderr);
  assert.equal(JSON.parse(fs.readFileSync(path.join(dir, "out", "kin-build-meta.json"), "utf8")).kin_version, "0.8.3");
  const bad = cli("b".repeat(40));
  assert.equal(bad.status, 1);
  assert.match(bad.stderr, /::error::release binary SHAs/);
});
