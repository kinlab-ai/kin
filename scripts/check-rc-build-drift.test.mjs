#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// A contract check that cannot fail is not evidence. Every case below breaks
// the producer and consumer contract in one specific way and requires the named
// complaint, so the green run on the tree means the two workflows actually
// agree: RC Build produces every byte, release.yml only consumes.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import test from "node:test";
import assert from "node:assert/strict";

import {
  activeLines,
  check,
  checkFiles,
  jobBody,
  RELEASE_WORKFLOW,
  RC_WORKFLOW,
} from "./check-rc-build-drift.mjs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const release = readFileSync(join(ROOT, RELEASE_WORKFLOW), "utf8");
const rc = readFileSync(join(ROOT, RC_WORKFLOW), "utf8");

const replaceOnce = (text, from, to) => {
  assert.equal(text.split(from).length - 1, 1, `anchor must match exactly once: ${from.slice(0, 80)}`);
  // A function replacement, because job bodies carry `$'` and `$&`, which a
  // string replacement would expand.
  return text.replace(from, () => to);
};
const only = (problems) => {
  assert.equal(problems.length, 1, `expected one problem, got:\n${problems.join("\n")}`);
  return problems[0];
};
const inJob = (text, job, from, to) => {
  const body = jobBody(text, job, "fixture");
  return replaceOnce(text, body, replaceOnce(body, from, to));
};

test("the workflows in the tree agree", () => {
  assert.deepEqual(checkFiles(ROOT), []);
});

test("a job body stops before the next job's header comment", () => {
  const body = jobBody(release, "locate_rc_build", RELEASE_WORKFLOW);
  assert.ok(!/^ {2}#/m.test(body.split("\n").slice(1).join("\n")));
  assert.ok(!body.includes("publish_daemon_image:"));
});

// 1. release.yml compiles nothing.

test("a compile in release.yml is reported, whatever compiles", () => {
  for (const [line, needle] of [
    ['          cargo build --locked --release --target "$TARGET" -p kin-cli -p kin-daemon', /runs cargo/],
    ['          cross build --locked --release --target "$TARGET"', /runs cross/],
    ["          rustup target add x86_64-unknown-linux-musl", /runs rustup/],
    ["          docker buildx build --file Dockerfile .", /runs a Docker build/],
    ["          docker build .", /runs a Docker build/],
    ['          cp "target/${TARGET}/release/kin" .', /reads a Cargo target directory/],
  ]) {
    const mutated = inJob(release, "ship_rc_archives", "      - name: Download the RC archive\n", `      - name: Sneak in a compile\n        run: |\n${line}\n\n      - name: Download the RC archive\n`);
    assert.match(only(check(mutated, rc)), needle, line);
  }
  const toolchain = inJob(release, "sign_macos", "      - name: Download the RC archive\n", "      - name: Install Rust stable\n        uses: ./.github/actions/rust-toolchain\n\n      - name: Download the RC archive\n");
  assert.match(only(check(toolchain, rc)), /installs the Rust toolchain/);
});

test("prose and registry reads are not compiles", () => {
  const commented = inJob(release, "ship_rc_archives", "      - name: Download the RC archive\n", "      # cargo build used to run here.\n      - name: Download the RC archive\n");
  assert.deepEqual(check(commented, rc), []);
  // publish_daemon_image already runs `docker buildx imagetools inspect`.
  assert.match(jobBody(release, "publish_daemon_image", RELEASE_WORKFLOW), /docker buildx imagetools inspect/);
});

// 2. rc-build.yml produces exactly the located archives and the image.

test("an archive RC Build stops building, or builds for another target, is reported", () => {
  const dropped = replaceOnce(rc, "          - os: depot-windows-2022-16\n            target: x86_64-pc-windows-msvc\n            artifact: kin-windows-x86_64\n            cross: false\n            github_os: windows-latest\n", "");
  const problems = check(release, dropped);
  assert.ok(problems.some((problem) => /builds kin-linux-x86_64, .* but scripts\/locate-rc-build\.mjs locates/.test(problem)), problems.join("\n"));
  const retargeted = replaceOnce(rc, "            target: aarch64-unknown-linux-musl\n            artifact: kin-linux-aarch64", "            target: aarch64-unknown-linux-gnu\n            artifact: kin-linux-aarch64");
  assert.match(only(check(release, retargeted)), /builds kin-linux-aarch64 for aarch64-unknown-linux-gnu/);
});

test("the image must be built once, as an OCI archive, and uploaded under the located name", () => {
  const twice = inJob(rc, "image", "      - name: Record the image's digests\n", "      - name: Build again\n        run: docker buildx build --load .\n\n      - name: Record the image's digests\n");
  assert.match(only(check(release, twice)), /build the daemon image exactly once, found 2/);
  const loaded = inJob(rc, "image", '--output "type=oci,dest=', '--output "type=docker,dest=');
  assert.match(only(check(release, loaded)), /as an OCI archive/);
  const renamed = inJob(rc, "image", "          name: kin-daemon-image\n", "          name: daemon-image\n");
  assert.match(only(check(release, renamed)), /upload the kin-daemon-image artifact/);
});

test("every archive and the image must be attested, after both producers finish", () => {
  const unattested = inJob(rc, "attest", "            ${{ runner.temp }}/rc/kin-daemon-image/kin-daemon-image.oci.tar\n", "");
  assert.match(only(check(release, unattested)), /attest job attests .* expected .*kin-daemon-image\.oci\.tar/);
  const early = inJob(rc, "attest", "    needs: [build, image]\n", "    needs: build\n");
  assert.match(only(check(release, early)), /wait for both the build and image jobs/);
});

// 3. release.yml ships exactly what rc-build.yml builds, from the located run.

test("an archive release.yml stops shipping, or ships from the wrong job, is reported", () => {
  const dropped = inJob(release, "ship_rc_archives", "          - os: ubuntu-24.04-arm\n            target: aarch64-unknown-linux-musl\n            artifact: kin-linux-aarch64\n", "");
  assert.match(only(check(dropped, rc)), /builds kin-linux-aarch64, which .* does not ship/);
  const unsigned = inJob(
    release,
    "ship_rc_archives",
    "          - os: windows-latest\n",
    "          - os: macos-latest\n            target: aarch64-apple-darwin\n            artifact: kin-macos-arm64\n          - os: windows-latest\n",
  );
  const problems = check(unsigned, rc);
  assert.ok(problems.some((problem) => /ship_rc_archives ships kin-macos-arm64 \(aarch64-apple-darwin\), which it may not/.test(problem)), problems.join("\n"));
  assert.ok(problems.some((problem) => /ships kin-macos-arm64, which .* does not build/.test(problem)), problems.join("\n"));
});

test("a publish inventory that drifts from the RC archives is reported", () => {
  const short = inJob(release, "publish", "            kin-windows-x86_64.tar.gz\n          )", "          )");
  assert.match(only(check(short, rc)), /publishes .* but RC Build builds/);
});

test("a consumer that takes bytes from anywhere but the located run, or skips a proof, is reported", () => {
  const elsewhere = inJob(release, "sign_macos", "          run-id: ${{ needs.locate_rc_build.outputs.run_id }}\n", "");
  assert.match(only(check(elsewhere, rc)), /sign_macos downloads from somewhere other than the located RC Build run/);
  const unattested = inJob(release, "publish_daemon_image", "          node scripts/locate-rc-build.mjs verify-attestation \\\n", "          true \\\n");
  assert.match(only(check(unattested, rc)), /publish_daemon_image must run scripts\/locate-rc-build\.mjs verify-attestation/);
  const unverified = inJob(release, "ship_rc_archives", "          node scripts/locate-rc-build.mjs verify-artifact \\\n", "          true \\\n");
  assert.match(only(check(unverified, rc)), /ship_rc_archives must run scripts\/locate-rc-build\.mjs verify-artifact/);
});

// 4. One native provenance rule, run by producer and consumer.

test("a consumer that may skip executing what it ships is reported", () => {
  const skipping = inJob(release, "ship_rc_archives", "            --out \"$RUNNER_TEMP/kin-release-provenance-$TARGET\" \\\n            --require-execution\n", "            --out \"$RUNNER_TEMP/kin-release-provenance-$TARGET\"\n");
  assert.match(only(check(skipping, rc)), /ship_rc_archives must run .* --require-execution 1 time/);
  const unsignedOnly = inJob(release, "sign_macos", "      - name: Verify native release build provenance of the signed binaries\n", "      - name: Verify native release build provenance of the signed binaries\n        if: false\n");
  assert.deepEqual(check(unsignedOnly, rc), [], "a condition is not this check's concern; the authority suite pins the step");
});

test("an inline copy of the provenance check is reported in either workflow", () => {
  const inline = inJob(release, "ship_rc_archives", "      - name: Download the RC archive\n", "      - name: Inline\n        run: |\n          echo \"kin bench-meta did not prove a clean, known release source\"\n\n      - name: Download the RC archive\n");
  assert.match(only(check(inline, rc)), /release\.yml carries an inline copy/);
  const producer = replaceOnce(rc, "          node scripts/verify-native-release-provenance.mjs \\\n", "          node scripts/other-check.mjs \\\n");
  assert.match(only(check(release, producer)), /rc-build\.yml's build job must run node scripts\/verify-native-release-provenance\.mjs once, found 0/);
});

// 5. rc-build.yml holds no release credential.

test("a secret, an environment or a signing step in rc-build.yml is reported", () => {
  const secret = inJob(rc, "build", "        env:\n          TARGET: ${{ matrix.target }}\n\n      - name: Build kin-cli + kin-daemon (cross)", "        env:\n          TARGET: ${{ matrix.target }}\n          KEY: ${{ secrets.MACOS_CERTIFICATE }}\n\n      - name: Build kin-cli + kin-daemon (cross)");
  assert.match(only(check(release, secret)), /reads a secret/);
  const environment = inJob(rc, "image", "    timeout-minutes: 45\n", "    timeout-minutes: 45\n    environment: release\n");
  assert.match(only(check(release, environment)), /declares an environment/);
  const signing = inJob(rc, "image", "      - name: Checkout\n", "      - name: Sign macOS binaries\n        run: true\n\n      - name: Checkout\n");
  assert.match(only(check(release, signing)), /carries the signing step "Sign macOS binaries"/);
  const unsigned = inJob(release, "sign_macos", "      - name: Sign macOS binaries\n", "      - name: Skip signing\n");
  assert.match(only(check(unsigned, rc)), /sign_macos job no longer carries "Sign macOS binaries"/);
});

// The properties that make rc-build.yml safe to run on a merge group or on a
// dispatch of an arbitrary ref. Each is one line to break.

test("rc-build.yml grants nothing beyond contents: read except the attest job's signing", () => {
  const block = /\npermissions:\n((?: {2}\S.*\n)+)/.exec(rc);
  assert.ok(block, "rc-build.yml declares no workflow permissions block");
  assert.deepEqual(block[1].trimEnd().split("\n"), ["  contents: read"]);
  for (const job of ["build", "image", "capability", "attest"]) {
    const body = jobBody(rc, job, RC_WORKFLOW);
    const permissions = /\n    permissions:\n((?: {6}\S.*\n)+)/.exec(`${body}\n`);
    const granted = permissions ? permissions[1].trimEnd().split("\n").map((line) => line.trim()) : ["(inherits contents: read)"];
    const allowed = job === "attest"
      ? ["contents: read", "id-token: write", "attestations: write"]
      : job === "image" ? ["contents: read"] : ["(inherits contents: read)"];
    assert.deepEqual(granted, allowed, `${job} permissions`);
  }
  const attest = jobBody(rc, "attest", RC_WORKFLOW);
  assert.ok(!attest.includes("actions/checkout@"), "the job holding the OIDC token checks out no repository code");
  assert.ok(!/^\s+run: .*(node|bash) scripts\//m.test(attest), "the attest job runs no repository script");
});

test("rc-build.yml writes to and reads from no cache", () => {
  const uses = [...rc.matchAll(/^\s*uses:\s*(\S+)/gm)].map((match) => match[1]);
  assert.ok(uses.length > 0, "rc-build.yml uses no actions at all, so this guard cannot fire");
  for (const action of uses) {
    assert.ok(!/cache/i.test(action), `rc-build.yml uses a caching action: ${action}`);
  }
  const active = activeLines(rc).join("\n");
  assert.ok(!/--cache-(from|to)|cache-(from|to):/.test(active), "rc-build.yml's image build uses a build cache");
});

test("rc-build.yml publishes nothing", () => {
  for (const forbidden of ["npm publish", "gh release", "cargo publish", "docker push", "softprops/action-gh-release", "docker://", "packages: write"]) {
    assert.equal(rc.includes(forbidden), false, `rc-build.yml contains ${forbidden}`);
  }
});

test("rc-build.yml runs for the merge queue on main and by dispatch, and takes no ref input", () => {
  const block = /\non:\n([\s\S]*?)(?=\npermissions:)/.exec(rc);
  assert.ok(block, "could not read rc-build.yml triggers");
  const triggers = block[1].split("\n").filter((line) => /^ {2}[^#\s]/.test(line));
  assert.deepEqual(triggers, ["  merge_group:", "  workflow_dispatch:"]);
  assert.match(block[1], /merge_group:\n {4}types: \[checks_requested\]/);
  assert.equal(/^\s*inputs:/m.test(block[1]), false, "rc-build.yml declares a dispatch input");
  // Only a queue for main may build. The capability and attest jobs depend on
  // these two, so they skip with them.
  const mainOnly = "if: ${{ github.event_name != 'merge_group' || github.event.merge_group.base_ref == 'refs/heads/main' }}";
  for (const job of ["build", "image"]) {
    assert.ok(jobBody(rc, job, RC_WORKFLOW).includes(`    ${mainOnly}`), `${job} admits a merge queue for another base`);
  }
  assert.match(jobBody(rc, "capability", RC_WORKFLOW), /\n    needs: build\n/);
});

// The selector reads RC Build by name and by job outcome. A build leg is the
// only job called "RC Build (<artifact>)", and a job that needs the build
// matrix must end skipped, never cancelled or run anyway, when a leg fails or
// times out, or a timeout reads as a spent attempt.
test("only build legs are named RC Build, and the jobs after the matrix skip with it", () => {
  assert.match(rc, /^name: RC Build$/m);
  const names = [...rc.matchAll(/^ {2}([a-z_-]+):\n {4}name: (.*)$/gm)].map((match) => [match[1], match[2]]);
  assert.deepEqual(
    names.filter(([, name]) => name.startsWith("RC Build (")),
    [["build", "RC Build (${{ matrix.artifact }})"]],
  );
  for (const job of ["capability", "attest"]) {
    const body = jobBody(rc, job, RC_WORKFLOW);
    assert.match(body, /\n {4}needs: (build|\[build, image\])\n/, `${job} needs the build matrix`);
    assert.ok(!/^ {4}if:/m.test(body), `${job} carries a job condition, so it may run or cancel instead of skipping`);
  }
});

// The merge queue gives a group 90 minutes to report. A build leg at its
// merge-group bound plus the longest job that waits on it must fit inside.
test("every merge-group bound fits the merge queue's 90-minute window", () => {
  const bound = (job) => {
    const line = jobBody(rc, job, RC_WORKFLOW).match(/^ {4}timeout-minutes: (.*)$/m)?.[1];
    assert.ok(line, `${job} declares no timeout`);
    const merge = line.match(/github\.event_name == 'merge_group' && (\d+) \|\| (\d+)/);
    return merge ? Number(merge[1]) : Number(line);
  };
  const build = bound("build");
  const image = bound("image");
  const after = Math.max(bound("capability"), bound("attest"));
  assert.ok(build <= 60, `build legs are bounded at ${build} in a merge group`);
  assert.ok(Math.max(build, image) + after <= 90, `worst case ${Math.max(build, image) + after} minutes`);
});
