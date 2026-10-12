// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { compare, expectedChecksumList, normalizeUnsignedMachO } from "./compare-signed-macos-archive.mjs";

const SCRIPTS = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.join(SCRIPTS, "..");

// A minimal thin 64-bit Mach-O header: two LC_SEGMENT_64 commands, __TEXT and
// __LINKEDIT, followed by some body bytes. Enough for the normalizer, which
// reads only the header and load commands.
function machO({ linkeditVmsize = 0x1000n, extra = [], body = "body" } = {}) {
  const segment = (name, vmsize) => {
    const cmd = Buffer.alloc(72);
    cmd.writeUInt32LE(0x19, 0);
    cmd.writeUInt32LE(72, 4);
    cmd.write(name, 8, "latin1");
    cmd.writeBigUInt64LE(0x100000000n, 24);
    cmd.writeBigUInt64LE(vmsize, 32);
    return cmd;
  };
  const commands = [segment("__TEXT", 0x4000n), segment("__LINKEDIT", linkeditVmsize), ...extra];
  const header = Buffer.alloc(32);
  header.writeUInt32LE(0xfeedfacf, 0);
  header.writeUInt32LE(0x0100000c, 4);
  header.writeUInt32LE(commands.length, 16);
  header.writeUInt32LE(commands.reduce((sum, cmd) => sum + cmd.length, 0), 20);
  return Buffer.concat([header, ...commands, Buffer.from(body)]);
}

test("only __LINKEDIT's virtual size is forgiven", () => {
  const original = normalizeUnsignedMachO(machO());
  assert.ok(original.equals(normalizeUnsignedMachO(machO({ linkeditVmsize: 0x8000n }))));
  assert.ok(!original.equals(normalizeUnsignedMachO(machO({ body: "bodz" }))));
  // The input is never modified in place.
  const bytes = machO({ linkeditVmsize: 0x8000n });
  normalizeUnsignedMachO(bytes);
  assert.equal(bytes.readBigUInt64LE(32 + 72 + 32), 0x8000n);
});

test("a binary the normalizer cannot read exactly is refused", () => {
  const signature = Buffer.alloc(16);
  signature.writeUInt32LE(0x1d, 0);
  signature.writeUInt32LE(16, 4);
  assert.throws(() => normalizeUnsignedMachO(machO({ extra: [signature] }), "kin"), /kin still carries a code signature/);
  assert.throws(() => normalizeUnsignedMachO(Buffer.from("#!/bin/sh\n"), "kin"), /not a thin 64-bit/);
  const fat = Buffer.alloc(64);
  fat.writeUInt32BE(0xcafebabe, 0);
  assert.throws(() => normalizeUnsignedMachO(fat, "kin"), /not a thin 64-bit/);
  const truncated = machO();
  truncated.writeUInt32LE(4096, 20);
  assert.throws(() => normalizeUnsignedMachO(truncated, "kin"), /past its end/);
  const noLinkedit = machO();
  noLinkedit.write("__DATA\0\0\0\0", 32 + 72 + 8, "latin1");
  assert.throws(() => normalizeUnsignedMachO(noLinkedit, "kin"), /0 __LINKEDIT segments/);
});

test("the checksum list is the docs writer's listing of the root", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kin-checksums-"));
  fs.writeFileSync(path.join(root, "kin"), "k");
  fs.writeFileSync(path.join(root, "kin-daemon"), "d");
  fs.mkdirSync(path.join(root, "KinNotifier.app"));
  execFileSync(process.execPath, [path.join(SCRIPTS, "write-release-archive-docs.mjs"), root, "aarch64-apple-darwin", "0.8.3"], {
    stdio: "ignore",
  });
  assert.equal(fs.readFileSync(path.join(root, "checksums-sha256.txt"), "utf8"), expectedChecksumList(root));
});

// The whole path on real bytes, where a Mac can produce them: an RC-shaped
// archive from the repository's own packaging pieces, signed ad hoc with the
// hardened runtime the release uses, packaged again the way release.yml does.
const darwin = process.platform === "darwin" && spawnSync("cc", ["--version"]).status === 0;

function packageArchive(stage, artifact, out, sign, { version = "0.8.3", afterDocs = () => {} } = {}) {
  const root = path.join(stage, artifact);
  if (sign) {
    for (const bin of ["kin", "kin-daemon"]) {
      execFileSync("codesign", ["--force", "--options", "runtime", "--sign", "-", path.join(root, bin)], { stdio: "ignore" });
    }
    execFileSync("codesign", ["--force", "--options", "runtime", "--sign", "-", path.join(root, "KinNotifier.app")], {
      stdio: "ignore",
    });
  }
  execFileSync(process.execPath, [path.join(SCRIPTS, "write-release-archive-docs.mjs"), root, "aarch64-apple-darwin", version], {
    stdio: "ignore",
  });
  afterDocs(root);
  execFileSync("tar", ["czf", out, artifact], { cwd: stage, env: { ...process.env, COPYFILE_DISABLE: "1" } });
}

function rcArchive(dir, artifact) {
  const stage = fs.mkdtempSync(path.join(dir, "rc-stage-"));
  const root = path.join(stage, artifact);
  fs.mkdirSync(root);
  const source = path.join(dir, "main.c");
  fs.writeFileSync(source, "int main(void) { return 0; }\n");
  for (const bin of ["kin", "kin-daemon", "KinNotifier"]) {
    execFileSync("cc", ["-O2", "-o", path.join(bin === "KinNotifier" ? dir : root, bin), source]);
  }
  execFileSync("bash", [path.join(ROOT, "crates/kin-notifier-macos/bundle.sh"), path.join(dir, "KinNotifier"), root, "0.8.3"], {
    stdio: "ignore",
  });
  const archive = path.join(dir, `${artifact}.rc.tar.gz`);
  packageArchive(stage, artifact, archive, false);
  return archive;
}

function signedFrom(dir, rc, artifact, tamper = () => {}, options = {}) {
  const stage = fs.mkdtempSync(path.join(dir, "signed-stage-"));
  execFileSync("tar", ["-xzf", rc, "-C", stage]);
  fs.rmSync(path.join(stage, artifact, "checksums-sha256.txt"));
  tamper(path.join(stage, artifact));
  const archive = path.join(stage, `${artifact}.tar.gz`);
  packageArchive(stage, artifact, archive, true, options);
  return archive;
}

test("a re-signed archive passes and any other change to its bytes refuses", { skip: !darwin }, () => {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "kin-sign-compare-")));
  const artifact = "kin-macos-aarch64";
  const rc = rcArchive(dir, artifact);
  const quiet = () => {};

  const { added } = compare({ rcArchive: rc, signedArchive: signedFrom(dir, rc, artifact), artifact, log: quiet });
  assert.deepEqual(added, ["KinNotifier.app/Contents/_CodeSignature/CodeResources"]);

  for (const [label, tamper, needle, options] of [
    [
      "an executable rebuilt from other code",
      (root) => {
        const source = path.join(dir, "other.c");
        fs.writeFileSync(source, "int main(void) { return 1; }\n");
        execFileSync("cc", ["-O2", "-o", path.join(root, "kin-daemon"), source]);
      },
      /kin-daemon differs from the RC build by more than its code signature/,
    ],
    [
      "documentation written for another version",
      () => {},
      /(INSTALL|README)\.md differs/,
      { version: "0.8.4" },
    ],
    [
      "documentation edited after it was written",
      () => {},
      /INSTALL\.md differs/,
      { afterDocs: (root) => fs.appendFileSync(path.join(root, "INSTALL.md"), "\nextra\n") },
    ],
    [
      "a checksum list that does not name the signed files",
      () => {},
      /checksums-sha256\.txt does not list the signed archive's own files/,
      { afterDocs: (root) => fs.appendFileSync(path.join(root, "checksums-sha256.txt"), `${"0".repeat(64)}  ghost\n`) },
    ],
    ["a stray member", (root) => fs.writeFileSync(path.join(root, "KinNotifier.app/Contents/Resources/extra.txt"), "x"), /unexpected KinNotifier\.app\/Contents\/Resources\/extra\.txt/],
    ["a dropped member", (root) => fs.rmSync(path.join(root, "KinNotifier.app/Contents/Resources/Kin.icns")), /missing KinNotifier\.app\/Contents\/Resources\/Kin\.icns/],
    ["a lost executable bit", (root) => fs.chmodSync(path.join(root, "kin"), 0o644), /executable bits/],
  ]) {
    assert.throws(
      () => compare({ rcArchive: rc, signedArchive: signedFrom(dir, rc, artifact, tamper, options), artifact, log: quiet }),
      needle,
      label,
    );
  }

  // An archive nobody signed is refused, not admitted as "no difference".
  assert.throws(() => compare({ rcArchive: rc, signedArchive: rc, artifact, log: quiet }), /unsigned KinNotifier\.app/);
});
