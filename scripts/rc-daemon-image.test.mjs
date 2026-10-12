// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import { RC_IMAGE, verifyArtifact } from "./locate-rc-build.mjs";
import { checkLoaded, compareLoaded, record } from "./rc-daemon-image.mjs";

const SHA = "a".repeat(40);
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

const CONFIG = {
  architecture: "amd64",
  os: "linux",
  config: {
    Entrypoint: ["/usr/local/bin/kin-entrypoint"],
    Env: ["PATH=/usr/local/bin:/usr/bin"],
    User: "kin",
    ExposedPorts: { "4219/tcp": {} },
  },
  rootfs: { type: "layers", diff_ids: [`sha256:${"1".repeat(64)}`] },
};

function archive() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-image-record-"));
  const layout = path.join(dir, "layout");
  const blobs = path.join(layout, "blobs", "sha256");
  fs.mkdirSync(blobs, { recursive: true });
  const put = (bytes) => {
    fs.writeFileSync(path.join(blobs, sha256(bytes)), bytes);
    return { digest: `sha256:${sha256(bytes)}`, size: bytes.length };
  };
  const layer = put(Buffer.from("layer"));
  const config = put(Buffer.from(JSON.stringify(CONFIG)));
  const manifest = put(Buffer.from(JSON.stringify({
    schemaVersion: 2,
    mediaType: "application/vnd.oci.image.manifest.v1+json",
    config: { mediaType: "application/vnd.oci.image.config.v1+json", ...config },
    layers: [{ mediaType: "application/vnd.oci.image.layer.v1.tar+gzip", ...layer }],
  })));
  fs.writeFileSync(path.join(layout, "oci-layout"), "{}");
  fs.writeFileSync(path.join(layout, "index.json"), JSON.stringify({
    schemaVersion: 2,
    manifests: [{ mediaType: "application/vnd.oci.image.manifest.v1+json", ...manifest }],
  }));
  const file = path.join(dir, RC_IMAGE.archive);
  execFileSync("tar", ["-cf", file, "-C", layout, "oci-layout", "index.json", "blobs"]);
  return { dir, file, manifestDigest: manifest.digest, configDigest: config.digest };
}

const env = { GITHUB_REPOSITORY: "kinlab-ai/kin", GITHUB_RUN_ID: "77", GITHUB_RUN_ATTEMPT: "1" };

test("the record names the archive's own digests and is what release.yml verifies", () => {
  const built = archive();
  const written = record({ archive: built.file, commit: SHA, env });
  assert.equal(written.manifest_digest, built.manifestDigest);
  assert.equal(written.config_digest, built.configDigest);
  assert.equal(written.archive.sha256, sha256(fs.readFileSync(built.file)));
  // Laid out as the artifact upload carries it, the consumer accepts it.
  const artifact = path.join(built.dir, "artifact");
  fs.mkdirSync(artifact);
  fs.copyFileSync(built.file, path.join(artifact, RC_IMAGE.archive));
  fs.writeFileSync(path.join(artifact, `${RC_IMAGE.archive}.sha256`), `${written.archive.sha256}  ${RC_IMAGE.archive}\n`);
  fs.writeFileSync(path.join(artifact, RC_IMAGE.record), JSON.stringify(written));
  const digests = verifyArtifact(artifact, RC_IMAGE.artifact, {
    repository: "kinlab-ai/kin", sha: SHA, version: "0.8.4", runId: "77", runAttempt: "1", preflight: {},
  });
  assert.equal(digests.manifest_digest, built.manifestDigest);
  assert.throws(() => record({ archive: built.file, commit: "HEAD", env }), /40-character/);
});

test("the loaded image must carry the archive's layers and runtime config", () => {
  const inspected = { RootFS: { Layers: CONFIG.rootfs.diff_ids }, Config: { ...CONFIG.config, Cmd: null, Labels: null } };
  compareLoaded(CONFIG, inspected);
  assert.throws(() => compareLoaded(CONFIG, { ...inspected, RootFS: { Layers: [`sha256:${"2".repeat(64)}`] } }), /layers are not the archive's/);
  assert.throws(() => compareLoaded(CONFIG, { ...inspected, Config: { ...inspected.Config, User: "root" } }), /User is not the archive's/);
  assert.throws(() => compareLoaded({ ...CONFIG, rootfs: { diff_ids: [] } }, inspected), /layers/);

  const built = archive();
  const exec = (file, args) => {
    if (file === "tar") return execFileSync(file, args, { encoding: "utf8" });
    assert.deepEqual([file, ...args], ["docker", "image", "inspect", "kin:ci"]);
    return JSON.stringify([inspected]);
  };
  assert.equal(checkLoaded({ archive: built.file, image: "kin:ci", exec }), built.configDigest);
});
