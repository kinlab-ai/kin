// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Run the workspace page against a small DOM and synthetic responses shaped
// like the local server's. No repository, runtime, browser, or mutation
// endpoint is contacted. The map engine is the real graph.js.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const test = require("node:test");
const assets = path.resolve(__dirname, "../crates/kin-cli/assets/workspace_ui");
const source = fs.readFileSync(path.join(assets, "workspace.js"), "utf8");
const graphSource = fs.readFileSync(path.join(assets, "graph.js"), "utf8");
const flush = async () => { for (let i = 0; i < 4; i++) await new Promise(setImmediate); };
const refWire = (name) => ({ bytes_hex: Buffer.from(`refs/heads/${name}`).toString("hex") });
const hashWire = (byte) => Array(32).fill(byte);
const uuid = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;

class Element {
  constructor(tag, document) {
    this.tagName = tag.toUpperCase(); this.ownerDocument = document; this.children = []; this.parentNode = null;
    this.attrs = {}; this.dataset = {}; this.events = {}; this.value = ""; this.disabled = false; this.style = {};
    this.classList = { toggle: (name, on) => { const list = new Set(this.className.split(" ").filter(Boolean)); if (on ?? !list.has(name)) list.add(name); else list.delete(name); this.className = [...list].join(" "); } };
  }
  get id() { return this.attrs.id || ""; }
  get type() { return this.attrs.type || "text"; }
  get className() { return this.attrs.class || ""; }
  set className(value) { this.attrs.class = value; }
  get textContent() { return this.text ?? this.children.map((child) => child.textContent).join(""); }
  set textContent(value) { this.children = []; this.text = String(value); }
  setAttribute(key, value) { this.attrs[key] = String(value); if (key === "value") this.value = String(value); if (key === "disabled") this.disabled = true; if (key === "checked") this.checked = true; if (key.startsWith("data-")) this.dataset[key.slice(5).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] = String(value); }
  getAttribute(key) { return this.attrs[key] ?? null; }
  removeAttribute(key) { delete this.attrs[key]; }
  append(...children) { delete this.text; for (const child of children) { const node = child instanceof Element ? child : this.ownerDocument.createTextNode(String(child)); node.parentNode = this; this.children.push(node); } }
  replaceChildren(...children) { this.children.forEach((child) => { child.parentNode = null; }); this.children = []; this.append(...children); }
  remove() { if (this.parentNode) this.parentNode.children = this.parentNode.children.filter((child) => child !== this); this.parentNode = null; }
  contains(node) { return node === this || this.children.some((child) => child.contains(node)); }
  matches(selector) {
    const tag = selector.match(/^[a-z][\w-]*/i)?.[0];
    if (tag && this.tagName !== tag.toUpperCase()) return false;
    for (const [, id] of selector.matchAll(/#([\w-]+)/g)) if (this.id !== id) return false;
    for (const [, cls] of selector.matchAll(/\.([\w-]+)/g)) if (!this.className.split(" ").includes(cls)) return false;
    for (const [, name, value] of selector.matchAll(/\[([\w-]+)(?:=["']?([^\]"']+)["']?)?\]/g)) if (value ? this.getAttribute(name) !== value : this.getAttribute(name) === null) return false;
    return true;
  }
  querySelectorAll(selector) {
    if (selector.includes(",")) return [...new Set(selector.split(",").flatMap((part) => this.querySelectorAll(part.trim())))];
    const parts = selector.trim().split(/\s+/); const found = [];
    const visit = (node) => { for (const child of node.children) { if (child.matches(parts.at(-1))) { let parent = child.parentNode; let index = parts.length - 2; while (parent && index >= 0) { if (parent.matches(parts[index])) index--; parent = parent.parentNode; } if (index < 0) found.push(child); } visit(child); } };
    visit(this); return found;
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  closest(selector) { return selector.split(",").some((part) => this.matches(part.trim())) ? this : this.parentNode?.closest(selector) || null; }
  addEventListener(type, listener) { (this.events[type] ||= []).push(listener); }
  removeEventListener() {}
  emit(type, values = {}) { const event = { target: this, type, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, ...values }; for (const listener of this.events[type] || []) listener(event); }
  click() { if (!this.disabled) this.emit("click"); }
  focus() { this.ownerDocument.activeElement = this; }
  setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
}
class Input extends Element {}

const guide = (summary = {}, extra = {}) => ({ schema: "kin.review.guide.v1", change: { base: "change-1" }, summary: { added: 0, modified: 0, removed: 0, recorded_consumers: 0, recorded_tests: 0, consumers_not_updated: 0, ...summary }, chapters: [], working_tree: { differs: false, derived: true, caught_up: true }, ...extra });

function fixture({ merge = true, preview = true, initialFailure = false, tabStorage = new Map(), observedIdentity = true, newServer = true, hash = "#/home", origin = { owner: "pallets", host: "github.com" }, repositories = null } = {}) {
  const document = new Element("document", null); document.ownerDocument = document;
  document.createElement = (tag) => new (tag === "input" ? Input : Element)(tag, document);
  document.createElementNS = (_, tag) => document.createElement(tag);
  document.createTextNode = (value) => { const node = new Element("#text", document); node.text = value; return node; };
  document.getElementById = (id) => document.querySelector(`#${id}`);
  document.documentElement = document.createElement("html"); document.body = document.createElement("body");
  document.append(document.documentElement); document.documentElement.append(document.body); document.activeElement = document.body;
  document.body.dataset.apiBase = "/repositories/fixture";
  const add = (tag, id, parent = document.body) => { const node = document.createElement(tag); node.setAttribute("id", id); parent.append(node); return node; };
  const nav = add("nav", "workspace-nav");
  for (const route of ["home", "explore", "changes", "branches"]) { const node = add("a", `nav-${route}`, nav); node.dataset.route = route; }
  for (const id of ["workspace-refresh", "workspace-theme"]) add("button", id);
  add("main", "workspace-main");
  for (const id of ["workspace-status", "workspace-status-text", "workspace-announcer", "repository-name", "repository-branch", "workspace-repository-picker", "workspace-version", "workspace-tip"]) add("div", id);
  const events = {}; const requests = []; const failures = new Map(); const held = new Map();
  if (initialFailure) failures.set("/api/workspace/session", "Session disconnected");
  const expected = { repository_id: "repository-a", workspace_id: "workspace-a", roots: { generation: 4 }, target_ref: refWire("main"), source_ref: refWire("feature"), ours_change: hashWire(1), theirs_change: hashWire(2), workspace_tree_hash: hashWire(3) };
  const data = {
    "/api/workspace/session": { repository: { name: "Fixture", ...origin, key: "fixture", repository_id: observedIdentity ? "repository-a" : null, workspace_id: observedIdentity ? "workspace-a" : null }, repositories: repositories || [{ key: "fixture", name: "Fixture", ...origin, selected: true, url: "/repositories/fixture/workspace/" }], runtime: { state: "unknown", detail: "Not observed" }, capabilities: { graph: true, review: true, merge, merge_preview: preview, branch_switch: true }, token: "synthetic-token", actor: "Reader", review_url: "/repositories/fixture/review/", merge_unavailable_reason: "Confirmation requires explicit authorization.", version: "0.8.2" },
    "/api/changes": { changes: [{ id: "change-0", title: "Update Kin's understanding of this code for Kin 0.8.2", author: "Dana Reader", timestamp: "2026-01-02T00:00:00Z", origin: "native", entity_deltas: 10, kin_checkpoint: true }, { id: "change-1", title: "Preserved change", author: "Dana Reader <dana@example.invalid> 1749933342 -0700", timestamp: "2026-01-01T00:00:00Z", entity_deltas: 2 }, { id: "change-2", title: "Merge branch", author: "Dana Reader", timestamp: "2025-12-01T00:00:00Z", entity_deltas: 7, parents: 2 }], truncated: false },
    // Each change's own guided review count: the merge counts what it brought in.
    "/api/counts": { counts: { "change-0": { entities: 10, module_containers: 0, containers_through_members: 0 }, "change-1": { entities: 2, module_containers: 1, containers_through_members: 0 }, "change-2": { entities: 4, module_containers: 2, containers_through_members: 1, merge: true, merge_counted: true } } },
    "/api/standing": { standing: { "change-1": { state: "approved" } } },
    "/api/branches": { branches: [{ name: "main", active: true, default: true }, { name: "feature", active: false }] },
    "/api/workspace/conflicts/details": { schema: "kin.workspace.conflicts.v1", report: { record: null }, state: { status: "none", active: false } },
    "/api/workspace/branches": { schema: "kin.branch-list.v1", branches: [{ name: refWire("main"), active: true }, { name: refWire("feature"), active: false }] },
    "/api/workspace/branch/preview": { schema: "kin.branch-switch-preview.v1", expected: { repository_id: "repository-a", workspace_id: "workspace-a", workspace_tree: hashWire(3), target_ref: refWire("feature"), target_change: hashWire(2) }, source: { display: "refs/heads/main" }, target: { display: "refs/heads/feature", name: refWire("feature"), change: hashWire(2) }, pending_work: { present: true, detail: "Compatible pending work is carried." }, limitations: ["The selected state is rechecked before switching."] },
    "/api/workspace/branch/confirm": { mutated: true, report: { schema: "kin.branch-list.v1", repository_id: "repository-a", workspace_id: "workspace-a", branches: [{ name: refWire("feature"), active: true }] } },
    "/api/guide": guide({ modified: 1, recorded_consumers: 16, recorded_tests: 2 }, { chapters: [{ steps: [{ name: "want_bytes", kind: "function", entity_id: uuid(2) }] }] }),
    "/api/workspace/merge/preview": { schema: "kin.merge-preview.v1", expected, base_change: hashWire(0), outcome: "merged", conflicts: [], conflicts_total: 0, limitations: ["Publication checks run on confirmation."] },
    "/api/workspace/merge/confirm": { mutated: true, report: { source_ref: refWire("feature"), target_ref: refWire("main"), outcome: "merged", merge_change: hashWire(4) } },
    "/api/workspace/graph": { schema: "kin.workspace.graph.v1", scope: "workspace", repository_id: "repository-a", offset: 0, page_size: 50,
      graph: { nodes: [{ id: uuid(1), name: "encoding", kind: "Module", degree: 3 }, { id: uuid(2), name: "want_bytes", kind: "Function", degree: 20 }], links: [{ source: uuid(2), target: uuid(1), kind: "Contains" }], entity_count: 40, relation_count: 90, unresolved_links: 0, filtered_links: 0, sampled: true },
      inventory: { repo_id: "repository-a", total: 40, entities: [{ id: uuid(2), name: "want_bytes", kind: "Function", dependents: 20, signature: "def want_bytes(s)" }] } },
  };
  if (newServer) Object.assign(data, {
    "/api/workspace/overview": { schema: "kin.workspace.overview.v1", counts: { ok: true, entities: 216, relationships: 602 },
      most_relied_on: { ok: true, items: [{ id: uuid(2), name: "want_bytes", kind: "function", signature: "def want_bytes(s)", container: { id: uuid(1), name: "encoding", kind: "module" }, dependents: 23 }, { id: uuid(3), name: "Signer", kind: "class", container: { id: uuid(4), name: "signer", kind: "module" }, dependents: 22 }], total: 172, shown: 2 } },
    "/api/workspace/map": { schema: "kin.workspace.map.v1", nodes: [
      { id: uuid(1), name: "encoding", kind: "module", role: "container", size: 6, degree: 3, parent: null, group: uuid(1) },
      { id: uuid(4), name: "signer", kind: "module", role: "container", size: 30, degree: 11, parent: null, group: uuid(4) },
      { id: uuid(2), name: "want_bytes", kind: "function", role: "entity", degree: 20, parent: uuid(1), group: uuid(1) },
      { id: uuid(3), name: "Signer", kind: "class", role: "entity", degree: 34, parent: uuid(4), group: uuid(4) }],
      edges: [{ source: uuid(4), target: uuid(1), level: "container", weight: 14 }, { source: uuid(3), target: uuid(2), level: "entity", weight: 1 }],
      totals: { entities: 216, relationships: 602, containers: 15 }, sampled: true },
    "/api/workspace/ask": { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [
      { id: uuid(5), name: "Signer.verify_signature", kind: "method", signature: "def verify_signature(self, value, sig) -> bool", container: { id: uuid(3), name: "Signer", kind: "class" }, matched: ["meaning"], why: "Its description and code share words with the question." },
      { id: uuid(2), name: "want_bytes", kind: "function", signature: "def want_bytes(s)", container: { id: uuid(1), name: "encoding", kind: "module" }, matched: ["name", "meaning"], why: "Its name matches." }], total: 2 },
  });
  const location = { origin: "http://local.test", href: "http://local.test/repositories/fixture/workspace/", hash, assign: (url) => { location.assigned = url; }, reload() {} };
  let operation = 0;
  const maps = [];
  const context = {
    document, location, URL, TextDecoder, Node: Element, HTMLElement: Element, HTMLInputElement: Input, AbortController, setTimeout, clearTimeout, Date, Intl,
    localStorage: { getItem: () => null, setItem() {} },
    sessionStorage: { getItem: (key) => tabStorage.get(key) || null, setItem: (key, value) => tabStorage.set(key, value), removeItem: (key) => tabStorage.delete(key) },
    crypto: { randomUUID: () => `00000000-0000-4000-8000-${String(++operation).padStart(12, "0")}` },
    navigator: { clipboard: { writeText: async () => {} } },
    async fetch(url, options = {}) {
      assert.ok(url.startsWith("/repositories/fixture/api/"), url);
      const parsed = new URL(url, location.origin);
      const name = parsed.pathname.slice("/repositories/fixture".length);
      requests.push({ name, url, options });
      if (held.has(name)) await held.get(name);
      const failure = failures.get(name);
      if (name === "/api/workspace/branch/confirm") data[name].operation_id = JSON.parse(options.body).operation_id;
      if (failure instanceof Error) throw failure;
      if (!failure && !(name in data)) return { ok: false, status: 404, json: async () => ({ error: "Not found" }) };
      const body = typeof data[name] === "function" ? data[name](parsed) : data[name];
      if (body && body.__status) return { ok: false, status: body.__status, json: async () => ({ error: body.error }) };
      return { ok: !failure, status: failure ? failure.status || 503 : 200, json: async () => failure ? failure.body || { error: failure } : body };
    },
    addEventListener(type, listener) { (events[type] ||= []).push(listener); }, scrollTo() {},
  };
  context.window = context;
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(graphSource, context, { filename: "graph.js" });
  const realCreate = context.KinWorkspaceGraph.createMap;
  context.KinWorkspaceGraph.createMap = (host, model, options) => { const controller = realCreate(host, model, options); maps.push({ model, options, controller, disposed: false }); const dispose = controller.dispose; controller.dispose = () => { maps.at(-1).disposed = true; dispose(); }; return controller; };
  vm.runInContext(source, context, { filename: "workspace.js" });
  const api = context.KinWorkspace;
  return { document, requests, data, failures, expected, held, location, tabStorage, maps, api,
    node: (id) => document.getElementById(id), main: () => document.getElementById("workspace-main"), text: () => document.getElementById("workspace-main").textContent,
    links: () => document.getElementById("workspace-main").querySelectorAll("a"),
    buttons: () => document.getElementById("workspace-main").querySelectorAll("button"),
    button(label) { return this.buttons().find((node) => node.textContent === label); },
    navigate(route) { location.hash = `#/${route}`; for (const listener of events.hashchange || []) listener(); },
    refresh() { document.getElementById("workspace-refresh").click(); },
    count: (name) => requests.filter((request) => request.name === name).length,
    async preview() { this.navigate("branches"); this.button("Merge into main").click(); await flush(); },
    confirm() { const checkbox = this.node("workspace-merge-confirm"); checkbox.checked = true; checkbox.emit("change"); this.node("workspace-merge-submit").click(); },
  };
}

// ── Words ──────────────────────────────────────────────────────────────────

test("uncommitted work reads as the guided review's sentence of real numbers", () => {
  const { api } = fixture();
  const sentence = (summary, steps = [{ kind: "function" }]) => api.workSentence(guide(summary, { chapters: [{ steps }] }));
  assert.equal(sentence({ modified: 1, recorded_consumers: 16, recorded_tests: 2 }), "One function changed. 16 other places rely on it. None of them changed. Two tests reach it.");
  assert.equal(sentence({ modified: 3, recorded_consumers: 13, consumers_not_updated: 2, recorded_tests: 7 }, [{ kind: "function" }, { kind: "method" }]),
    "Three entities changed. 13 other places rely on them. Two were not updated. Seven tests reach them.");
  assert.equal(sentence({ added: 1 }), "One function changed. Nothing outside the change relies on it. No test reaches it.");
  assert.equal(sentence({ modified: 2, recorded_consumers: 2, consumers_not_updated: 2 }), "Two functions changed. Two other places rely on them. Neither was updated. No test reaches them.");
  assert.equal(sentence({}), null);
  // The guide's verdicts, when it gives them: what the change asks of the places that rely on it.
  assert.equal(sentence({ modified: 1, recorded_consumers: 16, recorded_tests: 2, consumers_need_no_update: 16, consumers_may_need_review: 0 }), "One function changed. 16 other places rely on it. None need an update. Two tests reach it.");
  assert.equal(sentence({ modified: 1, recorded_consumers: 16, recorded_tests: 2, consumers_need_no_update: 13, consumers_may_need_review: 3 }), "One function changed. 16 other places rely on it. Check three of them. Two tests reach it.");
  assert.equal(sentence({ modified: 1, recorded_consumers: 3, consumers_not_updated: 3, consumers_need_no_update: 0, consumers_may_need_review: 0 }), "One function changed. Three other places rely on it. None were updated. No test reaches it.");
});

test("the repository sentence uses only the counts a read returned", () => {
  const { api } = fixture();
  assert.equal(api.repositorySentence({ entities: 216, relations: 602, containers: 15, containerWord: "module", containerPlural: "modules" }), "Kin keeps track of how this code fits together: 216 entities in 15 modules, 602 connections.");
  assert.equal(api.repositorySentence({ entities: 1, relations: 0 }), "Kin keeps track of how this code fits together: 1 entity.");
  assert.equal(api.repositorySentence({}), "");
  assert.match(api.repositorySentence({ entities: 40, relations: 90, latest: new Date(Date.now() - 3 * 86_400_000).toISOString() }), /^Kin keeps track of how this code fits together: 40 entities, 90 connections\. The latest change was 3 days ago\.$/);
});

test("a recorded Git signature reads as a person's name", () => {
  const { api } = fixture();
  assert.equal(api.personName("David Lord <davidism@gmail.com> 1749933342 -0700"), "David Lord");
  assert.equal(api.personName("David Lord"), "David Lord");
  assert.equal(api.personName("<bot@example.invalid>"), "bot");
  assert.equal(api.personName("dependabot[bot]"), "dependabot[bot]");
  assert.equal(api.personName({ name: "Dana", email: "dana@example.invalid" }), "Dana");
  assert.equal(api.personName(""), "");
});

test("routes keep their old addresses working", () => {
  const { api } = fixture();
  const id = uuid(7);
  assert.deepEqual({ ...api.parseRoute("#/overview") }, { name: "home" });
  assert.deepEqual({ ...api.parseRoute(`#/entities?entity=${id}`) }, { name: "entity", id });
  assert.deepEqual({ ...api.parseRoute("#/entities") }, { name: "explore", q: "" });
  assert.deepEqual({ ...api.parseRoute("#/review") }, { name: "changes" });
  assert.deepEqual({ ...api.parseRoute("#/repository") }, { name: "branches" });
  assert.deepEqual({ ...api.parseRoute(`#/entity/${id}`) }, { name: "entity", id });
  assert.deepEqual({ ...api.parseRoute("#/entity/not-an-id") }, { name: "explore", q: "" });
  assert.deepEqual({ ...api.parseRoute("#/explore?q=verify%20a%20signature") }, { name: "explore", q: "verify a signature" });
  assert.deepEqual({ ...api.parseRoute("#/nowhere") }, { name: "home" });
});

test("an entity body keeps the indentation it had in place", () => {
  const { api } = fixture();
  assert.deepEqual([...api.bodyLines("def sign(self):\n        \"\"\"Doc.\"\"\"\n        return 1\n")], ["def sign(self):", "    \"\"\"Doc.\"\"\"", "    return 1"]);
  assert.deepEqual([...api.bodyLines("class A:\n    x = 1\n    def f(self):\n        pass")], ["class A:", "    x = 1", "    def f(self):", "        pass"]);
  assert.deepEqual([...api.bodyLines("one line")], ["one line"]);
});

// ── Home ───────────────────────────────────────────────────────────────────

test("home leads with the repository's numbers, the map, and uncommitted work into the guided review", async () => {
  const browser = fixture(); await flush();
  assert.match(browser.text(), /Kin keeps track of how this code fits together: 216 entities in 15 modules, 602 connections\./);
  assert.match(browser.text(), /One function changed\. 16 other places rely on it\. None of them changed\. Two tests reach it\./);
  // Uncommitted work comes before the map, so the first thing to act on is first.
  assert.ok(browser.text().indexOf("Uncommitted work") < browser.text().indexOf("Repository map"));
  const primary = browser.links().filter((node) => node.className.includes("button-primary"));
  assert.equal(primary.length, 1, "one primary action");
  assert.equal(primary[0].getAttribute("href"), "/repositories/fixture/review/#/w");
  assert.equal(browser.count("/api/workspace/graph"), 0, "the graph sample is not read when the overview and map are offered");
  assert.equal(browser.maps.length, 1);
  assert.deepEqual(browser.maps[0].options.labelIds, [uuid(2), uuid(3)], "the most relied-on entities are the ones named on the map");
  assert.match(browser.text(), /Showing the 2 most connected of 216 entities, grouped by the module that holds them/);
  assert.match(browser.text(), /want_bytes.*Function in encoding.*23rely on it/s);
  assert.equal(browser.maps[0].options.describe({ id: uuid(3), kind: "class", weight: 34 }), "Class · 22 rely on it", "the map says what the tile says");
  assert.match(browser.text(), /Preserved change.*Dana Reader/s);
  assert.deepEqual([...browser.maps[0].options.markIds], [uuid(2)], "uncommitted work is ringed on the map");
  assert.equal(browser.maps[0].options.describe({ id: uuid(2), kind: "function", weight: 20 }), "Function · 23 rely on it · Changed in your uncommitted work");
  // Home's recent changes lead with changes that changed an entity.
  const quiet = fixture();
  quiet.data["/api/changes"] = { changes: [{ id: "q-1", title: "Merge branch 'stable'", author: "Dana Reader", timestamp: "2026-01-03T00:00:00Z", parents: 2 }, ...quiet.data["/api/changes"].changes], truncated: false };
  quiet.data["/api/counts"] = { counts: { ...quiet.data["/api/counts"].counts, "q-1": { entities: 0, module_containers: 0, containers_through_members: 0, merge: true, merge_counted: true } } };
  await flush();
  const recent = quiet.node("home-changes").textContent;
  assert.doesNotMatch(recent, /Merge branch 'stable'/, "a merge that brought nothing new waits on the Changes page");
  assert.match(recent, /Preserved change/);
  const empty = fixture();
  empty.data["/api/counts"] = { counts: Object.fromEntries(Object.keys(empty.data["/api/counts"].counts).map((id) => [id, { entities: 0, module_containers: 0, containers_through_members: 0 }])) };
  await flush();
  assert.match(empty.node("home-changes").textContent, /Preserved change.*No entity changed/s, "with nothing that changed an entity, the latest change still shows");
  // Kin's own update leads neither the headline nor the recent changes.
  assert.doesNotMatch(browser.text(), /Update Kin's understanding/);
  assert.match(browser.text(), /The latest change was .*ago\./);
  assert.doesNotMatch(browser.text(), /<dana@|1749933342/);
  assert.doesNotMatch(browser.text(), /\.py|src\//, "no locations");
});

test("an older server without the overview and map falls back to the graph sample", async () => {
  const browser = fixture({ newServer: false }); await flush();
  assert.equal(browser.count("/api/workspace/overview"), 1);
  assert.equal(browser.count("/api/workspace/graph"), 1);
  assert.match(browser.text(), /40 entities, 90 connections\./);
  assert.match(browser.text(), /want_bytes/);
  browser.refresh(); await flush();
  assert.equal(browser.count("/api/workspace/overview"), 1, "a missing endpoint is not asked again");
  assert.equal(browser.count("/api/workspace/graph"), 2);
});

test("with no uncommitted work the primary action opens the latest change", async () => {
  const browser = fixture(); browser.data["/api/guide"] = guide(); browser.refresh(); await flush();
  const browser2 = fixture();
  browser2.data["/api/guide"] = guide();
  await flush(); browser2.refresh(); await flush();
  assert.match(browser2.text(), /No uncommitted work\. Your code matches your last commit\./);
  const primary = browser2.links().filter((node) => node.className.includes("button-primary"));
  assert.equal(primary.length, 1);
  assert.equal(primary[0].getAttribute("href"), "/repositories/fixture/review/#/c/change-1");
});

test("a failed read says so in its own section with a retry, and the rest of the page keeps working", async () => {
  const browser = fixture();
  browser.failures.set("/api/guide", "Working read failed");
  await flush();
  assert.match(browser.text(), /Kin couldn't load your uncommitted work\./);
  assert.match(browser.text(), /Working read failed/, "Kin's own words are kept behind Show details");
  assert.equal(browser.main().querySelectorAll(".read-failed details").length, 1);
  assert.match(browser.text(), /216 entities/);
  assert.match(browser.text(), /Preserved change/);
  assert.equal(browser.node("workspace-status").dataset.state, "stale");
  browser.failures.delete("/api/guide");
  browser.button("Try again").click(); await flush();
  assert.doesNotMatch(browser.text(), /Kin couldn't load your uncommitted work/);
  assert.match(browser.text(), /One function changed/);
  assert.equal(browser.node("workspace-status").dataset.state, "ready");
  assert.match(browser.node("workspace-status-text").textContent, /^Up to date /);
});

test("initial session failure has a real retry that recovers the page", async () => {
  const browser = fixture({ initialFailure: true }); await flush();
  assert.match(browser.text(), /This repository isn't answering\./);
  browser.failures.clear(); browser.button("Try again").click(); await flush();
  assert.match(browser.text(), /One function changed/);
});

test("concurrent refresh clicks share one read cycle", async () => {
  const browser = fixture(); await flush();
  let resolve; browser.held.set("/api/guide", new Promise((done) => { resolve = done; }));
  const before = browser.requests.length;
  browser.refresh(); browser.refresh();
  assert.equal(browser.requests.length - before, 9);
  assert.equal(browser.node("workspace-refresh").disabled, true);
  resolve(); await flush();
  assert.equal(browser.node("workspace-refresh").disabled, false);
});

test("a map is drawn once per read and disposed when the view changes", async () => {
  const browser = fixture(); await flush();
  assert.equal(browser.maps.length, 1);
  browser.refresh(); await flush();
  assert.equal(browser.maps.length, 1, "an unchanged read does not redraw the map");
  browser.navigate("changes");
  assert.equal(browser.maps[0].disposed, true);
});

test("the repository reads as its remote's owner and name, and a repository without one says Local", async () => {
  const browser = fixture();
  await flush();
  assert.equal(browser.node("repository-name").textContent, "pallets / Fixture");
  assert.match(browser.main().querySelector("h1").textContent, /^pallets\/Fixture$/);
  assert.equal(browser.document.querySelectorAll(".pill-local").length, 0);
  assert.equal(browser.document.title, "Home · pallets/Fixture");

  const local = fixture({ origin: {} });
  await flush();
  assert.equal(local.node("repository-name").textContent, "Fixture");
  assert.equal(local.document.querySelectorAll(".pill-local").length, 2, "the sidebar and the title both say Local");

  const elsewhere = fixture({ origin: { owner: "group/sub", host: "gitlab.com" } });
  await flush();
  assert.match(elsewhere.node("repository-branch").textContent, /gitlab\.com/, "a host other than GitHub is named");
  assert.doesNotMatch(browser.node("repository-branch").textContent, /github\.com/, "GitHub is not");
  assert.equal(browser.node("repository-branch").textContent, "main", "nothing but the branch when there is nothing else to say");

  const picker = fixture({ repositories: [
    { key: "fixture", name: "Fixture", owner: "pallets", host: "github.com", selected: true, url: "/repositories/fixture/workspace/" },
    { key: "other", name: "notes", url: "/repositories/other/workspace/" },
  ] });
  await flush();
  const options = picker.node("workspace-repository-picker").querySelectorAll("option").map((option) => option.textContent);
  assert.deepEqual([...options], ["pallets / Fixture", "notes"]);
});

// ── Explore ────────────────────────────────────────────────────────────────

test("explore asks by meaning and by name and shows entity rows with kind, container and signature", async () => {
  const browser = fixture({ hash: "#/explore" }); await flush();
  assert.match(browser.node("explore-input").getAttribute("placeholder"), /Ask about this code, like “want_bytes”/);
  assert.match(browser.text(), /Most relied on/);
  browser.navigate("explore?q=verify%20a%20signature"); await flush();
  const ask = browser.requests.find((request) => request.name === "/api/workspace/ask");
  assert.match(ask.url, /q=verify%20a%20signature/);
  assert.match(browser.text(), /Best matches/);
  assert.match(browser.text(), /Signer\.verify_signatureMethod/);
  assert.match(browser.text(), /def verify_signature\(self, value, sig\) -> bool/);
  assert.match(browser.text(), /want_bytesFunctionin encoding/);
  const rows = browser.main().querySelectorAll(".entity-row");
  assert.equal(rows[0].getAttribute("href"), `#/entity/${uuid(5)}`);
  const input = browser.node("explore-input");
  input.emit("keydown", { key: "ArrowDown" });
  assert.equal(rows[0].getAttribute("aria-selected"), "true");
  input.emit("keydown", { key: "Enter" });
  assert.equal(browser.location.hash, `#/entity/${uuid(5)}`);
});

test("explore falls back to name search when the server cannot ask by meaning", async () => {
  const browser = fixture({ hash: "#/explore", newServer: false }); await flush();
  assert.match(browser.text(), /Find an entity by name/);
  browser.navigate("explore?q=want"); await flush();
  assert.equal(browser.count("/api/workspace/ask"), 0);
  assert.ok(browser.requests.some((request) => request.name === "/api/workspace/graph" && /query=want/.test(request.url)));
  assert.match(browser.text(), /By name/);
  assert.match(browser.text(), /want_bytes/);
});

test("ask answers group into one ranked list and keep a notice only when a search could not answer", () => {
  const { api } = fixture();
  const answer = api.askGroups({ schema: "kin.workspace.ask.v1", mode: "name_only", notice: "Meaning search is off.", results: [{ id: uuid(1), name: "a", kind: "function" }, { id: "nope", name: "b" }], total: 1 }, "a");
  assert.equal(answer.groups.length, 1);
  assert.equal(answer.groups[0].title, "By name");
  assert.equal(answer.groups[0].rows.length, 1);
  assert.equal(answer.notice, "Meaning search is off.");
  assert.equal(api.askGroups({ schema: "kin.workspace.ask.v1", mode: "meaning_and_name", notice: "Ran without embeddings.", results: [] }, "a").notice, "");
});

// ── Entity ─────────────────────────────────────────────────────────────────

function entityV2(id, overrides = {}) {
  return {
    schema: "kin.workspace.entity.v2", id,
    header: { ok: true, name: "Signer.sign", kind: "method", language: "python", signature: "def sign(self, value)", summary: "Signs the ``value``.", container: { id: uuid(3), name: "Signer", kind: "class" } },
    source: { ok: true, text: "def sign(self, value):\n        return value", start_line: 40, language: "python" },
    relied_on_by: { ok: true, items: [{ id: uuid(10), name: "Serializer.dumps", kind: "method", relationship: "calls", is_test: false }, { id: uuid(11), name: "TestSigner.test_sign", kind: "method", relationship: "calls", is_test: true }], total: 2, shown: 2, bound: "exact" },
    relies_on: { ok: true, items: [{ id: uuid(2), name: "want_bytes", kind: "function", relationship: "calls" }], total: 1, shown: 1 },
    members: { ok: true, items: [], total: 0, shown: 0 },
    tests: { ok: true, items: [{ id: uuid(11), name: "TestSigner.test_sign", kind: "method", hops: 1 }, { id: uuid(12), name: "TestSerializer.test_dumps", kind: "method", hops: 2 }], total: 2, shown: 2, bound: "at_least" },
    impact: { ok: true, direct: 2, transitive: 5, tests: 2, depth: 3, top: [{ id: uuid(13), name: "Serializer.dump", kind: "method", hops: 2, reason: "runtime_caller", is_test: false }], bound: "at_least", notice: "Some call sites could not be proven on this machine, so the counts are a lower bound." },
    history: { ok: true, items: [{ change_id: "abc123", title: "Bound input", author: "Dana Reader", time: "2026-01-01T00:00:00Z" }], total: 1, shown: 1 },
    editor: { ok: false, reason: "This entity has no verified current source to open." },
    ...overrides,
  };
}

test("an entity page draws each part of the sectioned read and links into the guided review", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id);
  await flush();
  const text = browser.text();
  assert.match(text, /Signer\.sign/);
  assert.match(text, /Signs the value\./);
  assert.match(text, /2 rely on it/, "a test that relies on it is counted like any other entity");
  assert.match(text, /2 tests reach it/);
  assert.match(text, /A change here would reach 5 entities within 3 steps\. Two tests reach it\./);
  assert.doesNotMatch(text, /directly/, "no second number for what relies on it");
  assert.match(text, /Serializer\.dump2 steps away/);
  assert.match(text, /This entity has no verified current source to open\./);
  assert.equal(browser.main().querySelectorAll(".code-no")[0].textContent, "40");
  const history = browser.links().find((node) => node.textContent.includes("Bound input"));
  assert.equal(history.getAttribute("href"), "/repositories/fixture/review/#/c/abc123");
  const focus = browser.maps.at(-1);
  assert.equal(focus.options.mode, "focus");
  assert.deepEqual([...focus.model.nodes.map((row) => row.role).sort()], ["focal", "in", "out", "test", "test"]);
  assert.equal(browser.node("nav-explore").getAttribute("aria-current"), "page");
});

test("one failed part of an entity read shows its own reason and retry while the rest draws", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  let calls = 0;
  browser.data["/api/workspace/entity"] = () => (++calls === 1
    ? entityV2(id, { history: { ok: false, reason: "Kin could not read this entity's history. It did not answer within 8 seconds." }, relied_on_by: { ok: false, reason: "Kin could not read what relies on this entity." } })
    : entityV2(id));
  await flush();
  assert.match(browser.text(), /Kin could not read this entity's history\./);
  assert.match(browser.text(), /Kin could not read what relies on this entity\./);
  assert.match(browser.text(), /def sign/, "source still draws");
  assert.match(browser.text(), /TestSigner\.test_sign/, "tests still draw");
  browser.button("Try again").click(); await flush();
  assert.doesNotMatch(browser.text(), /could not read/);
  assert.match(browser.text(), /Bound input/);
});

test("an older server's entity read still draws relationships with tests marked", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entities?entity=${id}`, newServer: false });
  browser.data["/api/workspace/entity"] = () => ({ schema: "kin.workspace.entity.v1", scope: "workspace", id,
    source: { id, name: "Signer.sign", kind: "Method", body: "def sign(self):\n        return 1", language: "python", semantic_owners: [{ name: "Signer" }] },
    neighborhood: { focal_id: id, truncated: false, entities: [{ id, name: "Signer.sign", kind: "Method" }, { id: uuid(10), name: "Serializer.dumps", kind: "Method" }, { id: uuid(11), name: "TestSigner.test_sign", kind: "Method" }],
      relations: [{ src: { Entity: uuid(10) }, dst: { Entity: id }, kind: "Calls" }, { src: { Entity: uuid(11) }, dst: { Entity: id }, kind: "Calls" }] },
    editor_uri: null, editor_unavailable: "No verified source." });
  await flush();
  assert.match(browser.text(), /2 rely on it/);
  assert.match(browser.text(), /1 test reaches it/);
  assert.match(browser.text(), /This version of Kin does not list an entity's history here\./);
  assert.match(browser.text(), /TestSigner\.test_signTest/);
});

// ── Changes ────────────────────────────────────────────────────────────────

test("changes list names, dates and entity counts, and shows decisions only when one exists", async () => {
  const browser = fixture({ hash: "#/changes" }); await flush();
  assert.match(browser.text(), /Three changes\. One approved\./);
  assert.match(browser.text(), /Preserved changeDana Reader.*2 entities changed.*Approved/s);
  // A merge reads as its own review counts it, never as the history's raw delta count.
  assert.match(browser.text(), /Merge branchDana Reader.*4 entities changed.*To review/s);
  assert.doesNotMatch(browser.text(), /7 entities changed/);
  assert.match(browser.text(), /Update Kin's understanding of this code for Kin 0\.8\.2.*Kin update/s);
  assert.equal(browser.main().querySelectorAll(".change-row-quiet").length, 1, "Kin's own update is dimmed");
  browser.data["/api/standing"] = { standing: {} }; browser.refresh(); await flush();
  assert.equal(browser.main().querySelectorAll(".decision-none").length, 0, "an empty decision column is left out");
  assert.match(browser.text(), /None reviewed yet\./);
});

test("range references and keyboard focus survive a workspace refresh", async () => {
  const browser = fixture({ hash: "#/changes" }); await flush();
  browser.node("changes-compare-toggle").click();
  const base = browser.node("workspace-range-base"); base.value = "release-base"; base.emit("input"); base.focus();
  const head = browser.node("workspace-range-head"); head.value = "feature"; head.emit("input");
  browser.refresh(); await flush();
  assert.equal(browser.node("workspace-range-base").value, "release-base");
  assert.equal(browser.node("workspace-range-head").value, "feature");
  assert.equal(browser.document.activeElement.id, "workspace-range-base");
  browser.document.querySelector(".workspace-compare").emit("submit");
  assert.equal(browser.location.assigned, "/repositories/fixture/review/#/r/release-base..feature");
});

// ── Merges ─────────────────────────────────────────────────────────────────

test("a preview-only session shows heads and notes without confirmation controls", async () => {
  const browser = fixture({ merge: false }); await flush(); await browser.preview();
  assert.match(browser.text(), /Publication checks run on confirmation/);
  assert.match(browser.text(), /Confirmation requires explicit authorization/);
  assert.equal(browser.node("workspace-merge-submit"), null);
  assert.equal(browser.requests.filter((request) => request.name.endsWith("/confirm")).length, 0);
});

test("Rust wire merge identities display losslessly and link to the recorded hexadecimal change", async () => {
  const browser = fixture(); await flush();
  const original = JSON.stringify(browser.expected);
  await browser.preview();
  assert.deepEqual(JSON.parse(browser.requests.find((request) => request.name.endsWith("/merge/preview")).options.body), { source: "feature" });
  assert.match(browser.text(), /Branchrefs\/heads\/feature/, "the exact names stay behind Show details");
  assert.match(browser.text(), /I've reviewed what this merge brings into main\./);
  for (const byte of [0, 1, 2]) assert.ok(browser.text().includes(byte.toString(16).padStart(2, "0").repeat(32)));
  assert.doesNotMatch(browser.text(), /\[object Object\]|1,1,1,1/);
  browser.confirm(); await flush();
  const call = browser.requests.find((request) => request.name.endsWith("/confirm"));
  assert.deepEqual(JSON.parse(call.options.body).expected, JSON.parse(original));
  assert.equal(JSON.stringify(browser.expected), original, "display must not mutate the returned expected bundle");
  assert.match(browser.text(), /Kin merged feature into main\./);
  const link = browser.links().find((node) => node.textContent === "Review the merge");
  assert.equal(link.getAttribute("href"), `/repositories/fixture/review/#/c/${"04".repeat(32)}`);
});

test("malformed merge reference and hash wire shapes cannot produce confirmation controls", async () => {
  for (const invalid of [
    { source_ref: "feature" }, { source_ref: { bytes_hex: "ABCDEF" } },
    { target_ref: { ...refWire("main"), unexpected: true } },
    { ours_change: Array(31).fill(1) }, { theirs_change: [...Array(31).fill(2), true] },
    { ours_change: [...Array(31).fill(1), 256] },
  ]) {
    const browser = fixture(); await flush();
    browser.data["/api/workspace/merge/preview"].expected = { ...browser.expected, ...invalid };
    await browser.preview();
    assert.match(browser.text(), /incomplete merge preview/);
    assert.equal(browser.node("workspace-merge-submit"), null);
    assert.equal(browser.requests.filter((request) => request.name.endsWith("/confirm")).length, 0);
  }
});

test("an unreadable recorded merge identity is disclosed without constructing a review URL", async () => {
  const browser = fixture(); await flush(); await browser.preview();
  browser.data["/api/workspace/merge/confirm"].report.merge_change = Array(31).fill(4);
  browser.confirm(); await flush();
  assert.match(browser.text(), /Kin couldn't read which change the merge made\./);
  assert.equal(browser.links().some((node) => node.textContent === "Review the merge"), false);
});

test("confirmation carries the exact preview and never retries an unknown result", async () => {
  const browser = fixture(); await flush(); await browser.preview();
  browser.failures.set("/api/workspace/merge/confirm", new Error("Connection closed after request"));
  browser.confirm(); await flush();
  assert.match(browser.text(), /Merge outcome is unknown/);
  const calls = browser.requests.filter((request) => request.name.endsWith("/confirm"));
  assert.equal(calls.length, 1);
  assert.deepEqual(JSON.parse(calls[0].options.body).expected, browser.expected);
  assert.equal(calls[0].options.headers["x-kin-review-token"], "synthetic-token");
  assert.match(JSON.parse(calls[0].options.body).operation_id, /^00000000/);
  browser.refresh(); await flush(); browser.navigate("home"); browser.navigate("branches");
  assert.equal(browser.requests.filter((request) => request.name.endsWith("/confirm")).length, 1);
  assert.equal(browser.node("workspace-merge-source").disabled, true);
});

test("a typed stale refusal clears the old preview and requires a new explicit preview and confirmation", async () => {
  const browser = fixture(); await flush(); await browser.preview();
  browser.failures.set("/api/workspace/merge/confirm", { status: 409, body: { outcome: "refused", error: "The target head moved before the write.", remedy: "Read a new preview of the current heads." } });
  browser.confirm(); await flush();
  assert.match(browser.text(), /target head moved before the write/);
  assert.match(browser.text(), /Read a new preview of the current heads/);
  assert.doesNotMatch(browser.text(), /Merge outcome is unknown/);
  assert.equal(browser.node("workspace-merge-submit"), null);
  assert.equal(browser.node("workspace-merge-source").disabled, false);
  assert.equal(browser.tabStorage.size, 0, "definite pre-write refusal clears the pending receipt");
  assert.equal(browser.requests.filter((request) => request.name.endsWith("/confirm")).length, 1);
  browser.failures.delete("/api/workspace/merge/confirm");
  browser.data["/api/workspace/merge/preview"] = { ...browser.data["/api/workspace/merge/preview"], expected: { ...browser.expected, ours_change: hashWire(5) } };
  browser.document.querySelector(".workspace-merge-form").emit("submit"); await flush();
  assert.equal(browser.node("workspace-merge-submit").disabled, true, "fresh evidence still requires explicit acknowledgment");
  assert.equal(browser.requests.filter((request) => request.name.endsWith("/confirm")).length, 1);
  browser.confirm(); await flush();
  const calls = browser.requests.filter((request) => request.name.endsWith("/confirm"));
  assert.equal(calls.length, 2);
  assert.deepEqual(JSON.parse(calls[1].options.body).expected.ours_change, hashWire(5));
  assert.notEqual(JSON.parse(calls[0].options.body).operation_id, JSON.parse(calls[1].options.body).operation_id);
});

test("an untyped HTTP error cannot establish a pre-write refusal and retains the server operation identity", async () => {
  const browser = fixture(); await flush(); await browser.preview();
  browser.failures.set("/api/workspace/merge/confirm", { status: 409, body: { error: "No definite outcome returned.", operation_id: "server-reported-operation" } });
  browser.confirm(); await flush();
  assert.match(browser.text(), /Merge outcome is unknown/);
  assert.match(browser.text(), /server-reported-operation/);
  assert.equal(browser.node("workspace-merge-source").disabled, true);
});

test("reloading a tab with an in-flight confirmation restores the receipt without replaying a request", async () => {
  const tabStorage = new Map();
  const browser = fixture({ tabStorage }); await flush(); await browser.preview();
  let resolve; browser.held.set("/api/workspace/merge/confirm", new Promise((done) => { resolve = done; }));
  browser.confirm();
  assert.equal(tabStorage.size, 1, "receipt is persisted before confirmation is dispatched");
  assert.doesNotMatch([...tabStorage.values()].join(""), /synthetic-token|workspace_tree_hash/);
  const reloaded = fixture({ tabStorage }); await flush(); reloaded.navigate("branches");
  assert.match(reloaded.text(), /earlier merge request/);
  assert.match(reloaded.text(), /Merge outcome is unknown/);
  assert.equal(reloaded.node("workspace-merge-source").disabled, true);
  assert.equal(reloaded.requests.filter((request) => request.name.endsWith("/confirm")).length, 0);
  resolve(); await flush();
});

test("unknown confirmation survives a reload when runtime authority observation disappears or returns", async () => {
  for (const observedIdentity of [true, false]) {
    const tabStorage = new Map();
    const browser = fixture({ tabStorage, observedIdentity }); await flush(); await browser.preview();
    browser.failures.set("/api/workspace/merge/confirm", new Error("No confirmation response observed"));
    browser.confirm(); await flush();
    const receipt = JSON.parse([...tabStorage.values()][0]);
    assert.equal(receipt.repositoryId, browser.expected.repository_id, "receipt keeps the displayed authority even if session observation was unavailable");
    assert.equal(receipt.workspaceId, browser.expected.workspace_id);
    const reloaded = fixture({ tabStorage, observedIdentity: !observedIdentity }); await flush(); reloaded.navigate("branches");
    assert.match(reloaded.text(), /Merge outcome is unknown/);
    assert.match(reloaded.text(), new RegExp(receipt.operationId));
    assert.equal(reloaded.node("workspace-merge-source").disabled, true);
    assert.equal(reloaded.requests.filter((request) => request.name.endsWith("/confirm")).length, 0);
  }
});

test("a conflicted confirmation is shown as a conflict record rather than a successful merge", async () => {
  const browser = fixture(); await flush();
  browser.data["/api/workspace/merge/preview"].outcome = "conflicted";
  browser.data["/api/workspace/merge/confirm"].report.outcome = "conflicted";
  delete browser.data["/api/workspace/merge/confirm"].report.merge_change;
  await browser.preview(); assert.equal(browser.node("workspace-merge-submit").textContent, "Create conflict record");
  browser.confirm(); await flush();
  assert.match(browser.text(), /Conflicts saved to resolve/);
  assert.match(browser.text(), /The branches are not merged yet/);
});

// ── Branches ───────────────────────────────────────────────────────────────

async function branchPreview(browser) {
  browser.navigate("branches");
  browser.button("Switch").click();
  await flush();
}
function branchConfirm(browser) {
  const checkbox = browser.node("workspace-branch-confirm");
  checkbox.checked = true; checkbox.emit("change");
  browser.node("workspace-branch-submit").click();
}

test("the branch list opens a switch or a merge only when asked", async () => {
  const browser = fixture({ hash: "#/branches" }); await flush();
  assert.match(browser.text(), /You're on main\. One other branch: feature\./);
  assert.doesNotMatch(browser.text(), /You are here/);
  assert.equal(browser.links().filter((node) => node.textContent === "Review")[0].className, "button button-small button-primary", "Review is the one primary action on a branch");
  assert.equal(browser.node("workspace-branch-target"), null);
  assert.equal(browser.node("workspace-merge-source"), null);
  assert.match(browser.text(), /No merge is waiting to be resolved\./);
});

test("each other branch says what it holds beyond the current one and opens its review", async () => {
  const browser = fixture({ hash: "#/branches" });
  browser.data["/api/guide"] = (url) => url.searchParams.get("base") === "main" && url.searchParams.get("change") === "feature"
    ? guide({ modified: 1, recorded_consumers: 3, recorded_tests: 2 }, { chapters: [{ steps: [{ kind: "function" }] }], range: { base_ref: "main", head_ref: "feature", changes_total: 1, changes: [{ id: "c9", title: "Bound base64 input before decoding", author: "Dana Reader", timestamp: new Date(Date.now() - 600_000).toISOString() }] } })
    : guide();
  await flush(); await flush();
  assert.match(browser.text(), /featureOne function changed\. Three other places rely on it\. None of them changed\. Two tests reach it\./);
  assert.match(browser.text(), /1 change ahead of main · Bound base64 input before decoding · Dana Reader, 10 minutes ago/);
  const review = browser.links().find((node) => node.textContent === "Review");
  assert.equal(review.getAttribute("href"), "/repositories/fixture/review/#/r/main..feature");
  assert.equal(browser.requests.filter((request) => /base=main/.test(request.url)).length, 1, "read once");
});

/** A sectioned entity read: answers only the sections asked for, as the server does. */
function sectioned(full) {
  return (url) => {
    const asked = (url.searchParams.get("sections") || "").split(",").filter(Boolean);
    if (!asked.length) return full;
    const answer = { schema: full.schema, id: full.id, sections: asked };
    for (const key of asked) answer[key] = full[key];
    return answer;
  };
}

test("an entity reads its quick parts first and draws what relies on it and its history when they land", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  const full = entityV2(id, {
    relied_on_by: { ok: true, total: 3, confirmed: 2, shown: 3, bound: "at_least",
      notice: "Some call sites could not be proven on this machine, so there may be more. 1 of them matches by name alone, and Kin could not prove it uses this one.",
      items: [
        { id: uuid(20), name: "Serializer.__init__", kind: "method", relationship: "references", is_test: false, confirmed: true, sites: 14, declarations: 6 },
        { id: uuid(21), name: "Serializer.dumps", kind: "method", relationship: "calls", is_test: false, confirmed: false },
        { id: uuid(11), name: "TestSigner.test_sign", kind: "method", relationship: "calls", is_test: true, confirmed: true },
      ] },
  });
  const answer = sectioned(full);
  let release;
  const slow = new Promise((done) => { release = done; });
  browser.data["/api/workspace/entity"] = (url) => /relied_on_by/.test(url.searchParams.get("sections") || "") ? slow.then(() => answer(url)) : answer(url);
  await flush();
  const reads = () => browser.requests.filter((request) => request.name === "/api/workspace/entity");
  assert.match(reads()[0].url, /sections=header,source,relies_on,members,impact,tests,editor/);
  assert.match(reads()[1].url, /sections=relied_on_by,history/);
  assert.match(browser.text(), /def sign/, "the source draws before the slow parts");
  assert.equal(browser.main().querySelectorAll(".rel-group[aria-busy=true]").length, 1, "its history waits with a placeholder");
  assert.equal(browser.main().querySelectorAll(".reliers[aria-busy=true]").length, 1, "what relies on it waits with a placeholder");
  release(); await flush();
  assert.equal(browser.main().querySelectorAll(".rel-group[aria-busy=true]").length, 0);
  assert.match(browser.text(), /Serializer\.__init__6 overloads/);
  assert.doesNotMatch(browser.text(), /more not listed/, "six declarations count as six entities");
  assert.match(browser.text(), /Serializer\.dumpsby name only/);
  assert.equal(browser.main().querySelectorAll(".rel-row-unproven").length, 1);
  assert.match(browser.text(), /Kin found these by name and couldn't confirm they use it\./);
  assert.match(browser.text(), /7 rely on it/, "each declaration and each test that relies on it counts once");
  assert.match(browser.text(), /Bound input/);
});

test("a server that does not read sections answers the whole entity at once", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = (url) => url.searchParams.has("sections") ? { __status: 400, error: "unknown field `sections`" } : entityV2(id);
  await flush();
  const reads = browser.requests.filter((request) => request.name === "/api/workspace/entity");
  assert.equal(reads.length, 2);
  assert.doesNotMatch(reads[1].url, /sections/);
  assert.match(browser.text(), /Signer\.sign/);
  assert.match(browser.text(), /Bound input/);
  assert.doesNotMatch(browser.text(), /Couldn't read this entity/);
});

test("the entity header leaves out what it would repeat or what is empty", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, { relies_on: { ok: true, items: [], total: 0, shown: 0 } });
  await flush();
  assert.doesNotMatch(browser.text(), /relies on 0/);
  assert.equal(browser.main().querySelectorAll(".entity-signature-block").length, 0, "the source's first line already shows the signature");
  assert.match(browser.text(), /It relies on nothing else in this code\./);
  const other = fixture({ hash: `#/entity/${id}` });
  other.data["/api/workspace/entity"] = () => entityV2(id, { source: { ok: false, reason: "Kin could not read this entity's text." } });
  await flush();
  assert.equal(other.main().querySelectorAll(".entity-signature-block").length, 1, "without the source the signature is shown");
});

test("Switch is offered before the branch names are read and opens the panel to wait for them", async () => {
  const browser = fixture({ hash: "#/branches" });
  let release; browser.held.set("/api/workspace/branches", new Promise((done) => { release = done; }));
  await flush();
  const offered = browser.button("Switch");
  assert.ok(offered, "the row offers a switch while the exact names are still being read");
  offered.click();
  assert.match(browser.text(), /Switch branch/);
  assert.equal(browser.node("workspace-branch-target"), null, "no choice is offered until the names are read");
  assert.equal(browser.requests.filter((row) => row.name === "/api/workspace/branch/preview").length, 0);
  release(); await flush();
  assert.ok(browser.node("workspace-branch-target"), "the choice appears once the names land");
});

test("branch switch sends the original byte identity and pinned expectation, then rereads dependent state", async () => {
  const browser = fixture(); await flush(); await branchPreview(browser);
  assert.match(browser.text(), /Compatible pending work is carried/);
  const previewCall = browser.requests.find((row) => row.name === "/api/workspace/branch/preview");
  assert.deepEqual(JSON.parse(previewCall.options.body), { name: refWire("feature") });
  const expected = structuredClone(browser.data["/api/workspace/branch/preview"].expected);
  const reads = browser.count("/api/guide");
  branchConfirm(browser); await flush();
  const calls = browser.requests.filter((row) => row.name === "/api/workspace/branch/confirm");
  assert.equal(calls.length, 1);
  assert.deepEqual(JSON.parse(calls[0].options.body).expected, expected);
  assert.match(browser.text(), /Switched to feature/);
  assert.equal(browser.count("/api/guide"), reads + 1);
  assert.equal(browser.tabStorage.size, 0);
});

test("branch stale refusal discards the preview and requires a new explicit confirmation", async () => {
  const browser = fixture(); await flush(); await branchPreview(browser);
  browser.failures.set("/api/workspace/branch/confirm", { status: 409, body: { outcome: "refused", error: "Workspace changed after preview" } });
  branchConfirm(browser); await flush();
  assert.match(browser.text(), /refused before changing the workspace/);
  assert.equal(browser.node("workspace-branch-submit"), null);
  assert.equal(browser.tabStorage.size, 0);
  browser.failures.delete("/api/workspace/branch/confirm");
  browser.document.querySelector(".workspace-branch-form").emit("submit"); await flush();
  assert.equal(browser.node("workspace-branch-submit").disabled, true);
  assert.equal(browser.requests.filter((row) => row.name === "/api/workspace/branch/confirm").length, 1);
});

test("unknown branch result survives reload and prevents both branch and merge confirmation", async () => {
  const tabStorage = new Map(); const browser = fixture({ tabStorage }); await flush(); await branchPreview(browser);
  browser.failures.set("/api/workspace/branch/confirm", new Error("Connection lost after send"));
  branchConfirm(browser); await flush();
  assert.match(browser.text(), /branch switch outcome is unknown/);
  assert.equal(browser.node("workspace-branch-target").disabled, true);
  assert.equal(browser.requests.filter((row) => row.name === "/api/workspace/branch/confirm").length, 1);
  const restored = fixture({ tabStorage }); await flush(); restored.navigate("branches");
  assert.match(restored.text(), /earlier branch switch/);
  assert.equal(restored.node("workspace-branch-target").disabled, true);
  assert.equal(restored.button("Merge into main").disabled, true);
  assert.equal(restored.node("workspace-merge-submit"), null);
  assert.equal(restored.requests.some((row) => row.name.endsWith("/confirm")), false);
});

test("a refresh requested during branch confirmation cannot strand the switched workspace with empty reads", async () => {
  const browser = fixture(); await flush(); await branchPreview(browser);
  let done; browser.held.set("/api/workspace/branch/confirm", new Promise((resolve) => { done = resolve; }));
  branchConfirm(browser);
  const before = browser.requests.length;
  browser.refresh();
  assert.equal(browser.requests.length, before, "refresh is disabled while confirmation is in flight");
  done(); await flush();
  assert.match(browser.text(), /Switched to feature/);
  assert.equal(browser.node("workspace-status").dataset.state, "ready");
  browser.navigate("changes");
  assert.match(browser.text(), /Preserved change/);
});

test("a branch confirmation cannot reuse a merge preview made before switching", async () => {
  const browser = fixture(); await flush(); await browser.preview();
  assert.ok(browser.node("workspace-merge-confirm"));
  await branchPreview(browser); branchConfirm(browser); await flush();
  browser.navigate("home"); browser.navigate("branches");
  assert.equal(browser.node("workspace-merge-confirm"), null);
  assert.equal(browser.requests.filter((row) => row.name === "/api/workspace/merge/confirm").length, 0);
});

// ── Copy ───────────────────────────────────────────────────────────────────

test("the page's own words avoid files, paths, internal terms and em dashes", () => {
  const files = ["workspace.js", "graph.js", "index.html"].map((name) => fs.readFileSync(path.join(assets, name), "utf8"));
  // Strings the page shows: quoted literals and template text, not identifiers or comments.
  const shown = files.join("\n").split("\n").filter((line) => !/^\s*(\/\/|\*|\/\*)/.test(line)).join("\n");
  const literals = [...shown.matchAll(/"([^"\\]*(?:\\.[^"\\]*)*)"|`([^`]*)`/g)].map((match) => (match[1] ?? match[2]).replace(/\$\{[^}]*\}/g, "")).filter((value) => /[A-Za-z]{3,} [A-Za-z]{2,}/.test(value));
  for (const value of literals) {
    assert.doesNotMatch(value, /\bfiles?\b|\bpaths?\b|inventory|reconcil|authority|evidence|call-site debt|\bscope\b|unverified/i, value);
    assert.doesNotMatch(value, /working tree|working copy|\bconsumers?\b|\brelations?\b|enrichment|\brecorded\b|Close in meaning|^Reading$|Read again/i, value);
    assert.doesNotMatch(value, /—/, value);
    assert.doesNotMatch(value, /\b(every|all) (entit|change|relationship|connection|test)/i, value);
  }
  for (const file of files) assert.doesNotMatch(file.replace(/\/\/.*$/gm, ""), /—/);
});

// ── One count for what relies on an entity ─────────────────────────────────

test("an entity says one number for what relies on it, the number Home shows", async () => {
  const id = uuid(30);
  const browser = fixture({ hash: `#/entity/${id}` });
  const relying = Array.from({ length: 20 }, (_, i) => ({ id: uuid(100 + i), name: `caller_${i}`, kind: "function", relationship: "calls", is_test: i >= 15, confirmed: true, relies: true }));
  browser.data["/api/workspace/entity"] = () => entityV2(id, {
    header: { ok: true, name: "Signer", kind: "class", dependents: 21, container: { id: uuid(4), name: "signer", kind: "module" } },
    relied_on_by: { ok: true, total: 22, confirmed: 22, relies_total: 21, shown: 22, bound: "exact",
      items: [{ id: uuid(140), name: "Serializer.__init__", kind: "method", relationship: "references", is_test: false, confirmed: true, relies: true, declarations: 1 }, ...relying,
        { id: uuid(141), name: "TimestampSigner", kind: "class", relationship: "extends", is_test: false, confirmed: true, relies: false }] },
    impact: { ok: true, direct: 22, transitive: 43, tests: 21, depth: 3, top: [], bound: "at_least", notice: "Kin may have missed some callers, so there could be more." },
  });
  await flush();
  const text = browser.text();
  assert.match(text, /21 rely on it/, "the chip says the ranking's number");
  assert.match(text, /Relies on it21/, "the tree's branch says the same number");
  assert.doesNotMatch(text, /22 rely on it|22 of them directly/, "no second number for the same thing");
  assert.match(text, /A change here would reach 43 entities within 3 steps\. 21 tests reach it\./);
  assert.match(text, /Builds on it1TimestampSignerextends it/, "a subclass is listed apart");
  // The tree shows six, then shows the rest in place.
  assert.match(text, /Show 15 more/);
  browser.main().querySelectorAll(".relier-more-button")[0].click(); await flush();
  assert.match(browser.text(), /caller_19/);
  assert.doesNotMatch(browser.text(), /Tests that reach it\d+[A-Za-z_]*Relies on it/, "what relies on it is listed once");
});

test("a class relies on what its members rely on", async () => {
  const id = uuid(31);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, {
    header: { ok: true, name: "Signer", kind: "class" },
    relies_on: { ok: true, through_members: true, total: 2, shown: 2, bound: "exact", items: [
      { id: uuid(2), name: "want_bytes", kind: "function", relationship: "calls", through: ["Signer.sign", "Signer.unsign"] },
      { id: uuid(5), name: "HMACAlgorithm", kind: "class", relationship: "references", through: ["Signer.__init__"] }] },
  });
  await flush();
  assert.match(browser.text(), /It and its members rely on2/);
  assert.match(browser.text(), /want_bytesthrough sign and 1 more/);
  assert.match(browser.text(), /relies on 2/);
});

test("an entity the current code does not hold says so plainly, with one way on", async () => {
  const id = uuid(32);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = { __status: 404, error: `No entity with id ${id} is in this repository's current graph.` };
  await flush();
  assert.match(browser.text(), /This entity isn't in the current code\.It may have been renamed or removed\./);
  assert.equal(browser.links().find((node) => node.textContent === "Search for it").getAttribute("href"), "#/explore");
  assert.equal(browser.node("entity-map-card").hidden, true, "empty sections are left out");
  assert.equal(browser.node("entity-impact-card").hidden, true);
  assert.equal(browser.node("entity-layout").hidden, true);
  assert.doesNotMatch(browser.text(), /^Entity|Source/);
});

test("explore folds overloads into one row and says only a name match", async () => {
  const browser = fixture({ hash: "#/explore?q=init" });
  const init = (n) => ({ id: uuid(60 + n), name: "Serializer.__init__", kind: "method", signature: `def __init__(self, a${n})`, container: { id: uuid(50), name: "Serializer", kind: "class" }, matched: ["meaning"] });
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [init(1), init(2), init(3), init(4), { id: uuid(70), name: "want_bytes", kind: "function", matched: ["name"], dependents: 23 }], total: 5 };
  await flush();
  const text = browser.text();
  assert.equal((text.match(/Serializer\.__init__/g) || []).length, 1);
  assert.match(text, /Serializer\.__init__· 4 overloads/);
  assert.doesNotMatch(text, /def __init__/, "a folded row shows no one signature");
  assert.doesNotMatch(text, /Close in meaning/);
  assert.match(text, /want_bytes.*23 rely on it/s);
  assert.match(text, /5 entities/, "the overloads a row folds are counted as shown");
});

test("a failure is one plain line, one action, and Kin's own words behind Show details", async () => {
  const browser = fixture({ hash: "#/branches" });
  browser.failures.set("/api/guide", { status: 502, body: { error: "Kin could not answer: It did not answer within 20 seconds." } });
  await flush();
  assert.match(browser.text(), /Kin is still reading this branch\./);
  assert.ok(browser.button("Try again"));
  assert.match(browser.text(), /Show details.*did not answer within 20 seconds/s);
});
