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

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const entrypoint = fs.readFileSync(path.join(root, "docker-entrypoint.sh"), "utf8");
const hash = (file) => createHash("sha256").update(fs.readFileSync(file)).digest("hex");

function fixture(run) {
  const dir = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "kin-entrypoint-runtime-")));
  try {
    const home = path.join(dir, "home"), runtime = path.join(home, ".kin"), workspace = path.join(dir, "workspace");
    const image = path.join(dir, "image"), events = path.join(dir, "events.jsonl");
    fs.mkdirSync(image); fs.mkdirSync(home); fs.mkdirSync(workspace);
    for (const name of ["kin", "kin-daemon"]) fs.writeFileSync(path.join(image, name), `#!/usr/bin/env node
const fs=require("node:fs"),path=require("node:path");
const args=process.argv.slice(2),name=${JSON.stringify(name)};
const event={name,args,exe:fs.realpathSync(process.argv[1]),home:process.env.HOME,kin_home:process.env.KIN_HOME,mode:process.env.KIN_RUNTIME_MODE,layout:process.env.KIN_STORE_LAYOUT,digest:process.env.KIN_RELEASE_DAEMON_DIGEST_INTERNAL};
fs.appendFileSync(process.env.FIXTURE_EVENTS,JSON.stringify(event)+"\\n");
if(name==="kin"&&args[0]==="runtime") {console.log(JSON.stringify({schema:"kin.runtime-status.v1",ready:process.env.FIXTURE_READY!=="0",mode:event.mode}));process.exit(process.env.FIXTURE_READY==="0"?1:0);}
if(name==="kin"&&args[0]==="init") {const repo=path.join(args[1],".kin");fs.mkdirSync(path.join(repo,"kindb"),{recursive:true});for(const file of ["manifest.json","config.toml","version"])fs.writeFileSync(path.join(repo,file),"fixture");}
`, { mode: 0o755 });
    const script = path.join(dir, "entrypoint.sh");
    fs.writeFileSync(script, entrypoint.replaceAll("/usr/local/bin/kin-daemon", path.join(image, "kin-daemon")).replaceAll("/usr/local/bin/kin", path.join(image, "kin")));
    const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !/^_?KIN_|^(?:HOME|USERPROFILE|FIXTURE_)/.test(key)));
    Object.assign(env, { HOME: home, KIN_HOME: runtime, KIN_WORKSPACE_DIR: workspace, FIXTURE_EVENTS: events });
    const execute = (over = {}) => {
      const result = spawnSync("sh", [script, "--port", "4219"], { env: { ...env, ...over }, encoding: "utf8", timeout: 15_000 });
      const observed = fs.existsSync(events) ? fs.readFileSync(events, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse) : [];
      return { ...result, observed };
    };
    const seed = () => { fs.mkdirSync(path.join(runtime, "bin"), { recursive: true }); for (const name of ["kin", "kin-daemon"]) fs.copyFileSync(path.join(image, name), path.join(runtime, "bin", name)); };
    run({ dir, home, runtime, workspace, image, execute, seed });
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}

test("both image stages seed root-owned default canonical hardlinks without fixing KIN_HOME", () => {
  const docker = fs.readFileSync(path.join(root, "Dockerfile"), "utf8");
  assert.equal((docker.match(/ln \/usr\/local\/bin\/kin \/home\/kin\/\.kin\/bin\/kin/g) ?? []).length, 2);
  assert.equal((docker.match(/ln \/usr\/local\/bin\/kin-daemon \/home\/kin\/\.kin\/bin\/kin-daemon/g) ?? []).length, 2);
  assert.ok(!/^ENV KIN_HOME=/m.test(docker));
  assert.ok(!/chown[^\n]*\/usr\/local\/bin/.test(docker));
});

test("fresh local runtime verifies image bytes before init and executes the canonical daemon", () => fixture(({ runtime, image, execute }) => {
  const result = execute(); assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(result.observed.map((x) => x.args[0]), ["runtime", "init", "--port"]);
  assert.ok(result.observed.every((x) => x.mode === "managed" && x.kin_home === runtime));
  assert.equal(result.observed.at(-1).exe, path.join(runtime, "bin", "kin-daemon"));
  for (const name of ["kin", "kin-daemon"]) assert.equal(hash(path.join(runtime, "bin", name)), hash(path.join(image, name)));
}));

test("matching mounted pair is reused without replacing either executable", () => fixture(({ runtime, seed, execute }) => {
  seed(); const before = ["kin", "kin-daemon"].map((name) => fs.statSync(path.join(runtime, "bin", name)).ino);
  const result = execute(); assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(["kin", "kin-daemon"].map((name) => fs.statSync(path.join(runtime, "bin", name)).ino), before);
}));

for (const arm of ["partial", "mismatch"]) test(`${arm} mounted pair refuses before init and preserves repository and binary bytes`, () => fixture(({ runtime, workspace, seed, execute }) => {
  seed(); const cli = path.join(runtime, "bin", "kin"), daemon = path.join(runtime, "bin", "kin-daemon");
  if (arm === "partial") fs.unlinkSync(daemon); else fs.appendFileSync(cli, "\n// different image\n");
  const before = hash(cli); fs.mkdirSync(path.join(workspace, ".kin"));
  const sentinel = path.join(workspace, ".kin", "preserve"); fs.writeFileSync(sentinel, "existing repository");
  const result = execute(); assert.equal(result.status, 1); assert.equal(result.observed.length, 0);
  assert.equal(hash(cli), before); assert.equal(fs.readFileSync(sentinel, "utf8"), "existing repository");
  if (arm === "partial") assert.ok(!fs.existsSync(daemon));
}));

test("not-ready managed predicate refuses before initialization or daemon launch", () => fixture(({ execute }) => {
  const result = execute({ FIXTURE_READY: "0" }); assert.equal(result.status, 1);
  assert.deepEqual(result.observed.map((x) => x.args[0]), ["runtime"]);
}));

test("GCS init preserves selected persistent home and layout while final daemon retains hosted environment and path", () => fixture(({ home, runtime, image, execute }) => {
  const result = execute({ KIN_STORAGE: "gcs", KIN_RUNTIME_MODE: "development", KIN_STORE_LAYOUT: "in-tree", KIN_RELEASE_DAEMON_DIGEST_INTERNAL: "fixture-image-digest" });
  assert.equal(result.status, 0, result.stderr);
  const init = result.observed.find((x) => x.args[0] === "init"), daemon = result.observed.at(-1);
  assert.equal(init.home, home); assert.equal(init.kin_home, runtime); assert.equal(init.mode, "managed"); assert.equal(init.layout, "in-tree");
  assert.equal(daemon.exe, path.join(image, "kin-daemon")); assert.equal(daemon.mode, "development");
  assert.equal(daemon.kin_home, runtime); assert.equal(daemon.digest, "fixture-image-digest");
}));

test("materialized GCS repository launches original hosted daemon without touching an unrelated managed slot", () => fixture(({ runtime, workspace, image, execute }) => {
  fs.mkdirSync(path.join(workspace, ".kin", "kindb"), { recursive: true });
  for (const name of ["manifest.json", "config.toml", "version"]) fs.writeFileSync(path.join(workspace, ".kin", name), "existing");
  fs.mkdirSync(path.join(runtime, "bin"), { recursive: true }); fs.writeFileSync(path.join(runtime, "bin", "kin"), "unrelated managed slot");
  const before = hash(path.join(runtime, "bin", "kin"));
  const result = execute({ KIN_STORAGE: "gcs" }); assert.equal(result.status, 0, result.stderr);
  assert.equal(result.observed.length, 1); assert.equal(result.observed[0].exe, path.join(image, "kin-daemon"));
  assert.equal(hash(path.join(runtime, "bin", "kin")), before);
}));
