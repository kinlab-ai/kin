#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The native provenance check for one release archive's two executables.
//
// rc-build.yml runs it on the binaries it has just linked, and release.yml runs
// it again on the bytes it is about to publish, on the runner that executes
// them. It used to be pasted inline into both workflows' build jobs and kept
// equal by scripts/check-rc-build-drift.mjs. release.yml no longer compiles, so
// it no longer has a build job to mirror, and one implementation that both
// workflows call is what keeps the producer and the consumer judging bytes by
// the same rule.
//
// What it proves, by running the binaries rather than by reading metadata:
//   - each macOS binary is a thin Mach-O of its target's own architecture, so
//     an x86_64 archive executed under Rosetta 2 is still the x86_64 bytes;
//   - `kin --version` and `kin-daemon --version` both name the expected full
//     commit and neither was built dirty;
//   - `kin bench-meta --json` reports that commit, a clean and known source,
//     the expected package version when one is given, the sha256 of the
//     Cargo.lock it was built from, and the default archive feature profile.
//
// A binary for another architecture is skipped only when the caller allows it.
// rc-build.yml allows it, because the matching macOS leg executes the same
// source. release.yml passes --require-execution, because it runs each archive
// on a runner that can execute it and a skip there would publish bytes nothing
// ran.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { isDeepStrictEqual } from "node:util";

const COMMIT_SHA = /^[0-9a-f]{40}$/;
const VERSION_LINE_SHA = /\(([0-9a-f]{40})(-dirty)?\s/;

export const REQUIRED_FEATURES = Object.freeze(["vector", "external-embeddings"]);
export const FORBIDDEN_FEATURES = Object.freeze(["embeddings", "metal"]);

function usage(message) {
  process.stderr.write(
    `${message}\n\n` +
      "usage: verify-native-release-provenance.mjs --kin <path> --daemon <path> --target <triple>\n" +
      "         --expect-commit <40-hex sha> --cargo-lock <path> --out <dir>\n" +
      "         [--expect-version <version>] [--require-execution]\n",
  );
  process.exit(2);
}

export function parseArgs(argv) {
  const args = { requireExecution: false };
  const take = (flag, index) => {
    const value = argv[index + 1];
    if (value === undefined || value.startsWith("--")) usage(`${flag} needs a value`);
    return value;
  };
  for (let i = 0; i < argv.length; i += 1) {
    const flag = argv[i];
    switch (flag) {
      case "--kin": args.kin = take(flag, i); i += 1; break;
      case "--daemon": args.daemon = take(flag, i); i += 1; break;
      case "--target": args.target = take(flag, i); i += 1; break;
      case "--expect-commit": args.expectCommit = take(flag, i); i += 1; break;
      case "--expect-version": args.expectVersion = take(flag, i); i += 1; break;
      case "--cargo-lock": args.cargoLock = take(flag, i); i += 1; break;
      case "--out": args.out = take(flag, i); i += 1; break;
      case "--require-execution": args.requireExecution = true; break;
      default: usage(`unknown argument: ${flag}`);
    }
  }
  for (const [key, flag] of [
    ["kin", "--kin"],
    ["daemon", "--daemon"],
    ["target", "--target"],
    ["expectCommit", "--expect-commit"],
    ["cargoLock", "--cargo-lock"],
    ["out", "--out"],
  ]) {
    if (!args[key]) usage(`${flag} is required`);
  }
  if (!COMMIT_SHA.test(args.expectCommit)) {
    usage(`--expect-commit must be a 40-character lowercase commit sha, got "${args.expectCommit}"`);
  }
  return args;
}

// The runner's architecture as GitHub names it. RUNNER_ARCH is what the
// workflows always ran this decision on; process.arch stands in for it off a
// runner, so a local run decides the same way.
export function hostArch(env = process.env, arch = process.arch) {
  if (env.RUNNER_ARCH) return env.RUNNER_ARCH;
  if (arch === "x64") return "X64";
  if (arch === "arm64") return "ARM64";
  return arch.toUpperCase();
}

// Whether this host can execute the target's binaries, and how.
//   native   the host runs them directly
//   rosetta  an x86_64 macOS binary on Apple Silicon, run under Rosetta 2
//   skip     another architecture this host cannot run
export function executionPlan(runnerArch, target) {
  if (
    (runnerArch === "X64" && target.startsWith("x86_64-")) ||
    (runnerArch === "ARM64" && target.startsWith("aarch64-"))
  ) {
    return "native";
  }
  if (runnerArch === "ARM64" && target === "x86_64-apple-darwin") {
    return "rosetta";
  }
  return "skip";
}

export function machoArch(target) {
  const arch = target.split("-")[0];
  return arch === "aarch64" ? "arm64" : arch;
}

// Both version lines must carry the same full, clean commit, and it must be the
// one expected.
export function judgeVersionLines(lines, expected) {
  const shas = lines.map((line) => {
    const match = line.match(VERSION_LINE_SHA);
    if (!match) throw new Error(`release version line lacks a full build SHA: ${line}`);
    if (match[2]) throw new Error(`release binary was built dirty: ${line}`);
    return match[1];
  });
  if (shas.some((sha) => sha !== expected) || shas[0] !== shas[1]) {
    throw new Error(`release binary SHAs ${shas.join(", ")} do not match checkout ${expected}`);
  }
  return shas[0];
}

export function judgeBuildMeta(meta, { expectedCommit, expectedVersion, lockSha, target }) {
  if (expectedVersion !== undefined && meta.kin_version !== expectedVersion) {
    throw new Error(
      `kin package version ${meta.kin_version ?? "missing"} does not match release tag ${expectedVersion}`,
    );
  }
  if (meta.kin_commit !== expectedCommit || meta.kin_dirty !== false || meta.kin_source_known !== true) {
    throw new Error("kin bench-meta did not prove a clean, known release source");
  }
  if (meta.dependency_provenance !== lockSha) {
    throw new Error(
      `embedded Cargo.lock provenance ${meta.dependency_provenance ?? "missing"} ` +
        `does not match tagged Cargo.lock ${lockSha}`,
    );
  }
  // Attest the measured default archive profile on every target.
  const flags = Array.isArray(meta.feature_flags) ? meta.feature_flags : [];
  for (const feature of REQUIRED_FEATURES) {
    if (!flags.includes(feature)) {
      throw new Error(`release build for ${target} did not enable ${feature}`);
    }
  }
  for (const feature of FORBIDDEN_FEATURES) {
    if (flags.includes(feature)) {
      throw new Error(`default release build unexpectedly enabled ${feature}`);
    }
  }
  if (meta.embeddings?.vector_enabled !== true ||
      meta.embeddings?.embeddings_enabled !== true ||
      meta.embeddings?.local_inference_enabled !== false ||
      meta.embeddings?.external_embeddings_enabled !== true ||
      meta.embeddings?.metal_enabled !== false ||
      meta.embeddings?.provider !== "none") {
    throw new Error("release metadata does not attest the engine-free default archive profile");
  }
}

const run = (file, args, options = {}) =>
  execFileSync(file, args, { ...options, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], maxBuffer: 64 * 1024 * 1024 });

const identityFields = ["version", "commit_sha", "source_dirty", "source_known", "target", "build_profile"];
const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");
const identityFromMeta = (meta) => ({
  version: meta.kin_version,
  commit_sha: meta.kin_commit,
  source_dirty: meta.kin_dirty,
  source_known: meta.kin_source_known,
  target: meta.kin_target,
  build_profile: meta.kin_build_profile,
});
const identityFromStatus = (binary) => Object.fromEntries(identityFields.map((key) => [key, binary?.[key]]));

function activateCanonicalPair(args, { env, exec, log }) {
  const root = mkdtempSync(join(realpathSync(args.out), "native-runtime-"));
  try {
    const home = join(root, "home"), kinHome = join(home, ".kin"), bin = join(kinHome, "bin");
    mkdirSync(bin, { recursive: true, mode: 0o700 });
    const suffix = args.target.includes("windows") ? ".exe" : "";
    const pair = Object.fromEntries([["cli", args.kin, `kin${suffix}`], ["daemon", args.daemon, `kin-daemon${suffix}`]].map(([role, source, name]) => {
      const bytes = readFileSync(source), path = join(bin, name), hash = sha256(bytes);
      writeFileSync(path, bytes, { flag: "wx", mode: 0o755 });
      if (sha256(readFileSync(path)) !== hash) throw new Error(`canonical ${role} copy changed the selected artifact bytes`);
      return [role, { source: realpathSync(source), path, sha256: hash }];
    }));
    const scopedEnv = Object.fromEntries(Object.entries(env).filter(([key]) =>
      !/^(?:_?KIN_|GIT_|DYLD_|LD_|HOME$|USERPROFILE$|APPDATA$|LOCALAPPDATA$|XDG_)/i.test(key)));
    Object.assign(scopedEnv, { HOME: home, USERPROFILE: home, KIN_HOME: kinHome, KIN_DIR: kinHome,
      KIN_RUNTIME_MODE: "managed", APPDATA: join(home, "appdata"), LOCALAPPDATA: join(home, "local-appdata"),
      XDG_CONFIG_HOME: join(home, "config"), XDG_CACHE_HOME: join(home, "cache") });
    const kinVersion = exec(args.kin, ["--version"], { env: scopedEnv });
    const daemonVersion = exec(args.daemon, ["--version"], { env: scopedEnv });
    writeFileSync(join(args.out, "kin-version.txt"), kinVersion);
    writeFileSync(join(args.out, "kin-daemon-version.txt"), daemonVersion);
    log(kinVersion.trim());
    log(daemonVersion.trim());
    judgeVersionLines([kinVersion.trim(), daemonVersion.trim()], args.expectCommit);
    const probe = (binary, argv, name) => {
      const text = exec(binary, argv, { env: scopedEnv });
      writeFileSync(join(args.out, name), text);
      try { return JSON.parse(text); }
      catch (cause) { throw new Error(`${name} did not print JSON: ${cause.message}`, { cause }); }
    };
    const cliMeta = probe(pair.cli.path, ["bench-meta", "--json", "--runtime-identity-only"], "kin-runtime-build-identity.json");
    if (cliMeta.schema !== "kin.runtime-build-identity.v1") throw new Error("CLI runtime identity schema is unsupported");
    const identity = identityFromMeta(cliMeta);
    if (typeof identity.version !== "string" || !identity.version ||
        !COMMIT_SHA.test(identity.commit_sha ?? "") || identity.commit_sha !== args.expectCommit ||
        identity.source_dirty !== false || identity.source_known !== true ||
        identity.target !== args.target || identity.build_profile !== "release" ||
        (args.expectVersion !== undefined && identity.version !== args.expectVersion)) {
      throw new Error("selected CLI runtime identity did not prove the expected clean, known release target");
    }
    const daemon = probe(pair.daemon.path, ["--compat-json"], "kin-daemon-compat.json");
    if (daemon.schema !== "kin.daemon.compat.v2" || !isDeepStrictEqual(daemon.runtime_identity, identity)) {
      throw new Error("independent daemon runtime identity does not match the selected CLI");
    }
    const status = probe(pair.cli.path, ["runtime", "status", "--json"], "kin-runtime-status.json");
    if (status.schema !== "kin.runtime-status.v1" || status.ready !== true || status.mode !== "managed" ||
        status.pair_matches !== true || status.kin_home !== kinHome) {
      throw new Error("isolated canonical runtime pair is not ready in managed mode");
    }
    for (const role of ["cli", "daemon"]) {
      if (status[role]?.path !== pair[role].path || status[role]?.sha256 !== pair[role].sha256 ||
          !isDeepStrictEqual(identityFromStatus(status[role]), identity) ||
          sha256(readFileSync(pair[role].path)) !== pair[role].sha256 ||
          sha256(readFileSync(pair[role].source)) !== pair[role].sha256) {
        throw new Error(`canonical ${role} path, hash or identity differs from the selected artifact`);
      }
    }
    writeFileSync(join(args.out, "kin-canonical-pair.json"), JSON.stringify({
      schema: "kin.native-provenance-canonical-pair.v1", verified: true, pair, identity,
      mode: status.mode, channel: status.channel, release_kind: status.release_kind,
      scope: "isolated selected archive execution; no public release or installer first-contact claim",
    }, null, 2) + "\n");
    return { root, pair, env: scopedEnv, identity };
  } catch (error) {
    rmSync(root, { recursive: true, force: true });
    throw error;
  }
}

export function verify(args, { env = process.env, exec = run, log = console.log } = {}) {
  const plan = executionPlan(hostArch(env), args.target);
  if (plan === "skip") {
    if (args.requireExecution) {
      throw new Error(
        `this ${hostArch(env)} host cannot execute ${args.target}, and the caller requires the ` +
          "archive it publishes to have been executed",
      );
    }
    log(
      `Skipping execution of cross-architecture artifact ${args.target} on ${hostArch(env)}; ` +
        "the matching macOS leg executes the same source and lock provenance.",
    );
    return { executed: false };
  }
  if (plan === "rosetta") {
    // Refuse rather than skip when Rosetta 2 is missing, because a skip would
    // leave a shipped archive with no execution proof.
    try {
      exec("/usr/bin/arch", ["-x86_64", "/usr/bin/true"]);
    } catch {
      throw new Error(
        `Rosetta 2 cannot run ${args.target} on this ${hostArch(env)} host, so its provenance cannot be executed`,
      );
    }
  }

  // Each macOS binary must be a thin Mach-O of its target's own architecture.
  // On a cross-built leg this is what ties the bytes executed under Rosetta 2
  // to the x86_64 bytes that ship. Keyed on the target rather than the host:
  // only a macOS target reaches here on a macOS host, and the check is about
  // the bytes.
  if (args.target.endsWith("-apple-darwin")) {
    const want = machoArch(args.target);
    for (const bin of [args.kin, args.daemon]) {
      const have = exec("lipo", ["-archs", bin]).trim();
      if (have !== want) {
        throw new Error(`${bin} is a ${have} Mach-O, expected ${want} for ${args.target}`);
      }
      log(`${bin} is a thin ${have} Mach-O`);
    }
  }

  mkdirSync(args.out, { recursive: true });
  const canonical = activateCanonicalPair(args, { env, exec, log });
  try {
    const benchMeta = exec(canonical.pair.cli.path, ["bench-meta", "--json"], { env: canonical.env });
    writeFileSync(join(args.out, "kin-build-meta.json"), benchMeta);
    const lockSha = createHash("sha256").update(readFileSync(args.cargoLock)).digest("hex");
    let meta;
    try {
      meta = JSON.parse(benchMeta);
    } catch (cause) {
      throw new Error(`kin bench-meta --json did not print JSON: ${cause.message}`, { cause });
    }
    judgeBuildMeta(meta, {
      expectedCommit: args.expectCommit,
      expectedVersion: args.expectVersion,
      lockSha,
      target: args.target,
    });
    const fullIdentity = identityFromMeta(meta);
    for (const key of identityFields) {
      if ((["target", "build_profile"].includes(key) && fullIdentity[key] === undefined)) continue;
      if (fullIdentity[key] !== canonical.identity[key]) {
        throw new Error("full metadata identity differs from the independently verified canonical pair");
      }
    }
    for (const binary of Object.values(canonical.pair)) {
      if (sha256(readFileSync(binary.path)) !== binary.sha256 || sha256(readFileSync(binary.source)) !== binary.sha256) {
        throw new Error("selected artifact bytes changed during full metadata execution");
      }
    }
    log(
      `${args.target}: kin and kin-daemon executed ${plan === "rosetta" ? "under Rosetta 2" : "natively"} ` +
        `and proved ${args.expectCommit}, a clean known source, Cargo.lock ${lockSha} and the default archive profile`,
    );
    return { executed: true, plan, lockSha };
  } finally {
    rmSync(canonical.root, { recursive: true, force: true });
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
  try {
    verify(parseArgs(process.argv.slice(2)));
  } catch (error) {
    console.error(`::error::${error.message}`);
    process.exit(1);
  }
}
