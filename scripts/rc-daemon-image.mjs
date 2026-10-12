#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The daemon image RC Build builds once and release.yml pushes unchanged.
//
// rc-build.yml's image job exports the image as an OCI image layout archive,
// which keeps every compressed layer, the config and the manifest as exact
// blobs, so `skopeo copy --preserve-digests` at the tag serves the same
// manifest digest this job recorded. Two commands:
//
//   record        write kin-daemon-image.json: the commit, the archive's
//                 sha256, and the manifest and config digests read from the
//                 layout by scripts/locate-rc-build.mjs's own reader, which is
//                 what release.yml checks the record against.
//   check-loaded  prove the image the smoke tests run is the image in the
//                 archive: the loaded image's layer diff ids and runtime
//                 config must equal the archive's config blob. The image id is
//                 not compared, because the classic Docker store names an
//                 image by its config digest and the containerd store by its
//                 manifest digest.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { fileURLToPath } from "node:url";

import { RC_IMAGE, readOciLayout } from "./locate-rc-build.mjs";

const COMMIT_SHA = /^[0-9a-f]{40}$/;
// The runtime fields a container is started with. A loaded image that agrees
// on these and on every layer runs exactly what the archive holds.
export const RUNTIME_FIELDS = Object.freeze(["Entrypoint", "Cmd", "Env", "User", "WorkingDir", "ExposedPorts", "Volumes", "Labels"]);

const run = (file, args) =>
  execFileSync(file, args, { encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], maxBuffer: 64 * 1024 * 1024 });

function withLayout(archive, exec, use) {
  const work = mkdtempSync(join(tmpdir(), "kin-daemon-image-"));
  try {
    exec("tar", ["-xf", archive, "-C", work]);
    return use(work, readOciLayout(work));
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

export function record({ archive, commit, env = process.env, exec = run }) {
  if (!COMMIT_SHA.test(commit ?? "")) throw new Error(`--commit must be a 40-character commit sha, got ${commit}`);
  if (basename(archive) !== RC_IMAGE.archive) throw new Error(`the image archive must be named ${RC_IMAGE.archive}`);
  const { manifestDigest, configDigest } = withLayout(archive, exec, (_, layout) => layout);
  return {
    schema_version: 1,
    commit,
    platform: RC_IMAGE.platform,
    archive: { name: RC_IMAGE.archive, sha256: createHash("sha256").update(readFileSync(archive)).digest("hex") },
    manifest_digest: manifestDigest,
    config_digest: configDigest,
    workflow: {
      repository: env.GITHUB_REPOSITORY,
      run_id: env.GITHUB_RUN_ID,
      run_attempt: env.GITHUB_RUN_ATTEMPT,
    },
  };
}

const normalize = (value) => (value === undefined || value === null ? null : value);

export function compareLoaded(config, inspected) {
  const want = config?.rootfs?.diff_ids;
  const have = inspected?.RootFS?.Layers;
  if (!Array.isArray(want) || want.length === 0 || JSON.stringify(want) !== JSON.stringify(have)) {
    throw new Error("the loaded image's layers are not the archive's layers");
  }
  for (const field of RUNTIME_FIELDS) {
    const archived = normalize(config?.config?.[field]);
    const loaded = normalize(inspected?.Config?.[field]);
    if (JSON.stringify(archived) !== JSON.stringify(loaded)) {
      throw new Error(`the loaded image's ${field} is not the archive's`);
    }
  }
}

export function checkLoaded({ archive, image, exec = run }) {
  return withLayout(archive, exec, (root, { configDigest }) => {
    const config = JSON.parse(readFileSync(join(root, "blobs", "sha256", configDigest.slice("sha256:".length)), "utf8"));
    const [inspected] = JSON.parse(exec("docker", ["image", "inspect", image]));
    compareLoaded(config, inspected);
    return configDigest;
  });
}

function parseArgs(argv) {
  const [command, ...rest] = argv;
  const args = { command };
  for (let i = 0; i < rest.length; i += 2) {
    if (!rest[i]?.startsWith("--") || rest[i + 1] === undefined) {
      throw new Error("usage: rc-daemon-image.mjs record --archive <file> --commit <sha> --out <file> | check-loaded --archive <file> --image <ref>");
    }
    args[rest[i].slice(2)] = rest[i + 1];
  }
  return args;
}

export function main(argv = process.argv.slice(2)) {
  const args = parseArgs(argv);
  if (args.command === "record") {
    const written = record({ archive: args.archive, commit: args.commit });
    writeFileSync(args.out, `${JSON.stringify(written, null, 2)}\n`);
    console.log(`${args.out}: manifest ${written.manifest_digest}, config ${written.config_digest}`);
    return;
  }
  if (args.command === "check-loaded") {
    const configDigest = checkLoaded({ archive: args.archive, image: args.image });
    console.log(`${args.image} runs the archive's layers and config (${configDigest})`);
    return;
  }
  throw new Error(`unknown command ${args.command ?? "<none>"}; expected record or check-loaded`);
}

function isDirectRun() {
  const entry = process.argv[1];
  if (!entry) return false;
  const self = fileURLToPath(import.meta.url);
  if (entry === self) return true;
  try {
    return realpathSync(entry) === realpathSync(self);
  } catch {
    return false;
  }
}

if (isDirectRun()) {
  try {
    main();
  } catch (error) {
    console.error(`::error::${error.message}`);
    process.exit(1);
  }
}
