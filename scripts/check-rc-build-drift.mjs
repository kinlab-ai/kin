#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// RC Build produces every byte a release ships; release.yml only consumes.
//
// This file used to hold rc-build.yml's build job to a byte-for-byte copy of
// release.yml's, because both compiled the same archives: the candidate for the
// preflight to judge, and then the release again at the tag. release.yml no
// longer compiles. It takes the RC Build archives and image the preflight
// judged, signs the macOS ones, and publishes, so there is no second build job
// to mirror. What has to hold instead is the producer and consumer contract,
// and this checks it on every pull request that touches either workflow:
//
//   1. release.yml compiles nothing. No active line runs cargo, cross, rustup
//      or a Docker build, uses the Rust toolchain action, or reads a Cargo
//      target directory.
//   2. rc-build.yml is the producer. Its build matrix is exactly the archive
//      table scripts/locate-rc-build.mjs locates, its image job builds the
//      daemon image once as an OCI archive, and its attest job attests every
//      archive and the image.
//   3. release.yml ships exactly what rc-build.yml builds. Its shipping
//      matrices cover every RC Build archive on the same target, macOS only
//      in the signing job, and its publish inventory names exactly those
//      archives. Every byte it ships is downloaded from the located RC Build
//      run and checked there: the artifact and its attestation.
//   4. Both judge bytes by one rule: each runs
//      scripts/verify-native-release-provenance.mjs, release.yml with
//      --require-execution, and neither keeps an inline copy of it.
//   5. rc-build.yml holds no release credential: no secret, no environment,
//      and none of the signing steps, which live in release.yml's sign_macos.
//
// It does not compare the files loosely; each rule reads the jobs it is about
// and fails naming what moved.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import { RC_ARCHIVES, RC_IMAGE } from "./locate-rc-build.mjs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
export const RELEASE_WORKFLOW = ".github/workflows/release.yml";
export const RC_WORKFLOW = ".github/workflows/rc-build.yml";

// The release.yml jobs that ship RC Build archives, and which archives each
// may ship. macOS archives are signed; every other one ships unchanged.
export const SHIPPING_JOBS = Object.freeze({
  ship_rc_archives: (target) => !target.endsWith("-apple-darwin"),
  sign_macos: (target) => target.endsWith("-apple-darwin"),
});
// The jobs that take bytes from the located RC Build run.
export const CONSUMING_JOBS = Object.freeze(["ship_rc_archives", "sign_macos", "publish_daemon_image"]);
export const LOCATED_RUN = "run-id: ${{ needs.locate_rc_build.outputs.run_id }}";

// The signing steps. They exist in release.yml's signing job and nowhere in
// rc-build.yml, which reads no secret.
export const SIGNING_STEPS = Object.freeze([
  "Resolve macOS signing credential availability",
  "Import code signing certificate (macOS)",
  "Sign macOS binaries",
  "Stage signed binaries for notarization (macOS)",
  "Notarize macOS binaries (notarytool)",
  "Upload signed binaries for Linux notarization",
]);

// Anything on an active line of release.yml that compiles. `docker buildx
// imagetools` reads a registry and is not a build.
export const COMPILE_PATTERNS = Object.freeze([
  [/(^|[\s;&|(])cargo\s/, "runs cargo"],
  [/(^|[\s;&|(])cross\s+build/, "runs cross"],
  [/(^|[\s;&|(])rustup\s/, "runs rustup"],
  [/(^|[\s;&|(])rustc\s/, "runs rustc"],
  [/docker\s+(buildx\s+)?build(\s|$)/, "runs a Docker build"],
  [/docker\/build-push-action@/, "uses docker/build-push-action"],
  [/\.\/\.github\/actions\/rust-toolchain/, "installs the Rust toolchain"],
  [/target\/(\$\{?TARGET\}?|\$\{\{\s*matrix\.target\s*\}\})\/release/, "reads a Cargo target directory"],
]);

// Text only the native provenance check itself carries. An inline copy in
// either workflow is a second rule that can drift from the script.
export const INLINE_PROVENANCE_MARKERS = Object.freeze([
  "kin bench-meta did not prove a clean, known release source",
  "release binary SHAs",
]);

export function jobBody(text, job, workflow) {
  const lines = text.split("\n");
  const start = lines.findIndex((line) => line === `  ${job}:`);
  if (start === -1) throw new Error(`${workflow}: no ${job} job`);
  let end = lines.length;
  for (let i = start + 1; i < lines.length; i += 1) {
    if (/^  [A-Za-z_][A-Za-z0-9_-]*:\s*$/.test(lines[i])) {
      end = i;
      break;
    }
  }
  // The comment block introducing the next job documents that job.
  while (end > start && /^ {2}#/.test(lines[end - 1])) end -= 1;
  return lines.slice(start, end).join("\n");
}

export function buildJobBody(text, workflow) {
  return jobBody(text, "build", workflow);
}

export function matrixRows(body, workflow) {
  const lines = body.split("\n");
  const start = lines.findIndex((line) => line === "        include:");
  if (start === -1) throw new Error(`${workflow}: job has no matrix include`);
  const rows = [];
  let current = null;
  for (let i = start + 1; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^\s*#/.test(line) || line.trim() === "") continue;
    if (line.startsWith("          - ")) {
      current = [`${line.slice("          - ".length)}`];
      rows.push(current);
      continue;
    }
    if (line.startsWith("            ")) {
      if (!current) throw new Error(`${workflow}: matrix key before any row`);
      current.push(line.trim());
      continue;
    }
    break;
  }
  return new Map(
    rows.map((row) => {
      const artifact = row.find((entry) => entry.startsWith("artifact:"));
      if (!artifact) throw new Error(`${workflow}: a matrix row names no artifact`);
      return [artifact.slice("artifact:".length).trim(), row];
    }),
  );
}

const rowValue = (row, key) => row.find((entry) => entry.startsWith(`${key}:`))?.slice(key.length + 1).trim();

export function activeLines(text) {
  return text.split("\n").filter((line) => !/^\s*#/.test(line));
}

function stepNames(text) {
  return [...text.matchAll(/^\s+- name: (.*)$/gm)].map((match) => match[1].trim());
}

export function check(releaseText, rcText) {
  const problems = [];
  const guard = (fn) => {
    try {
      fn();
    } catch (error) {
      problems.push(error.message);
    }
  };

  // 1. release.yml compiles nothing.
  activeLines(releaseText).forEach((line) => {
    for (const [pattern, what] of COMPILE_PATTERNS) {
      if (pattern.test(line)) {
        problems.push(`${RELEASE_WORKFLOW} ${what}, but RC Build is the only producer of release bytes: ${line.trim()}`);
      }
    }
  });

  // 2. rc-build.yml produces exactly the located archives and the image.
  guard(() => {
    const rows = matrixRows(buildJobBody(rcText, RC_WORKFLOW), RC_WORKFLOW);
    const built = [...rows.keys()];
    const located = RC_ARCHIVES.map((entry) => entry.artifact);
    if (JSON.stringify(built) !== JSON.stringify(located)) {
      problems.push(`${RC_WORKFLOW} builds ${built.join(", ")}, but scripts/locate-rc-build.mjs locates ${located.join(", ")}`);
    }
    for (const entry of RC_ARCHIVES) {
      const row = rows.get(entry.artifact);
      if (row && rowValue(row, "target") !== entry.target) {
        problems.push(`${RC_WORKFLOW} builds ${entry.artifact} for ${rowValue(row, "target")}, not ${entry.target}`);
      }
    }
  });
  guard(() => {
    const image = jobBody(rcText, "image", RC_WORKFLOW);
    const builds = activeLines(image).filter((line) => /docker\s+buildx\s+build/.test(line)).length;
    if (builds !== 1) problems.push(`${RC_WORKFLOW}'s image job must build the daemon image exactly once, found ${builds}`);
    if (!image.includes("type=oci,dest=") || !image.includes(`${RC_IMAGE.archive}`)) {
      problems.push(`${RC_WORKFLOW}'s image job must export ${RC_IMAGE.archive} as an OCI archive, so its digests survive the push`);
    }
    if (!image.includes(`name: ${RC_IMAGE.artifact}`)) {
      problems.push(`${RC_WORKFLOW}'s image job must upload the ${RC_IMAGE.artifact} artifact release.yml locates`);
    }
  });
  guard(() => {
    const attest = jobBody(rcText, "attest", RC_WORKFLOW);
    const subjects = [...attest.matchAll(/^\s+\$\{\{ runner\.temp \}\}\/rc\/(\S+)$/gm)].map((match) => match[1]).sort();
    const expected = [
      ...RC_ARCHIVES.flatMap((entry) => entry.archives.map((name) => `${entry.artifact}/${name}`)),
      `${RC_IMAGE.artifact}/${RC_IMAGE.archive}`,
    ].sort();
    if (JSON.stringify(subjects) !== JSON.stringify(expected)) {
      problems.push(`${RC_WORKFLOW}'s attest job attests ${subjects.join(", ") || "nothing"}, expected ${expected.join(", ")}`);
    }
    if (!/needs: \[build, image\]/.test(attest)) {
      problems.push(`${RC_WORKFLOW}'s attest job must wait for both the build and image jobs`);
    }
  });

  // 3. release.yml ships exactly what rc-build.yml builds, from the located run.
  guard(() => {
    const shipped = new Map();
    for (const [job, admits] of Object.entries(SHIPPING_JOBS)) {
      for (const [artifact, row] of matrixRows(jobBody(releaseText, job, RELEASE_WORKFLOW), RELEASE_WORKFLOW)) {
        const target = rowValue(row, "target");
        if (shipped.has(artifact)) problems.push(`${RELEASE_WORKFLOW} ships ${artifact} from two jobs`);
        shipped.set(artifact, target);
        if (!admits(target ?? "")) problems.push(`${RELEASE_WORKFLOW}'s ${job} ships ${artifact} (${target}), which it may not`);
      }
    }
    for (const entry of RC_ARCHIVES) {
      if (!shipped.has(entry.artifact)) {
        problems.push(`${RC_WORKFLOW} builds ${entry.artifact}, which ${RELEASE_WORKFLOW} does not ship`);
      } else if (shipped.get(entry.artifact) !== entry.target) {
        problems.push(`${RELEASE_WORKFLOW} ships ${entry.artifact} as ${shipped.get(entry.artifact)}, but RC Build builds it for ${entry.target}`);
      }
    }
    for (const artifact of shipped.keys()) {
      if (!RC_ARCHIVES.some((entry) => entry.artifact === artifact)) {
        problems.push(`${RELEASE_WORKFLOW} ships ${artifact}, which ${RC_WORKFLOW} does not build`);
      }
    }
  });
  guard(() => {
    const publish = jobBody(releaseText, "publish", RELEASE_WORKFLOW);
    const start = publish.indexOf("          assets=(");
    const end = publish.indexOf("          )", start);
    if (start === -1 || end === -1) throw new Error(`${RELEASE_WORKFLOW}'s publish job no longer lists its asset inventory`);
    const inventory = publish.slice(start, end).split("\n").slice(1).map((line) => line.trim()).filter(Boolean).sort();
    const expected = RC_ARCHIVES.flatMap((entry) => entry.archives).sort();
    if (JSON.stringify(inventory) !== JSON.stringify(expected)) {
      problems.push(`${RELEASE_WORKFLOW} publishes ${inventory.join(", ")}, but RC Build builds ${expected.join(", ")}`);
    }
  });
  for (const job of CONSUMING_JOBS) {
    guard(() => {
      const body = jobBody(releaseText, job, RELEASE_WORKFLOW);
      const downloads = body.split("uses: actions/download-artifact@").slice(1);
      if (downloads.length !== 1) {
        problems.push(`${RELEASE_WORKFLOW}'s ${job} must download exactly one artifact from the located RC Build run, found ${downloads.length} downloads`);
      }
      for (const download of downloads) {
        const withBlock = download.split("\n      - ")[0];
        if (!withBlock.includes(LOCATED_RUN)) {
          problems.push(`${RELEASE_WORKFLOW}'s ${job} downloads from somewhere other than the located RC Build run`);
        }
      }
      for (const command of ["scripts/locate-rc-build.mjs verify-artifact", "scripts/locate-rc-build.mjs verify-attestation"]) {
        if (!activeLines(body).some((line) => line.includes(command))) {
          problems.push(`${RELEASE_WORKFLOW}'s ${job} must run ${command} on the bytes it ships`);
        }
      }
    });
  }

  // 4. One native provenance rule, run by producer and consumer.
  const PROVENANCE = "node scripts/verify-native-release-provenance.mjs";
  guard(() => {
    const producer = activeLines(buildJobBody(rcText, RC_WORKFLOW)).filter((line) => line.includes(PROVENANCE)).length;
    if (producer !== 1) problems.push(`${RC_WORKFLOW}'s build job must run ${PROVENANCE} once, found ${producer}`);
  });
  for (const [job, expected] of [["ship_rc_archives", 1], ["sign_macos", 2]]) {
    guard(() => {
      const body = jobBody(releaseText, job, RELEASE_WORKFLOW);
      const runs = activeLines(body).filter((line) => line.includes(PROVENANCE)).length;
      const required = activeLines(body).filter((line) => line.includes("--require-execution")).length;
      if (runs !== expected || required !== expected) {
        problems.push(`${RELEASE_WORKFLOW}'s ${job} must run ${PROVENANCE} --require-execution ${expected} time(s), found ${runs} run(s) and ${required} --require-execution`);
      }
    });
  }
  for (const [workflow, text] of [[RELEASE_WORKFLOW, releaseText], [RC_WORKFLOW, rcText]]) {
    for (const marker of INLINE_PROVENANCE_MARKERS) {
      if (text.includes(marker)) {
        problems.push(`${workflow} carries an inline copy of the native provenance check ("${marker}"); call the script instead`);
      }
    }
  }

  // 5. rc-build.yml holds no release credential.
  const rcActive = activeLines(rcText).join("\n");
  if (/\$\{\{\s*secrets\./.test(rcActive)) problems.push(`${RC_WORKFLOW} reads a secret; it must hold no release credential`);
  if (/^\s+environment:/m.test(rcActive)) problems.push(`${RC_WORKFLOW} declares an environment; it must hold no release credential`);
  const rcSteps = stepNames(rcText);
  const signSteps = (() => {
    try {
      return stepNames(jobBody(releaseText, "sign_macos", RELEASE_WORKFLOW));
    } catch {
      return [];
    }
  })();
  for (const name of SIGNING_STEPS) {
    if (rcSteps.includes(name)) problems.push(`${RC_WORKFLOW} carries the signing step "${name}", which belongs to ${RELEASE_WORKFLOW}`);
    if (!signSteps.includes(name)) problems.push(`${RELEASE_WORKFLOW}'s sign_macos job no longer carries "${name}"`);
  }
  return problems;
}

export function checkFiles(root = ROOT) {
  return check(
    readFileSync(join(root, RELEASE_WORKFLOW), "utf8"),
    readFileSync(join(root, RC_WORKFLOW), "utf8"),
  );
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const problems = checkFiles();
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(`::error::${problem}`);
    }
    console.error(
      `\n${RC_WORKFLOW} is the only producer of the bytes a release ships and ${RELEASE_WORKFLOW} only consumes them.\n` +
        `Change both sides together, and scripts/locate-rc-build.mjs's archive table with them.`,
    );
    process.exit(1);
  }
  console.log(
    `${RC_WORKFLOW} produces and ${RELEASE_WORKFLOW} consumes: ${RC_ARCHIVES.length} archives and the daemon image, ` +
      `nothing compiled at the tag, one native provenance rule, signing only in the release.`,
  );
}
