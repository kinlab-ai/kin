// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { PREFLIGHT_SCHEMA } from "./check-release-proof-artifacts.mjs";
import { buildJobBody, matrixRows } from "./check-rc-build-drift.mjs";
import {
  MERGE_QUEUE_BRANCH,
  PREFLIGHT_REQUIRED,
  RC_ARCHIVES,
  RC_ARTIFACTS,
  RC_BUILD_WORKFLOW,
  RC_IMAGE,
  RELEASE_AUTOMATION_ACTORS,
  candidateBranch,
  judgeArtifacts,
  judgeCandidateManifest,
  judgeRun,
  locate,
  main,
  preflightArchive,
  preflightDigests,
  preflightRun,
  readListing,
  readOciLayout,
  selectRun,
  verifyArtifact,
  verifyAttestation,
  verifyDownloads,
} from "./locate-rc-build.mjs";

const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
const REPO = "kinlab-ai/kin";
const SHA = "a".repeat(40);
const OTHER = "b".repeat(40);
const VERSION = "0.8.4";
const RUN_ID = 4242;
const QUEUE_REF = `gh-readonly-queue/main/pr-1851-${"c".repeat(40)}`;
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

// Archive bytes per file name, distinct so a swap between two archives shows.
const BYTES = Object.fromEntries(
  RC_ARCHIVES.flatMap((entry) => entry.archives).map((name) => [name, Buffer.from(`archive bytes of ${name}\n`)]),
);
const DIGEST = Object.fromEntries(Object.entries(BYTES).map(([name, bytes]) => [name, sha256(bytes)]));

function leg(target, archive = DIGEST[`${target}.tar.gz`], over = {}) {
  return {
    name: `host (${target.slice(4)}, archive)`,
    kind: "host",
    target,
    verdict: "PASS",
    result: { expected: { commit: SHA }, archive: { path: `/tmp/${target}.tar.gz`, sha256: archive } },
    ...over,
  };
}

function preflight(legs = PREFLIGHT_REQUIRED.map((target) => leg(target)), over = {}) {
  return { schema: PREFLIGHT_SCHEMA, verdict: "PASS", allow_dirty: false, legs, ...over };
}

// A run as the API returns it. Merge-queue runs are the default producer; a
// candidate-branch dispatch is the fallback.
function run(over = {}) {
  return {
    id: RUN_ID,
    run_attempt: 1,
    path: RC_BUILD_WORKFLOW,
    repository: { full_name: REPO },
    head_repository: { full_name: REPO },
    head_sha: SHA,
    head_branch: QUEUE_REF,
    event: "merge_group",
    status: "completed",
    conclusion: "success",
    actor: { login: "troyjr4103", type: "User" },
    triggering_actor: { login: "troyjr4103", type: "User" },
    html_url: `https://github.com/${REPO}/actions/runs/${RUN_ID}`,
    ...over,
  };
}

const dispatched = (over = {}) =>
  run({
    event: "workflow_dispatch",
    head_branch: candidateBranch(VERSION),
    actor: { login: "github-actions[bot]", type: "Bot" },
    triggering_actor: { login: "github-actions[bot]", type: "Bot" },
    ...over,
  });

function artifactList(over = {}) {
  return RC_ARTIFACTS.map((name, index) => ({
    id: 900 + index,
    name,
    expired: false,
    ...(over[name] ?? {}),
  })).concat([{ id: 999, name: "candidate-install-kin-linux-x86_64", expired: false }]);
}

const context = { repository: REPO, sha: SHA, version: VERSION };

test("the preflight record maps each judged leg to the RC Build archive it judged", () => {
  const digests = preflightDigests(preflight(), SHA);
  assert.deepEqual(Object.keys(digests).sort(), [...PREFLIGHT_REQUIRED].sort());
  assert.equal(digests["kin-macos-aarch64"], DIGEST["kin-macos-aarch64.tar.gz"]);
  // A record that judged more archives binds more of them.
  const wider = preflightDigests(preflight([...preflight().legs, leg("kin-macos-x86_64")]), SHA);
  assert.equal(wider["kin-macos-x86_64"], DIGEST["kin-macos-x86_64.tar.gz"]);
});

test("a preflight record the release cannot map refuses", () => {
  assert.throws(() => preflightDigests(preflight([...preflight().legs, leg("kin-freebsd-x86_64", "1".repeat(64))]), SHA), /RC Build does not produce/);
  assert.throws(() => preflightDigests(preflight([...preflight().legs, leg("kin-linux-x86_64", "2".repeat(64))]), SHA), /two preflight legs judged kin-linux-x86_64/);
  assert.throws(() => preflightDigests(preflight(preflight().legs.slice(1)), SHA), /judged no kin-linux-aarch64/);
  // The mint's own judge runs first, so its refusals carry through unchanged.
  assert.throws(() => preflightDigests(preflight(undefined, { verdict: "FAIL" }), SHA), /not PASS/);
  assert.throws(() => preflightDigests(preflight(), OTHER), /different build/);
  assert.throws(() => preflightDigests(preflight([leg("kin-linux-x86_64", "not-a-digest")]), SHA), /archive sha256/);
});

test("a merge-queue run for main and Release Cut's dispatch are the two admitted producers", () => {
  assert.deepEqual(judgeRun(run(), context), []);
  // Whoever enqueued the pull request started the merge group, a person or an App.
  assert.deepEqual(judgeRun(run({ actor: { login: "kin-release-bot[bot]", type: "Bot" } }), context), []);
  assert.deepEqual(judgeRun(dispatched(), context), []);
  assert.deepEqual(judgeRun(dispatched({ actor: { login: "kin-release-bot[bot]", type: "Bot" } }), context), []);
  // A person re-running a failed leg changes only triggering_actor.
  assert.deepEqual(judgeRun(dispatched({ triggering_actor: { login: "troyjr4103", type: "User" } }), context), []);
  assert.ok(MERGE_QUEUE_BRANCH.test(QUEUE_REF));
  assert.deepEqual(RELEASE_AUTOMATION_ACTORS, ["github-actions[bot]", "kin-release-bot[bot]"]);
});

test("every identity a run must carry is checked, for both producers", () => {
  for (const [label, candidate, needle] of [
    ["another workflow", run({ path: ".github/workflows/release.yml" }), /workflow path/],
    ["another repository", run({ repository: { full_name: "someone/kin" } }), /belongs to someone\/kin/],
    ["a fork", run({ head_repository: { full_name: "someone/kin" } }), /built someone\/kin/],
    ["another commit", run({ head_sha: OTHER }), /head_sha/],
    ["a failed run", run({ conclusion: "failure" }), /completed\/failure/],
    ["a running run", run({ status: "in_progress", conclusion: null }), /in_progress/],
    ["a push run", run({ event: "push", head_branch: "main" }), /neither merge_group nor workflow_dispatch/],
    ["a merge group for another base", run({ head_branch: `gh-readonly-queue/release/pr-1-${SHA}` }), /not a merge queue ref for main/],
    ["a branch posing as a merge group", run({ head_branch: "main" }), /not a merge queue ref for main/],
    ["a dispatch on another branch", dispatched({ head_branch: "main" }), /dispatched on main/],
    ["a dispatch by a person", dispatched({ actor: { login: "troyjr4103", type: "User" } }), /dispatched by troyjr4103/],
    ["a bot login on a user", dispatched({ actor: { login: "github-actions[bot]", type: "User" } }), /release automation/],
    ["another bot", dispatched({ actor: { login: "dependabot[bot]", type: "Bot" } }), /dispatched by dependabot/],
  ]) {
    const reasons = judgeRun(candidate, context);
    assert.ok(reasons.some((reason) => needle.test(reason)), `${label}: ${reasons.join("; ")}`);
  }
});

test("a run must still hold exactly one unexpired artifact per archive and the image", () => {
  const good = judgeArtifacts(artifactList());
  assert.deepEqual(good.reasons, []);
  assert.deepEqual(Object.keys(good.artifacts), RC_ARTIFACTS);
  assert.equal(good.artifacts["kin-linux-x86_64"], 900);
  assert.equal(good.artifacts[RC_IMAGE.artifact], 905);
  for (const [over, needle] of [
    [{ "kin-macos-x86_64": { expired: true } }, /kin-macos-x86_64 artifact has expired/],
    [{ "kin-windows-x86_64": { name: "something-else" } }, /no kin-windows-x86_64 artifact/],
    [{ [RC_IMAGE.artifact]: { expired: true } }, /kin-daemon-image artifact has expired/],
    [{ "kin-linux-aarch64": { id: null } }, /carries no id/],
  ]) {
    const { reasons } = judgeArtifacts(artifactList(over));
    assert.ok(reasons.some((reason) => needle.test(reason)), reasons.join("; "));
  }
  const twice = artifactList();
  twice.push({ ...twice[0], id: 1000 });
  assert.match(judgeArtifacts(twice).reasons.join(";"), /2 artifacts named kin-linux-x86_64/);
});

test("exactly one run is selected, a missing run refuses and two runs are ambiguous", async () => {
  const lists = new Map([[RUN_ID, artifactList()]]);
  const read = async (id) => lists.get(id) ?? artifactList();

  const chosen = await selectRun(
    [run({ id: 1, conclusion: "failure" }), run(), dispatched({ id: 3, actor: { login: "troyjr4103", type: "User" } })],
    read,
    context,
  );
  assert.equal(chosen.run.id, RUN_ID);

  await assert.rejects(selectRun([], read, context), /no RC Build run can supply.*no rc-build\.yml run exists/s);
  await assert.rejects(
    selectRun([run({ conclusion: "cancelled" })], read, context),
    /no RC Build run can supply[\s\S]*run 4242 \(merge_group\): it is completed\/cancelled/,
  );
  // The merge queue is the producer and a dispatch the fallback, so a
  // merge-group build wins over a dispatch for the same sha; two runs from the
  // same producer are ambiguous.
  assert.equal((await selectRun([dispatched({ id: 7 }), run()], read, context)).run.id, RUN_ID);
  assert.equal((await selectRun([dispatched({ id: 7 }), run({ conclusion: "failure" })], read, context)).run.id, 7);
  await assert.rejects(selectRun([run(), run({ id: 8 })], read, context), /2 RC Build runs from the same producer.*ambiguous/s);
  await assert.rejects(selectRun([dispatched(), dispatched({ id: 8 })], read, context), /2 RC Build runs from the same producer.*ambiguous/s);

  // An earlier success whose bytes expired is not a second candidate, and its
  // reason is still reported.
  lists.set(5, artifactList({ "kin-linux-x86_64": { expired: true } }));
  const afterExpiry = await selectRun([run({ id: 5 }), dispatched()], read, context);
  assert.equal(afterExpiry.run.id, RUN_ID);
  await assert.rejects(selectRun([run({ id: 5 })], read, context), /run 5 \(merge_group\): its kin-linux-x86_64 artifact has expired/);
});

test("a preflight record that names its RC Build run binds the release to exactly that run", async () => {
  const read = async () => artifactList();
  assert.equal(preflightRun(preflight()), null);
  assert.equal(preflightRun(preflight(undefined, { rc_build: { run_id: 7 } })), 7);
  assert.equal(preflightRun(preflight(undefined, { rc_build: { run_id: "7" } })), 7);
  assert.throws(() => preflightRun(preflight(undefined, { rc_build: { run_id: "latest" } })), /not a run id/);

  // A record that says which run it judged overrides the producer order.
  const both = [run(), dispatched({ id: 7 })];
  assert.equal((await selectRun(both, read, { ...context, namedRun: 7 })).run.id, 7);
  assert.equal((await selectRun(both, read, { ...context, namedRun: RUN_ID })).run.id, RUN_ID);
  await assert.rejects(
    selectRun([run(), dispatched({ id: 7, conclusion: "failure" })], read, { ...context, namedRun: 7 }),
    /judged RC Build run 7, which cannot supply[\s\S]*run 7 \(workflow_dispatch\): it is completed\/failure/,
  );
  await assert.rejects(selectRun([run()], read, { ...context, namedRun: 99 }), /judged RC Build run 99/);

  const { api } = fakeApi(both);
  const named = async () => JSON.stringify(preflight(undefined, { rc_build: { run_id: 7 } }));
  const located = await locate({ repository: REPO, sha: SHA, version: VERSION, api, readEvidence: named });
  assert.equal(located.run.id, 7);
});

test("a listing must be read whole", async () => {
  const pages = {
    1: { total_count: 101, items: Array.from({ length: 100 }, (_, i) => ({ id: i + 1 })) },
    2: { total_count: 101, items: [{ id: 101 }] },
  };
  const api = async (p) => {
    const page = Number(new URL(`https://x/${p}`).searchParams.get("page"));
    return { total_count: pages[page].total_count, workflow_runs: pages[page].items };
  };
  const all = await readListing(api, (page) => `runs?per_page=100&page=${page}`, "workflow_runs");
  assert.equal(all.length, 101);
  await assert.rejects(
    readListing(async () => ({ total_count: 3, workflow_runs: [{ id: 1 }] }), (page) => `runs?page=${page}`, "workflow_runs"),
    /returned 1 of 3 entries/,
  );
  await assert.rejects(
    readListing(async () => ({ message: "Not Found" }), (page) => `runs?page=${page}`, "workflow_runs"),
    /did not return a workflow_runs listing/,
  );
});

function fakeApi(runs) {
  const seen = [];
  const api = async (p) => {
    seen.push(p);
    if (p.startsWith(`repos/${REPO}/actions/workflows/rc-build.yml/runs?head_sha=${SHA}`)) {
      return { total_count: runs.length, workflow_runs: runs };
    }
    const artifacts = p.match(/^repos\/[^/]+\/[^/]+\/actions\/runs\/(\d+)\/artifacts/);
    if (artifacts) {
      const list = artifactList();
      return { total_count: list.length, artifacts: list };
    }
    throw new Error(`unexpected read ${p}`);
  };
  return { api, seen };
}

const readEvidence = async (sha, name) => {
  assert.equal(sha, SHA);
  assert.equal(name, "preflight.json");
  return JSON.stringify(preflight());
};

test("locate reads the published preflight and the run listing for the tag's sha", async () => {
  const { api, seen } = fakeApi([run(), run({ id: 11, conclusion: "failure" })]);
  const located = await locate({ repository: REPO, sha: SHA, version: VERSION, api, readEvidence });
  assert.equal(located.run.id, RUN_ID);
  assert.equal(located.preflight["kin-linux-x86_64"], DIGEST["kin-linux-x86_64.tar.gz"]);
  assert.equal(seen.filter((p) => p.includes("/artifacts")).length, 1, "a refused run's artifacts are never read");

  const absent = new Error("evidence/x/preflight.json does not exist");
  await assert.rejects(
    locate({ repository: REPO, sha: SHA, version: VERSION, api, readEvidence: async () => { throw absent; } }),
    /does not exist/,
  );
  await assert.rejects(locate({ repository: REPO, sha: "HEAD", version: VERSION, api, readEvidence }), /not a 40-character/);
});

test("select writes the run, its producer, the artifact ids and the preflight digests", async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-rc-select-"));
  const out = path.join(dir, "out");
  const summary = path.join(dir, "summary");
  const log = console.log;
  console.log = () => {};
  try {
    await main(
      ["select", "--repo", REPO, "--sha", SHA, "--version", VERSION, "--github-output", out, "--summary", summary],
      {},
      { api: fakeApi([run()]).api, readEvidence },
    );
  } finally {
    console.log = log;
  }
  const outputs = Object.fromEntries(
    fs.readFileSync(out, "utf8").trim().split("\n").map((line) => [line.slice(0, line.indexOf("=")), line.slice(line.indexOf("=") + 1)]),
  );
  assert.equal(outputs.run_id, String(RUN_ID));
  assert.equal(outputs.event, "merge_group");
  assert.equal(outputs.head_branch, QUEUE_REF);
  assert.equal(JSON.parse(outputs.artifact_ids)[RC_IMAGE.artifact], 905);
  assert.equal(JSON.parse(outputs.preflight)["kin-macos-aarch64"], DIGEST["kin-macos-aarch64.tar.gz"]);
  assert.match(fs.readFileSync(summary, "utf8"), /produced by \| `merge_group` on `gh-readonly-queue\/main\/pr-1851-/);
});

// An OCI image layout like the one `docker buildx build --output type=oci`
// writes, archived with the system tar.
function ociArchive(dir, { corrupt = false, twoManifests = false } = {}) {
  const layout = fs.mkdtempSync(path.join(dir, "layout-"));
  const blobs = path.join(layout, "blobs", "sha256");
  fs.mkdirSync(blobs, { recursive: true });
  const put = (bytes) => {
    const digest = sha256(bytes);
    fs.writeFileSync(path.join(blobs, digest), bytes);
    return { digest: `sha256:${digest}`, size: bytes.length };
  };
  const layer = put(Buffer.from("layer bytes"));
  const config = put(Buffer.from(JSON.stringify({ architecture: "amd64", os: "linux" })));
  const manifest = put(
    Buffer.from(
      JSON.stringify({
        schemaVersion: 2,
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        config: { mediaType: "application/vnd.oci.image.config.v1+json", ...config },
        layers: [{ mediaType: "application/vnd.oci.image.layer.v1.tar+gzip", ...layer }],
      }),
    ),
  );
  const entry = { mediaType: "application/vnd.oci.image.manifest.v1+json", ...manifest };
  fs.writeFileSync(path.join(layout, "oci-layout"), JSON.stringify({ imageLayoutVersion: "1.0.0" }));
  fs.writeFileSync(
    path.join(layout, "index.json"),
    JSON.stringify({ schemaVersion: 2, manifests: twoManifests ? [entry, entry] : [entry] }),
  );
  if (corrupt) fs.writeFileSync(path.join(blobs, layer.digest.slice(7)), "tampered");
  const archive = path.join(dir, `oci-${path.basename(layout)}.tar`);
  execFileSync("tar", ["-cf", archive, "-C", layout, "oci-layout", "index.json", "blobs"]);
  return { archive, manifestDigest: manifest.digest, configDigest: config.digest };
}

function writeImageArtifact(root, oci, over = {}) {
  fs.mkdirSync(root);
  const bytes = fs.readFileSync(oci.archive);
  fs.writeFileSync(path.join(root, RC_IMAGE.archive), bytes);
  fs.writeFileSync(path.join(root, `${RC_IMAGE.archive}.sha256`), `${sha256(bytes)}  ${RC_IMAGE.archive}\n`);
  fs.writeFileSync(
    path.join(root, RC_IMAGE.record),
    JSON.stringify({
      schema_version: 1,
      commit: SHA,
      platform: "linux/amd64",
      archive: { name: RC_IMAGE.archive, sha256: sha256(bytes) },
      manifest_digest: oci.manifestDigest,
      config_digest: oci.configDigest,
      workflow: { repository: REPO, run_id: String(RUN_ID), run_attempt: "1" },
      ...over,
    }),
  );
}

// The downloaded artifact directories, laid out the way a multi-artifact
// download writes them: one directory per artifact name.
function downloads({ mutate = () => {}, oci: ociOptions = {}, record = {} } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-rc-downloads-"));
  for (const entry of RC_ARCHIVES) {
    const root = path.join(dir, entry.artifact);
    fs.mkdirSync(root);
    for (const name of entry.archives) {
      fs.writeFileSync(path.join(root, name), BYTES[name]);
      fs.writeFileSync(path.join(root, `${name}.sha256`), `${DIGEST[name]}  ${name}\n`);
    }
    const [primary, ...additional] = entry.archives;
    const manifest = {
      schema_version: 2,
      release_candidate: { ref: SHA, version: VERSION },
      artifact: entry.artifact,
      target: entry.target,
      kin: { commit: SHA, cargo_lock_sha256: "c".repeat(64), embedded_dependency_provenance: "c".repeat(64) },
      archive: { name: primary, sha256: DIGEST[primary], size_bytes: BYTES[primary].length },
      ...(additional.length ? { additional_archives: additional.map((name) => ({ name, sha256: DIGEST[name], size_bytes: BYTES[name].length })) } : {}),
      archive_contents: [{ name: "kin" }, { name: "kin-daemon" }],
      workflow: { repository: REPO, run_id: String(RUN_ID), run_attempt: "1" },
    };
    fs.writeFileSync(path.join(root, `${entry.artifact}.provenance.json`), JSON.stringify(manifest));
  }
  writeImageArtifact(path.join(dir, RC_IMAGE.artifact), ociArchive(dir, ociOptions), record);
  mutate(dir);
  return dir;
}

const verifyContext = {
  repository: REPO,
  sha: SHA,
  version: VERSION,
  runId: String(RUN_ID),
  runAttempt: "1",
  preflight: Object.fromEntries(PREFLIGHT_REQUIRED.map((artifact) => [artifact, DIGEST[`${artifact}.tar.gz`]])),
};

const editManifest = (artifact, edit) => (dir) => {
  const file = path.join(dir, artifact, `${artifact}.provenance.json`);
  const manifest = JSON.parse(fs.readFileSync(file, "utf8"));
  edit(manifest);
  fs.writeFileSync(file, JSON.stringify(manifest));
};

test("the downloaded bytes are exactly the ones the preflight and RC Build's records name", () => {
  const dir = downloads();
  const digests = verifyDownloads(dir, verifyContext);
  for (const [name, digest] of Object.entries(DIGEST)) assert.equal(digests[name], digest);
  assert.match(digests.manifest_digest, /^sha256:[0-9a-f]{64}$/);
  // Each release.yml job verifies only the artifact it ships.
  assert.deepEqual(verifyArtifact(path.join(dir, "kin-windows-x86_64"), "kin-windows-x86_64", verifyContext), {
    "kin-windows-x86_64.zip": DIGEST["kin-windows-x86_64.zip"],
    "kin-windows-x86_64.tar.gz": DIGEST["kin-windows-x86_64.tar.gz"],
  });
  // A re-run leg can carry an earlier attempt than the run's latest.
  verifyDownloads(downloads(), { ...verifyContext, runAttempt: "3" });
  assert.throws(() => verifyArtifact(dir, "kin-freebsd-x86_64", verifyContext), /not an artifact RC Build produces/);
});

test("any downloaded archive byte, sidecar or manifest that disagrees refuses", () => {
  const cases = [
    ["a stray file in an artifact", (dir) => fs.writeFileSync(path.join(dir, "kin-linux-x86_64", "extra"), "x"), /holds .*extra/],
    ["a missing artifact", (dir) => fs.rmSync(path.join(dir, "kin-windows-x86_64"), { recursive: true }), /kin-windows-x86_64 artifact was not downloaded/],
    ["a CRLF sidecar", (dir) => fs.writeFileSync(path.join(dir, "kin-linux-aarch64", "kin-linux-aarch64.tar.gz.sha256"), `${DIGEST["kin-linux-aarch64.tar.gz"]}  kin-linux-aarch64.tar.gz\r\n`), /single LF-terminated line/],
    ["bytes that are not the sidecar's", (dir) => fs.writeFileSync(path.join(dir, "kin-macos-x86_64", "kin-macos-x86_64.tar.gz"), "other bytes"), /kin-macos-x86_64\.tar\.gz\.sha256 is not/],
    [
      "bytes the preflight did not judge",
      (dir) => {
        const name = "kin-macos-aarch64.tar.gz";
        const bytes = Buffer.from("a rebuild");
        fs.writeFileSync(path.join(dir, "kin-macos-aarch64", name), bytes);
        fs.writeFileSync(path.join(dir, "kin-macos-aarch64", `${name}.sha256`), `${sha256(bytes)}  ${name}\n`);
      },
      /preflight judged .*not the bytes the preflight judged/,
    ],
    ["a manifest from another commit", editManifest("kin-linux-x86_64", (m) => { m.release_candidate.ref = OTHER; }), /was built from b+/],
    ["a manifest for another version", editManifest("kin-linux-x86_64", (m) => { m.release_candidate.version = "0.8.2"; }), /names version 0\.8\.2/],
    ["a release manifest", editManifest("kin-linux-x86_64", (m) => { m.release_tag = "v0.8.4"; }), /names a release tag/],
    ["a manifest from another run", editManifest("kin-windows-x86_64", (m) => { m.workflow.run_id = "1"; }), /written by run 1, not 4242/],
    ["a manifest from a future attempt", editManifest("kin-windows-x86_64", (m) => { m.workflow.run_attempt = "2"; }), /not an attempt of run 4242/],
    ["a manifest naming other bytes", editManifest("kin-macos-x86_64", (m) => { m.archive.sha256 = "e".repeat(64); }), /not the downloaded bytes/],
    ["a Windows manifest without its tarball", editManifest("kin-windows-x86_64", (m) => { delete m.additional_archives; }), /additional archives kin-windows-x86_64\.tar\.gz/],
    ["a manifest for another target", editManifest("kin-linux-aarch64", (m) => { m.target = "x86_64-unknown-linux-musl"; }), /names target x86_64-unknown-linux-musl/],
    ["an unreadable manifest", (dir) => fs.writeFileSync(path.join(dir, "kin-linux-x86_64", "kin-linux-x86_64.provenance.json"), "{"), /not readable JSON/],
  ];
  for (const [label, mutate, needle] of cases) {
    assert.throws(() => verifyDownloads(downloads({ mutate }), verifyContext), needle, label);
  }
  assert.throws(
    () => judgeCandidateManifest({ schema_version: 1 }, RC_ARCHIVES[0], { ...verifyContext, digests: DIGEST }),
    /unsupported schema/,
  );
});

test("the image archive must be an intact OCI layout whose digests its record names", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-oci-"));
  const oci = ociArchive(dir);
  const unpacked = fs.mkdtempSync(path.join(dir, "unpacked-"));
  execFileSync("tar", ["-xf", oci.archive, "-C", unpacked]);
  assert.deepEqual(readOciLayout(unpacked), { manifestDigest: oci.manifestDigest, configDigest: oci.configDigest });

  for (const [label, options, needle] of [
    ["a tampered layer", { oci: { corrupt: true } }, /blob .* is not the bytes its name says/],
    ["an index naming two manifests", { oci: { twoManifests: true } }, /exactly one OCI image manifest/],
    ["a record from another commit", { record: { commit: OTHER } }, /was built from b+/],
    ["a record naming another manifest", { record: { manifest_digest: `sha256:${"0".repeat(64)}` } }, /names manifest sha256:0+, not the archive's/],
    ["a record naming another config", { record: { config_digest: `sha256:${"0".repeat(64)}` } }, /names config/],
    ["a record for another platform", { record: { platform: "linux/arm64" } }, /names platform linux\/arm64/],
    ["a record from another run", { record: { workflow: { repository: REPO, run_id: "1", run_attempt: "1" } } }, /written by run 1/],
  ]) {
    assert.throws(() => verifyDownloads(downloads(options), verifyContext), needle, label);
  }
});

test("verify writes the digests and the list a person checks a candidate with", async () => {
  const dir = downloads();
  const out = path.join(dir, "github-output");
  const list = path.join(dir, "archives.tsv");
  const log = console.log;
  console.log = () => {};
  try {
    await main([
      "verify", "--repo", REPO, "--sha", SHA, "--version", VERSION, "--dir", dir,
      "--run-id", String(RUN_ID), "--run-attempt", "1",
      "--preflight", JSON.stringify(verifyContext.preflight),
      "--github-output", out, "--list", list,
    ]);
  } finally {
    console.log = log;
  }
  const digests = JSON.parse(fs.readFileSync(out, "utf8").match(/^digests=(.*)$/m)[1]);
  assert.equal(digests["kin-linux-x86_64.tar.gz"], DIGEST["kin-linux-x86_64.tar.gz"]);
  const rows = fs.readFileSync(list, "utf8").trim().split("\n").map((row) => row.split("\t"));
  assert.equal(rows.length, 7);
  assert.deepEqual(rows.find((row) => row[1] === "kin-windows-x86_64.tar.gz"), [
    "kin-windows-x86_64", "kin-windows-x86_64.tar.gz", DIGEST["kin-windows-x86_64.tar.gz"],
  ]);
  await assert.rejects(main(["verify", "--repo", REPO, "--sha", SHA, "--version", VERSION]), /--dir is required/);
  await assert.rejects(main(["publish", "--repo", REPO, "--sha", SHA, "--version", VERSION]), /unknown command publish/);
});

function attestation(runUri, over = {}) {
  return {
    verificationResult: {
      signature: { certificate: { runInvocationURI: runUri } },
      statement: { predicate: { runDetails: { metadata: { invocationId: runUri } } } },
      ...over,
    },
  };
}

test("an attestation must verify against rc-build.yml at the commit and be signed by the selected run", () => {
  const selected = `https://github.com/${REPO}/actions/runs/${RUN_ID}/attempts/1`;
  const elsewhere = `https://github.com/${REPO}/actions/runs/77/attempts/1`;
  const calls = [];
  const exec = (results) => (file, args) => {
    calls.push([file, ...args]);
    if (results instanceof Error) throw results;
    return JSON.stringify(results);
  };
  const request = { file: "kin-linux-x86_64.tar.gz", repository: REPO, sha: SHA, sourceRef: `refs/heads/${QUEUE_REF}`, runId: String(RUN_ID) };

  assert.equal(verifyAttestation(request, { exec: exec([attestation(elsewhere), attestation(selected)]) }), 2);
  const args = calls[0].join(" ");
  for (const pinned of [
    "gh attestation verify kin-linux-x86_64.tar.gz",
    `--repo ${REPO}`,
    `--signer-workflow ${REPO}/.github/workflows/rc-build.yml`,
    `--signer-digest ${SHA}`,
    `--source-digest ${SHA}`,
    `--source-ref refs/heads/${QUEUE_REF}`,
    "--predicate-type https://slsa.dev/provenance/v1",
    "--deny-self-hosted-runners",
  ]) {
    assert.ok(args.includes(pinned), `gh is called with ${pinned}: ${args}`);
  }

  assert.throws(() => verifyAttestation(request, { exec: exec([attestation(elsewhere)]) }), /was signed by RC Build run 4242/);
  assert.throws(() => verifyAttestation(request, { exec: exec([]) }), /no verified attestation/);
  assert.throws(
    () => verifyAttestation(request, { exec: exec([attestation(selected, { statement: { predicate: { runDetails: { metadata: { invocationId: elsewhere } } } } })]) }),
    /disagrees with its own certificate/,
  );
  assert.throws(
    () => verifyAttestation(request, { exec: exec([attestation(`https://github.com/${REPO}/actions/runs/${RUN_ID}0/attempts/1`)]) }),
    /was signed by RC Build run 4242/,
  );
  const refused = Object.assign(new Error("exit 1"), { stderr: "no matching attestations found" });
  assert.throws(() => verifyAttestation(request, { exec: exec(refused) }), /no RC Build attestation verifies.*no matching attestations/);
});

// The lists this tool carries are copies of facts other files own. Read the
// owners, so a platform added to one of them cannot silently go unlocated.
test("the archive table matches rc-build.yml's build matrix and the cut judges every required archive", () => {
  const rcBuild = fs.readFileSync(path.join(ROOT, ".github/workflows/rc-build.yml"), "utf8");
  const rows = matrixRows(buildJobBody(rcBuild, "rc-build.yml"), "rc-build.yml");
  assert.deepEqual([...rows.keys()], RC_ARCHIVES.map((entry) => entry.artifact));
  for (const entry of RC_ARCHIVES) {
    assert.ok(rows.get(entry.artifact).includes(`target: ${entry.target}`), `${entry.artifact} target`);
  }

  const cut = fs.readFileSync(path.join(ROOT, ".github/workflows/release-cut.yml"), "utf8");
  const preflightJob = cut.slice(cut.indexOf("\n  preflight:"), cut.indexOf("\n  publish:"));
  const legs = [...preflightJob.matchAll(/^\s+artifact: (\S+)$/gm)].map((match) => match[1]);
  // The cut may judge more archives than the release requires, never fewer,
  // and only archives RC Build produces: a dropped leg would let the mint tag
  // a candidate whose release then refuses at the frozen tag.
  for (const artifact of PREFLIGHT_REQUIRED) {
    assert.ok(legs.includes(artifact), `release-cut.yml's preflight no longer judges ${artifact}`);
  }
  for (const artifact of legs) {
    assert.ok(RC_ARCHIVES.some((entry) => entry.artifact === artifact), `release-cut.yml judges ${artifact}, which RC Build does not produce`);
  }
});

// The script release.yml's publish job runs to bind the shipped archives to the
// RC Build, read out of the workflow text so this judges what actually runs.
function publishBindingScript() {
  const release = fs.readFileSync(path.join(ROOT, ".github/workflows/release.yml"), "utf8");
  const step = release.indexOf("      - name: Verify the shipped archives against the RC Build\n");
  assert.ok(step > 0, "release.yml no longer carries the publish job's RC Build binding step");
  const open = "          node <<'NODE'\n";
  const start = release.indexOf(open, step);
  const end = release.indexOf("\n          NODE\n", start);
  assert.ok(start > step && end > start, "the RC Build binding step no longer runs one node heredoc");
  const lines = release.slice(start + open.length, end).split("\n");
  for (const line of lines) {
    assert.ok(line === "" || line.startsWith("          "), `the binding script has a line outside its heredoc indent: ${line}`);
  }
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-publish-binding-script-"));
  const file = path.join(dir, "bind.cjs");
  fs.writeFileSync(file, `${lines.map((line) => line.slice(10)).join("\n")}\n`);
  return file;
}

// The archives and release manifests the publish job downloads, as the
// shipping jobs write them: each manifest's rc_build names the RC Build records
// the bytes came from, the primary archive first and Windows's tarball beside it.
function shippedArchives() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kin-shipped-"));
  for (const entry of RC_ARCHIVES) {
    const records = entry.archives.map((name) => ({ name, sha256: DIGEST[name] }));
    for (const name of entry.archives) fs.writeFileSync(path.join(dir, name), BYTES[name]);
    const [primary, ...additional] = records;
    const extra = additional.length ? { additional_archives: additional } : {};
    fs.writeFileSync(
      path.join(dir, `${entry.artifact}.provenance.json`),
      JSON.stringify({
        artifact: entry.artifact,
        archive: primary,
        ...extra,
        rc_build: { workflow: RC_BUILD_WORKFLOW, run_id: String(RUN_ID), archive: primary, ...extra },
      }),
    );
  }
  return dir;
}

function runPublishBinding(script, dir, preflightDigest) {
  try {
    return execFileSync(process.execPath, [script], {
      cwd: dir,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, RC_RUN_ID: String(RUN_ID), RC_PREFLIGHT: JSON.stringify(preflightDigest) },
    });
  } catch (error) {
    throw new Error(`the publish binding refused: ${error.stderr}`);
  }
}

// A preflight leg judges the tarball, which on Windows is the additional
// archive beside the primary zip. locate and publish must read the same record
// for it, or the first release with a Windows leg refuses in one of them.
test("locate and publish judge every preflight leg against the archive the preflight judged", () => {
  const script = publishBindingScript();
  const rc = downloads();
  const shipped = shippedArchives();
  const windows = RC_ARCHIVES.find((entry) => entry.artifact === "kin-windows-x86_64");
  assert.notEqual(preflightArchive(windows.artifact), windows.archives[0], "Windows's primary archive is the zip");
  assert.ok(windows.archives.includes(preflightArchive(windows.artifact)), "Windows ships the tarball a leg judges");

  // The digests a passing leg for each archive records, read back the way
  // locate's select reads a published record.
  const judgedBy = (entry) => {
    const legs = PREFLIGHT_REQUIRED.includes(entry.artifact) ? preflight().legs : [...preflight().legs, leg(entry.artifact)];
    return preflightDigests(preflight(legs), SHA);
  };
  for (const entry of RC_ARCHIVES) {
    const judged = judgedBy(entry);
    assert.equal(judged[entry.artifact], DIGEST[preflightArchive(entry.artifact)], entry.artifact);
    verifyArtifact(path.join(rc, entry.artifact), entry.artifact, { ...verifyContext, preflight: judged });
    assert.match(runPublishBinding(script, shipped, judged), new RegExp(`${entry.artifact}: `));
  }

  // A digest the leg did not record refuses in both. For Windows that is the
  // primary zip's, which the publish job once compared instead.
  for (const entry of RC_ARCHIVES) {
    const judged = judgedBy(entry);
    const other = entry.archives.find((name) => name !== preflightArchive(entry.artifact));
    const wrong = { ...judged, [entry.artifact]: other ? DIGEST[other] : "f".repeat(64) };
    const judgedName = preflightArchive(entry.artifact).replaceAll(".", "\\.");
    assert.throws(
      () => verifyArtifact(path.join(rc, entry.artifact), entry.artifact, { ...verifyContext, preflight: wrong }),
      new RegExp(`${judgedName} downloaded from run ${RUN_ID} is ${DIGEST[preflightArchive(entry.artifact)]}, but the preflight judged`),
      entry.artifact,
    );
    assert.throws(
      () => runPublishBinding(script, shipped, wrong),
      new RegExp(`${judgedName} came from ${DIGEST[preflightArchive(entry.artifact)]}, but the preflight judged`),
      entry.artifact,
    );
  }

  // The name is the preflight's own: release-cut.yml hands each leg the
  // tarball from the RC Build download, and the driver names the leg's target
  // after that tarball and accepts nothing else.
  const cut = fs.readFileSync(path.join(ROOT, ".github/workflows/release-cut.yml"), "utf8");
  const preflightJob = cut.slice(cut.indexOf("\n  preflight:"), cut.indexOf("\n  publish:"));
  assert.ok(preflightJob.includes(`archive="$RUNNER_TEMP/rc/$ARTIFACT/${preflightArchive("$ARTIFACT")}"`), "the cut hands the preflight the tarball");
  assert.ok(preflightJob.includes('--archive "$KIN_RC_ARCHIVE"'), "the cut hands the preflight the archive it downloaded");
  const driver = fs.readFileSync(path.join(ROOT, "scripts/release-proof/bin/kin-release-preflight"), "utf8");
  assert.ok(driver.includes('target="${name%.tar.gz}"'), "the preflight names a leg's target after its tarball");
  assert.ok(driver.includes("--archive must be a release-shaped kin-<os>-<arch>.tar.gz"), "the preflight judges only a tarball");
});
