#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Write one platform's release provenance manifest for the archive release.yml
// is about to publish, and bind it to the RC Build bytes it came from.
//
// The manifest is the one `kin update`, the public install proof and the
// publish job have always read: schema 2, the release tag, the Kin commit and
// Cargo.lock, the archive digests, the per-component inventory with each
// executable's static build identity, and this workflow run. It used to be
// written by release.yml's build job about bytes that job had just compiled.
// release.yml now compiles nothing, so it is written about the RC Build
// archive, and it adds `rc_build`, the run and the archive digests the bytes
// came from.
//
// What it refuses, against the candidate manifest RC Build wrote for the same
// archive:
//   - a different commit, run, artifact or target;
//   - a different component inventory, or a static build identity that moved
//     (a code signature does not touch it);
//   - for every platform but macOS, any byte of any archive or component that
//     differs from RC Build's, because those archives ship exactly as built;
//   - for macOS, nothing more here: scripts/compare-signed-macos-archive.mjs
//     has already proved the signed archive differs only by its signatures.

"use strict";

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

const { readUpdateBuildIdentity } = require("./read-update-build-identity.cjs");
const { classifyReleaseArchiveRoot } = require("./release-archive-shape.cjs");

const RC_BUILD_WORKFLOW = ".github/workflows/rc-build.yml";
const COMMIT_SHA = /^[0-9a-f]{40}$/;

const sha256 = (file) => crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");

function usage(message) {
  throw new Error(
    `${message}\nusage: write-release-artifact-provenance.cjs --artifact <name> --target <triple> ` +
      "--content-root <dir> --archive-dir <dir> --build-meta <file> --rc-manifest <file> --rc-run-id <id> " +
      "--commit <sha> --cargo-lock <file> --tag <tag> --out <file>",
  );
}

function parseArgs(argv) {
  const keys = {
    "--artifact": "artifact",
    "--target": "target",
    "--content-root": "contentRoot",
    "--archive-dir": "archiveDir",
    "--build-meta": "buildMeta",
    "--rc-manifest": "rcManifest",
    "--rc-run-id": "rcRunId",
    "--commit": "commit",
    "--cargo-lock": "cargoLock",
    "--tag": "tag",
    "--out": "out",
  };
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    const key = keys[argv[i]];
    if (!key || argv[i + 1] === undefined) usage(`unknown or valueless argument ${argv[i]}`);
    args[key] = argv[i + 1];
  }
  for (const [flag, key] of Object.entries(keys)) {
    if (!args[key]) usage(`${flag} is required`);
  }
  if (!COMMIT_SHA.test(args.commit)) usage(`--commit must be a 40-character commit sha, got ${args.commit}`);
  if (!/^[1-9][0-9]*$/.test(args.rcRunId)) usage(`--rc-run-id must be a run id, got ${args.rcRunId}`);
  return args;
}

function buildManifest(args, env = process.env) {
  const { artifact, target } = args;
  const windowsTarget = target.includes("-windows-");
  const signedTarget = target.endsWith("-apple-darwin");
  const authorityNames = new Set(windowsTarget ? ["kin.exe", "kin-daemon.exe"] : ["kin", "kin-daemon"]);
  const kinCommit = args.commit;
  const kinLockSha = sha256(args.cargoLock);
  const expectedVersion = args.tag.replace(/^v/, "");

  const buildMeta = JSON.parse(fs.readFileSync(args.buildMeta, "utf8"));
  if (buildMeta.kin_commit !== kinCommit || buildMeta.dependency_provenance !== kinLockSha) {
    throw new Error("embedded build provenance does not match the tagged Kin source and Cargo.lock");
  }

  // Windows publishes two containers of the same components: the zip every
  // Windows resolver asks for and the tarball the POSIX installer builds its
  // name from. The zip stays the primary archive because `kin update` and the
  // public install proof find the platform record by that exact name.
  const primaryArchive = windowsTarget ? `${artifact}.zip` : `${artifact}.tar.gz`;
  const additionalArchiveNames = windowsTarget ? [`${artifact}.tar.gz`] : [];
  const expectedArchives = [primaryArchive, ...additionalArchiveNames].sort();
  const archives = [`${artifact}.tar.gz`, `${artifact}.zip`]
    .filter((file) => fs.existsSync(path.join(args.archiveDir, file)))
    .sort();
  if (JSON.stringify(archives) !== JSON.stringify(expectedArchives)) {
    throw new Error(
      `expected final archives ${expectedArchives.join(", ")} for ${artifact}, found ${archives.join(", ") || "none"}`,
    );
  }
  const archiveRecord = (name) => {
    const file = path.join(args.archiveDir, name);
    return { name, sha256: sha256(file), size_bytes: fs.statSync(file).size };
  };

  // The content inventory covers the archive's component files and nothing
  // else. An installed updater refuses any inventory name outside its managed
  // component list, so the notification bundle and the documentation members
  // are classified and shape-checked here but stay out of the manifest.
  const contents = classifyReleaseArchiveRoot(args.contentRoot, { target })
    .files.map((name) => {
      const file = path.join(args.contentRoot, name);
      const record = { name, sha256: sha256(file), size_bytes: fs.statSync(file).size };
      if (authorityNames.has(name)) {
        record.build_identity = readUpdateBuildIdentity(file);
      }
      return record;
    })
    .sort((left, right) => left.name.localeCompare(right.name));
  if (contents.length < 2) {
    throw new Error(`${artifact} does not contain the mandatory kin and kin-daemon binaries`);
  }
  const authorityRecords = contents.filter((record) => authorityNames.has(record.name));
  if (authorityRecords.length !== authorityNames.size) {
    throw new Error(`${artifact} does not contain both static build-identity authorities`);
  }
  const graphVersions = new Set();
  for (const record of authorityRecords) {
    const identity = record.build_identity;
    if (
      identity.version !== expectedVersion ||
      identity.commit !== kinCommit ||
      identity.clean !== true ||
      identity.source_known !== true ||
      identity.dependency_provenance !== kinLockSha
    ) {
      throw new Error(`${record.name} static build identity does not match the tagged release source`);
    }
    graphVersions.add(identity.graph_snapshot_version);
  }
  if (graphVersions.size !== 1) {
    throw new Error(`${artifact} CLI and daemon graph snapshot identities disagree`);
  }

  // The RC Build manifest for the same archive, which the located run wrote.
  const rc = JSON.parse(fs.readFileSync(args.rcManifest, "utf8"));
  const fail = (condition, message) => {
    if (!condition) throw new Error(`${artifact} RC Build manifest ${message}`);
  };
  fail(rc.schema_version === 2 && rc.release_tag === undefined, "is not a candidate manifest");
  fail(rc.release_candidate?.ref === kinCommit && rc.kin?.commit === kinCommit, `is about another commit than ${kinCommit}`);
  fail(rc.release_candidate?.version === expectedVersion, `names version ${rc.release_candidate?.version}, not ${expectedVersion}`);
  fail(rc.artifact === artifact && rc.target === target, "is about another artifact or target");
  fail(String(rc.workflow?.run_id) === args.rcRunId, `was written by run ${rc.workflow?.run_id}, not the located run ${args.rcRunId}`);
  fail(rc.kin?.cargo_lock_sha256 === kinLockSha, "names another Cargo.lock");
  const rcContents = Array.isArray(rc.archive_contents) ? rc.archive_contents : [];
  fail(
    JSON.stringify(rcContents.map((record) => record.name)) === JSON.stringify(contents.map((record) => record.name)),
    "inventories other components",
  );
  for (const record of authorityRecords) {
    const before = rcContents.find((entry) => entry.name === record.name);
    fail(
      JSON.stringify(before?.build_identity) === JSON.stringify(record.build_identity),
      `records another static build identity for ${record.name}`,
    );
  }

  const archive = archiveRecord(primaryArchive);
  const additional = additionalArchiveNames.map(archiveRecord);
  if (!signedTarget) {
    // These archives ship exactly as RC Build made them.
    fail(rc.archive?.name === archive.name && rc.archive?.sha256 === archive.sha256, `names other bytes for ${archive.name}`);
    for (const record of additional) {
      const before = (rc.additional_archives ?? []).find((entry) => entry.name === record.name);
      fail(before?.sha256 === record.sha256, `names other bytes for ${record.name}`);
    }
    fail(JSON.stringify(rcContents) === JSON.stringify(contents), "inventories other component bytes");
  }

  return {
    schema_version: 2,
    release_tag: args.tag,
    artifact,
    target,
    kin: {
      commit: kinCommit,
      cargo_lock_sha256: kinLockSha,
      embedded_dependency_provenance: buildMeta.dependency_provenance,
    },
    archive,
    ...(additional.length ? { additional_archives: additional } : {}),
    archive_contents: contents,
    workflow: {
      repository: env.GITHUB_REPOSITORY,
      run_id: env.GITHUB_RUN_ID,
      run_attempt: env.GITHUB_RUN_ATTEMPT,
    },
    // Where the bytes came from. Every platform but macOS ships these exact
    // archives; the macOS archives are these signed.
    rc_build: {
      workflow: RC_BUILD_WORKFLOW,
      run_id: args.rcRunId,
      archive: { name: rc.archive.name, sha256: rc.archive.sha256 },
      ...(rc.additional_archives?.length
        ? { additional_archives: rc.additional_archives.map(({ name, sha256: digest }) => ({ name, sha256: digest })) }
        : {}),
    },
  };
}

module.exports = { RC_BUILD_WORKFLOW, buildManifest, parseArgs };

if (require.main === module) {
  try {
    const args = parseArgs(process.argv.slice(2));
    const manifest = buildManifest(args);
    fs.writeFileSync(args.out, `${JSON.stringify(manifest, null, 2)}\n`);
    console.log(`wrote ${args.out} for ${manifest.archive.name} ${manifest.archive.sha256}`);
  } catch (error) {
    console.error(`::error::${error.message}`);
    process.exit(1);
  }
}
