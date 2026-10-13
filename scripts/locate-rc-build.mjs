#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Find the RC Build run whose bytes a tag ships, and prove the bytes downloaded
// from it are the ones RC Build built, attested and the preflight judged.
//
// release.yml used to compile all five archives and the daemon image again from
// the tag. Those were a second build of the same commit that no preflight had
// judged: the preflight legs in release-cut.yml ran on the rc-build.yml
// archives, while the Linux and Windows archives a user installed came from
// the rebuild. release.yml now ships the RC Build bytes themselves, so the
// archive a preflight leg judged is the archive that is published, the image
// RC Build smoke-tested is the image that is pushed, and the only bytes
// release.yml changes are the macOS signatures.
//
// That is only safe if release.yml is exact about which run it takes bytes
// from, so this tool refuses every reading it cannot make exactly.
//
//   select              reads evidence/<sha>/preflight.json off the
//                       release-evidence branch and judges it with the mint's
//                       own judgePreflight, then reads every rc-build.yml run
//                       for the tag's peeled sha. A run qualifies only when its
//                       workflow path is .github/workflows/rc-build.yml on this
//                       repository (never a fork), its head_sha is the tag's,
//                       it concluded success, it still holds every artifact
//                       unexpired, and it came from one of the two producers:
//                       GitHub's merge queue building the exact commit that
//                       lands on main, or Release Cut's dispatch on
//                       release/v<version>-candidate. The merge-group run wins
//                       over a fallback dispatch, the run the preflight record
//                       names wins when it names one, and otherwise exactly one
//                       run must qualify; none is a missing run and two from
//                       the same producer are ambiguous.
//                       API reads only, so it is not a serial download in front
//                       of the jobs that use the bytes.
//   verify-artifact     judges one downloaded artifact: its exact file set, LF
//                       sidecars naming each file's sha256, the preflight's
//                       sha256 for every archive a preflight leg judged, and
//                       the record RC Build wrote about the same bytes. Each
//                       release.yml job runs it on the artifact it ships.
//   verify-attestation  runs `gh attestation verify` on one file, pinned to
//                       rc-build.yml at the tag's commit on the run's ref, and
//                       requires an attestation signed by the selected run.
//   verify              verify-artifact over every artifact of a full download,
//                       for checking a candidate by hand before it is tagged.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  appendFileSync,
  existsSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

import { PREFLIGHT_RECORD, fetchEvidence, judgePreflight } from "./check-release-proof-artifacts.mjs";

export const RC_BUILD_WORKFLOW = ".github/workflows/rc-build.yml";
// Every archive rc-build.yml's build matrix produces, in the matrix's order.
// The first archive of an entry is its primary one. scripts/check-rc-build-drift.mjs
// holds this list equal to the build matrix and to the archives release.yml
// publishes, so a platform cannot be built without being shipped or shipped
// without being located here.
export const RC_ARCHIVES = Object.freeze([
  { artifact: "kin-linux-x86_64", target: "x86_64-unknown-linux-musl", archives: ["kin-linux-x86_64.tar.gz"] },
  { artifact: "kin-linux-aarch64", target: "aarch64-unknown-linux-musl", archives: ["kin-linux-aarch64.tar.gz"] },
  { artifact: "kin-macos-x86_64", target: "x86_64-apple-darwin", archives: ["kin-macos-x86_64.tar.gz"] },
  { artifact: "kin-macos-aarch64", target: "aarch64-apple-darwin", archives: ["kin-macos-aarch64.tar.gz"] },
  {
    artifact: "kin-windows-x86_64",
    target: "x86_64-pc-windows-msvc",
    archives: ["kin-windows-x86_64.zip", "kin-windows-x86_64.tar.gz"],
  },
]);
// The daemon image RC Build's image job built and smoke-tested, as an OCI
// image layout archive, and the record naming its manifest and config digests.
export const RC_IMAGE = Object.freeze({
  artifact: "kin-daemon-image",
  archive: "kin-daemon-image.oci.tar",
  record: "kin-daemon-image.json",
  platform: "linux/amd64",
});
export const RC_ARTIFACTS = Object.freeze([...RC_ARCHIVES.map((entry) => entry.artifact), RC_IMAGE.artifact]);
// The archives every preflight record must have judged: the three
// release-cut.yml's preflight matrix runs, one per runner that executes them
// natively. The test requires that matrix to judge at least these. A record
// judging more of them binds more of them, and every leg it carries is
// enforced. The other two archives and the image are bound to the same run by
// the run's identity, the records it wrote and its attestation, and RC Build
// installed every archive and smoke-tested the image in that run.
export const PREFLIGHT_REQUIRED = Object.freeze(["kin-linux-aarch64", "kin-linux-x86_64", "kin-macos-aarch64"]);
// The archive a preflight leg judges for an artifact, and so the one whose
// sha256 its record names. kin-release-preflight accepts only a
// kin-<os>-<arch>.tar.gz and names the leg's target after that file, and
// release-cut.yml's preflight job hands it <artifact>.tar.gz from the RC Build
// download. That tarball is the primary archive on every platform but Windows,
// whose primary archive is the zip and whose tarball is the additional one. So
// a preflight digest is compared with the record this names, never with
// whichever archive is primary. release.yml's publish job reads the same name
// out of each manifest's rc_build records.
export const preflightArchive = (artifact) => `${artifact}.tar.gz`;
// Who may have dispatched a candidate-branch run. Release Cut dispatches
// rc-build.yml with the release App's token when the App holds the Actions
// permission and with the workflow token otherwise, which GitHub records as
// these two bots. A bot login ends in [bot] and cannot be registered by a
// person. `actor` is who started the run; a later re-run of a failed leg by a
// person changes only `triggering_actor`, re-executes the same workflow at the
// same commit, and is reported rather than refused.
export const RELEASE_AUTOMATION_ACTORS = Object.freeze(["github-actions[bot]", "kin-release-bot[bot]"]);
// The ref GitHub's merge queue builds a group for main on. Only the queue
// creates these refs and only the queue emits merge_group events, so the event
// and the ref together are the producer's identity. Its actor is whoever
// enqueued the pull request, which the main ruleset restricts to writers.
export const MERGE_QUEUE_BRANCH = /^gh-readonly-queue\/main\/pr-[1-9][0-9]*-[0-9a-f]{40}$/;

const COMMIT_SHA = /^[0-9a-f]{40}$/;
const FILE_SHA = /^[0-9a-f]{64}$/;
const OCI_DIGEST = /^sha256:[0-9a-f]{64}$/;
const OCI_MANIFEST = "application/vnd.oci.image.manifest.v1+json";
const PAGE_SIZE = 100;

export const candidateBranch = (version) => `release/v${version}-candidate`;

const sha256File = (file) => createHash("sha256").update(readFileSync(file)).digest("hex");
const run = (file, args) =>
  execFileSync(file, args, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], maxBuffer: 64 * 1024 * 1024 });

// ---------------------------------------------------------------------------
// The preflight record.
// ---------------------------------------------------------------------------

// The archive sha256 each preflight leg judged, by artifact. judgePreflight is
// the mint's own reading, so a record release.yml accepts is one the mint
// accepted; what this adds is the mapping from a leg to the RC Build artifact
// it judged, and the refusal of a leg this release could not ship.
export function preflightDigests(record, sha) {
  judgePreflight(record, sha);
  const known = new Set(RC_ARCHIVES.map((entry) => entry.artifact));
  const digests = {};
  for (const leg of record.legs) {
    const target = leg.target;
    if (!known.has(target)) {
      throw new Error(
        `preflight leg "${leg.name ?? target}" judged ${target ?? "<no target>"}, which RC Build does not produce`,
      );
    }
    if (Object.prototype.hasOwnProperty.call(digests, target)) {
      throw new Error(`two preflight legs judged ${target}; which bytes the record names is ambiguous`);
    }
    const archive = leg.result?.archive?.sha256;
    if (!FILE_SHA.test(archive ?? "")) {
      throw new Error(`preflight leg for ${target} records no archive sha256`);
    }
    digests[target] = archive;
  }
  const missing = PREFLIGHT_REQUIRED.filter((artifact) => !(artifact in digests));
  if (missing.length > 0) {
    throw new Error(`the preflight record for ${sha} judged no ${missing.join(", ")} archive`);
  }
  return digests;
}

// ---------------------------------------------------------------------------
// The run.
// ---------------------------------------------------------------------------

// Every reason a run is not the one the release may take bytes from. An empty
// list admits it. Reported whole rather than at the first failure, so a refusal
// says everything that disqualified each run it saw.
export function judgeRun(run, { repository, sha, version }) {
  const reasons = [];
  if (run?.path !== RC_BUILD_WORKFLOW) {
    reasons.push(`workflow path ${run?.path ?? "<none>"} is not ${RC_BUILD_WORKFLOW}`);
  }
  if (run?.repository?.full_name !== repository) {
    reasons.push(`it belongs to ${run?.repository?.full_name ?? "<no repository>"}, not ${repository}`);
  }
  if (run?.head_repository?.full_name !== repository) {
    reasons.push(`it built ${run?.head_repository?.full_name ?? "<no head repository>"}, not ${repository}`);
  }
  if (run?.head_sha !== sha) {
    reasons.push(`head_sha ${run?.head_sha ?? "<none>"} is not the tag's ${sha}`);
  }
  if (run?.status !== "completed" || run?.conclusion !== "success") {
    reasons.push(`it is ${run?.status ?? "<no status>"}/${run?.conclusion ?? "<no conclusion>"}, not completed/success`);
  }
  const actor = run?.actor ?? {};
  if (run?.event === "merge_group") {
    if (!MERGE_QUEUE_BRANCH.test(run?.head_branch ?? "")) {
      reasons.push(`its merge group ran on ${run?.head_branch ?? "<no branch>"}, not a merge queue ref for main`);
    }
  } else if (run?.event === "workflow_dispatch") {
    const branch = candidateBranch(version);
    if (run?.head_branch !== branch) {
      reasons.push(`it was dispatched on ${run?.head_branch ?? "<no branch>"}, not ${branch}`);
    }
    if (!RELEASE_AUTOMATION_ACTORS.includes(actor.login) || actor.type !== "Bot") {
      reasons.push(
        `it was dispatched by ${actor.login ?? "<nobody>"} (${actor.type ?? "no type"}), not the release automation ` +
          `(${RELEASE_AUTOMATION_ACTORS.join(" or ")})`,
      );
    }
  } else {
    reasons.push(`event ${run?.event ?? "<none>"} is neither merge_group nor workflow_dispatch`);
  }
  return reasons;
}

// The run's artifacts by name, or the reasons it cannot supply them.
export function judgeArtifacts(artifacts) {
  const reasons = [];
  const chosen = {};
  for (const artifact of RC_ARTIFACTS) {
    const named = artifacts.filter((entry) => entry?.name === artifact);
    if (named.length === 0) {
      reasons.push(`it holds no ${artifact} artifact`);
      continue;
    }
    if (named.length > 1) {
      reasons.push(`it holds ${named.length} artifacts named ${artifact}`);
      continue;
    }
    const [entry] = named;
    if (entry.expired !== false) {
      reasons.push(`its ${artifact} artifact has expired`);
      continue;
    }
    if (!Number.isInteger(entry.id) || entry.id <= 0) {
      reasons.push(`its ${artifact} artifact carries no id`);
      continue;
    }
    chosen[artifact] = entry.id;
  }
  return { reasons, artifacts: chosen };
}

// The RC Build run a preflight record says it judged, when it says so.
// release-cut.yml can record it as `rc_build.run_id`; a record that names one
// binds the release to exactly that run, so a second admissible run for the
// same commit (a merge-group build and a fallback dispatch, say) is no longer
// ambiguous. A record that names none is held to "exactly one run qualifies".
export function preflightRun(record) {
  const named = record?.rc_build?.run_id;
  if (named === undefined) return null;
  const id = typeof named === "string" && /^[1-9][0-9]*$/.test(named) ? Number(named) : named;
  if (!Number.isSafeInteger(id) || id <= 0) {
    throw new Error(`the preflight record names RC Build run ${JSON.stringify(named)}, which is not a run id`);
  }
  return id;
}

// Exactly one run, or a refusal that names every run it saw and why.
export async function selectRun(runs, readArtifacts, context) {
  const report = [];
  const admitted = [];
  for (const candidate of runs) {
    const reasons = judgeRun(candidate, context);
    if (reasons.length === 0) {
      const { reasons: artifactReasons, artifacts } = judgeArtifacts(await readArtifacts(candidate.id));
      if (artifactReasons.length === 0) {
        admitted.push({ run: candidate, artifacts });
        report.push(`run ${candidate.id} (${candidate.event}): admitted`);
        continue;
      }
      reasons.push(...artifactReasons);
    }
    report.push(`run ${candidate.id} (${candidate.event ?? "no event"}): ${reasons.join("; ")}`);
  }
  const seen = report.length > 0 ? `\n  ${report.join("\n  ")}` : " (no rc-build.yml run exists for it)";
  if (context.namedRun !== undefined && context.namedRun !== null) {
    const named = admitted.find((entry) => entry.run.id === context.namedRun);
    if (!named) {
      throw new Error(
        `the preflight record for ${context.sha} judged RC Build run ${context.namedRun}, which cannot supply ` +
          `the release bytes:${seen}`,
      );
    }
    return named;
  }
  if (admitted.length === 0) {
    throw new Error(`no RC Build run can supply the release bytes for ${context.sha}:${seen}`);
  }
  // The merge queue is the producer; a candidate dispatch is the fallback for
  // a commit the queue did not build. So a merge-group run wins when there is
  // one, and two runs from the same producer are ambiguous. Whichever is
  // chosen, every archive a preflight leg judged must still match its sha256,
  // so choosing the wrong run refuses rather than ships.
  const queued = admitted.filter((entry) => entry.run.event === "merge_group");
  const candidates = queued.length > 0 ? queued : admitted;
  if (candidates.length > 1) {
    throw new Error(
      `${candidates.length} RC Build runs from the same producer could supply the release bytes for ` +
        `${context.sha}, so which bytes the preflight judged is ambiguous:${seen}`,
    );
  }
  return candidates[0];
}

// ---------------------------------------------------------------------------
// The downloaded bytes.
// ---------------------------------------------------------------------------

function requireExactFiles(dir, expected, label) {
  if (!existsSync(dir) || !statSync(dir).isDirectory()) {
    throw new Error(`the ${label} artifact was not downloaded to ${dir}`);
  }
  const actual = readdirSync(dir).sort();
  const want = [...expected].sort();
  if (JSON.stringify(actual) !== JSON.stringify(want)) {
    throw new Error(`the ${label} artifact holds ${actual.join(", ") || "nothing"}, expected exactly ${want.join(", ")}`);
  }
}

// A file and its sidecar, which must be the one LF-terminated line sha256sum
// writes. Returns the file's sha256.
function judgeSidecar(dir, name) {
  const file = join(dir, name);
  if (!statSync(file).isFile() || statSync(file).size === 0) {
    throw new Error(`${name} is empty or not a regular file`);
  }
  const digest = sha256File(file);
  if (readFileSync(`${file}.sha256`, "utf8") !== `${digest}  ${name}\n`) {
    throw new Error(`${name}.sha256 is not the single LF-terminated line naming ${digest}`);
  }
  return digest;
}

function judgeWorkflowRecord(workflow, { repository, runId, runAttempt }, where) {
  if (workflow?.repository !== repository) throw new Error(`${where} was written by another repository`);
  if (String(workflow?.run_id) !== String(runId)) {
    throw new Error(`${where} was written by run ${workflow?.run_id ?? "<none>"}, not ${runId}`);
  }
  const attempt = Number(workflow?.run_attempt);
  if (!Number.isInteger(attempt) || attempt < 1 || attempt > Number(runAttempt)) {
    throw new Error(`${where} names attempt ${workflow?.run_attempt ?? "<none>"}, which is not an attempt of run ${runId}`);
  }
}

// The candidate manifest RC Build wrote beside each archive, which must be
// about exactly these bytes, this commit, this version and this run.
export function judgeCandidateManifest(manifest, entry, { repository, sha, version, runId, runAttempt, digests }) {
  const where = `${entry.artifact}.provenance.json`;
  const fail = (condition, message) => {
    if (!condition) throw new Error(`${where} ${message}`);
  };
  fail(manifest?.schema_version === 2, "has an unsupported schema");
  fail(manifest.release_tag === undefined, "names a release tag, so it is not a candidate manifest");
  fail(manifest.release_candidate?.ref === sha, `was built from ${manifest.release_candidate?.ref ?? "<none>"}, not ${sha}`);
  fail(manifest.release_candidate?.version === version, `names version ${manifest.release_candidate?.version ?? "<none>"}, not ${version}`);
  fail(manifest.artifact === entry.artifact, "names a different artifact");
  fail(manifest.target === entry.target, `names target ${manifest.target ?? "<none>"}, not ${entry.target}`);
  fail(manifest.kin?.commit === sha, "records a different Kin commit");
  const [primary, ...additional] = entry.archives;
  fail(manifest.archive?.name === primary, `names primary archive ${manifest.archive?.name ?? "<none>"}, not ${primary}`);
  fail(manifest.archive?.sha256 === digests[primary], `records ${primary} as ${manifest.archive?.sha256 ?? "<none>"}, not the downloaded bytes`);
  const recordedAdditional = (manifest.additional_archives ?? []).map((record) => record?.name);
  fail(JSON.stringify(recordedAdditional) === JSON.stringify(additional), `does not declare exactly the additional archives ${additional.join(", ") || "(none)"}`);
  for (const record of manifest.additional_archives ?? []) {
    fail(record.sha256 === digests[record.name], `records ${record.name} as ${record.sha256 ?? "<none>"}, not the downloaded bytes`);
  }
  fail(Array.isArray(manifest.archive_contents) && manifest.archive_contents.length >= 2, "has no archive content inventory");
  judgeWorkflowRecord(manifest.workflow, { repository, runId, runAttempt }, where);
}

// One release archive artifact, downloaded into `dir` on its own.
export function verifyArchiveArtifact(dir, entry, context) {
  requireExactFiles(
    dir,
    [...entry.archives.flatMap((name) => [name, `${name}.sha256`]), `${entry.artifact}.provenance.json`],
    entry.artifact,
  );
  const digests = Object.fromEntries(entry.archives.map((name) => [name, judgeSidecar(dir, name)]));
  const judged = context.preflight[entry.artifact];
  const judgedArchive = preflightArchive(entry.artifact);
  if (judged !== undefined && digests[judgedArchive] !== judged) {
    throw new Error(
      `${judgedArchive} downloaded from run ${context.runId} is ${digests[judgedArchive] ?? "<absent>"}, but the ` +
        `preflight judged ${judged}; these are not the bytes the preflight judged`,
    );
  }
  let manifest;
  try {
    manifest = JSON.parse(readFileSync(join(dir, `${entry.artifact}.provenance.json`), "utf8"));
  } catch (cause) {
    throw new Error(`${entry.artifact}.provenance.json is not readable JSON: ${cause.message}`, { cause });
  }
  judgeCandidateManifest(manifest, entry, { ...context, digests });
  return digests;
}

// The OCI image layout inside the image archive: every blob is the bytes its
// name says, the index names exactly one image manifest, and that manifest's
// config is present. Returns the manifest and config digests, which are what a
// registry push with preserved digests serves and what `docker load` names the
// image.
export function readOciLayout(root) {
  const blobs = join(root, "blobs", "sha256");
  if (!existsSync(join(root, "oci-layout")) || !existsSync(join(root, "index.json")) || !existsSync(blobs)) {
    throw new Error("the image archive is not an OCI image layout");
  }
  for (const name of readdirSync(blobs)) {
    if (!FILE_SHA.test(name) || sha256File(join(blobs, name)) !== name) {
      throw new Error(`the image archive's blob ${name} is not the bytes its name says`);
    }
  }
  const index = JSON.parse(readFileSync(join(root, "index.json"), "utf8"));
  const manifests = Array.isArray(index?.manifests) ? index.manifests : [];
  if (index?.schemaVersion !== 2 || manifests.length !== 1 || manifests[0].mediaType !== OCI_MANIFEST) {
    throw new Error("the image archive's index must name exactly one OCI image manifest");
  }
  const manifestDigest = manifests[0].digest;
  const blob = (digest) => {
    if (!OCI_DIGEST.test(digest ?? "")) throw new Error(`the image archive names an invalid digest ${digest}`);
    const file = join(blobs, digest.slice("sha256:".length));
    if (!existsSync(file)) throw new Error(`the image archive is missing blob ${digest}`);
    return file;
  };
  const manifest = JSON.parse(readFileSync(blob(manifestDigest), "utf8"));
  const configDigest = manifest?.config?.digest;
  blob(configDigest);
  for (const layer of manifest?.layers ?? []) blob(layer?.digest);
  if (!Array.isArray(manifest?.layers) || manifest.layers.length === 0) {
    throw new Error("the image archive's manifest names no layers");
  }
  return { manifestDigest, configDigest };
}

// The daemon image artifact, downloaded into `dir` on its own.
export function verifyImageArtifact(dir, context, { exec = run } = {}) {
  requireExactFiles(dir, [RC_IMAGE.archive, `${RC_IMAGE.archive}.sha256`, RC_IMAGE.record], RC_IMAGE.artifact);
  const archiveSha = judgeSidecar(dir, RC_IMAGE.archive);
  const work = mkdtempSync(join(tmpdir(), "kin-daemon-image-"));
  let layout;
  try {
    exec("tar", ["-xf", join(dir, RC_IMAGE.archive), "-C", work]);
    layout = readOciLayout(work);
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
  let record;
  try {
    record = JSON.parse(readFileSync(join(dir, RC_IMAGE.record), "utf8"));
  } catch (cause) {
    throw new Error(`${RC_IMAGE.record} is not readable JSON: ${cause.message}`, { cause });
  }
  const fail = (condition, message) => {
    if (!condition) throw new Error(`${RC_IMAGE.record} ${message}`);
  };
  fail(record?.schema_version === 1, "has an unsupported schema");
  fail(record.commit === context.sha, `was built from ${record.commit ?? "<none>"}, not ${context.sha}`);
  fail(record.platform === RC_IMAGE.platform, `names platform ${record.platform ?? "<none>"}, not ${RC_IMAGE.platform}`);
  fail(record.archive?.name === RC_IMAGE.archive && record.archive?.sha256 === archiveSha, "names other archive bytes");
  fail(record.manifest_digest === layout.manifestDigest, `names manifest ${record.manifest_digest ?? "<none>"}, not the archive's ${layout.manifestDigest}`);
  fail(record.config_digest === layout.configDigest, `names config ${record.config_digest ?? "<none>"}, not the archive's ${layout.configDigest}`);
  judgeWorkflowRecord(record.workflow, context, RC_IMAGE.record);
  return { [RC_IMAGE.archive]: archiveSha, manifest_digest: layout.manifestDigest, config_digest: layout.configDigest };
}

export function verifyArtifact(dir, artifact, context, options = {}) {
  if (artifact === RC_IMAGE.artifact) return verifyImageArtifact(dir, context, options);
  const entry = RC_ARCHIVES.find((candidate) => candidate.artifact === artifact);
  if (!entry) throw new Error(`${artifact} is not an artifact RC Build produces`);
  return verifyArchiveArtifact(dir, entry, context);
}

// Every artifact of a full download laid out one directory per artifact, which
// is what `gh run download` and a multi-artifact download-artifact write.
export function verifyDownloads(dir, context, options = {}) {
  const digests = {};
  for (const artifact of RC_ARTIFACTS) {
    Object.assign(digests, verifyArtifact(join(dir, artifact), artifact, context, options));
  }
  return digests;
}

// ---------------------------------------------------------------------------
// The attestation.
// ---------------------------------------------------------------------------

// `gh attestation verify` pinned to rc-build.yml at the tag's commit on the
// ref the run built, then bound to the run: at least one verified attestation
// must have been signed by the selected run, and every one must agree with its
// own certificate about which run signed it.
export function verifyAttestation(
  { file, repository, sha, sourceRef, runId, server = "https://github.com" },
  { exec = run } = {},
) {
  const args = [
    "attestation", "verify", file,
    "--repo", repository,
    "--signer-workflow", `${repository}/${RC_BUILD_WORKFLOW}`,
    "--signer-digest", sha,
    "--source-digest", sha,
    "--source-ref", sourceRef,
    "--predicate-type", "https://slsa.dev/provenance/v1",
    "--deny-self-hosted-runners",
    "--format", "json",
  ];
  let output;
  try {
    output = exec("gh", args);
  } catch (cause) {
    const detail = cause.stderr ? `: ${String(cause.stderr).trim()}` : "";
    throw new Error(`no RC Build attestation verifies for ${file}${detail}`, { cause });
  }
  const results = JSON.parse(output);
  if (!Array.isArray(results) || results.length === 0) {
    throw new Error(`gh attestation verify returned no verified attestation for ${file}`);
  }
  const runUri = `${server}/${repository}/actions/runs/${runId}/attempts/`;
  let signedBySelectedRun = false;
  for (const result of results) {
    const certificateRun = result?.verificationResult?.signature?.certificate?.runInvocationURI;
    const predicateRun = result?.verificationResult?.statement?.predicate?.runDetails?.metadata?.invocationId;
    if (typeof certificateRun !== "string" || certificateRun !== predicateRun) {
      throw new Error(`an attestation for ${file} disagrees with its own certificate about the run that signed it`);
    }
    if (certificateRun.startsWith(runUri) && /^[1-9][0-9]*$/.test(certificateRun.slice(runUri.length))) {
      signedBySelectedRun = true;
    }
  }
  if (!signedBySelectedRun) {
    throw new Error(`no verified attestation for ${file} was signed by RC Build run ${runId}`);
  }
  return results.length;
}

// ---------------------------------------------------------------------------
// GitHub reads.
// ---------------------------------------------------------------------------

export function githubApi({ token, fetchImpl = fetch, base = "https://api.github.com" } = {}) {
  return async (path) => {
    let response;
    try {
      response = await fetchImpl(`${base}/${path}`, {
        headers: {
          accept: "application/vnd.github+json",
          "user-agent": "kin-locate-rc-build",
          "x-github-api-version": "2022-11-28",
          ...(token ? { authorization: `Bearer ${token}` } : {}),
        },
      });
    } catch (cause) {
      throw new Error(`could not reach GitHub to read ${path}: ${cause.message}`, { cause });
    }
    if (!response.ok) {
      throw new Error(`could not read ${path}: HTTP ${response.status} ${response.statusText}`);
    }
    return response.json();
  };
}

// A whole listing or a refusal. A short read is indistinguishable from an
// absent run or artifact, so the item count must reach total_count.
export async function readListing(api, pathForPage, key) {
  const items = [];
  let total = null;
  for (let page = 1; page <= 50; page += 1) {
    const body = await api(pathForPage(page));
    const batch = body?.[key];
    if (!Array.isArray(batch) || !Number.isInteger(body?.total_count)) {
      throw new Error(`${pathForPage(page)} did not return a ${key} listing`);
    }
    total = body.total_count;
    items.push(...batch);
    if (batch.length < PAGE_SIZE || items.length >= total) break;
  }
  if (items.length !== total) {
    throw new Error(`the ${key} listing returned ${items.length} of ${total} entries; refusing a partial reading`);
  }
  return items;
}

export async function locate({ repository, sha, version, api, readEvidence }) {
  if (!COMMIT_SHA.test(sha ?? "")) throw new Error(`"${sha}" is not a 40-character commit sha`);
  if (!/^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/.test(version ?? "")) {
    throw new Error(`"${version}" is not a release version`);
  }
  const text = await readEvidence(sha, PREFLIGHT_RECORD);
  let record;
  try {
    record = JSON.parse(text);
  } catch (cause) {
    throw new Error(`evidence/${sha}/${PREFLIGHT_RECORD} is not JSON: ${cause.message}`, { cause });
  }
  const preflight = preflightDigests(record, sha);
  const namedRun = preflightRun(record);
  const runs = await readListing(
    api,
    (page) => `repos/${repository}/actions/workflows/rc-build.yml/runs?head_sha=${sha}&exclude_pull_requests=true&per_page=${PAGE_SIZE}&page=${page}`,
    "workflow_runs",
  );
  const readArtifacts = (runId) =>
    readListing(api, (page) => `repos/${repository}/actions/runs/${runId}/artifacts?per_page=${PAGE_SIZE}&page=${page}`, "artifacts");
  const { run: selected, artifacts } = await selectRun(runs, readArtifacts, { repository, sha, version, namedRun });
  return { run: selected, artifacts, preflight };
}

// ---------------------------------------------------------------------------
// Command line.
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const [command, ...rest] = argv;
  const args = { command };
  for (let i = 0; i < rest.length; i += 2) {
    const flag = rest[i];
    const value = rest[i + 1];
    if (!flag?.startsWith("--") || value === undefined) {
      throw new Error(`usage: locate-rc-build.mjs <command> --flag value ...; got ${flag ?? "nothing"}`);
    }
    args[flag.slice(2)] = value;
  }
  return args;
}

function requireArgs(args, names) {
  for (const name of names) {
    if (!args[name]) throw new Error(`--${name} is required`);
  }
}

function writeOutputs(file, outputs) {
  const lines = Object.entries(outputs).map(([key, value]) => `${key}=${value}`);
  if (file) appendFileSync(file, `${lines.join("\n")}\n`);
  for (const line of lines) console.log(line);
}

function context(args) {
  requireArgs(args, ["repo", "sha", "version", "run-id", "run-attempt", "preflight"]);
  return {
    repository: args.repo,
    sha: args.sha,
    version: args.version,
    runId: args["run-id"],
    runAttempt: args["run-attempt"],
    preflight: JSON.parse(args.preflight),
  };
}

export async function main(argv = process.argv.slice(2), env = process.env, options = {}) {
  const args = parseArgs(argv);
  switch (args.command) {
    case "select": {
      requireArgs(args, ["repo", "sha", "version"]);
      const token = env.GH_TOKEN;
      const { run: selected, artifacts, preflight } = await locate({
        repository: args.repo,
        sha: args.sha,
        version: args.version,
        api: options.api ?? githubApi({ token }),
        readEvidence: options.readEvidence ?? ((sha, name) => fetchEvidence(sha, name, { repository: args.repo, token })),
      });
      const startedBy = selected.actor?.login ?? "<unknown>";
      writeOutputs(args["github-output"], {
        run_id: selected.id,
        run_attempt: selected.run_attempt,
        event: selected.event,
        head_branch: selected.head_branch,
        actor: startedBy,
        artifact_ids: JSON.stringify(artifacts),
        preflight: JSON.stringify(preflight),
      });
      if (args.summary) {
        appendFileSync(
          args.summary,
          [
            "### RC Build located",
            "",
            "| field | value |",
            "| --- | --- |",
            `| run | ${selected.html_url ?? selected.id} (attempt ${selected.run_attempt}) |`,
            `| produced by | \`${selected.event}\` on \`${selected.head_branch}\` at \`${selected.head_sha}\` |`,
            `| started by | ${startedBy} |`,
            `| last triggered by | ${selected.triggering_actor?.login ?? startedBy} |`,
            ...Object.entries(preflight).map(([artifact, digest]) => `| preflight ${artifact} | \`${digest}\` |`),
            "",
          ].join("\n"),
        );
      }
      return;
    }
    case "verify-artifact": {
      requireArgs(args, ["artifact", "dir"]);
      const digests = verifyArtifact(args.dir, args.artifact, context(args), options);
      writeOutputs(args["github-output"], { digests: JSON.stringify(digests) });
      return;
    }
    case "verify": {
      requireArgs(args, ["dir"]);
      const digests = verifyDownloads(args.dir, context(args), options);
      if (args.list) {
        const rows = RC_ARCHIVES.flatMap((entry) => entry.archives.map((name) => `${entry.artifact}\t${name}\t${digests[name]}`));
        rows.push(`${RC_IMAGE.artifact}\t${RC_IMAGE.archive}\t${digests[RC_IMAGE.archive]}`);
        writeFileSync(args.list, `${rows.join("\n")}\n`);
      }
      writeOutputs(args["github-output"], { digests: JSON.stringify(digests) });
      return;
    }
    case "verify-attestation": {
      requireArgs(args, ["file", "repo", "sha", "source-ref", "run-id"]);
      const count = verifyAttestation(
        {
          file: args.file,
          repository: args.repo,
          sha: args.sha,
          sourceRef: args["source-ref"],
          runId: args["run-id"],
          server: env.GITHUB_SERVER_URL || "https://github.com",
        },
        options,
      );
      console.log(`${args.file}: ${count} verified RC Build attestation(s), one signed by run ${args["run-id"]}`);
      return;
    }
    default:
      throw new Error(`unknown command ${args.command ?? "<none>"}; expected select, verify-artifact, verify-attestation or verify`);
  }
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
  main().catch((error) => {
    console.error(`::error::${error.message}`);
    process.exit(1);
  });
}
