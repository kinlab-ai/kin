// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Run the workspace page against a small DOM, a controlled clock and synthetic
// responses shaped like the local server's. No repository, runtime, browser,
// or mutation endpoint is contacted. The frame is built from the real
// index.html, and the page code is the real graph.js, agents.js and workspace.js.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const test = require("node:test");
const assets = path.resolve(__dirname, "../crates/kin-cli/assets/workspace_ui");
const source = fs.readFileSync(path.join(assets, "workspace.js"), "utf8");
const graphSource = fs.readFileSync(path.join(assets, "graph.js"), "utf8");
const agentsSource = fs.readFileSync(path.join(assets, "agents.js"), "utf8");
const frameSource = fs.readFileSync(path.join(assets, "index.html"), "utf8");
const flush = async () => { for (let i = 0; i < 4; i++) await new Promise(setImmediate); };
const refWire = (name) => ({ bytes_hex: Buffer.from(`refs/heads/${name}`).toString("hex") });
const hashWire = (byte) => Array(32).fill(byte);
const uuid = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, "0")}`;
// A repository answers under the key the server derives for it: 64 hex digits.
const repositoryKey = (digit) => digit.repeat(64);
const KEY = repositoryKey("a");
const NOTES = repositoryKey("b");
const ARCHIVE = repositoryKey("c");
const BASE = `/repositories/${KEY}`;
// Where a change is read: its review, a page of this workspace.
const guided = (id) => `#/review/c/${id}`;

class Element {
  constructor(tag, document) {
    this.tagName = tag.toUpperCase(); this.attrs = {}; this.dataset = {}; this.value = ""; this.disabled = false; this.hidden = false;
    // A node's links into the rest of the page are kept out of what a failed assertion prints, so it prints the node and not the whole page.
    for (const [key, value] of [["ownerDocument", document], ["children", []], ["parentNode", null], ["events", {}]]) Object.defineProperty(this, key, { value, writable: true, enumerable: false });
    this.style = { setProperty(name, value) { this[name] = String(value); } };
    this.classList = { toggle: (name, on) => { const list = new Set(this.className.split(" ").filter(Boolean)); if (on ?? !list.has(name)) list.add(name); else list.delete(name); this.className = [...list].join(" "); } };
  }
  get id() { return this.attrs.id || ""; }
  get type() { return this.attrs.type || "text"; }
  get className() { return this.attrs.class || ""; }
  set className(value) { this.attrs.class = value; }
  get isConnected() { return this.ownerDocument.contains(this); }
  get textContent() { return this.text ?? this.children.map((child) => child.textContent).join(""); }
  set textContent(value) { this.children = []; this.text = String(value); }
  setAttribute(key, value) { this.attrs[key] = String(value); if (key === "value") this.value = String(value); if (key === "disabled") this.disabled = true; if (key === "hidden") this.hidden = true; if (key === "checked") this.checked = true; if (key.startsWith("data-")) this.dataset[key.slice(5).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] = String(value); }
  getAttribute(key) { return this.attrs[key] ?? null; }
  removeAttribute(key) { delete this.attrs[key]; if (key === "hidden") this.hidden = false; }
  adopt(children) { return children.map((child) => { const node = child instanceof Element ? child : this.ownerDocument.createTextNode(String(child)); node.parentNode = this; return node; }); }
  append(...children) { delete this.text; this.children.push(...this.adopt(children)); }
  prepend(...children) { delete this.text; this.children.unshift(...this.adopt(children)); }
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
  emit(type, values = {}) { const event = { target: this, type, defaultPrevented: false, preventDefault() { this.defaultPrevented = true; }, stopPropagation() { this.stopped = true; }, ...values }; for (const listener of this.events[type] || []) listener(event); return event; }
  // A click runs the element's own listeners. A link then goes where it points, unless a listener stopped it.
  click() { if (this.disabled) return; const event = this.emit("click"); const href = this.tagName === "A" ? this.getAttribute("href") : null; if (href && !event.defaultPrevented) this.ownerDocument.follow?.(href); }
  focus() { this.ownerDocument.activeElement = this; }
  setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
  scrollIntoView() { this.ownerDocument.scrolledTo = this; }
  // Nothing is laid out here. A test that needs sizes gives the document a layout to measure by.
  getBoundingClientRect() { const box = this.ownerDocument.layout?.(this) || { left: 0, top: 0, width: 0, height: 0 }; return { ...box, right: box.left + box.width, bottom: box.top + box.height }; }
}
class Input extends Element {}
class TextArea extends Element {}

/** The frame the server sends: every element index.html gives an id, with its attributes, for the page script to find. */
function frame(document) {
  const body = frameSource.slice(frameSource.indexOf("<body"));
  for (const [, tag, attributes] of body.matchAll(/<([a-z]+)\b([^>]*\sid="[^"]+"[^>]*)>/g)) {
    const node = document.createElement(tag);
    for (const [, name, value] of attributes.matchAll(/([\w-]+)(?:="([^"]*)")?/g)) node.setAttribute(name, value ?? "");
    document.body.append(node);
  }
}

// The page reads the time and waits on timers. Both are held here, so a test
// moves time forward itself and nothing is left running when it ends.
const START = Date.parse("2026-02-02T12:00:00Z");
const secondsAgo = (n) => new Date(START - n * 1000).toISOString();
function clockFixture() {
  let now = START;
  let nextId = 0;
  const tasks = new Map();
  const schedule = (run, delay, repeat) => {
    const id = ++nextId;
    tasks.set(id, { run, due: now + (delay || 0), delay, repeat });
    return id;
  };
  return {
    Date: class extends Date {
      constructor(...args) { super(...(args.length ? args : [now])); }
      static now() { return now; }
    },
    setTimeout: (run, delay) => schedule(run, delay, false),
    setInterval: (run, delay) => schedule(run, delay, true),
    clearTimeout: (id) => tasks.delete(id),
    clearInterval: (id) => tasks.delete(id),
    requestAnimationFrame: (run) => schedule(run, 16, false),
    async advance(ms) {
      const until = now + ms;
      while (true) {
        const next = [...tasks].filter(([, task]) => task.due <= until).sort((a, b) => a[1].due - b[1].due)[0];
        if (!next) break;
        const [id, task] = next;
        now = task.due;
        if (task.repeat) task.due += task.delay;
        else tasks.delete(id);
        task.run();
        await flush();
      }
      now = until;
      await flush();
    },
  };
}

const guide = (summary = {}, extra = {}) => ({ schema: "kin.review.guide.v1", change: { base: "change-1" }, summary: { added: 0, modified: 0, removed: 0, recorded_consumers: 0, recorded_tests: 0, consumers_not_updated: 0, ...summary }, chapters: [], working_tree: { differs: false, derived: true, caught_up: true }, ...extra });

// An agent as the workspace server describes it: a name, what it did, what it
// has open and the change it committed. No session id or place on disk.
const AGENT_KEY = "00000000000000a1";
const AGENT_CHANGE = "c".repeat(64);
function agentsPayload({ agents = null, earlier = [] } = {}) {
  return { schema: "kin.workspace.agents.v1", read_at: secondsAgo(0), active_window_secs: 60,
    agents: agents ?? [{ key: AGENT_KEY, name: "Claude Code", state: "active", busy: false, through: "mcp",
      started_at: secondsAgo(240), last_activity_at: secondsAgo(5), calls_total: 7,
      recent_calls: [
        { seq: 7, at: secondsAgo(5), tool: "impact_analysis", entity_ids: [uuid(2), uuid(3), uuid(9)], result_count: 9, duration_ms: 37, is_error: false },
        { seq: 6, at: secondsAgo(20), tool: "find_references", entity_ids: [uuid(5)], result_count: 4, duration_ms: 12, is_error: false },
        { seq: 5, at: secondsAgo(30), tool: "get_entity_source", entity_ids: [uuid(5)], result_count: 2, duration_ms: 8, is_error: false },
        { seq: 4, at: secondsAgo(31), tool: "get_entity_source", entity_ids: [uuid(5)], result_count: 2, duration_ms: 8, is_error: false },
        { seq: 3, at: secondsAgo(40), tool: "semantic_locate", entity_ids: [uuid(5), uuid(3)], result_count: 5, duration_ms: 1800, is_error: false }],
      open_changes: [{ staged_edits: 1, age_secs: 9 }], intents: [], scope_names: {},
      // Committed before the newest changes its repository's summary lists, so Home finds it there or not at all.
      changes: [{ id: AGENT_CHANGE, title: "Explain how want_bytes treats text", time: secondsAgo(3600), entities_changed: 1 }], changes_total: 1 }],
    earlier,
    names: { [uuid(5)]: { label: "Signer.verify_signature", kind: "method" }, [uuid(2)]: { label: "want_bytes", kind: "function" }, [uuid(3)]: { label: "Signer", kind: "class" } } };
}

class FakeEventSource {
  constructor(url) { this.url = url; this.listeners = {}; FakeEventSource.opened.push(this); }
  addEventListener(type, listener) { (this.listeners[type] ||= []).push(listener); }
  emit(type, data = {}) { for (const listener of this.listeners[type] || []) listener({ data: JSON.stringify(data) }); }
  close() { this.closed = true; }
}
FakeEventSource.opened = [];

/** A repository as the hub lists it. One that is not open says nothing of its branch, its work or its agents. */
function listed(key, name, fields = {}) {
  return { key, name, url: `/repositories/${key}/workspace/`, location: `~/code/${name.toLowerCase()}`, available: true, entities: 216, last_change_at: secondsAgo(7200), runtime: "idle", ...fields };
}
/** What the hub adds for a repository whose own service is running. */
const open = (fields = {}) => ({ runtime: "running", branch: "main", working: { changed: 0, differs: false }, agents: { active: 0, connected: 0, names: [] }, merge_in_progress: false, ...fields });
const homePayload = (repositories, fields = {}) => ({ schema: "kin.workspace.home.v1", version: "0.8.2", read_at: secondsAgo(0), repositories, unavailable: 0, warning: null, ...fields });
/** Where one repository answers a read of its own, under the server's root. */
const under = (key, path) => `/repositories/${key}${path}`;
/** A repository's summary as its own server answers it: how large it is, what it is written in, and what was committed lately. */
const summaryOf = ({ entities = 216, relationships = 602, languages = [["python", 210], ["toml", 6]], recent = [] } = {}) => ({ schema: "kin.workspace.overview.v1", read_at: secondsAgo(0),
  counts: { ok: true, entities, relationships }, languages: { ok: true, items: languages.map(([language, n]) => ({ language, entities: n })) },
  recent_changes: { ok: true, shown: recent.length, items: recent, truncated: false } });

// Every browser a test opens keeps the requests the page had no business making:
// one that leaves the local server, one at the server's root from inside a
// repository, or one outside the hub's own reads. A test that caused one fails.
let opened = [];
test.afterEach(() => { const strays = opened.flat(); opened = []; assert.deepEqual(strays, [], "the page asked for something it should not"); });

/**
 * A browser on the page. By default it is inside a repository; with `hub` it
 * is the page at the server's root. `data` holds each answer by the path
 * asked, without the repository's base: an object, a function of the request,
 * or a list answered in turn with its last answer repeating. A request that
 * is not a GET is looked up under its method first, as "POST
 * /api/home/clients", and a guide of a branch or of one change under
 * "/api/guide?base" or "/api/guide?change". `kept` is what the browser keeps
 * between visits, and `tabStorage` what this tab keeps.
 */
function fixture({ hub = false, merge = true, preview = true, initialFailure = false, tabStorage = new Map(), kept = new Map(), observedIdentity = true, hash = "", origin = { owner: "pallets", host: "github.com" }, capabilities = {}, session = {}, live = false } = {}) {
  const apiBase = hub ? "" : BASE;
  const document = new Element("document", null); document.ownerDocument = document;
  document.createElement = (tag) => new ({ input: Input, textarea: TextArea }[tag] || Element)(tag, document);
  document.createElementNS = (_, tag) => document.createElement(tag);
  document.createTextNode = (value) => { const node = new Element("#text", document); node.text = value; return node; };
  document.getElementById = (id) => document.querySelector(`#${id}`);
  document.documentElement = document.createElement("html"); document.body = document.createElement("body");
  document.append(document.documentElement); document.documentElement.append(document.body); document.activeElement = document.body;
  frame(document);
  // The server writes the repository's base into the frame, and nothing at its root.
  document.body.dataset.apiBase = apiBase;
  const events = {}; const requests = []; const failures = new Map(); const held = new Map(); const observers = []; const copied = []; const clipboard = { refuses: false };
  const strays = []; opened.push(strays);
  const sessionPath = hub ? "/api/home/session" : "/api/workspace/session";
  if (initialFailure) failures.set(sessionPath, "Session disconnected");
  const expected = { repository_id: "repository-a", workspace_id: "workspace-a", roots: { generation: 4 }, target_ref: refWire("main"), source_ref: refWire("feature"), ours_change: hashWire(1), theirs_change: hashWire(2), workspace_tree_hash: hashWire(3) };
  const data = hub ? {
    "/api/home/session": { schema: "kin.workspace.home-session.v1", version: "0.8.2", actor: "Reader", token: "synthetic-token", ...session },
    "/api/home": homePayload([
      listed(KEY, "Fixture", { ...origin, ...open({ working: { changed: 2, differs: true }, agents: { active: 1, connected: 1, names: ["Claude Code"] } }), last_change_at: secondsAgo(600) }),
      listed(NOTES, "notes", { ...open(), entities: 40, last_change_at: secondsAgo(86_400) }),
      listed(ARCHIVE, "archive", { entities: 12, last_change_at: secondsAgo(30 * 86_400) })]),
    "/api/home/clients": { schema: "kin.setup-clients.v1", clients: [
      { id: "lantern", name: "Lantern", detected: false, connected: false },
      { id: "harbor-editor", name: "Harbor Editor", detected: true, connected: false },
      { id: "claude-code", name: "Claude Code", detected: true, connected: true }] },
    // What Home reads from each repository it opens: its summary, its agents and its uncommitted work.
    [under(KEY, "/api/workspace/overview")]: summaryOf({ recent: [
      { id: "change-0", title: "Update Kin's understanding of this code for Kin 0.8.2", author: "Dana Reader", time: secondsAgo(300), kin_checkpoint: true },
      { id: "change-1", title: "Bound base64 input", author: "Dana Reader", time: secondsAgo(600), kin_checkpoint: false }] }),
    [under(KEY, "/api/workspace/agents")]: agentsPayload(),
    [under(KEY, "/api/guide")]: working({ modified: 2, recorded_consumers: 3, consumers_not_updated: 1, recorded_tests: 1 }, [
      changed(2, "want_bytes", { connections: connected([relying(10, "Serializer.dumps"), relying(11, "Serializer.loads")], [], { consumers_total: 3 }), consumer_verdict: { verdict: "not_updated", reason: "signature_changed", consumers: 1 } }),
      changed(7, "quiet", { connections: connected([relying(10, "Serializer.dumps")]), consumer_verdict: { verdict: "needs_no_update", reason: "comments_or_layout_only", consumers: 1 } })]),
    [under(NOTES, "/api/workspace/overview")]: summaryOf({ entities: 40, relationships: 51, languages: [["rust", 40]], recent: [{ id: "n-1", title: "Add the notes index", author: "Ana Writer", time: secondsAgo(86_400), kin_checkpoint: false }] }),
    [under(NOTES, "/api/workspace/agents")]: agentsPayload({ agents: [] }),
    [under(ARCHIVE, "/api/workspace/overview")]: summaryOf({ entities: 12, relationships: 9, languages: [["go", 12]] }),
    [under(ARCHIVE, "/api/workspace/agents")]: agentsPayload({ agents: [] }),
    [under(ARCHIVE, "/api/guide")]: guide(),
  } : {
    "/api/workspace/session": { repository: { name: "Fixture", ...origin, key: KEY, repository_id: observedIdentity ? "repository-a" : null, workspace_id: observedIdentity ? "workspace-a" : null },
      repositories: [{ key: KEY, name: "Fixture", ...origin, selected: true, url: `${BASE}/workspace/` }, { key: NOTES, name: "notes", selected: false, url: `/repositories/${NOTES}/workspace/` }],
      runtime: { state: "unknown", detail: "Not observed" }, capabilities: { graph: true, review: true, merge, merge_preview: preview, commit: true, branch_switch: true, conflict_details: true, ...capabilities },
      token: "synthetic-token", actor: "Reader", review_url: `${BASE}/review/`, merge_unavailable_reason: "Confirmation requires explicit authorization.", version: "0.8.2", ...session },
    "/api/home": homePayload([
      listed(KEY, "Fixture", { ...origin, ...open({ working: { changed: 1, differs: true }, agents: { active: 1, connected: 1, names: ["Claude Code"] } }) }),
      listed(NOTES, "notes", { entities: 40 })]),
    "/api/changes": { changes: [{ id: "change-0", title: "Update Kin's understanding of this code for Kin 0.8.2", author: "Dana Reader", timestamp: "2026-01-02T00:00:00Z", origin: "native", entity_deltas: 10, kin_checkpoint: true }, { id: "change-1", title: "Preserved change", author: "Dana Reader <dana@example.invalid> 1749933342 -0700", timestamp: "2026-01-01T00:00:00Z", entity_deltas: 2 }, { id: "change-2", title: "Merge branch", author: "Dana Reader", timestamp: "2025-12-01T00:00:00Z", entity_deltas: 7, parents: 2 }], truncated: false },
    // Each change's own guided review count: the merge counts what it brought in.
    "/api/counts": { counts: { "change-0": { entities: 10, module_containers: 0, containers_through_members: 0 }, "change-1": { entities: 2, module_containers: 1, containers_through_members: 0 }, "change-2": { entities: 4, module_containers: 2, containers_through_members: 1, merge: true, merge_counted: true } } },
    "/api/standing": { standing: { "change-1": { state: "approved" } } },
    "/api/branches": { branches: [{ name: "main", active: true, default: true }, { name: "feature", active: false }] },
    "/api/workspace/conflicts/details": { schema: "kin.workspace.conflicts.v1", report: { record: null }, state: { status: "none", active: false } },
    "/api/workspace/branches": { schema: "kin.branch-list.v1", branches: [{ name: refWire("main"), active: true }, { name: refWire("feature"), active: false }] },
    "/api/workspace/branch/preview": { schema: "kin.branch-switch-preview.v1", expected: { repository_id: "repository-a", workspace_id: "workspace-a", workspace_tree: hashWire(3), target_ref: refWire("feature"), target_change: hashWire(2) }, source: { display: "refs/heads/main" }, target: { display: "refs/heads/feature", name: refWire("feature"), change: hashWire(2) }, pending_work: { present: true, detail: "Compatible pending work is carried." }, limitations: ["The selected state is rechecked before switching."] },
    "/api/workspace/branch/confirm": { mutated: true, report: { schema: "kin.branch-list.v1", repository_id: "repository-a", workspace_id: "workspace-a", branches: [{ name: refWire("feature"), active: true }] } },
    // The guide also says where a changed entity sits on disk. The page never shows it.
    "/api/guide": guide({ modified: 1, recorded_consumers: 16, recorded_tests: 2 }, { chapters: [{ steps: [changed(2, "want_bytes", { connections: connected([relying(10, "Serializer.dumps")], [relying(20, "TestEncoding.test_want_bytes", { relation: "tests" }), relying(21, "TestEncoding.test_want_text", { relation: "tests" })], { consumers_total: 16 }), consumer_verdict: { verdict: "needs_no_update", reason: "comments_or_layout_only", consumers: 16 } })] }] }),
    // What a branch holds beyond the current one, and one recorded change, are guides of their own.
    "/api/guide?base": guide({ modified: 1, recorded_consumers: 3, recorded_tests: 2 }, { chapters: [{ steps: [{ kind: "function" }] }], range: { base_ref: "main", head_ref: "feature", changes_total: 1, changes: [{ id: "c9", title: "Bound base64 input before decoding", author: "Dana Reader", timestamp: secondsAgo(600) }] } }),
    "/api/guide?change": (url) => guide({ modified: 1, recorded_consumers: 16, recorded_tests: 2 }, { change: { id: url.searchParams.get("change"), base: "change-0", title: "Preserved change", author: "Dana Reader", timestamp: "2026-01-01T00:00:00Z" }, chapters: [{ steps: [changed(2, "want_bytes")] }] }),
    "/api/workspace/merge/preview": { schema: "kin.merge-preview.v1", expected, base_change: hashWire(0), outcome: "merged", conflicts: [], conflicts_total: 0, limitations: ["Publication checks run on confirmation."] },
    "/api/workspace/merge/confirm": { mutated: true, report: { source_ref: refWire("feature"), target_ref: refWire("main"), outcome: "merged", merge_change: hashWire(4) } },
    "/api/workspace/overview": { schema: "kin.workspace.overview.v1", counts: { ok: true, entities: 216, relationships: 602 },
      languages: { ok: true, items: [{ language: "python", entities: 210 }, { language: "toml", entities: 6 }] },
      containers: { ok: true, shown: 2, total: 15, items: [{ id: uuid(4), name: "signer", kind: "module", members: 9, entities: 30, container: null }, { id: uuid(1), name: "encoding", kind: "module", members: 4, entities: 6, container: null }] },
      most_relied_on: { ok: true, items: [{ id: uuid(2), name: "want_bytes", kind: "function", signature: "def want_bytes(s)", container: { id: uuid(1), name: "encoding", kind: "module" }, dependents: 23 }, { id: uuid(3), name: "Signer", kind: "class", container: { id: uuid(4), name: "signer", kind: "module" }, dependents: 22 }], total: 172, shown: 2 } },
    "/api/workspace/agents": agentsPayload(),
    "/api/workspace/agents/timeline": () => ({ schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 7, earliest_kept_from_start: true,
      agent: { ...agentsPayload().agents[0], recent_calls: null },
      calls: [
        { seq: 7, at: secondsAgo(5), tool: "kin_transaction_commit", entity_ids: [uuid(2)], result_count: 1, duration_ms: 900, is_error: false },
        { seq: 6, at: secondsAgo(9), tool: "kin_transaction_stage", entity_ids: [uuid(2)], result_count: 1, duration_ms: 20, is_error: false },
        { seq: 5, at: secondsAgo(10), tool: "kin_transaction_begin", entity_ids: [], result_count: 0, duration_ms: 20, is_error: false },
        ...agentsPayload().agents[0].recent_calls.slice(0, 4)],
      names: agentsPayload().names }),
    "/api/workspace/ask": { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [
      { id: uuid(5), name: "Signer.verify_signature", kind: "method", signature: "def verify_signature(self, value, sig) -> bool", container: { id: uuid(3), name: "Signer", kind: "class" }, matched: ["meaning"], why: "Its description and code share words with the question." },
      { id: uuid(2), name: "want_bytes", kind: "function", signature: "def want_bytes(s)", container: { id: uuid(1), name: "encoding", kind: "module" }, matched: ["name", "meaning"], why: "Its name matches." }], total: 2 },
  };
  const fire = (type) => { for (const listener of events[type] || []) listener(); };
  let address = hash;
  const location = { origin: "http://local.test", href: `http://local.test${hub ? "/" : `${BASE}/workspace/`}`, assign: (url) => { location.assigned = url; }, reload() {},
    get hash() { return address; },
    // When the page moves itself to another address, the browser tells it so afterwards.
    set hash(next) { const before = address; address = String(next); if (address !== before) Promise.resolve().then(() => fire("hashchange")); } };
  document.follow = (href) => { if (href.startsWith("#")) location.hash = href; else location.followed = href; };
  const clock = clockFixture();
  let operation = 0;
  const context = {
    document, location, URL, TextDecoder, Node: Element, HTMLElement: Element, HTMLInputElement: Input, HTMLTextAreaElement: TextArea, AbortController, Intl,
    Date: clock.Date, setTimeout: clock.setTimeout, clearTimeout: clock.clearTimeout, setInterval: clock.setInterval, clearInterval: clock.clearInterval, requestAnimationFrame: clock.requestAnimationFrame,
    ResizeObserver: class { constructor(redraw) { this.redraw = redraw; this.watched = []; observers.push(this); } observe(node) { this.watched.push(node); } disconnect() { this.disconnected = true; } },
    ...(live ? { EventSource: FakeEventSource } : {}),
    history: { replaceState: (_, __, next) => { address = next; } },
    localStorage: { getItem: (key) => kept.get(key) ?? null, setItem: (key, value) => kept.set(key, String(value)), removeItem: (key) => kept.delete(key) },
    sessionStorage: { getItem: (key) => tabStorage.get(key) || null, setItem: (key, value) => tabStorage.set(key, value), removeItem: (key) => tabStorage.delete(key) },
    crypto: { randomUUID: () => `00000000-0000-4000-8000-${String(++operation).padStart(12, "0")}` },
    navigator: { clipboard: { writeText: async (text) => { if (clipboard.refuses) throw new Error("Not allowed"); copied.push(text); } } },
    async fetch(url, options = {}) {
      const parsed = new URL(url, location.origin);
      // A read under the repository's base is named without it. At the server's root the hub makes its own reads, and a repository reads the list of repositories.
      const scoped = !hub && parsed.pathname.startsWith(`${BASE}/api/`);
      const name = scoped ? parsed.pathname.slice(BASE.length) : parsed.pathname;
      const method = options.method || "GET";
      // Home reads what each repository it opens holds, how an agent's change waiting there reads, where an entity two of its pictures name alike sits, and puts a question to each; it writes nothing there.
      const opening = hub && method === "GET" && /^\/repositories\/[0-9a-f]{64}\/api\/(workspace\/(overview|agents|ask|entity)|guide|standing|counts)$/.test(name) && (!name.endsWith("/api/guide") || parsed.searchParams.get("working") === "1" || parsed.searchParams.has("change"));
      if (parsed.origin !== location.origin || !(scoped || opening || (hub ? /^\/api\/home(\/|$)/.test(name) : name === "/api/home" && method === "GET"))) { strays.push(`${method} ${url}`); return { ok: false, status: 599, json: async () => ({ error: "No such address." }) }; }
      const keyed = `${method} ${name}`;
      // The three guides share a path: uncommitted work, what a branch holds, and one change.
      const formed = name.endsWith("/api/guide") && !parsed.searchParams.has("working") ? `${name}?${parsed.searchParams.has("base") ? "base" : "change"}` : name;
      requests.push({ name, url, method, options });
      // A held read waits for its release, or for the page to give up on it, as a browser's fetch does.
      const abandoned = new Promise((_, reject) => options.signal?.addEventListener?.("abort", () => reject(Object.assign(new Error("The operation was aborted."), { name: "AbortError" }))));
      for (const key of [keyed, formed, name]) if (held.has(key)) { await Promise.race([held.get(key), abandoned]); break; }
      const failure = failures.get(keyed) ?? failures.get(formed) ?? failures.get(name);
      const at = [keyed, formed, name].find((key) => key in data) ?? name;
      if (name === "/api/workspace/branch/confirm") data[name].operation_id = JSON.parse(options.body).operation_id;
      if (failure instanceof Error) throw failure;
      if (!failure && !(at in data)) return { ok: false, status: 404, json: async () => ({ error: "Not found" }) };
      let body = data[at];
      if (Array.isArray(body)) body = body.length > 1 ? body.shift() : body[0];
      if (typeof body === "function") body = await body(parsed, options);
      if (body && body.__status) return { ok: false, status: body.__status, headers: { get: (header) => body.headers?.[header.toLowerCase()] ?? null }, json: async () => ({ error: body.error, ...body.body }) };
      return { ok: !failure, status: failure ? failure.status || 503 : 200, json: async () => failure ? failure.body || { error: failure } : body };
    },
    addEventListener(type, listener) { (events[type] ||= []).push(listener); }, scrollTo() {},
  };
  context.window = context;
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(graphSource, context, { filename: "graph.js" });
  vm.runInContext(agentsSource, context, { filename: "agents.js" });
  vm.runInContext(source, context, { filename: "workspace.js" });
  const api = context.KinWorkspace;
  return { document, requests, data, failures, expected, held, location, tabStorage, kept, observers, copied, clipboard, api, agents: context.KinWorkspaceAgents,
    node: (id) => document.getElementById(id), main: () => document.getElementById("workspace-main"), text: () => document.getElementById("workspace-main").textContent,
    nav: () => document.getElementById("workspace-nav"),
    links: () => document.getElementById("workspace-main").querySelectorAll("a"),
    buttons: () => document.getElementById("workspace-main").querySelectorAll("button"),
    button(label) { return this.buttons().find((node) => node.textContent === label); },
    navigate(route) { address = `#/${route}`; fire("hashchange"); },
    refresh() { document.getElementById("workspace-refresh").click(); },
    /** Move the page's clock forward, running what falls due on the way. */
    advance: (ms) => clock.advance(ms),
    /** The window regains focus, as when the reader comes back to the tab. */
    focus() { fire("focus"); },
    /** Type into a field the way a reader does: the value, then the event the page listens for. */
    type(id, value) { const field = document.getElementById(id); field.value = value; field.emit("input"); return field; },
    count: (name) => requests.filter((request) => request.name === name).length,
    /** The bodies this page sent to a path, in order. */
    sent: (name) => requests.filter((request) => request.name === name && request.method !== "GET").map((request) => JSON.parse(request.options.body)),
    async preview() { this.navigate("branches"); this.button("Merge into main").click(); await flush(); },
    confirm() { const checkbox = this.node("workspace-merge-confirm"); checkbox.checked = true; checkbox.emit("change"); this.node("workspace-merge-submit").click(); },
  };
}

/** A changed entity as the guided review walks it, with what relies on it and the tests that reach it. */
function changed(n, name, fields = {}) {
  return { entity_id: uuid(n), name, kind: "function", role: "source", language: "python", within: "module encoding", file: "src/itsdangerous/encoding.py", line: 12,
    change: "modified", risk: "low", narration: "", fields: [{ field: "body" }], findings: [], connections: connected([]),
    evidence: { head_change: "WORKSPACE", fingerprint_after: { signature: `s-${n}`, behavior: `b-${n}`, ast: `a-${n}` } }, ...fields };
}
/** One entity that relies on a changed one, or one test that reaches it. */
const relying = (n, name, fields = {}) => ({ relation_id: `r-${n}`, relation: "calls", origin: "parsed", confidence: 1, entity: { id: uuid(n), name, kind: "method", role: "source", within: "class Serializer" }, changed_here: false, ...fields });
const connected = (consumers, tests = [], totals = {}) => ({ consumers, consumers_total: consumers.length, consumers_changed_here: consumers.filter((row) => row.changed_here).length, tests, tests_total: tests.length, relation_changes: [], relation_changes_total: 0, ...totals });
/**
 * Uncommitted work as the working read answers it. Kin stamps the answer with
 * the time it was read. `edited`, when given, is how long ago the work last
 * changed, which Kin may also say.
 */
const working = (summary, steps, { edited, ...fields } = {}) => guide(summary, { change: { id: "WORKSPACE", base: "change-1", parents: ["change-1"], title: "Uncommitted work", timestamp: secondsAgo(0), origin: "workspace" }, chapters: [{ title: "Encoding", steps }],
  working_tree: { differs: true, derived: true, caught_up: true, ...(edited === undefined ? {} : { changed_at: secondsAgo(edited) }) }, ...fields });
/** A recorded change as its own guide answers it. */
const recorded = (id, summary, steps, fields = {}) => ({ ...guide(summary, { change: { id, base: "change-0", parents: ["change-0"], title: "Preserved change", message: "Preserved change", author: "Dana Reader <dana@example.invalid> 1749933342 -0700", timestamp: secondsAgo(3600), origin: "native" }, chapters: steps.length ? [{ title: "Encoding", steps }] : [], ...fields }), working_tree: undefined });
const workingKey = `kin.workspace.working:${BASE}`;
/** Where this browser keeps what a reader marked in a review of uncommitted work, and of one change. */
const marksKey = (subject) => `kin.workspace.review:${BASE}:${subject}`;

/**
 * Four changed entities, one of each kind a review meets, in the order Kin's
 * guide lists them: one that asks nothing, one whose callers were left
 * behind, a new one, and a removed one something still relies on.
 */
const fourChanged = () => [
  changed(7, "quiet", { connections: connected([relying(14, "Signer.derive_key")]), consumer_verdict: { verdict: "needs_no_update", reason: "comments_or_layout_only", consumers: 1 } }),
  changed(2, "want_bytes", { fields: [{ field: "signature" }, { field: "body" }],
    before: { state: "recorded", text: "def want_bytes(s):\n    return s.encode()\n", start_line: 12 }, after: { state: "recorded", text: "def want_bytes(s, encoding):\n    return s.encode(encoding)\n", start_line: 12, working: true },
    connections: connected([relying(10, "Serializer.dumps"), relying(11, "Serializer.loads", { changed_here: true }), relying(12, "Signer.sign", { relation: "references" })], [relying(20, "TestEncoding.test_want_bytes", { relation: "tests" })], { consumers_total: 9 }),
    consumer_verdict: { verdict: "not_updated", reason: "signature_changed", consumers: 8 } }),
  changed(5, "base64_encode", { change: "added", fields: [], after: { state: "recorded", text: "def base64_encode(s):\n    return s\n", start_line: 30, working: true }, consumer_verdict: { verdict: "may_need_review", reason: "added", consumers: 0 } }),
  changed(6, "old_helper", { change: "removed", fields: [], evidence: { head_change: "WORKSPACE", fingerprint_before: { signature: "s-6", behavior: "b-6", ast: "a-6" } },
    before: { state: "recorded", text: "def old_helper():\n    pass\n", start_line: 50 }, connections: connected([relying(13, "Signer.unsign")]), consumer_verdict: { verdict: "may_need_review", reason: "removed", consumers: 1 } }),
];
const FOUR = { added: 1, modified: 2, removed: 1, recorded_consumers: 10, consumers_not_updated: 8, consumers_may_need_review: 1, consumers_need_no_update: 1, recorded_tests: 1 };
/** The four, and five more that nothing relies on: more than a picture holds. */
const manyChanged = () => [...fourChanged(), ...[0, 1, 2, 3, 4].map((i) => changed(30 + i, `extra_${i}`))];
/** Lay a picture's two columns side by side, or one above the other, so its lines have boxes to measure. */
function pictureLayout(browser, host, { stacked = false } = {}) {
  browser.document.layout = (node) => {
    const group = host();
    if (node === group) return { left: 0, top: 0, width: 800, height: 400 };
    const columns = group.children.filter((column) => column.tagName === "DIV" && column.getAttribute("aria-hidden") === null);
    for (const [index, column] of columns.entries()) {
      const at = column.children.filter(boxWords).indexOf(node);
      if (at >= 0) return { left: index && !stacked ? 500 : 0, top: 20 + at * 40 + (index && stacked ? 300 : 0), width: 200, height: 30 };
    }
    return null;
  };
}

/** A list of changed entities, as a reader meets it: its groups in order, and each row's number, name, place, reason, mark and address. */
const reviewRows = (host) => host.querySelectorAll("section").filter((group) => group.querySelector("ul")).map((group) => ({ title: group.getAttribute("aria-label"), count: group.querySelector("h2").querySelector("span").textContent,
  rows: group.querySelectorAll("a").map((row) => { const [number, name, place, reason, mark] = row.children.map((cell) => cell.textContent); return { number, name, place, reason, mark, href: row.getAttribute("href") }; }) }));
/** The words of a box in a picture: its name and where it sits. Anything else in a picture has no such pair. */
const boxWords = (node) => node.children.find((child) => child.children.length === 2 && child.children.every((part) => part.tagName === "SPAN"));
/**
 * A picture of a piece of work as a reader meets it: what it says it shows,
 * and each column's heading, its boxes and what it says of the rest. A box is
 * its name, where it sits, the word beside it, where it leads and the tone it
 * is drawn in.
 */
function picture(host) {
  const group = host?.querySelector("[role=group]");
  if (!group) return null;
  const box = (node) => [...boxWords(node).children.map((part) => part.textContent), node.children.filter((child) => child !== boxWords(node) && child.getAttribute("aria-hidden") !== "true").map((child) => child.textContent).join(""), node.getAttribute("href"), node.dataset.tone ?? ""];
  const columns = group.children.filter((node) => node.tagName === "DIV" && node.getAttribute("aria-hidden") === null).map((column) => {
    const [first, ...rest] = column.children;
    const head = first && !boxWords(first) ? first : null;
    const items = head ? rest : column.children;
    return { head: head?.textContent ?? null, boxes: items.filter(boxWords).map(box), more: items.filter((node) => !boxWords(node)).map((node) => [node.textContent, node.getAttribute("href")]) };
  });
  return { label: group.getAttribute("aria-label"), columns, lines: () => group.querySelector("svg")?.querySelectorAll("path").map((line) => [line.getAttribute("d"), line.getAttribute("data-tone")]) || [] };
}
/** A before-and-after as its lines: [number before, number after, mark, text]. A folded run is its button's words. */
function diffRows(host) {
  const cells = host.querySelector("[role=region]").children[0].children;
  const rows = [];
  for (let at = 0; at < cells.length;) {
    if (cells[at].tagName === "BUTTON") { rows.push(cells[at].textContent); at += 1; }
    else { rows.push(cells.slice(at, at + 4).map((cell) => cell.textContent)); at += 4; }
  }
  return rows;
}
/** The tabs at the head of Work: each one's name, the count beside it, where it leads, and which is open. */
const workTabs = (browser) => browser.main().querySelectorAll("nav").filter((node) => node.getAttribute("aria-label") === "Work").flatMap((nav) => nav.querySelectorAll("a"))
  .map((tab) => [tab.children[0].textContent, tab.children[1]?.textContent ?? "", tab.getAttribute("href"), tab.getAttribute("aria-current")]);
/** The commit form's parts, wherever they are drawn. */
const commitForm = (browser) => ({ field: browser.node("commit-message"), submit: browser.node("commit-submit") });
/** Write a message and commit, opening the form first where it waits to be asked for. */
function writeCommit(browser, message) {
  if (!browser.node("commit-message")) browser.button("Commit").click();
  browser.type("commit-message", message);
  browser.node("commit-submit").click();
}
/** The sidebar's row for a page, and the count it carries with what it says it counts. */
const sideRoute = (browser, route) => browser.nav().querySelector(`a[data-route=${route}]`);
const sideCount = (browser, route) => { const badge = sideRoute(browser, route).querySelectorAll("span").find((node) => node.getAttribute("aria-label")); return badge ? [badge.textContent, badge.getAttribute("aria-label")] : null; };
const sideCurrent = (browser) => browser.nav().querySelectorAll("a").filter((node) => node.getAttribute("aria-current") === "page").map((node) => node.dataset.route ?? node.textContent);
/** What Home asked of each repository, in the order it asked: its summary, its agents, its uncommitted work, or a question. */
function homeReads(hub) {
  const asked = {};
  for (const request of hub.requests) {
    const match = request.name.match(/^\/repositories\/([0-9a-f]{64})\/api\/(?:workspace\/)?(.+)$/);
    const query = String(new URL(request.url, "http://local.test").searchParams);
    if (match) (asked[match[1]] ||= []).push(query ? `${match[2]}?${query}` : match[2]);
  }
  return asked;
}
/** The repositories the sidebar lists: each one's name, where it leads, what its dot says, and the branch beside it. */
const sideRepositories = (browser) => browser.nav().querySelectorAll("a[title]").filter((node) => !node.getAttribute("aria-label"))
  .map((node) => ({ name: node.getAttribute("title"), href: node.getAttribute("href"), mark: node.children[0].dataset.state ?? "", branch: node.children[2]?.textContent ?? "" }));

// ── Words ──────────────────────────────────────────────────────────────────

test("uncommitted work reads as the guided review's sentence of real numbers", () => {
  const { api } = fixture();
  const sentence = (summary, steps = [{ kind: "function" }]) => api.workSentence(guide(summary, { chapters: [{ steps }] }));
  assert.equal(sentence({ modified: 1, recorded_consumers: 16, recorded_tests: 2 }), "One function changed. 16 other places rely on it. None of them changed. Two tests reach it.");
  assert.equal(sentence({ modified: 3, recorded_consumers: 13, consumers_not_updated: 2, recorded_tests: 7 }, [{ kind: "function" }, { kind: "method" }]),
    "Three entities changed. 13 other places rely on them. Two were not updated. Seven tests reach them.");
  assert.equal(sentence({ added: 1 }), "One function changed. Kin found no connections outside this change. Kin found no tests that reach this work.");
  assert.equal(sentence({ modified: 2, recorded_consumers: 2, consumers_not_updated: 2 }), "Two functions changed. Two other places rely on them. Neither was updated. Kin found no tests that reach this work.");
  assert.equal(sentence({}), null);
  // The guide's verdicts, when it gives them: what the change asks of the places that rely on it.
  assert.equal(sentence({ modified: 1, recorded_consumers: 16, recorded_tests: 2, consumers_need_no_update: 16, consumers_may_need_review: 0 }), "One function changed. 16 other places rely on it. None need an update. Two tests reach it.");
  assert.equal(sentence({ modified: 1, recorded_consumers: 16, recorded_tests: 2, consumers_need_no_update: 13, consumers_may_need_review: 3 }), "One function changed. 16 other places rely on it. Check three of them. Two tests reach it.");
  assert.equal(sentence({ modified: 1, recorded_consumers: 3, consumers_not_updated: 3, consumers_need_no_update: 0, consumers_may_need_review: 0 }), "One function changed. Three other places rely on it. None were updated. Kin found no tests that reach this work.");
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
  // A route read from the page, with what it targets, as plain values.
  const route = (address) => { const { target, ...rest } = api.parseRoute(address); return target === undefined ? rest : { ...rest, target: { ...target } }; };
  for (const address of ["", "#/uncommitted", "#/work", "#/home", "#/overview", "#/nowhere"]) assert.deepEqual(route(address), { name: "uncommitted" }, address);
  assert.deepEqual(route("#/history"), { name: "history" });
  assert.deepEqual(route("#/changes"), { name: "history" });
  assert.deepEqual(route("#/branches"), { name: "branches" });
  assert.deepEqual(route("#/repository"), { name: "branches" });
  assert.deepEqual(route(`#/entities?entity=${id}`), { name: "entity", id });
  assert.deepEqual(route("#/entities"), { name: "explore", q: "" });
  assert.deepEqual(route(`#/entity/${id}`), { name: "entity", id });
  assert.deepEqual(route("#/entity/not-an-id"), { name: "explore", q: "" });
  assert.deepEqual(route("#/explore?q=verify%20a%20signature"), { name: "explore", q: "verify a signature" });
  assert.deepEqual(route(`#/agents/${AGENT_KEY}`), { name: "agents", agent: AGENT_KEY });
  assert.deepEqual(route("#/agents/not-a-key"), { name: "agents" });
  // A review is a walk through uncommitted work or through one change. An address that names neither a step nor the end opens the page that lists what it would walk.
  assert.deepEqual(route("#/review"), { name: "history" });
  assert.deepEqual(route("#/review/w"), { name: "uncommitted" });
  assert.deepEqual(route("#/review/c/not%20an%20id"), { name: "history" });
  assert.deepEqual(route("#/review/w/3"), { name: "review", target: { kind: "working" }, step: 3, done: false });
  assert.deepEqual(route("#/review/w/done"), { name: "review", target: { kind: "working" }, step: null, done: true });
  assert.deepEqual(route("#/review/c/change-1"), { name: "review", target: { kind: "change", id: "change-1" }, step: null, done: false });
  assert.deepEqual(route("#/review/c/change-1/12"), { name: "review", target: { kind: "change", id: "change-1" }, step: 12, done: false });
  assert.deepEqual(route("#/review/c/change-1/done"), { name: "review", target: { kind: "change", id: "change-1" }, step: null, done: true });
  assert.deepEqual(route("#/review/c/change-1/0"), { name: "review", target: { kind: "change", id: "change-1" }, step: null, done: false }, "a number that is no step opens the change's list");
  // At the server's root the page lists the repositories, and has a page for the coding agents.
  const hub = fixture({ hub: true }).api;
  assert.deepEqual({ ...hub.parseRoute("") }, { name: "hub", add: false });
  assert.deepEqual({ ...hub.parseRoute("#/add") }, { name: "hub", add: true });
  assert.deepEqual({ ...hub.parseRoute("#/agents") }, { name: "clients", add: false });
  assert.deepEqual({ ...hub.parseRoute("#/clients") }, { name: "clients", add: false });
  assert.deepEqual({ ...hub.parseRoute("#/branches") }, { name: "hub", add: false });
});

test("an entity body keeps the indentation it had in place", () => {
  const { api } = fixture();
  assert.deepEqual([...api.bodyLines("def sign(self):\n        \"\"\"Doc.\"\"\"\n        return 1\n")], ["def sign(self):", "    \"\"\"Doc.\"\"\"", "    return 1"]);
  assert.deepEqual([...api.bodyLines("class A:\n    x = 1\n    def f(self):\n        pass")], ["class A:", "    x = 1", "    def f(self):", "        pass"]);
  assert.deepEqual([...api.bodyLines("one line")], ["one line"]);
});

// ── Frame ──────────────────────────────────────────────────────────────────

test("the frame holds every element the page looks up and does not build itself", () => {
  const scripts = `${source}\n${agentsSource}`;
  const framed = new Set([...frameSource.matchAll(/\sid="([^"]+)"/g)].map((match) => match[1]));
  const built = new Set([...scripts.matchAll(/\bid: "([\w-]+)"|\bsection\("([\w-]+)"/g)].map((match) => match[1] || match[2]));
  const looked = new Set([...scripts.matchAll(/\b(?:byId|getElementById)\("([\w-]+)"\)/g)].map((match) => match[1]));
  assert.ok(looked.size > 20, "the page's lookups were found");
  for (const id of looked) assert.ok(framed.has(id) || built.has(id), `the page looks up #${id}, which neither index.html nor the page provides`);
  // What the page hangs its frame on is in the frame the server sends.
  for (const id of ["workspace-side", "workspace-nav", "workspace-status", "workspace-status-text", "workspace-refresh", "workspace-version", "workspace-theme", "workspace-menu-toggle", "workspace-main", "workspace-announcer"]) assert.ok(framed.has(id), id);
  assert.match(frameSource, /<body data-api-base="__KIN_BASE__">/, "the server writes the repository's base where the page reads it");
});

test("a repository reads its own state and the machine's list of repositories, and never a graph sample or a map", async () => {
  const browser = fixture(); await flush();
  const asked = browser.requests.map((request) => `${request.method} ${request.name}${request.name === "/api/guide" ? `?${new URL(request.url, "http://local.test").searchParams.keys().next().value}` : ""}`);
  assert.deepEqual(asked, ["GET /api/workspace/session", "GET /api/changes", "GET /api/standing", "GET /api/branches", "GET /api/workspace/branches", "GET /api/workspace/conflicts/details", "GET /api/guide?working", "GET /api/workspace/overview", "GET /api/workspace/agents", "GET /api/home", "GET /api/workspace/remotes", "GET /api/workspace/flags", "GET /api/counts"],
    "each read once, then each listed change's count, and nothing else: no graph sample and no map");
  assert.equal(browser.node("workspace-version").textContent, "Kin 0.8.2");
  assert.equal(browser.node("workspace-status").dataset.state, "ready");
  assert.match(browser.node("workspace-status-text").textContent, /^Up to date /);

  // At the server's root the page reads the machine's own state, then what Home shows of each repository, and nothing else.
  const hub = fixture({ hub: true }); await flush();
  assert.deepEqual(hub.requests.slice(0, 3).map((request) => `${request.method} ${request.name}`), ["GET /api/home/session", "GET /api/home", "GET /api/home/clients"]);
  assert.deepEqual(homeReads(hub), {
    [KEY]: ["overview", "agents", "guide?working=1"],
    [NOTES]: ["overview", "agents"],
    [ARCHIVE]: ["overview", "agents", "guide?working=1"] }, "a repository the list says is clean is not asked what is uncommitted in it");
  assert.equal(hub.requests.length, 3 + 8);
  assert.equal(hub.document.title, "Home · Kin");
});

test("a read Kin is too busy to answer waits its turn and asks again, and a write is never sent twice", async () => {
  const browser = fixture({ hash: "#/explore" });
  const overview = browser.data["/api/workspace/overview"];
  let asked = 0;
  browser.data["/api/workspace/overview"] = () => { asked += 1; return asked === 1 ? { __status: 429, error: "Too many reads at once." } : asked === 2 ? { __status: 429, error: "Too many reads at once.", headers: { "retry-after": "60" } } : overview; };
  await flush();
  assert.equal(asked, 1);
  assert.equal(browser.node("workspace-status-text").textContent, "Loading", "the read is still on its way");
  assert.doesNotMatch(browser.text(), /couldn't load/, "being asked to wait is not a failure");
  await browser.advance(799);
  assert.equal(asked, 1);
  await browser.advance(1);
  assert.equal(asked, 2);
  // Kin may say how long to wait. The page waits no more than ten seconds whatever it says.
  await browser.advance(9_999);
  assert.equal(asked, 2);
  await browser.advance(1);
  assert.equal(asked, 3);
  assert.match(browser.text(), /216 entities and 602 connections\./);
  assert.equal(browser.node("workspace-status").dataset.state, "ready");

  // A read that is turned away every time is asked four more times, each after a longer wait, and then says so.
  const busy = fixture({ hash: "#/explore" });
  busy.data["/api/workspace/overview"] = { __status: 429, error: "Too many reads at once." };
  await flush();
  for (const [wait, asks] of [[800, 2], [1600, 3], [2400, 4], [3200, 5]]) { await busy.advance(wait - 1); assert.equal(busy.count("/api/workspace/overview"), asks - 1); await busy.advance(1); assert.equal(busy.count("/api/workspace/overview"), asks); }
  assert.match(busy.node("explore-results").textContent, /Kin couldn't load the summary\..*Too many reads at once\./s);
  await busy.advance(60_000);
  assert.equal(busy.count("/api/workspace/overview"), 5);

  // A write that is turned away is not sent again behind the reader's back.
  const writer = fixture(); await flush();
  writer.data["/api/workspace/commit"] = { __status: 429, error: "Too many at once." };
  writeCommit(writer, "Bound input"); await flush();
  await writer.advance(60_000);
  assert.equal(writer.count("/api/workspace/commit"), 1);
  assert.doesNotMatch(writer.node("work-commit").textContent, /^Committed/);
});

// ── Uncommitted ────────────────────────────────────────────────────────────

test("Uncommitted leads with what changed, one primary action into the review, and the list a review walks", async () => {
  const browser = fixture(); await flush();
  assert.equal(browser.main().querySelector("h1").textContent, "Work");
  assert.deepEqual(workTabs(browser), [["Uncommitted", "1", "#/uncommitted", "page"], ["Branches", "1", "#/branches", null], ["History", "", "#/history", null]], "uncommitted work, the other branches and what has landed are three views of one section");
  assert.equal(browser.node("work-lede").textContent, "One function changed. 16 other places rely on it. None of them changed. Two tests reach it.No agent reported writing it, so Kin cannot say whether it is yours or an agent's.");
  const primary = browser.links().filter((node) => node.className.includes("button-primary"));
  assert.deepEqual(primary.map((node) => [node.textContent, node.getAttribute("href")]), [["Start review", "#/review/w/1"]], "one primary action");
  assert.ok(browser.node("work-actions").contains(primary[0]), "it sits with the page's title");
  // A short list is its picture: what changed on one side, what relies on it and did not change on the other.
  const drawn = picture(browser.node("work-flight"));
  assert.equal(drawn.label, "1 changed entity, and 16 entities that rely on it without having changed");
  assert.deepEqual(drawn.columns[0], { head: "Changed1", boxes: [["want_bytes", "Function in encoding", "", "#/review/w/1", "quiet"]], more: [] });
  // Where a box on the right sits is pinned by its own test below.
  assert.deepEqual({ ...drawn.columns[1], boxes: drawn.columns[1].boxes.map(([name, , mark, href, tone]) => [name, mark, href, tone]) },
    { head: "Rely on it, and did not change16", boxes: [["Serializer.dumps", "", `#/entity/${uuid(10)}`, ""]], more: [["and 15 more", null]] });
  assert.deepEqual(reviewRows(browser.node("work-flight")), [], "the picture holds every entity, so no list repeats it");
  assert.doesNotMatch(browser.text(), /\.py|src\//, "no locations");
  assert.doesNotMatch(browser.text(), /Claude Code|Preserved change/, "who is connected and what was committed have pages of their own");
  assert.equal(browser.document.title, "Uncommitted · pallets/Fixture");
  assert.deepEqual(sideCurrent(browser), ["uncommitted"]);
});

test("with nothing uncommitted the one action opens the latest change", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = guide();
  await flush();
  assert.equal(browser.node("work-lede").textContent, "Nothing in flight. Your code matches your last commit.");
  assert.equal(browser.node("work-actions").textContent, "", "there is nothing to commit or to review");
  assert.match(browser.node("work-flight").textContent, /^Nothing is uncommitted/);
  assert.deepEqual(browser.node("work-flight").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Review the latest change", guided("change-1")]], "Kin's own update is not the latest change a person made");
  assert.equal(sideCount(browser, "uncommitted"), null);
  assert.deepEqual(workTabs(browser)[0], ["Uncommitted", "", "#/uncommitted", "page"], "and the tab carries no count");
  // Code that differs with no entity changed says so, can still be committed, and is marked without a number.
  browser.data["/api/guide"] = guide({}, { working_tree: { differs: true, derived: true, caught_up: true } });
  browser.refresh(); await flush();
  assert.equal(browser.node("work-lede").textContent, "Your code differs from your last commit, but no entity changed.");
  assert.deepEqual(browser.node("work-actions").querySelectorAll("button").map((node) => node.textContent), ["Commit"]);
  assert.equal(browser.node("work-actions").querySelectorAll("a").length, 0, "there is no entity to walk");
  assert.deepEqual(sideCount(browser, "uncommitted"), ["•", "uncommitted work"]);
  assert.deepEqual(workTabs(browser)[0], ["Uncommitted", "•", "#/uncommitted", "page"]);
});

test("code that differs with no entity changed is not also called nothing uncommitted", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = guide({}, { working_tree: { differs: true, derived: true, caught_up: true } });
  await flush();
  assert.equal(browser.node("work-lede").textContent, "Your code differs from your last commit, but no entity changed.");
  assert.equal(browser.node("work-flight").textContent, "", "the sentence above already says what there is");
});

test("a failed read says so in its own section with a retry, and the rest of the page keeps working", async () => {
  const browser = fixture();
  browser.failures.set("/api/guide", "Working read failed");
  await flush();
  assert.match(browser.node("work-lede").textContent, /Kin couldn't load your uncommitted work\./);
  assert.match(browser.node("work-lede").textContent, /Working read failed/, "Kin's own words are kept behind Show details");
  assert.equal(browser.main().querySelectorAll("[role=alert] details").length, 1);
  assert.equal(browser.node("workspace-status").dataset.state, "stale");
  assert.equal(browser.node("workspace-status-text").textContent, "Some parts didn't load");
  // The other reads landed: the sidebar and the tabs are drawn and every other page answers.
  for (const route of ["uncommitted", "agents", "explore"]) assert.ok(sideRoute(browser, route), route);
  assert.deepEqual(workTabs(browser).map(([name, , href]) => [name, href]), [["Uncommitted", "#/uncommitted"], ["Branches", "#/branches"], ["History", "#/history"]]);
  browser.navigate("history");
  assert.match(browser.node("changes-results").textContent, /Preserved change/);
  browser.navigate("uncommitted");
  browser.failures.delete("/api/guide");
  browser.button("Try again").click(); await flush();
  assert.doesNotMatch(browser.text(), /Kin couldn't load your uncommitted work/);
  assert.match(browser.node("work-lede").textContent, /One function changed/);
  assert.equal(browser.node("workspace-status").dataset.state, "ready");
  assert.match(browser.node("workspace-status-text").textContent, /^Up to date /);
});

test("initial session failure has a real retry that recovers the page", async () => {
  const browser = fixture({ initialFailure: true }); await flush();
  assert.match(browser.text(), /This repository isn't answering\./);
  assert.match(browser.text(), /Kin can't reach this repository right now\./);
  assert.equal(browser.node("workspace-status").dataset.state, "error");
  browser.failures.clear(); browser.button("Try again").click(); await flush();
  assert.match(browser.text(), /One function changed/);
  assert.equal(browser.node("workspace-status").dataset.state, "ready");

  const hub = fixture({ hub: true, initialFailure: true }); await flush();
  assert.match(hub.text(), /Kin isn't answering\./);
  assert.match(hub.text(), /Kin isn't answering on this machine right now\./);
  hub.failures.clear(); hub.button("Try again").click(); await flush();
  assert.equal(hub.node("home-headline").textContent, "pallets / Fixture needs you.One agent is working. Three repositories on this machine.");
});

test("concurrent refresh clicks share one read cycle", async () => {
  const browser = fixture(); await flush();
  let resolve; browser.held.set("/api/guide", new Promise((done) => { resolve = done; }));
  const before = browser.requests.length;
  browser.refresh();
  const asked = browser.requests.slice(before).map((request) => request.name);
  assert.equal(asked.length, 12, "each read is asked once, including previously unavailable optional capabilities");
  assert.equal(new Set(asked).size, asked.length);
  browser.refresh();
  assert.equal(browser.requests.length - before, asked.length, "a second click starts nothing while the first is reading");
  assert.equal(browser.node("workspace-refresh").disabled, true);
  assert.equal(browser.node("workspace-status-text").textContent, "Loading");
  resolve(); await flush();
  assert.equal(browser.node("workspace-refresh").disabled, false);
});

test("the repository reads as its remote's owner and name, and a repository without one by its name alone", async () => {
  const opened = (browser) => browser.nav().querySelectorAll("a[title]").find((node) => node.getAttribute("href") === "#/uncommitted");
  const browser = fixture();
  await flush();
  assert.equal(opened(browser).getAttribute("title"), "pallets / Fixture");
  assert.equal(opened(browser).textContent, "pallets / Fixturemain", "the sidebar names it and the branch it is on");
  assert.equal(browser.document.title, "Uncommitted · pallets/Fixture");
  browser.navigate("history");
  assert.equal(browser.document.title, "History · pallets/Fixture");

  const local = fixture({ origin: {} });
  await flush();
  assert.equal(opened(local).getAttribute("title"), "Fixture");
  assert.equal(opened(local).textContent, "Fixturemain");
  assert.equal(local.document.title, "Uncommitted · Fixture");
});

test("the list of uncommitted work groups each changed entity by what it asks of the reader", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = working({ ...FOUR, completeness: { state: "pending", message: "", pending_entities: 4 } }, fourChanged());
  await flush();
  assert.equal(browser.node("work-lede").textContent, "Four functions changed. At least ten other places rely on them. Eight were not updated. Check one more. At least one test reaches them.No agent reported writing any of them, so Kin cannot say whether they are yours or an agent's.Kin is still mapping 4 entities, so these counts may grow.");
  // What was left behind first, then what may need a look, then the rest, each numbered in the order a review reads it and drawn in the tone of what it asks.
  const drawn = picture(browser.node("work-flight"));
  assert.deepEqual(drawn.columns[0], { head: "Changed4", more: [], boxes: [
    ["want_bytes", "Function in encoding · Signature changed", "", "#/review/w/1", "bad"],
    ["old_helper", "Function in encoding · Removed", "", "#/review/w/2", "warn"],
    ["quiet", "Function in encoding", "", "#/review/w/3", "quiet"],
    ["base64_encode", "Function in encoding · New", "", "#/review/w/4", "quiet"]] });
  // Each entity that relies on the work and did not change is drawn once, in the order the work reaches it, and the rest are counted.
  assert.deepEqual(drawn.columns[1].boxes.map(([name, , , href]) => [name, href]), [["Serializer.dumps", `#/entity/${uuid(10)}`], ["Signer.sign", `#/entity/${uuid(12)}`], ["Signer.unsign", `#/entity/${uuid(13)}`], ["Signer.derive_key", `#/entity/${uuid(14)}`]],
    "what changed here too is on the left, not the right");
  assert.equal(drawn.columns[1].head, "Rely on them, and did not change≥10");
  assert.deepEqual(drawn.columns[1].more, [["and 6 more", null]]);
  assert.deepEqual(reviewRows(browser.node("work-flight")), []);
  assert.deepEqual(browser.node("work-actions").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Start review", "#/review/w/1"]]);
  assert.deepEqual(sideCount(browser, "uncommitted"), ["4", "4 uncommitted"]);
  assert.doesNotMatch(browser.text(), /\.py|src\//, "no locations");
  // A reading Kin has finished carries no caveat, and one it has not finished without a count says so plainly.
  browser.data["/api/guide"] = working({ ...FOUR, completeness: { state: "settled", message: "Complete" } }, fourChanged());
  browser.refresh(); await flush();
  assert.doesNotMatch(browser.node("work-lede").textContent, /still mapping/);
  browser.data["/api/guide"] = working({ ...FOUR, completeness: { state: "pending", message: "" } }, fourChanged());
  browser.refresh(); await flush();
  assert.match(browser.node("work-lede").textContent, /Kin is still mapping this code, so these counts may grow\.$/);
});

test("more than the picture holds is listed in full under it, grouped by what each entity asks", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = working({ ...FOUR, modified: 7 }, manyChanged());
  await flush();
  const drawn = picture(browser.node("work-flight"));
  assert.equal(drawn.columns[0].head, "Changed9");
  assert.deepEqual(drawn.columns[0].boxes.map(([name, , , href]) => [name, href]), [["want_bytes", "#/review/w/1"], ["old_helper", "#/review/w/2"], ["quiet", "#/review/w/3"], ["base64_encode", "#/review/w/4"],
    ...[0, 1, 2, 3].map((i) => [`extra_${i}`, `#/review/w/${i + 5}`])], "the picture holds eight");
  // The rest are listed under the picture on this same page, so the count leads nowhere else.
  assert.deepEqual(drawn.columns[0].more, [["and 1 more", null]]);
  assert.deepEqual(reviewRows(browser.node("work-flight")), [
    { title: "Relied on, and not updated", count: "1", rows: [{ number: "1", name: "want_bytes", place: "Function in encoding", reason: "Signature changed. 8 entities rely on it and were not updated.", mark: "", href: "#/review/w/1" }] },
    { title: "Relied on by code that did not change", count: "1", rows: [{ number: "2", name: "old_helper", place: "Function in encoding", reason: "Removed. 1 entity relies on it and did not change here.", mark: "", href: "#/review/w/2" }] },
    { title: "No known follow-up", count: "7", rows: [
      { number: "3", name: "quiet", place: "Function in encoding", reason: "1 entity relies on it and needs no update.", mark: "", href: "#/review/w/3" },
      { number: "4", name: "base64_encode", place: "Function in encoding", reason: "New. Kin found no connections outside this change.", mark: "", href: "#/review/w/4" },
      ...[0, 1, 2, 3, 4].map((i) => ({ number: String(i + 5), name: `extra_${i}`, place: "Function in encoding", reason: "Kin found no connections outside this change.", mark: "", href: `#/review/w/${i + 5}` }))] }],
    "every entity is listed below the picture, numbered in the order a review reads it");
});

test("the picture joins each changed entity to what relies on it, in its tone, and stops measuring when it is drawn again", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = working(FOUR, fourChanged());
  await flush();
  const host = () => browser.node("work-flight").querySelector("[role=group]");
  pictureLayout(browser, host);
  await browser.advance(40);
  const drawn = picture(browser.node("work-flight"));
  assert.equal(host().children[0].tagName, "SVG", "the lines sit behind the boxes");
  assert.equal(host().children[0].getAttribute("aria-hidden"), "true");
  // From a changed box's right edge, at its middle, to the left edge of what relies on it, measured inside the picture.
  assert.deepEqual(drawn.lines(), [
    ["M200.0 35.0C350.0 35.0 350.0 35.0 500.0 35.0", "bad"], ["M200.0 35.0C350.0 35.0 350.0 75.0 500.0 75.0", "bad"],
    ["M200.0 75.0C350.0 75.0 350.0 115.0 500.0 115.0", "warn"], ["M200.0 115.0C350.0 115.0 350.0 155.0 500.0 155.0", "quiet"]]);
  const watching = browser.observers.at(-1);
  assert.deepEqual(watching.watched, [host()]);
  assert.equal(host().children[0].getAttribute("data-drawn"), null);
  await browser.advance(900);
  assert.equal(host().children[0].getAttribute("data-drawn"), "", "the lines draw themselves in once");
  // On a narrow window the columns stack, and there is no room for lines.
  pictureLayout(browser, host, { stacked: true });
  watching.redraw();
  assert.deepEqual(drawn.lines(), []);
  browser.data["/api/guide"] = working(FOUR, fourChanged().slice(0, 3));
  browser.refresh(); await flush();
  assert.equal(watching.disconnected, true, "the picture it measured is gone");
});

test("work Kin counted and did not walk says how much there is, and can still be committed", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = guide({ added: 20_000 }, { not_walked: { entities: 20_000 }, working_tree: { differs: true, derived: true, caught_up: true } });
  await flush();
  assert.equal(browser.node("work-lede").textContent, "20,000 entities changed. Kin found no connections outside this change. Kin found no tests that reach this work.");
  assert.equal(browser.node("work-flight").textContent, "This change is too large to read one entity at a time20,000 entities changed. Kin can still show it whole.Read it on one page");
  assert.deepEqual(browser.node("work-flight").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Read it on one page", `${BASE}/review/#/w`]], "the whole change, on the page that reads it at once");
  assert.deepEqual(browser.node("work-actions").querySelectorAll("button").map((node) => node.textContent), ["Commit"]);
  assert.equal(browser.node("work-actions").querySelectorAll("a").length, 0, "there is no walk to start");
  assert.deepEqual(sideCount(browser, "uncommitted"), ["20,000", "20000 uncommitted"]);
});

test("an agent's own activity that meets uncommitted work is said, each kind in its own words", async () => {
  const agent = (key, name, calls) => ({ ...agentsPayload().agents[0], key, name, recent_calls: calls, open_changes: [], changes: [], changes_total: 0 });
  const call = (seq, ago, tool, ids, extra = {}) => ({ seq, at: secondsAgo(ago), tool, entity_ids: ids.map(uuid), result_count: 1, duration_ms: 5, is_error: false, ...extra });
  const steps = () => [changed(2, "want_bytes", { connections: connected([relying(10, "Serializer.dumps")]) }), changed(5, "base64_encode", { change: "added" }), changed(6, "old_helper", { change: "removed" })];
  const summary = { added: 1, modified: 1, removed: 1, recorded_consumers: 1 };
  const agents = agentsPayload({ agents: [
    // Staged an edit to an entity this work changed. It read another before the last edit.
    agent(AGENT_KEY, "Claude Code", [call(9, 10, "kin_transaction_stage", [2]), call(8, 20, "get_entity_source", [2]), call(7, 300, "get_entity_source", [5])]),
    // Changed something that relies on a changed entity. Its latest read of one entity came after the edit, of another before it.
    agent("00000000000000b2", "Lantern", [call(5, 15, "kin_mutate", [10]), call(4, 30, "find_references", [5]), call(3, 200, "impact_analysis", [5]), call(2, 400, "get_entity_source", [6]), call(1, 500, "get_entity_source", [2], { is_error: true })]),
  ] });
  const browser = fixture();
  // Kin says the work last changed a minute ago.
  browser.data["/api/guide"] = working(summary, steps(), { edited: 60 });
  browser.data["/api/workspace/agents"] = agents;
  await flush();
  const section = browser.node("work-overlaps");
  assert.match(section.querySelector("h2").textContent, /^Overlaps with agents4$/);
  const said = section.querySelectorAll("li").map((row) => row.querySelector("p").textContent);
  assert.deepEqual(said, [
    "Claude Code changed want_bytes, which your uncommitted work also changed.",
    "Lantern changed Serializer.dumps, which relies on want_bytes in your uncommitted work.",
    "Claude Code last read base64_encode before your latest edit to this work, so what it saw may be out of date.",
    "Lantern last read old_helper before your latest edit to this work, so what it saw may be out of date."]);
  assert.deepEqual(section.querySelectorAll("li").map((row) => row.querySelector("time").textContent), ["just now", "just now", "5 minutes ago", "7 minutes ago"]);
  const hrefs = (row) => section.querySelectorAll("li")[row].querySelectorAll("a").map((node) => node.getAttribute("href"));
  assert.deepEqual(hrefs(0), [`#/agents/${AGENT_KEY}`, `#/entity/${uuid(2)}`]);
  assert.deepEqual(hrefs(1), ["#/agents/00000000000000b2", `#/entity/${uuid(10)}`, `#/entity/${uuid(2)}`]);
  assert.deepEqual(hrefs(3), ["#/agents/00000000000000b2"], "a removed entity has no page to open");

  // Kin does not always say when the work last changed. The time it read the work is no such time, so nothing is called out of date.
  const untimed = fixture();
  untimed.data["/api/guide"] = working(summary, steps());
  untimed.data["/api/workspace/agents"] = agents;
  await flush();
  assert.deepEqual(untimed.node("work-overlaps").querySelectorAll("li").map((row) => row.querySelector("p").textContent), [
    "Claude Code changed want_bytes, which your uncommitted work also changed.",
    "Lantern changed Serializer.dumps, which relies on want_bytes in your uncommitted work."]);

  // An agent that read after the latest edit saw what is there, so nothing is said.
  const current = fixture();
  current.data["/api/guide"] = working({ modified: 1 }, [changed(2, "want_bytes")], { edited: 60 });
  current.data["/api/workspace/agents"] = agentsPayload({ agents: [agent(AGENT_KEY, "Claude Code", [call(3, 20, "get_entity_source", [2]), call(2, 300, "get_entity_source", [2]), call(1, 30, "get_entity_source", [9])])] });
  await flush();
  assert.equal(current.node("work-overlaps").textContent, "");
  // Its reading is out of date again once the work changes after it.
  current.data["/api/guide"] = working({ modified: 1 }, [changed(2, "want_bytes")], { edited: 5 });
  current.refresh(); await flush();
  assert.match(current.node("work-overlaps").textContent, /Claude Code last read want_bytes before your latest edit/);
});

test("the last uncommitted-work reading is shown as of its time until the fresh one lands, and Commit waits for it", async () => {
  const names = (browser) => picture(browser.node("work-flight"))?.columns[0].boxes.map(([name]) => name) ?? [];
  const tabStorage = new Map();
  const remembered = working({ modified: 1, recorded_consumers: 1 }, [changed(2, "want_bytes")]);
  tabStorage.set(workingKey, JSON.stringify({ at: START - 90_000, data: remembered }));
  const browser = fixture({ tabStorage });
  let release; browser.held.set("/api/guide", new Promise((done) => { release = done; }));
  browser.data["/api/guide"] = working({ modified: 2, recorded_consumers: 3, consumers_not_updated: 1, recorded_tests: 1 }, [changed(2, "want_bytes"), changed(5, "base64_encode")]);
  await flush();
  assert.equal(browser.node("work-lede").textContent, "One function changed. One other place relies on it. It did not change. Kin found no tests that reach this work.No agent reported writing it, so Kin cannot say whether it is yours or an agent's.", "what this tab last saw");
  assert.match(browser.node("work-flight").textContent, /^As of .+\. Refreshing/);
  assert.deepEqual(names(browser), ["want_bytes"]);
  assert.equal(browser.button("Commit"), undefined, "a commit acts on what is there now, so it is not offered on an old reading");
  assert.ok(browser.links().some((node) => node.textContent === "Start review"), "what was last seen can still be read");
  release(); await flush();
  assert.equal(browser.node("work-lede").textContent, "Two functions changed. Three other places rely on them. One was not updated. One test reaches them.No agent reported writing any of them, so Kin cannot say whether they are yours or an agent's.");
  assert.doesNotMatch(browser.node("work-flight").textContent, /Refreshing/);
  assert.deepEqual(names(browser), ["want_bytes", "base64_encode"]);
  assert.ok(browser.button("Commit"), "the fresh reading can be committed");
  const kept = JSON.parse(tabStorage.get(workingKey));
  assert.equal(kept.at, START, "the fresh reading replaces the remembered one");
  assert.equal(kept.data.summary.modified, 2);

  // A fresh reading equal to the remembered one still clears the mark, and Commit comes back.
  const same = fixture({ tabStorage: new Map([[workingKey, JSON.stringify({ at: START - 90_000, data: remembered })]]) });
  let land; same.held.set("/api/guide", new Promise((done) => { land = done; }));
  same.data["/api/guide"] = remembered;
  await flush();
  assert.match(same.node("work-flight").textContent, /Refreshing/);
  land(); await flush();
  assert.doesNotMatch(same.node("work-flight").textContent, /Refreshing/);
  assert.ok(same.button("Commit"));

  // Another repository's reading, or one that is not a reading, is never shown.
  const other = new Map([[`kin.workspace.working:/repositories/${NOTES}`, JSON.stringify({ at: START - 90_000, data: remembered })], [`${workingKey}:old`, "not json"]]);
  const elsewhere = fixture({ tabStorage: other });
  let hold; elsewhere.held.set("/api/guide", new Promise((done) => { hold = done; }));
  await flush();
  assert.deepEqual(names(elsewhere), []);
  assert.doesNotMatch(elsewhere.node("work-flight").textContent, /Refreshing/);
  hold(); await flush();
  const unreadable = fixture({ tabStorage: new Map([[workingKey, JSON.stringify({ at: START - 1, data: { chapters: [] } })]]) });
  let wait; unreadable.held.set("/api/guide", new Promise((done) => { wait = done; }));
  await flush();
  assert.doesNotMatch(unreadable.node("work-flight").textContent, /Refreshing/);
  wait(); await flush();
});

test("Commit sends the message once, says what was committed and reads the page again", async () => {
  const browser = fixture(); await flush();
  const change = "d".repeat(64);
  browser.data["/api/workspace/commit"] = (url, options) => {
    const body = JSON.parse(options.body);
    browser.data["/api/guide"] = guide();
    return { operation_id: body.operation_id, outcome: "committed", change_id: change, title: body.message.split("\n")[0] };
  };
  browser.button("Commit").click();
  assert.equal(browser.document.activeElement.id, "commit-message");
  assert.equal(browser.button("Commit").id, "commit-submit", "the form's own button is the only Commit left");
  assert.equal(commitForm(browser).submit.disabled, true, "a commit needs a message");
  const hint = () => browser.node("work-commit").querySelectorAll("span").find((node) => node.textContent === "A message is needed to commit.");
  assert.equal(hint().hidden, false, "and the form says so while it has none");
  browser.type("commit-message", "   ");
  assert.equal(hint().hidden, false);
  assert.equal(commitForm(browser).submit.disabled, true);
  commitForm(browser).field.emit("keydown", { key: "Enter", metaKey: true });
  assert.equal(browser.count("/api/workspace/commit"), 0, "a blank message sends nothing");
  browser.type("commit-message", "  Bound input before decoding\n\nIt was unbounded.  ");
  assert.equal(commitForm(browser).submit.disabled, false);
  assert.equal(hint().hidden, true);
  const reads = { guide: browser.count("/api/guide"), changes: browser.count("/api/changes"), standing: browser.count("/api/standing"), branches: browser.count("/api/branches") };
  commitForm(browser).submit.click();
  assert.equal(commitForm(browser).submit.disabled, true);
  assert.equal(commitForm(browser).field.disabled, true);
  commitForm(browser).submit.click(); browser.refresh();
  await flush();
  const calls = browser.requests.filter((request) => request.name === "/api/workspace/commit");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "POST");
  assert.equal(calls[0].options.headers["x-kin-review-token"], "synthetic-token");
  assert.deepEqual(JSON.parse(calls[0].options.body), { message: "Bound input before decoding\n\nIt was unbounded.", operation_id: uuid(1) });
  const said = browser.node("work-commit");
  assert.match(said.textContent, /^Committedwant_bytes is committed as change dddddddd, “Bound input before decoding”\./);
  assert.equal(said.querySelectorAll("a").find((node) => node.textContent === "Review it").getAttribute("href"), guided(change));
  assert.equal(commitForm(browser).field, null, "the form is gone");
  assert.equal(browser.count("/api/guide"), reads.guide + 1);
  assert.equal(browser.count("/api/changes"), reads.changes + 1);
  assert.equal(browser.count("/api/standing"), reads.standing + 1);
  assert.equal(browser.count("/api/branches"), reads.branches + 1);
  assert.equal(browser.node("work-lede").textContent, "Nothing in flight. Your code matches your last commit.");
  assert.match(browser.node("work-flight").textContent, /^Nothing is uncommitted/);
  browser.button("Dismiss").click();
  assert.equal(said.textContent, "");
});

test("Commit is offered only to a session that may commit", async () => {
  for (const options of [{ capabilities: { commit: false } }, { session: { token: "" } }]) {
    const browser = fixture(options); await flush();
    assert.equal(browser.button("Commit"), undefined);
    assert.ok(browser.links().some((node) => node.textContent === "Start review"), "the work can still be reviewed");
  }
});

test("a commit that finds nothing to commit says so, and the work that is still there can be committed again", async () => {
  const browser = fixture(); await flush();
  browser.data["/api/workspace/commit"] = (url, options) => ({ operation_id: JSON.parse(options.body).operation_id, outcome: "nothing_to_commit" });
  const reads = browser.count("/api/guide");
  writeCommit(browser, "Bound input"); await flush();
  assert.match(browser.node("work-commit").textContent, /^Nothing to commitYour code already matches your last commit\./);
  assert.equal(browser.node("work-commit").querySelectorAll("a").length, 0, "there is no change to review");
  assert.equal(browser.count("/api/workspace/commit"), 1);
  // The work was read again. The reading is the same as before, and the page still stops calling it an old one.
  assert.equal(browser.count("/api/guide"), reads + 1);
  assert.doesNotMatch(browser.node("work-flight").textContent, /Refreshing/);
  browser.button("Dismiss").click();
  assert.ok(browser.node("work-actions").querySelectorAll("button").some((node) => node.textContent === "Commit"));
});

test("a refused commit wrote nothing, keeps the message and can be sent again", async () => {
  for (const refusal of [
    { __status: 409, error: "The projection is blocked.", body: { outcome: "refused", remedy: "Nothing was committed. Resolve what is reported, then commit again." } },
    { __status: 403, error: "This session may not write." },
  ]) {
    const browser = fixture(); await flush();
    browser.data["/api/workspace/commit"] = refusal;
    const reads = browser.count("/api/guide");
    writeCommit(browser, "Bound input"); await flush();
    const said = browser.node("work-commit").textContent;
    assert.match(said, /The commit was not made/);
    assert.match(said, refusal.body ? /Nothing was committed\. Resolve what is reported, then commit again\..*Show details.*The projection is blocked\./s : /This session may not write\./);
    assert.doesNotMatch(said, /outcome is unknown/);
    assert.equal(commitForm(browser).field.textContent, "Bound input", "the message is kept");
    assert.equal(commitForm(browser).submit.disabled, false);
    assert.equal(browser.count("/api/guide"), reads, "nothing changed, so nothing is read again");
    commitForm(browser).submit.click(); await flush();
    const calls = browser.sent("/api/workspace/commit");
    assert.equal(calls.length, 2);
    assert.notEqual(calls[0].operation_id, calls[1].operation_id, "a new attempt is a new operation");
  }
});

test("a commit whose outcome is unknown is never sent again", async () => {
  for (const lost of [
    { __status: 502, error: "The commit did not finish.", body: { outcome: "unknown", remedy: "The commit may have landed." } },
    { __status: 409, error: "No definite outcome returned." },
    new Error("Connection closed after request"),
  ]) {
    const browser = fixture(); await flush();
    if (lost instanceof Error) browser.failures.set("/api/workspace/commit", lost); else browser.data["/api/workspace/commit"] = lost;
    writeCommit(browser, "Bound input"); await flush();
    const said = browser.node("work-commit");
    assert.match(said.textContent, /The commit's outcome is unknown/);
    assert.match(said.textContent, /This page will not send it again\./);
    assert.match(said.textContent, new RegExp(`Operation ${uuid(1)}`), "the reader can name the request");
    assert.ok(said.querySelector("[role=alert]"), "it is announced as a problem");
    assert.equal(commitForm(browser).field, null, "there is no form to send");
    assert.equal(commitForm(browser).submit, null);
    assert.equal(browser.node("work-actions").querySelectorAll("button").length, 0, "and no Commit beside the title");
    // Reading again, leaving and coming back, or a live pulse cannot send it.
    browser.refresh(); await flush();
    browser.navigate("history"); browser.navigate("uncommitted");
    browser.api.pulse(["working", "changes"]); await browser.advance(600);
    assert.match(browser.node("work-commit").textContent, /The commit's outcome is unknown/);
    assert.equal(browser.count("/api/workspace/commit"), 1);
    // Only the reader, having checked, gets the form back, with the message still there to decide on.
    browser.button("I've checked").click();
    assert.equal(commitForm(browser).field.textContent, "Bound input");
    assert.equal(browser.count("/api/workspace/commit"), 1);
  }
});

test("a commit whose outcome is unknown sends the reader to the page that lists changes by its name", async () => {
  const browser = fixture(); await flush();
  browser.failures.set("/api/workspace/commit", new Error("Connection closed after request"));
  writeCommit(browser, "Bound input"); await flush();
  assert.match(browser.node("work-commit").textContent, /The commit's outcome is unknown/);
  assert.match(browser.node("work-commit").textContent, /Check History to see whether it landed\./);
  assert.deepEqual(workTabs(browser).map(([name]) => name).filter((name) => name === "History"), ["History"], "and History is where it says");
});

// ── Review ─────────────────────────────────────────────────────────────────

/** A browser on uncommitted work with four changed entities, at an address of its review. */
async function reviewing(address, options = {}) {
  const browser = fixture({ hash: address, ...options });
  browser.data["/api/guide"] = working(FOUR, fourChanged());
  await flush();
  return browser;
}
/** A browser on a recorded change with the same four entities. */
async function reviewingChange(address, options = {}) {
  const browser = fixture({ hash: address, ...options });
  browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), FOUR, fourChanged());
  await flush();
  return browser;
}
/** One page of a review, as a reader meets it: where it sits in the walk, how it was marked, and the entity's parts. */
function stepOf(browser) {
  const body = browser.node("review-body");
  const [position, , marked] = body.children[0].children;
  const article = body.querySelector("article");
  const [, place, verdict] = article.querySelector("header").children;
  const part = (id) => article.querySelector(`#${id}`).parentNode;
  return { position: position.textContent, marked: marked?.textContent ?? "", article, place: place.textContent, verdict: verdict.textContent,
    source: part("step-source-title"), relies: part("step-relies-title"), tests: part("step-tests-title"), rows: (section) => section.querySelectorAll("li").map((row) => row.textContent) };
}
/** Mark the entity on the page with one of the two buttons, and let the page move on. */
async function mark(browser, ...kinds) { for (const kind of kinds) { browser.node(kind === "flag" ? "review-flag" : "review-fine").click(); await flush(); } }
/** Flag the entity on the page with a note saying why. */
async function flagWith(browser, note) { browser.type("review-note", note); await mark(browser, "flag"); }
/** What a control is called: its words, without the key drawn beside them for the eye. */
const named = (node) => node.children.filter((child) => child.getAttribute("aria-hidden") !== "true").map((child) => child.textContent).join("");
const marksOf = (kept, subject = "w:change-1") => kept.has(marksKey(subject)) ? JSON.parse(kept.get(marksKey(subject))) : null;
const finishTitle = (browser) => browser.node("review-body").querySelector("h1").textContent;
/** Each changed entity in a list and how it was marked, from the rows under a picture or, where the picture is the list, from its boxes. */
const listMarks = (host) => { const rows = reviewRows(host).flatMap((group) => group.rows.map((row) => [row.name, row.mark])); return rows.length ? rows : picture(host)?.columns[0].boxes.map(([name, , mark]) => [name, mark]) ?? []; };
const reviewAction = (host) => host.querySelectorAll("a").filter((node) => /review$/.test(node.textContent)).map((node) => [node.textContent, node.getAttribute("href")]);
/** The four as a review's picture draws them, at a review's address. */
const FOUR_BOXES = (base) => [
  ["want_bytes", "Function in encoding · Signature changed", "", `${base}/1`, "bad"], ["old_helper", "Function in encoding · Removed", "", `${base}/2`, "warn"],
  ["quiet", "Function in encoding", "", `${base}/3`, "quiet"], ["base64_encode", "Function in encoding · New", "", `${base}/4`, "quiet"]];

test("a change opens on the list its review walks: what asks the most first, whatever order Kin listed it in", async () => {
  const browser = await reviewingChange("#/review/c/change-1");
  const head = browser.node("review-head");
  assert.equal(head.querySelector("h1").textContent, "Preserved change");
  assert.deepEqual(head.querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["History", "#/history"], ["Start review", "#/review/c/change-1/1"]]);
  assert.match(head.textContent, /Four functions changed\. Ten other places rely on them\. Eight were not updated\. Check one more\. One test reaches them\./);
  assert.match(head.textContent, /Dana Reader, 1 hour ago$/, "who made it, by name, and when");
  assert.deepEqual(picture(browser.node("review-body")).columns[0], { head: "Changed4", boxes: FOUR_BOXES("#/review/c/change-1"), more: [] });
  assert.equal(picture(browser.node("review-body")).columns[1].head, "Rely on them, and did not change10");
  assert.equal(browser.document.title, "Review · pallets/Fixture");
  assert.deepEqual(sideCurrent(browser), ["uncommitted"], "a change is part of Work");
  assert.doesNotMatch(browser.text(), /\.py|src\/|<dana@|1749933342/, "no locations, and a signature reads as a name");
  // A change on record never changes, so it is read once however its pages are moved between.
  const reads = (id) => browser.requests.filter((request) => request.name === "/api/guide" && new URL(request.url, "http://local.test").searchParams.get("change") === id).length;
  for (const page of ["review/c/change-1/1", "review/c/change-1/done", "review/c/change-1"]) { browser.navigate(page); await flush(); }
  assert.equal(reads("change-1"), 1);
  browser.navigate("review/c/change-2"); await flush();
  assert.equal(reads("change-2"), 1, "another change is read for itself");
  assert.equal(browser.node("review-head").querySelectorAll("a").at(-1).getAttribute("href"), "#/review/c/change-2/1");
});

test("each step shows what happened to one entity, its source, what relies on it with a state for each, and the tests that reach it", async () => {
  const browser = await reviewing("#/review/w/1");
  let step = stepOf(browser);
  assert.equal(step.position, "1 of 4");
  assert.equal(step.marked, "", "nothing says it was looked at before it was");
  assert.equal(browser.node("review-previous"), null, "nothing comes before the first");
  assert.deepEqual([named(browser.node("review-next")), browser.node("review-next").getAttribute("href")], ["Skip", "#/review/w/2"]);
  // Each control shows the key that presses it, and the two that mark say so to a screen reader as well.
  assert.deepEqual(["review-next", "review-flag", "review-fine"].map((id) => [named(browser.node(id)), browser.node(id).querySelector("kbd").textContent, browser.node(id).getAttribute("aria-keyshortcuts")]),
    [["Skip", "→", null], ["Flag it", "F", "f"], ["Looks right", "↵", "Enter"]]);
  assert.equal(step.article.getAttribute("aria-label"), "want_bytes");
  assert.equal(step.article.querySelector("h1").querySelector("a").getAttribute("href"), `#/entity/${uuid(2)}`, "its name opens its own page");
  assert.equal(step.place, "Function in encoding · No agent reported this edit");
  // One sentence says its counts one way: as numerals.
  assert.equal(step.verdict, "Its signature and its body changed. 8 entities rely on it and were not updated. 1 test reaches it.");
  assert.match(step.source.textContent, /^Before and after/);
  assert.deepEqual(diffRows(step.source), [["12", "", "−", "def want_bytes(s):"], ["13", "", "−", "    return s.encode()"], ["", "12", "+", "def want_bytes(s, encoding):"], ["", "13", "+", "    return s.encode(encoding)"]]);
  assert.equal(step.relies.querySelector("h2").textContent, "What relies on it9", "the count is the guide's, not the boxes drawn");
  // The entity, joined to each thing that relies on it, each in the tone of where that thing stands.
  const relies = picture(step.relies);
  assert.equal(relies.label, "want_bytes and 9 entities that rely on it");
  assert.deepEqual(relies.columns, [
    { head: null, boxes: [["want_bytes", "Changed here", "", null, ""]], more: [] },
    { head: null, boxes: [
      ["Serializer.dumps", "calls it", "Not updated", `#/entity/${uuid(10)}`, "bad"], ["Serializer.loads", "calls it", "Changed here too", `#/entity/${uuid(11)}`, "good"],
      ["Signer.sign", "refers to it", "Not updated", `#/entity/${uuid(12)}`, "bad"]], more: [["and 6 more that Kin counted and did not list here", null]] }]);
  assert.equal(step.tests.querySelector("h2").textContent, "Tests that reach it1");
  assert.deepEqual(step.rows(step.tests), ["TestEncoding.test_want_bytes1 step away"], "a test the guide gives no distance for is a direct one");
  assert.deepEqual(browser.node("review-head").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Uncommitted", "#/uncommitted"]], "the way back to the list");
  assert.deepEqual(sideCurrent(browser), ["uncommitted"], "a review of uncommitted work is part of Uncommitted");

  browser.navigate("review/w/2"); await flush();
  step = stepOf(browser);
  assert.equal(step.position, "2 of 4");
  assert.deepEqual([named(browser.node("review-previous")), browser.node("review-previous").querySelector("kbd").textContent, browser.node("review-previous").getAttribute("href")], ["Previous", "←", "#/review/w/1"]);
  assert.equal(browser.node("review-next").getAttribute("href"), "#/review/w/3");
  assert.equal(step.article.getAttribute("aria-label"), "old_helper");
  assert.equal(step.article.querySelector("h1").querySelectorAll("a").length, 0, "an entity that is gone has no page to open");
  assert.equal(step.verdict, "This function was removed. 1 entity relies on it and did not change here.");
  assert.match(step.source.textContent, /^Removed source/);
  assert.deepEqual(diffRows(step.source), [["50", "", "−", "def old_helper():"], ["51", "", "−", "    pass"]]);
  assert.deepEqual(picture(step.relies).columns.map((column) => column.boxes), [[["old_helper", "Removed here", "", null, ""]], [["Signer.unsign", "calls it", "May need review", `#/entity/${uuid(13)}`, "warn"]]]);
  assert.equal(step.tests.textContent, "Tests that reach itKin found no tests that reach it.");

  browser.navigate("review/w/3"); await flush();
  step = stepOf(browser);
  assert.equal(step.verdict, "Its body changed. Its signature did not. 1 entity relies on it and needs no update. Kin found no tests that reach it.");
  assert.deepEqual(picture(step.relies).columns[1].boxes, [["Signer.derive_key", "calls it", "Needs no update", `#/entity/${uuid(14)}`, "quiet"]]);
  assert.equal(step.source.textContent, "Before and afterNo source was returned for this entity.");

  browser.navigate("review/w/4"); await flush();
  step = stepOf(browser);
  assert.equal(step.verdict, "New function. Kin found no connections outside this change. Kin found no tests that reach it.");
  assert.match(step.source.textContent, /^New source/);
  assert.deepEqual(diffRows(step.source), [["", "30", "+", "def base64_encode(s):"], ["", "31", "+", "    return s"]]);
  assert.equal(step.relies.textContent, "What relies on it0Kin found no entities that rely on it.");
  assert.deepEqual([named(browser.node("review-next")), browser.node("review-next").getAttribute("href")], ["Skip", "#/review/w/done"], "after the last comes the finish");
  assert.doesNotMatch(browser.text(), /\.py|src\//, "no locations");

  // While Kin is still mapping the code, a test it has not found is not yet a test that does not exist.
  const mapping = fixture({ hash: "#/review/w/1" });
  mapping.data["/api/guide"] = working({ modified: 1, completeness: { state: "pending", message: "", pending_entities: 4 } }, [changed(2, "want_bytes")]);
  await flush();
  assert.equal(stepOf(mapping).tests.textContent, "Tests that reach itKin found no tests that reach it.");
  assert.equal(stepOf(mapping).relies.textContent, "What relies on it0Kin found no entities that rely on it.");
});

test("a changed entity's source reads as kept, removed and added lines, numbered from where each version starts", async () => {
  const browser = fixture({ hash: "#/review/w/1" });
  const body = (lines) => `${lines.join("\n")}\n`;
  const table = (n, word, changedAt = -1) => body(Array.from({ length: n }, (_, i) => i === changedAt ? `${word}_${i} = ${i}000` : `${word}_${i} = ${i}`));
  browser.data["/api/guide"] = working({ modified: 3 }, [
    changed(2, "want_bytes", { before: { state: "recorded", text: body(["def want_bytes(s):", "    x = 1", "", "    return s"]), start_line: 12 }, after: { state: "recorded", text: body(["def want_bytes(s):", "    x = 2", "    y = 3", "", "    return s"]), start_line: 20, working: true } }),
    changed(5, "long_table", { before: { state: "recorded", text: table(41, "row"), start_line: 100 }, after: { state: "recorded", text: table(41, "row", 20), start_line: 100, working: true } }),
    changed(6, "huge_table", { before: { state: "recorded", text: table(700, "old"), start_line: 1 }, after: { state: "recorded", text: table(700, "new"), start_line: 1, working: true } }),
  ]);
  await flush();
  const source = () => stepOf(browser).source;
  assert.equal(source().querySelector("[role=region]").getAttribute("aria-label"), "Source, 2 lines added and 1 line removed");
  assert.deepEqual(diffRows(source()), [
    ["12", "20", " ", "def want_bytes(s):"],
    ["13", "", "−", "    x = 1"],
    ["", "21", "+", "    x = 2"],
    ["", "22", "+", "    y = 3"],
    ["14", "23", " ", " "],
    ["15", "24", " ", "    return s"]]);
  // A long body shows its change with three kept lines either side, and folds the rest until asked.
  browser.navigate("review/w/2"); await flush();
  assert.deepEqual(diffRows(source()), [
    "17 unchanged lines",
    ["117", "117", " ", "row_17 = 17"], ["118", "118", " ", "row_18 = 18"], ["119", "119", " ", "row_19 = 19"],
    ["120", "", "−", "row_20 = 20"], ["", "120", "+", "row_20 = 20000"],
    ["121", "121", " ", "row_21 = 21"], ["122", "122", " ", "row_22 = 22"], ["123", "123", " ", "row_23 = 23"],
    "17 unchanged lines"]);
  source().querySelector("[role=region]").querySelector("button").click();
  const opened = diffRows(source());
  assert.equal(opened.length, 42);
  assert.deepEqual(opened[0], ["100", "100", " ", "row_0 = 0"]);
  assert.ok(opened.every((row) => Array.isArray(row)), "nothing stays folded");
  // A body too long to compare line by line is shown as it is now, and all of it can be read.
  browser.navigate("review/w/3"); await flush();
  assert.match(source().textContent, /new_159 = 159/);
  assert.doesNotMatch(source().textContent, /new_160 = 160|old_0 = 0/);
  source().querySelectorAll("button").find((node) => node.textContent === "Show 540 more lines").click();
  assert.match(source().textContent, /new_699 = 699/, "asking for the rest shows it");
  assert.equal(source().querySelectorAll("button").length, 0);
});

test("Looks right and Flag it mark the entity and move on, and what was marked is kept on this machine", async () => {
  const kept = new Map();
  const browser = await reviewing("#/review/w/1", { kept });
  await mark(browser, "fine");
  assert.equal(browser.location.hash, "#/review/w/2");
  assert.equal(stepOf(browser).position, "2 of 4");
  await mark(browser, "flag");
  assert.equal(browser.location.hash, "#/review/w/3");
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`], flagged: [`${uuid(6)}:a-6`], notes: {} }, "by the entity and the shape it had, under the base this work stands on");
  // A step says how it was marked, and marking it again replaces the mark.
  browser.navigate("review/w/1"); await flush();
  assert.equal(stepOf(browser).marked, "Looks right");
  browser.navigate("review/w/3"); await flush();
  assert.equal(stepOf(browser).marked, "");
  browser.navigate("review/w/2"); await flush();
  assert.equal(stepOf(browser).marked, "Flagged");
  await mark(browser, "fine");
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`, `${uuid(6)}:a-6`], flagged: [], notes: {} });
  browser.navigate("review/w/2"); await flush();
  await mark(browser, "flag");
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`], flagged: [`${uuid(6)}:a-6`], notes: {} });
  // The list shows the marks, and its action picks up at the first entity not looked at.
  browser.navigate("uncommitted");
  assert.deepEqual(listMarks(browser.node("work-flight")), [["want_bytes", "Looks right"], ["old_helper", "Flagged"], ["quiet", ""], ["base64_encode", ""]]);
  assert.deepEqual(reviewAction(browser.node("work-actions")), [["Continue review", "#/review/w/3"]]);

  // A reload finds them again.
  const reloaded = await reviewing("#/uncommitted", { kept });
  assert.deepEqual(listMarks(reloaded.node("work-flight")), [["want_bytes", "Looks right"], ["old_helper", "Flagged"], ["quiet", ""], ["base64_encode", ""]]);
  assert.deepEqual(reviewAction(reloaded.node("work-actions")), [["Continue review", "#/review/w/3"]]);
  reloaded.navigate("review/w/3"); await flush();
  await mark(reloaded, "fine", "fine");
  assert.equal(reloaded.location.hash, "#/review/w/done");
  reloaded.navigate("uncommitted");
  assert.deepEqual(reviewAction(reloaded.node("work-actions")), [["Finish review", "#/review/w/done"]], "with each one looked at, what is left is to finish");

  // An entity that changed again since it was marked is new again.
  const again = fixture({ kept });
  again.data["/api/guide"] = working(FOUR, fourChanged().map((step) => step.name === "want_bytes" ? { ...step, evidence: { head_change: "WORKSPACE", fingerprint_after: { signature: "s-2", behavior: "b-2", ast: "a-2-again" } } } : step));
  await flush();
  assert.deepEqual(listMarks(again.node("work-flight")), [["want_bytes", ""], ["old_helper", "Flagged"], ["quiet", "Looks right"], ["base64_encode", "Looks right"]]);
  assert.deepEqual(reviewAction(again.node("work-actions")), [["Continue review", "#/review/w/1"]]);
  // Work that stands on another change, and a recorded change, each keep marks of their own.
  const later = fixture({ kept });
  later.data["/api/guide"] = { ...working(FOUR, fourChanged()), change: { id: "WORKSPACE", base: "change-9", title: "Uncommitted work", timestamp: secondsAgo(0) } };
  await flush();
  assert.deepEqual(listMarks(later.node("work-flight")).map(([, said]) => said), ["", "", "", ""]);
  assert.deepEqual(reviewAction(later.node("work-actions")), [["Start review", "#/review/w/1"]]);
  const change = await reviewingChange("#/review/c/change-1/1", { kept });
  await mark(change, "flag");
  assert.deepEqual(marksOf(kept, "change-1"), { fine: [], flagged: [`${uuid(2)}:a-2`], notes: {} });
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`, `${uuid(7)}:a-7`, `${uuid(5)}:a-5`], flagged: [`${uuid(6)}:a-6`], notes: {} }, "the work's own marks are as they were");
  change.navigate("review/c/change-1"); await flush();
  assert.deepEqual(listMarks(change.node("review-body")), [["want_bytes", "Flagged"], ["old_helper", ""], ["quiet", ""], ["base64_encode", ""]]);
  assert.deepEqual(reviewAction(change.node("review-head")), [["Continue review", "#/review/c/change-1/2"]]);
});

test("a flag can say why, and the reason stays with the flag until the entity is marked again", async () => {
  const kept = new Map();
  const browser = await reviewing("#/review/w/1", { kept });
  assert.equal(browser.node("review-note").value, "");
  await flagWith(browser, "  The callers still pass one argument.  ");
  assert.deepEqual(marksOf(kept), { fine: [], flagged: [`${uuid(2)}:a-2`], notes: { [`${uuid(2)}:a-2`]: "The callers still pass one argument." } });
  // A note typed and then not flagged is not kept: it is a reason for a flag.
  browser.type("review-note", "Never mind.");
  await mark(browser, "fine");
  assert.deepEqual(marksOf(kept).notes, { [`${uuid(2)}:a-2`]: "The callers still pass one argument." });
  // Coming back to the step finds the reason, ready to change.
  browser.navigate("review/w/1"); await flush();
  assert.equal(browser.node("review-note").value, "The callers still pass one argument.");
  assert.equal(stepOf(browser).marked, "Flagged");
  // The finish page says the reason in place of what the entity asks, and the copied flags carry it.
  browser.navigate("review/w/3"); await flush();
  await mark(browser, "fine", "flag");
  assert.equal(finishTitle(browser), "Two look right, two flagged");
  const flagged = reviewRows(browser.node("review-body")).find((group) => group.title === "Flagged");
  assert.deepEqual(flagged.rows.map((row) => [row.number, row.name, row.place, row.reason, row.mark, row.href]), [
    ["1", "want_bytes", "Function in encoding", "The callers still pass one argument.", "Flagged", "#/review/w/1"],
    ["4", "base64_encode", "Function in encoding", "New. Kin found no connections outside this change.", "Flagged", "#/review/w/4"]], "each leads back to its own page");
  browser.button("Copy the flags").click(); await flush();
  assert.deepEqual(browser.copied, ["want_bytes (Function in encoding): The callers still pass one argument. Its signature and its body changed. 8 entities rely on it and were not updated.\nbase64_encode (Function in encoding): New function. Kin found no connections outside this change."]);
  // Marking it as looking right drops the flag and its reason. A long reason is cut to what a row can hold.
  browser.navigate("review/w/1"); await flush();
  await mark(browser, "fine");
  assert.deepEqual(marksOf(kept).notes, {});
  browser.navigate("review/w/1"); await flush();
  assert.equal(browser.node("review-note").value, "");
  await flagWith(browser, "x".repeat(500));
  assert.equal(marksOf(kept).notes[`${uuid(2)}:a-2`].length, 400);
  // A reload keeps the reason with the flag.
  const reloaded = await reviewing("#/review/w/4", { kept });
  assert.equal(reloaded.node("review-note").value, "");
  await flagWith(reloaded, "Kin found no tests that reach this work.");
  reloaded.navigate("review/w/4"); await flush();
  assert.equal(reloaded.node("review-note").value, "Kin found no tests that reach this work.");
});

test("on a step the arrow keys move, Enter says it looks right and f flags it", async () => {
  const kept = new Map();
  const browser = await reviewing("#/review/w/2", { kept });
  const press = async (key, extra = {}) => { const event = browser.document.emit("keydown", { key, ...extra }); await flush(); return event; };
  await press("ArrowRight");
  assert.equal(browser.location.hash, "#/review/w/3");
  await press("ArrowLeft"); await press("ArrowLeft");
  assert.equal(browser.location.hash, "#/review/w/1");
  await press("ArrowLeft");
  assert.equal(browser.location.hash, "#/review/w/1", "nothing comes before the first");
  assert.equal(marksOf(kept), null, "moving marks nothing");
  assert.equal((await press("Enter")).defaultPrevented, true);
  assert.equal(browser.location.hash, "#/review/w/2");
  await press("f");
  assert.equal(browser.location.hash, "#/review/w/3");
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`], flagged: [`${uuid(6)}:a-6`], notes: {} });
  // A key held with another, or typed into the note, is not the review's.
  for (const held of [{ ctrlKey: true }, { metaKey: true }, { altKey: true }]) { await press("Enter", held); await press("f", held); await press("ArrowRight", held); }
  for (const key of ["Enter", "f", "ArrowRight"]) assert.equal((await press(key, { target: browser.node("review-note") })).defaultPrevented, false);
  assert.equal(browser.location.hash, "#/review/w/3");
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`], flagged: [`${uuid(6)}:a-6`], notes: {} });
  // Off a step the keys do nothing.
  browser.navigate("review/w/done"); await flush();
  for (const key of ["Enter", "f", "ArrowRight", "ArrowLeft"]) await press(key);
  assert.equal(browser.location.hash, "#/review/w/done");
  browser.navigate("uncommitted");
  for (const key of ["Enter", "f", "ArrowRight"]) await press(key);
  assert.equal(browser.location.hash, "#/uncommitted");
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`], flagged: [`${uuid(6)}:a-6`], notes: {} });
});

test("Enter on a button or link that has the focus does that control's own job", async () => {
  const kept = new Map();
  const browser = await reviewing("#/review/w/1", { kept });
  for (const target of [browser.node("review-flag"), browser.node("review-next"), browser.nav().querySelector("a")]) {
    target.focus();
    const event = browser.document.emit("keydown", { key: "Enter", target }); await flush();
    assert.equal(event.defaultPrevented, false, "the key is left to the control that has the focus");
  }
  assert.equal(marksOf(kept), null, "nothing is marked that the reader did not mark");
  assert.equal(browser.location.hash, "#/review/w/1");
  // Anywhere else on the step, Enter still says it looks right.
  browser.document.emit("keydown", { key: "Enter", target: browser.node("review-body") }); await flush();
  assert.deepEqual(marksOf(kept), { fine: [`${uuid(2)}:a-2`], flagged: [], notes: {} });
});

test("Skip and Previous move without marking, and the finish page says what is left", async () => {
  const kept = new Map();
  const browser = await reviewing("#/review/w/1", { kept });
  browser.node("review-next").click(); await flush();
  assert.equal(browser.location.hash, "#/review/w/2");
  browser.node("review-previous").click(); await flush();
  assert.equal(browser.location.hash, "#/review/w/1");
  for (let i = 0; i < 4; i++) { browser.node("review-next").click(); await flush(); }
  assert.equal(browser.location.hash, "#/review/w/done");
  assert.equal(marksOf(kept), null);
  assert.equal(finishTitle(browser), "0 of 4 marked");
  const goOn = () => browser.node("review-body").querySelectorAll("a").find((node) => node.textContent === "Continue review");
  assert.match(browser.node("review-body").textContent, /4 entities left unmarked\.Continue review/);
  assert.equal(goOn().getAttribute("href"), "#/review/w/1");
  goOn().click(); await flush();
  await mark(browser, "fine");
  browser.navigate("review/w/done"); await flush();
  assert.equal(finishTitle(browser), "1 of 4 marked");
  assert.match(browser.node("review-body").textContent, /3 entities left unmarked\.Continue review/);
  assert.equal(goOn().getAttribute("href"), "#/review/w/2", "it goes on from the first entity not looked at");
  browser.button("Commit anyway").click();
  assert.match(commitForm(browser).field.textContent, /Change want_bytes/);
  assert.equal(commitForm(browser).submit.disabled, false, "bypassing unmarked work still drafts the same commit message");
});

test("the finish page of uncommitted work commits the work there, and the change keeps what was marked", async () => {
  const kept = new Map();
  const browser = await reviewing("#/review/w/1", { kept });
  await mark(browser, "fine", "flag", "fine", "flag");
  assert.equal(browser.location.hash, "#/review/w/done");
  assert.equal(finishTitle(browser), "Two look right, two flagged");
  assert.deepEqual(reviewRows(browser.node("review-body")).map((group) => [group.title, group.count, group.rows.map((row) => row.name)]), [["Flagged", "2", ["old_helper", "base64_encode"]]]);
  // Committing is how uncommitted work is accepted, so the form is already here, and it is the page: there is nothing to cancel back to.
  assert.ok(commitForm(browser).field, "the form is here without being asked for");
  // The message starts as a draft of what the commit holds, named by entity, and says it is a draft.
  assert.match(commitForm(browser).field.textContent, /^Change want_bytes/);
  assert.match(browser.node("review-body").textContent, /Kin drafted this from what changed\. Say why in your own words\./);
  assert.equal(commitForm(browser).submit.disabled, false);
  assert.equal(browser.button("Cancel"), undefined);
  const hint = () => browser.node("review-body").querySelectorAll("span").find((node) => node.textContent === "A message is needed to commit.");
  assert.equal(hint().hidden, true);
  browser.type("commit-message", "");
  assert.equal(commitForm(browser).submit.disabled, true, "an emptied message cannot be committed");
  assert.equal(hint().hidden, false);
  browser.type("commit-message", "Bound");
  assert.equal(hint().hidden, true, "the form stops asking once it has one");
  const change = "d".repeat(64);
  browser.data["/api/workspace/commit"] = (url, options) => { browser.data["/api/guide"] = guide(); return { operation_id: JSON.parse(options.body).operation_id, outcome: "committed", change_id: change, title: JSON.parse(options.body).message }; };
  writeCommit(browser, "Bound input before decoding");
  commitForm(browser).submit?.click();
  await flush();
  assert.deepEqual(browser.sent("/api/workspace/commit"), [{ message: "Bound input before decoding", operation_id: uuid(1) }]);
  const body = browser.node("review-body");
  assert.match(body.textContent, /^Committedquiet, want_bytes and 2 more are committed as change dddddddd, “Bound input before decoding”\./);
  assert.match(body.textContent, /Nothing is uncommitted/);
  assert.deepEqual(body.querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Review it", guided(change)], ["Open History", "#/history"]]);
  assert.equal(commitForm(browser).field, null);
  assert.equal(sideCount(browser, "uncommitted"), null);
  // What was looked at in the work stays looked at in the change it became.
  assert.deepEqual(marksOf(kept, change), marksOf(kept));
  browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), FOUR, fourChanged());
  browser.navigate(`review/c/${change}`); await flush();
  assert.deepEqual(listMarks(browser.node("review-body")), [["want_bytes", "Looks right"], ["old_helper", "Flagged"], ["quiet", "Looks right"], ["base64_encode", "Flagged"]]);
  assert.deepEqual(reviewAction(browser.node("review-head")), [["Finish review", `#/review/c/${change}/done`]]);
});

test("each outcome of a commit made at the end of a review is said there", async () => {
  const finished = async (options = {}) => { const browser = await reviewing("#/review/w/1", options); await mark(browser, "fine", "fine", "fine", "fine"); return browser; };
  let browser = await finished();
  assert.equal(finishTitle(browser), "All four look right");
  assert.deepEqual(reviewRows(browser.node("review-body")), [], "nothing was flagged, so nothing is listed");
  browser.data["/api/workspace/commit"] = (url, options) => ({ operation_id: JSON.parse(options.body).operation_id, outcome: "nothing_to_commit" });
  writeCommit(browser, "Bound input"); await flush();
  assert.match(browser.node("review-body").textContent, /Nothing to commitYour code already matches your last commit\./);
  assert.equal(browser.count("/api/workspace/commit"), 1);

  const kept = new Map();
  browser = await finished({ kept });
  browser.data["/api/workspace/commit"] = { __status: 409, error: "The projection is blocked.", body: { outcome: "refused", remedy: "Nothing was committed. Resolve what is reported, then commit again." } };
  writeCommit(browser, "Bound input"); await flush();
  assert.match(browser.node("review-body").textContent, /The commit was not made.*Nothing was committed\. Resolve what is reported, then commit again\./s);
  assert.equal(commitForm(browser).field.textContent, "Bound input", "the message is kept to send again");
  assert.equal(commitForm(browser).submit.disabled, false);
  assert.deepEqual([...kept.keys()].filter((key) => key.startsWith("kin.workspace.review:")), [marksKey("w:change-1")], "nothing was committed, so no change has marks");

  browser = await finished();
  browser.failures.set("/api/workspace/commit", new Error("Connection closed after request"));
  writeCommit(browser, "Bound input"); await flush();
  assert.match(browser.node("review-body").textContent, /The commit's outcome is unknown.*This page will not send it again\./s);
  assert.equal(commitForm(browser).field, null, "there is no form to send");
  browser.refresh(); await flush();
  browser.navigate("uncommitted"); browser.navigate("review/w/done"); await flush();
  assert.match(browser.node("review-body").textContent, /The commit's outcome is unknown/);
  assert.equal(browser.count("/api/workspace/commit"), 1);

  // A session that may not commit can still read the work through to the end.
  browser = await finished({ capabilities: { commit: false } });
  assert.equal(finishTitle(browser), "All four look right");
  assert.equal(commitForm(browser).field, null);
  assert.match(browser.node("review-body").textContent, /This version of Kin can't commit from this page\./);
});

const viewOf = (browser) => browser.node("review-body").querySelectorAll("section").find((node) => node.getAttribute("aria-label") === "Your view of this change");

test("the finish page of a change takes the reader's view of it and sends it once", async () => {
  const browser = await reviewingChange("#/review/c/change-2/1");
  await mark(browser, "fine", "fine", "fine", "fine");
  assert.equal(browser.location.hash, "#/review/c/change-2/done");
  assert.equal(finishTitle(browser), "All four look right");
  assert.deepEqual(browser.node("review-head").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["History", "#/history"], ["Preserved change", "#/review/c/change-2"]]);
  assert.match(viewOf(browser).textContent, /^Your view of this changeYour own record of how it reads\. Saving it blocks nothing and changes no code\./);
  // The choice reads as the walk's own: "Flag it" acts, and "Flagged" is what it leaves.
  assert.deepEqual(viewOf(browser).querySelectorAll("button").map((node) => node.textContent), ["Flag it", "Looks right"]);
  assert.doesNotMatch(browser.text(), /Approve|Block|Needs work/, "a view is how the change read, not a gate");
  assert.doesNotMatch(viewOf(browser).textContent, /Your view now/, "nobody has given one yet");
  assert.equal(commitForm(browser).field, null, "a change on record is not committed again");
  browser.data["/api/decide"] = (url, options) => {
    browser.data["/api/standing"] = { standing: { ...browser.data["/api/standing"].standing, "change-2": { state: JSON.parse(options.body).state } } };
    return { reviews: [], reviews_total: 1 };
  };
  browser.type("review-comment", "  The callers were left behind.  ");
  const standing = browser.count("/api/standing");
  browser.button("Flag it").click();
  assert.ok(viewOf(browser).querySelectorAll("button").every((node) => node.disabled), "one view at a time");
  viewOf(browser).querySelectorAll("button").forEach((node) => node.click());
  await flush();
  const calls = browser.requests.filter((request) => request.name === "/api/decide");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "POST");
  assert.equal(calls[0].options.headers["x-kin-review-token"], "synthetic-token");
  assert.deepEqual(JSON.parse(calls[0].options.body), { change: "change-2", title: "Preserved change", state: "needs_work", comment: "The callers were left behind." });
  assert.match(viewOf(browser).textContent, /Your view now: flagged\. A new one replaces it\./);
  assert.equal(browser.count("/api/standing"), standing + 1, "the views are read again");
  // A new view replaces the last, and a blank note is not sent.
  browser.type("review-comment", "   ");
  browser.button("Looks right").click(); await flush();
  assert.deepEqual(browser.sent("/api/decide")[1], { change: "change-2", title: "Preserved change", state: "approved" });
  assert.match(viewOf(browser).textContent, /Your view now: looks right\. A new one replaces it\./);
  browser.navigate("history");
  assert.match(browser.node("changes-results").querySelectorAll("a").find((row) => row.getAttribute("href") === guided("change-2")).textContent, /Looks right$/, "History says the same");
});

test("a view Kin could not save says why and can be sent again, and a session that may not write is offered nothing to press", async () => {
  const browser = await reviewingChange("#/review/c/change-1/done");
  assert.match(viewOf(browser).textContent, /Your view now: looks right\. A new one replaces it\./, "the view already given is said");
  browser.data["/api/decide"] = { __status: 502, error: "Kin could not reach this repository." };
  browser.type("review-comment", "The callers were left behind.");
  browser.button("Flag it").click(); await flush();
  assert.match(viewOf(browser).querySelector("[role=alert]").textContent, /Your view was not saved.*Kin could not reach this repository\./s);
  assert.match(viewOf(browser).textContent, /Your view now: looks right\./, "the view that stands is unchanged");
  assert.equal(browser.node("review-comment").textContent, "The callers were left behind.", "the note is kept");
  assert.ok(viewOf(browser).querySelectorAll("button").every((node) => !node.disabled));
  browser.data["/api/decide"] = { reviews: [] };
  browser.button("Flag it").click(); await flush();
  assert.equal(browser.count("/api/decide"), 2);
  assert.equal(viewOf(browser).querySelector("[role=alert]"), null);
  assert.match(viewOf(browser).textContent, /Your view now: flagged\./);

  const reader = await reviewingChange("#/review/c/change-1/done", { session: { token: "" } });
  assert.ok(viewOf(reader).querySelectorAll("button").every((node) => node.disabled));
});

test("Copy the flags hands over what was flagged as plain text", async () => {
  const browser = await reviewing("#/review/w/1");
  await mark(browser, "flag", "fine", "fine", "flag");
  const status = () => browser.node("review-body").querySelector("[role=status]").textContent;
  assert.equal(status(), "");
  browser.button("Copy the flags").click(); await flush();
  assert.deepEqual(browser.copied, ["want_bytes (Function in encoding): Its signature and its body changed. 8 entities rely on it and were not updated.\nbase64_encode (Function in encoding): New function. Kin found no connections outside this change."]);
  assert.equal(status(), "Copied. Paste it to the agent or person who made the change.");
  browser.clipboard.refuses = true;
  browser.button("Copy the flags").click(); await flush();
  assert.equal(status(), "This browser would not copy it.");
  assert.equal(browser.copied.length, 1);
  // With nothing flagged there is nothing to copy.
  const clean = await reviewing("#/review/w/1");
  await mark(clean, "fine", "fine", "fine", "fine");
  assert.equal(clean.button("Copy the flags"), undefined);
});

/** What a branch holds beyond another, as the guide of a range answers it. */
const ranged = (base, head) => ({ ...recorded(head, FOUR, fourChanged()), range: { base_ref: base, head_ref: head, merge_base: "change-0", changes_total: 2, changes: [{ id: "c8", title: "Bound base64 input before decoding", author: "Dana Reader", timestamp: secondsAgo(600) }, { id: "c9", title: "Drop the old helper", author: "Dana Reader", timestamp: secondsAgo(300) }] } });

test("what one branch holds beyond another is reviewed the same way, and the view of it names both", async () => {
  const kept = new Map();
  const browser = fixture({ hash: "#/review/r/main..feature", kept });
  browser.data["/api/guide?base"] = (url) => ranged(url.searchParams.get("base"), url.searchParams.get("change"));
  await flush();
  const asked = browser.requests.filter((request) => request.name === "/api/guide" && /base=/.test(request.url));
  assert.deepEqual(asked.map((request) => request.url.split("?")[1]), ["base=main&change=feature"]);
  const head = browser.node("review-head");
  assert.equal(head.querySelector("h1").textContent, "feature against main");
  assert.deepEqual(head.querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Branches", "#/branches"], ["Start review", "#/review/r/main..feature/1"]]);
  assert.match(head.textContent, /Four functions changed\./);
  assert.doesNotMatch(head.textContent, /Dana Reader/, "a range has no one author");
  assert.deepEqual(picture(browser.node("review-body")).columns[0].boxes, FOUR_BOXES("#/review/r/main..feature"));
  assert.deepEqual(sideCurrent(browser), ["uncommitted"], "comparing branches is part of Work");
  browser.navigate("review/r/main..feature/1"); await flush();
  assert.equal(stepOf(browser).article.getAttribute("aria-label"), "want_bytes");
  assert.deepEqual(browser.node("review-head").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Branches", "#/branches"], ["feature against main", "#/review/r/main..feature"]]);
  await mark(browser, "fine", "flag", "fine", "fine");
  assert.equal(browser.location.hash, "#/review/r/main..feature/done");
  assert.deepEqual(marksOf(kept, "r:main..feature"), { fine: [`${uuid(2)}:a-2`, `${uuid(7)}:a-7`, `${uuid(5)}:a-5`], flagged: [`${uuid(6)}:a-6`], notes: {} }, "a range keeps marks of its own");
  assert.equal(finishTitle(browser), "Three look right, one flagged");
  browser.data["/api/decide"] = { reviews: [] };
  browser.type("review-comment", "One helper is still relied on.");
  browser.button("Flag it").click(); await flush();
  assert.deepEqual(browser.sent("/api/decide"), [{ change: "feature", base: "main", range: true, state: "needs_work", comment: "One helper is still relied on." }]);
  assert.match(viewOf(browser).textContent, /Your view now: flagged\./);
  assert.equal(asked.length, 1);
  assert.equal(browser.requests.filter((request) => request.name === "/api/guide" && /base=/.test(request.url)).length, 1, "the range is read once");

  // A branch name may hold a slash, and its pages are still found.
  const slashed = fixture({ hash: "#/review/r/main..feat%2Fbound-input/2" });
  slashed.data["/api/guide?base"] = (url) => ranged(url.searchParams.get("base"), url.searchParams.get("change"));
  await flush();
  assert.match(slashed.requests.find((request) => /base=/.test(request.url)).url, /base=main&change=feat%2Fbound-input$/);
  assert.equal(stepOf(slashed).position, "2 of 4");
  assert.equal(slashed.node("review-next").getAttribute("href"), "#/review/r/main..feat%2Fbound-input/3");
  assert.deepEqual({ ...slashed.api.parseRoute("#/review/r/main..feat/bound-input/done").target }, { kind: "range", base: "main", head: "feat/bound-input" });
  assert.deepEqual({ ...slashed.api.parseRoute("#/review/r/main") }, { name: "history" }, "an address that names one branch compares nothing");

  // Branches Kin could not compare say so, with a retry.
  const failing = fixture({ hash: "#/review/r/main..gone" });
  failing.failures.set("/api/guide?base", { status: 404, body: { error: "No branch is named gone." } });
  await flush();
  assert.match(failing.node("review-body").textContent, /Kin couldn't compare these branches\..*No branch is named gone\./s);
  failing.failures.delete("/api/guide?base");
  failing.node("review-body").querySelector("button").click(); await flush();
  assert.equal(failing.node("review-head").querySelector("h1").textContent, "gone against main");
});

test("a change too large to walk, or with no entity in it, says so and starts no walk", async () => {
  const large = fixture({ hash: "#/review/c/large" });
  large.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { added: 20_000 }, [], { not_walked: { entities: 20_000 } });
  await flush();
  assert.equal(large.node("review-body").textContent, "This change is too large to read one entity at a time20,000 entities changed. Kin can still show it whole.Read it on one page");
  assert.deepEqual(large.node("review-body").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Read it on one page", `${BASE}/review/#/c/large`]], "the whole change, on the page that reads it at once");
  assert.deepEqual(reviewAction(large.node("review-head")), [], "there is no walk to start");
  const wide = fixture({ hash: "#/review/r/main..feature" });
  wide.data["/api/guide?base"] = (url) => ({ ...ranged(url.searchParams.get("base"), url.searchParams.get("change")), summary: guide({ added: 20_000 }).summary, chapters: [], not_walked: { entities: 20_000 } });
  await flush();
  assert.deepEqual(wide.node("review-body").querySelectorAll("a").map((node) => node.getAttribute("href")), [`${BASE}/review/#/r/main..feature`], "and two branches are read whole the same way");
  assert.match(large.node("review-head").textContent, /20,000 entities changed\./);

  const empty = fixture({ hash: "#/review/c/empty" });
  empty.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), {}, []);
  await flush();
  assert.match(empty.node("review-head").textContent, /No entity changed in this change\./);
  assert.equal(empty.node("review-body").textContent, "No entity changed");
  assert.deepEqual(reviewAction(empty.node("review-head")), []);
});

test("a step number that is not in the review leads back to the list", async () => {
  const change = await reviewingChange("#/review/c/change-1/9");
  assert.match(change.node("review-body").textContent, /^That entity is not in this review/);
  assert.deepEqual(change.node("review-body").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Back to the list", "#/review/c/change-1"], ["Finish review", "#/review/c/change-1/done"]]);
  assert.equal(change.node("review-fine"), null, "there is nothing to mark");
  const work = await reviewing("#/review/w/9");
  assert.deepEqual(work.node("review-body").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Back to the list", "#/uncommitted"], ["Finish review", "#/review/w/done"]]);
  assert.match(work.node("review-body").textContent, /It holds 4 entities now\./);
  // Work that shrank while it was being read leaves the same way.
  const shrinking = await reviewing("#/review/w/4");
  assert.equal(stepOf(shrinking).position, "4 of 4");
  shrinking.data["/api/guide"] = working({ modified: 1 }, [changed(2, "want_bytes")]);
  shrinking.refresh(); await flush();
  assert.match(shrinking.node("review-body").textContent, /^That entity is not in this review/);
});

test("a change Kin read while its graph was being written is read again by itself, with what is happening in plain words and Kin's own words behind Show details", async () => {
  // The app server's answer when the engine declined: its own refusal, carried whole.
  const refusal = "kin review refused (HTTP 503): no settled graph authority for review: a graph-authority writer was mutating the graph while this read ran. This read reached its 45 s limit for waiting out graph-authority writers.";
  const browser = fixture({ hash: "#/review/c/change-1", session: { history: { state: "partial", admitted: 1, total: 2809, deriving: true, staged: false, step: 11, steps: 16 } } });
  browser.failures.set("/api/guide?change", { status: 502, body: { error: refusal } });
  await flush();
  const body = () => browser.node("review-body");
  // What a reader sees outside Show details, and what is behind it.
  const plain = () => { const details = body().querySelector("details"); return body().textContent.replace(details ? details.textContent : "", ""); };
  assert.match(plain(), /^Kin was taking in new work in Fixture while it read this change, so it is reading it again\./);
  assert.match(plain(), /Fixture is reading its older history in the background, step 11 of 16\. This review reads the change as it was committed, so it does not wait for that\./);
  assert.match(plain(), /Kin reads it again in 2 s, attempt 2\./);
  assert.doesNotMatch(plain(), /settled graph authority|graph-authority|reconciliation_status|HTTP 503/);
  assert.equal(body().querySelector("details").querySelector("summary").textContent, "Show details");
  assert.match(body().querySelector("details").textContent, /no settled graph authority for review/);
  assert.equal(body().querySelector("[role=alert]"), null, "a read Kin is asking again for is not shown as a failure");
  // The seconds count down without the reader doing anything, and the page asks again by itself.
  await browser.advance(1000);
  assert.match(plain(), /Kin reads it again in 1 s, attempt 2\./);
  const asked = browser.count("/api/guide");
  let release; browser.held.set("/api/guide?change", new Promise((done) => { release = done; }));
  await browser.advance(1000); await flush();
  assert.equal(browser.count("/api/guide"), asked + 1, "asked again after its wait");
  // While it reads again, the page says so at once, and the old answer stays behind Show details.
  assert.match(plain(), /^Kin was taking in new work in Fixture while it read this change, so it is reading it again\./);
  assert.match(plain(), /Reading it again, attempt 2, 2 s so far\./);
  assert.equal(body().querySelector("button"), null, "nothing to press while a read is on its way");
  await browser.advance(5000);
  // The clock counts the reader's whole wait, from the first attempt, not only this one.
  assert.match(plain(), /Reading it again, attempt 2, 7 s so far\./);
  // The graph held still this time: the change is shown.
  browser.failures.delete("/api/guide?change");
  browser.held.delete("/api/guide?change"); release(); await flush();
  assert.deepEqual(picture(body()).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
  assert.doesNotMatch(body().textContent, /reading it again|Show details/);
  await browser.advance(60_000);
  assert.equal(browser.count("/api/guide"), asked + 1, "nothing more is asked once the change is shown");
});

test("a change that takes a while to read says how long it has taken, and that it does not wait for older history being read", async () => {
  const browser = fixture({ hash: "#/review/c/change-1", session: { history: { state: "partial", admitted: 1, total: 2809, deriving: true, staged: false, step: 5, steps: 16 } } });
  let release; browser.held.set("/api/guide?change", new Promise((done) => { release = done; }));
  await flush();
  const body = () => browser.node("review-body");
  assert.match(body().textContent, /^Reading this change\.Fixture is reading its older history in the background, step 5 of 16\./);
  await browser.advance(3000);
  assert.match(body().textContent, /3 s so far\./);
  await browser.advance(9000);
  assert.match(body().textContent, /12 s so far\./);
  browser.held.delete("/api/guide?change"); release(); await flush();
  assert.deepEqual(picture(body()).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
  // A repository whose history is all read says nothing about it, and a quick read says nothing at all.
  const quick = fixture({ hash: "#/review/c/change-1" });
  let done; quick.held.set("/api/guide?change", new Promise((resolve) => { done = resolve; }));
  await flush();
  assert.equal(quick.node("review-body").textContent, "");
  quick.held.delete("/api/guide?change"); done(); await flush();
  assert.deepEqual(picture(quick.node("review-body")).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
});

/** How many times a page asked for one recorded change's guide, apart from its reads of uncommitted work. */
const changeReads = (page) => page.requests.filter((request) => request.name.endsWith("/api/guide") && request.url.includes("change=")).length;

test("a review read that takes longer than a minute is waited for, longer on each attempt, and opens when Kin answers", async () => {
  const browser = fixture({ hash: "#/review/c/change-1" });
  let release; browser.held.set("/api/guide?change", new Promise((done) => { release = done; }));
  await flush();
  const body = () => browser.node("review-body");
  // A minute is not the end of it: the first attempt waits a minute and a half.
  await browser.advance(75_000);
  assert.equal(changeReads(browser), 1);
  assert.match(body().textContent, /75 s so far\./);
  assert.equal(body().querySelector("[role=alert]"), null);
  await browser.advance(15_000);
  assert.match(body().textContent, /Kin reads it again in 2 s, attempt 2\./);
  assert.match(body().querySelector("details").textContent, /This read took longer than 90 seconds\./);
  await browser.advance(2000);
  assert.equal(changeReads(browser), 2, "asked again, where Kin is still reading it");
  // The second attempt waits longer than the first did, and the clock counts the whole wait.
  await browser.advance(120_000);
  assert.equal(changeReads(browser), 2, "the second attempt is still waiting after two minutes");
  assert.match(body().textContent, /Reading it again, attempt 2, 3 min 32 s so far\./);
  browser.held.delete("/api/guide?change"); release(); await flush();
  assert.deepEqual(picture(body()).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
  assert.doesNotMatch(body().textContent, /so far|again/);
});

test("Kin saying it is still reading a change keeps the attempt going rather than failing it", async () => {
  const browser = fixture({ hash: "#/review/c/change-1" });
  const answer = browser.data["/api/guide?change"];
  browser.data["/api/guide?change"] = [{ state: "reading", retry_after_secs: 5 }, { state: "reading" }, answer];
  await flush();
  const body = () => browser.node("review-body");
  assert.equal(body().querySelector("[role=alert]"), null);
  assert.doesNotMatch(body().textContent, /attempt|Show details/, "an answer that it is still reading is not a failed attempt");
  await browser.advance(1000);
  assert.match(body().textContent, /Kin is still reading it, 1 s so far\./);
  await browser.advance(4000);
  assert.equal(changeReads(browser), 2, "asked again when Kin said to");
  await browser.advance(3000);
  assert.equal(changeReads(browser), 3);
  assert.deepEqual(picture(body()).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
});

test("a change Home read is handed to its review in the same tab, so it is not read twice", async () => {
  const tabStorage = new Map();
  const hub = fixture({ hub: true, tabStorage });
  const rows = [listed(NOTES, "notes", open())];
  hub.data["/api/home"] = homePayload(rows);
  hub.data[under(NOTES, "/api/workspace/overview")] = summaryOf({ recent: [
    { id: "n-2", title: "Return two from value", author: "Claude Code", agent: { name: "Claude Code", key: "00000000000000a1" }, time: secondsAgo(120), kin_checkpoint: false }] });
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  hub.data[under(NOTES, "/api/counts")] = { counts: { "n-2": { entities: 1, module_containers: 0, containers_through_members: 0 } } };
  hub.data[under(NOTES, "/api/guide?change")] = recorded("n-2", { modified: 1 }, [changed(2, "value")]);
  answering(hub, rows);
  await flush();
  assert.equal(hub.count(under(NOTES, "/api/guide")), 1);
  const kept = JSON.parse(tabStorage.get(`kin.workspace.change-guide:/repositories/${NOTES}:n-2`));
  assert.equal(kept.data.change.id, "n-2", "Home keeps what it read for the review it leads to");
  // The review of that change, opened from Home in this tab, draws it without asking Kin again.
  const handed = new Map([[`kin.workspace.change-guide:${BASE}:n-2`, JSON.stringify({ at: START - 60_000, data: kept.data })]]);
  const browser = fixture({ hash: "#/review/c/n-2", tabStorage: handed });
  await flush();
  assert.equal(changeReads(browser), 0);
  assert.deepEqual(picture(browser.node("review-body")).columns[0].boxes.map(([name]) => name), ["value"]);
  // A reading from long ago is read again, and what the review reads is kept for Home in turn.
  const old = fixture({ hash: "#/review/c/change-1", tabStorage: new Map([[`kin.workspace.change-guide:${BASE}:change-1`, JSON.stringify({ at: START - 3_600_000, data: kept.data })]]) });
  await flush();
  assert.equal(changeReads(old), 1);
  assert.equal(JSON.parse(old.tabStorage.get(`kin.workspace.change-guide:${BASE}:change-1`)).data.change.id, "change-1");
  const home = fixture({ hub: true, tabStorage: new Map([[`kin.workspace.change-guide:/repositories/${NOTES}:n-2`, JSON.stringify({ at: START - 1000, data: kept.data })]]) });
  home.data["/api/home"] = homePayload(rows);
  for (const key of [under(NOTES, "/api/workspace/overview"), under(NOTES, "/api/standing"), under(NOTES, "/api/counts")]) home.data[key] = hub.data[key];
  answering(home, rows);
  await flush();
  assert.equal(home.count(under(NOTES, "/api/guide")), 0, "Home draws a change its review already read");
  assert.equal(needCards(home)[0].picture.columns[0].boxes[0][0], "value");
});

test("a change Kin keeps declining is asked for less often, then left to the reader with Try again, and Try now asks at once", async () => {
  const browser = fixture({ hash: "#/review/c/change-1" });
  browser.failures.set("/api/guide?change", { status: 502, body: { error: "kin review refused (HTTP 503): no settled graph authority for review: a graph-authority writer was mutating the graph while this read ran." } });
  await flush();
  const body = () => browser.node("review-body");
  // Without a history being read, nothing is said about one.
  assert.doesNotMatch(body().textContent, /older history/);
  // The reader need not wait: Try now asks at once.
  const before = browser.count("/api/guide");
  body().querySelector("button").click(); await flush();
  assert.equal(browser.count("/api/guide"), before + 1);
  assert.match(body().textContent, /Kin reads it again in 4 s, attempt 3\./);
  // Then it waits longer each time, up to a quarter of a minute.
  for (const [wait, attempt] of [[4000, 3], [8000, 4], [15_000, 5], [15_000, 6], [15_000, 7]]) {
    await browser.advance(wait - 1); await flush();
    assert.equal(browser.count("/api/guide"), before + attempt - 2, `attempt ${attempt} waits ${wait} ms`);
    await browser.advance(1); await flush();
    assert.equal(browser.count("/api/guide"), before + attempt - 1);
  }
  await browser.advance(15_000); await flush();
  assert.equal(browser.count("/api/guide"), before + 7);
  // Eight attempts in all: the page stops asking and says it could not load the change, with Kin's words behind Show details.
  assert.match(body().querySelector("[role=alert]").textContent, /^Kin couldn't load this change\./);
  assert.match(body().querySelector("details").textContent, /no settled graph authority/);
  await browser.advance(120_000); await flush();
  assert.equal(browser.count("/api/guide"), before + 7, "it no longer asks by itself");
  // Try again starts over, and shows its progress rather than the old failure while it reads.
  let release; browser.held.set("/api/guide?change", new Promise((done) => { release = done; }));
  browser.failures.delete("/api/guide?change");
  body().querySelector("button").click(); await flush();
  assert.equal(body().querySelector("[role=alert]"), null);
  assert.match(body().textContent, /Reading it again\./);
  browser.held.delete("/api/guide?change"); release(); await flush();
  assert.deepEqual(picture(body()).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
});

test("a change Kin could not read says so with a retry, and uncommitted work that could not be read says so in the review too", async () => {
  const browser = fixture({ hash: "#/review/c/change-1" });
  browser.failures.set("/api/guide?change", { status: 502, body: { error: "Kin could not answer: It did not answer within 20 seconds." } });
  await flush();
  // Kin is only slow, so the page says so and asks again by itself.
  assert.match(browser.node("review-body").textContent, /^Kin is still reading this change\. This fills in when it is ready\./);
  browser.failures.delete("/api/guide?change");
  await browser.advance(4000); await flush();
  assert.deepEqual(picture(browser.node("review-body")).columns[0].boxes.map(([name]) => name), ["want_bytes"]);
  assert.equal(browser.node("review-head").querySelector("h1").textContent, "Preserved change");
  const gone = fixture({ hash: "#/review/c/gone" });
  gone.failures.set("/api/guide?change", { status: 404, body: { error: "No such change." } });
  await flush();
  assert.match(gone.node("review-body").textContent, /Kin couldn't load this change\..*No such change\./s);

  const work = fixture({ hash: "#/review/w/1" });
  work.failures.set("/api/guide", "Working read failed");
  await flush();
  assert.match(work.node("review-body").textContent, /Kin couldn't load your uncommitted work\..*Working read failed/s);
  work.failures.delete("/api/guide");
  work.node("review-body").querySelector("button").click(); await flush();
  assert.equal(stepOf(work).article.getAttribute("aria-label"), "want_bytes");
});

test("the finish page's title counts in plain words whatever the numbers are", async () => {
  const finished = async (...marks) => {
    const browser = fixture({ hash: "#/review/w/1" });
    browser.data["/api/guide"] = working({ modified: marks.length }, marks.map((_, i) => changed(40 + i, `entity_${i}`)));
    await flush();
    await mark(browser, ...marks);
    return finishTitle(browser);
  };
  for (const [marks, title] of [
    [["fine"], "It looks right"], [["flag"], "Flagged"],
    [["fine", "fine"], "Both look right"], [["flag", "flag"], "Both flagged"], [["fine", "flag"], "One looks right, one flagged"],
    [["flag", "flag", "flag"], "All three flagged"], [["fine", "fine", "flag"], "Two look right, one flagged"], [["fine", "fine", "fine"], "All three look right"]]) {
    assert.equal(await finished(...marks), title, marks.join(", "));
  }
  const none = fixture({ hash: "#/review/c/empty/done" });
  none.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), {}, []);
  await flush();
  assert.equal(finishTitle(none), "Nothing to look at here");
});

test("a step's picture joins the entity to each thing that relies on it, in the tone of where that thing stands", async () => {
  const browser = await reviewing("#/review/w/1");
  const host = () => stepOf(browser).relies.querySelector("[role=group]");
  pictureLayout(browser, host);
  await browser.advance(40);
  assert.deepEqual(picture(stepOf(browser).relies).lines(), [
    ["M200.0 35.0C350.0 35.0 350.0 35.0 500.0 35.0", "bad"], ["M200.0 35.0C350.0 35.0 350.0 75.0 500.0 75.0", "good"], ["M200.0 35.0C350.0 35.0 350.0 115.0 500.0 115.0", "bad"]]);
  const watching = browser.observers.at(-1);
  assert.deepEqual(watching.watched, [host()]);
  browser.navigate("review/w/2"); await flush();
  assert.equal(watching.disconnected, true, "the next step's picture is measured on its own");
});

test("a list longer than its picture is read on the review's own page, whatever is being reviewed", async () => {
  const browser = fixture({ hash: "#/review/c/change-1" });
  browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { ...FOUR, modified: 7 }, manyChanged());
  await flush();
  assert.equal(reviewRows(browser.node("review-body")).flatMap((group) => group.rows).length, 9, "every entity is listed under the picture");
  const [[words, href]] = picture(browser.node("review-body")).columns[0].more;
  assert.equal(words, "and 1 more");
  assert.ok(href === null || href.startsWith("#/review/c/change-1"), `the rest of this change is not on ${href}`);
});

test("a box on the right says where it sits the way a changed box does", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = working({ modified: 2, recorded_consumers: 1 }, [changed(2, "want_bytes", { connections: connected([relying(10, "Serializer.dumps")]) }), changed(11, "Serializer.loads", { kind: "method", within: "class Serializer" })]);
  await flush();
  const [left, right] = picture(browser.node("work-flight")).columns;
  assert.equal(left.boxes[1][1], "Method in Serializer");
  assert.equal(right.boxes[0][1], "Method in Serializer");
});

// ── Home ───────────────────────────────────────────────────────────────────

const OPERATION = "00000000-0000-4000-8000-0000000000aa";
/** What Kin found at a place it was asked to look at. */
const inspected = (fields = {}) => ({ path: "/work/itsdangerous", location: "~/code/itsdangerous", exists: true, kind: "git", name: "itsdangerous", registered: false, note: null, empty: false, actions: ["convert"], git: { commits: 1204, branch: "main" }, ...fields });
/** One reading of an operation the hub started. */
const operation = (state, fields = {}) => ({ operation_id: OPERATION, action: "convert", location: "~/code/itsdangerous", state, stage: "reading", phase: "Reading the project", lines: [], started_at: 1_770_000_000, finished_at: null, status_url: `/api/home/operations/${OPERATION}`, ...fields });
const addPanel = (hub) => hub.node("hub-add");
/** What the add panel offers to do with the place it looked at: its buttons and links outside the form and the choice of what to add. */
const offered = (hub) => [...addPanel(hub).querySelectorAll("button"), ...addPanel(hub).querySelectorAll("a")].filter((node) => !node.closest("form") && node.getAttribute("aria-pressed") === null && node.textContent !== "Close").map((node) => node.textContent);
const operationBox = (hub) => addPanel(hub).querySelector("[role=status]");
/** Open the add panel the way a reader does on Home: by its link beside the repositories. */
async function openAdding(hub) {
  hub.node("home-repos").querySelectorAll("a").find((node) => node.textContent === "Add repository").click();
  await flush();
}
/** Name a place and ask Kin to look at it, opening the panel first when it is closed. */
async function look(hub, answer, place = "~/code/itsdangerous") {
  if (!hub.node("add-path")) await openAdding(hub);
  hub.data["POST /api/home/inspect"] = answer;
  hub.type("add-path", place);
  addPanel(hub).querySelector("form").emit("submit");
  await flush();
}
/** Answer what Home reads of each of these repositories: a summary, no agents, and nothing uncommitted. */
function answering(hub, rows) {
  for (const row of rows) {
    hub.data[under(row.key, "/api/workspace/overview")] ??= summaryOf({ entities: 10, relationships: 5, languages: [["python", 10]] });
    // A repository whose own list of agents is not given here does not answer it when the machine's list counts some,
    // so the machine's list is the word Home has; otherwise it lists none, as the machine's list says.
    if ((row.agents?.connected || 0) > 0 && !(under(row.key, "/api/workspace/agents") in hub.data)) hub.failures.set(under(row.key, "/api/workspace/agents"), "The agents read did not answer within 20 seconds.");
    hub.data[under(row.key, "/api/workspace/agents")] ??= agentsPayload({ agents: [] });
    hub.data[under(row.key, "/api/guide")] ??= guide();
  }
}
/** Each repository that needs the reader, as its card says it: where it leads, what to do, what it says, and its picture. */
const needCards = (hub) => hub.node("home-attention").querySelectorAll("article").map((card) => {
  const links = card.querySelectorAll("a").filter((node) => !node.closest("[role=group]"));
  return { repository: [links[0].textContent, links[0].getAttribute("href")], actions: links.slice(1).map((node) => [node.textContent, node.getAttribute("href")]),
    said: card.querySelectorAll("p").map((node) => node.textContent), busy: card.querySelectorAll("[aria-busy=true]").length, picture: picture(card) };
});
/** Each repository as its card on Home says it: its name and branch, its counts, its languages, what is in motion, and where it leads. */
const repoCards = (hub) => hub.node("home-repos").querySelectorAll("a[data-state]").map((card) => {
  const [head, ...parts] = card.children;
  const live = parts.find((part) => part.className === "repo-card-live");
  const [stats, languages, foot] = parts.filter((part) => part !== live);
  return { name: head.children[0].textContent, branch: head.children[1]?.textContent ?? "", stats: stats.children.map((stat) => stat.children.map((part) => part.textContent).join(" ")),
    languages: languages.children[1]?.children.map((key) => key.textContent) ?? [], foot: foot.children.map((part) => part.textContent), href: card.getAttribute("href"), state: card.dataset.state,
    ...(live ? { live: live.textContent } : {}) };
});
/** Each agent at work as its card on Home says it: who and where, what it did last and before, and where the card leads. */
const agentCards = (hub) => hub.node("home-agents").querySelectorAll("a[data-state]").map((card) => {
  const [head, action, ...rest] = card.children;
  return { head: head.children.filter((part) => part.getAttribute("aria-hidden") !== "true").map((part) => part.textContent), action: action.textContent,
    links: action.querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), before: rest.flatMap((part) => part.querySelectorAll("li").map((item) => item.textContent)),
    note: rest.at(-1).textContent, href: card.getAttribute("href"), state: card.dataset.state };
});
const headline = (hub) => [hub.node("home-headline").querySelector("h1")?.textContent ?? "", hub.node("home-headline").querySelector("p")?.textContent ?? ""];
const URL_OF = (key) => `/repositories/${key}/workspace/`;
/** What a repository answers when asked about signing. */
const ASKED = () => ({ schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [
      { id: uuid(5), name: "Signer.verify_signature", kind: "method", signature: "def verify_signature(self, value, sig) -> bool", container: { id: uuid(3), name: "Signer", kind: "class" }, matched: ["meaning"], why: "Its description and code share words with the question." },
      { id: uuid(2), name: "want_bytes", kind: "function", signature: "def want_bytes(s)", container: { id: uuid(1), name: "encoding", kind: "module" }, matched: ["name", "meaning"], why: "Its name matches." }], total: 2 });

test("Home says first what needs the reader across the machine, then who is at work and how many repositories there are", async () => {
  // What the list says of each repository's agents is what is counted here.
  const said = async (repositories, prepare = () => {}) => { const hub = fixture({ hub: true }); hub.data["/api/home"] = homePayload(repositories); for (const key of [KEY, NOTES, ARCHIVE]) delete hub.data[under(key, "/api/workspace/agents")]; answering(hub, repositories); prepare(hub); await flush(); return headline(hub); };
  const hub = fixture({ hub: true }); await flush();
  assert.equal(hub.main().querySelector("h1").textContent, "pallets / Fixture needs you.", "the page's title is what needs the reader");
  assert.deepEqual(headline(hub), ["pallets / Fixture needs you.", "One agent is working. Three repositories on this machine."]);
  assert.deepEqual(await said([listed(KEY, "Fixture", open()), listed(NOTES, "notes", open())]), ["Nothing needs you right now.", "No agent is connected. Two repositories on this machine."]);
  assert.deepEqual(await said([listed(KEY, "Fixture", open({ working: { changed: 3, differs: true } })), listed(NOTES, "notes", open({ merge_in_progress: true }))]), ["Two repositories need you.", "No agent is connected. Two repositories on this machine."],
    "entities changed, and a merge to settle, each need the reader");
  // Code Kin has seen differ before it has counted what changed may be only Kin's own linking, so it asks nothing of the reader yet.
  assert.deepEqual(await said([listed(KEY, "Fixture", open({ working: { differs: true } }))], (page) => { page.data[under(KEY, "/api/guide")] = guide(); }), ["Nothing needs you right now.", "No agent is connected. One repository on this machine."]);
  assert.deepEqual(await said([listed(KEY, "Fixture", open({ agents: { active: 0, connected: 2, names: [] } })), listed(NOTES, "notes", open({ agents: { active: 0, connected: 1, names: [] } }))]), ["Nothing needs you right now.", "Three agents are connected and idle. Two repositories on this machine."]);
  assert.deepEqual(await said([listed(KEY, "Fixture", open({ agents: { active: 2, connected: 2, names: [] } })), listed(NOTES, "notes", open({ agents: { active: 1, connected: 1, names: [] } }))]), ["Nothing needs you right now.", "Three agents are working. Two repositories on this machine."]);
  assert.deepEqual(await said([listed(KEY, "Fixture", open({ agents: { active: 0, connected: 1, names: [] } }))]), ["Nothing needs you right now.", "One agent is connected and idle. One repository on this machine."]);
  // A repository that is not running says nothing of its work until Home opens it, and work found there needs the reader too.
  assert.deepEqual(await said([listed(ARCHIVE, "archive")], (page) => { page.data[under(ARCHIVE, "/api/guide")] = working({ modified: 1 }, [changed(2, "want_bytes")]); }), ["archive needs you.", "No agent is connected. One repository on this machine."]);
  // A read that failed claims nothing about the repositories, and the next reading draws them.
  const failing = fixture({ hub: true }); failing.failures.set("/api/home", "Registry unreadable"); await flush();
  assert.doesNotMatch(failing.text(), /needs you|Nothing uncommitted/);
  assert.equal(failing.node("home-repos").textContent, "");
  assert.deepEqual(homeReads(failing), {}, "no repository is opened without the list");
  failing.failures.delete("/api/home");
  await failing.advance(10_000);
  assert.deepEqual(headline(failing), ["pallets / Fixture needs you.", "One agent is working. Three repositories on this machine."]);
  assert.equal(failing.count(under(KEY, "/api/workspace/overview")), 1, "and only then are the repositories opened");
});

test("a list of repositories Home could not read says so, with a way to try again", async () => {
  const failing = fixture({ hub: true }); failing.failures.set("/api/home", "Registry unreadable"); await flush();
  assert.match(failing.node("home-headline").textContent, /Kin couldn't load the repositories on this machine\..*Registry unreadable/s);
  failing.failures.delete("/api/home");
  failing.button("Try again").click(); await flush();
  assert.deepEqual(headline(failing), ["pallets / Fixture needs you.", "One agent is working. Three repositories on this machine."]);
});

test("Home opens a few repositories itself, the running ones first, and asks each for one thing at a time", async () => {
  const rows = [
    listed(repositoryKey("1"), "one", { ...open(), last_change_at: secondsAgo(100) }),
    listed(repositoryKey("2"), "two", { last_change_at: secondsAgo(50) }),
    listed(repositoryKey("3"), "three", { ...open(), last_change_at: secondsAgo(1000) }),
    ...[4, 5, 6, 7].map((n) => listed(repositoryKey(String(n)), `idle-${n}`, { last_change_at: secondsAgo(n * 100) })),
    listed(repositoryKey("8"), "never", { last_change_at: null }),
    listed(repositoryKey("9"), "gone", { available: false }),
    listed("not-a-key", "odd")];
  const hub = fixture({ hub: true });
  hub.data["/api/home"] = homePayload(rows);
  answering(hub, rows);
  let release; hub.held.set(under(repositoryKey("1"), "/api/workspace/overview"), new Promise((done) => { release = done; }));
  await flush();
  // Six at most: the ones already running, then the most recently changed. The rest are not woken.
  assert.deepEqual(Object.keys(homeReads(hub)).sort(), ["1", "2", "3", "4", "5", "6"].map(repositoryKey).sort());
  assert.deepEqual(homeReads(hub)[repositoryKey("1")], ["overview"], "a repository's next read waits for the one before it");
  assert.deepEqual(homeReads(hub)[repositoryKey("3")], ["overview", "agents"], "the list says it is clean, so its work is not read");
  assert.deepEqual(homeReads(hub)[repositoryKey("2")], ["overview", "agents", "guide?working=1"], "one that is not running is opened and read through");
  release(); await flush();
  assert.deepEqual(homeReads(hub)[repositoryKey("1")], ["overview", "agents"]);
  // A repository that is gone, or one the list names with a key that is not one, is neither read nor shown.
  assert.deepEqual(repoCards(hub).map((card) => card.name), ["two", "one", "idle-4", "idle-5", "idle-6", "idle-7", "three", "never"], "every repository is shown, the most recently changed first");
  assert.equal(hub.requests.filter((request) => /not-a-key|9{64}/.test(request.url)).length, 0);
});

test("Home reads a repository again only for what the list says moved in it", async () => {
  const hub = fixture({ hub: true }); await flush();
  const reads = () => homeReads(hub);
  // An agent is connected in one, so what it is doing is read with each reading of the list. Nothing else moved.
  await hub.advance(10_000);
  assert.deepEqual(reads(), { [KEY]: ["overview", "agents", "guide?working=1", "agents"], [NOTES]: ["overview", "agents"], [ARCHIVE]: ["overview", "agents", "guide?working=1"] });
  // A new change in one: its summary, its agents and its work are read again.
  const rows = () => hub.data["/api/home"].repositories;
  hub.data["/api/home"] = homePayload([{ ...rows()[0], last_change_at: secondsAgo(5) }, ...rows().slice(1)]);
  await hub.advance(10_000);
  assert.deepEqual(reads()[KEY].slice(4), ["overview", "agents", "guide?working=1"]);
  // Work started in another: its work and its agents, and not its summary, which only a change moves.
  hub.data[under(NOTES, "/api/guide")] = working({ modified: 3 }, [changed(40, "index"), changed(41, "lookup"), changed(42, "store")]);
  hub.data["/api/home"] = homePayload([rows()[0], { ...rows()[1], working: { changed: 3, differs: true } }, rows()[2]]);
  await hub.advance(10_000);
  assert.deepEqual(reads()[NOTES], ["overview", "agents", "agents", "guide?working=1"]);
  assert.deepEqual(needCards(hub).map((card) => card.repository[0]), ["pallets / FixturemainUncommitted", "notesmainUncommitted"]);
  // The list saying a repository is clean is enough: its work is not read, and what Home held of it is let go.
  hub.data["/api/home"] = homePayload([{ ...rows()[0], working: { changed: 0, differs: false } }, ...rows().slice(1)]);
  const before = reads()[KEY].length;
  await hub.advance(10_000);
  assert.deepEqual(reads()[KEY].slice(before), ["agents"]);
  assert.deepEqual(needCards(hub).map((card) => card.repository[0]), ["notesmainUncommitted"]);
  assert.deepEqual(repoCards(hub).find((card) => card.name === "pallets / Fixture").foot.slice(0, 1), ["All committed"]);
});

test("each repository that needs the reader is a card with what changed, the way into its review, and the picture of it", async () => {
  const hub = fixture({ hub: true }); await flush();
  const [card] = needCards(hub);
  assert.deepEqual(card.repository, ["pallets / FixturemainUncommitted", URL_OF(KEY)]);
  assert.deepEqual(card.actions, [["Open", `${URL_OF(KEY)}#/uncommitted`], ["Review 2", `${URL_OF(KEY)}#/review/w/1`]], "each action opens that repository's own page");
  assert.deepEqual(card.said, ["Two functions changed. Three other places rely on them. One was not updated. One test reaches them."]);
  assert.deepEqual(card.picture.columns[0].boxes, [["want_bytes", "Function in encoding", "", `${URL_OF(KEY)}#/review/w/1`, "bad"], ["quiet", "Function in encoding", "", `${URL_OF(KEY)}#/review/w/2`, "quiet"]]);
  assert.deepEqual(card.picture.columns[1].boxes.map(([name, , , href]) => [name, href]), [["Serializer.dumps", `${URL_OF(KEY)}#/entity/${uuid(10)}`], ["Serializer.loads", `${URL_OF(KEY)}#/entity/${uuid(11)}`]]);
  assert.deepEqual(card.picture.columns[1].more, [["and 1 more", null]]);
  // Its lines are drawn, and let go when the card is drawn again.
  const host = () => hub.node("home-attention").querySelector("[role=group]");
  pictureLayout(hub, host);
  await hub.advance(40);
  assert.deepEqual(picture(hub.node("home-attention")).lines().map(([, tone]) => tone), ["bad", "bad", "quiet"]);
  const watching = hub.observers.at(-1);
  hub.data["/api/home"] = homePayload([{ ...hub.data["/api/home"].repositories[0], working: { changed: 0, differs: false } }, ...hub.data["/api/home"].repositories.slice(1)]);
  await hub.advance(10_000);
  assert.equal(watching.disconnected, true);
  assert.equal(hub.node("home-attention").textContent, "", "with nothing that needs the reader there are no cards");
});

test("a card says what it can of work Home has not read, could not read, or is reading again", async () => {
  const card = async (rows, prepare = () => {}, options = {}) => { const hub = fixture({ hub: true, ...options }); hub.data["/api/home"] = homePayload(rows); answering(hub, rows); prepare(hub); await flush(); return hub; };
  // A merge to settle, with nothing uncommitted.
  let hub = await card([listed(NOTES, "notes", open({ merge_in_progress: true }))]);
  assert.deepEqual(needCards(hub)[0].repository, ["notesmainMerge", URL_OF(NOTES)]);
  assert.deepEqual(needCards(hub)[0].actions, [["Open", `${URL_OF(NOTES)}#/branches`], ["Settle the merge", `${URL_OF(NOTES)}#/branches`]]);
  assert.deepEqual(needCards(hub)[0].said, ["A merge is waiting for you to settle its conflicts."]);
  // Work the list told of, while Home is still reading it.
  let release;
  hub = await card([listed(KEY, "Fixture", open({ working: { changed: 2, differs: true } }))], (page) => { page.held.set(under(KEY, "/api/guide"), new Promise((done) => { release = done; })); });
  assert.deepEqual(needCards(hub)[0].said, ["Uncommitted work is waiting."]);
  assert.ok(needCards(hub)[0].busy > 0, "and is drawn as being read");
  assert.deepEqual(needCards(hub)[0].actions, [["Open", `${URL_OF(KEY)}#/uncommitted`]], "nothing to review until it is read");
  release(); await flush();
  assert.equal(needCards(hub)[0].busy, 0);
  assert.deepEqual(needCards(hub)[0].actions.at(-1), ["Review 2", `${URL_OF(KEY)}#/review/w/1`]);
  // Work Home could not read.
  hub = await card([listed(KEY, "Fixture", open({ working: { changed: 2, differs: true } }))], (page) => { page.failures.set(under(KEY, "/api/guide"), "The working read did not answer within 20 seconds."); });
  assert.deepEqual(needCards(hub)[0].said, ["Uncommitted work is waiting.", "Kin couldn't read what changed here yet. Open the repository to see it."]);
  assert.equal(needCards(hub)[0].busy, 0);
  // Code that differs with no entity changed has nothing to walk.
  hub = await card([listed(KEY, "Fixture", open({ working: { changed: 0, differs: true } }))], (page) => { page.data[under(KEY, "/api/guide")] = guide({}, { working_tree: { differs: true, derived: true, caught_up: true } }); });
  assert.deepEqual(needCards(hub)[0].said, ["Your code differs from your last commit, but no entity changed."]);
  assert.deepEqual(needCards(hub)[0].actions, [["Open", `${URL_OF(KEY)}#/uncommitted`]]);
  assert.equal(needCards(hub)[0].picture, null);
  // A reading Kin has not finished says its counts may grow.
  hub = await card([listed(KEY, "Fixture", open({ working: { changed: 4, differs: true } }))], (page) => { page.data[under(KEY, "/api/guide")] = working({ ...FOUR, completeness: { state: "pending", message: "", pending_entities: 4 } }, fourChanged()); });
  assert.deepEqual(needCards(hub)[0].said.slice(1), ["Kin is still mapping 4 entities, so these counts may grow."]);
  // What this tab last read of a repository's work is shown, said to be read again, until the new reading lands.
  const tabStorage = new Map([[`kin.workspace.working:/repositories/${KEY}`, JSON.stringify({ at: START - 60_000, data: working({ modified: 1 }, [changed(2, "want_bytes")]) })]]);
  hub = await card([listed(KEY, "Fixture", open({ working: { changed: 2, differs: true } }))], (page) => { page.held.set(under(KEY, "/api/guide"), new Promise((done) => { release = done; })); }, { tabStorage });
  assert.deepEqual(needCards(hub)[0].said, ["One function changed. Kin found no connections outside this change. Kin found no tests that reach this work.", "Reading it again"]);
  release(); await flush();
  assert.deepEqual(needCards(hub)[0].said, ["Two functions changed. Three other places rely on them. One was not updated. One test reaches them."]);
  assert.equal(JSON.parse(tabStorage.get(`kin.workspace.working:/repositories/${KEY}`)).data.summary.modified, 2, "the new reading is the one kept");
});

test("an unreviewed agent commit at the head of a clean repository needs the reader, drawn as what it changed and what relies on that", async () => {
  const hub = fixture({ hub: true });
  const rows = [listed(NOTES, "notes", open())];
  hub.data["/api/home"] = homePayload(rows);
  hub.data[under(NOTES, "/api/workspace/overview")] = summaryOf({ recent: [
    { id: "n-0", title: "Update Kin's understanding", author: "Kin", time: secondsAgo(60), kin_checkpoint: true },
    { id: "n-2", title: "Return two from value", author: "Claude Code", agent: { name: "Claude Code", key: "00000000000000a1" }, time: secondsAgo(120), kin_checkpoint: false },
    { id: "n-1", title: "Add the notes index", author: "Ana Writer", time: secondsAgo(86_400), kin_checkpoint: false }] });
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  // The change's own count answers first; its guide, which draws the picture, takes longer.
  hub.data[under(NOTES, "/api/counts")] = { counts: { "n-2": { entities: 2, module_containers: 0, containers_through_members: 0 } } };
  hub.data[under(NOTES, "/api/guide?change")] = recorded("n-2", { modified: 2, recorded_consumers: 2, consumers_may_need_review: 2, recorded_tests: 0 }, [
    changed(2, "value", { authored_by: [{ name: "Claude Code" }], connections: connected([relying(10, "Serializer.dumps")]), consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 1 } }),
    changed(3, "index", { connections: connected([relying(11, "Notes.load")]), consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 1 } })]);
  let release;
  hub.held.set(under(NOTES, "/api/guide?change"), new Promise((done) => { release = done; }));
  answering(hub, rows);
  await flush();
  assert.equal(hub.count(under(NOTES, "/api/standing")), 1, "the standing is read because an agent made the newest commit");
  const [card] = needCards(hub);
  assert.deepEqual([card.repository[0], card.actions], ["notesmainAgent commit", [["Review", `${URL_OF(NOTES)}${guided("n-2")}`]]]);
  // While the guide is read, the change's own count says how much it holds, and nothing else on Home waits for it.
  assert.deepEqual(card.said, ["Claude Code committed “Return two from value” 2 minutes ago, and nobody has reviewed it yet.", "Two entities changed. Reading what relies on them"]);
  assert.ok(card.busy > 0, "the picture is drawn as being read");
  assert.ok(repoCards(hub).length === 1 && hub.node("home-agents").textContent.length > 0, "the rest of Home is drawn meanwhile");
  // Nothing waits on the picture, so a guide that takes longer than a minute is still waited for.
  await hub.advance(90_000);
  assert.deepEqual(needCards(hub)[0].said.at(-1), "Two entities changed. Reading what relies on them");
  release(); await flush();
  const drawn = needCards(hub)[0];
  assert.equal(drawn.busy, 0);
  assert.deepEqual(drawn.picture.columns.map((column) => [column.head, column.boxes.map((box) => [box[0], box[1], box[3]])]), [
    ["Changed2", [["value", "Function in encoding · Claude Code", `${URL_OF(NOTES)}${guided("n-2")}/1`], ["index", "Function in encoding", `${URL_OF(NOTES)}${guided("n-2")}/2`]]],
    ["Rely on them, and did not change2", [["Serializer.dumps", "Method in Serializer", `${URL_OF(NOTES)}#/entity/${uuid(10)}`], ["Notes.load", "Method in Serializer", `${URL_OF(NOTES)}#/entity/${uuid(11)}`]]]]);
  assert.equal(hub.count(under(NOTES, "/api/guide")), 1, "the change is read once, not on every redraw");
  // The commit is said once more, as a recent commit, and not a third time as a list of what agents committed.
  assert.deepEqual(hub.node("home-agents").querySelectorAll("a.agent-commit"), []);
  assert.doesNotMatch(hub.node("home-agents").textContent, /Committed by agents/);
  assert.deepEqual(hub.node("home-recent").querySelectorAll("a").map((node) => node.querySelector(".change-title").textContent), ["Return two from value", "Add the notes index"]);
  // Its repository waits on the reader in the sidebar and among the repositories, after work of the reader's own.
  const notes = hub.nav().querySelectorAll("a.repo-item").find((node) => node.textContent.startsWith("notes"));
  assert.equal(notes.querySelector(".repo-dot").dataset.state, "review");
  // Said as briefly as uncommitted work is, so it fits beside when the repository last changed.
  assert.deepEqual(repoCards(hub)[0].foot, ["1 to review", "2 hours ago"]);
  // Once someone has reviewed it, it no longer needs the reader.
  hub.data[under(NOTES, "/api/standing")] = { standing: { "n-2": { state: "approved" } } };
  hub.data[under(NOTES, "/api/workspace/overview")] = { ...hub.data[under(NOTES, "/api/workspace/overview")], read_at: secondsAgo(-1), counts: { ok: true, entities: 11, relationships: 5 } };
  hub.data["/api/home"] = homePayload([{ ...rows[0], last_change_at: secondsAgo(1) }]);
  await hub.advance(10_000);
  assert.deepEqual(needCards(hub), []);
  assert.equal(hub.nav().querySelectorAll("a.repo-item").find((node) => node.textContent.startsWith("notes")).querySelector(".repo-dot").dataset.state, undefined);
});

test("two entities of one name on a Home card are told apart by where each sits, as the review tells them apart", async () => {
  const hub = fixture({ hub: true });
  const rows = [listed(NOTES, "zod", open())];
  hub.data["/api/home"] = homePayload(rows);
  hub.data[under(NOTES, "/api/workspace/overview")] = summaryOf({ recent: [{ id: "z-1", title: "Lowercase strings with the locale's rules", author: "Claude Code", agent: { name: "Claude Code", key: "00000000000000a1" }, time: secondsAgo(120), kin_checkpoint: false }] });
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  hub.data[under(NOTES, "/api/counts")] = { counts: {} };
  // Two modules named checks rely on the changed function, and the guide places neither in anything.
  const checks = (n) => relying(n, "checks", { relation: "imports", entity: { id: uuid(n), name: "checks", kind: "module", role: "source", within: "" } });
  hub.data[under(NOTES, "/api/guide?change")] = recorded("z-1", { modified: 1, recorded_consumers: 3, consumers_may_need_review: 3 }, [
    changed(2, "_toLowerCase", { within: "module api", connections: connected([relying(10, "_ZodString", { entity: { id: uuid(10), name: "_ZodString", kind: "function", within: "module schemas" } }), checks(11), checks(12)]), consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 3 } })]);
  // Each is read once, from its own repository, for the place Kin records it under.
  hub.data[under(NOTES, "/api/workspace/entity")] = (url) => ({ header: { module: url.searchParams.get("id") === uuid(11) ? "zod.src.v4.classic.checks" : "zod.src.v4.mini.checks" } });
  answering(hub, rows);
  await flush(); await flush();
  const boxes = needCards(hub)[0].picture.columns[1].boxes.map(([name, place]) => [name, place]);
  assert.deepEqual(boxes, [["_ZodString", "Function in schemas"], ["checks in classic", "Module"], ["checks in mini", "Module"]]);
  assert.deepEqual(hub.requests.filter((request) => request.name.endsWith("/api/workspace/entity")).map((request) => [request.name, new URL(request.url, "http://local.test").searchParams.get("id")]),
    [[under(NOTES, "/api/workspace/entity"), uuid(11)], [under(NOTES, "/api/workspace/entity"), uuid(12)]]);
});

test("an agent change Home could not read in full is still said, with its count, and asked about again later", async () => {
  const hub = fixture({ hub: true });
  const rows = [listed(NOTES, "notes", open())];
  hub.data["/api/home"] = homePayload(rows);
  hub.data[under(NOTES, "/api/workspace/overview")] = summaryOf({ recent: [
    { id: "n-2", title: "Return two from value", author: "Claude Code", agent: { name: "Claude Code", key: "00000000000000a1" }, time: secondsAgo(120), kin_checkpoint: false }] });
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  hub.data[under(NOTES, "/api/counts")] = { counts: { "n-2": { entities: 1, module_containers: 0, containers_through_members: 0 } } };
  hub.failures.set(under(NOTES, "/api/guide?change"), "The review guide did not answer within 45 seconds.");
  answering(hub, rows);
  await flush();
  assert.deepEqual(needCards(hub)[0].said, ["Claude Code committed “Return two from value” 2 minutes ago, and nobody has reviewed it yet.", "One entity changed. Kin couldn't read what relies on it yet. Its review has the rest."]);
  assert.equal(needCards(hub)[0].busy, 0);
  hub.failures.delete(under(NOTES, "/api/guide?change"));
  hub.data[under(NOTES, "/api/guide?change")] = recorded("n-2", { modified: 1, recorded_consumers: 1, consumers_may_need_review: 1 }, [
    changed(2, "value", { connections: connected([relying(10, "Serializer.dumps")]), consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 1 } })]);
  await hub.advance(10_000);
  assert.equal(hub.count(under(NOTES, "/api/guide")), 1, "not asked again at once");
  await hub.advance(30_000);
  assert.equal(needCards(hub)[0].picture.columns[0].boxes[0][0], "value");
});

test("Home shows the agents at work across the repositories it has open, each in its own words and leading into its repository", async () => {
  const hub = fixture({ hub: true }); await flush();
  assert.equal(hub.node("home-agents").querySelector("h2").textContent, "Agents1");
  assert.deepEqual(hub.node("home-agents").querySelectorAll("a").filter((node) => !node.dataset.state).map((node) => [node.textContent, node.getAttribute("href")]).slice(0, 1), [["Connect an agent", "#/agents"]]);
  assert.deepEqual(agentCards(hub), [{ head: ["Claude Code", "in pallets / Fixture", "Working"], action: "Checked what a change to want_bytes would touch", links: [["want_bytes", `${URL_OF(KEY)}#/entity/${uuid(2)}`]],
    before: ["Checked what uses Signer.verify_signature", "Read Signer.verify_signature"], note: "7 calls · Connected for 4 minutes", href: `${URL_OF(KEY)}#/agents`, state: "working" }]);
  // Idle agents in another repository, newest first, at most six.
  const idle = (n, name) => ({ ...agentsPayload().agents[0], key: `00000000000000c${n}`, name, state: "idle", recent_calls: [], last_activity_at: secondsAgo(n * 10), calls_total: 0 });
  hub.data[under(NOTES, "/api/workspace/agents")] = agentsPayload({ agents: [1, 2, 3, 4, 5, 6].map((n) => idle(n, `Lantern ${n}`)) });
  hub.data["/api/home"] = homePayload([hub.data["/api/home"].repositories[0], { ...hub.data["/api/home"].repositories[1], agents: { active: 0, connected: 6, names: [] } }, hub.data["/api/home"].repositories[2]]);
  await hub.advance(10_000);
  assert.equal(hub.node("home-agents").querySelector("h2").textContent, "Agents7");
  assert.deepEqual(agentCards(hub).map((card) => card.head[0]), ["Claude Code", "Lantern 1", "Lantern 2", "Lantern 3", "Lantern 4", "Lantern 5"]);
  assert.deepEqual(agentCards(hub)[1], { head: ["Lantern 1", "in notes", "Idle"], action: "Connected. It has not asked Kin for anything yet.", links: [], before: [], note: "0 calls · Connected for 4 minutes, quiet for 20 seconds", href: `${URL_OF(NOTES)}#/agents`, state: "idle" });

  // With none connected, the last one that worked is named, where and when.
  const quiet = fixture({ hub: true });
  quiet.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [], earlier: [{ ...agentsPayload().agents[0], key: "00000000000000b2", name: "Lantern", state: "ended", last_activity_at: secondsAgo(720) }] });
  quiet.data["/api/home"] = homePayload(quiet.data["/api/home"].repositories.map((row) => ({ ...row, agents: row.agents && { active: 0, connected: 0, names: [] } })));
  await flush();
  assert.equal(quiet.node("home-agents").querySelector("h2").textContent, "Agents");
  // The session is said by what it came to, the change it committed, and not by the last call it made.
  assert.equal(quiet.node("home-agents").textContent, "AgentsConnect an agentNo agent is connected.Lantern last worked in pallets / Fixture 12 minutes ago. Committed “Explain how want_bytes treats text”.");
  assert.deepEqual(quiet.node("home-agents").querySelectorAll("p")[1].querySelectorAll("a").map((node) => node.getAttribute("href")), [`${URL_OF(KEY)}#/agents`]);
  // Once Home has read how its review stands, it says the change waits for the reader.
  const waiting = fixture({ hub: true });
  waiting.data[under(KEY, "/api/workspace/agents")] = quiet.data[under(KEY, "/api/workspace/agents")];
  waiting.data["/api/home"] = quiet.data["/api/home"];
  waiting.data[under(KEY, "/api/workspace/overview")] = summaryOf({ recent: [{ id: AGENT_CHANGE, title: "Explain how want_bytes treats text", author: "Lantern", agent: { name: "Lantern", key: "00000000000000b2" }, time: secondsAgo(720), kin_checkpoint: false }] });
  waiting.data[under(KEY, "/api/standing")] = { standing: {} };
  waiting.data[under(KEY, "/api/counts")] = { counts: {} };
  waiting.data[under(KEY, "/api/guide?change")] = recorded(AGENT_CHANGE, { modified: 1 }, [changed(2, "want_bytes")]);
  await flush();
  assert.match(waiting.node("home-agents").textContent, /Lantern last worked in pallets \/ Fixture 12 minutes ago\. Committed “Explain how want_bytes treats text”, waiting for your review\.$/);
  // A session that committed nothing is said by its last call, a failed one by what it tried.
  const tried = fixture({ hub: true });
  tried.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [], earlier: [{ ...agentsPayload().agents[0], key: "00000000000000b3", name: "Lantern", state: "ended", open_changes: [], changes: [], changes_total: 0, edited: [], last_activity_at: secondsAgo(720),
    recent_calls: [{ seq: 9, at: secondsAgo(720), tool: "kin_transaction_abort", entity_ids: [], result_count: 0, duration_ms: 27_900, is_error: true }] }] });
  tried.data["/api/home"] = quiet.data["/api/home"];
  await flush();
  assert.match(tried.node("home-agents").textContent, /Lantern last worked in pallets \/ Fixture 12 minutes ago\. Tried to drop the change it had started; Kin did not answer within 28 s\.$/);
  const none = fixture({ hub: true });
  none.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [] });
  delete none.data["/api/home/clients"];
  await flush();
  assert.equal(none.node("home-agents").textContent, "AgentsNo agent is connected.A coding agent works in your repositories through Kin once it is connected.", "a server that lists no coding agents offers no way to connect one");
});

test("each repository is a card with how large it is, what it is written in and what is in motion in it", async () => {
  const hub = fixture({ hub: true });
  const rows = [
    listed(ARCHIVE, "archive", { last_change_at: secondsAgo(30 * 86_400) }),
    listed(NOTES, "notes", { ...open(), last_change_at: secondsAgo(86_400) }),
    listed(repositoryKey("1"), "busy", { ...open({ agents: { active: 2, connected: 2, names: [] } }), last_change_at: secondsAgo(4000) }),
    listed(KEY, "Fixture", { owner: "pallets", host: "github.com", ...open({ working: { changed: 2, differs: true }, agents: { active: 1, connected: 1, names: ["Claude Code"] } }), last_change_at: secondsAgo(600) }),
    listed(repositoryKey("d"), "merging", { ...open({ merge_in_progress: true }), last_change_at: secondsAgo(3000) }),
    listed(repositoryKey("e"), "elsewhere", { url: "https://elsewhere.example/repositories/x/workspace/", last_change_at: null, entities: null }),
    listed(repositoryKey("f"), "script", { url: "javascript:alert(1)", last_change_at: null })];
  hub.data["/api/home"] = homePayload(rows, { unavailable: 2, warning: "One entry in the registry could not be read." });
  answering(hub, rows);
  hub.data[under(repositoryKey("1"), "/api/workspace/overview")] = summaryOf({ entities: 3200, relationships: 9100, languages: [["rust", 2950], ["toml", 40], ["python", 10]] });
  hub.data[under(repositoryKey("d"), "/api/workspace/overview")] = { schema: "kin.workspace.overview.v1", counts: { ok: false, reason: "Kin could not count this repository's entities." }, languages: { ok: false, reason: "No languages." } };
  await flush();
  assert.equal(hub.node("home-repos").querySelector("h2").textContent, "Repositories7");
  // What needs the reader first, then where agents are at work, then the most recently changed.
  assert.deepEqual(repoCards(hub), [
    { name: "pallets / Fixture", branch: "main", stats: ["216 entities", "602 connections"], languages: ["Python 97%", "TOML 3%"], foot: ["2 uncommitted", "10 minutes ago"], href: URL_OF(KEY), state: "work", live: "Claude Code is working" },
    { name: "merging", branch: "main", stats: ["216 entities"], languages: [], foot: ["A merge to settle", "50 minutes ago"], href: URL_OF(repositoryKey("d")), state: "clean" },
    { name: "busy", branch: "main", stats: ["3,200 entities", "9,100 connections"], languages: ["Rust 98%", "TOML 1%"], foot: ["All committed", "1 hour ago"], href: URL_OF(repositoryKey("1")), state: "clean", live: "2 agents are working" },
    { name: "notes", branch: "main", stats: ["40 entities", "51 connections"], languages: ["Rust 100%"], foot: ["All committed", "yesterday"], href: URL_OF(NOTES), state: "clean" },
    { name: "archive", branch: "", stats: ["12 entities", "9 connections"], languages: ["Go 100%"], foot: ["All committed", "last month"], href: URL_OF(ARCHIVE), state: "clean" },
    { name: "elsewhere", branch: "", stats: ["10 entities", "5 connections"], languages: ["Python 100%"], foot: ["All committed"], href: "/", state: "clean" },
    // Home opens six; the seventh says what the list said of it, and nothing about its work.
    { name: "script", branch: "", stats: ["216 entities"], languages: [], foot: [], href: "/", state: "closed" }],
    "a card leads to its own repository, and never off this server");
  assert.match(hub.node("home-repos").textContent, /2 repositories Kin knew about are no longer on this machine\./);
  assert.match(hub.node("home-repos").textContent, /One entry in the registry could not be read\./);
  // Before Home has opened a repository, its card says what the list said and no more.
  const early = fixture({ hub: true });
  early.held.set(under(KEY, "/api/workspace/overview"), new Promise(() => {}));
  await flush();
  assert.deepEqual(repoCards(early)[0], { name: "pallets / Fixture", branch: "main", stats: ["216 entities"], languages: [], foot: ["Uncommitted work", "10 minutes ago"], href: URL_OF(KEY), state: "work", live: "Claude Code is working" });
  // A share of the whole never rounds a language up to all of it, and one too small to see is left out.
  const shares = fixture({ hub: true });
  shares.data[under(KEY, "/api/workspace/overview")] = summaryOf({ languages: [["python", 995], ["toml", 5]] });
  await flush();
  assert.deepEqual(repoCards(shares)[0].languages, ["Python 99%"]);
});

test("a repository card says what Kin is busy with there while nothing in it asks for the reader", async () => {
  const hub = fixture({ hub: true });
  const rows = [
    listed(repositoryKey("1"), "indexing", { ...open(), activity: { phase: "indexing" }, last_change_at: secondsAgo(100) }),
    listed(repositoryKey("2"), "history", { ...open(), activity: { phase: "reading_history" }, history: { admitted: 1200, total: 4000 }, last_change_at: secondsAgo(200) }),
    listed(repositoryKey("3"), "linking", { ...open({ working: { changed: 2, differs: true } }), activity: { phase: "enriching" }, last_change_at: secondsAgo(300) }),
    listed(repositoryKey("4"), "unheard", { ...open(), activity: { phase: "something_new" }, last_change_at: secondsAgo(400) })];
  hub.data["/api/home"] = homePayload(rows);
  answering(hub, rows);
  hub.data[under(repositoryKey("3"), "/api/guide")] = working({ modified: 2 }, [changed(2, "want_bytes"), changed(5, "base64_encode")]);
  await flush();
  assert.deepEqual(repoCards(hub).map((card) => [card.name, card.foot.slice(0, -1)]), [
    ["linking", ["2 uncommitted"]], ["indexing", ["Building the graph"]], ["history", ["Reading its history"]], ["unheard", ["All committed"]]],
    "work to commit is said before what Kin is doing, and a phase Kin has no words for here is not shown");
  assert.deepEqual(hub.nav().querySelectorAll("a[title]").filter((node) => !node.getAttribute("aria-label")).map((node) => [node.getAttribute("title"), node.children[0].dataset.state ?? ""]),
    [["linking", "work"], ["history", ""], ["indexing", ""], ["unheard", ""]], "and the sidebar marks only the work");
});

test("code Kin has seen differ before counting it asks nothing of the reader, and is not said to be read once Home has read it", async () => {
  const hub = fixture({ hub: true });
  hub.data["/api/home"] = homePayload([listed(KEY, "Fixture", open({ working: { differs: true } }))]);
  hub.data[under(KEY, "/api/guide")] = guide();
  await flush();
  assert.equal(homeReads(hub)[KEY].at(-1), "guide?working=1", "Home reads the work to count it");
  assert.deepEqual(needCards(hub), []);
  assert.equal(repoCards(hub)[0].foot[0], "All committed");
});

test("Home lists what was committed lately across the repositories it has open, newest first", async () => {
  const hub = fixture({ hub: true }); await flush();
  const rows = () => hub.node("home-recent").querySelectorAll("a").map((row) => [row.querySelector("span").children.map((part) => part.textContent), row.getAttribute("href")]);
  assert.equal(hub.node("home-recent").querySelector("h2").textContent, "Committed in the last two weeks");
  assert.deepEqual(rows(), [
    [["Bound base64 input", "pallets / FixtureDana Reader10 minutes ago"], `${URL_OF(KEY)}#/review/c/change-1`],
    [["Add the notes index", "notesAna Writeryesterday"], `${URL_OF(NOTES)}#/review/c/n-1`]], "Kin's own updates are not listed");
  // Six at most, the newest across every repository first.
  hub.data[under(ARCHIVE, "/api/workspace/overview")] = summaryOf({ recent: Array.from({ length: 7 }, (_, i) => ({ id: `a-${i}`, title: `Archive change ${i}`, author: "Ana Writer", time: secondsAgo(60 + i * 1000), kin_checkpoint: false })) });
  hub.data["/api/home"] = homePayload([...hub.data["/api/home"].repositories.slice(0, 2), { ...hub.data["/api/home"].repositories[2], last_change_at: secondsAgo(60) }]);
  await hub.advance(10_000);
  assert.deepEqual(rows().map(([[title]]) => title), ["Archive change 0", "Bound base64 input", "Archive change 1", "Archive change 2", "Archive change 3", "Archive change 4"]);
});

test("a question on Home is put to each repository it has open, and each answers from its own graph", async () => {
  const hub = fixture({ hub: true });
  hub.data[under(KEY, "/api/workspace/ask")] = ASKED();
  hub.data[under(NOTES, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  hub.failures.set(under(ARCHIVE, "/api/workspace/ask"), "The index is not ready.");
  await flush();
  const asked = () => Object.fromEntries(Object.entries(homeReads(hub)).map(([key, reads]) => [key, reads.filter((read) => read.startsWith("ask"))]));
  assert.equal(hub.node("home-ask-input").getAttribute("placeholder"), "Ask your code, across every repository");
  for (const typed of ["s", "si", "sign"]) { hub.type("home-ask-input", typed); await hub.advance(100); }
  assert.deepEqual(asked(), { [KEY]: [], [NOTES]: [], [ARCHIVE]: [] }, "nothing is asked while the reader is still typing");
  await hub.advance(220);
  assert.equal(hub.node("home-body").hidden, true, "the question has the page to itself");
  assert.deepEqual(asked(), { [KEY]: ["ask?q=sign&limit=10"], [NOTES]: ["ask?q=sign&limit=10"], [ARCHIVE]: ["ask?q=sign&limit=10"] });
  const answers = hub.node("home-ask").querySelectorAll("section");
  assert.deepEqual(answers.map((answer) => answer.querySelector("h2").textContent), ["pallets / Fixture"], "only the repositories with an answer are listed");
  assert.deepEqual(answers[0].querySelectorAll("a").map((node) => node.getAttribute("href")), [`${URL_OF(KEY)}#/explore?q=sign`, `${URL_OF(KEY)}#/entity/${uuid(5)}`, `${URL_OF(KEY)}#/entity/${uuid(2)}`],
    "the rest of the answer, and each entity, open in that repository");
  assert.equal(answers[0].querySelector("a").textContent, "More here");
  assert.equal(hub.node("workspace-announcer").textContent, "2 results across 3 repositories.");
  // Escape puts the page back as it was. Enter asks at once.
  assert.equal(hub.node("home-ask-input").emit("keydown", { key: "Escape" }).defaultPrevented, true);
  assert.equal(hub.node("home-body").hidden, false);
  assert.equal(hub.node("home-ask").textContent, "");
  hub.data[under(KEY, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  hub.failures.delete(under(ARCHIVE, "/api/workspace/ask"));
  hub.data[under(ARCHIVE, "/api/workspace/ask")] = hub.data[under(KEY, "/api/workspace/ask")];
  hub.node("home-ask-input").value = "zzz";
  hub.node("home-ask-input").emit("keydown", { key: "Enter" }); await flush();
  assert.equal(asked()[KEY].at(-1), "ask?q=zzz&limit=10");
  assert.equal(hub.node("home-ask").textContent, "Nothing in these repositories matches that.");
  assert.equal(hub.node("workspace-announcer").textContent, "No results.");
  // An answer to a question the reader has since changed is not shown.
  let release; hub.held.set(under(KEY, "/api/workspace/ask"), new Promise((done) => { release = done; }));
  hub.data[under(KEY, "/api/workspace/ask")] = (url) => url.searchParams.get("q") === "sign" ? ASKED() : { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  hub.type("home-ask-input", "sign"); await hub.advance(320);
  hub.held.delete(under(KEY, "/api/workspace/ask"));
  hub.type("home-ask-input", "nothing"); await hub.advance(320);
  release(); await flush();
  assert.equal(hub.node("home-ask").textContent, "Nothing in these repositories matches that.");
  // A slash, or the search key with K, puts the reader in the question.
  hub.document.activeElement = hub.document.body;
  assert.equal(hub.document.emit("keydown", { key: "/" }).defaultPrevented, true);
  assert.equal(hub.document.activeElement.id, "home-ask-input");
  hub.document.activeElement = hub.document.body;
  hub.document.emit("keydown", { key: "k", metaKey: true });
  assert.equal(hub.document.activeElement.id, "home-ask-input");
});

test("a question on Home leads with the strongest answer: a traced chain first, then names that hold the question's words, and a loose match is one line", async () => {
  const hub = fixture({ hub: true });
  const row = (n, name, kind, fields = {}) => ({ id: uuid(n), name, kind, matched: ["meaning"], why: "Its embedding is close to the question.", ...fields });
  // The first repository Home lists matched only by meaning, and leads the list it was asked in.
  hub.data[under(KEY, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", total: 3, results: [
    row(40, "lazyWithGetterOverride", "function", { signature: "function lazyWithGetterOverride<T>(getter: () => T)", container: { id: uuid(41), name: "zod.src.v4.core.util", kind: "module" } }),
    row(42, "ZodLazyDef.getter", "method", { signature: "getter: () => T", container: { id: uuid(43), name: "ZodLazyDef", kind: "interface" } }),
    row(44, "GET", "function", { signature: "async function GET()" })] };
  // The second answered with a chain Kin traced, and a match whose name holds every word asked.
  hub.data[under(NOTES, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", total: 2,
    answer: { sentence: "Reply.send calls serialize.", steps: [{ id: uuid(50), name: "Reply.send", kind: "method", relation_to_next: "calls" }, { id: uuid(51), name: "serialize", kind: "function" }] },
    results: [row(52, "Reply.getSerializationFunction", "method", { signature: "Reply.prototype.getSerializationFunction = function (schemaOrStatus)", matched: ["name", "meaning"] }),
      row(53, "port", "method", { signature: "port?: number", container: { id: uuid(54), name: "FastifyListenOptions", kind: "interface" } }),
      row(55, "RecordKeysToLowercase", "typealias", { signature: "type RecordKeysToLowercase<Input> = Input" })] };
  // The third repeats one name in many places.
  hub.data[under(ARCHIVE, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", total: 6, results: [
    ...[60, 61, 62, 63, 64].map((n) => row(n, "read_items", "function", { container: { id: uuid(n + 10), name: `docs_src.tutorial${n}.py310`, kind: "module" } })),
    row(65, "items_router", "function")] };
  await flush();
  hub.type("home-ask-input", "how does a reply get serialized"); await hub.advance(320);
  const answers = hub.node("home-ask").querySelectorAll("section");
  assert.deepEqual(answers.map((answer) => answer.querySelector("h2").textContent), ["notes"], "the traced answer comes first, and only strong answers are drawn in full");
  assert.equal(answers[0].querySelector(".answer").querySelector("p").textContent, "Reply.send calls serialize.");
  const rows = answers[0].querySelectorAll(".entity-row").map((node) => [node.querySelector(".ent-name").textContent, node.querySelector(".entity-container").textContent]);
  assert.deepEqual(rows, [["Reply.getSerializationFunction", "Method"], ["port", "Property in FastifyListenOptions"], ["RecordKeysToLowercase", "Type alias"]],
    "a member declared as a value is a property, and a type alias is said as two words");
  // A repository whose matches name none of the question's words, or one name many times over, is one line each.
  const loose = hub.node("home-ask").querySelectorAll(".home-answer-loose").map((node) => [node.querySelector(".home-answer-loose-name").textContent, node.querySelector(".home-answer-loose-text").textContent]);
  assert.deepEqual(loose, [["pallets / Fixture", "Only loose matches: lazyWithGetterOverride, ZodLazyDef.getter and 1 more."], ["archive", "Only loose matches: read_items, items_router and 4 more."]]);
  // Opening one draws it in full, with each name once and its repeats counted, and modules by their own names.
  hub.node("home-ask").querySelectorAll(".home-answer-loose")[1].querySelector("button").click(); await flush();
  const archive = hub.node("home-ask").querySelectorAll("section").find((node) => node.querySelector("h2").textContent === "archive");
  assert.deepEqual(archive.querySelectorAll(".entity-row").map((node) => node.querySelector(".entity-line").textContent), ["read_itemsFunction in py310and 4 more of this name", "items_routerFunction"]);
  hub.data[under(ARCHIVE, "/api/workspace/ask")].results[0].container.name = "zod.src.v4.classic.schemas";
  hub.type("home-ask-input", "read items"); await hub.advance(320);
  assert.match(hub.node("home-ask").textContent, /read_itemsFunction in schemasand 4 more of this name/, "a module recorded under a dotted place is named as code names it");
  hub.data[under(ARCHIVE, "/api/workspace/ask")].results[0].container.name = "extract.ws";
  hub.type("home-ask-input", "read item"); await hub.advance(320);
  assert.match(hub.node("home-ask").textContent, /read_itemsFunction in wsand 4 more of this name/);
  hub.data[under(ARCHIVE, "/api/workspace/ask")].results[0].container.name = "apply.test";
  hub.type("home-ask-input", "reads item"); await hub.advance(320);
  assert.match(hub.node("home-ask").textContent, /read_itemsFunction in apply\.testand 4 more of this name/, "a suffix of the module's own name stays");
});

test("a repository that answers a question by name shows those names, folds its looser matches into one line, and names modules as code does", async () => {
  const hub = fixture({ hub: true });
  const row = (n, name, kind, fields = {}) => ({ id: uuid(n), name, kind, matched: ["meaning"], why: "Its description and code share words with the question.", ...fields });
  // As zod's ask answers it: a method placed only by the module it is recorded under.
  hub.data[under(KEY, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", total: 3, results: [
    row(70, "_ZodString.lowercase", "method", { module: "zod.src.v4.classic.schemas", signature: "lowercase(params?: string): this" }),
    row(71, "_lowercase", "function", { container: { id: uuid(72), name: "api", kind: "module" }, module: "zod.src.v4.core.api" }),
    row(73, "$ZodCheckLowerCaseDef", "interface", { container: { id: uuid(74), name: "checks", kind: "module" } })] };
  // As fastify's answers it: one name that holds a word of the question, and four that do not.
  hub.data[under(NOTES, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", total: 5, results: [
    row(80, "RecordKeysToLowercase", "typealias", { container: { id: uuid(81), name: "utils", kind: "module" } }),
    row(82, "ResolveFastifyRequestType.headers", "method", { signature: "headers: RecordKeysToLowercase<Headers>" }),
    row(83, "HTTPMethods", "typealias", { container: { id: uuid(81), name: "utils", kind: "module" } }),
    row(84, "FastifyLoggerOptions.serializers", "method", { signature: "serializers?: { req?: unknown }" }),
    row(85, "FastifyReply.serializeInput", "method", { signature: "serializeInput(input: unknown): string" })] };
  hub.data[under(ARCHIVE, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", total: 0, results: [] };
  await flush();
  hub.type("home-ask-input", "where are strings lowercased"); await hub.advance(320);
  const answers = hub.node("home-ask").querySelectorAll("section");
  assert.deepEqual(answers.map((answer) => answer.querySelector("h2").textContent), ["pallets / Fixture", "notes"], "the repository whose names hold more of the question leads");
  const rows = (answer) => answer.querySelectorAll(".entity-row").map((node) => [node.querySelector(".ent-name").textContent, node.querySelector(".entity-container").textContent]);
  assert.deepEqual(rows(answers[0]), [["_ZodString.lowercase", "Method in schemas"], ["_lowercase", "Function in api"], ["$ZodCheckLowerCaseDef", "Interface in checks"]]);
  assert.deepEqual(rows(answers[1]), [["RecordKeysToLowercase", "Type alias in utils"]]);
  assert.equal(answers[1].querySelector(".home-answer-looser").querySelector(".home-answer-loose-text").textContent, "Also 4 loose matches: ResolveFastifyRequestType.headers, HTTPMethods and 2 more.");
  answers[1].querySelector(".home-answer-looser").querySelector("button").click(); await flush();
  const opened = hub.node("home-ask").querySelectorAll("section").find((node) => node.querySelector("h2").textContent === "notes");
  assert.deepEqual(rows(opened).map(([name]) => name), ["RecordKeysToLowercase", "ResolveFastifyRequestType.headers", "HTTPMethods", "FastifyLoggerOptions.serializers", "FastifyReply.serializeInput"]);
  assert.equal(opened.querySelector(".home-answer-looser"), null);
});

test("a repository that could not answer a question on Home is not said to have nothing that matches", async () => {
  const hub = fixture({ hub: true });
  hub.data[under(KEY, "/api/workspace/ask")] = ASKED();
  hub.data[under(NOTES, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  hub.failures.set(under(ARCHIVE, "/api/workspace/ask"), "The index is not ready.");
  await flush();
  hub.type("home-ask-input", "sign"); await hub.advance(320);
  assert.doesNotMatch(hub.node("home-ask").textContent, /Nothing in [^.]*archive[^.]* matches/);
  // Nor is one whose search by meaning did not answer and whose names hold nothing.
  hub.failures.delete(under(ARCHIVE, "/api/workspace/ask"));
  hub.data[under(ARCHIVE, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "name_only", notice: "Meaning search could not answer, so these are name matches only.", results: [], total: 0, names: { ok: true, total: 0 } };
  hub.type("home-ask-input", "signs"); await hub.advance(320);
  assert.doesNotMatch(hub.node("home-ask").textContent, /Nothing in [^.]*archive[^.]* matches/);
  assert.match(hub.node("home-ask").textContent, /Kin couldn't ask archive just now\./);
});

test("each part of Home is drawn again only when what it shows has changed", async () => {
  const hub = fixture({ hub: true });
  let releaseArchive;
  hub.held.set(under(ARCHIVE, "/api/guide"), new Promise((done) => { releaseArchive = done; }));
  await flush();
  const parts = ["home-headline", "home-attention", "home-agents", "home-recent"];
  const drawn = () => parts.map((id) => hub.node(id).children[0]);
  const before = drawn();
  releaseArchive();
  await flush();
  assert.deepEqual(drawn().map((node, i) => node === before[i]), [true, true, true, true], "a clean repository finishing its working read leaves the attention card and its lines untouched");
  await hub.advance(10_000);
  assert.deepEqual(drawn().map((node, i) => node === before[i]), [true, true, true, true], "a reading that says the same draws nothing");
  hub.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [{ ...agentsPayload().agents[0], calls_total: 8, recent_calls: [{ seq: 8, at: secondsAgo(1), tool: "get_entity_source", entity_ids: [uuid(3)], result_count: 1, duration_ms: 5, is_error: false }, ...agentsPayload().agents[0].recent_calls] }] });
  await hub.advance(10_000);
  assert.deepEqual(drawn().map((node, i) => node === before[i]), [true, true, false, true], "an agent's new call draws the agents again, and nothing else");
  assert.equal(agentCards(hub)[0].action, "Read Signer");
});

test("a repository card the reader has reached with the keyboard keeps the focus while Home reads again", async () => {
  const hub = fixture({ hub: true }); await flush();
  hub.node("home-repos").querySelectorAll("a[data-state]")[1].focus();
  await hub.advance(10_000);
  assert.equal(hub.document.activeElement.isConnected, true);
});

test("Home opened from another page of the machine reads its repositories straight away", async () => {
  const hub = fixture({ hub: true, hash: "#/agents" }); await flush();
  hub.navigate(""); await flush();
  assert.equal(homeReads(hub)[KEY]?.[0], "overview");
  assert.deepEqual(repoCards(hub)[0].stats, ["216 entities", "602 connections"]);
});

test("Home's line about agents agrees with the agent cards under it", async () => {
  const hub = fixture({ hub: true });
  hub.data["/api/home"] = homePayload([listed(ARCHIVE, "archive")]);
  hub.data[under(ARCHIVE, "/api/workspace/agents")] = agentsPayload();
  await flush();
  assert.deepEqual(agentCards(hub).map((card) => card.head.at(-1)), ["Working"]);
  assert.equal(headline(hub)[1], "One agent is working. One repository on this machine.");
  hub.data[under(ARCHIVE, "/api/workspace/agents")] = agentsPayload({ agents: [] });
  hub.data["/api/home"] = homePayload([listed(ARCHIVE, "archive", { last_change_at: secondsAgo(5) })]);
  await hub.advance(10_000);
  assert.deepEqual(agentCards(hub), []);
  assert.equal(headline(hub)[1], "No agent is connected. One repository on this machine.");
});

/** Everything Home says at once: its headline, the note under it, each card's repository, each repository card's foot and agent line, the agents panel, and the sidebar's dots. */
function homeSays(hub) {
  return { headline: headline(hub), known: hub.node("home-headline").querySelector(".home-known")?.textContent ?? "",
    needs: needCards(hub).map((card) => card.repository[0]), cards: repoCards(hub).map((card) => [card.name, card.foot, card.live ?? ""]),
    agents: agentCards(hub).map((card) => card.head), quiet: hub.node("home-agents").querySelector(".quiet-card-title")?.textContent ?? "",
    side: sideRepositories(hub).map((row) => [row.name, row.mark]) };
}

test("Home holds still while a repository's daemon stops answering, and says that what it shows was last known", async () => {
  const hub = fixture({ hub: true });
  await flush();
  const before = homeSays(hub);
  assert.deepEqual(before.headline, ["pallets / Fixture needs you.", "One agent is working. Three repositories on this machine."]);
  assert.deepEqual(before.side.find(([name]) => name === "pallets / Fixture"), ["pallets / Fixture", "work"]);
  assert.equal(before.known, "");
  // The daemon stops answering: an older server lists the repository with nothing in motion, and the repository's own reads fail.
  const rows = () => hub.data["/api/home"].repositories;
  const [fixtureRow, ...rest] = rows();
  const { working, agents, branch, merge_in_progress, ...silent } = fixtureRow;
  hub.data["/api/home"] = homePayload([{ ...silent, runtime: "idle" }, ...rest]);
  for (const part of ["/api/workspace/agents", "/api/guide", "/api/workspace/overview"]) hub.failures.set(under(KEY, part), "kin daemon is required: the daemon did not answer.");
  const at = new Date(START).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  // Its card says when what it shows was known, in place of when it last changed.
  const asOf = (cards) => cards.map(([name, foot, live]) => [name, name === "pallets / Fixture" ? [foot[0], `As of ${at}`] : foot, live]);
  for (let poll = 0; poll < 4; poll++) {
    await hub.advance(10_000);
    const now = homeSays(hub);
    // Every part says what it said before, and one line says why.
    assert.deepEqual({ ...now, known: "" }, { ...before, cards: asOf(before.cards), known: "" }, `poll ${poll}: nothing moved but the daemon`);
    assert.equal(now.known, `Kin can't reach pallets / Fixture right now; showing what it knew at ${at}.`);
  }
  // A card of what needs the reader says it too.
  assert.match(hub.node("home-attention").textContent, /Kin can't reach pallets \/ Fixture right now; showing what it knew at/);
  // The daemon answers again: the note goes, and nothing else moved.
  for (const part of ["/api/workspace/agents", "/api/guide", "/api/workspace/overview"]) hub.failures.delete(under(KEY, part));
  hub.data["/api/home"] = homePayload([fixtureRow, ...rest]);
  await hub.advance(10_000);
  const untimed = (says) => ({ ...says, cards: says.cards.map(([name, foot, live]) => [name, foot.slice(0, 1), live]) });
  assert.deepEqual(untimed(homeSays(hub)), untimed(before));
  assert.equal(homeSays(hub).known, "");
});

test("a repository the server says it cannot reach is shown as last known with when, and a stopped one holds no agent", async () => {
  const hub = fixture({ hub: true });
  const knownAt = secondsAgo(600);
  const at = new Date(Date.parse(knownAt)).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  hub.data["/api/home"] = homePayload([
    listed(KEY, "Fixture", { owner: "pallets", host: "github.com", ...open({ working: { changed: 2, differs: true }, agents: { active: 1, connected: 1, names: ["Claude Code"] } }), reach: "not_answering", known_at: knownAt }),
    listed(NOTES, "notes", { runtime: "idle", reach: "stopped", known_at: knownAt, branch: "main", working: { changed: 4, differs: true } })]);
  hub.failures.set(under(KEY, "/api/workspace/agents"), "kin daemon is required");
  hub.data[under(NOTES, "/api/guide")] = working({ added: 4 }, [changed(40, "notes"), changed(41, "addNote"), changed(42, "findNote"), changed(43, "main")]);
  answering(hub, hub.data["/api/home"].repositories);
  await flush();
  const says = homeSays(hub);
  assert.deepEqual(says.headline, ["Two repositories need you.", "One agent is working. Two repositories on this machine."]);
  assert.equal(says.known, `Kin can't reach pallets / Fixture right now; showing what it knew at ${at}. Kin can't reach notes right now; showing what it knew at ${at}.`);
  // The stopped one's work stays, with its dot, and no agent is said to be in it.
  assert.deepEqual(says.side.find(([name]) => name === "notes"), ["notes", "work"]);
  assert.deepEqual(says.cards.find(([name]) => name === "notes"), ["notes", ["4 uncommitted", `As of ${at}`], ""]);
  assert.deepEqual(says.agents, [["Claude Code", "in pallets / Fixture", "Working"]]);
});

test("Home's headline, agent cards, repository cards and sidebar all take their agents from the same reading", async () => {
  // The machine's list counts an agent; the repository's own list, read after it, has none.
  const hub = fixture({ hub: true });
  hub.data["/api/home"] = homePayload([listed(KEY, "Fixture", { owner: "pallets", host: "github.com", ...open({ agents: { active: 1, connected: 1, names: ["Claude Code"] } }) })]);
  hub.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [] });
  hub.data[under(KEY, "/api/guide")] = guide();
  await flush();
  let says = homeSays(hub);
  assert.equal(says.headline[1], "No agent is connected. One repository on this machine.");
  assert.equal(says.quiet, "No agent is connected.");
  assert.deepEqual(says.cards[0][2], "", "no card says an agent is working");
  assert.deepEqual(says.side, [["pallets / Fixture", ""]]);
  // The other way round: the repository's own list is old and failing, and the list's newer count is said everywhere.
  const counted = fixture({ hub: true });
  counted.data["/api/home"] = homePayload([listed(KEY, "Fixture", { owner: "pallets", host: "github.com", ...open({ agents: { active: 1, connected: 1, names: ["Claude Code"] } }) })]);
  counted.failures.set(under(KEY, "/api/workspace/agents"), "The agents read did not answer within 20 seconds.");
  counted.data[under(KEY, "/api/guide")] = guide();
  await flush();
  says = homeSays(counted);
  assert.equal(says.headline[1], "One agent is working. One repository on this machine.");
  assert.deepEqual(says.agents, [["Claude Code", "in pallets / Fixture", "Working"]]);
  assert.equal(agentCards(counted)[0].action, "Working through Kin now.");
  assert.equal(says.cards[0][2], "Claude Code is working");
  assert.deepEqual(says.side, [["pallets / Fixture", "live"]]);
});

test("an agent's commit waiting for review stays on Home when a later summary could not read its recent changes, and after a reload", async () => {
  const tabStorage = new Map();
  const hub = fixture({ hub: true, tabStorage });
  const rows = [listed(NOTES, "notes", open())];
  hub.data["/api/home"] = homePayload(rows);
  const summary = summaryOf({ recent: [{ id: "n-2", title: "Return two from value", author: "Claude Code", agent: { name: "Claude Code", key: "00000000000000a1" }, time: secondsAgo(120), kin_checkpoint: false }] });
  hub.data[under(NOTES, "/api/workspace/overview")] = summary;
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  hub.data[under(NOTES, "/api/counts")] = { counts: { "n-2": { entities: 1, module_containers: 0, containers_through_members: 0 } } };
  hub.data[under(NOTES, "/api/guide?change")] = recorded("n-2", { modified: 1 }, [changed(2, "value")]);
  answering(hub, rows);
  await flush();
  const before = homeSays(hub);
  assert.deepEqual(before.needs, ["notesmainAgent commit"]);
  // A new change elsewhere in it makes Home read its summary again, and that read times out on its recent changes.
  hub.data[under(NOTES, "/api/workspace/overview")] = { ...summary, read_at: secondsAgo(-1), recent_changes: { ok: false, reason: "Kin did not answer within 20 seconds." } };
  hub.data["/api/home"] = homePayload([{ ...rows[0], last_change_at: secondsAgo(1) }]);
  await hub.advance(10_000);
  assert.ok(hub.count(under(NOTES, "/api/workspace/overview")) >= 2);
  assert.deepEqual(homeSays(hub).needs, before.needs, "the card stays");
  assert.deepEqual(homeSays(hub).side, before.side);
  // A reload in this tab draws the card from what the tab last read, before the repository answers again.
  const reloaded = fixture({ hub: true, tabStorage });
  reloaded.data["/api/home"] = homePayload(rows);
  answering(reloaded, rows);
  reloaded.held.set(under(NOTES, "/api/workspace/overview"), new Promise(() => {}));
  reloaded.held.set(under(NOTES, "/api/standing"), new Promise(() => {}));
  await flush();
  assert.deepEqual(homeSays(reloaded).needs, ["notesmainAgent commit"]);
  assert.equal(needCards(reloaded)[0].picture.columns[0].boxes[0][0], "value", "drawn from the guide the tab already read");
  assert.equal(reloaded.count(under(NOTES, "/api/guide")) - reloaded.requests.filter((request) => request.url.includes("working=1")).length, 0);
});

test("an agent's commit is waiting on Home as soon as its session names it, before a busy repository's summary lists it", async () => {
  const hub = fixture({ hub: true });
  const rows = [listed(NOTES, "axum", open())];
  hub.data["/api/home"] = homePayload(rows);
  // The summary's recent changes timed out; its other parts answered.
  hub.data[under(NOTES, "/api/workspace/overview")] = { ...summaryOf(), recent_changes: { ok: false, reason: "Kin could not read recent changes. It did not answer within 10 seconds." } };
  // The session that committed it has ended, and Kin could not read the change's title yet.
  const fresh = "9".repeat(64), old = "8".repeat(64);
  hub.data[under(NOTES, "/api/workspace/agents")] = agentsPayload({ agents: [], earlier: [
    { ...agentsPayload().agents[0], key: "00000000000000b1", state: "ended", last_activity_at: secondsAgo(60), open_changes: [], changes: [{ id: fresh }], changes_total: 1 },
    { ...agentsPayload().agents[0], key: "00000000000000b2", state: "ended", last_activity_at: secondsAgo(3 * 86_400), open_changes: [], changes: [{ id: old, title: "An older change", time: secondsAgo(3 * 86_400) }], changes_total: 1 }] });
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  hub.data[under(NOTES, "/api/counts")] = { counts: { [fresh]: { entities: 1, module_containers: 0, containers_through_members: 0 } } };
  hub.data[under(NOTES, "/api/guide?change")] = recorded(fresh, { modified: 1 }, [changed(2, "Redirect::to")]);
  answering(hub, rows);
  await flush(); await flush();
  assert.equal(hub.count(under(NOTES, "/api/standing")), 1, "how its review stands is read because a session names a change");
  const says = homeSays(hub);
  assert.deepEqual(says.needs, ["axummainAgent commit"]);
  assert.equal(needCards(hub)[0].said[0], "Claude Code committed a change 1 minute ago, and nobody has reviewed it yet.");
  assert.deepEqual(needCards(hub)[0].actions, [["Review", `${URL_OF(NOTES)}${guided(fresh)}`]]);
  assert.deepEqual(says.side, [["axum", "review"]]);
  assert.equal(says.cards[0][1][0], "1 to review");
  assert.match(hub.node("home-agents").textContent, /Claude Code last worked in axum 1 minute ago\. Committed a change, waiting for your review\.$/);
});

test("a machine with no repository in Kin yet offers, where it says so, the way to bring one in", async () => {
  const hub = fixture({ hub: true }); hub.data["/api/home"] = homePayload([]); await flush();
  assert.deepEqual(headline(hub), ["Bring your first project into Kin.", "Kin converts a project you already have, keeps its history, and maps how its code connects. Your project stays where it is."]);
  assert.equal(hub.node("home-ask-input"), null, "there is nothing to ask");
  const add = hub.main().querySelectorAll("a, button").find((node) => /^Add/.test(node.textContent));
  add.click(); await flush();
  assert.ok(hub.node("add-path"));
});

test("adding a project asks Kin what is there, then follows the conversion until the repository is ready", async () => {
  const hub = fixture({ hub: true }); await flush();
  assert.equal(hub.node("add-path"), null, "the panel is closed until asked for");
  await openAdding(hub);
  assert.equal(hub.document.activeElement.id, "add-path");
  assert.equal(hub.node("add-submit").disabled, true, "there is nothing to look at yet");
  hub.data["POST /api/home/inspect"] = inspected({ note: "Kin keeps its history." });
  hub.type("add-path", "  ~/code/itsdangerous ");
  assert.equal(hub.node("add-submit").disabled, false);
  addPanel(hub).querySelector("form").emit("submit");
  assert.equal(hub.node("add-path").disabled, true, "the place cannot change while Kin looks at it");
  await flush();
  const asked = hub.requests.find((request) => request.name === "/api/home/inspect");
  assert.equal(asked.method, "POST");
  assert.equal(asked.options.headers["x-kin-review-token"], "synthetic-token");
  assert.deepEqual(hub.sent("/api/home/inspect"), [{ path: "~/code/itsdangerous" }]);
  assert.match(addPanel(hub).textContent, /itsdangerous.*~\/code\/itsdangerous · 1,204 commits of history · on main.*Kin keeps its history\./s);
  assert.deepEqual(offered(hub), ["Convert to Kin"]);
  assert.equal(hub.count("/api/home/repositories"), 0, "looking changes nothing");

  hub.data["POST /api/home/repositories"] = operation("running", { stage: "starting", phase: "Starting" });
  hub.data[`/api/home/operations/${OPERATION}`] = [
    operation("running", { stage: "graph", progress: { done: 120, total: 800 } }),
    operation("running", { stage: "references", progress: { done: 1200, total: 4000 } }),
    operation("done", { stage: "finishing", finished_at: 1_770_000_060, repository: { key: repositoryKey("d"), name: "itsdangerous", url: `/repositories/${repositoryKey("d")}/workspace/` }, note: "Its history is still being read." })];
  const listReads = hub.count("/api/home");
  hub.button("Convert to Kin").click(); await flush();
  assert.deepEqual(hub.sent("/api/home/repositories"), [{ action: "convert", path: "/work/itsdangerous" }], "the place as Kin resolved it, and nothing else");
  assert.equal(hub.requests.find((request) => request.name === "/api/home/repositories").options.headers["x-kin-review-token"], "synthetic-token");
  assert.equal(operationBox(hub).dataset.state, "running");
  assert.match(operationBox(hub).textContent, /^Converting itsdangerous/);
  assert.equal(hub.node("add-path").disabled, true);
  // The panel cannot be closed, nor another started, while this one runs.
  hub.button("Close").click();
  assert.ok(operationBox(hub), "closing waits for the operation");
  assert.equal(hub.count(`/api/home/operations/${OPERATION}`), 0, "the first reading waits its turn");
  await hub.advance(900);
  assert.match(operationBox(hub).textContent, /Building the graph, 120 of 800/);
  await hub.advance(900);
  assert.match(operationBox(hub).textContent, /Linking references, 1,200 of 4,000/);
  assert.equal(hub.count("/api/home"), listReads, "the list is not read again until the operation ends");
  await hub.advance(900);
  assert.equal(operationBox(hub).dataset.state, "done");
  assert.match(operationBox(hub).textContent, /itsdangerous is in Kin\. Its history is still being read\./);
  assert.deepEqual(operationBox(hub).querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Open repository", `/repositories/${repositoryKey("d")}/workspace/`]]);
  assert.equal(hub.count("/api/home"), listReads + 1, "the list is read again once it is done");
  assert.equal(hub.count(`/api/home/operations/${OPERATION}`), 3);
  assert.equal(hub.count("/api/home/repositories"), 1);
  hub.button("Add another").click();
  assert.equal(operationBox(hub), null);
  assert.equal(hub.node("add-path").value, "");
  assert.equal(hub.node("add-path").disabled, false);
  hub.button("Close").click();
  assert.equal(hub.node("add-path"), null, "with nothing running the panel closes");
});

test("a conversion that fails says so, keeps Kin's own words behind a disclosure and can be started over", async () => {
  const hub = fixture({ hub: true }); await flush();
  await look(hub, inspected({ kind: "plain", git: undefined }));
  // The server may answer with the operation's id alone.
  hub.data["POST /api/home/repositories"] = { operation_id: OPERATION };
  hub.data[`/api/home/operations/${OPERATION}`] = [operation("running"), operation("failed", { error: "Kin could not convert this project.", detail: "The conversion stopped with status 1.", phase: "Kin could not convert this project.", lines: ["Parsing 12 of 40", "One entity could not be parsed."] })];
  hub.button("Convert to Kin").click(); await flush();
  assert.equal(operationBox(hub).dataset.state, "running");
  await hub.advance(900); await hub.advance(900);
  const box = operationBox(hub);
  assert.equal(box.dataset.state, "failed");
  assert.match(box.textContent, /This did not finish.*Kin could not convert this project\./s);
  assert.equal(box.querySelector("details pre").textContent, "The conversion stopped with status 1.\nParsing 12 of 40\nOne entity could not be parsed.");
  assert.equal(box.querySelectorAll("a").length, 0, "there is no repository to open");
  await hub.advance(60_000);
  assert.equal(hub.count(`/api/home/operations/${OPERATION}`), 2);
  hub.button("Start over").click();
  assert.equal(operationBox(hub), null);
  assert.equal(hub.node("add-path").value, "");
});

test("an operation Kin misses a reading of is read again, and one it stops answering about is left for the reader to check", async () => {
  const hub = fixture({ hub: true }); await flush();
  await look(hub, inspected());
  hub.data["POST /api/home/repositories"] = operation("running");
  let readings = 0;
  hub.data[`/api/home/operations/${OPERATION}`] = () => (++readings === 1 ? { __status: 503, error: "Busy" } : operation("running", { stage: "graph" }));
  hub.button("Convert to Kin").click(); await flush();
  await hub.advance(900);
  assert.equal(operationBox(hub).dataset.state, "running", "one missed reading is not a failure");
  await hub.advance(900);
  assert.match(operationBox(hub).textContent, /Building the graph/);
  hub.failures.set(`/api/home/operations/${OPERATION}`, "Daemon unavailable");
  const before = hub.count(`/api/home/operations/${OPERATION}`);
  await hub.advance(900 * 5);
  assert.equal(operationBox(hub).dataset.state, "running", "five misses in a row are still tried again");
  await hub.advance(900);
  assert.equal(operationBox(hub).dataset.state, "failed");
  assert.match(operationBox(hub).textContent, /Kin stopped answering about this\./);
  assert.equal(hub.count(`/api/home/operations/${OPERATION}`), before + 6);
  await hub.advance(60_000);
  assert.equal(hub.count(`/api/home/operations/${OPERATION}`), before + 6, "it is not read for ever");
  assert.equal(hub.count("/api/home/repositories"), 1, "and never started again");
});

test("the add panel offers only what Kin says can be done at that place", async () => {
  const hub = fixture({ hub: true }); await flush();
  await look(hub, inspected({ kind: "not_a_directory", actions: [], git: undefined }));
  assert.deepEqual(offered(hub), [], "nothing can be done there, so nothing is offered");
  // A place already in Kin opens; it is not converted again whatever else the answer lists.
  await look(hub, inspected({ kind: "kin", registered: true, actions: ["open", "convert"], repository: { key: KEY, url: `${BASE}/workspace/` } }));
  assert.deepEqual(offered(hub), ["Open repository"]);
  assert.equal(addPanel(hub).querySelectorAll("a").find((node) => node.textContent === "Open repository").getAttribute("href"), `${BASE}/workspace/`);
  await look(hub, inspected({ kind: "kin", actions: ["open"] }));
  assert.deepEqual(offered(hub), ["Open in Kin"]);
  // A new repository is offered only when that is what was asked for.
  await look(hub, inspected({ kind: "missing", exists: false, actions: ["create"], git: undefined }));
  assert.deepEqual(offered(hub), [], "a place with nothing in it cannot be converted");
  const modes = addPanel(hub).querySelectorAll("button[aria-pressed]");
  assert.deepEqual(modes.map((node) => node.getAttribute("aria-pressed")), ["true", "false", "false"]);
  modes[1].click();
  assert.deepEqual(offered(hub), [], "changing what to add forgets what was looked at");
  await look(hub, inspected({ kind: "missing", exists: false, actions: ["create"], git: undefined }), "~/code/new-project");
  assert.deepEqual(offered(hub), ["Create repository"]);
  await look(hub, inspected({ kind: "git", actions: ["convert"] }));
  assert.deepEqual(offered(hub), [], "a project that is already there is not created over");
  // A place inside a repository Kin already holds says so and leads there.
  await look(hub, inspected({ kind: "plain", actions: [], git: undefined, inside: { key: KEY, name: "Fixture", url: `${BASE}/workspace/`, location: "~/code/fixture" } }));
  assert.match(addPanel(hub).textContent, /It sits inside Fixture, which is already in Kin\./);
  assert.equal(addPanel(hub).querySelectorAll("a").find((node) => node.textContent === "Fixture").getAttribute("href"), `${BASE}/workspace/`);
  // Typing another place forgets the answer about the last one.
  hub.type("add-path", "~/code/other");
  assert.doesNotMatch(addPanel(hub).textContent, /It sits inside/);
  assert.equal(hub.count("/api/home/repositories"), 0);

  hub.data["POST /api/home/repositories"] = operation("running", { action: "create" });
  await look(hub, inspected({ kind: "missing", exists: false, path: "/work/new-project", name: "new-project", actions: ["create"], git: undefined }), "~/code/new-project");
  hub.button("Create repository").click(); await flush();
  assert.deepEqual(hub.sent("/api/home/repositories"), [{ action: "create", path: "/work/new-project" }]);
  assert.match(operationBox(hub).textContent, /^Creating new-project/);
});

test("cloning sends where to clone from and where to put it, without looking first", async () => {
  const hub = fixture({ hub: true }); await flush();
  await openAdding(hub);
  addPanel(hub).querySelectorAll("button[aria-pressed]")[2].click();
  assert.equal(hub.document.activeElement.id, "add-source");
  hub.type("add-path", "~/code/itsdangerous");
  assert.equal(hub.node("add-submit").disabled, true, "a clone needs both");
  hub.type("add-source", "https://example.invalid/pallets/itsdangerous.git");
  assert.equal(hub.node("add-submit").disabled, false);
  hub.data["POST /api/home/repositories"] = operation("running", { action: "clone" });
  addPanel(hub).querySelector("form").emit("submit"); await flush();
  assert.equal(hub.count("/api/home/inspect"), 0);
  assert.deepEqual(hub.sent("/api/home/repositories"), [{ action: "clone", source: "https://example.invalid/pallets/itsdangerous.git", path: "~/code/itsdangerous" }]);
  assert.match(operationBox(hub).textContent, /^Cloning itsdangerous/);
});

test("a place Kin cannot look at, or an addition it refuses, says why and starts nothing", async () => {
  const hub = fixture({ hub: true }); await flush();
  await look(hub, { __status: 400, error: "Kin adds a project from this machine by where it is." });
  assert.match(addPanel(hub).querySelector("[role=alert]").textContent, /Kin adds a project from this machine by where it is\./);
  assert.deepEqual(offered(hub), []);
  hub.type("add-path", "~/code/other");
  assert.equal(addPanel(hub).querySelector("[role=alert]"), null, "the message goes once the place changes");
  await look(hub, inspected());
  hub.data["POST /api/home/repositories"] = { __status: 409, error: "Another conversion of this project is running." };
  hub.button("Convert to Kin").click(); await flush();
  assert.match(addPanel(hub).querySelector("[role=alert]").textContent, /Another conversion of this project is running\./);
  assert.equal(operationBox(hub), null);
  await hub.advance(60_000);
  assert.equal(hub.requests.filter((request) => request.name.startsWith("/api/home/operations/")).length, 0);
});

test("the hub's addresses open the add panel and the coding agents", async () => {
  const offer = (page) => page.node("home-repos").querySelectorAll("a").find((node) => node.textContent === "Add repository");
  const adding = fixture({ hub: true, hash: "#/add" }); await flush();
  assert.ok(adding.node("add-path"), "the address opens the panel");
  assert.equal(adding.document.title, "Home · Kin");
  assert.equal(offer(adding), undefined, "a panel that is open is not offered again");
  // Closing it leaves the address that opened it, and offers it again.
  adding.button("Close").click(); await flush();
  assert.equal(adding.node("add-path"), null);
  assert.equal(adding.location.hash, "#/");
  assert.equal(offer(adding).getAttribute("href"), "#/add");
  const hub = fixture({ hub: true }); await flush();
  hub.navigate("add");
  assert.ok(hub.node("add-path"));
  assert.equal(hub.document.activeElement.id, "add-path");
  assert.equal(hub.nav().querySelectorAll("a").find((node) => node.getAttribute("aria-label") === "Add a repository").getAttribute("href"), "#/add", "the sidebar's own way to add one stays on this page");
  hub.navigate("agents");
  assert.equal(hub.main().querySelector("h1").textContent, "Coding agents");
  assert.equal(hub.document.title, "Coding agents · Kin");
  assert.ok(hub.node("hub-clients"));
  assert.equal(hub.node("home-repos"), null, "one job to a page");
  hub.navigate("");
  assert.equal(hub.main().querySelector("h1").textContent, "pallets / Fixture needs you.");
  assert.equal(hub.document.title, "Home · Kin");
  const landing = fixture({ hub: true, hash: "#/agents" }); await flush();
  assert.equal(landing.main().querySelector("h1").textContent, "Coding agents");
  assert.match(landing.node("hub-clients").textContent, /Harbor Editor/);
});

test("coding agents are listed by whether they are here and connected, and Connect asks Kin once", async () => {
  const hub = fixture({ hub: true, hash: "#/agents" }); await flush();
  const rows = () => hub.node("hub-clients").querySelectorAll("li").map((row) => ({ text: row.textContent, button: row.querySelector("button") }));
  assert.deepEqual(rows().map((row) => row.text.replace(/Connect$/, "")), ["Claude CodeConfigured", "Harbor EditorInstalled, not configured", "LanternNot installed on this machine"], "connected first, then what is waiting, then what is not here");
  assert.deepEqual(rows().map((row) => Boolean(row.button)), [false, true, false], "only an agent that is here and not connected can be connected");
  const done = { operation_id: OPERATION, action: "connect", state: "done", stage: "finishing", phase: "Connected", lines: [] };
  hub.data["POST /api/home/clients"] = { operation_id: OPERATION, action: "connect", state: "running", stage: "starting", phase: "Starting", lines: [] };
  hub.data[`/api/home/operations/${OPERATION}`] = [{ ...done, state: "running" }, done];
  const listReads = hub.requests.filter((request) => request.name === "/api/home/clients" && request.method === "GET").length;
  rows()[1].button.click();
  assert.equal(rows()[1].button.disabled, true);
  rows()[1].button.click();
  await flush();
  const sent = hub.requests.filter((request) => request.name === "/api/home/clients" && request.method === "POST");
  assert.equal(sent.length, 1);
  assert.equal(sent[0].options.headers["x-kin-review-token"], "synthetic-token");
  assert.deepEqual(JSON.parse(sent[0].options.body), { connect: ["harbor-editor"] });
  hub.data["/api/home/clients"].clients.find((row) => row.id === "harbor-editor").connected = true;
  await hub.advance(900);
  assert.equal(rows()[1].button.disabled, true, "still connecting");
  await hub.advance(900); await flush();
  assert.equal(hub.requests.filter((request) => request.name === "/api/home/clients" && request.method === "GET").length, listReads + 1, "the list is read again when Kin says it is done");
  assert.deepEqual(rows().map((row) => row.text).sort(), ["Claude CodeConfigured", "Harbor EditorConfigured", "LanternNot installed on this machine"]);
  assert.equal(hub.node("hub-clients").querySelector("[role=alert]"), null);
});

test("an agent Kin could not connect says why, and a session that cannot write is offered nothing to press", async () => {
  const hub = fixture({ hub: true, hash: "#/agents" }); await flush();
  const connect = () => hub.node("hub-clients").querySelector("button");
  hub.data["POST /api/home/clients"] = { operation_id: OPERATION, state: "running" };
  hub.data[`/api/home/operations/${OPERATION}`] = { operation_id: OPERATION, action: "connect", state: "failed", error: "Harbor Editor's settings could not be written." };
  connect().click(); await flush(); await hub.advance(900); await flush();
  assert.match(hub.node("hub-clients").querySelector("[role=alert]").textContent, /That agent was not connected.*Harbor Editor's settings could not be written\./s);
  assert.equal(connect().disabled, false, "it can be tried again");
  hub.data["POST /api/home/clients"] = { __status: 403, error: "This session may not write." };
  connect().click(); await flush();
  assert.match(hub.node("hub-clients").querySelector("[role=alert]").textContent, /This session may not write\./);

  const reader = fixture({ hub: true, hash: "#/agents", session: { token: "" } }); await flush();
  assert.equal(reader.node("hub-clients").querySelector("button").disabled, true);
  const failing = fixture({ hub: true, hash: "#/agents" }); failing.failures.set("/api/home/clients", "Settings unreadable"); await flush();
  assert.match(failing.node("hub-clients").textContent, /Kin couldn't load which coding agents are connected\..*Settings unreadable/s);
  failing.failures.delete("/api/home/clients");
  failing.button("Try again").click(); await flush();
  assert.match(failing.node("hub-clients").textContent, /Harbor Editor/);
});

test("a server that lists no coding agents leaves their page out of the sidebar and says so at its address", async () => {
  const hub = fixture({ hub: true }); await flush();
  const machine = (page) => page.nav().querySelectorAll("a").filter((node) => !node.getAttribute("title")).map((node) => [node.textContent, node.getAttribute("href")]);
  assert.deepEqual(machine(hub), [["Home", "#/"], ["Coding agents", "#/agents"]]);
  assert.deepEqual(sideCurrent(hub), ["Home"]);
  hub.navigate("agents");
  assert.deepEqual(sideCurrent(hub), ["Coding agents"]);

  const without = fixture({ hub: true }); delete without.data["/api/home/clients"]; await flush();
  assert.deepEqual(machine(without), [["Home", "#/"]]);
  assert.doesNotMatch(without.nav().textContent, /null|undefined/, "the page that is left out leaves nothing behind");
  assert.equal(without.node("workspace-status").dataset.state, "ready", "a read the server does not have is not a failed read");
  without.navigate("agents");
  assert.equal(without.node("hub-clients").textContent, "This version of Kin can't list coding agents here.");
  without.refresh(); await flush();
  assert.equal(without.count("/api/home/clients"), 2, "an explicit refresh retries the unavailable capability");
});

test("the hub reads its list again while it is being looked at, and not while it is hidden", async () => {
  const hub = fixture({ hub: true });
  hub.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [] });
  await flush();
  const reads = () => hub.count("/api/home");
  const first = reads();
  await hub.advance(10_000);
  assert.equal(reads(), first + 1);
  hub.data["/api/home"] = homePayload([listed(KEY, "Fixture", open({ merge_in_progress: true }))]);
  await hub.advance(10_000);
  assert.equal(reads(), first + 2);
  assert.deepEqual(headline(hub), ["Fixture needs you.", "No agent is connected. One repository on this machine."], "what changed is drawn");
  hub.document.hidden = true;
  await hub.advance(60_000);
  assert.equal(reads(), first + 2, "a hidden page reads nothing");
  hub.document.hidden = false;
  await hub.advance(10_000);
  assert.equal(reads(), first + 3);
});

// ── Sidebar ────────────────────────────────────────────────────────────────

test("the sidebar is the map of the machine: every repository, and the open one unfolded into its pages", async () => {
  const browser = fixture();
  browser.data["/api/home"] = homePayload([
    listed(ARCHIVE, "archive"),
    listed(repositoryKey("d"), "elsewhere", { url: "https://elsewhere.example/workspace/" }),
    listed(repositoryKey("1"), "busy", open({ agents: { active: 1, connected: 1, names: ["Lantern"] } })),
    listed(NOTES, "notes", open({ working: { changed: 3, differs: true } })),
    listed(KEY, "Fixture", { owner: "pallets", host: "github.com", ...open() })]);
  await flush();
  const head = browser.nav().querySelectorAll("a").slice(0, 2);
  assert.deepEqual(head.map((node) => [node.textContent, node.getAttribute("href")]), [["Home", "/"], ["", "/#/add"]], "the way back to the machine's Home, and the way to add a repository");
  assert.ok(browser.nav().querySelectorAll("span").some((node) => node.textContent === "Repositories"), "the list is named");
  assert.equal(head[0].getAttribute("aria-current"), null, "Home is the machine's page, not this one");
  // The ones with something in motion first, then the rest, each by name. Opening a repository does not move it,
  // so the list holds still under the pointer. Each dot says what is in motion there.
  assert.deepEqual(sideRepositories(browser), [
    { name: "notes", href: `/repositories/${NOTES}/workspace/`, mark: "work", branch: "main" },
    { name: "pallets / Fixture", href: "#/uncommitted", mark: "work", branch: "main" },
    { name: "busy", href: `/repositories/${repositoryKey("1")}/workspace/`, mark: "live", branch: "main" },
    { name: "archive", href: `/repositories/${ARCHIVE}/workspace/`, mark: "", branch: "" },
    { name: "elsewhere", href: "/", mark: "", branch: "" }], "the open repository leads into itself, the others to their own addresses and never off this server");
  // Only the open repository is unfolded, into pages of this address.
  assert.deepEqual(browser.nav().querySelectorAll("a[data-route]").map((node) => [node.textContent.replace(/\d+$/, ""), node.getAttribute("href")]), [["Work", "#/uncommitted"], ["Agents", "#/agents"], ["Code", "#/explore"]]);
  assert.deepEqual(sideCurrent(browser), ["uncommitted"]);
  assert.deepEqual(sideCount(browser, "uncommitted"), ["1", "1 uncommitted"], "the entities this repository's uncommitted work changed");
  assert.deepEqual(sideCount(browser, "agents"), ["1", "1 working now"]);
  assert.equal(sideCount(browser, "explore"), null);
  // The page that is open is marked. Work holds what is uncommitted, the branches, the history and every review; an entity belongs to the code.
  for (const [address, page] of [["history", "uncommitted"], ["branches", "uncommitted"], ["agents", "agents"], [`agents/${AGENT_KEY}`, "agents"], ["explore", "explore"], [`entity/${uuid(2)}`, "explore"],
    ["review/w/1", "uncommitted"], ["review/w/done", "uncommitted"], ["review/c/change-1", "uncommitted"], ["review/c/change-1/1", "uncommitted"], ["review/c/change-1/done", "uncommitted"],
    ["review/r/main..feature", "uncommitted"], ["review/r/main..feature/1", "uncommitted"], ["uncommitted", "uncommitted"]]) {
    browser.navigate(address);
    assert.deepEqual(sideCurrent(browser), [page], address);
  }
  // The open repository's dot and counts follow its own reads: work first, then an agent working, then nothing.
  const fixtureRow = () => sideRepositories(browser).find((row) => row.name === "pallets / Fixture");
  browser.data["/api/guide"] = guide();
  browser.refresh(); await flush();
  assert.equal(fixtureRow().mark, "live");
  assert.equal(sideCount(browser, "uncommitted"), null);
  browser.data["/api/workspace/agents"] = agentsPayload({ agents: [{ ...agentsPayload().agents[0], state: "idle" }] });
  browser.refresh(); await flush();
  assert.equal(fixtureRow().mark, "");
  assert.equal(sideCount(browser, "agents"), null, "an idle agent is not working now");
  browser.data["/api/branches"] = { branches: [{ name: "main", active: false, default: true }, { name: "feature", active: true }] };
  browser.refresh(); await flush();
  assert.equal(fixtureRow().branch, "feature", "the branch it names is the one that is checked out");
  assert.doesNotMatch(browser.nav().textContent, /null|undefined/, "a part that is left out is left out, not written as a word");
});

test("an agent's change nobody has reviewed marks its repository in every page's sidebar, after uncommitted work and before an agent at work", async () => {
  // Home read it in this tab: notes waits for a review.
  const tabStorage = new Map([[`kin.workspace.awaiting-review:${NOTES}`, JSON.stringify(["n-2"])]]);
  const browser = fixture({ tabStorage });
  browser.data["/api/home"] = homePayload([
    listed(repositoryKey("1"), "busy", open({ agents: { active: 1, connected: 1, names: ["Lantern"] } })),
    listed(NOTES, "notes", open()),
    listed(KEY, "Fixture", { owner: "pallets", host: "github.com", ...open() })]);
  browser.data["/api/workspace/agents"] = agentsPayload({ agents: [] });
  browser.data["/api/guide"] = guide();
  // This repository's own newest change is an agent's, and nobody has reviewed it.
  browser.data["/api/changes"] = { changes: [{ id: "c-9", title: "Return two", author: "Claude Code", agent: { name: "Claude Code", key: AGENT_KEY }, timestamp: secondsAgo(60), entity_deltas: 1 }, ...browser.data["/api/changes"].changes], truncated: false };
  browser.data["/api/standing"] = { standing: {} };
  await flush();
  assert.deepEqual(sideRepositories(browser).map((row) => [row.name, row.mark]), [["notes", "review"], ["pallets / Fixture", "review"], ["busy", "live"]]);
  assert.deepEqual(JSON.parse(tabStorage.get(`kin.workspace.awaiting-review:${KEY}`)), ["c-9"], "what this page read of itself is kept for the next page");
  // Reviewed here, it no longer waits.
  browser.data["/api/standing"] = { standing: { "c-9": { state: "approved" } } };
  browser.refresh(); await flush();
  assert.deepEqual(sideRepositories(browser).map((row) => [row.name, row.mark]), [["notes", "review"], ["busy", "live"], ["pallets / Fixture", ""]]);
});

test("opening a repository from the sidebar keeps the list where it was while the new page reads it", async () => {
  // Home drew this tab's list: notes has work, Fixture waits for a review.
  const hub = fixture({ hub: true });
  hub.data["/api/home"] = homePayload([listed(KEY, "Fixture", { ...open(), owner: "pallets" }), listed(NOTES, "notes", open({ working: { changed: 2, differs: true } })), listed(ARCHIVE, "archive")]);
  hub.data[under(KEY, "/api/workspace/overview")] = summaryOf({ recent: [{ id: "c-9", title: "Return two", author: "Claude Code", agent: { name: "Claude Code", key: AGENT_KEY }, time: secondsAgo(60), kin_checkpoint: false }] });
  hub.data[under(KEY, "/api/standing")] = { standing: {} };
  answering(hub, hub.data["/api/home"].repositories);
  await flush();
  const before = sideRepositories(hub).map((row) => [row.name, row.mark]);
  assert.deepEqual(before, [["notes", "work"], ["pallets / Fixture", "review"], ["archive", ""]]);
  // The repository's own page opens in the same tab, before it has read the machine's list.
  const browser = fixture({ tabStorage: hub.tabStorage });
  let release; browser.held.set("/api/home", new Promise((done) => { release = done; }));
  browser.data["/api/home"] = homePayload([listed(KEY, "Fixture", { ...open(), owner: "pallets" }), listed(NOTES, "notes", open({ working: { changed: 2, differs: true } })), listed(ARCHIVE, "archive")]);
  browser.data["/api/session"] = undefined;
  await flush();
  assert.deepEqual(sideRepositories(browser).map((row) => row.name), ["notes", "pallets / Fixture"], "the repositories the session names, in the order the reader last saw");
  release(); await flush();
  assert.deepEqual(sideRepositories(browser).map((row) => row.name), ["notes", "pallets / Fixture", "archive"]);
});

test("a long list of repositories keeps the open one in the sidebar and leads to the rest", async () => {
  const browser = fixture();
  browser.data["/api/home"] = homePayload([...Array.from({ length: 9 }, (_, i) => listed(repositoryKey(String(i + 1)), `service-${i + 1}`)), listed(KEY, "Fixture", open())]);
  await flush();
  const shown = sideRepositories(browser);
  assert.equal(shown.length, 8);
  assert.deepEqual([shown[0].name, shown[0].href], ["Fixture", "#/uncommitted"], "the open repository is listed first though Kin listed it tenth");
  const rest = browser.nav().querySelectorAll("a").find((node) => node.textContent === "All 10 repositories");
  assert.equal(rest.getAttribute("href"), "/");

  const hub = fixture({ hub: true }); await flush();
  assert.deepEqual(sideRepositories(hub), [
    { name: "pallets / Fixture", href: `${BASE}/workspace/`, mark: "work", branch: "main" },
    { name: "archive", href: `/repositories/${ARCHIVE}/workspace/`, mark: "", branch: "" },
    { name: "notes", href: `/repositories/${NOTES}/workspace/`, mark: "", branch: "main" }], "what is in motion first, then the rest by name");
  assert.equal(hub.nav().querySelectorAll("a[data-route]").length, 0, "no repository is open at the hub");
  assert.doesNotMatch(hub.nav().textContent, /null|undefined/);
});

test("a server with no list of repositories to read shows the ones its session names and retries on refresh", async () => {
  const browser = fixture();
  delete browser.data["/api/home"];
  await flush();
  assert.deepEqual(sideRepositories(browser).map((row) => [row.name, row.href]), [["pallets / Fixture", "#/uncommitted"], ["notes", `/repositories/${NOTES}/workspace/`]]);
  assert.equal(browser.node("workspace-status").dataset.state, "ready", "a list that is not there is not a failed read");
  browser.refresh(); await flush();
  assert.equal(browser.count("/api/home"), 2);

  const alone = fixture({ session: { repositories: [] } });
  delete alone.data["/api/home"];
  await flush();
  assert.deepEqual(sideRepositories(alone).map((row) => [row.name, row.href]), [["pallets / Fixture", "#/uncommitted"]]);
  for (const route of ["uncommitted", "agents", "explore"]) assert.ok(sideRoute(alone, route), route);
  assert.doesNotMatch(alone.nav().textContent, /null|undefined/);
});

test("the menu folds the sidebar, and Escape or a change of page closes it", async () => {
  const browser = fixture(); await flush();
  const toggle = browser.node("workspace-menu-toggle");
  toggle.click();
  assert.equal(toggle.getAttribute("aria-expanded"), "true");
  assert.equal(browser.node("workspace-side").dataset.menu, "open");
  browser.document.emit("keydown", { key: "Escape" });
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
  assert.equal(browser.node("workspace-side").dataset.menu, "closed");
  assert.equal(browser.document.activeElement, toggle);
  toggle.click();
  browser.navigate("history");
  assert.equal(toggle.getAttribute("aria-expanded"), "false");
});

test("the theme control steps through its choices and keeps the one chosen", async () => {
  const kept = new Map();
  const browser = fixture({ kept }); await flush();
  const control = browser.node("workspace-theme");
  assert.equal(control.textContent, "Auto");
  assert.equal(browser.document.documentElement.getAttribute("data-theme"), null);
  control.click();
  assert.equal(control.textContent, "Light");
  assert.equal(browser.document.documentElement.getAttribute("data-theme"), "light");
  assert.equal(control.getAttribute("aria-label"), "Colour theme: Light. Change it.");
  control.click();
  assert.equal(browser.document.documentElement.getAttribute("data-theme"), "dark");
  const reloaded = fixture({ kept }); await flush();
  assert.equal(reloaded.node("workspace-theme").textContent, "Dark", "the choice is there on the next visit");
  reloaded.node("workspace-theme").click();
  assert.equal(reloaded.document.documentElement.getAttribute("data-theme"), null, "and comes back round to following the system");
});

test("coming back to the window reads what moves again, and not more often than a reader needs", async () => {
  const browser = fixture(); await flush();
  const before = browser.requests.length;
  browser.focus();
  assert.equal(browser.requests.length, before, "a window that was only just read is not read again");
  await browser.advance(15_000);
  browser.focus();
  assert.deepEqual(browser.requests.slice(before).map((request) => request.name).sort(), ["/api/branches", "/api/changes", "/api/guide", "/api/home", "/api/standing", "/api/workspace/session"]);
  await flush();
  browser.focus();
  assert.equal(browser.requests.length, before + 6);
  browser.document.hidden = true;
  await browser.advance(60_000);
  browser.document.emit("visibilitychange");
  assert.equal(browser.requests.length, before + 6, "a hidden page reads nothing");
  browser.document.hidden = false;
  browser.document.emit("visibilitychange");
  assert.equal(browser.requests.length, before + 12, "showing it again does");
});

// ── Explore ────────────────────────────────────────────────────────────────

const exploreRows = (browser) => browser.node("explore-results").querySelectorAll("a[data-entity]");

test("explore opens on the most relied-on entities and what the repository holds", async () => {
  const browser = fixture({ hash: "#/explore" }); await flush();
  assert.match(browser.node("explore-input").getAttribute("placeholder"), /Ask about this code, like “want_bytes”/);
  assert.match(browser.text(), /216 entities and 602 connections\./);
  assert.match(browser.text(), /Most relied on/);
  const top = exploreRows(browser);
  assert.deepEqual(top.map((row) => row.getAttribute("href")), [`#/entity/${uuid(2)}`, `#/entity/${uuid(3)}`]);
  assert.deepEqual(top.map((row) => row.textContent), ["want_bytesFunction in encodingdef want_bytes(s)23 rely on it", "SignerClass in signer22 rely on it"], "each with the one number every page says for what relies on it");
  // Beside it, what the repository holds at its top, each a way into its own page.
  const held = browser.node("explore-results").querySelectorAll("a").filter((node) => !node.getAttribute("data-entity"));
  assert.deepEqual(held.map((node) => [node.textContent, node.getAttribute("href")]), [["signer30 entities", `#/entity/${uuid(4)}`], ["encoding6 entities", `#/entity/${uuid(1)}`]]);
  assert.match(browser.text(), /Largest containerssigner30 entitiesencoding6 entities/);
  assert.match(browser.text(), /LanguagesPython210 entitiesTOML6 entities/);
  assert.equal(browser.count("/api/workspace/ask"), 0, "nothing is asked until the reader asks");
  assert.deepEqual(sideCurrent(browser), ["explore"]);
  assert.equal(browser.main().querySelector("h1").textContent, "Code");
  assert.equal(browser.document.title, "Code · pallets/Fixture");
  // Two containers of one name are told apart by what each is written in; a name held once needs no such word.
  const twins = fixture({ hash: "#/explore" });
  twins.data["/api/workspace/overview"] = { ...twins.data["/api/workspace/overview"], containers: { ok: true, shown: 3, total: 3, items: [
    { id: uuid(40), name: "utils", kind: "module", language: "python", entities: 30, container: null }, { id: uuid(41), name: "utils", kind: "module", language: "rust", entities: 12, container: null },
    { id: uuid(42), name: "signer", kind: "module", language: "python", entities: 9, container: null }] } };
  await flush();
  assert.deepEqual(twins.node("explore-results").querySelectorAll("a").filter((node) => !node.getAttribute("data-entity")).map((node) => [node.textContent, node.getAttribute("href")]),
    [["utilsPython30 entities", `#/entity/${uuid(40)}`], ["utilsRust12 entities", `#/entity/${uuid(41)}`], ["signer9 entities", `#/entity/${uuid(42)}`]]);
});

test("explore lists what is relied on and no recorded test reaches, in place of the largest containers", async () => {
  const browser = fixture({ hash: "#/explore" });
  browser.data["/api/workspace/overview"] = { ...browser.data["/api/workspace/overview"], untested: { ok: true, total: 2, scope: "recorded_graph", bound: "observed", items: [
    { id: uuid(50), name: "want_bytes", kind: "function", container: { name: "encoding" }, dependents: 23 },
    { id: uuid(51), name: "Signer", kind: "class", container: { name: "signer" }, dependents: 1 }] } };
  await flush();
  // A test that reaches an entity through what it uses counts, so the heading names no test, not no direct one.
  assert.match(browser.text(), /Relied on, with no tests found that reach them2/);
  assert.doesNotMatch(browser.text(), /no direct test/);
  assert.doesNotMatch(browser.text(), /Largest containers/);
  const rows = browser.node("explore-results").querySelectorAll("a.structure-row");
  assert.deepEqual(rows.map((node) => node.getAttribute("href")), [`#/entity/${uuid(50)}`, `#/entity/${uuid(51)}`]);
  assert.match(rows[0].textContent, /23 rely on it/);
  assert.match(rows[1].textContent, /1 relies on it/);
});

test("explore asks by meaning and by name and shows entity rows with kind, container and signature", async () => {
  const browser = fixture({ hash: "#/explore" }); await flush();
  browser.navigate("explore?q=verify%20a%20signature"); await flush();
  const ask = browser.requests.find((request) => request.name === "/api/workspace/ask");
  assert.match(ask.url, /q=verify%20a%20signature/);
  assert.match(browser.text(), /Best matches/);
  assert.match(browser.text(), /Signer\.verify_signatureMethod/);
  assert.match(browser.text(), /def verify_signature\(self, value, sig\) -> bool/);
  assert.match(browser.text(), /want_bytesFunction in encoding/);
  const rows = exploreRows(browser);
  assert.equal(rows[0].getAttribute("href"), `#/entity/${uuid(5)}`);
  const input = browser.node("explore-input");
  input.emit("keydown", { key: "ArrowDown" });
  assert.equal(rows[0].getAttribute("aria-selected"), "true");
  input.emit("keydown", { key: "ArrowDown" }); input.emit("keydown", { key: "ArrowDown" });
  assert.deepEqual(rows.map((row) => row.getAttribute("aria-selected")), ["false", "true"], "the choice stops at the last row");
  input.emit("keydown", { key: "ArrowUp" });
  input.emit("keydown", { key: "Enter" });
  assert.equal(browser.location.hash, `#/entity/${uuid(5)}`);
});

test("typing asks once the reader pauses, and Escape clears the question", async () => {
  const browser = fixture({ hash: "#/explore" }); await flush();
  for (const typed of ["s", "si", "sign"]) { browser.type("explore-input", typed); await browser.advance(100); }
  assert.equal(browser.count("/api/workspace/ask"), 0, "nothing is asked while the reader is still typing");
  await browser.advance(260);
  const asked = browser.requests.filter((request) => request.name === "/api/workspace/ask");
  assert.equal(asked.length, 1);
  assert.match(asked[0].url, /q=sign&/);
  assert.equal(exploreRows(browser).length, 2);
  assert.equal(browser.node("workspace-announcer").textContent, "2 results.");
  const event = browser.node("explore-input").emit("keydown", { key: "Escape" });
  assert.equal(event.defaultPrevented, true);
  assert.equal(browser.node("explore-input").value, "");
  assert.match(browser.text(), /Most relied on/, "the page is back where it opened");
  // A search Kin could not run says so and can be tried again.
  browser.failures.set("/api/workspace/ask", "The index is not ready.");
  browser.navigate("explore?q=sign"); await flush();
  assert.match(browser.node("explore-results").textContent, /Kin couldn't run that search\..*The index is not ready\./s);
  browser.failures.delete("/api/workspace/ask");
  browser.node("explore-results").querySelector("button").click(); await flush();
  assert.equal(exploreRows(browser).length, 2);
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  browser.navigate("explore?q=zzz"); await flush();
  assert.match(browser.node("explore-results").textContent, /Nothing matched “zzz”\./);
});

test("a question typed into Explore is kept in the address, so the page can be opened again on it", async () => {
  const browser = fixture({ hash: "#/explore" }); await flush();
  browser.type("explore-input", "sign"); await browser.advance(260);
  assert.equal(exploreRows(browser).length, 2, "the question was asked");
  assert.equal(browser.location.hash, "#/explore?q=sign");
  browser.node("explore-input").emit("keydown", { key: "Escape" });
  assert.equal(browser.location.hash, "#/explore");
});

test("a slash, or the search key with K, opens Explore with the question ready to type", async () => {
  const browser = fixture(); await flush();
  const slash = browser.document.emit("keydown", { key: "/" });
  assert.equal(slash.defaultPrevented, true);
  await flush(); await browser.advance(0);
  assert.equal(browser.location.hash, "#/explore");
  assert.equal(browser.main().querySelector("h1").textContent, "Code");
  assert.equal(browser.document.activeElement.id, "explore-input");
  browser.navigate("history");
  browser.document.emit("keydown", { key: "k", metaKey: true });
  await flush(); await browser.advance(0);
  assert.equal(browser.location.hash, "#/explore");
  // A slash typed into a field is a slash.
  browser.navigate("history");
  const typed = browser.document.emit("keydown", { key: "/", target: browser.node("workspace-review-search") });
  await flush();
  assert.equal(typed.defaultPrevented, false);
  assert.equal(browser.location.hash, "#/history");
  // At the server's root the same keys put the reader in Home's question to every repository.
  const hub = fixture({ hub: true }); await flush();
  assert.equal(hub.document.emit("keydown", { key: "/" }).defaultPrevented, true);
  await flush();
  assert.equal(hub.document.activeElement.id, "home-ask-input");
  assert.equal(hub.location.hash, "", "and stays on Home");
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

test("explore folds overloads into one row, and a row says what relies on it and not how it matched", async () => {
  const browser = fixture({ hash: "#/explore?q=init" });
  const init = (n) => ({ id: uuid(60 + n), name: "Serializer.__init__", kind: "method", signature: `def __init__(self, a${n})`, container: { id: uuid(50), name: "Serializer", kind: "class" }, matched: ["meaning"] });
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [init(1), init(2), init(3), init(4), { id: uuid(70), name: "want_bytes", kind: "function", matched: ["name"], dependents: 23 }, { id: uuid(71), name: "want_text", kind: "function", matched: ["name"] }, { id: uuid(72), name: "decode", kind: "function", matched: ["meaning"] }], total: 7 };
  await flush();
  const text = browser.text();
  assert.equal((text.match(/Serializer\.__init__/g) || []).length, 1);
  assert.match(text, /Serializer\.__init__4 overloads/);
  assert.doesNotMatch(text, /def __init__/, "a folded row shows no one signature");
  assert.doesNotMatch(text, /Close in meaning/);
  const said = Object.fromEntries(exploreRows(browser).map((row) => [row.getAttribute("data-entity"), row.textContent]));
  assert.equal(said[uuid(70)], "want_bytesFunction23 rely on it", "a row that carries the count says it and nothing else");
  assert.equal(said[uuid(71)], "want_textFunction", "the list is already in the order Kin ranked it, so a row does not say how it matched");
  assert.equal(said[uuid(72)], "decodeFunction");
  assert.doesNotMatch(text, /Name match/);
  assert.match(text, /7 entities/, "the overloads a row folds are counted as shown");
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
/** The picture's three columns: what relies on the entity, the entity, and what it relies on. */
function connections(browser) {
  const host = browser.node("entity-connections").querySelector("[role=group]");
  const [left, focus, right] = host.children.filter((node) => node.tagName === "DIV");
  // A box is its entity's name, where it leads, and the small word beside the name when it carries one.
  const boxes = (column) => column.querySelectorAll("a").map((node) => [node.getAttribute("title"), node.getAttribute("href"), ...node.children.slice(1).map((tag) => tag.textContent)]);
  return { host, left, focus, right, relying: boxes(left), reliesOn: boxes(right), more: (column) => column.querySelectorAll("button").find((node) => /^Show \d+ more$/.test(node.textContent)),
    lines: () => host.querySelector("svg")?.querySelectorAll("path").map((line) => line.getAttribute("d")) || [] };
}
/** Lay the picture's columns side by side, or one above the other, so its lines have boxes to measure. */
function layout(browser, { width = 900, stacked = false } = {}) {
  browser.document.layout = (node) => {
    const { host, left, focus, right } = connections(browser);
    if (node === host) return { left: 10, top: 20, width, height: 300 };
    if (node === focus) return stacked ? { left: 10, top: 120, width: 200, height: 40 } : { left: 10 + width / 2 - 50, top: 120, width: 100, height: 40 };
    for (const [column, x] of [[left, 10], [right, 10 + width - 200]]) {
      const at = column.querySelectorAll("a").indexOf(node);
      if (at >= 0) return { left: stacked ? 10 : x, top: 20 + at * 40 + (stacked && column === right ? 200 : 0), width: 200, height: 30 };
    }
    return null;
  };
}

test("an entity page draws each part of the sectioned read and links into the review", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id);
  await flush();
  const head = browser.node("entity-head");
  assert.equal(head.querySelector("h1").textContent, "Signer.sign");
  assert.match(head.textContent, /^Code\/SignerSigner\.signMethodSigns the value\./, "where it sits, its name, what it is, and what it is for");
  assert.equal(head.querySelector("code").textContent, "value", "a literal in its summary is set as code");
  assert.deepEqual(head.querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Code", "#/explore"], ["Signer", `#/entity/${uuid(3)}`]]);
  assert.match(head.textContent, /Kin can't open this one in an editor yet\.$/);
  assert.equal(browser.node("entity-source").textContent, "SourcePython40def sign(self, value):41    return value", "the source is numbered from where it sits and keeps its indentation");
  // The picture: what relies on it on one side, tests last and said to be tests, and what it relies on on the other.
  const picture = connections(browser);
  assert.deepEqual(picture.relying, [["Serializer.dumps", `#/entity/${uuid(10)}`], ["TestSigner.test_sign", `#/entity/${uuid(11)}`, "test"]]);
  assert.deepEqual(picture.reliesOn, [["want_bytes", `#/entity/${uuid(2)}`]]);
  assert.match(picture.left.textContent, /^What relies on it2/, "a test that relies on it is counted like any other entity");
  assert.match(picture.right.textContent, /^What it relies on1/);
  assert.equal(picture.focus.textContent, "Signer.sign");
  assert.equal(picture.host.getAttribute("aria-label"), "Signer.sign, with 2 rely on it, and it relies on 1 entity");
  // How far a change here reaches is said once, above the tabs; the lists behind it sit with the picture.
  assert.equal(browser.node("entity-reach").textContent, "A change here might affect at least 5 entities within 3 steps. At least two tests reach it.Some call sites could not be proven on this machine, so the counts are a lower bound.", "a count Kin calls a lower bound is said as one");
  assert.equal(browser.node("entity-impact").parentNode.getAttribute("aria-label"), "What a change might affect");
  assert.equal(browser.node("entity-impact").textContent, "Further out1Serializer.dump2 steps awayTests that reach it≥2TestSigner.test_signTestTestSerializer.test_dumps2 steps away");
  assert.doesNotMatch(browser.text(), /directly/, "no second number for what relies on it");
  const history = browser.links().find((node) => node.textContent.includes("Bound input"));
  assert.equal(history.getAttribute("href"), guided("abc123"));
  assert.deepEqual(sideCurrent(browser), ["explore"]);
  assert.equal(browser.document.title, "Signer.sign · pallets/Fixture");
  // A count Kin gives as exact is said without the hedge.
  const exact = fixture({ hash: `#/entity/${id}` });
  exact.data["/api/workspace/entity"] = () => entityV2(id, { impact: { ok: true, direct: 2, transitive: 5, tests: 0, depth: 3, top: [], bound: "exact" }, tests: { ok: true, items: [], total: 0 } });
  await flush();
  assert.equal(exact.node("entity-reach").textContent, "A change here might affect 5 entities within 3 steps. Kin found no tests that reach it.");
});

/** An entity's tabs: each one's words, whether it is the one shown, and whether the keyboard stops on it. */
const entityTabs = (browser) => browser.node("entity-tabs").querySelectorAll("[role=tab]").map((tab) => [tab.textContent, tab.getAttribute("aria-selected"), tab.getAttribute("tabindex")]);
/** Which of an entity's panels are shown. */
const shownPanels = (browser) => ["connections", "source", "members", "history"].filter((key) => browser.node(`entity-panel-${key}`) && !browser.node(`entity-panel-${key}`).hidden);

test("an entity shows one part at a time under its tabs, and the arrow keys move between them", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id);
  await flush();
  assert.equal(browser.node("entity-tabs").querySelector("[role=tablist]").getAttribute("aria-label"), "About this entity");
  assert.deepEqual(entityTabs(browser), [["Connections", "true", "0"], ["Source", "false", "-1"], ["History1", "false", "-1"]], "an entity with no members has no tab for them, and its history says how many changes");
  assert.deepEqual(browser.node("entity-tabs").querySelectorAll("[role=tab]").map((tab) => [tab.id, tab.getAttribute("aria-controls")]), [["entity-tab-connections", "entity-panel-connections"], ["entity-tab-source", "entity-panel-source"], ["entity-tab-history", "entity-panel-history"]]);
  assert.deepEqual(shownPanels(browser), ["connections"]);
  assert.equal(browser.node("entity-panel-source").getAttribute("role"), "tabpanel");
  assert.equal(browser.node("entity-panel-source").getAttribute("aria-labelledby"), "entity-tab-source");
  browser.node("entity-tab-source").click();
  assert.deepEqual(shownPanels(browser), ["source"]);
  assert.deepEqual(entityTabs(browser).map(([, selected]) => selected), ["false", "true", "false"]);
  assert.equal(browser.document.activeElement.id, "entity-tab-source");
  // Right and left move to the next tab and the one before, round from the ends.
  const press = (key) => { const event = browser.document.activeElement.emit("keydown", { key }); return event; };
  const moved = press("ArrowRight");
  assert.equal(moved.defaultPrevented, true);
  assert.equal(moved.stopped, true, "the key is the tabs' and goes no further");
  assert.deepEqual(shownPanels(browser), ["history"]);
  press("ArrowRight");
  assert.deepEqual(shownPanels(browser), ["connections"]);
  press("ArrowLeft");
  assert.deepEqual(shownPanels(browser), ["history"]);
  assert.equal(browser.document.activeElement.id, "entity-tab-history");
  assert.equal(press("Enter").defaultPrevented, false, "another key is left alone");
  assert.equal(browser.links().find((node) => node.textContent.includes("Bound input")).getAttribute("href"), guided("abc123"));
  // An entity that holds members has a tab for them, with how many.
  const held = fixture({ hash: `#/entity/${id}` });
  held.data["/api/workspace/entity"] = () => entityV2(id, { members: { ok: true, items: [{ id: uuid(32), name: "Signer.sign.inner", kind: "function" }], total: 1, shown: 1 } });
  await flush();
  assert.deepEqual(entityTabs(held).map(([words]) => words), ["Connections", "Source", "Members1", "History1"]);
  held.node("entity-tab-members").click();
  assert.deepEqual(shownPanels(held), ["members"]);
  assert.equal(held.node("entity-members").querySelector("a").getAttribute("href"), `#/entity/${uuid(32)}`);
  // Opening an entity with no members while that tab was chosen shows its connections.
  held.data["/api/workspace/entity"] = () => entityV2(uuid(10));
  held.navigate(`entity/${uuid(10)}`); await flush();
  assert.deepEqual(shownPanels(held), ["connections"]);
  assert.deepEqual(entityTabs(held).map(([words]) => words), ["Connections", "Source", "History1"]);
});

test("the picture joins the entity to each box with one line, measures again when it changes size, and stops when the page moves on", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id);
  layout(browser);
  await flush(); await browser.advance(40);
  const picture = connections(browser);
  assert.equal(picture.lines().length, 3, "one line for each of the two that rely on it and the one it relies on");
  assert.equal(picture.host.children[0].tagName, "SVG", "the lines sit behind the boxes");
  // The first box's right edge, at its middle, to the entity's left edge at its middle. All measured inside the picture.
  assert.equal(picture.lines()[0], "M200.0 15.0C300.0 15.0 300.0 120.0 400.0 120.0");
  assert.equal(picture.lines()[2], "M500.0 120.0C600.0 120.0 600.0 15.0 700.0 15.0");
  const watching = browser.observers.at(-1);
  assert.deepEqual(watching.watched, [picture.host]);
  layout(browser, { width: 1200 });
  watching.redraw();
  assert.equal(picture.lines()[2], "M650.0 120.0C825.0 120.0 825.0 15.0 1000.0 15.0", "the lines follow the boxes");
  // One above the other, there is no room for lines.
  layout(browser, { stacked: true });
  watching.redraw();
  assert.equal(picture.lines().length, 0);
  assert.notEqual(watching.disconnected, true);
  browser.navigate("history");
  assert.equal(watching.disconnected, true);
});

test("one failed part of an entity read shows its own reason and retry while the rest draws", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  let calls = 0;
  browser.data["/api/workspace/entity"] = () => (++calls === 1
    ? entityV2(id, { history: { ok: false, reason: "Kin could not read this entity's history. It did not answer within 8 seconds." }, relied_on_by: { ok: false, reason: "Kin could not read what relies on this entity." } })
    : entityV2(id));
  await flush();
  assert.match(browser.node("entity-history").textContent, /Kin could not read this entity's history\./);
  assert.deepEqual(entityTabs(browser).map(([words]) => words), ["Connections", "Source", "History"], "a history that could not be read has no count");
  assert.match(browser.node("entity-connections").textContent, /Kin could not read what relies on this entity\./, "the picture says why one side is empty");
  assert.match(browser.node("entity-source").textContent, /def sign/, "source still draws");
  assert.match(browser.node("entity-impact").textContent, /TestSigner\.test_sign/, "tests still draw");
  assert.deepEqual(connections(browser).reliesOn, [["want_bytes", `#/entity/${uuid(2)}`]], "and so does what it relies on");
  browser.button("Try again").click(); await flush();
  assert.doesNotMatch(browser.text(), /could not read/);
  assert.match(browser.text(), /Bound input/);
  assert.equal(connections(browser).relying.length, 2);
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
/** How many parts of a section are still waiting for their read. */
const waiting = (browser, id) => browser.node(id).querySelectorAll("[aria-label=Loading]").length;
/** An entity that six overloads of one name, a caller Kin matched by name alone, and a test rely on. */
const overloaded = (id) => entityV2(id, {
  relied_on_by: { ok: true, total: 3, confirmed: 2, shown: 3, bound: "at_least",
    notice: "Some call sites could not be proven on this machine, so there may be more. 1 of them matches by name alone, and Kin could not prove it uses this one.",
    items: [
      { id: uuid(20), name: "Serializer.__init__", kind: "method", relationship: "references", is_test: false, confirmed: true, sites: 14, declarations: 6 },
      { id: uuid(21), name: "Serializer.dumps", kind: "method", relationship: "calls", is_test: false, confirmed: false },
      { id: uuid(11), name: "TestSigner.test_sign", kind: "method", relationship: "calls", is_test: true, confirmed: true },
    ] },
});

test("an entity reads its quick parts first and draws what relies on it and its history when they land", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  const answer = sectioned(overloaded(id));
  let release;
  const slow = new Promise((done) => { release = done; });
  browser.data["/api/workspace/entity"] = (url) => /relied_on_by/.test(url.searchParams.get("sections") || "") ? slow.then(() => answer(url)) : answer(url);
  await flush();
  const reads = () => browser.requests.filter((request) => request.name === "/api/workspace/entity");
  assert.match(reads()[0].url, /sections=header,source,relies_on,members,impact,tests,editor/);
  assert.match(reads()[1].url, /sections=relied_on_by,history/);
  assert.match(browser.node("entity-source").textContent, /def sign/, "the source draws before the slow parts");
  assert.equal(waiting(browser, "entity-history"), 1, "its history waits with a placeholder");
  assert.equal(waiting(browser, "entity-connections"), 1, "and so does what relies on it");
  assert.deepEqual(connections(browser).reliesOn, [["want_bytes", `#/entity/${uuid(2)}`]], "what it relies on is already there");
  release(); await flush();
  for (const part of ["entity-history", "entity-connections"]) assert.equal(waiting(browser, part), 0);
  assert.match(connections(browser).left.textContent, /^What relies on it≥7/, "each declaration and each test that relies on it counts once");
  assert.deepEqual(connections(browser).relying, [["Serializer.__init__", `#/entity/${uuid(20)}`, "6 overloads"], ["TestSigner.test_sign", `#/entity/${uuid(11)}`, "test"]], "a caller Kin could not confirm is not drawn as relying on it");
  assert.doesNotMatch(browser.text(), /more that Kin counted and did not list/, "six declarations count as six entities");
  assert.match(browser.node("entity-relations").textContent, /Matched by name only1Serializer\.dumpsby name only/);
  assert.equal(browser.node("entity-relations").querySelectorAll("a").filter((row) => /Kin matched this by name and could not prove it uses this entity\./.test(row.getAttribute("title") || "")).length, 1);
  assert.match(browser.text(), /Kin found these by name and couldn't confirm they use it\./);
  assert.match(browser.node("entity-connections").textContent, /Some call sites could not be proven on this machine, so there may be more\./, "what Kin says about the count sits with the count");
  assert.match(browser.text(), /Bound input/);
  assert.equal(reads().length, 2, "each part is read once");
});

test("a box that stands for several overloads says so, so the count beside it adds up", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => overloaded(id);
  await flush();
  assert.match(connections(browser).left.textContent, /^What relies on it≥7/);
  assert.equal(connections(browser).relying.length, 2);
  assert.match(connections(browser).left.textContent, /6 overloads/);
});

test("the entity header leaves out what it would repeat or what is not a sentence", async () => {
  const id = uuid(9);
  const signature = (browser) => browser.node("entity-head").querySelectorAll("pre").filter((node) => node.getAttribute("aria-label") === "Signature");
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, { relies_on: { ok: true, items: [], total: 0, shown: 0 } });
  await flush();
  assert.equal(signature(browser).length, 0, "the source's first line already shows the signature");
  assert.match(connections(browser).right.textContent, /^What it relies on0Kin found no entities it relies on\.$/);
  const other = fixture({ hash: `#/entity/${id}` });
  other.data["/api/workspace/entity"] = () => entityV2(id, { source: { ok: false, reason: "Kin could not read this entity's text." } });
  await flush();
  assert.deepEqual(signature(other).map((node) => node.textContent), ["def sign(self, value)"], "without the source the signature is shown");
  // A summary is a sentence. Comment markup that came through unread is left out.
  for (const summary of ["* @param value the value to sign", "@returns the signed value", "/** Signs the value. */"]) {
    const marked = fixture({ hash: `#/entity/${id}` });
    marked.data["/api/workspace/entity"] = () => entityV2(id, { header: { ...entityV2(id).header, summary } });
    await flush();
    assert.match(marked.node("entity-head").textContent, /^Code\/SignerSigner\.signMethodCopy entity ID/, summary);
  }
  // The signature is left out whenever the source opens with it, whatever the spacing and whatever word comes first.
  for (const [signed, text] of [["def sign(self, value)", "def sign(\n    self,\n    value\n):\n    return value"], ["async function sign(value: string): Promise<string>", "export async function sign(value: string): Promise<string> {\n  return value;\n}"]]) {
    const opened = fixture({ hash: `#/entity/${id}` });
    opened.data["/api/workspace/entity"] = () => entityV2(id, { header: { ...entityV2(id).header, signature: signed }, source: { ok: true, text, start_line: 1, language: "python" } });
    await flush();
    assert.equal(signature(opened).length, 0, signed);
  }
  const elsewhere = fixture({ hash: `#/entity/${id}` });
  elsewhere.data["/api/workspace/entity"] = () => entityV2(id, { header: { ...entityV2(id).header, signature: "def sign(self, value, salt)" } });
  await flush();
  assert.deepEqual(signature(elsewhere).map((node) => node.textContent), ["def sign(self, value, salt)"], "a signature the source does not open with is shown");
  // A word each row of a list would repeat is left out; one that tells the rows apart stays.
  const tested = fixture({ hash: `#/entity/${id}` });
  tested.data["/api/workspace/entity"] = () => entityV2(id, { tests: { ok: true, items: [{ id: uuid(11), name: "TestSigner.test_sign", kind: "method", hops: 1 }, { id: uuid(12), name: "TestSigner.test_unsign", kind: "method", hops: 1 }], total: 2, shown: 2 } });
  await flush();
  assert.match(tested.node("entity-impact").textContent, /Tests that reach it2TestSigner\.test_signTestSigner\.test_unsign/);
  // A method names its class in its own name, so the place does not say it twice; a function says where it lives.
  const placed = fixture({ hash: `#/entity/${id}` });
  placed.data["/api/workspace/entity"] = () => entityV2(id, { header: { ok: true, name: "want_bytes", kind: "function", container: { id: uuid(1), name: "encoding", kind: "module" } } });
  await flush();
  assert.match(placed.node("entity-head").textContent, /^Code\/encodingwant_bytesFunction in encoding/);
});

test("an entity says one number for what relies on it, the number Explore shows", async () => {
  const id = uuid(30);
  const browser = fixture({ hash: `#/entity/${id}` });
  const relyingRows = Array.from({ length: 20 }, (_, i) => ({ id: uuid(100 + i), name: `caller_${i}`, kind: "function", relationship: "calls", is_test: i >= 15, confirmed: true, relies: true }));
  browser.data["/api/workspace/entity"] = () => entityV2(id, {
    header: { ok: true, name: "Signer", kind: "class", dependents: 21, container: { id: uuid(4), name: "signer", kind: "module" } },
    relied_on_by: { ok: true, total: 22, confirmed: 22, relies_total: 21, shown: 22, bound: "exact",
      items: [{ id: uuid(140), name: "Serializer.__init__", kind: "method", relationship: "references", is_test: false, confirmed: true, relies: true, declarations: 1 }, ...relyingRows,
        { id: uuid(141), name: "TimestampSigner", kind: "class", relationship: "extends", is_test: false, confirmed: true, relies: false }] },
    impact: { ok: true, direct: 22, transitive: 43, tests: 21, depth: 3, top: [], bound: "at_least", notice: "Kin may have missed some callers, so there could be more." },
  });
  await flush();
  const text = browser.text();
  assert.match(connections(browser).left.textContent, /^What relies on it21/, "the picture says the ranking's number");
  assert.match(connections(browser).host.getAttribute("aria-label"), /with 21 rely on it/);
  assert.doesNotMatch(text, /22 rely on it|What relies on it22|22 of them directly/, "no second number for the same thing");
  assert.match(text, /A change here might affect at least 43 entities within 3 steps\. At least 21 tests reach it\./);
  assert.match(text, /Builds on it1TimestampSignerextends it/, "a subclass is listed apart");
  // The picture shows a few, entities other than tests first, then the rest in place.
  assert.equal(connections(browser).relying.length, 7);
  assert.deepEqual(connections(browser).relying[0], ["Serializer.__init__", `#/entity/${uuid(140)}`]);
  assert.equal(connections(browser).more(connections(browser).left).textContent, "Show 14 more");
  connections(browser).more(connections(browser).left).click(); await flush();
  assert.equal(connections(browser).relying.length, 21);
  assert.deepEqual(connections(browser).relying.at(-1), ["caller_19", `#/entity/${uuid(119)}`, "test"], "tests come last, each said to be a test");
  assert.equal(connections(browser).more(connections(browser).left), undefined);
  const elsewhere = [...browser.node("entity-impact").querySelectorAll("a"), ...browser.node("entity-relations").querySelectorAll("a")].filter((row) => row.getAttribute("href") === `#/entity/${uuid(100)}`);
  assert.equal(elsewhere.length, 0, "what relies on it is listed once, in the picture");
});

test("two boxes with one name in a column say what kind each is", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, {
    relied_on_by: { ok: true, total: 3, shown: 3, bound: "exact", items: [
      { id: uuid(10), name: "dumps", kind: "method", relationship: "calls", is_test: false }, { id: uuid(15), name: "dumps", kind: "function", relationship: "calls", is_test: false },
      { id: uuid(16), name: "loads", kind: "function", relationship: "calls", is_test: false }] },
  });
  await flush();
  assert.deepEqual(connections(browser).relying, [["dumps", `#/entity/${uuid(10)}`, "method"], ["dumps", `#/entity/${uuid(15)}`, "function"], ["loads", `#/entity/${uuid(16)}`]]);
});

test("a class relies on what its members rely on", async () => {
  const id = uuid(31);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, {
    header: { ok: true, name: "Signer", kind: "class" },
    members: { ok: true, items: [{ id: uuid(32), name: "Signer.sign", kind: "method" }, { id: uuid(33), name: "Signer.unsign", kind: "method" }], total: 2, shown: 2 },
    relies_on: { ok: true, through_members: true, total: 2, shown: 2, bound: "exact", items: [
      { id: uuid(2), name: "want_bytes", kind: "function", relationship: "calls", through: ["Signer.sign", "Signer.unsign"] },
      { id: uuid(5), name: "HMACAlgorithm", kind: "class", relationship: "references", through: ["Signer.__init__"] }] },
  });
  await flush();
  assert.match(connections(browser).right.textContent, /^What it and its members rely on2/);
  assert.deepEqual(connections(browser).reliesOn, [["want_bytes", `#/entity/${uuid(2)}`], ["HMACAlgorithm", `#/entity/${uuid(5)}`]]);
  assert.equal(connections(browser).host.getAttribute("aria-label"), "Signer, with 2 rely on it, and it relies on 2 entities");
  assert.equal(browser.node("entity-members").textContent, "MembersSigner.signSigner.unsign", "a word each row would repeat is left out, and the count is the tab's");
  assert.equal(browser.node("entity-tab-members").textContent, "Members2");
});

test("an entity the current code does not hold says so plainly, with one way on", async () => {
  const id = uuid(32);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = { __status: 404, error: `No entity with id ${id} is in this repository's current graph.` };
  await flush();
  assert.match(browser.text(), /This entity isn't in the current code\.It may have been renamed or removed\./);
  assert.equal(browser.links().find((node) => node.textContent === "Search for it").getAttribute("href"), "#/explore");
  assert.equal(browser.node("entity-body").hidden, true, "empty sections are left out");
  for (const part of ["entity-reach", "entity-tabs", "entity-connections", "entity-source", "entity-impact", "entity-relations", "entity-members", "entity-history"]) assert.equal(browser.node(part).textContent, "", part);
  assert.doesNotMatch(browser.text(), /^Entity|Source/);
  // A read that failed for another reason is a failure with a retry, not a missing entity.
  const failing = fixture({ hash: `#/entity/${id}` });
  failing.failures.set("/api/workspace/entity", "The graph is not loaded.");
  await flush();
  assert.match(failing.node("entity-head").textContent, /Kin couldn't load this entity\..*The graph is not loaded\./s);
  assert.equal(failing.node("entity-body").hidden, false);
  failing.failures.delete("/api/workspace/entity");
  failing.data["/api/workspace/entity"] = () => entityV2(id);
  failing.button("Try again").click(); await flush();
  assert.match(failing.node("entity-head").textContent, /Signer\.sign/);
});

const workTag = (browser) => browser.node("entity-head").querySelectorAll("a").find((node) => node.textContent === "changed in uncommitted work");

test("an entity says it changed in uncommitted work only while the working read names it", async () => {
  const browser = fixture();
  browser.data["/api/workspace/entity"] = (url) => entityV2(url.searchParams.get("id"));
  await flush();
  browser.navigate(`entity/${uuid(2)}`); await flush();
  assert.equal(workTag(browser).getAttribute("href"), "#/uncommitted", "it leads to the work that changed it");
  browser.navigate(`entity/${uuid(3)}`); await flush();
  assert.equal(workTag(browser), undefined, "an entity the work did not change says nothing of it");

  // Opened by its own address, it is said once the working read lands, whichever read lands first.
  const direct = fixture({ hash: `#/entity/${uuid(2)}` });
  direct.data["/api/workspace/entity"] = (url) => entityV2(url.searchParams.get("id"));
  let release; direct.held.set("/api/guide", new Promise((done) => { release = done; }));
  await flush();
  assert.match(direct.node("entity-head").textContent, /Signer\.sign/, "the entity is drawn");
  assert.equal(workTag(direct), undefined, "nothing is said before Kin has said what is uncommitted");
  release(); await flush();
  assert.equal(workTag(direct).getAttribute("href"), "#/uncommitted");
  // Once the work is committed, the next reading of it takes the words away.
  direct.data["/api/guide"] = guide();
  direct.api.pulse(["working"]); await direct.advance(500);
  assert.equal(workTag(direct), undefined);
});

// ── History ────────────────────────────────────────────────────────────────

const changeRows = (browser) => browser.node("changes-results").querySelectorAll("a");

test("History lists names, dates and entity counts, and shows how each change read only once one has been read", async () => {
  const browser = fixture({ hash: "#/history" }); await flush();
  assert.equal(browser.main().querySelector("h1").textContent, "Work");
  assert.deepEqual(workTabs(browser), [["Uncommitted", "1", "#/uncommitted", null], ["Branches", "1", "#/branches", null], ["History", "", "#/history", "page"]]);
  assert.equal(browser.node("changes-lede").textContent, "Three changes. One looks right.");
  assert.match(browser.text(), /Preserved changeDana Reader.*2 entities changed.*Looks right/s);
  // A merge reads as its own review counts it, never as the history's raw delta count.
  assert.match(browser.text(), /Merge branchDana Reader.*4 entities changed.*Not reviewed/s);
  assert.deepEqual(browser.node("workspace-review-filter").querySelectorAll("button").map((node) => node.textContent), ["All", "Not reviewed", "Flagged", "Looks right"]);
  assert.doesNotMatch(browser.text(), /7 entities changed/);
  assert.match(browser.text(), /Update Kin's understanding of this code for Kin 0\.8\.2.*Kin update/s);
  const kin = changeRows(browser).find((row) => row.textContent.includes("Update Kin's understanding"));
  assert.doesNotMatch(kin.textContent, /entities changed|Not reviewed|Looks right/, "Kin's own update carries no count and waits on nobody's view");
  assert.doesNotMatch(browser.text(), /<dana@|1749933342/, "a recorded signature reads as a name");
  assert.deepEqual(changeRows(browser).map((row) => row.getAttribute("href")), [guided("change-0"), guided("change-1"), guided("change-2")], "each opens its own review");
  assert.doesNotMatch(browser.text(), /Uncommitted work|One function changed/, "what is uncommitted has a page of its own");
  assert.doesNotMatch(browser.text(), /changes shown/, "a list that is whole needs no count under it");
  assert.equal(browser.document.title, "History · pallets/Fixture");
  browser.data["/api/standing"] = { standing: {} }; browser.refresh(); await flush();
  assert.doesNotMatch(browser.node("changes-results").textContent, /Not reviewed/, "an empty column is left out");
  assert.equal(browser.node("workspace-review-filter"), null, "and so is the choice between views nobody has given");
  assert.equal(browser.node("changes-lede").textContent, "Three changes.");
});

/** What History says each change left behind, by its title: the line under it, and the tone it is drawn in. */
const consequenceLines = (browser) => Object.fromEntries(changeRows(browser).map((row) => {
  const line = row.querySelector("span").parentNode.children.find((part) => part.children.some((child) => child.getAttribute("aria-hidden") === "true"));
  return [row.querySelector("span").textContent, line ? [line.textContent, line.className.match(/-(good|warn|bad)\b/)?.[1] ?? ""] : null];
}));
const guideReads = (browser) => browser.requests.filter((request) => request.name === "/api/guide" && new URL(request.url, "http://local.test").searchParams.has("change")).map((request) => new URL(request.url, "http://local.test").searchParams.get("change"));

test("History says under each of its newest changes what that change left for the code that relies on it", async () => {
  const browser = fixture({ hash: "#/history" });
  const left = { "change-1": { recorded_consumers: 3, consumers_not_updated: 2, recorded_tests: 0 }, "change-2": { recorded_consumers: 1, consumers_may_need_review: 1, recorded_tests: 1 } };
  browser.data["/api/guide?change"] = (url) => guide({ modified: 1, ...left[url.searchParams.get("change")] }, { change: { id: url.searchParams.get("change") } });
  let release; browser.held.set("/api/guide?change", new Promise((done) => { release = done; }));
  await flush();
  assert.deepEqual(guideReads(browser), [], "nothing is read until the list is drawn");
  await browser.advance(0);
  assert.deepEqual(guideReads(browser), ["change-1"], "one at a time, and never Kin's own update");
  browser.held.delete("/api/guide?change");
  release(); await flush(); await browser.advance(0);
  assert.deepEqual(guideReads(browser), ["change-1", "change-2"]);
  assert.deepEqual(consequenceLines(browser), {
    "Update Kin's understanding of this code for Kin 0.8.2": null,
    "Preserved change": ["2 entities that rely on it were not updated. Kin found no tests that reach it.", "bad"],
    "Merge branch": ["1 entity relies on it and did not change. 1 test reaches it.", "warn"] });
  await browser.advance(60_000);
  assert.deepEqual(guideReads(browser), ["change-1", "change-2"], "each is read once");
  // What relies on a change and needed no update, and a change nothing relies on, are said too; a change that could not be read says nothing.
  const quiet = fixture({ hash: "#/history" });
  quiet.data["/api/guide?change"] = (url) => url.searchParams.get("change") === "change-1" ? guide({ modified: 1, recorded_consumers: 16, consumers_need_no_update: 16, recorded_tests: 2 }) : guide({ modified: 1 });
  await flush(); await quiet.advance(0); await quiet.advance(0);
  assert.deepEqual([consequenceLines(quiet)["Preserved change"], consequenceLines(quiet)["Merge branch"]], [["16 entities rely on it and need no update. 2 tests reach it.", "good"], ["Kin found no connections outside this change. Kin found no tests that reach it.", "good"]]);
  const failing = fixture({ hash: "#/history" });
  failing.failures.set("/api/guide?change", "Kin could not answer.");
  await flush(); await failing.advance(0); await failing.advance(0);
  assert.equal(consequenceLines(failing)["Preserved change"], null);
  assert.match(changeRows(failing)[1].textContent, /^Preserved changeDana Reader/);
});

test("History reads what its newest changes left behind, and not every change it lists", async () => {
  const browser = fixture({ hash: "#/history" });
  browser.data["/api/changes"] = { changes: Array.from({ length: 9 }, (_, i) => ({ id: `c-${i}`, title: `Change ${i}`, author: "Dana Reader", timestamp: secondsAgo(i * 3600) })), truncated: false };
  browser.data["/api/counts"] = (url) => ({ counts: Object.fromEntries(url.searchParams.get("changes").split(",").map((id, i) => [id, { entities: id === "c-2" ? 0 : id === "c-3" ? 500 : 1 }])) });
  await flush();
  for (let i = 0; i < 12; i++) await browser.advance(0);
  assert.deepEqual(guideReads(browser), ["c-0", "c-1", "c-4", "c-5", "c-6", "c-7"], "six, newest first, past a change with no entity and one too large to read quickly");
});

test("History can be narrowed to how a change read, and by its title, author or id", async () => {
  const browser = fixture({ hash: "#/history" });
  browser.data["/api/standing"] = { standing: { "change-1": { state: "approved" }, "change-2": { state: "needs_work" } } };
  await flush();
  const filter = () => browser.node("workspace-review-filter").querySelectorAll("button");
  const titles = () => changeRows(browser).map((row) => row.querySelector("span").textContent);
  assert.deepEqual(filter().map((node) => node.textContent), ["All", "Not reviewed", "Flagged", "Looks right"]);
  assert.match(changeRows(browser).find((row) => row.textContent.startsWith("Merge branch")).textContent, /Flagged$/);
  filter()[3].click();
  assert.deepEqual(titles(), ["Preserved change"]);
  assert.deepEqual(filter().map((node) => node.getAttribute("aria-pressed")), ["false", "false", "false", "true"]);
  filter()[2].click();
  assert.deepEqual(titles(), ["Merge branch"]);
  filter()[1].click();
  assert.deepEqual(titles(), ["Update Kin's understanding of this code for Kin 0.8.2"]);
  filter()[0].click();
  assert.equal(titles().length, 3);
  browser.type("workspace-review-search", "MERGE");
  assert.deepEqual(titles(), ["Merge branch"]);
  browser.type("workspace-review-search", "change-1");
  assert.deepEqual(titles(), ["Preserved change"]);
  browser.type("workspace-review-search", "nothing like this");
  assert.equal(browser.node("changes-results").textContent, "No change matches.");
  // The field keeps what was typed while the list is read again.
  browser.refresh(); await flush();
  assert.equal(browser.node("workspace-review-search").value, "nothing like this");
  // A choice is kept only while there is a view to choose by: once none is left, every change is listed again.
  browser.type("workspace-review-search", "");
  filter()[3].click();
  assert.deepEqual(titles(), ["Preserved change"]);
  browser.data["/api/standing"] = { standing: {} };
  browser.refresh(); await flush();
  assert.equal(browser.node("workspace-review-filter"), null);
  assert.equal(titles().length, 3);
  // Until how changes read is known, there is nothing to narrow by.
  const unread = fixture({ hash: "#/history" });
  unread.failures.set("/api/standing", "Reviews unreadable");
  await flush();
  assert.equal(unread.node("workspace-review-filter"), null);
  assert.equal(changeRows(unread).length, 3);
});

test("a long history says how much of it is shown and reads older changes when asked", async () => {
  const browser = fixture({ hash: "#/history" });
  browser.data["/api/changes"] = (url) => ({ changes: Array.from({ length: Number(url.searchParams.get("count")) }, (_, i) => ({ id: `c-${i}`, title: `Change ${i}`, author: "Dana Reader", timestamp: secondsAgo(i * 3600) })), truncated: Number(url.searchParams.get("count")) < 240 });
  browser.data["/api/counts"] = (url) => ({ counts: Object.fromEntries(url.searchParams.get("changes").split(",").map((id) => [id, { entities: 1 }])) });
  await flush();
  assert.match(browser.node("changes-lede").textContent, /^60 most recent changes\./);
  assert.match(browser.node("changes-results").textContent, /60 changes shown/);
  browser.button("Show older changes").click(); await flush();
  assert.match(browser.requests.filter((request) => request.name === "/api/changes").at(-1).url, /count=120$/);
  assert.equal(changeRows(browser).length, 120);
  browser.button("Show older changes").click(); await flush();
  assert.equal(changeRows(browser).length, 240);
  assert.equal(browser.button("Show older changes"), undefined, "the whole history is here");
  assert.doesNotMatch(browser.node("changes-results").textContent, /changes shown/);
});

test("a history Kin is still reading says how far it has got", async () => {
  const lede = async (history) => { const browser = fixture({ hash: "#/history", session: history ? { history } : {} }); await flush(); return browser.node("changes-lede").textContent; };
  assert.equal(await lede({ state: "partial", admitted: 1200, total: 4000 }), "Three changes. One looks right.Kin has read 1,200 of 4,000 commits from this repository's history. This list includes the history available so far.");
  assert.equal(await lede({ state: "partial" }), "Three changes. One looks right.This repository's imported history is incomplete. This list includes the history available so far.");
  assert.equal(await lede({ state: "complete", admitted: 4000, total: 4000 }), "Three changes. One looks right.");
  assert.equal(await lede(null), "Three changes. One looks right.");
});

/** The authors History marks as agents. */
const agentAuthors = (browser) => browser.main().querySelectorAll("span").filter((node) => node.getAttribute("title") === "Made by an agent through Kin");

test("History marks an agent's change and filters by who made it", async () => {
  const browser = fixture({ hash: "#/history" });
  browser.data["/api/changes"].changes.unshift({ id: AGENT_CHANGE, title: "Explain how want_bytes treats text", author: "Claude Code", agent: { name: "Claude Code", key: AGENT_KEY }, timestamp: secondsAgo(120), entity_deltas: 1 });
  await flush(); await flush();
  const marked = agentAuthors(browser);
  assert.equal(marked.length, 1);
  assert.equal(marked[0].textContent, "Claude Code");
  const filter = browser.node("workspace-author-filter");
  const buttons = filter.querySelectorAll("button");
  assert.deepEqual(buttons.map((node) => node.textContent), ["Everyone", "People", "Claude Code1"]);
  buttons.find((node) => node.textContent === "People").click();
  assert.doesNotMatch(browser.node("changes-results").textContent, /Explain how want_bytes treats text/);
  assert.match(browser.node("changes-results").textContent, /Preserved change/);
  browser.node("workspace-author-filter").querySelectorAll("button").find((node) => node.textContent.startsWith("Claude Code")).click();
  assert.match(browser.node("changes-results").textContent, /Explain how want_bytes treats text/);
  assert.doesNotMatch(browser.node("changes-results").textContent, /Preserved change/);
});

test("a person-only history offers no author filter", async () => {
  const browser = fixture({ hash: "#/history" }); await flush(); await flush();
  assert.equal(browser.node("workspace-author-filter"), null);
  assert.equal(agentAuthors(browser).length, 0);
});

test("History's sentence says how a change read in the word its row says", async () => {
  const browser = fixture({ hash: "#/history" });
  browser.data["/api/standing"] = { standing: { "change-1": { state: "approved" }, "change-2": { state: "needs_work" } } };
  await flush();
  assert.match(changeRows(browser).find((row) => row.textContent.startsWith("Merge branch")).textContent, /Flagged$/);
  assert.equal(browser.node("changes-lede").textContent, "Three changes. One looks right, one is flagged.");
  browser.data["/api/standing"] = { standing: { "change-1": { state: "blocked" }, "change-2": { state: "needs_work" } } };
  browser.refresh(); await flush();
  assert.equal(browser.node("changes-lede").textContent, "Three changes. Two are flagged.");
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
  assert.equal(link.getAttribute("href"), guided("04".repeat(32)));
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
  browser.refresh(); await flush(); browser.navigate("uncommitted"); browser.navigate("branches");
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
  assert.deepEqual([...browser.tabStorage.keys()].filter((key) => key.includes("pending-")), [], "definite pre-write refusal clears the pending receipt");
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

/** What this tab keeps to remember a request whose result it never saw. The last working read it also keeps is not one. */
const receipts = (tabStorage) => [...tabStorage].filter(([key]) => key.includes("pending-")).map(([, value]) => value);

test("reloading a tab with an in-flight confirmation restores the receipt without replaying a request", async () => {
  const tabStorage = new Map();
  const browser = fixture({ tabStorage }); await flush(); await browser.preview();
  let resolve; browser.held.set("/api/workspace/merge/confirm", new Promise((done) => { resolve = done; }));
  browser.confirm();
  assert.equal(receipts(tabStorage).length, 1, "receipt is persisted before confirmation is dispatched");
  assert.doesNotMatch(receipts(tabStorage).join(""), /synthetic-token|workspace_tree_hash/);
  assert.doesNotMatch([...tabStorage.values()].join(""), /synthetic-token/, "nothing this tab keeps carries the session's token");
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
    const receipt = JSON.parse(receipts(tabStorage)[0]);
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
/** What a branch's row offers, in order. */
const branchRow = (browser, name) => browser.node("branches-list").querySelectorAll("li").find((node) => node.querySelector(".branch-label")?.textContent === name);
/** What a branch's row offers besides opening it: its buttons. */
const branchActions = (browser, name) => branchRow(browser, name).querySelectorAll("button").map((node) => node.textContent);
/** Where a branch's row opens, when it opens anywhere: the range of what it holds against the current branch. */
const branchOpens = (browser, name) => branchRow(browser, name).querySelector("a.branch-open")?.getAttribute("href") ?? null;

test("the branch list opens a switch or a merge only when asked", async () => {
  const browser = fixture({ hash: "#/branches" }); await flush();
  assert.match(browser.text(), /You're on main\. One other branch: feature\./);
  assert.doesNotMatch(browser.text(), /You are here/);
  assert.deepEqual(branchActions(browser, "feature"), ["Switch", "Merge into main"], "a branch that holds something can be switched to or merged");
  assert.equal(branchOpens(browser, "feature"), "#/review/r/main..feature", "and its row opens what it holds, against the current branch");
  assert.equal(branchOpens(browser, "main"), null, "the current branch has nothing to hold against itself");
  assert.equal(browser.node("workspace-branch-target"), null);
  assert.equal(browser.node("workspace-merge-source"), null);
  assert.equal(browser.node("branches-conflicts").textContent, "", "with no merge to resolve, nothing is said of one");
  assert.equal(browser.requests.filter((request) => request.method !== "GET").length, 0, "nothing is previewed until asked");
  assert.equal(browser.document.title, "Branches · pallets/Fixture");
});

test("each other branch says what it holds beyond the current one and opens its review", async () => {
  const browser = fixture({ hash: "#/branches" });
  await flush(); await flush();
  assert.match(browser.text(), /featureOne function changed\. Three other places rely on it\. None of them changed\. Two tests reach it\./);
  assert.match(browser.text(), /1 change ahead of main · Bound base64 input before decoding · Dana Reader, 10 minutes ago/);
  assert.equal(branchOpens(browser, "feature"), "#/review/r/main..feature", "what it holds is reviewed like a change, from anywhere on its row");
  assert.equal(branchRow(browser, "feature").querySelector("a.branch-open").textContent, "feature");
  assert.equal(browser.requests.filter((request) => /base=main/.test(request.url)).length, 1, "read once");
});

test("a branch with nothing beyond the current one offers no review and no merge", async () => {
  const browser = fixture({ hash: "#/branches" });
  browser.data["/api/branches"] = { branches: [{ name: "main", active: true, default: true }, { name: "feature", active: false }, { name: "level", active: false }] };
  browser.data["/api/workspace/branches"] = { schema: "kin.branch-list.v1", branches: [{ name: refWire("main"), active: true }, { name: refWire("feature"), active: false }, { name: refWire("level"), active: false }] };
  const ahead = browser.data["/api/guide?base"];
  browser.data["/api/guide?base"] = (url) => url.searchParams.get("change") === "level" ? guide({}, { range: { base_ref: "main", head_ref: "level", changes_total: 0, base_changes_total: 0, changes: [] } }) : ahead;
  let release; browser.held.set("/api/guide?base", new Promise((done) => { release = done; }));
  await flush();
  // Until Kin has said what a branch holds, nothing is withheld.
  assert.deepEqual([branchActions(browser, "level"), branchOpens(browser, "level")], [["Switch", "Merge into main"], "#/review/r/main..level"]);
  release(); await flush(); await flush();
  assert.deepEqual([branchActions(browser, "feature"), branchOpens(browser, "feature")], [["Switch", "Merge into main"], "#/review/r/main..feature"]);
  assert.deepEqual([branchActions(browser, "level"), branchOpens(browser, "level")], [["Switch"], null], "there is nothing in it to review or to merge");
  assert.match(browser.node("branches-list").textContent, /levelNo work of its own, even with main/);
  assert.match(browser.text(), /You're on main\. Two other branches: feature and level\./);
});

test("Switch is offered before the branch names are read and opens the panel to wait for them", async () => {
  const browser = fixture({ hash: "#/branches" });
  let release; browser.held.set("/api/workspace/branches", new Promise((done) => { release = done; }));
  await flush();
  const offeredSwitch = browser.button("Switch");
  assert.ok(offeredSwitch, "the row offers a switch while the exact names are still being read");
  offeredSwitch.click();
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
  assert.deepEqual(receipts(browser.tabStorage), []);
});

test("branch stale refusal discards the preview and requires a new explicit confirmation", async () => {
  const browser = fixture(); await flush(); await branchPreview(browser);
  browser.failures.set("/api/workspace/branch/confirm", { status: 409, body: { outcome: "refused", error: "Workspace changed after preview" } });
  branchConfirm(browser); await flush();
  assert.match(browser.text(), /refused before changing the workspace/);
  assert.equal(browser.node("workspace-branch-submit"), null);
  assert.deepEqual(receipts(browser.tabStorage), []);
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
  browser.navigate("history");
  assert.match(browser.text(), /Preserved change/);
});

test("a branch confirmation cannot reuse a merge preview made before switching", async () => {
  const browser = fixture(); await flush(); await browser.preview();
  assert.ok(browser.node("workspace-merge-confirm"));
  await branchPreview(browser); branchConfirm(browser); await flush();
  browser.navigate("uncommitted"); browser.navigate("branches");
  assert.equal(browser.node("workspace-merge-confirm"), null);
  assert.equal(browser.requests.filter((row) => row.name === "/api/workspace/merge/confirm").length, 0);
});

test("a failure is one plain line, one action, and Kin's own words behind Show details", async () => {
  const browser = fixture({ hash: "#/branches" });
  // A branch Kin is only slow to read says so and is asked for again by itself.
  browser.failures.set("/api/guide", { status: 502, body: { error: "Kin could not answer: It did not answer within 20 seconds." } });
  await flush();
  assert.match(browser.text(), /Still reading what [^ ]+ changed/);
  // One it cannot read is a failure: one plain line, one action, and Kin's own words behind Show details.
  browser.failures.set("/api/guide", { status: 500, body: { error: "Kin could not answer: the branch's record is unreadable." } });
  await browser.advance(4000); await flush();
  assert.match(browser.text(), /Kin couldn't load what this branch holds\./);
  assert.ok(browser.button("Try again"));
  assert.match(browser.text(), /Show details.*the branch's record is unreadable/s);
});

test("two branches are compared from Branches, and what was typed survives a refresh", async () => {
  const browser = fixture({ hash: "#/branches" }); await flush();
  const toggle = () => browser.node("branches-compare-toggle");
  assert.equal(toggle().getAttribute("aria-expanded"), "false");
  assert.equal(browser.node("workspace-range-base"), null, "the form waits to be asked for");
  toggle().click();
  assert.equal(toggle().getAttribute("aria-expanded"), "true");
  assert.equal(browser.document.activeElement.id, "workspace-range-base");
  assert.equal(browser.node("workspace-range-base").value, "main", "it starts from the default branch");
  const base = browser.node("workspace-range-base"); base.value = "release-base"; base.emit("input"); base.focus();
  const head = browser.node("workspace-range-head"); head.value = "feature"; head.emit("input");
  browser.refresh(); await flush();
  assert.equal(browser.node("workspace-range-base").value, "release-base");
  assert.equal(browser.node("workspace-range-head").value, "feature");
  assert.equal(browser.document.activeElement.id, "workspace-range-base");
  const form = () => browser.main().querySelectorAll("form").find((node) => node.getAttribute("aria-label") === "Compare branches");
  // Left as it is, the end is the branch that is checked out.
  browser.type("workspace-range-head", "HEAD");
  form().emit("submit");
  assert.equal(browser.location.hash, "#/review/r/release-base..main");
  browser.type("workspace-range-head", "feat/bound-input");
  form().emit("submit");
  assert.equal(browser.location.hash, "#/review/r/release-base..feat%2Fbound-input", "a name with a slash in it is still one name");
  await flush();
  assert.equal(browser.node("review-head").querySelector("h1").textContent, "feat/bound-input against release-base", "comparing opens the review of what one holds beyond the other");
  browser.navigate("branches");
  assert.equal(toggle().getAttribute("aria-expanded"), "true", "the form is as it was left");
  toggle().click();
  assert.equal(browser.node("workspace-range-base"), null);
  // A blank start compares nothing.
  toggle().click();
  browser.type("workspace-range-base", "  ");
  form().emit("submit"); await flush();
  assert.equal(browser.location.hash, "#/branches");
  // With one branch there is nothing to compare.
  const alone = fixture({ hash: "#/branches" });
  alone.data["/api/branches"] = { branches: [{ name: "main", active: true, default: true }] };
  await flush();
  assert.equal(alone.node("branches-compare-toggle"), null);
  assert.match(alone.text(), /You're on main, the only branch\./);
});

const newBranch = (browser) => browser.main().querySelectorAll("form").find((form) => form.getAttribute("aria-label") === "New branch");

test("a new branch is named once, starts at the current branch, and the lists are read again", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { branch_create: true } }); await flush();
  assert.equal(newBranch(browser), undefined, "the form waits to be asked for");
  browser.button("New branch").click();
  assert.equal(browser.document.activeElement.id, "branch-new-name");
  assert.equal(browser.button("New branch"), undefined, "the button that opened it is gone while it is open");
  assert.match(newBranch(browser).textContent, /starting at main's latest change/);
  assert.equal(browser.node("branch-new-submit").disabled, true, "a branch needs a name");
  browser.type("branch-new-name", "   ");
  newBranch(browser).emit("submit");
  assert.equal(browser.count("/api/workspace/branch/create"), 0);
  browser.type("branch-new-name", "  feat/bound-input ");
  assert.equal(browser.node("branch-new-submit").disabled, false);
  browser.data["/api/workspace/branch/create"] = (url, options) => {
    browser.data["/api/branches"] = { branches: [...browser.data["/api/branches"].branches, { name: "feat/bound-input", active: false }] };
    return { outcome: "created", operation_id: JSON.parse(options.body).operation_id, name: "feat/bound-input" };
  };
  const reads = { branches: browser.count("/api/branches"), names: browser.count("/api/workspace/branches") };
  newBranch(browser).emit("submit");
  assert.equal(browser.node("branch-new-submit").disabled, true);
  newBranch(browser).emit("submit");
  await flush();
  const calls = browser.requests.filter((request) => request.name === "/api/workspace/branch/create");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "POST");
  assert.equal(calls[0].options.headers["x-kin-review-token"], "synthetic-token");
  assert.deepEqual(JSON.parse(calls[0].options.body), { name: "feat/bound-input", operation_id: uuid(1) });
  assert.match(browser.node("branches-new").textContent, /^Created feat\/bound-inputIt starts at main's latest change\./);
  assert.equal(browser.count("/api/branches"), reads.branches + 1);
  assert.equal(browser.count("/api/workspace/branches"), reads.names + 1);
  assert.match(browser.text(), /You're on main\. Two other branches: feature and feat\/bound-input\./);
  assert.ok(browser.button("New branch"), "another can be made");
  browser.button("Dismiss").click();
  assert.equal(browser.node("branches-new").textContent, "");
});

test("a branch Kin refuses to create says why and keeps the name, and a session that may not create one is not offered the form", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { branch_create: true } }); await flush();
  browser.button("New branch").click();
  browser.type("branch-new-name", "main");
  browser.data["/api/workspace/branch/create"] = { __status: 409, error: "refs/heads/main exists", body: { outcome: "refused", remedy: "A branch named main already exists. Choose another name." } };
  newBranch(browser).emit("submit"); await flush();
  assert.match(newBranch(browser).querySelector("[role=alert]").textContent, /The branch was not created.*A branch named main already exists\. Choose another name\./s);
  assert.equal(browser.node("branch-new-name").value, "main");
  assert.doesNotMatch(browser.text(), /Created main/);
  browser.button("Cancel").click();
  assert.equal(newBranch(browser), undefined);
  assert.ok(browser.button("New branch"));

  for (const options of [{}, { capabilities: { branch_create: true }, session: { token: "" } }]) {
    const reader = fixture({ hash: "#/branches", ...options }); await flush();
    assert.equal(reader.button("New branch"), undefined);
  }
});

// A merge parked with one entity changed on both sides, as the conflict read answers it.
// The read also carries what a terminal would be given for each step; the page offers the steps as buttons instead.
const RECORD = "e".repeat(64);
const CONTESTED = uuid(40);
function parked({ record = RECORD, settled = false, unavailable = [] } = {}) {
  const step = (id, label, selector, args) => ({ id, label, argv: ["kin", "resolve", "--expect", record, ...args], modifies: true, selector });
  return { schema: "kin.workspace.conflicts.v1",
    report: { record_hash: record, unresolved_count: settled ? 0 : 1, resolved_count: settled ? 1 : 0,
      record: { binding: { target_ref: refWire("main"), source_ref: refWire("feature") },
        entries: [{ subject: { subject: "entity", entity: CONTESTED }, divergence: { divergence: "changed_both_sides" }, label: "Signer.sign", resolution: { resolution: settled ? "theirs" : "unresolved" } }] },
      bodies: [{ subject: `entity ${CONTESTED}`, label: "Signer.sign", base: "def sign(): return 0", ours: "def sign(): return 1", theirs: "def sign(): return 2", unverified: [] }] },
    state: { status: "in_progress", active: true, all_resolved: settled },
    provenance: { base_change: "1".repeat(64), ours_change: "2".repeat(64), theirs_change: "3".repeat(64), record_hash: record },
    display: { truncated: false, truncations: [], omitted_body_rows: 0 },
    handoff: { commands: settled
      ? [step("continue", "Publish the recorded resolutions", null, ["--continue"]), step("abort", "Abandon the merge; keep current workspace files", null, ["--abort"])]
      : [step(`ours:${CONTESTED}`, "Use the recorded target side", CONTESTED, ["--ours", CONTESTED]), step(`theirs:${CONTESTED}`, "Use the recorded incoming side", CONTESTED, ["--theirs", CONTESTED]),
        step(`base:${CONTESTED}`, "Use the recorded base side", CONTESTED, ["--base", CONTESTED]), step(`remove:${CONTESTED}`, "Remove this identity from the merge", CONTESTED, ["--remove", CONTESTED]),
        step("abort", "Abandon the merge; keep current workspace files", null, ["--abort"])],
      unavailable_subjects: unavailable, unavailable: null,
      instructions: ["Run at most one chosen command in this repository's working tree. This page does not execute commands.", "argv contains separate arguments, not a shell command."] },
    read_at: 1_770_000_000 };
}
const NO_MERGE = { schema: "kin.workspace.conflicts.v1", report: { record: null }, state: { status: "none", active: false } };
const conflictPanel = (browser) => browser.node("branches-conflicts");
const settleSteps = (browser) => conflictPanel(browser).querySelectorAll("button").map((node) => node.textContent);

test("a merge to resolve is settled with buttons that name the step Kin offered and the record it was read at", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { conflict_settle: true } });
  browser.data["/api/workspace/conflicts/details"] = parked();
  await flush();
  assert.match(conflictPanel(browser).textContent, /Needs resolution.*1 unresolved, 0 resolved\. The branches are not merged yet\./s);
  assert.match(conflictPanel(browser).textContent, /Signer\.signUnresolved/);
  assert.match(conflictPanel(browser).textContent, /Entity · changed both sides/);
  assert.match(conflictPanel(browser).textContent, /1 conflict left to settle before the merge can finish\./);
  assert.deepEqual(settleSteps(browser), ["Keep the target side", "Take the incoming side", "Go back to the common base", "Remove it from the merge", "Abandon the merge"], "each conflict's choices, then the one step for the whole merge that is open now");
  assert.equal(conflictPanel(browser).querySelectorAll("a").find((node) => node.textContent === "Open the current entity").getAttribute("href"), `#/entity/${CONTESTED}`);
  // The page offers the steps. It never prints what a terminal would be given for them.
  assert.doesNotMatch(conflictPanel(browser).textContent, /kin resolve|--expect|--ours|--theirs|--continue|--abort|argv|Run at most one/);
  assert.equal(conflictPanel(browser).querySelectorAll("input").length + conflictPanel(browser).querySelectorAll("textarea").length, 0, "nothing is typed to settle a merge");

  const NEXT = "9".repeat(64);
  browser.data["/api/workspace/conflicts/settle"] = (url, options) => {
    browser.data["/api/workspace/conflicts/details"] = parked({ record: NEXT, settled: true });
    return { outcome: "settled", operation_id: JSON.parse(options.body).operation_id, details: parked({ record: NEXT, settled: true }) };
  };
  const reads = browser.count("/api/workspace/conflicts/details");
  browser.button("Take the incoming side").click();
  assert.ok(conflictPanel(browser).querySelectorAll("button").every((node) => node.disabled), "one step at a time");
  conflictPanel(browser).querySelectorAll("button").forEach((node) => node.click());
  await flush();
  const calls = browser.requests.filter((request) => request.name === "/api/workspace/conflicts/settle");
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "POST");
  assert.equal(calls[0].options.headers["x-kin-review-token"], "synthetic-token");
  assert.deepEqual(JSON.parse(calls[0].options.body), { command: `theirs:${CONTESTED}`, record_hash: RECORD, operation_id: uuid(1) }, "the step's own id and the record it was offered against, and nothing typed");
  // The answer's details replace what was shown, and the lists that depend on the merge are read again.
  assert.match(conflictPanel(browser).textContent, /0 unresolved, 1 resolved/);
  assert.match(conflictPanel(browser).textContent, /Signer\.signResolved/);
  assert.match(conflictPanel(browser).textContent, new RegExp(`Record${NEXT}`));
  assert.match(conflictPanel(browser).textContent, /Each conflict is settled\. Finishing completes the merge\./);
  assert.deepEqual(settleSteps(browser), ["Abandon the merge", "Finish the merge"]);
  assert.equal(browser.count("/api/workspace/conflicts/details"), reads + 1);

  browser.data["/api/workspace/conflicts/settle"] = () => { browser.data["/api/workspace/conflicts/details"] = NO_MERGE; return { outcome: "continued", details: NO_MERGE }; };
  browser.button("Finish the merge").click(); await flush();
  assert.deepEqual(browser.sent("/api/workspace/conflicts/settle")[1], { command: "continue", record_hash: NEXT, operation_id: uuid(2) }, "the next step names the record the last answer gave");
  assert.equal(conflictPanel(browser).textContent, "", "with the merge done there is nothing left to resolve");
});

test("a step Kin refuses changes nothing and can be chosen again, and one whose outcome is unknown is never sent again", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { conflict_settle: true } });
  browser.data["/api/workspace/conflicts/details"] = parked();
  await flush();
  browser.data["/api/workspace/conflicts/settle"] = { __status: 409, error: "The record moved.", body: { outcome: "refused", remedy: "The merge changed since this page read it. Choose again." } };
  browser.button("Keep the target side").click(); await flush();
  assert.match(conflictPanel(browser).querySelector("[role=alert]").textContent, /That step was not taken.*The merge changed since this page read it\. Choose again\./s);
  assert.equal(browser.button("Keep the target side").disabled, false);

  browser.failures.set("/api/workspace/conflicts/settle", new Error("Connection closed after request"));
  browser.button("Keep the target side").click(); await flush();
  assert.match(conflictPanel(browser).querySelector("[role=alert]").textContent, /That step's outcome is unknown.*This page will not send it again\./s);
  assert.ok(conflictPanel(browser).querySelectorAll("button").every((node) => node.disabled));
  conflictPanel(browser).querySelectorAll("button").forEach((node) => node.click());
  browser.refresh(); await flush();
  browser.navigate("uncommitted"); browser.navigate("branches");
  conflictPanel(browser).querySelectorAll("button").forEach((node) => node.click());
  await flush();
  assert.equal(browser.count("/api/workspace/conflicts/settle"), 2, "the refused step and the lost one, and no third");
});

test("a session that cannot settle a merge sees it and is offered no step", async () => {
  for (const options of [{}, { capabilities: { conflict_settle: true }, session: { token: "" } }]) {
    const browser = fixture({ hash: "#/branches", ...options });
    browser.data["/api/workspace/conflicts/details"] = parked({ unavailable: [{ selector: CONTESTED, reason: "Kin cannot tell this entity from another with the same identity." }] });
    await flush();
    assert.match(conflictPanel(browser).textContent, /1 unresolved, 0 resolved/);
    assert.deepEqual(settleSteps(browser), []);
    assert.match(conflictPanel(browser).textContent, /This version of Kin can't settle a merge from this page\./);
    assert.match(conflictPanel(browser).textContent, /This conflict can't be settled from here yet.*Kin cannot tell this entity from another with the same identity\./s);
    assert.doesNotMatch(conflictPanel(browser).textContent, /kin resolve|--expect|argv|Run at most one/);
  }
  const unreadable = fixture({ hash: "#/branches", capabilities: { conflict_settle: true } });
  unreadable.data["/api/workspace/conflicts/details"] = { schema: "kin.workspace.conflicts.v2", report: {}, state: {} };
  await flush();
  assert.match(conflictPanel(unreadable).textContent, /The merge record could not be read\. Resolving is unavailable\./);
  assert.deepEqual(settleSteps(unreadable), []);
});

// ── Agents ─────────────────────────────────────────────────────────────────

test("an agent's calls read as plain sentences about the entities they were about", () => {
  const browser = fixture();
  const A = browser.agents;
  const names = { [uuid(5)]: { label: "Signer.verify_signature" }, [uuid(2)]: { label: "want_bytes" }, [uuid(3)]: { label: "Signer" } };
  const say = (call) => A.plainText(A.describe(call, names));
  assert.equal(say({ tool: "semantic_locate", entity_ids: [uuid(5), uuid(3)], result_count: 5 }), "Looked up Signer.verify_signature and 4 related entities");
  assert.equal(say({ tool: "impact_analysis", entity_ids: [uuid(2)], result_count: 9 }), "Checked what a change to want_bytes would touch");
  assert.equal(say({ tool: "find_references", entity_ids: [uuid(5)] }), "Checked what uses Signer.verify_signature");
  assert.equal(say({ tool: "get_entity_source", entity_ids: [uuid(9), uuid(3)] }), "Read Signer");
  assert.equal(say({ tool: "trace_path", entity_ids: [uuid(5), uuid(2)] }), "Traced how Signer.verify_signature reaches want_bytes");
  assert.equal(say({ tool: "kin_transaction_commit", entity_ids: [uuid(2), uuid(3), uuid(5)] }), "Committed a change to want_bytes", "entities that only moved with the edit are not claimed as changed");
  assert.equal(say({ tool: "kin_transaction_begin", entity_ids: [] }), "Started a change");
  assert.equal(say({ tool: "semantic_locate", entity_ids: [], result_count: 0 }), "Looked something up and found no entity");
  assert.equal(say({ tool: "find_references", entity_ids: [uuid(9)] }), "Checked what uses an entity");
  assert.equal(say({ tool: "get_entity_source", entity_ids: [uuid(2)], is_error: true }), "Tried to read want_bytes; Kin could not do it", "a failed call reads as what it tried");
  assert.equal(say({ tool: "some_new_tool", entity_ids: [uuid(2)] }), "Asked Kin about want_bytes");
  const groups = A.group([{ tool: "get_entity_source", entity_ids: [uuid(5)] }, { tool: "get_entity_source", entity_ids: [uuid(5)] }, { tool: "find_references", entity_ids: [uuid(5)] }]);
  assert.deepEqual([...groups.map((entry) => entry.times)], [2, 1]);
});

test("refused tool names do not become reads in an agent's summary", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  const calls = [
    { seq: 5, at: secondsAgo(2), tool: "kin_transaction_commit", entity_ids: [uuid(2)], is_error: false },
    { seq: 4, at: secondsAgo(3), tool: "unrecognized_tool", entity_ids: [uuid(9)], result_count: 1, is_error: true },
    { seq: 3, at: secondsAgo(4), tool: "unknown", entity_ids: [uuid(10)], result_count: 1, is_error: true },
    { seq: 2, at: secondsAgo(5), tool: "get_entity", entity_ids: [uuid(3)], result_count: 1, is_error: false },
    { seq: 1, at: secondsAgo(6), tool: "get_entity_source", entity_ids: [uuid(2)], result_count: 1, is_error: false },
  ];
  const agent = { ...agentsPayload().agents[0], calls_total: calls.length, recent_calls: calls };
  const data = agentsPayload({ agents: [agent] });
  data.names[uuid(9)] = { label: "RefusedTarget", kind: "function" };
  data.names[uuid(10)] = { label: "UnknownTarget", kind: "function" };
  browser.data["/api/workspace/agents"] = data;
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true,
    kept: calls.length, earliest_kept_from_start: true, agent: { ...agent, recent_calls: null }, calls, names: data.names };
  await flush(); await flush(); await flush();
  const A = browser.agents;
  assert.equal(A.family("unrecognized_tool"), "unknown");
  assert.equal(A.family("unknown"), "unknown");
  assert.equal(A.family("get_entity"), "other", "registered reads without their own sentence keep the read fallback");
  assert.equal(A.family("get_entity_source"), "source");
  assert.match(browser.text(), /Five calls to Kin, 2 entities looked at, 1 change committed\./);
  assert.match(browser.text(), /Tried to ask Kin about RefusedTarget; Kin could not do it/, "refused calls remain visible as failed calls");
  assert.match(browser.text(), /Tried to ask Kin about UnknownTarget; Kin could not do it/);
});

/** The Agents page's lanes as a reader meets them: each one's head, where it leads, and the rows under it. */
const lanesOf = (browser) => browser.node("agents-list").querySelectorAll("li[data-state]").map((node) => {
  const [head, body] = node.children;
  const rows = body ? body.children.filter((row) => row.children.length === 2 && row.children[1].tagName === "SPAN").map((row) => [row.children[0].textContent, row.children[1].querySelectorAll("a").length && row.children[0].textContent !== "Last" ? row.children[1].querySelectorAll("a").map((chip) => [chip.textContent, chip.getAttribute("href")]) : row.children[1].textContent]) : [];
  return { node, head: head.children.filter((part) => part.getAttribute("aria-hidden") !== "true").map((part) => part.textContent), href: head.getAttribute("href"), open: head.getAttribute("aria-expanded"), state: node.dataset.state,
    rows, said: body ? body.children.filter((part) => part.tagName === "P").map((part) => part.textContent) : null };
});

test("the Agents page lists agents and shows the selected one's calls and changes, newest first", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` }); await flush(); await flush(); await flush();
  const timeline = browser.requests.filter((row) => row.name === "/api/workspace/agents/timeline");
  assert.ok(timeline.length >= 1);
  assert.match(timeline[0].url, new RegExp(`agent=${AGENT_KEY}`));
  const text = browser.text();
  assert.match(text, /One agent is connected and working\./);
  assert.match(text, /Connected for 4 minutes · through Kin’s MCP server/);
  assert.match(text, /Seven calls to Kin, 4 entities looked at, 1 change committed\./);
  assert.match(text, /Has 1 edit staged, not committed yet/, "in the words Home uses for the same open change");
  assert.match(text, /The change it committed/);
  assert.match(text, /What it did/);
  assert.match(text, /Committed a change to want_bytes/);
  assert.match(text, /Staged an edit to want_bytes/);
  assert.match(text, /Read Signer\.verify_signature/);
  assert.match(text, /2 times/);
  assert.ok(text.indexOf("Committed a change to want_bytes") < text.indexOf("Staged an edit to want_bytes"), "newest first");
  const selected = browser.main().querySelectorAll("a[aria-current=true]");
  assert.deepEqual(selected.map((node) => [node.getAttribute("href"), node.getAttribute("aria-expanded")]), [[`#/agents/${AGENT_KEY}`, "true"]]);
  assert.match(selected[0].textContent, /^Claude Codesince \d{1,2}[:.]\d{2}.* · 7 calls · 1 change committedActive$/, "a session is named with when it started, so two of one agent can be told apart");
  // The open lane holds everything the agent did, under what it is looking at.
  const [lane] = lanesOf(browser);
  assert.ok(lane.node.contains(browser.node("agents-detail")));
  assert.deepEqual(lane.rows, [["Looking at", [["want_bytes", `#/entity/${uuid(2)}`], ["Signer", `#/entity/${uuid(3)}`], ["Signer.verify_signature", `#/entity/${uuid(5)}`]]]], "each entity once, newest first, and none Kin could not name");
  assert.deepEqual(browser.main().querySelector("header").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Connect an agent", "/#/agents"]], "connecting another is always one step away");
  // Its change opens in the review, and an entity it touched opens its own page.
  const change = browser.links().find((node) => node.textContent.includes("Explain how want_bytes treats text"));
  assert.equal(change.getAttribute("href"), guided(AGENT_CHANGE));
  assert.ok(browser.links().some((node) => node.textContent === "want_bytes" && node.getAttribute("href") === `#/entity/${uuid(2)}`));
  assert.deepEqual(sideCount(browser, "agents"), ["1", "1 working now"]);
  assert.equal(sideRoute(browser, "agents").getAttribute("aria-current"), "page");
  assert.doesNotMatch(text, /session|mcp-agent|\/Users|\.py\b/i);
});

test("each agent is a lane: the ones here now first, each with what it is looking at and what it did last, and the earlier ones under them", async () => {
  const call = (seq, ago, tool, ids) => ({ seq, at: secondsAgo(ago), tool, entity_ids: ids.map(uuid), result_count: 1, duration_ms: 5, is_error: false });
  const base = agentsPayload().agents[0];
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  browser.data["/api/workspace/agents"] = { ...agentsPayload({
    agents: [base, { ...base, key: "00000000000000b2", name: "Lantern", state: "idle", calls_total: 2, changes: [], changes_total: 0, open_changes: [], recent_calls: [call(2, 30, "get_entity_source", [31, 30]), call(1, 90, "find_references", [30, 32])] },
      { ...base, key: "00000000000000b3", name: "Harbor Editor", state: "idle", calls_total: 0, changes: [], changes_total: 0, open_changes: [], recent_calls: [] }],
    earlier: [{ ...base, key: "00000000000000b4", name: "Lantern", state: "ended", calls_total: 3, changes: [], changes_total: 0, open_changes: [], recent_calls: [call(3, 720, "get_entity_source", [2])] }] }),
    names: { ...agentsPayload().names, [uuid(30)]: { label: "encode.<locals>.pad", kind: "function" }, [uuid(31)]: { label: "pad", kind: "function" }, [uuid(32)]: { label: "Signer.sign", kind: "method" } } };
  await flush(); await flush();
  const lanes = lanesOf(browser);
  assert.deepEqual(lanes.map((lane) => [lane.head[0], lane.head.at(-1), lane.state, lane.open, lane.href]), [
    ["Claude Code", "Active", "working", "true", `#/agents/${AGENT_KEY}`], ["Lantern", "Idle", "idle", "false", "#/agents/00000000000000b2"], ["Harbor Editor", "Idle", "idle", "false", "#/agents/00000000000000b3"],
    ["Lantern", lanes[3].head.at(-1), "ended", "false", "#/agents/00000000000000b4"]]);
  assert.match(lanes[1].head[1], /^since .*\d[:.]\d{2}.* · 2 calls$/, "a session says when it started and how many calls it made, and no changes it did not commit");
  // A lane that is not open says what the agent is looking at, each name once, and the last thing it did.
  assert.deepEqual(lanes[1].rows, [["Looking at", [["pad", `#/entity/${uuid(31)}`], ["Signer.sign", `#/entity/${uuid(32)}`]]], ["Last", "Read pad · just now"]],
    "a local inside a function is named for itself, two that read the same are one, and the first leads to its page");
  assert.deepEqual(lanes[2].said, ["Connected. It has not asked Kin for anything yet."]);
  // A session that is over carries no state, says when it was last heard from, and what it read.
  assert.equal(lanes[3].head.length, 2);
  assert.match(lanes[3].head[1], /3 calls · 12 minutes ago$/);
  assert.deepEqual(lanes[3].rows, [["Read", [["want_bytes", `#/entity/${uuid(2)}`]]]]);
  assert.deepEqual(lanes[3].said, []);
  assert.deepEqual(browser.node("agents-list").querySelectorAll("ul").map((list) => list.getAttribute("aria-label")).filter(Boolean), ["Connected now", "Earlier sessions"]);
  assert.deepEqual(browser.node("agents-list").querySelectorAll("h2").map((node) => node.textContent).filter((words) => words === "Earlier"), ["Earlier"]);
  // Opening another lane moves what the page holds open to it.
  browser.navigate("agents/00000000000000b4"); await flush(); await flush();
  assert.deepEqual(lanesOf(browser).map((lane) => lane.open), ["false", "false", "false", "true"]);
  assert.ok(lanesOf(browser)[3].node.contains(browser.node("agents-detail")));
  assert.deepEqual(lanesOf(browser)[3].rows, [["Read", [["want_bytes", `#/entity/${uuid(2)}`]]]]);
});

test("an agent that has gone is a lane with what it read, what it edited and the change it committed, kept for the window Kin says", async () => {
  const gone = "d".repeat(64);
  const base = agentsPayload().agents[0];
  const browser = fixture({ hash: "#/agents/00000000000000b5" });
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [], earlier: [
    { ...base, key: "00000000000000b4", state: "ended", started_at: secondsAgo(3 * 86_400), last_activity_at: secondsAgo(3 * 86_400 - 60), ended_at: secondsAgo(3 * 86_400 - 30),
      calls_total: 4, recent_calls: [], open_changes: [], reads_known: true, read: [uuid(5), uuid(2)], read_total: 4, edited: [uuid(2)], edited_total: 1 },
    { ...base, key: "00000000000000b5", name: "Codex", state: "ended", through: "other", started_at: secondsAgo(4 * 86_400), last_activity_at: secondsAgo(4 * 86_400),
      calls_total: 0, recent_calls: [], open_changes: [], reads_known: false, read: [], read_total: 0, edited: [uuid(3)], edited_total: 1, changes: [{ id: gone }], changes_total: 1 }] }),
    kept: { days: 30, sessions: 32 } };
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 0, earliest_kept_from_start: false,
    agent: { ...base, key: "00000000000000b5", name: "Codex", state: "ended", reads_known: false, edited_total: 1, changes: [{ id: gone }], changes_total: 1, recent_calls: null }, calls: [], names: {} };
  await flush(); await flush();
  assert.match(browser.text(), /No agent is connected right now\. Two agents worked here in the last 30 days\./);
  const [claude, codex] = lanesOf(browser);
  assert.match(claude.head[1], /^[A-Z][a-z]{2} \d+.* · 4 calls · 1 change committed · 3 days ago$/, "a session from another day says its day");
  assert.deepEqual(claude.rows, [
    ["Read", [["Signer.verify_signature", `#/entity/${uuid(5)}`], ["want_bytes", `#/entity/${uuid(2)}`]]],
    ["Edited", [["want_bytes", `#/entity/${uuid(2)}`]]]]);
  // What it committed leads the lane, with whether it waits for review, so the lane agrees with its head.
  assert.deepEqual(claude.said, ["Committed “Explain how want_bytes treats text”, waiting for your review · Review"]);
  assert.deepEqual(claude.node.querySelector("p.lane-outcome").querySelectorAll("a").map((node) => [node.textContent, node.getAttribute("href")]), [["Review", guided(AGENT_CHANGE)]]);
  assert.match(claude.node.textContent, /and 2 more/, "what it read past the names shown is counted");
  // One Kin knows only by what it wrote says what it read was not kept; open, it says so of its calls too.
  assert.doesNotMatch(codex.head[1], /call/);
  assert.equal(codex.open, "true");
  assert.deepEqual(codex.said, ["Kin kept what it wrote, not what it read."]);
  assert.deepEqual(codex.rows, [["Edited", [["Signer", `#/entity/${uuid(3)}`]]]], "an open lane leaves its changes to what it did");
  const detail = browser.node("agents-detail").textContent;
  assert.match(detail, /^Committed a change, waiting for your review\. 1 entity written, 1 change committed\./, "what the session came to leads its summary");
  assert.match(detail, /Kin kept the changes this agent wrote, not the calls it made\./);
  assert.match(detail, /A change it committed/);
  assert.match(browser.text(), /Kin keeps what each agent did here for 30 days after it leaves, the latest 32 at most\./);
  // Closed, its change still opens for review.
  browser.navigate("agents/00000000000000b4"); await flush(); await flush();
  assert.deepEqual(lanesOf(browser)[1].rows, [["Edited", [["Signer", `#/entity/${uuid(3)}`]]]]);
  assert.deepEqual(lanesOf(browser)[1].said, ["Committed a change, waiting for your review · Review", "Kin kept what it wrote, not what it read."]);
  assert.equal(lanesOf(browser)[1].node.querySelector("p.lane-outcome").querySelector("a").getAttribute("href"), guided(gone));
  assert.doesNotMatch(browser.text(), /session|mcp-agent|\/Users|\.py\b|since this page/i);
});

test("a local inside a function is named for itself wherever an agent's call is told on the Agents page", async () => {
  const call = (seq, ago, tool, ids) => ({ seq, at: secondsAgo(ago), tool, entity_ids: ids.map(uuid), result_count: 1, duration_ms: 5, is_error: false });
  const lantern = { ...agentsPayload().agents[0], key: "00000000000000b2", name: "Lantern", recent_calls: [call(2, 30, "get_entity_source", [30])] };
  const browser = fixture({ hash: "#/agents" });
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [agentsPayload().agents[0], lantern] }), names: { [uuid(30)]: { label: "encode.<locals>.pad", kind: "function" } } };
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 1, earliest_kept_from_start: true, agent: { ...lantern, recent_calls: null }, calls: [call(2, 30, "get_entity_source", [30])], names: { [uuid(30)]: { label: "encode.<locals>.pad", kind: "function" } } };
  await flush(); await flush();
  assert.deepEqual(lanesOf(browser)[1].rows, [["Looking at", [["pad", `#/entity/${uuid(30)}`]]], ["Last", "Read pad · just now"]]);
  const toPad = (host) => host.querySelectorAll("a").filter((node) => node.getAttribute("href") === `#/entity/${uuid(30)}`).map((node) => [node.textContent, node.getAttribute("title")]);
  assert.deepEqual(toPad(lanesOf(browser)[1].node), [["pad", "encode.<locals>.pad"], ["pad", "encode.<locals>.pad"]], "the full name is kept where a pointer finds it");
  browser.navigate("agents/00000000000000b2"); await flush(); await flush();
  assert.deepEqual(toPad(browser.node("agents-detail")), [["pad", "encode.<locals>.pad"]], "and what it did says the same");
});

test("a local inside a function is named for itself on Home's agent cards too", async () => {
  const hub = fixture({ hub: true });
  hub.data[under(KEY, "/api/workspace/agents")] = { ...agentsPayload({ agents: [{ ...agentsPayload().agents[0], recent_calls: [
    { seq: 2, at: secondsAgo(30), tool: "get_entity_source", entity_ids: [uuid(30)], result_count: 1, duration_ms: 5, is_error: false },
    { seq: 1, at: secondsAgo(60), tool: "find_references", entity_ids: [uuid(30)], result_count: 1, duration_ms: 5, is_error: false }] }] }), names: { [uuid(30)]: { label: "encode.<locals>.pad", kind: "function" } } };
  await flush();
  assert.equal(agentCards(hub)[0].action, "Read pad");
  assert.deepEqual(agentCards(hub)[0].before, ["Checked what uses pad"]);
});

// A session as the evaluation met it: it read, started a change, committed it
// outside the calls Kin kept, then failed to drop a change after 27.9 seconds.
const REDIRECT_CHANGE = "e".repeat(64);
const redirectCalls = () => [
  { seq: 3, at: secondsAgo(900), tool: "kin_transaction_abort", entity_ids: [], result_count: 0, duration_ms: 27_900, is_error: true },
  { seq: 2, at: secondsAgo(1400), tool: "kin_transaction_begin", entity_ids: [], result_count: 0, duration_ms: 20, is_error: false },
  { seq: 1, at: secondsAgo(1401), tool: "get_entity_source", entity_ids: [uuid(40), uuid(41)], result_count: 2, duration_ms: 9, is_error: false }];
const redirectNames = { [uuid(40)]: { label: "Redirect::status_code", kind: "method" }, [uuid(41)]: { label: "Redirect", kind: "struct" } };
const redirectSession = (fields = {}) => ({ ...agentsPayload().agents[0], state: "ended", started_at: secondsAgo(1500), last_activity_at: secondsAgo(900), ended_at: secondsAgo(890),
  calls_total: 3, recent_calls: redirectCalls(), open_changes: [], intents: [], reads_known: true, read: [uuid(40), uuid(41)], read_total: 2, edited: [uuid(40)], edited_total: 1,
  changes: [{ id: REDIRECT_CHANGE, title: "Say what status a redirect answers with", time: secondsAgo(1260), entities_changed: 1 }], changes_total: 1, ...fields });

test("a session's outcome says what it committed and how its review stands, then what it has open, what it edited, or its last call", () => {
  const A = fixture().agents;
  const names = { ...redirectNames, [uuid(2)]: { label: "want_bytes" } };
  const say = (agent, review) => { const parts = A.outcome(agent, names, review); return parts && A.plainText(parts); };
  const session = redirectSession();
  assert.equal(say(session, () => "pending"), "Committed “Say what status a redirect answers with”, waiting for your review");
  assert.equal(say(session, () => "approved"), "Committed “Say what status a redirect answers with”, and you marked it as looking right");
  assert.equal(say(session, () => "needs_work"), "Committed “Say what status a redirect answers with”, and you flagged it");
  assert.equal(say(session, () => null), "Committed “Say what status a redirect answers with”", "a review Kin has not read is not guessed");
  assert.equal(say(session), "Committed “Say what status a redirect answers with”", "it needs nothing of the page to say what a session came to");
  assert.equal(say({ ...session, changes_total: 3 }, () => "pending"), "Committed “Say what status a redirect answers with” and 2 others, the latest waiting for your review");
  assert.equal(say({ ...session, changes: [{ id: REDIRECT_CHANGE }] }, () => "pending"), "Committed a change, waiting for your review");
  assert.equal(say({ ...session, changes: [{ id: REDIRECT_CHANGE }], changes_total: 3 }, () => "approved"), "Committed 3 changes, and you marked the latest as looking right");
  const asked = [];
  A.outcome(session, names, (id) => { asked.push(id); return "pending"; });
  assert.deepEqual(asked, [REDIRECT_CHANGE], "the review asked about is the change it committed last");
  // With nothing committed: what it has open, then what it edited, then its last call.
  const working = { ...session, state: "active", changes: [], changes_total: 0 };
  assert.equal(say({ ...working, open_changes: [{ staged_edits: 2, age_secs: 30 }] }), "Has 2 edits staged, not committed yet");
  assert.equal(say({ ...working, open_changes: [{ staged_edits: 0 }] }), "Started a change, nothing staged yet");
  assert.deepEqual(JSON.parse(JSON.stringify(A.outcome({ ...working, open_changes: [] }, names))), ["Edited ", { id: uuid(40), label: "Redirect::status_code" }], "an entity it edited is a part the page links");
  assert.equal(say({ ...working, edited: [uuid(40), uuid(2)], edited_total: 2 }), "Edited Redirect::status_code and want_bytes");
  assert.equal(say({ ...working, edited: [uuid(40)], edited_total: 4 }), "Edited Redirect::status_code and 3 other entities");
  assert.equal(say({ ...working, edited: [uuid(99)], edited_total: 1 }), "Edited 1 entity");
  assert.equal(say({ ...working, edited: [], edited_total: 0 }), "Tried to drop the change it had started; Kin did not answer within 28 s");
  assert.equal(say({ ...working, edited: [], edited_total: 0, recent_calls: redirectCalls().slice(2) }), "Read Redirect::status_code");
  // A session with nothing Kin kept, and records missing their fields, say nothing rather than something wrong.
  assert.equal(A.outcome({ name: "Lantern", state: "idle" }, names), null);
  assert.equal(A.outcome(null), null);
  assert.equal(say({ recent_calls: [{}] }), "Asked Kin a question");
  assert.equal(say({ changes: [{ title: "No id" }], changes_total: "many", open_changes: [null], edited: ["not-an-id"], recent_calls: [] }), null);
});

test("a failed call reads as what it tried and why, and the timeline says whether its change is still open", async () => {
  const A = fixture().agents;
  const names = { [uuid(5)]: { label: "Signer.verify_signature" }, [uuid(2)]: { label: "want_bytes" } };
  const failed = (tool, ids = [], duration_ms = 40) => A.plainText(A.describe({ tool, entity_ids: ids.map(uuid), result_count: 0, duration_ms, is_error: true }, names));
  assert.equal(failed("kin_transaction_abort", [], 27_900), "Tried to drop the change it had started; Kin did not answer within 28 s");
  assert.equal(failed("kin_transaction_abort"), "Tried to drop the change it had started; Kin could not do it");
  assert.equal(failed("kin_transaction_begin"), "Tried to start a change; Kin could not do it");
  assert.equal(failed("kin_transaction_stage", [2]), "Tried to stage an edit to want_bytes; Kin could not do it");
  assert.equal(failed("kin_transaction_commit", [2, 5], 12_400), "Tried to commit a change to want_bytes; Kin did not answer within 12 s");
  assert.equal(failed("kin_mutate", [2]), "Tried to change want_bytes; Kin could not do it");
  assert.equal(failed("mutate"), "Tried to change the code; Kin could not do it");
  assert.equal(failed("get_entity_source", [2]), "Tried to read want_bytes; Kin could not do it");
  assert.equal(failed("semantic_locate", [5]), "Tried to look up Signer.verify_signature; Kin could not do it");
  assert.equal(failed("semantic_locate"), "Tried to look something up; Kin could not do it");
  assert.equal(failed("find_references", [5]), "Tried to check what uses Signer.verify_signature; Kin could not do it");
  assert.equal(failed("impact_analysis", [2]), "Tried to check what a change to want_bytes would touch; Kin could not do it");
  assert.equal(failed("get_context_pack", [2]), "Tried to read the context around want_bytes; Kin could not do it");
  assert.equal(failed("graph_neighborhood", [2]), "Tried to look at what want_bytes connects to; Kin could not do it");
  assert.equal(failed("trace_path", [5, 2]), "Tried to trace how Signer.verify_signature reaches want_bytes; Kin could not do it");
  assert.equal(failed("kin_provenance_query", [2]), "Tried to read the history of want_bytes; Kin could not do it");
  assert.equal(failed("kin_graph_status"), "Tried to check the repository's status; Kin could not do it");
  assert.equal(failed("unrecognized_tool", [2]), "Tried to ask Kin about want_bytes; Kin could not do it");
  assert.equal(A.plainText(A.describe({ tool: "kin_transaction_abort", is_error: true })), "Tried to drop the change it had started; Kin could not do it", "a call without its timing says only that Kin could not do it");

  // The session the evaluation opened: its summary leads with the change waiting for review, and the failed drop says what it tried.
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  const session = redirectSession();
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [], earlier: [session] }), names: redirectNames, kept: { days: 30, sessions: 32 } };
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 3, earliest_kept_from_start: true, agent: { ...session, recent_calls: null }, calls: redirectCalls(), names: redirectNames };
  await flush(); await flush(); await flush();
  const detail = browser.node("agents-detail");
  assert.match(detail.textContent, /^Committed “Say what status a redirect answers with”, waiting for your review\. Three calls to Kin, 2 entities looked at, 1 change committed\./);
  const steps = detail.querySelectorAll("li.agent-step");
  assert.deepEqual(steps.map((step) => step.querySelector("p").textContent), ["Tried to drop the change it had started; Kin did not answer within 28 s", "Started a change", "Read Redirect::status_code"]);
  assert.ok(steps[0].className.split(" ").includes("agent-step-error"), "a failed call keeps its error colour");
  // The change it started was committed before the drop was tried, so that is what is said under it.
  assert.equal(steps[0].querySelector("p.agent-step-open").textContent, `It had already committed the change it started, “Say what status a redirect answers with”, at ${new Date(Date.parse(secondsAgo(1260))).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" })}.`);
  assert.equal(detail.querySelectorAll("p.agent-step-open").length, 1);
  assert.doesNotMatch(detail.textContent, /could not answer|Dropped a change/);
  assert.deepEqual(faults(shownWords(browser, "Agents, a failed drop"), NEVER_SAID), []);
  // With no change committed since it started one, a session that has ended no longer reports what it holds.
  const uncommitted = fixture({ hash: `#/agents/${AGENT_KEY}` });
  const left = redirectSession({ changes: [], changes_total: 0 });
  uncommitted.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [], earlier: [left] }), names: redirectNames, kept: { days: 30, sessions: 32 } };
  uncommitted.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 3, earliest_kept_from_start: true, agent: { ...left, recent_calls: null }, calls: redirectCalls(), names: redirectNames };
  await flush(); await flush(); await flush();
  assert.equal(uncommitted.node("agents-detail").querySelector("p.agent-step-open").textContent, "Kin cannot say now whether that change is still open.");

  // A connected agent says what it holds now, and only under the newest failed attempt at a change.
  const timelineOf = (agent, calls) => ({ schema: "kin.workspace.agent-timeline.v1", listed: true, kept: calls.length, earliest_kept_from_start: true, agent: { ...agent, recent_calls: null }, calls, names: redirectNames });
  const openLines = async (open_changes) => {
    const live = fixture({ hash: `#/agents/${AGENT_KEY}` });
    const agent = { ...redirectSession(), state: "idle", open_changes, changes: [], changes_total: 0 };
    const calls = [redirectCalls()[0], { seq: 2, at: secondsAgo(1000), tool: "kin_transaction_stage", entity_ids: [uuid(40)], duration_ms: 50, is_error: true }, ...redirectCalls().slice(1)];
    live.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [agent] }), names: redirectNames };
    live.data["/api/workspace/agents/timeline"] = timelineOf(agent, calls);
    await flush(); await flush(); await flush();
    const rows = live.node("agents-detail").querySelectorAll("li.agent-step");
    assert.equal(rows[1].querySelector("p").textContent, "Tried to stage an edit to Redirect::status_code; Kin could not do it");
    return rows.map((row) => row.querySelector("p.agent-step-open")?.textContent ?? null);
  };
  assert.deepEqual(await openLines([{ staged_edits: 2, age_secs: 40 }]), ["Its change is still open, with 2 edits staged.", null, null, null]);
  assert.deepEqual(await openLines([{ staged_edits: 0, age_secs: 40 }]), ["Its change is still open, with nothing staged.", null, null, null]);
  assert.deepEqual(await openLines([]), ["No change of its is open now.", null, null, null]);
  assert.deepEqual(await openLines(undefined), [null, null, null, null], "when Kin does not say what it holds, nothing is claimed");
});

test("a closed lane says first what its session came to, and agrees with its head", async () => {
  const browser = fixture({ hash: "#/agents/00000000000000b2" });
  const lantern = { ...agentsPayload().agents[0], key: "00000000000000b2", name: "Lantern", state: "idle", calls_total: 1, changes: [], changes_total: 0, open_changes: [], recent_calls: [redirectCalls()[0]] };
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [agentsPayload().agents[0], lantern], earlier: [redirectSession({ key: "00000000000000b4" })] }), names: { ...agentsPayload().names, ...redirectNames } };
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 1, earliest_kept_from_start: true, agent: { ...lantern, recent_calls: null }, calls: lantern.recent_calls, names: redirectNames };
  await flush(); await flush(); await flush();
  const [claude, open, gone] = lanesOf(browser);
  assert.match(claude.head[1], / · 1 change committed$/);
  assert.deepEqual(claude.said, ["Committed “Explain how want_bytes treats text”, waiting for your review · Review"], "the change its head counts leads the lane");
  assert.equal(claude.node.querySelector("p.agent-open").textContent, "Has 1 edit staged, not committed yet · open for a few seconds", "what it has open since says so in the outcome's words");
  assert.equal(claude.rows.at(-1)[0], "Last");
  assert.equal(open.open, "true");
  assert.match(browser.node("agents-detail").textContent, /^Tried to drop the change it had started; Kin did not answer within 28 s\. One call to Kin/, "with nothing committed, its last call is what it came to");
  assert.deepEqual(gone.said, ["Committed “Say what status a redirect answers with”, waiting for your review · Review"]);
  assert.deepEqual(gone.rows.map(([label]) => label), ["Read", "Edited"]);
  assert.match(gone.head[1], /3 calls · 1 change committed/);
  // A decision on the change reaches the lanes as soon as review decisions are read again, with nothing else changed.
  browser.data["/api/standing"] = { standing: { [AGENT_CHANGE]: { state: "approved" }, [REDIRECT_CHANGE]: { state: "needswork" } } };
  const agentsReads = browser.count("/api/workspace/agents");
  browser.refresh(); await flush(); await flush();
  assert.equal(browser.count("/api/workspace/agents"), agentsReads + 1);
  assert.deepEqual([lanesOf(browser)[0].said, lanesOf(browser)[2].said], [
    ["Committed “Explain how want_bytes treats text”, and you marked it as looking right · Review"],
    ["Committed “Say what status a redirect answers with”, and you flagged it · Review"]]);
  // Without review decisions, a commit is said without its review.
  const unread = fixture({ hash: "#/agents/00000000000000b2" });
  unread.failures.set("/api/standing", "Review decisions unavailable");
  unread.data["/api/workspace/agents"] = browser.data["/api/workspace/agents"];
  unread.data["/api/workspace/agents/timeline"] = browser.data["/api/workspace/agents/timeline"];
  await flush(); await flush(); await flush();
  assert.equal(lanesOf(unread)[0].said[0], "Committed “Explain how want_bytes treats text” · Review");
});

test("a commit call that names its change leads to its review, and calls with fields missing still read plainly", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  const agent = agentsPayload().agents[0];
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 5, earliest_kept_from_start: true, agent: { ...agent, recent_calls: null }, names: agentsPayload().names,
    calls: [
      { seq: 5, at: secondsAgo(5), tool: "kin_transaction_commit", entity_ids: [uuid(2), uuid(3)], result_count: 2, duration_ms: 900, is_error: false, change_id: AGENT_CHANGE },
      { seq: 4, at: secondsAgo(6), tool: "kin_transaction_commit", entity_ids: [uuid(2)], result_count: 1, duration_ms: 900, is_error: false, change_id: "f".repeat(64) },
      { seq: 3, at: secondsAgo(7), tool: "kin_transaction_stage", entity_ids: [uuid(2)], is_error: false },
      { seq: 2 },
      { seq: 1, tool: "kin_mutate", entity_ids: "not a list", duration_ms: "slow", is_error: true }] };
  await flush(); await flush(); await flush();
  const steps = browser.node("agents-detail").querySelectorAll("li.agent-step");
  assert.deepEqual(steps.map((step) => step.querySelector("p").textContent), ["Committed a change to want_bytes", "Committed a change to want_bytes", "Staged an edit to want_bytes", "Asked Kin a question", "Tried to change the code; Kin could not do it"]);
  assert.deepEqual(steps[0].querySelectorAll("a.more-link").map((node) => [node.textContent, node.getAttribute("href")]), [["Review “Explain how want_bytes treats text”", guided(AGENT_CHANGE)]]);
  assert.deepEqual(steps[1].querySelectorAll("a.more-link"), [], "a change that is not one of its own is not linked");
  assert.equal(steps[4].querySelector("p.agent-step-open").textContent, "Its change is still open, with 1 edit staged.");
  assert.deepEqual(faults(shownWords(browser, "Agents, calls with fields missing"), NEVER_SAID), []);
});

test("an edit or commit that finished after its agent stopped waiting says so, and a mutate that committed leads to its review", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  const agent = agentsPayload().agents[0];
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 3, earliest_kept_from_start: true, agent: { ...agent, recent_calls: null }, names: agentsPayload().names,
    calls: [
      { seq: 3, at: secondsAgo(5), tool: "kin_mutate", entity_ids: [uuid(2)], result_count: 1, duration_ms: 41_000, is_error: false, change_id: AGENT_CHANGE, caller_left: true },
      { seq: 2, at: secondsAgo(60), tool: "kin_transaction_commit", entity_ids: [uuid(2)], result_count: 1, duration_ms: 31_000, is_error: true, caller_left: true },
      { seq: 1, at: secondsAgo(90), tool: "kin_transaction_begin", entity_ids: [], result_count: 0, duration_ms: 20, is_error: false }] };
  await flush(); await flush(); await flush();
  const steps = browser.node("agents-detail").querySelectorAll("li.agent-step");
  assert.deepEqual(steps.map((step) => step.querySelector("p").textContent), [
    "Changed want_bytes, after it had stopped waiting for the answer, so it was not told",
    "Tried to commit a change to want_bytes; Kin did not answer within 31 s",
    "Started a change"]);
  assert.deepEqual(steps[0].querySelectorAll("a.more-link").map((node) => [node.textContent, node.getAttribute("href")]), [["Review “Explain how want_bytes treats text”", guided(AGENT_CHANGE)]]);
});

test("a drop that failed after the change it started was committed says the change was committed", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  const agent = { ...agentsPayload().agents[0], state: "ended", open_changes: [], changes: [{ id: AGENT_CHANGE, title: "Say what status a redirect answers with", time: secondsAgo(500), entities_changed: 1 }], changes_total: 1 };
  browser.data["/api/workspace/agents"] = agentsPayload({ agents: [], earlier: [{ ...agent, key: AGENT_KEY }] });
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 3, earliest_kept_from_start: true, agent: { ...agent, recent_calls: null }, names: agentsPayload().names,
    calls: [
      { seq: 3, at: secondsAgo(30), tool: "kin_transaction_abort", entity_ids: [], result_count: 0, duration_ms: 27_900, is_error: true },
      { seq: 2, at: secondsAgo(560), tool: "kin_transaction_begin", entity_ids: [], result_count: 0, duration_ms: 20, is_error: false },
      { seq: 1, at: secondsAgo(561), tool: "get_entity_source", entity_ids: [uuid(5)], result_count: 1, duration_ms: 8, is_error: false }] };
  await flush(); await flush(); await flush();
  const steps = browser.node("agents-detail").querySelectorAll("li.agent-step");
  assert.equal(steps[0].querySelector("p").textContent, "Tried to drop the change it had started; Kin did not answer within 28 s");
  assert.equal(steps[0].querySelector("p.agent-step-open").textContent, `It had already committed the change it started, “Say what status a redirect answers with”, at ${new Date(Date.parse(secondsAgo(500))).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" })}.`);
});

test("the Agents lede counts agents and the sessions they ran, not sessions as agents", async () => {
  const A = fixture().agents;
  const session = (name, n, state = "ended") => ({ ...agentsPayload().agents[0], key: `00000000000000${String(n).padStart(2, "0")}`, name, state });
  const kept = { days: 30, sessions: 32 };
  const lede = (agents, earlier, window = kept) => A.headline({ agents, earlier, kept: window });
  assert.equal(lede([], [session("Claude Code", 1), session("Claude Code", 2), session("claude code", 3)]), "No agent is connected right now. Claude Code worked here in three sessions in the last 30 days.");
  assert.equal(lede([], [1, 2, 3].map((n) => session("Claude Code", n)).concat([4, 5].map((n) => session("Codex", n)))), "No agent is connected right now. Two agents worked here in five sessions in the last 30 days.");
  assert.equal(lede([], [session("Claude Code", 1), session("Codex", 2)]), "No agent is connected right now. Two agents worked here in the last 30 days.");
  assert.equal(lede([], [session("Claude Code", 1)], null), "No agent is connected right now. Claude Code worked here earlier.");
  assert.equal(lede([], Array.from({ length: 12 }, (_, n) => session("Claude Code", n))), "No agent is connected right now. Claude Code worked here in at least 12 sessions in the last 30 days.", "a full list is the most Kin sends, so its count is a lower bound");
  assert.equal(lede([session("Claude Code", 1, "active"), session("Claude Code", 2, "idle")], []), "Claude Code is connected in two sessions. One is working now.");
  assert.equal(lede([session("Claude Code", 1, "active"), session("Claude Code", 2, "idle"), session("Codex", 3, "idle")], []), "Two agents are connected in three sessions. One is working now.");
  assert.equal(lede([session("Claude Code", 1, "active"), session("Codex", 2, "active")], []), "Two agents are connected. Both are working.");
  // On the page, as the evaluation met it.
  const browser = fixture({ hash: "#/agents" });
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [], earlier: [1, 2, 3].map((n) => redirectSession({ key: `00000000000000c${n}` })) }), names: redirectNames, kept };
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 3, earliest_kept_from_start: true, agent: { ...redirectSession(), recent_calls: null }, calls: redirectCalls(), names: redirectNames };
  await flush(); await flush(); await flush();
  assert.match(browser.node("agents-lede").textContent, /^No agent is connected right now\. Claude Code worked here in three sessions in the last 30 days\./);
});

test("History says how far Kin looked when it found no test that reaches a change", async () => {
  const browser = fixture({ hash: "#/history" });
  const left = { "change-1": { recorded_consumers: 3, consumers_not_updated: 2, recorded_tests: 0, tests_depth: 3 }, "change-2": { recorded_consumers: 1, consumers_may_need_review: 1, recorded_tests: 0 } };
  browser.data["/api/guide?change"] = (url) => guide({ modified: 1, ...left[url.searchParams.get("change")] }, { change: { id: url.searchParams.get("change") } });
  await flush(); await browser.advance(0); await browser.advance(0); await browser.advance(0);
  assert.deepEqual([consequenceLines(browser)["Preserved change"], consequenceLines(browser)["Merge branch"]], [
    ["2 entities that rely on it were not updated. No test found within 3 steps of it.", "bad"],
    ["1 entity relies on it and did not change. Kin found no tests that reach it.", "warn"]], "the depth Kin looked to is said when its guide gives it");
});

test("an agent's calls are read with patience, and a read that only took too long is asked again while the last answer stays", async () => {
  const path = "/api/workspace/agents/timeline";
  // A first read that takes more than a minute is waited for, then asked again on its own.
  const slow = fixture({ hash: `#/agents/${AGENT_KEY}` });
  slow.held.set(path, new Promise(() => {}));
  await flush(); await flush();
  await slow.advance(90_000);
  assert.equal(slow.count(path), 1);
  assert.doesNotMatch(slow.node("agents-detail").textContent, /couldn't load|still reading/, "a read is given more than the page's usual minute");
  await slow.advance(91_000);
  assert.match(slow.node("agents-detail").textContent, /^Kin is still reading what this agent did\./);
  assert.equal(slow.node("agents-detail").querySelector("div.agent-detail").getAttribute("aria-busy"), "true", "it says so beside the outline of what is coming");
  slow.held.delete(path);
  await slow.advance(3_000);
  assert.equal(slow.count(path), 2);
  assert.match(slow.node("agents-detail").textContent, /Committed a change to want_bytes/);
  assert.doesNotMatch(slow.node("agents-detail").textContent, /still reading/);

  // A read again that takes too long keeps what was shown, says Kin is still reading, and asks again further apart.
  FakeEventSource.opened.length = 0;
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}`, live: true });
  await flush(); await flush(); await flush();
  const first = browser.count(path);
  browser.failures.set(path, "Kin did not answer within 20 seconds.");
  FakeEventSource.opened.at(-1).emit("agent", { agent: AGENT_KEY });
  await browser.advance(700);
  assert.equal(browser.count(path), first + 1);
  const detail = () => browser.node("agents-detail").textContent;
  assert.match(detail(), /Kin is still reading what this agent did\./);
  assert.match(detail(), /Committed a change to want_bytes/, "the last answer stays while Kin reads again");
  assert.doesNotMatch(detail(), /couldn't load|Showing what Kin last saw/);
  await browser.advance(2_900);
  assert.equal(browser.count(path), first + 1, "it waits before asking again");
  await browser.advance(100);
  assert.equal(browser.count(path), first + 2);
  await browser.advance(8_000);
  assert.equal(browser.count(path), first + 3);
  // Once Kin answers, the newer answer replaces the old one.
  browser.failures.delete(path);
  const answer = browser.data[path]();
  browser.data[path] = { ...answer, calls: [{ seq: 8, at: secondsAgo(1), tool: "kin_transaction_abort", entity_ids: [], duration_ms: 30, is_error: false }, ...answer.calls] };
  await browser.advance(20_000);
  assert.equal(browser.count(path), first + 4);
  assert.match(detail(), /Dropped a change it had started/);
  assert.doesNotMatch(detail(), /still reading/);

  // Kin that stays too slow is said to have failed, with the last answer kept; a read that failed for another reason is not asked again.
  browser.failures.set(path, "This read took longer than 3 minutes.");
  FakeEventSource.opened.at(-1).emit("agent", { agent: AGENT_KEY });
  await browser.advance(700 + 3_000 + 8_000 + 20_000);
  assert.equal(browser.count(path), first + 8);
  assert.match(detail(), /Showing what Kin last saw\./);
  assert.match(detail(), /Dropped a change it had started/);
  await browser.advance(60_000);
  assert.equal(browser.count(path), first + 8, "it stops asking after a few tries");
  const broken = fixture({ hash: `#/agents/${AGENT_KEY}` });
  broken.failures.set(path, "Daemon unavailable");
  await flush(); await flush(); await broken.advance(30_000);
  assert.equal(broken.count(path), 1);
  assert.match(broken.node("agents-detail").textContent, /Kin couldn't load what this agent did\./);

  // Moving to another agent stops asking about the one left.
  const moved = fixture({ hash: `#/agents/${AGENT_KEY}` });
  moved.failures.set(path, "Kin did not answer within 20 seconds.");
  await flush(); await flush();
  const asked = () => moved.requests.filter((request) => request.name === path && request.url.includes(`agent=${AGENT_KEY}`)).length;
  assert.equal(asked(), 1);
  moved.navigate("history"); await flush();
  await moved.advance(60_000);
  assert.equal(asked(), 1, "nobody is looking at the agent, so its calls are not read again");
});

test("the Agents page says first where an agent's work meets uncommitted work", async () => {
  const call = (seq, ago, tool, ids) => ({ seq, at: secondsAgo(ago), tool, entity_ids: ids.map(uuid), result_count: 1, duration_ms: 5, is_error: false });
  const browser = fixture({ hash: "#/agents" });
  browser.data["/api/guide"] = working({ modified: 1 }, [changed(2, "want_bytes")], { edited: 60 });
  browser.data["/api/workspace/agents"] = agentsPayload({ agents: [{ ...agentsPayload().agents[0], recent_calls: [call(9, 10, "kin_transaction_stage", [2])] }] });
  await flush(); await flush();
  assert.deepEqual(browser.node("agents-meets").querySelectorAll("li").map((row) => row.querySelector("p").textContent), ["Claude Code changed want_bytes, which your uncommitted work also changed."]);
  assert.ok(browser.node("agents-meets").contains(browser.main().querySelectorAll("li").find((row) => row.textContent.includes("which your uncommitted work also changed"))));
  // With nothing uncommitted, nothing meets it.
  browser.data["/api/guide"] = guide();
  browser.refresh(); await flush();
  assert.equal(browser.node("agents-meets").textContent, "");
});

test("with no agent connected, the Agents page says how agents connect and names no command to run", async () => {
  const browser = fixture({ hash: "#/agents" });
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [] }), kept: { days: 30, sessions: 32 } };
  await flush(); await flush();
  assert.match(browser.text(), /No agent is connected right now\./);
  assert.match(browser.text(), /No agent has worked here in the last 30 days\./, "the window Kin keeps, not the life of this page");
  assert.match(browser.text(), /Kin keeps what each agent did here for 30 days after it leaves, the latest 32 at most\./);
  assert.match(browser.text(), /work here through Kin once they are connected\. Connect an agent\./);
  assert.equal(browser.links().find((node) => node.textContent === "Connect an agent").getAttribute("href"), "/#/agents", "connecting is done at the hub");
  assert.doesNotMatch(browser.text(), /kin setup|terminal/i);
  assert.equal(sideCount(browser, "agents"), null);
  assert.equal(browser.count("/api/workspace/agents/timeline"), 0, "there is no agent to read the calls of");

  browser.data["/api/workspace/agents"] = agentsPayload({ agents: [], earlier: [{ ...agentsPayload().agents[0], state: "ended", last_activity_at: secondsAgo(720), open_changes: [] }] });
  browser.data["/api/workspace/agents/timeline"] = { schema: "kin.workspace.agent-timeline.v1", listed: true, kept: 0, earliest_kept_from_start: true, agent: { ...agentsPayload().agents[0], state: "ended", last_activity_at: secondsAgo(720), open_changes: [], recent_calls: null }, calls: [], names: {} };
  browser.refresh(); await flush(); await flush();
  assert.match(browser.text(), /No agent is connected right now\. Claude Code worked here earlier\./);
  assert.match(browser.text(), /Last active 12 minutes ago/);
  const ended = browser.links().find((node) => node.getAttribute("href") === `#/agents/${AGENT_KEY}`);
  assert.match(ended.textContent, /^Claude Code\d{1,2}[:.]\d{2}/, "a session that is over says when it started");
  assert.doesNotMatch(ended.textContent, /Disconnected|Active|Idle/, "a session that is over carries no state in the list");
  assert.deepEqual(lanesOf(browser)[0].rows.map(([label]) => label), ["Read"], "what it read; open, its changes are in what it did");

  // A Kin that keeps nothing past a session's end, and one whose record could not be read, say so.
  browser.data["/api/workspace/agents"] = agentsPayload({ agents: [] });
  browser.refresh(); await flush(); await flush();
  assert.match(browser.text(), /No agent has worked here that Kin has a record of\./);
  browser.data["/api/workspace/agents"] = { ...agentsPayload({ agents: [] }), earlier_unread: true };
  browser.refresh(); await flush(); await flush();
  assert.match(browser.text(), /Kin couldn't read which agents worked here before\./);
  assert.doesNotMatch(browser.text(), /No agent has worked here/);
});

test("the agents read fails on its own and the rest of the page keeps working", async () => {
  const browser = fixture({ hash: "#/agents" });
  browser.failures.set("/api/workspace/agents", "Daemon unavailable");
  await flush(); await flush();
  assert.match(browser.text(), /Kin couldn't load which agents are connected\./);
  assert.equal(browser.node("workspace-status").dataset.state, "stale");
  assert.deepEqual(sideCount(browser, "uncommitted"), ["1", "1 uncommitted"], "the other reads landed");
  browser.navigate("uncommitted");
  assert.match(browser.node("work-lede").textContent, /One function changed/);
  browser.navigate("agents");
  browser.failures.delete("/api/workspace/agents");
  browser.button("Try again").click(); await flush(); await flush();
  assert.match(browser.text(), /One agent is connected and working\./);
  assert.equal(browser.node("workspace-status").dataset.state, "ready");
});

test("a server that does not report agents says so and retries only on explicit refresh without a live feed", async () => {
  FakeEventSource.opened.length = 0;
  const browser = fixture({ hash: "#/agents", live: true });
  delete browser.data["/api/workspace/agents"];
  await flush(); await flush();
  assert.match(browser.text(), /This version of Kin does not report agent activity\./);
  assert.equal(browser.node("workspace-status").dataset.state, "ready", "a read the server does not have is not a failed read");
  browser.refresh(); await flush();
  assert.equal(browser.count("/api/workspace/agents"), 2);
  assert.equal(FakeEventSource.opened.length, 0);
  browser.navigate("uncommitted");
  assert.equal(browser.node("work-overlaps").textContent, "");
  assert.match(browser.node("work-lede").textContent, /One function changed/);
});

test("an agent whose record Kin no longer holds says so instead of failing", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  browser.data["/api/workspace/agents/timeline"] = { __status: 404, error: "gone" };
  await flush(); await flush(); await flush();
  assert.match(browser.text(), /Kin no longer holds this agent's record\./);
});

test("the page follows Kin live: an agent's call reads the agents again and a commit is announced", async () => {
  FakeEventSource.opened.length = 0;
  const browser = fixture({ live: true, hash: "#/agents" }); await flush(); await flush(); await flush();
  const source = FakeEventSource.opened.at(-1);
  assert.equal(source.url, `${BASE}/api/workspace/events`);
  source.emit("ready");
  await browser.advance(600);
  assert.match(browser.text(), /Updates live/);
  const before = browser.count("/api/workspace/agents");
  source.emit("agent", { agent: AGENT_KEY, tool: "find_references" });
  source.emit("agent", { agent: AGENT_KEY, tool: "impact_analysis" });
  await browser.advance(600);
  assert.equal(browser.count("/api/workspace/agents"), before + 1, "pulses close together are read once");
  const changes = browser.count("/api/changes");
  const work = browser.count("/api/guide");
  source.emit("change", { agent: AGENT_KEY, change: AGENT_CHANGE });
  assert.equal(browser.node("workspace-announcer").textContent, "Claude Code committed a change.");
  await browser.advance(600);
  assert.equal(browser.count("/api/changes"), changes + 1);
  assert.ok(browser.count("/api/guide") > work, "what is uncommitted is read again after a commit");
  source.onerror();
  assert.ok(source.closed);
  assert.match(browser.text(), /Reconnecting/);
  // A dropped feed is opened again after a pause, and the agents are read on a timer meanwhile.
  const polled = browser.count("/api/workspace/agents");
  await browser.advance(4_000);
  assert.equal(FakeEventSource.opened.length, 1, "not at once");
  await browser.advance(1_000);
  assert.equal(FakeEventSource.opened.length, 2);
  FakeEventSource.opened.at(-1).emit("ready");
  await browser.advance(600);
  assert.match(browser.text(), /Updates live/);
  assert.ok(browser.count("/api/workspace/agents") > polled);
});

test("an agent's change carries the same count History shows, its own review's", async () => {
  const browser = fixture({ hash: `#/agents/${AGENT_KEY}` });
  browser.data["/api/changes"].changes.unshift({ id: AGENT_CHANGE, title: "Explain how want_bytes treats text", author: "Claude Code", agent: { name: "Claude Code", key: AGENT_KEY }, timestamp: secondsAgo(120), entity_deltas: 2 });
  browser.data["/api/counts"] = (url) => ({ counts: Object.fromEntries(url.searchParams.get("changes").split(",").map((id) => [id, { entities: id === AGENT_CHANGE ? 1 : 3 }])) });
  await flush(); await flush(); await flush();
  const row = browser.links().find((node) => node.textContent.includes("Explain how want_bytes treats text"));
  assert.match(row.textContent, /1 entity changed/);
  assert.doesNotMatch(row.textContent, /2 entities changed/, "not the history's delta count");
});

// ── Copy ───────────────────────────────────────────────────────────────────

/**
 * The string literals of a script and the line each starts on: quoted strings
 * and the fixed text of templates, with the literals inside a template's
 * expressions found too. Comments, regular expressions and code are not
 * literals. A literal that names an element's classes is marked, since a class
 * is never shown.
 */
function literals(code) {
  const found = [];
  const before = [];
  let at = 0;
  let line = 1;
  // A slash starts a regular expression where an expression may start, and divides anywhere else.
  let regexAllowed = true;
  const step = (n = 1) => { for (let i = 0; i < n; i++) { if (code[at] === "\n") line += 1; at += 1; } };
  const keep = (value, start, classes) => { found.push({ value, line: start, classes }); before.push("literal"); regexAllowed = false; };
  const namesClasses = () => before.at(-1) === ":" && before.at(-2) === "class";
  const quoted = (quote) => {
    const start = line; const classes = namesClasses(); let value = ""; step();
    while (at < code.length && code[at] !== quote) { if (code[at] === "\\") { value += code[at + 1]; step(2); } else { value += code[at]; step(); } }
    step();
    keep(value, start, classes);
  };
  const template = () => {
    const start = line; const classes = namesClasses(); let value = ""; step();
    while (at < code.length && code[at] !== "`") {
      if (code[at] === "\\") { value += code[at + 1]; step(2); }
      else if (code[at] === "$" && code[at + 1] === "{") { step(2); script("}"); }
      else { value += code[at]; step(); }
    }
    step();
    keep(value, start, classes);
  };
  function script(until) {
    let depth = 0;
    while (at < code.length) {
      const c = code[at];
      if (until && c === until && depth === 0) { step(); return; }
      if (c === "/" && code[at + 1] === "/") { while (at < code.length && code[at] !== "\n") step(); continue; }
      if (c === "/" && code[at + 1] === "*") { while (at < code.length && !(code[at] === "*" && code[at + 1] === "/")) step(); step(2); continue; }
      if (c === "\"" || c === "'") { quoted(c); continue; }
      if (c === "`") { template(); continue; }
      if (c === "/" && regexAllowed) {
        let inSet = false; step();
        while (at < code.length && (code[at] !== "/" || inSet)) { if (code[at] === "\\") step(); else if (code[at] === "[") inSet = true; else if (code[at] === "]") inSet = false; step(); }
        step();
        while (/[a-z]/.test(code[at] || "")) step();
        before.push("regex"); regexAllowed = false; continue;
      }
      if (/\s/.test(c)) { step(); continue; }
      if (/[\w$]/.test(c)) {
        let word = "";
        while (at < code.length && /[\w$]/.test(code[at])) { word += code[at]; step(); }
        before.push(word);
        regexAllowed = ["return", "typeof", "case", "in", "of", "void", "delete", "throw", "new", "else", "do"].includes(word);
        continue;
      }
      if (c === "{") depth += 1;
      if (c === "}") depth -= 1;
      before.push(c);
      regexAllowed = !")]}".includes(c);
      step();
    }
  }
  script(null);
  return found;
}

/** What the page's own code can show: its literals of two words or more that are not class names, and the frame's words. */
function writtenWords() {
  const words = [];
  for (const [name, code] of [["workspace.js", source], ["agents.js", agentsSource], ["graph.js", graphSource]]) {
    for (const { value, line, classes } of literals(code)) if (!classes) words.push({ where: `${name}:${line}`, text: value });
  }
  const frameBody = frameSource.slice(frameSource.indexOf("<body"));
  for (const [, value] of frameBody.matchAll(/\s(?:aria-label|title|alt|placeholder)="([^"]*)"/g)) words.push({ where: "index.html", text: value });
  for (const [, tag, value] of frameBody.matchAll(/<(\/?[a-z0-9-]+)[^>]*>([^<]*)/gi)) {
    if (tag.toLowerCase() !== "script" && value.trim()) words.push({ where: "index.html", text: value.trim() });
  }
  return words;
}

/** Every piece of text a page is showing: each run of words, and what its labels, hints and placeholders say. */
function shownWords(browser, where) {
  const words = [{ where: `${where}, the title`, text: browser.document.title || "" }];
  const visit = (node) => {
    if (node.tagName === "#TEXT") { if (String(node.text).trim()) words.push({ where, text: String(node.text).trim(), code: Boolean(node.parentNode?.closest("code, pre, kbd")) }); return; }
    if (node.text !== undefined && String(node.text).trim()) words.push({ where, text: String(node.text).trim(), code: Boolean(node.closest("code, pre, kbd")) });
    for (const name of ["aria-label", "title", "placeholder", "alt"]) if (node.getAttribute(name)) words.push({ where: `${where}, ${name}`, text: node.getAttribute(name) });
    node.children.forEach(visit);
  };
  visit(browser.document.body);
  return words;
}

// What the page's own words never say. A sentence of Kin's that the page passes on is Kin's to word.
const NEVER_SAID = [
  ["a file or a path", /\bfiles?\b|\bpaths?\b/i],
  ["an internal term", /inventory|reconcil|authority|evidence|call-site debt|\bscope\b|unverified|working tree|working copy|\bconsumers?\b|\brelations?\b|enrichment|Close in meaning/i],
  ["a word this page retired", /\brecorded\b|Read again/i],
  ["an em dash", /—/],
  ["a claim of all or every", /\b(every|all) (entit|change|relationship|connection|test)/i],
  ["a terminal or a command to run in one", /\bterminal\b|command[- ]line|\bshell\b|\bCLI\b|\bkin [a-z]|(^|\s)--[a-z]/],
  ["a value that was never filled in", /\bundefined\b|\[object |\bNaN\b|Invalid Date/],
];
const faults = (words, rules) => [...new Set(words.flatMap(({ where, text }) => rules.filter(([, rule]) => rule.test(text)).map(([name]) => `${where}: ${name}: ${JSON.stringify(text.length > 120 ? `${text.slice(0, 120)}…` : text)}`)))];
/** The literals that read as words a person is shown: two words or more. A lone word is checked where it is shown. */
const phrases = (words) => words.filter(({ text }) => /[A-Za-z]{3,} [A-Za-z]{2,}/.test(text));

/**
 * Every page, in the states a reader can bring it to, read for its words. The
 * answers it is given say nothing this file's rules forbid, so a fault found
 * is in a word the page chose.
 */
async function tour() {
  const shown = [];
  const see = (browser, where) => { shown.push(...shownWords(browser, where)); return browser; };
  const settled = async (browser, where) => { await flush(); await flush(); return see(browser, where); };
  const press = async (browser, label, where) => { browser.button(label).click(); await settled(browser, where); };
  const go = async (browser, address, where) => { browser.navigate(address); await settled(browser, where); };
  const call = (seq, ago, tool, ids) => ({ seq, at: secondsAgo(ago), tool, entity_ids: ids.map(uuid), result_count: 1, duration_ms: 5, is_error: false });
  const table = (n, changedAt = -1) => `${Array.from({ length: n }, (_, i) => `row_${i} = ${i === changedAt ? 1000 : i}`).join("\n")}\n`;
  // Uncommitted work with one entity of each kind, a long body, an agent whose work meets it, and a reading Kin has not finished.
  const rich = (browser) => {
    browser.data["/api/guide"] = working({ ...FOUR, modified: 3, completeness: { state: "pending", message: "", pending_entities: 4 } },
      [...fourChanged(), changed(8, "long_table", { before: { state: "recorded", text: table(40), start_line: 1 }, after: { state: "recorded", text: table(40, 20), start_line: 1, working: true } })], { edited: 60 });
    browser.data["/api/workspace/agents"] = agentsPayload({ agents: [{ ...agentsPayload().agents[0], recent_calls: [call(9, 10, "kin_transaction_stage", [2]), call(8, 15, "kin_mutate", [13]), call(7, 300, "get_entity_source", [5])] }] });
    return browser;
  };
  const outcomes = [
    [(url, options) => ({ operation_id: JSON.parse(options.body).operation_id, outcome: "committed", change_id: "d".repeat(64), title: "Bound input" }), "committed"],
    [(url, options) => ({ operation_id: JSON.parse(options.body).operation_id, outcome: "nothing_to_commit" }), "nothing to commit"],
    [{ __status: 409, error: "The projection is blocked.", body: { outcome: "refused", remedy: "Nothing was committed. Resolve what is reported, then commit again." } }, "commit refused"],
    [{ __status: 502, error: "The commit did not finish.", body: { outcome: "unknown" } }, "commit unknown"]];

  // Uncommitted: the list, an agent's work meeting it, and committing it.
  let browser = rich(fixture());
  see(browser, "Uncommitted, loading");
  await settled(browser, "Uncommitted");
  browser.button("Commit").click(); see(browser, "Uncommitted, commit form");
  browser.button("Cancel").click(); see(browser, "Uncommitted, commit cancelled");
  for (const [answer, where] of outcomes) {
    browser = rich(fixture()); await flush();
    browser.data["/api/workspace/commit"] = answer;
    writeCommit(browser, "Bound input"); see(browser, `Uncommitted, ${where}, sending`);
    await settled(browser, `Uncommitted, ${where}`);
  }
  browser = fixture(); browser.data["/api/guide"] = guide(); await settled(browser, "Uncommitted, nothing");
  browser = fixture(); browser.data["/api/guide"] = guide({}, { working_tree: { differs: true, derived: true, caught_up: true } }); await settled(browser, "Uncommitted, no entity changed");
  browser = fixture(); browser.data["/api/guide"] = guide({ added: 20_000 }, { not_walked: { entities: 20_000 } }); await settled(browser, "Uncommitted, too large to list");
  browser = fixture(); browser.failures.set("/api/guide", "The working read did not answer within 20 seconds."); await settled(browser, "Uncommitted, failed read");
  browser = fixture({ tabStorage: new Map([[workingKey, JSON.stringify({ at: START - 90_000, data: working({ modified: 1 }, [changed(2, "want_bytes")]) })]]) });
  let release; browser.held.set("/api/guide", new Promise((done) => { release = done; }));
  await settled(browser, "Uncommitted, an earlier reading"); release(); await flush();
  await settled(fixture({ initialFailure: true }), "a repository that is not answering");

  // A review of uncommitted work: each page of it, marked and unmarked, and each way it ends.
  browser = rich(fixture({ hash: "#/review/w/1" })); await settled(browser, "a review, the first step");
  for (const step of [2, 3, 4, 5]) await go(browser, `review/w/${step}`, `a review, step ${step}`);
  browser.node("review-body").querySelector("[role=region]").querySelector("button")?.click(); see(browser, "a review, a long body opened");
  await go(browser, "review/w/9", "a review, a step that is not there");
  await go(browser, "review/w/done", "a review, finished with nothing looked at");
  await go(browser, "review/w/1", "a review, back at the start");
  await flagWith(browser, "The callers still pass one argument."); await mark(browser, "fine"); see(browser, "a review, two marked");
  await go(browser, "review/w/1", "a review, a step marked flagged");
  await go(browser, "review/w/2", "a review, a step marked as looking right");
  await go(browser, "review/w/done", "a review, finished part way");
  await go(browser, "review/w/3", "a review, going on");
  await mark(browser, "fine", "flag", "fine"); see(browser, "a review, finished with flags");
  await press(browser, "Copy the flags", "a review, flags copied");
  browser.clipboard.refuses = true; await press(browser, "Copy the flags", "a review, flags not copied");
  await go(browser, "uncommitted", "Uncommitted, with marks");
  for (const [answer, where] of outcomes) {
    browser = rich(fixture({ hash: "#/review/w/1" })); await flush();
    await mark(browser, "fine", "fine", "fine", "fine", "fine");
    browser.data["/api/workspace/commit"] = answer;
    writeCommit(browser, "Bound input"); see(browser, `a review, ${where}, sending`);
    await settled(browser, `a review, ${where}`);
  }
  await settled(rich(fixture({ hash: "#/review/w/done", capabilities: { commit: false } })), "a review, a session that cannot commit");
  browser = fixture({ hash: "#/review/w/1" }); browser.failures.set("/api/guide", "Working read failed"); await settled(browser, "a review, failed read");

  // A review of a change on record, and the reader's view of it.
  browser = await reviewingChange("#/review/c/change-2"); see(browser, "a change, its list");
  await go(browser, "review/c/change-2/1", "a change, a step");
  await mark(browser, "flag", "fine", "fine", "fine"); see(browser, "a change, finished");
  browser.type("review-comment", "The callers were left behind.");
  browser.data["/api/decide"] = { __status: 502, error: "Kin could not reach this repository." };
  await press(browser, "Flag it", "a change, a view that was not saved");
  browser.data["/api/decide"] = () => { browser.data["/api/standing"] = { standing: { "change-2": { state: "needs_work" } } }; return { reviews: [] }; };
  browser.button("Flag it").click(); see(browser, "a change, saving a view"); await settled(browser, "a change, a view saved");
  await settled(await reviewingChange("#/review/c/change-1/done"), "a change, a view already given");
  browser = fixture({ hash: "#/review/c/large" }); browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { added: 20_000 }, [], { not_walked: { entities: 20_000 } }); await settled(browser, "a change too large to walk");
  browser = fixture({ hash: "#/review/c/empty" }); browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), {}, []); await settled(browser, "a change with no entity");
  browser = fixture({ hash: "#/review/c/slow" }); browser.failures.set("/api/guide?change", { status: 502, body: { error: "Kin could not answer: It did not answer within 20 seconds." } }); await settled(browser, "a change Kin is still reading");
  browser = fixture({ hash: "#/review/c/gone" }); browser.failures.set("/api/guide?change", { status: 404, body: { error: "No such change." } }); await settled(browser, "a change that could not be read");

  // What one branch holds beyond another, read the same way.
  browser = fixture({ hash: "#/review/r/main..feature" }); browser.data["/api/guide?base"] = (url) => ranged(url.searchParams.get("base"), url.searchParams.get("change")); await settled(browser, "two branches, the list");
  await go(browser, "review/r/main..feature/1", "two branches, a step");
  await mark(browser, "fine", "fine", "fine", "fine"); see(browser, "two branches, finished");
  browser.data["/api/decide"] = { reviews: [] }; await press(browser, "Looks right", "two branches, a view saved");
  browser = fixture({ hash: "#/review/r/main..gone" }); browser.failures.set("/api/guide?base", { status: 404, body: { error: "No branch is named gone." } }); await settled(browser, "two branches that could not be compared");

  // History, with an agent's change, each way of narrowing it, and a history still being read.
  browser = fixture({ hash: "#/history", session: { history: { state: "partial", admitted: 1200, total: 4000 } } });
  browser.data["/api/changes"].changes.unshift({ id: AGENT_CHANGE, title: "Explain how want_bytes treats text", author: "Claude Code", agent: { name: "Claude Code", key: AGENT_KEY }, timestamp: secondsAgo(120), entity_deltas: 1 });
  browser.data["/api/standing"] = { standing: { "change-1": { state: "approved" }, "change-2": { state: "needs_work" }, [AGENT_CHANGE]: { state: "blocked" } } };
  await settled(browser, "History");
  for (const choice of browser.node("workspace-review-filter").querySelectorAll("button").map((node) => node.textContent)) { browser.node("workspace-review-filter").querySelectorAll("button").find((node) => node.textContent === choice).click(); see(browser, `History, ${choice}`); }
  browser.type("workspace-review-search", "nothing like this"); see(browser, "History, no match");
  browser = fixture({ hash: "#/history" });
  browser.data["/api/changes"] = (url) => ({ changes: Array.from({ length: Number(url.searchParams.get("count")) }, (_, i) => ({ id: `c-${i}`, title: `Change ${i}`, author: "Dana Reader", timestamp: secondsAgo(i * 3600) })), truncated: true });
  await settled(browser, "History, a long one");
  browser = fixture({ hash: "#/history" }); browser.data["/api/changes"] = { changes: [], truncated: false }; await settled(browser, "History, empty");
  browser = fixture({ hash: "#/history" }); browser.failures.set("/api/changes", "History unreadable."); browser.failures.set("/api/standing", "Reviews unreadable."); await settled(browser, "History, failed reads");

  // Explore: where it opens, an answer, no answer, and a search that failed.
  browser = fixture({ hash: "#/explore" }); await settled(browser, "Explore");
  await go(browser, "explore?q=verify%20a%20signature", "Explore, an answer");
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "name_only", notice: "Meaning search is off, so this is a search by name.", results: [{ id: uuid(70), name: "want_bytes", kind: "function", matched: ["name"] }, { id: uuid(60), name: "Serializer.__init__", kind: "method", matched: ["name"] }, { id: uuid(61), name: "Serializer.__init__", kind: "method", matched: ["name"] }], total: 30 };
  await go(browser, "explore?q=want", "Explore, by name");
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  await go(browser, "explore?q=zzz", "Explore, nothing matched");
  browser.failures.set("/api/workspace/ask", "The index is not ready."); await go(browser, "explore?q=sign", "Explore, failed search");
  browser = fixture({ hash: "#/explore" }); browser.data["/api/workspace/overview"] = { schema: "kin.workspace.overview.v1", counts: { ok: true, entities: 3, relationships: 0 }, most_relied_on: { ok: true, items: [], total: 0, shown: 0 } }; await settled(browser, "Explore, nothing relied on");
  browser = fixture({ hash: "#/explore" }); browser.failures.set("/api/workspace/overview", "The graph is not loaded."); await settled(browser, "Explore, failed read");

  // An entity: every part, parts that failed, one the code no longer holds, and one that could not be read.
  const id = uuid(9);
  browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/guide"] = working({ modified: 1 }, [changed(9, "Signer.sign", { kind: "method" })]);
  browser.data["/api/workspace/entity"] = () => entityV2(id, {
    relied_on_by: { ok: true, total: 30, confirmed: 29, shown: 12, bound: "at_least", notice: "Some call sites could not be proven on this machine, so there may be more.", items: [
      { id: uuid(20), name: "Serializer.__init__", kind: "method", relationship: "references", is_test: false, confirmed: true, declarations: 6 },
      { id: uuid(21), name: "Serializer.dumps", kind: "method", relationship: "calls", is_test: false, confirmed: false },
      { id: uuid(22), name: "TimestampSigner", kind: "class", relationship: "extends", is_test: false, confirmed: true, relies: false },
      { id: uuid(24), name: "dumps", kind: "method", relationship: "calls", is_test: false, confirmed: true }, { id: uuid(25), name: "dumps", kind: "function", relationship: "calls", is_test: false, confirmed: true },
      ...Array.from({ length: 6 }, (_, i) => ({ id: uuid(200 + i), name: `caller_${i}`, kind: "function", relationship: "calls", is_test: false, confirmed: true })),
      { id: uuid(11), name: "TestSigner.test_sign", kind: "method", relationship: "calls", is_test: true, confirmed: true }] },
    members: { ok: true, items: [{ id: uuid(23), name: "Signer.sign.inner", kind: "function" }], total: 1, shown: 1 },
    editor: { ok: true, uri: `vscode://firelock.kin-editor/open-entity?v=1&repository=team%2Frepo&workspace=${uuid(1)}&root=%2Frepo&entity=${id}&scope=workspace` } });
  await settled(browser, "an entity");
  connections(browser).more(connections(browser).left)?.click(); see(browser, "an entity, each box shown");
  browser.links().find((node) => node.textContent === "Open in VS Code").emit("click"); see(browser, "an entity, opening the editor");
  await press(browser, "Copy entity ID", "an entity, its ID copied");
  browser.clipboard.refuses = true; await press(browser, "Copy entity ID", "an entity, its ID shown to copy by hand");
  browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, { header: { ok: false, reason: "Kin could not read this entity's name." }, source: { ok: false, reason: "Kin could not read this entity's text." }, history: { ok: false, reason: "Kin could not read this entity's history. It did not answer within 8 seconds." },
    relied_on_by: { ok: false, reason: "Kin could not read what relies on this entity." }, relies_on: { ok: false, reason: "Kin could not read what this entity relies on." }, tests: { ok: false, reason: "Kin could not read the tests that reach it." }, impact: { ok: false, reason: "Kin could not work out what a change would touch." } });
  await settled(browser, "an entity, parts failed");
  browser = fixture({ hash: `#/entity/${id}` }); browser.data["/api/workspace/entity"] = () => entityV2(id, { relied_on_by: { ok: true, items: [], total: 0, shown: 0 }, relies_on: { ok: true, items: [], total: 0, shown: 0 }, tests: { ok: true, items: [], total: 0 }, impact: { ok: true, direct: 0, transitive: 0, tests: 0, depth: 3, top: [] }, history: { ok: true, items: [], total: 0 } });
  await settled(browser, "an entity nothing relies on");
  browser = fixture({ hash: `#/entity/${id}` }); browser.data["/api/workspace/entity"] = () => { const { impact, history, ...rest } = entityV2(id, { source: { ok: true, text: table(200), start_line: 1, language: "python" } }); return rest; };
  await settled(browser, "an entity, a long source and parts Kin left out");
  browser.node("entity-source").querySelector("button")?.click(); see(browser, "an entity, its whole source");
  browser = fixture({ hash: `#/entity/${id}` }); browser.data["/api/workspace/entity"] = { __status: 404, error: "No such entity is in the current graph." }; await settled(browser, "an entity that is gone");
  browser = fixture({ hash: `#/entity/${id}` }); browser.failures.set("/api/workspace/entity", "The graph is not loaded."); await settled(browser, "an entity, failed read");

  // Branches: the list, comparing two, a new branch, a switch and a merge through each of their outcomes.
  const branches = (options = {}) => fixture({ hash: "#/branches", capabilities: { branch_create: true, conflict_settle: true }, ...options });
  browser = await settled(branches(), "Branches");
  browser.node("branches-compare-toggle").click(); see(browser, "Branches, comparing two");
  browser.node("branches-compare-toggle").click();
  await press(browser, "New branch", "Branches, naming a branch");
  browser.type("branch-new-name", "main");
  browser.data["/api/workspace/branch/create"] = { __status: 409, error: "The name is taken.", body: { outcome: "refused", remedy: "A branch named main already exists. Choose another name." } };
  newBranch(browser).emit("submit"); await settled(browser, "Branches, a branch refused");
  browser.type("branch-new-name", "feat/bound-input");
  browser.data["/api/workspace/branch/create"] = { outcome: "created" };
  newBranch(browser).emit("submit"); see(browser, "Branches, creating"); await settled(browser, "Branches, a branch created");
  browser = branches(); browser.data["/api/guide?base"] = guide({}, { range: { base_ref: "main", head_ref: "feature", changes_total: 0, changes: [] } }); await settled(browser, "Branches, a branch with nothing beyond this one");
  browser = branches(); browser.data["/api/branches"] = { branches: [{ name: "feature", active: true }, { name: "main", active: false, default: true }, { name: "level", active: false }, { name: "spare", active: false }, { name: "old", active: false }] }; await settled(browser, "Branches, off the default branch");
  for (const [failure, where] of [[null, "switched"], [{ status: 409, body: { outcome: "refused", error: "Your uncommitted work must be committed first." } }, "switch refused"], [new Error("Connection lost after send"), "switch unknown"]]) {
    browser = branches(); await flush();
    await press(browser, "Switch", "Branches, a switch previewed");
    if (failure) browser.failures.set("/api/workspace/branch/confirm", failure);
    branchConfirm(browser); see(browser, "Branches, switching"); await settled(browser, `Branches, ${where}`);
  }
  browser = branches(); browser.failures.set("/api/workspace/branch/preview", "Your uncommitted work must be committed first."); await flush(); await press(browser, "Switch", "Branches, a switch that could not be previewed");
  for (const [shape, failure, where] of [
    ["merged", null, "merged"], ["conflicted", null, "merge conflicted"], ["fast_forward", null, "moved forward"], ["already_up_to_date", null, "nothing to merge"],
    ["merged", { status: 409, body: { outcome: "refused", error: "The target head moved before the write.", remedy: "Read a new preview of the current heads." } }, "merge refused"],
    ["merged", new Error("Connection closed after request"), "merge unknown"]]) {
    browser = branches(); await flush();
    Object.assign(browser.data["/api/workspace/merge/preview"], { outcome: shape, conflicts: shape === "conflicted" ? [{ name: "Signer.sign", detail: "changed on both branches" }] : [], conflicts_total: shape === "conflicted" ? 1 : 0 });
    browser.data["/api/workspace/merge/confirm"].report.outcome = shape;
    await press(browser, "Merge into main", `Branches, a merge previewed (${shape})`);
    if (shape === "already_up_to_date") continue;
    if (failure) browser.failures.set("/api/workspace/merge/confirm", failure);
    browser.confirm(); see(browser, "Branches, merging"); await settled(browser, `Branches, ${where}`);
  }
  browser = branches({ merge: false }); await flush(); await press(browser, "Merge into main", "Branches, a session that can only preview");
  browser = branches({ merge: false, preview: false }); await flush(); await press(browser, "Merge into main", "Branches, a session that cannot merge");
  browser = branches(); browser.failures.set("/api/workspace/merge/preview", "Your uncommitted work must be committed first."); await flush(); await press(browser, "Merge into main", "Branches, a merge that could not be previewed");
  // A merge to resolve, as Kin answers it: with what a terminal would be given, which the page never shows.
  browser = branches(); browser.data["/api/workspace/conflicts/details"] = parked({ unavailable: [{ selector: uuid(41), reason: "Kin cannot tell this entity from another with the same identity." }] });
  await settled(browser, "Branches, a merge to resolve");
  browser.data["/api/workspace/conflicts/settle"] = { __status: 409, error: "The merge changed.", body: { outcome: "refused", remedy: "The merge changed since this page read it. Choose again." } };
  await press(browser, "Keep the target side", "Branches, a step refused");
  browser.data["/api/workspace/conflicts/settle"] = () => { browser.data["/api/workspace/conflicts/details"] = parked({ settled: true }); return { outcome: "settled", details: parked({ settled: true }) }; };
  browser.button("Take the incoming side").click(); see(browser, "Branches, settling"); await settled(browser, "Branches, each conflict settled");
  browser.failures.set("/api/workspace/conflicts/settle", new Error("Connection closed after request")); await press(browser, "Finish the merge", "Branches, a step lost");
  browser = branches({ capabilities: {} }); browser.data["/api/workspace/conflicts/details"] = parked(); await settled(browser, "Branches, a merge this session cannot settle");
  browser = branches(); browser.data["/api/workspace/conflicts/details"] = { ...parked({ settled: true }), state: { status: "committed", active: false, all_resolved: true } }; await settled(browser, "Branches, the last merge's record");
  browser = branches(); browser.data["/api/workspace/conflicts/details"] = { schema: "kin.workspace.conflicts.v2" }; await settled(browser, "Branches, a merge that could not be read");
  browser = branches(); browser.failures.set("/api/branches", "Branches unreadable."); browser.failures.set("/api/workspace/conflicts/details", "Conflicts unreadable."); await settled(browser, "Branches, failed reads");

  // Agents: one at work, none, one from earlier, a read that failed, and the live feed's states.
  FakeEventSource.opened.length = 0;
  browser = fixture({ hash: `#/agents/${AGENT_KEY}`, live: true }); await flush(); await settled(browser, "Agents");
  FakeEventSource.opened.at(-1).emit("ready"); await browser.advance(600); see(browser, "Agents, live");
  FakeEventSource.opened.at(-1).emit("change", { agent: AGENT_KEY, change: AGENT_CHANGE }); see(browser, "Agents, a commit announced");
  FakeEventSource.opened.at(-1).onerror(); see(browser, "Agents, reconnecting");
  await browser.advance(5_000); FakeEventSource.opened.at(-1).onerror(); see(browser, "Agents, checking on a timer");
  browser = fixture({ hash: "#/agents" }); browser.data["/api/workspace/agents"] = agentsPayload({ agents: [] }); await settled(browser, "Agents, none");
  browser = fixture({ hash: "#/agents" }); browser.data["/api/workspace/agents"] = agentsPayload({ agents: [{ ...agentsPayload().agents[0], state: "idle", recent_calls: [], open_changes: [], intents: [{ task: "Bound the input", entity_ids: [uuid(2)] }], changes: [], changes_total: 0 }], earlier: [{ ...agentsPayload().agents[0], key: "00000000000000b2", name: "Lantern", state: "ended", last_activity_at: secondsAgo(720) }] });
  browser.data["/api/workspace/agents/timeline"] = { __status: 404, error: "gone" }; await settled(browser, "Agents, idle and earlier");
  browser = fixture({ hash: `#/agents/${AGENT_KEY}` }); browser.failures.set("/api/workspace/agents/timeline", "Daemon unavailable"); await settled(browser, "Agents, calls that could not be read");
  browser = fixture({ hash: "#/agents" }); browser.failures.set("/api/workspace/agents", "Daemon unavailable"); await settled(browser, "Agents, failed read");
  browser = fixture({ hash: "#/agents" }); delete browser.data["/api/workspace/agents"]; await settled(browser, "Agents, not reported");

  // The machine's own pages: Home in each of its states, a question put to every repository, adding one through every answer, and its coding agents.
  const hub = (options = {}) => fixture({ hub: true, ...options });
  browser = hub();
  const many = [...browser.data["/api/home"].repositories, listed(repositoryKey("d"), "merging", open({ merge_in_progress: true, working: { changed: 0, differs: true } })),
    listed(repositoryKey("e"), "watched", open({ agents: { active: 0, connected: 3, names: [] } })), listed(repositoryKey("f"), "busy", open({ agents: { active: 2, connected: 2, names: [] } })),
    listed(repositoryKey("0"), "counting", open({ working: { differs: true } })),
    ...["starting", "reconciling", "indexing", "enriching", "reading_history"].map((phase, i) => listed(repositoryKey(String(i + 1)), `service-${i + 1}`, { ...open(), activity: { phase }, history: { admitted: 1200, total: 4000 }, entities: null, last_change_at: null }))];
  browser.data["/api/home"] = homePayload(many, { unavailable: 2, warning: "One entry in the registry could not be read." });
  answering(browser, many);
  browser.data[under(repositoryKey("d"), "/api/guide")] = working(FOUR, fourChanged());
  browser.data[under(KEY, "/api/workspace/ask")] = ASKED();
  browser.held.set(under(repositoryKey("0"), "/api/guide"), new Promise(() => {}));
  see(browser, "Home, loading"); await settled(browser, "Home");
  await browser.advance(40); see(browser, "Home, its pictures drawn");
  browser.type("home-ask-input", "sign"); await browser.advance(320); see(browser, "Home, a question answered");
  for (const row of many) browser.data[under(row.key, "/api/workspace/ask")] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: [], total: 0 };
  browser.type("home-ask-input", "zzz"); await browser.advance(320); see(browser, "Home, a question nothing matches");
  for (const row of many) browser.failures.set(under(row.key, "/api/workspace/ask"), "The index is not ready.");
  browser.type("home-ask-input", "sign"); await browser.advance(320); see(browser, "Home, a question nobody could answer");
  browser.node("home-ask-input").emit("keydown", { key: "Escape" }); see(browser, "Home, the question cleared");
  // Agents connected and idle, and none connected after one worked.
  browser = hub();
  browser.data[under(NOTES, "/api/workspace/agents")] = agentsPayload({ agents: [{ ...agentsPayload().agents[0], key: "00000000000000b2", name: "Lantern", state: "idle", recent_calls: [] }] });
  await settled(browser, "Home, an agent idle");
  browser = hub();
  browser.data[under(KEY, "/api/workspace/agents")] = agentsPayload({ agents: [], earlier: [{ ...agentsPayload().agents[0], key: "00000000000000b2", name: "Lantern", state: "ended", last_activity_at: secondsAgo(720) }] });
  browser.data["/api/home"] = homePayload(browser.data["/api/home"].repositories.map((row) => ({ ...row, agents: row.agents && { active: 0, connected: 0, names: [] } })));
  await settled(browser, "Home, no agent connected");
  // Work Home is reading, could not read, or is reading again.
  browser = hub(); browser.held.set(under(KEY, "/api/guide"), new Promise(() => {})); await settled(browser, "Home, work being read");
  browser = hub(); browser.failures.set(under(KEY, "/api/guide"), "The working read did not answer within 20 seconds."); await settled(browser, "Home, work that could not be read");
  browser = hub({ tabStorage: new Map([[`kin.workspace.working:/repositories/${KEY}`, JSON.stringify({ at: START - 60_000, data: working({ modified: 1 }, [changed(2, "want_bytes")]) })]]) });
  browser.held.set(under(KEY, "/api/guide"), new Promise(() => {})); await settled(browser, "Home, work read again");
  browser = hub(); browser.data[under(KEY, "/api/guide")] = working({ ...FOUR, completeness: { state: "pending", message: "" } }, fourChanged()); await settled(browser, "Home, work Kin is still mapping");
  browser = hub(); browser.data["/api/home"] = homePayload([listed(KEY, "Fixture", open({ merge_in_progress: true }))], { unavailable: 1 }); await settled(browser, "Home, one with a merge");
  browser = hub(); browser.data["/api/home"] = homePayload([]); await settled(browser, "Home, no repository");
  browser = hub(); browser.failures.set("/api/home", "Registry unreadable"); await settled(browser, "Home, failed read");
  await settled(hub({ initialFailure: true }), "the machine, not answering");
  browser = hub({ hash: "#/agents" }); await flush(); await go(browser, "", "Home, from Coding agents");
  browser = hub(); await flush();
  await openAdding(browser); see(browser, "Home, adding");
  for (const [mode, answers] of [
    [0, [inspected({ note: "Kin keeps its history." }), inspected({ kind: "plain", git: undefined }), inspected({ kind: "not_a_directory", actions: [], git: undefined }), inspected({ kind: "kin", actions: ["open"] }), inspected({ kind: "something_new", name: "", actions: [] }),
      inspected({ kind: "kin", registered: true, actions: [], repository: { key: KEY, url: `${BASE}/workspace/` } }), inspected({ kind: "plain", actions: [], git: undefined, inside: { key: KEY, name: "Fixture", url: `${BASE}/workspace/`, location: "~/code/fixture" } }),
      { __status: 400, error: "Kin adds a project from this machine by where it is." }, { __status: 404, error: "Not found" }]],
    [1, [inspected({ kind: "missing", exists: false, actions: ["create"], git: undefined })]]]) {
    addPanel(browser).querySelectorAll("button[aria-pressed]")[mode].click(); see(browser, `Home, adding (${mode})`);
    for (const answer of answers) { browser.data["POST /api/home/inspect"] = answer; browser.type("add-path", "~/code/itsdangerous"); addPanel(browser).querySelector("form").emit("submit"); see(browser, "Home, checking a place"); await settled(browser, "Home, a place checked"); }
  }
  addPanel(browser).querySelectorAll("button[aria-pressed]")[2].click(); see(browser, "Home, cloning");
  browser.type("add-source", "https://example.invalid/pallets/itsdangerous.git"); browser.type("add-path", "~/code/itsdangerous");
  browser.data["POST /api/home/repositories"] = { __status: 409, error: "Another clone into that place is running." };
  addPanel(browser).querySelector("form").emit("submit"); see(browser, "Home, starting a clone"); await settled(browser, "Home, a clone refused");
  for (const [action, readings] of [
    ["convert", [...["starting", "reading", "graph", "references", "history", "finishing"].map((stage) => operation("running", { stage, progress: stage === "graph" ? { done: 120, total: 800 } : undefined })), operation("done", { stage: "finishing", repository: { key: repositoryKey("d"), name: "itsdangerous", url: `/repositories/${repositoryKey("d")}/workspace/` }, note: "Its history is still being read." })]],
    ["convert", [...["checked 1,200 of 4,000", "Starting the language server", "Waiting for the daemon", "Reading history", "Cloning into place", "Parsing 12 of 40", ""].map((phase) => operation("running", { stage: "", phase })), operation("failed", { error: "Kin could not convert this project.", detail: "The conversion stopped with status 1.", lines: ["Parsing 12 of 40"] })]],
    ["clone", [operation("running", { action: "clone", stage: "", phase: "" }), operation("done", { action: "clone" })]],
    ["open", [operation("failed", { action: "open", error: "" })]]]) {
    browser = hub(); await flush();
    await look(browser, inspected());
    browser.data["POST /api/home/repositories"] = operation("running", { action, stage: "starting" });
    browser.data[`/api/home/operations/${OPERATION}`] = [...readings];
    browser.button("Convert to Kin").click(); await settled(browser, "Home, an operation started");
    for (const reading of readings) { await browser.advance(900); see(browser, `Home, an operation ${reading.state}`); }
  }
  browser = hub(); await flush(); await look(browser, inspected());
  browser.data["POST /api/home/repositories"] = operation("running"); browser.failures.set(`/api/home/operations/${OPERATION}`, "Daemon unavailable");
  browser.button("Convert to Kin").click(); await flush(); await browser.advance(900 * 7); see(browser, "Home, an operation Kin stopped answering about");
  browser = await settled(hub({ hash: "#/agents" }), "Coding agents");
  browser.data["POST /api/home/clients"] = { operation_id: OPERATION, state: "running" };
  browser.data[`/api/home/operations/${OPERATION}`] = { operation_id: OPERATION, action: "connect", state: "failed", error: "Harbor Editor's settings could not be written." };
  browser.node("hub-clients").querySelector("button").click(); see(browser, "Coding agents, connecting one"); await flush(); await browser.advance(900); await settled(browser, "Coding agents, one that was not connected");
  browser = hub({ hash: "#/agents" }); browser.failures.set("/api/home/clients", "Settings unreadable"); await settled(browser, "Coding agents, failed read");
  browser = hub({ hash: "#/agents" }); delete browser.data["/api/home/clients"]; await settled(browser, "Coding agents, not listed");
  return shown;
}

test("the page's code is read for its words, and nothing but its words", () => {
  const sample = literals("const a = { class: `row row-${tone === \"x\" ? \"is on\" : \"off\"}`, title: \"Shown here\" }; // \"not shown\"\nconst b = text.split(/`([^`]+)`/).map((part) => `${count(n)} ${n === 1 ? \"relies on it\" : 'rely on it'} of ${a / b}`);\n/* \"nor this\" */ const c = a / 2 + \"last one\";");
  assert.deepEqual(sample.map(({ value, line, classes }) => [value, line, classes]), [
    ["x", 1, false], ["is on", 1, false], ["off", 1, false], ["row row-", 1, true], ["Shown here", 1, false],
    ["relies on it", 2, false], ["rely on it", 2, false], ["  of ", 2, false], ["last one", 3, false]]);
  const written = writtenWords();
  for (const expected of ["Kin couldn't load the changes.", "Skip to content", "Refresh", "This page needs JavaScript."]) assert.ok(written.some(({ text }) => text === expected), expected);
  assert.deepEqual(phrases(written).filter(({ text }) => /=>|\bconst \w|\bfunction \w|;\s*\n/.test(text)), [], "no code was taken for a literal");
});

// The pages are toured once, and each test reads what the tour saw.
let toured = null;
const touredOnce = () => toured ??= tour();

test("the page's own words avoid files, paths, internal terms, em dashes and commands to run", async () => {
  const written = writtenWords();
  assert.deepEqual(faults(phrases(written), NEVER_SAID), []);
  for (const code of [source, agentsSource, graphSource, frameSource]) assert.doesNotMatch(code.replace(/\/\/.*$/gm, ""), /—/);
  const shown = await touredOnce();
  assert.ok(shown.length > 5000, "the tour drew the pages");
  assert.deepEqual(faults(shown, NEVER_SAID), []);
  // A span set as code holds a name or an id, never something to run.
  assert.deepEqual(shown.filter(({ code, text }) => code && /^\s*(kin|git|npm|cargo|sudo|brew|sh|bash)\s/.test(text)).map(({ where, text }) => `${where}: ${text}`), []);
});

test("the page never says, as a word alone, the word it retired for a reading in progress", async () => {
  assert.deepEqual(writtenWords().filter(({ text }) => /^Reading$/.test(text)).map(({ where }) => where), []);
  assert.deepEqual((await touredOnce()).filter(({ text }) => /^Reading$/.test(text)).map(({ where }) => where), []);
});

const exactChanged = (n, name, fields = {}) => changed(n, name, {
  after: { state: "recorded", text: `def ${name}():\n    return 1\n`, working: true, body_hash: "a".repeat(64) }, ...fields,
});

test("entity undo pins the exact source, refuses a missing guard and preserves its pending receipt on reload", async () => {
  const tabStorage = new Map();
  const browser = fixture({ hash: "#/review/w/1", capabilities: { entity_restore: true }, tabStorage });
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes")]);
  browser.data["POST /api/workspace/entity/restore"] = { __status: 409, error: "The entity changed after this reading.", body: { outcome: "refused", remedy: "Refresh this entity and review its latest source." } };
  await flush(); browser.node("review-undo").click(); browser.node("review-undo-confirm").click(); await flush();
  const sent = browser.requests.filter((row) => row.method === "POST");
  assert.equal(sent.length, 1);
  assert.deepEqual(JSON.parse(sent[0].options.body), { entity: uuid(2), operation_id: uuid(1), expected: `source:${"a".repeat(64)}` });
  assert.match(browser.text(), /The entity changed after this reading\./, "a refusal says Kin's own sentence");
  assert.ok(browser.node("review-undo-reread"), "a refusal offers a fresh reading");
  assert.equal([...tabStorage.keys()].filter((key) => key.includes("pending-restore")).length, 0, "a refusal removes the pending receipt");
  browser.failures.set("POST /api/workspace/entity/restore", new Error("The answer was lost"));
  browser.node("review-undo-confirm").click(); await flush();
  assert.match(browser.text(), /The undo may have happened/);
  assert.equal(browser.button("I've checked").disabled, true, "an old read cannot settle uncertainty");
  const reload = fixture({ hash: "#/review/w/1", capabilities: { entity_restore: true }, tabStorage });
  reload.data["/api/guide"] = browser.data["/api/guide"]; await flush();
  assert.match(reload.text(), /The undo may have happened/);
  assert.equal(reload.requests.filter((row) => row.method === "POST").length, 0, "reload never replays the undo");
  const missing = fixture({ hash: "#/review/w/1", capabilities: { entity_restore: true } });
  await flush(); assert.equal(missing.node("review-undo").disabled, true);
  missing.node("review-undo").click(); assert.equal(missing.requests.some((row) => row.method === "POST"), false);
});

test("a refused undo reads the work again and undoes from that reading, never resending the old one", async () => {
  const browser = fixture({ hash: "#/review/w/1", capabilities: { entity_restore: true } });
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes")]);
  browser.data["POST /api/workspace/entity/restore"] = { __status: 409, error: "want_bytes changed since it was read, so nothing was restored.", body: { outcome: "refused", remedy: "Nothing was changed. Resolve what is reported, then restore it again." } };
  await flush(); browser.node("review-undo").click(); browser.node("review-undo-confirm").click(); await flush();
  assert.match(browser.text(), /want_bytes changed since it was read, so nothing was restored\./);
  assert.doesNotMatch(browser.text(), /\bfiles?\b/i, "a refusal speaks of the entity");
  assert.doesNotMatch(browser.text(), /The undo may have happened/, "a refusal is settled, not uncertain");
  const reads = browser.count("/api/guide");
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes", { after: { state: "recorded", text: "def want_bytes():\n    # A newer comment\n    return 1\n", working: true, body_hash: "b".repeat(64) } })]);
  browser.node("review-undo-reread").click(); await flush();
  assert.equal(browser.count("/api/guide"), reads + 1, "the work is read again");
  assert.equal(browser.node("review-undo-confirm"), null, "the refused confirmation closes with its reading");
  assert.match(browser.text(), /A newer comment/);
  assert.equal(browser.count("/api/workspace/entity/restore"), 1, "the refused undo is never resent");
  browser.data["POST /api/workspace/entity/restore"] = () => { browser.data["/api/guide"] = guide(); return { outcome: "restored", entity: uuid(2), name: "want_bytes", to: "c".repeat(64), replayed: false }; };
  browser.node("review-undo").click(); browser.node("review-undo-confirm").click(); await flush();
  const sent = browser.sent("/api/workspace/entity/restore");
  assert.equal(sent.length, 2);
  assert.equal(sent[1].expected, `source:${"b".repeat(64)}`, "the undo names the source it was read again at");
  assert.notEqual(sent[1].operation_id, sent[0].operation_id);
});

test("bringing back a removed entity pins its absence", async () => {
  const browser = fixture({ hash: "#/review/w/1", capabilities: { entity_restore: true } });
  browser.data["/api/guide"] = working({ removed: 1 }, [changed(2, "gone", { change: "removed" })]);
  browser.data["POST /api/workspace/entity/restore"] = () => { browser.data["/api/guide"] = guide(); return { outcome: "restored", entity: uuid(2), name: "gone", to: "b".repeat(64), replayed: false }; };
  await flush(); browser.node("review-undo").click(); browser.node("review-undo-confirm").click(); await flush();
  assert.equal(JSON.parse(browser.requests.find((row) => row.method === "POST").options.body).expected, "absent");
  assert.equal(browser.count("/api/workspace/entity/restore"), 1);
});

test("review marks become new again when source changes without a syntax or behavior change", async () => {
  const browser = fixture({ hash: "#/review/w/1" });
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes")]);
  await flush(); await mark(browser, "fine");
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes", { after: { state: "recorded", text: "# A new comment\ndef want_bytes():\n    return 1\n", body_hash: "b".repeat(64) } })]);
  browser.refresh(); await flush(); browser.navigate("review/w/1"); await flush();
  assert.equal(stepOf(browser).marked, "", "the same behavior hash never carries a review to a different source span");
});

test("selected commits preserve the exact selection when the engine refuses to split new content", async () => {
  const browser = fixture({ hash: "#/review/w/1", capabilities: { entity_commit: true } });
  browser.data["/api/guide"] = working({ modified: 2 }, [exactChanged(2, "first"), exactChanged(3, "second")]);
  browser.data["POST /api/workspace/commit"] = { __status: 409, error: "New content cannot be split yet.", body: { outcome: "refused", remedy: "Keep this selection uncommitted or explicitly choose to commit all entities." } };
  await flush(); await mark(browser, "fine", "flag");
  assert.equal(browser.node("commit-only-fine").checked, true);
  writeCommit(browser, "Keep first"); await flush();
  assert.deepEqual(JSON.parse(browser.requests.find((row) => row.method === "POST").options.body).entities, [uuid(2)]);
  assert.equal(browser.node("commit-message").textContent, "Keep first");
  assert.match(browser.text(), /Keep this selection uncommitted/);
  browser.node("commit-submit").click(); await flush();
  assert.deepEqual(browser.requests.filter((row) => row.method === "POST").map((row) => JSON.parse(row.options.body).entities), [[uuid(2)], [uuid(2)]], "retry never widens the selection");
});

test("durable flags use exact source and report the agent who saw them without claiming authorship", async () => {
  const browser = fixture({ hash: "#/review/w/1", capabilities: { review_flags: true } });
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes")]);
  browser.data["/api/workspace/flags"] = { flags: [] };
  browser.data["POST /api/workspace/flags"] = (_, options) => {
    const body = JSON.parse(options.body);
    browser.data["/api/workspace/flags"] = { flags: [{ entity_id: body.entity, fingerprint: body.fingerprint, seen_by: [{ agent: "Claude Code", at: secondsAgo(5) }], changed_since: false }] };
    return { outcome: "flagged" };
  };
  browser.data["POST /api/workspace/flags/resolve"] = { outcome: "resolved" };
  await flush(); await flagWith(browser, "Check the callers"); await browser.advance(1);
  const request = browser.requests.find((row) => row.method === "POST");
  assert.equal(JSON.parse(request.options.body).fingerprint, `source:${"a".repeat(64)}`);
  assert.match(browser.text(), /Seen by Claude Code/);
  browser.data["/api/workspace/flags"].flags[0].changed_since = true;
  browser.refresh(); await flush(); assert.match(browser.text(), /Changed since you flagged it/);
  assert.doesNotMatch(browser.text(), /Claude Code changed it/);
  browser.navigate("review/w/1"); await flush(); await mark(browser, "fine");
  assert.equal(browser.count("/api/workspace/flags/resolve"), 1, "the durable entity_id is used when clearing its flag");
});

test("a flag the repository keeps marks its entity in any browser; a mark of looks right stays in the browser that made it", async () => {
  // A browser that never marked anything opens uncommitted work the repository holds a flag on.
  const browser = fixture({ hash: "#/review/w/1", capabilities: { review_flags: true } });
  browser.data["/api/guide"] = working({ modified: 2 }, [exactChanged(2, "want_bytes"), changed(3, "other", { after: { state: "recorded", text: "def other():\n    return 2\n", working: true, body_hash: "b".repeat(64) } })]);
  browser.data["/api/workspace/flags"] = { flags: [
    { entity_id: uuid(2), fingerprint: `source:${"a".repeat(64)}`, note: "Check the callers", seen_by: [], changed_since: false },
    // A flag on a version this work no longer shows is about something else.
    { entity_id: uuid(3), fingerprint: `source:${"c".repeat(64)}`, note: "Old", seen_by: [], changed_since: true }] };
  await flush();
  assert.equal(stepOf(browser).marked, "Flagged", "the repository's flag on this version marks it here too");
  assert.equal(browser.node("review-note").value, "Check the callers");
  browser.navigate("review/w/done"); await flush();
  assert.match(browser.node("review-body").textContent, /^1 of 2 marked/);
  assert.deepEqual(reviewRows(browser.node("review-body")).map((group) => [group.title, group.rows.map((row) => row.name)]), [["Flagged", ["want_bytes"]]]);
  // What this browser marks itself wins over what the repository keeps.
  browser.navigate("review/w/1"); await flush();
  browser.data["POST /api/workspace/flags/resolve"] = () => { browser.data["/api/workspace/flags"] = { flags: [] }; return { outcome: "resolved" }; };
  browser.node("review-fine").click(); await flush(); await browser.advance(1);
  browser.navigate("review/w/1"); await flush();
  assert.equal(stepOf(browser).marked, "Looks right");
  assert.equal(browser.count("/api/workspace/flags/resolve"), 1, "and the repository's flag is closed");
});

test("entities a view would show under one name are told apart by what holds each", async () => {
  const browser = fixture({ hash: "#/review/w/1" });
  const module = (n, name) => ({ relation_id: `r-${n}`, relation: "references", origin: "parsed", confidence: 1, entity: { id: uuid(n), name, kind: "module", role: "source" }, changed_here: false });
  const test_ = (n, within) => ({ relation_id: `t-${n}`, relation: "tests", entity: { id: uuid(n), name: "Test", kind: "constant", role: "test", within }, hops: 3 });
  browser.data["/api/guide"] = working({ modified: 1, recorded_consumers: 3, consumers_may_need_review: 3, recorded_tests: 3 }, [
    changed(2, "_toLowerCase", { connections: connected([relying(10, "_ZodString", { relation: "calls", entity: { id: uuid(10), name: "_ZodString", kind: "function", role: "source", within: "module schemas" } }), module(11, "checks"), module(12, "checks")],
      [test_(20, "module error-utils.test"), test_(21, "module object.test"), relying(22, "apply.test", { relation: "tests", entity: { id: uuid(22), name: "apply.test", kind: "module", role: "test" }, hops: 3 }),
        // Three of one name where two also share what holds them: each is said by the end of its place no other shares.
        ...[[23, "test_tutorial001"], [24, "test_tutorial002"], [25, "test_tutorial001"]].map(([n, module]) => ({ relation: "tests", entity: { id: uuid(n), name: "client", kind: "function", role: "test", within: `module ${module}` }, hops: 3 }))]),
      consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 3 } })]);
  // Kin places the two modules nowhere in the guide, so each is read once for the place Kin records it under.
  browser.data["/api/workspace/entity"] = (url) => {
    const id = url.searchParams.get("id");
    const place = { [uuid(11)]: "zod.src.v4.classic.checks", [uuid(12)]: "zod.src.v4.mini.checks",
      [uuid(23)]: "tests.test_tutorial.test_query_params.test_tutorial001", [uuid(24)]: "tests.test_tutorial.test_query_params.test_tutorial002", [uuid(25)]: "tests.test_tutorial.test_body.test_tutorial001" }[id];
    return { id, header: { ok: true, name: "checks", kind: "module", module: place }, source: { ok: true, text: "export { _toLowerCase } from \"./api\";", start_line: 1, language: "typescript" } };
  };
  await flush(); await flush();
  const step = stepOf(browser);
  const reliers = picture(step.relies).columns[1].boxes.map(([name, place]) => [name, place]);
  assert.deepEqual(reliers, [["_ZodString", "calls it"], ["checks in classic", "imports it"], ["checks in mini", "imports it"]]);
  // How each uses it says which is which in the same words.
  assert.deepEqual(browser.node("review-sites").querySelectorAll(".site-head").map((head) => head.textContent), ["_ZodStringdid not change", "checksin classicdid not change", "checksin minidid not change"]);
  assert.deepEqual(browser.node("step-tests").querySelectorAll("li").map((row) => row.textContent), ["Testin error-utils.test3 steps away", "Testin object.test3 steps away", "apply.test3 steps away",
    "clientin test_query_params.test_tutorial0013 steps away", "clientin test_tutorial0023 steps away", "clientin test_body.test_tutorial0013 steps away"]);
  assert.equal(browser.requests.filter((request) => request.name === "/api/workspace/entity" && /sections=header/.test(request.url)).length, 5, "only the ones that share a name and a holder are read for their place, once each");
  // The picture of the whole work tells them apart the same way.
  browser.navigate("uncommitted"); await flush();
  assert.deepEqual(picture(browser.node("work-flight")).columns[1].boxes.map(([name]) => name), ["_ZodString", "checks in classic", "checks in mini"]);
});

test("a failed durable flag remains visibly local and never blocks committing", async () => {
  const browser = fixture({ hash: "#/review/w/1", capabilities: { review_flags: true } });
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes")]);
  browser.data["POST /api/workspace/flags"] = { __status: 403, error: "This session cannot save flags." };
  await flush(); await mark(browser, "flag");
  assert.match(browser.text(), /Kin could not save the flag for agents/);
  assert.match(browser.text(), /Your review mark is still kept in this browser/);
  assert.equal(browser.node("commit-submit").disabled, false);
});

test("verification shows full-suite failure, unavailable and no-tests outcomes honestly", async () => {
  for (const [answer, title] of [[{ state: "unavailable", reason: "No verification plan is configured." }, "Tests are unavailable"], [{ state: "done", outcome: "no_tests", result: { run: null, reach: "targeted", tests: [] } }, "No tests ran"], [{ state: "done", outcome: "failed", result: { run: { outcome: "failed", exit_code: 1 }, reach: "whole", tests: [] } }, "Test command failed"]]) {
    const browser = fixture({ capabilities: { verify_run: true } });
    browser.data["POST /api/workspace/verify"] = { operation_id: uuid(1), state: "running", phase: "Planning tests" };
    browser.data[`/api/workspace/operations/${uuid(1)}`] = { operation_id: uuid(1), ...answer };
    if (answer.state === "unavailable") browser.data["POST /api/workspace/verify"] = answer;
    await flush(); browser.node("work-run-tests").click(); await flush(); await browser.advance(1000);
    assert.match(browser.node("work-verify").textContent, new RegExp(title));
    if (answer.outcome === "no_tests") assert.match(browser.text(), /has not passed a test run/);
    if (answer.outcome === "failed") assert.match(browser.text(), /full test suite/);
  }
});

test("a test command that could not run is said with why, its exit code and its output, and its tests read as not run", async () => {
  const notRun = { state: "not_run", at: secondsAgo(2), summary: "The test command could not run, so this test did not run. It exited 127." };
  const tests = [relying(20, "404s.test", { result: notRun }), relying(21, "body-limit.test", { result: notRun })];
  const run = { run_id: "r", outcome: "could_not_run", duration_ms: 395, exit_code: 127, finished_at: secondsAgo(1), reason: "The shell could not find `eslint`, so no test ran.", output_tail: "> fastify@5.6.1 lint\n> eslint\nsh: eslint: command not found" };
  const browser = fixture({ capabilities: { verify_run: true } });
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "setErrorHeaders", { tests_result: { not_run: 2 }, connections: connected([], tests) })]);
  browser.data["POST /api/workspace/verify"] = { operation_id: uuid(1), state: "running", phase: "Planning the tests that reach this work." };
  browser.data[`/api/workspace/operations/${uuid(1)}`] = { operation_id: uuid(1), state: "done", outcome: "could_not_run", phase: "The test command could not run.", result: { reach: "whole", entities: [], tests: [], run } };
  await flush(); browser.node("work-run-tests").click(); await flush(); await browser.advance(1000);
  const note = browser.node("work-verify");
  assert.match(note.textContent, /Test command could not run/);
  assert.match(note.textContent, /The shell could not find `eslint`, so no test ran\. It exited 127\. No test passed or failed/);
  assert.doesNotMatch(note.textContent, /Test command failed|full test suite|Run failed/, "a command that never reached a test is not said as a failed run");
  assert.equal(note.querySelectorAll(".message-bad").length, 0);
  assert.equal(note.querySelectorAll(".message-warn").length, 1);
  const details = note.querySelector("details");
  assert.match(details.textContent, /Exit code 127/);
  assert.equal(details.querySelector("pre").textContent, run.output_tail, "the end of its output is shown as it printed it");
  assert.match(details.textContent, new RegExp(`Operation ${uuid(1)}`));

  browser.navigate("review/w/1"); await flush();
  const step = stepOf(browser);
  assert.deepEqual(step.rows(step.tests), ["404s.test1 step awayNot run", "body-limit.test1 step awayNot run"]);
  const word = step.tests.querySelector("span.state");
  assert.match(word.getAttribute("title"), /^The test command could not run, so this test did not run\. It exited 127\. Tried /);
  assert.doesNotMatch(browser.text(), /Run failed|These results describe the test command/);

  // A test command that ran and failed is still a failure, with its exit code and output kept.
  const failing = fixture({ capabilities: { verify_run: true } });
  failing.data["POST /api/workspace/verify"] = { operation_id: uuid(1), state: "running", phase: "Planning tests" };
  failing.data[`/api/workspace/operations/${uuid(1)}`] = { operation_id: uuid(1), state: "done", outcome: "failed", result: { reach: "whole", tests: [], run: { outcome: "failed", exit_code: 1, output_tail: "not ok 3 - reply carries the status" } } };
  await flush(); failing.node("work-run-tests").click(); await flush(); await failing.advance(1000);
  const failed = failing.node("work-verify");
  assert.match(failed.textContent, /Test command failed/);
  assert.match(failed.textContent, /Kin ran the repository's full test suite\. It exited 1\. It did not report individual test outcomes\./);
  assert.equal(failed.querySelectorAll(".message-bad").length, 1);
  assert.equal(failed.querySelector("details pre").textContent, "not ok 3 - reply carries the status");
});

test("verification with a lost start answer checks its operation without starting another run", async () => {
  const browser = fixture({ capabilities: { verify_run: true } });
  browser.failures.set("POST /api/workspace/verify", new Error("Answer lost"));
  browser.data[`/api/workspace/operations/${uuid(1)}`] = { operation_id: uuid(1), state: "done", outcome: "passed", result: { reach: "targeted", run: { outcome: "passed" } } };
  await flush(); browser.node("work-run-tests").click(); await flush();
  assert.match(browser.text(), /test run's outcome is unknown/);
  assert.equal(browser.node("work-run-tests").disabled, true);
  browser.button("Check the run").click(); await browser.advance(1000);
  assert.match(browser.text(), /Test command passed/);
  assert.equal(browser.count("/api/workspace/verify"), 1, "checking the operation never sends the start again");
});

const remotePayload = (resolve = "fetch_then_merge") => ({ remotes: [{ name: "peer", kind: "kin", can_pull: true, can_push: true, can_fetch: true, standing: { state: "diverged", summary: "Both branches have changes.", push: { state: "blocked" }, pull: { state: "blocked", resolve, resolve_branch: "peer/main" } } }] });

test("diverged native work fetches into a separate branch and opens the existing merge preview", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { remote_pull: true, remote_push: true, remote_fetch: true } });
  browser.data["/api/workspace/remotes"] = remotePayload();
  browser.data["POST /api/workspace/fetch"] = (_, options) => {
    browser.data["/api/workspace/remotes"] = remotePayload("merge");
    browser.data["/api/branches"].branches.push({ name: "peer/main", active: false });
    return { operation_id: JSON.parse(options.body).operation_id, outcome: "fetched", into_branch: "peer/main", changes: 2, entities: 3 };
  };
  await flush(); assert.equal(browser.button("Pull"), undefined);
  browser.button("Fetch for review").click(); await flush();
  assert.deepEqual(JSON.parse(browser.requests.find((row) => row.method === "POST").options.body), { remote: "peer", operation_id: uuid(1) });
  assert.match(browser.text(), /Fetched from peer/);
  assert.match(browser.text(), /ready to review and merge/);
  browser.button("Preview merge").click(); await flush();
  assert.equal(browser.node("workspace-merge-source").value, "peer/main");
  assert.equal(browser.count("/api/workspace/merge/preview"), 1);
  assert.equal(browser.count("/api/workspace/pull"), 0, "divergence is never treated as a fast-forward pull");
});

test("optional capabilities recover on explicit refresh and failed remotes leave work available", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { remote_fetch: true } });
  await flush(); assert.equal(browser.node("branches-remotes").textContent, "");
  browser.data["/api/workspace/remotes"] = remotePayload();
  browser.refresh(); await flush(); assert.ok(browser.button("Fetch for review"));
  browser.failures.set("/api/workspace/remotes", "The remote could not be reached.");
  browser.refresh(); await flush();
  browser.navigate("uncommitted"); assert.ok(browser.button("Commit"));
});

test("a graph-proven test is never also listed as matched by name only", async () => {
  const browser = fixture({ hash: `#/entity/${uuid(9)}` });
  browser.data["/api/workspace/entity"] = () => entityV2(uuid(9), {
    relied_on_by: { ok: true, total: 2, confirmed: 0, items: [
      { id: uuid(11), name: "TestSigner.test_sign", kind: "method", relationship: "calls", is_test: true, confirmed: false },
      { id: uuid(21), name: "possibleCaller", kind: "function", relationship: "calls", confirmed: false },
    ] },
  });
  await flush();
  const guessed = browser.main().querySelectorAll(".rel-group").find((node) => node.textContent.includes("Matched by name only"));
  assert.match(browser.node("entity-impact").textContent, /TestSigner.test_sign/);
  assert.ok(guessed); assert.match(guessed.textContent, /possibleCaller/);
  assert.doesNotMatch(guessed.textContent, /TestSigner.test_sign/, "coverage is stronger than a name-only candidate for the same entity");
});

test("test labels describe the command and do not count one shared test once for each changed entity", async () => {
  const browser = fixture();
  const tests = [relying(20, "testShared", { result: { state: "passed", summary: "The test command passed; Kin does not report individual test outcomes." } })];
  browser.data["/api/guide"] = working({ modified: 2 }, [exactChanged(2, "first", { tests_result: { passed: 1 }, connections: connected([], tests) }), exactChanged(3, "second", { tests_result: { passed: 1 }, connections: connected([], tests) })]);
  await flush();
  assert.equal(browser.node("work-verify").textContent, "A test command passed.");
  browser.navigate("review/w/1"); await flush();
  assert.match(browser.text(), /Run passed/);
  assert.match(browser.text(), /Kin did not report individual test outcomes/);
  assert.doesNotMatch(browser.text(), /Two tests pass/);
});

test("a lost commit answer stays uncertain across reload without a second write", async () => {
  const tabStorage = new Map();
  const browser = fixture({ tabStorage }); await flush();
  browser.failures.set("POST /api/workspace/commit", new Error("Connection lost"));
  writeCommit(browser, "Keep work"); await flush();
  const reload = fixture({ tabStorage }); await flush();
  assert.match(reload.node("work-commit").textContent, /commit's outcome is unknown/);
  assert.equal(reload.requests.some((row) => row.method === "POST"), false);
  assert.equal(reload.button("Commit").disabled, true);
});

test("a selected commit keeps unchosen review marks on the new working base", async () => {
  const kept = new Map();
  const browser = fixture({ hash: "#/review/w/1", capabilities: { entity_commit: true }, kept });
  const first = exactChanged(2, "first"), second = exactChanged(3, "second");
  browser.data["/api/guide"] = working({ modified: 2 }, [first, second]);
  const changeId = "c".repeat(64);
  browser.data["POST /api/workspace/commit"] = (_, options) => {
    browser.data["/api/guide"] = { ...working({ modified: 1 }, [second]), change: { base: changeId } };
    return { outcome: "committed", change_id: changeId, title: JSON.parse(options.body).message, entities: [uuid(2)] };
  };
  await flush(); await mark(browser, "fine", "flag");
  writeCommit(browser, "Keep first"); await flush();
  const committedMarks = marksOf(kept, changeId);
  const unchosenMarks = marksOf(kept, `w:${changeId}`);
  assert.deepEqual(committedMarks, { fine: [`${uuid(2)}:${"a".repeat(64)}`], flagged: [], notes: {} });
  assert.deepEqual(unchosenMarks, { fine: [], flagged: [`${uuid(3)}:${"a".repeat(64)}`], notes: {} });
  browser.navigate("review/w/1"); await flush(); assert.equal(stepOf(browser).marked, "Flagged");
});

test("undo uncertainty stays recoverable after the restored entity disappears", async () => {
  const browser = fixture({ hash: "#/review/w/1", capabilities: { entity_restore: true } });
  browser.data["/api/guide"] = working({ added: 1 }, [exactChanged(2, "newEntity", { change: "added" })]);
  browser.data["POST /api/workspace/entity/restore"] = () => { browser.data["/api/guide"] = guide(); throw new Error("The answer was lost after undoing it"); };
  await flush(); browser.node("review-undo").click(); browser.node("review-undo-confirm").click(); await flush();
  browser.button("Read the work again").click(); await flush();
  assert.match(browser.text(), /That entity is not in this review/);
  assert.match(browser.text(), /undo's outcome is unknown/);
  assert.ok(browser.button("I've checked"), "recovery belongs to the review, not to a surviving step");
  browser.navigate("uncommitted");
  assert.match(browser.node("work-restore").textContent, /undo's outcome is unknown/);
  browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(3, "laterEntity")]);
  browser.refresh(); await flush();
  assert.equal(browser.button("Commit").disabled, true, "the recovery message explains the visible lockout");
  browser.button("I've checked").click();
  assert.equal(browser.button("Commit").disabled, false);
  assert.equal(browser.node("work-restore").textContent, "");
  assert.equal(browser.count("/api/workspace/entity/restore"), 1, "recovery never replays the undo");
});

test("copied flags do not infer where an unattributed edit happened", async () => {
  const browser = fixture({ hash: "#/review/w/1" });
  browser.data["/api/guide"] = working({ modified: 2, authors: [{ name: "Claude Code", entities: 1 }] }, [exactChanged(2, "first", { authored_by: [{ name: "Claude Code" }] }), exactChanged(3, "second")]);
  await flush(); await mark(browser, "flag", "flag");
  browser.button("Copy the flags").click(); await flush();
  assert.match(browser.copied[0], /Claude Code:\nfirst/);
  assert.match(browser.copied[0], /No agent reported this edit:\nsecond/);
  assert.doesNotMatch(browser.copied[0], /outside Kin|you changed/);
});

test("a selected history review never presents the working guide while its own read is pending", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = { ...working(FOUR, fourChanged()), change: { base: "change-1", title: "Uncommitted work", author: "Current worker" } };
  await flush();
  const changeId = "c".repeat(64);
  browser.data["/api/guide?change"] = () => ({ ...recorded(changeId, { modified: 2 }, [changed(2, "keptFirst"), changed(3, "keptSecond")]), change: { id: changeId, title: "Selected change", author: "Original author", timestamp: secondsAgo(600) } });
  let release; browser.held.set("/api/guide?change", new Promise((done) => { release = done; }));
  browser.navigate("history"); browser.navigate(`review/c/${changeId}`);
  assert.doesNotMatch(browser.node("review-head").textContent, /Uncommitted work|Current worker|Four functions/);
  release(); await flush();
  assert.equal(browser.node("review-head").querySelector("h1").textContent, "Selected change");
  assert.match(browser.node("review-head").textContent, /Original author/);
  assert.match(browser.node("review-head").textContent, /Two functions changed/);
  browser.data["/api/guide"] = { ...working({ modified: 3 }, [changed(30, "newWorking")]), change: { title: "Later uncommitted work", author: "Later worker" } };
  browser.refresh(); await flush();
  assert.equal(browser.node("review-head").querySelector("h1").textContent, "Selected change");
  assert.doesNotMatch(browser.node("review-head").textContent, /Later worker|newWorking/);
});

test("graph-negative claims describe what Kin found and incomplete counts remain lower bounds", async () => {
  const browser = fixture();
  browser.data["/api/guide"] = working({ modified: 1, recorded_consumers: 3, recorded_tests: 1, completeness: { state: "settled", unresolved_entities: 2 } }, [changed(2, "want_bytes", { connections: connected([relying(10, "caller")], [relying(20, "testIt")], { consumers_total: 3 }) })]);
  await flush();
  assert.match(browser.node("work-lede").textContent, /At least three other places/);
  assert.match(browser.node("work-lede").textContent, /At least one test reaches/);
  assert.doesNotMatch(browser.node("work-lede").textContent, /still mapping/, "unproven connections are not promised to resolve by waiting");
  browser.data["/api/guide"] = working({ modified: 1, completeness: { state: "not_settled", pending_entities: 2 } }, [changed(2, "want_bytes")]);
  browser.refresh(); await flush();
  assert.match(browser.node("work-lede").textContent, /Kin found no connections outside this change/);
  assert.match(browser.node("work-lede").textContent, /Kin found no tests that reach this work/);
  assert.doesNotMatch(browser.node("work-lede").textContent, /No test reaches|Nothing outside/);
});

test("an entity count without listed names is disclosed instead of claiming no connections", async () => {
  const browser = fixture({ hash: `#/entity/${uuid(9)}` });
  browser.data["/api/workspace/entity"] = () => entityV2(uuid(9), { header: { name: "held", kind: "function", dependents: 1 }, relied_on_by: { ok: true, items: [], total: 1, relies_total: 1, bound: "at_least" } });
  await flush();
  assert.match(connections(browser).left.textContent, /What relies on it≥1/);
  assert.match(connections(browser).left.textContent, /1 more that Kin counted and did not list/);
  assert.doesNotMatch(connections(browser).left.textContent, /found no entities/);
});

test("deeper entities sharing a name disclose their kinds", async () => {
  const browser = fixture({ hash: `#/entity/${uuid(9)}` });
  browser.data["/api/workspace/entity"] = () => entityV2(uuid(9), { impact: { ok: true, transitive: 2, tests: 0, depth: 3, bound: "at_least", top: [{ id: uuid(31), name: "fastify", kind: "function", hops: 3 }, { id: uuid(32), name: "fastify", kind: "module", hops: 3 }] } });
  await flush();
  const rows = browser.node("entity-impact").querySelectorAll(".relier").filter((row) => row.textContent.includes("fastify"));
  assert.equal(rows.length, 2);
  assert.match(rows[0].textContent, /Function.*3 steps away/);
  assert.match(rows[1].textContent, /Module.*3 steps away/);
});

test("working review repaints after returning from a historical review through an entity", async () => {
  const browser = await reviewingChange("#/review/c/change-1/1");
  browser.data["/api/workspace/entity"] = () => entityV2(uuid(9));
  browser.navigate(`entity/${uuid(9)}`); await flush();
  browser.navigate("review/w/1"); await flush();
  assert.ok(browser.node("review-fine"));
  assert.equal(stepOf(browser).position, "1 of 1");
  assert.match(browser.node("review-head").textContent, /Uncommitted/);
});

test("outgoing connection counts retain lower-bound status in visible and accessible text", async () => {
  const browser = fixture({ hash: `#/entity/${uuid(9)}` });
  browser.data["/api/workspace/entity"] = () => entityV2(uuid(9), { relies_on: { ok: true, items: [], total: 3, bound: "at_least" } });
  await flush();
  assert.match(connections(browser).right.textContent, /What it relies on≥3/);
  assert.match(connections(browser).right.textContent, /3 more that Kin counted and did not list/);
  assert.match(connections(browser).host.getAttribute("aria-label"), /it relies on at least 3 entities/);
});

// ── What a real run through an agent's work found ──────────────────────────

test("Home lists every agent change nobody has reviewed beside uncommitted work, each with its review", async () => {
  const hub = fixture({ hub: true });
  const agent = { name: "Claude Code", key: "00000000000000a1" };
  // Uncommitted work, and under it a person's commit made on top of two agent commits, one of them reviewed.
  hub.data[under(KEY, "/api/workspace/overview")] = summaryOf({ recent: [
    { id: "p-3", title: "Trim the request id", author: "Dana Reader", time: secondsAgo(60), kin_checkpoint: false },
    { id: "a-2", title: "Prefix request ids", author: "Claude Code", agent, time: secondsAgo(600), kin_checkpoint: false },
    { id: "a-1", title: "Bound the header", author: "Claude Code", agent, time: secondsAgo(900), kin_checkpoint: false }] });
  hub.data[under(KEY, "/api/standing")] = { standing: { "a-1": { state: "approved" } } };
  // A clean repository whose agent committed four changes nobody has reviewed.
  hub.data[under(NOTES, "/api/workspace/overview")] = summaryOf({ recent: [1, 2, 3, 4].map((n) => ({ id: `n-${n}`, title: `Note change ${n}`, author: "Claude Code", agent, time: secondsAgo(n * 60), kin_checkpoint: false })) });
  hub.data[under(NOTES, "/api/standing")] = { standing: {} };
  await flush(); await flush();
  const cards = hub.node("home-attention").querySelectorAll("article");
  const fixtureCard = cards.find((card) => card.textContent.includes("Fixture"));
  assert.match(fixtureCard.querySelector(".focus-kind").textContent, /^Uncommitted$/, "uncommitted work keeps its card");
  const agentRows = (card) => card.querySelector(".focus-agents").querySelectorAll("li").map((row) => [row.querySelector(".change-title").textContent, row.querySelector("a").textContent, row.querySelector("a").getAttribute("href")]);
  assert.deepEqual(agentRows(fixtureCard), [["Prefix request ids", "Review", `${URL_OF(KEY)}${guided("a-2")}`]], "the agent's change under the person's is still waiting, and the reviewed one is not");
  const notesCard = cards.find((card) => card.textContent.includes("notes"));
  assert.equal(notesCard.querySelector(".focus-kind").textContent, "Agent commits");
  assert.equal(notesCard.querySelector(".focus-sentence").textContent, "Claude Code committed four changes nobody has reviewed yet.");
  assert.deepEqual(agentRows(notesCard).map(([title, , href]) => [title, href]), [1, 2, 3].map((n) => [`Note change ${n}`, `${URL_OF(NOTES)}${guided(`n-${n}`)}`]), "the newest three, so Home stays short");
  assert.match(notesCard.textContent, /1 more in History/);
  assert.match(hub.node("home-headline").textContent, /^Two repositories need you\./);
});

test("a flag on a recorded change reaches agents only while the entity is still what the change left, and says so when it is not", async () => {
  const kept = new Map();
  const browser = fixture({ hash: "#/review/c/change-1/1", capabilities: { review_flags: true }, kept });
  browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { modified: 2 }, [exactChanged(2, "want_bytes"), exactChanged(7, "quiet", { after: { state: "recorded", text: "def quiet():\n    return 1\n", body_hash: "c".repeat(64) } })]);
  browser.data["/api/workspace/flags"] = { flags: [] };
  // Kin's reading of each entity now: want_bytes is as the change left it, quiet changed since.
  browser.data["/api/workspace/entity"] = (url) => ({ id: url.searchParams.get("id"), source: { ok: true, text: "def x():\n    return 1\n", body_hash: url.searchParams.get("id") === uuid(2) ? "a".repeat(64) : "d".repeat(64) } });
  browser.data["POST /api/workspace/flags"] = (_, options) => {
    const body = JSON.parse(options.body);
    browser.data["/api/workspace/flags"] = { flags: [{ entity_id: body.entity, fingerprint: body.fingerprint, seen_by: [], changed_since: false }] };
    return { outcome: "flagged" };
  };
  browser.data["POST /api/workspace/flags/resolve"] = { outcome: "resolved" };
  await flush();
  await flagWith(browser, "The callers still pass one argument."); await browser.advance(1);
  assert.deepEqual(browser.sent("/api/workspace/flags").map((body) => [body.entity, body.note, body.fingerprint]), [[uuid(2), "The callers still pass one argument.", `source:${"a".repeat(64)}`]], "shared through the same durable route as a flag on uncommitted work");
  await mark(browser, "flag"); await browser.advance(1);
  assert.equal(browser.sent("/api/workspace/flags").length, 1, "quiet changed since, so no flag about a gone version is sent");
  assert.equal(browser.location.hash, "#/review/c/change-1/done");
  const body = browser.node("review-body");
  const states = body.querySelectorAll("li").map((row) => [row.querySelector(".cell-main").textContent, row.querySelectorAll(".cell").at(-1).textContent]);
  assert.deepEqual(states, [["want_bytes", "No agent has seen it yet"], ["quiet", "Not shared: it changed since"]]);
  assert.match(body.textContent, /quiet changed after this change, so the flag was not shared: an agent would be told about a version that is gone\./);
  assert.match(body.textContent, /An agent is told about each flag above that was shared, the next time it touches that entity\./);
  // A reload forgets what this page sent; with no flag on that version in the repository, the row says the flag stayed here.
  const reloaded = fixture({ hash: "#/review/c/change-1/done", capabilities: { review_flags: true }, kept });
  reloaded.data["/api/guide?change"] = browser.data["/api/guide?change"];
  reloaded.data["/api/workspace/flags"] = browser.data["/api/workspace/flags"];
  await flush();
  const rowsAfter = reloaded.node("review-body").querySelectorAll("li").map((row) => [row.querySelector(".cell-main").textContent, row.querySelectorAll(".cell").at(-1).textContent]);
  assert.deepEqual(rowsAfter, [["want_bytes", "No agent has seen it yet"], ["quiet", "Kept in this browser"]]);
  // Looking again and finding it right closes the flag that was shared.
  browser.navigate("review/c/change-1/1"); await flush(); await mark(browser, "fine"); await browser.advance(1);
  assert.equal(browser.count("/api/workspace/flags/resolve"), 1);
});

test("saying a recorded change needs work shares the flags this page had not shared yet", async () => {
  const kept = new Map([[marksKey("change-1"), JSON.stringify({ fine: [], flagged: [`${uuid(2)}:${"a".repeat(64)}`], notes: { [`${uuid(2)}:${"a".repeat(64)}`]: "Check the callers." } })]]);
  const browser = fixture({ hash: "#/review/c/change-1/done", capabilities: { review_flags: true }, kept });
  browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { modified: 1 }, [exactChanged(2, "want_bytes")]);
  browser.data["/api/workspace/flags"] = { flags: [] };
  browser.data["/api/workspace/entity"] = (url) => ({ id: url.searchParams.get("id"), source: { ok: true, text: "def x():\n    return 1\n", body_hash: "a".repeat(64) } });
  browser.data["POST /api/workspace/flags"] = { outcome: "flagged" };
  browser.data["/api/decide"] = { reviews: [] };
  await flush();
  browser.button("Flag it").click(); await flush(); await browser.advance(1);
  assert.deepEqual(browser.sent("/api/decide").map((body) => body.state), ["needs_work"]);
  assert.deepEqual(browser.sent("/api/workspace/flags").map((body) => [body.entity, body.note]), [[uuid(2), "Check the callers."]]);
});

test("committing what looks right says what was committed, as which change, and what stays, where it was taken", async () => {
  const browser = await reviewing("#/review/w/1", { capabilities: { entity_commit: true } });
  await mark(browser, "fine", "flag");
  browser.navigate("review/w/done"); await flush();
  browser.button("Commit anyway").click(); await flush();
  const choice = browser.node("commit-only-fine").parentNode.textContent;
  assert.equal(choice, "Commit only the one that looks right. The one you flagged and the two you did not mark stay uncommitted.", "both what was flagged and what was not marked are counted");
  const change = "d".repeat(64);
  browser.data["/api/workspace/commit"] = (url, options) => {
    const sent = JSON.parse(options.body);
    browser.data["/api/guide"] = working({ added: 1, modified: 1, removed: 1 }, fourChanged().filter((step) => step.entity_id !== uuid(2)));
    return { operation_id: sent.operation_id, outcome: "committed", change_id: change, title: sent.message, entities: sent.entities };
  };
  writeCommit(browser, "Bound want_bytes");
  browser.node("commit-submit").click(); await flush();
  assert.deepEqual(browser.sent("/api/workspace/commit").map((body) => body.entities), [[uuid(2)]]);
  const body = browser.node("review-body");
  assert.match(body.textContent, /^Committedwant_bytes is committed as change dddddddd, “Bound want_bytes”\. 3 entities stay uncommitted\./, "the receipt leads the page the commit was made on");
  assert.equal(body.querySelector("code").textContent, "dddddddd");
  assert.equal(body.querySelectorAll("a").find((node) => node.textContent === "Review it").getAttribute("href"), guided(change));
});

test("an undo says what it put back, and the walk moves on instead of losing its place", async () => {
  const browser = fixture({ hash: "#/review/w/2", capabilities: { entity_restore: true } });
  browser.data["/api/guide"] = working({ modified: 2 }, [exactChanged(2, "want_bytes"), exactChanged(5, "base64_decode")]);
  await flush();
  assert.equal(stepOf(browser).article.getAttribute("aria-label"), "base64_decode");
  browser.node("review-undo").click();
  browser.data["POST /api/workspace/entity/restore"] = () => { browser.data["/api/guide"] = working({ modified: 1 }, [exactChanged(2, "want_bytes")]); return { outcome: "restored", entity: uuid(5), name: "base64_decode", to: "c".repeat(64), replayed: false }; };
  browser.node("review-undo-confirm").click(); await flush(); await flush();
  assert.equal(browser.location.hash, "#/review/w/done", "the last page is gone, so the walk ends where it can");
  await flush();
  const said = browser.node("restore-receipt").textContent;
  assert.match(said, /^Undid the change to base64_decodebase64_decode is back as it was at change cccccccc\. The rest of the work stays as it is\./);
  assert.doesNotMatch(browser.node("review-body").textContent, /That entity is not in this review/);
  browser.navigate("uncommitted"); await flush();
  assert.match(browser.node("work-restore").textContent, /Undid the change to base64_decode/, "Uncommitted says it too");
  browser.button("Dismiss").click();
  assert.equal(browser.node("work-restore").textContent, "");
});

test("tests that reach an entity show how far each is, open in place, and a search with a reach never reads as a bare zero", async () => {
  const tests = Array.from({ length: 10 }, (_, i) => relying(60 + i, `TestReach.test_${i}`, { relation: "tests", ...(i % 2 ? { hops: 3 } : {}) }));
  const browser = fixture({ hash: "#/review/w/1" });
  browser.data["/api/guide"] = working({ modified: 2, recorded_tests: 12 }, [
    changed(2, "want_bytes", { connections: connected([], tests, { tests_total: 12, tests_depth: 3 }) }),
    changed(7, "quiet", { connections: connected([], [], { tests_total: 0, tests_depth: 3 }) })]);
  await flush();
  let step = stepOf(browser);
  assert.equal(step.tests.querySelector("h2").textContent, "Tests that reach it12");
  const rows = () => step.tests.querySelectorAll("li").map((row) => row.textContent);
  assert.deepEqual(rows().slice(0, 2), ["TestReach.test_01 step away", "TestReach.test_13 steps away"], "an omitted distance is one step, and a far one says how far");
  assert.equal(rows().length, 9);
  assert.equal(rows()[8], "Show 2 more of the 12");
  browser.node("step-tests-more").click();
  step = stepOf(browser);
  assert.equal(rows().length, 11);
  assert.equal(rows()[10], "Kin lists 10 of the 12 here.");
  browser.navigate("review/w/2"); await flush();
  step = stepOf(browser);
  assert.equal(step.tests.querySelector("h2").textContent, "Tests that reach it", "no bare zero");
  assert.equal(step.tests.querySelector("p").textContent, "No test found within 3 steps of it.");
  assert.match(step.verdict, /No test found within 3 steps of it\.$/);
  const clean = fixture();
  clean.data["/api/guide"] = working({ modified: 1 }, [changed(7, "quiet", { connections: connected([], [], { tests_total: 0, tests_depth: 3 }) })]);
  clean.refresh(); await flush();
  assert.match(clean.node("work-lede").textContent, /No test found within 3 steps of it\./);
});

test("tests that reach an entity say within how many steps, how each reaches it, and where same-named ones sit", async () => {
  // Rows as the guide orders them: nearest first, and at one distance the ones that reach by calls first.
  const testRow = (n, name, fields = {}, within) => relying(n, name, { relation: "references", ...fields, entity: { id: uuid(n), name, kind: "module", role: "test", ...(within ? { within } : {}) } });
  const tests = [
    testRow(80, "helper.test", { relation: "calls", call_hops: 1 }),
    testRow(81, "string.test", { relation: "calls", hops: 2, call_hops: 2 }),
    testRow(82, "index.test", { relation: "uses_type", hops: 3, call_hops: 1 }, "namespace classic.tests"),
    testRow(83, "index.test", { hops: 3 }, "namespace mini.tests"),
    testRow(84, "apply.test", { hops: 3 })];
  const browser = fixture({ hash: "#/review/w/1" });
  browser.data["/api/guide"] = working({ modified: 1, recorded_tests: 5, tests_depth: 3 }, [
    changed(2, "_toLowerCase", { connections: connected([], tests, { tests_total: 5, tests_depth: 3 }) })]);
  await flush();
  const step = stepOf(browser);
  assert.match(step.verdict, /5 tests reach it within 3 steps\.$/, "the count says how far Kin looked");
  assert.deepEqual(step.rows(step.tests), [
    "helper.test1 step away, calls it",
    "string.test2 steps away, through calls",
    "index.testin classic.tests3 steps away",
    "index.testin mini.tests3 steps away",
    "apply.test3 steps away"], "same-named tests say where each sits, and a route of calls alone says so");
  browser.navigate("uncommitted"); await flush();
  assert.match(browser.node("work-lede").textContent, /Five tests reach it within 3 steps\./);
});

test("documentation Kin only re-read is one quiet line and never a step, and a documentation edit shows what it said before and after", async () => {
  const browser = fixture({ hash: "#/review/c/change-9" });
  const documented = changed(3, "$ZodStringBoolParams.case", { kind: "method", fields: [{ field: "doc_summary", before: "Options: sensitive", after: "Options: sensitive or insensitive" }] });
  browser.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"),
    { modified: 2, documentation_rereads: 1, documentation_reread_names: ["$ZodStringPrefix.case"] }, [changed(2, "_toLowerCase"), documented]);
  await flush();
  assert.equal(browser.node("review-rereads").textContent, "Kin re-read the documentation of $ZodStringPrefix.case; the code is identical.");
  assert.doesNotMatch(browser.node("review-body").textContent, /\$ZodStringPrefix/, "what was only re-read is not a step");
  browser.navigate("review/c/change-9/2"); await flush();
  const step = stepOf(browser);
  assert.equal(step.article.getAttribute("aria-label"), "$ZodStringBoolParams.case");
  assert.match(step.verdict, /^Its documentation changed\. Its code reads the same token for token\./, "the guide's own reading is said, never a bare \"It changed\"");
  assert.match(step.source.textContent, /Documentation before“?Options: sensitive”?Documentation after“?Options: sensitive or insensitive”?/);
  const many = fixture({ hash: "#/review/c/change-8" });
  many.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { modified: 1, documentation_rereads: 14, documentation_reread_names: ["a", "b"] }, [changed(2, "_toLowerCase")]);
  await flush();
  assert.equal(many.node("review-rereads").textContent, "Kin re-read the documentation of 14 entities; their code is identical.");
  const none = fixture({ hash: "#/review/c/change-7" });
  none.data["/api/guide?change"] = (url) => recorded(url.searchParams.get("change"), { modified: 1 }, [changed(2, "_toLowerCase")]);
  await flush();
  assert.equal(none.node("review-rereads"), null);
});

test("a branch says how far it is ahead of and behind the current one, and is even only when both are counted and none", async () => {
  const browser = fixture({ hash: "#/branches" });
  browser.data["/api/branches"] = { branches: [{ name: "main", active: true, default: true }, { name: "behind", active: false }, { name: "both", active: false }, { name: "older", active: false }] };
  const ranges = { behind: [0, 2], both: [1, 2], older: [0, undefined] };
  browser.data["/api/guide?base"] = (url) => { const [ahead, behind] = ranges[url.searchParams.get("change")]; return guide(ahead ? { modified: 1 } : {}, { chapters: ahead ? [{ steps: [{ kind: "function" }] }] : [], range: { base_ref: "main", head_ref: url.searchParams.get("change"), changes_total: ahead, ...(behind === undefined ? {} : { base_changes_total: behind }), changes: [] } }); };
  await flush(); await flush();
  const said = browser.node("branches-list").textContent;
  assert.match(said, /behindNo work of its own, 2 changes behind main/);
  assert.match(said, /both.*1 change ahead of main, 2 behind/);
  assert.match(said, /olderNo work of its own beyond main/, "a server that did not count the other side is not read as even");
  assert.doesNotMatch(said, /even with main/);
  assert.deepEqual([branchActions(browser, "behind"), branchOpens(browser, "behind")], [["Switch"], null], "nothing in it to review or merge");
  // What differs from a branch with no work of its own is only where the current branch moved on, which is not this branch's to say.
  assert.doesNotMatch(branchRow(browser, "behind").textContent, /changed\./);
});

test("a switch is one state at a time: the preview names what comes along in plain words, and the landed switch never sits beside the branch it left", async () => {
  const browser = fixture();
  browser.data["/api/workspace/branch/preview"] = { ...browser.data["/api/workspace/branch/preview"], pending_work: { present: true, detail: "Compatible pending work will be carried to the destination; work already present there is retained." },
    limitations: ["This preview reads recorded repository state and does not admit or inspect unrecorded working-copy changes.", "Confirmation may change the working copy. It rechecks exact authority and uses the existing pending-work carry, projection and admission-policy checks; a preview does not guarantee success."] };
  await flush(); await branchPreview(browser);
  const preview = browser.main().querySelector(".preview");
  const visible = (node) => node.children.filter((child) => child.tagName !== "DETAILS").map((child) => child.tagName === "DIV" || child.tagName === "P" || child.tagName === "H3" || child.tagName === "LABEL" ? child.textContent : visible(child)).join(" ");
  const words = visible(preview);
  assert.match(words, /Switch from main to feature/);
  assert.match(words, /Your uncommitted change to want_bytes comes along to feature\./, "the entities that come along are named");
  assert.doesNotMatch(words, /admit|authority|projection|admission|pending-work|destination/, "Kin's own words stay behind Show Kin's own words");
  assert.match(preview.querySelector("details").textContent, /^Show Kin's own words.*admission-policy/s);
  assert.equal(browser.node("workspace-branch-submit").textContent, "Switch to feature");
  // The switch lands while the branch lists are still being read again.
  let release; browser.held.set("/api/workspace/branches", new Promise((done) => { release = done; }));
  browser.held.set("/api/branches", new Promise((done) => { release = ((first) => () => { first(); done(); })(release); }));
  browser.data["/api/branches"] = { branches: [{ name: "main", active: false, default: true }, { name: "feature", active: true }] };
  browser.data["/api/workspace/branches"] = { schema: "kin.branch-list.v1", branches: [{ name: refWire("main"), active: false }, { name: refWire("feature"), active: true }] };
  branchConfirm(browser); await flush();
  const panel = () => browser.node("workspace-branch-host").textContent;
  assert.match(panel(), /Switched to featureReading feature\. Your uncommitted change to want_bytes came along\./);
  assert.doesNotMatch(panel(), /only branch|not available|Preview what a switch/);
  assert.doesNotMatch(browser.text(), /You're on main/, "the lede does not keep the branch that was left");
  release(); await flush(); await flush();
  assert.match(panel(), /You're on feature now\./);
  assert.match(browser.node("branches-lede").textContent, /^You're on feature\./);
});

test("a created branch keeps where it started, offers the switch as a button, and the remote's reason is said once", async () => {
  const browser = fixture({ hash: "#/branches", capabilities: { branch_create: true } });
  const reason = "Kin holds 1 of this repository's 4,673 commits, so it cannot be pushed or pulled until it holds all of it.";
  browser.data["/api/workspace/remotes"] = { remotes: [{ name: "origin", kind: "git", can_pull: false, can_push: false, unavailable_reason: reason, pull_unavailable_reason: reason, standing: { state: "diverged" } }] };
  await flush();
  assert.equal(browser.node("branches-remotes").textContent.split(reason).length - 1, 1);
  browser.button("New branch").click();
  browser.type("branch-new-name", "feat/x");
  browser.data["/api/workspace/branch/create"] = () => {
    browser.data["/api/branches"] = { branches: [...browser.data["/api/branches"].branches, { name: "feat/x", active: false }] };
    browser.data["/api/workspace/branches"] = { schema: "kin.branch-list.v1", branches: [...browser.data["/api/workspace/branches"].branches, { name: refWire("feat/x"), active: false }] };
    return { outcome: "created", name: "feat/x" };
  };
  newBranch(browser).emit("submit"); await flush();
  assert.match(browser.node("branches-new").textContent, /^Created feat\/xIt starts at main's latest change\. You're still on main\./);
  assert.ok(browser.button("Switch to feat/x"), "the next step is a button, not an instruction");
  // On the new branch the receipt still says where it started, never that it starts at itself.
  browser.data["/api/branches"] = { branches: [{ name: "main", active: false, default: true }, { name: "feature", active: false }, { name: "feat/x", active: true }] };
  browser.refresh(); await flush();
  assert.match(browser.node("branches-new").textContent, /You're on it now\. It started at main's latest change\./);
  assert.doesNotMatch(browser.node("branches-new").textContent, /feat\/x's latest change/);
});

test("small words hold together: a long title wraps, a branch name stays whole, counts are numerals, and Kin's record start is its own line", async () => {
  const css = fs.readFileSync(path.join(assets, "style.css"), "utf8");
  const rule = (selector) => css.split("\n").find((line) => line.startsWith(`${selector} {`) && line.endsWith("}"))?.slice(selector.length + 2, -1) ?? "";
  // A step's title is its entity's name, which wraps on a narrow window rather than losing its end.
  assert.match(rule(".step-name .ent"), /white-space: normal/);
  assert.match(rule(".step-name .ent"), /overflow-wrap: anywhere/);
  assert.doesNotMatch(rule(".step-name .ent"), /ellipsis/);
  // A branch name in a sentence is one word to the reader.
  assert.match(rule(".nobreak"), /white-space: nowrap/);
  // A test row told apart by a long place wraps rather than pushing its result off a narrow window.
  assert.match(css, /\.rows-item:has\(\.name-qualifier\) \{ flex-wrap: wrap;/);
  let browser = fixture({ hash: "#/branches" });
  browser.data["/api/branches"] = { branches: [{ name: "main", active: true, default: true }, { name: "feat/payload-size-helper", active: false }, { name: "feat/workflow-driver", active: false }] };
  await flush();
  assert.deepEqual(browser.node("branches-lede").querySelectorAll(".nobreak").map((node) => node.textContent), ["main", "feat/payload-size-helper", "feat/workflow-driver"]);
  assert.equal(browser.node("branches-lede").textContent, "You're on main. Two other branches: feat/payload-size-helper and feat/workflow-driver.");
  // Comparing names the branch you are on as that, never as Git's word for it.
  browser.node("branches-compare-toggle").click(); await flush();
  assert.deepEqual([browser.node("workspace-range-head").value, browser.node("workspace-range-head").getAttribute("placeholder")], ["", "The current branch"]);
  browser.node("workspace-range-base").value = "feat/workflow-driver";
  browser.node("workspace-range-base").closest("form").emit("submit"); await flush();
  assert.equal(browser.location.hash, "#/review/r/feat%2Fworkflow-driver..main", "an empty To is the branch you are on");
  browser = fixture({ hash: "#/review/r/main..HEAD" }); await flush();
  assert.equal(browser.node("review-head").querySelector("h1").textContent, "The current branch against main");
  // A step's sentence says its counts as numerals, a lower bound included.
  browser = fixture({ hash: "#/review/w/1" });
  browser.data["/api/guide"] = working({ modified: 1, recorded_consumers: 1, recorded_tests: 3, consumers_may_need_review: 1, completeness: { state: "pending", pending_entities: 2 } }, [
    changed(2, "want_bytes", { connections: connected([relying(10, "Serializer.dumps")], [relying(20, "test_a", { relation: "tests" }), relying(21, "test_b", { relation: "tests" }), relying(22, "test_c", { relation: "tests" })]), consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 1 } })]);
  await flush();
  assert.equal(browser.main().querySelector(".step-verdict").textContent, "Its body changed. Its signature did not. At least 1 entity relies on it and did not change here. At least 3 tests reach it.");
  // Where Kin's record of a converted repository starts is said under its oldest change, on a line of its own, never as one more fact after a dot.
  browser = fixture({ hash: "#/history", session: { history: { state: "partial", admitted: 1, total: 4673 } } });
  await flush();
  const oldest = browser.node("changes-results").querySelectorAll("a.change-row").at(-1);
  assert.equal(oldest.querySelector(".change-start").textContent, "Kin's record starts here, with the repository as it was");
  assert.doesNotMatch(oldest.querySelector(".change-meta").textContent, /record starts here/);
});

test("the tabs scroll inside their own box, so a narrow window never grows sideways", () => {
  const css = fs.readFileSync(path.join(assets, "style.css"), "utf8");
  const rule = (selector) => css.split("\n").find((line) => line.startsWith(`${selector} {`) && line.endsWith("}"))?.slice(selector.length + 2, -1) ?? "";
  assert.match(rule(".tabs"), /overflow-x: auto/);
  assert.match(rule(".tabs"), /max-width: 100%/);
  assert.match(rule(".tab"), /flex: none/);
  assert.doesNotMatch(rule(".tab"), /margin-bottom: -/, "a tab's line is drawn inside the scroll box, where it is not clipped");
  assert.match(css, /\.app \{ grid-template-columns: minmax\(0, 1fr\); grid-template-rows: auto 1fr; \}/, "on a narrow window a short page never stretches the bar above it");
});

test("counts say at least where the summary says they can miss callers, and a search's page size is not a count of matches", async () => {
  const browser = fixture({ hash: "#/explore" });
  const overview = browser.data["/api/workspace/overview"];
  browser.data["/api/workspace/overview"] = { ...overview, counts: { ...overview.counts, relationships_bound: "at_least" }, most_relied_on: { ...overview.most_relied_on, bound: "at_least" } };
  await flush();
  assert.match(browser.node("explore-lede").textContent, /^216 entities and at least 602 connections\./);
  assert.deepEqual(browser.node("explore-list").querySelectorAll(".entity-side").map((node) => node.textContent), ["at least 23 rely on it", "at least 22 rely on it"]);
  const exact = fixture({ hash: "#/explore" }); await flush();
  assert.deepEqual(exact.node("explore-list").querySelectorAll(".entity-side").map((node) => node.textContent), ["23 rely on it", "22 rely on it"], "an unsaid bound is not guessed");
  // Twenty rows are the twenty asked for, not how many matched.
  const rows = Array.from({ length: 20 }, (_, i) => ({ id: uuid(100 + i), name: `match_${i}`, kind: "function", container: { id: uuid(1), name: "encoding", kind: "module" } }));
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "meaning_and_name", results: rows, total: 20 };
  browser.type("explore-input", "request id"); await browser.advance(300);
  assert.equal(browser.node("explore-results").querySelector(".result-head").textContent, "Best matchesThe closest 20");
  browser.data["/api/workspace/ask"] = { schema: "kin.workspace.ask.v1", mode: "name_only", results: rows, total: 20, names: { ok: true, total: 37 } };
  browser.type("explore-input", "match"); await browser.advance(300);
  assert.equal(browser.node("explore-results").querySelector(".result-head").textContent, "By name20 of 37");
  // Home's repository card says the same of its connections.
  const hub = fixture({ hub: true });
  hub.data[under(NOTES, "/api/workspace/overview")] = { ...summaryOf({ entities: 40, relationships: 51 }), counts: { ok: true, entities: 40, relationships: 51, relationships_bound: "at_least" } };
  await flush(); await flush();
  assert.deepEqual(repoCards(hub).find((card) => card.name === "notes").stats, ["40 entities", "≥51 connections"]);
});

test("a lower bound that starts a sentence is capitalized, and nothing is said to be at least zero", async () => {
  const browser = fixture({ hash: "#/review/w/1" });
  browser.data["/api/guide"] = working({ modified: 2, recorded_consumers: 1 }, [
    changed(2, "want_bytes", { connections: connected([relying(10, "Serializer.dumps")]), consumer_verdict: { verdict: "needs_no_update", reason: "comments_or_layout_only", consumers: 1 } }),
    changed(7, "quiet")], { summary: { added: 0, modified: 2, removed: 0, recorded_consumers: 1, recorded_tests: 0, consumers_not_updated: 0, completeness: { state: "not_settled", pending_entities: 2 } } });
  await flush();
  assert.match(stepOf(browser).verdict, /^Its body changed\. Its signature did not\. At least 1 entity relies on it and needs no update\./);
  assert.doesNotMatch(browser.text(), /\. at least/);
  const alone = fixture();
  alone.data["/api/guide"] = working({ modified: 1 }, [changed(7, "quiet")], { summary: { added: 0, modified: 1, removed: 0, recorded_consumers: 0, recorded_tests: 0, completeness: { state: "not_settled", pending_entities: 1 } } });
  alone.refresh(); await flush();
  assert.equal(picture(alone.node("work-flight")).label, "1 changed entity, and nothing Kin found relies on it from outside");
});

test("an uncommitted entity names the agent that reported it, and otherwise says no agent did, never guessing it is yours", async () => {
  const solo = fixture({ hash: "#/review/w/1" });
  // A server that lists no agent sessions has only seen none lately: that is no evidence of who edited.
  solo.data["/api/workspace/agents"] = agentsPayload({ agents: [], earlier: [] });
  solo.data["/api/guide"] = working({ modified: 1 }, [changed(2, "want_bytes")]);
  await flush();
  assert.equal(stepOf(solo).place, "Function in encoding · No agent reported this edit");
  solo.navigate("uncommitted"); await flush();
  assert.match(solo.node("work-lede").textContent, /No agent reported writing it, so Kin cannot say whether it is yours or an agent's\./);
  assert.doesNotMatch(solo.text(), /your edit|are yours\./);
  const shared = fixture();
  shared.data["/api/guide"] = working({ modified: 2 }, [changed(2, "want_bytes", { authored_by: [{ name: "Claude Code", session: "s", at: secondsAgo(5), through: "client" }] }), changed(7, "quiet")],
    { summary: { added: 0, modified: 2, removed: 0, recorded_consumers: 0, recorded_tests: 0, authors: [{ name: "Claude Code", session: "s", entities: 1, through: "client" }] } });
  shared.refresh(); await flush();
  assert.match(shared.node("work-lede").textContent, /Claude Code changed 1 entity\. No agent reported the other one\./);
  shared.navigate("review/w/1"); await flush();
  assert.equal(stepOf(shared).place, "Function in encoding · changed by Claude Code");
});

test("what relies on a changed entity is named by the relation and by what each entity is", async () => {
  const browser = fixture({ hash: "#/review/w/1" });
  const consumer = (n, name, kind, relation) => ({ ...relying(n, name, { relation }), entity: { id: uuid(n), name, kind, role: "source", within: "module route" } });
  browser.data["/api/guide"] = working({ modified: 1, recorded_consumers: 4, consumers_may_need_review: 4 }, [changed(2, "getGenReqId", {
    connections: connected([consumer(10, "route", "module", "uses_type"), consumer(11, "buildRouting", "function", "uses_type"), consumer(12, "fourOhFour", "function", "calls"), consumer(13, "fastify", "module", "imports")]),
    consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 4 } })]);
  await flush();
  const boxes = picture(stepOf(browser).relies).columns[1].boxes.map(([name, how]) => [name, how]);
  assert.deepEqual(boxes, [["route", "imports it"], ["buildRouting", "refers to it"], ["fourOhFour", "calls it"], ["fastify", "imports it"]], "a module that imports a function never uses its type");
  const typed = fixture({ hash: "#/review/w/1" });
  typed.data["/api/guide"] = working({ modified: 1, recorded_consumers: 1, consumers_may_need_review: 1 }, [changed(3, "Signer", { kind: "class", connections: connected([consumer(14, "make_signer", "function", "uses_type")]), consumer_verdict: { verdict: "may_need_review", reason: "body_changed", consumers: 1 } })]);
  await flush();
  assert.deepEqual(picture(stepOf(typed).relies).columns[1].boxes.map(([name, how]) => [name, how]), [["make_signer", "uses its type"]]);
});

test("Kin's drafted message names what the commit holds as the choice changes, and joins its clauses once", async () => {
  const browser = await reviewing("#/review/w/1", { capabilities: { entity_commit: true } });
  browser.navigate("review/w/2"); await flush();
  await mark(browser, "flag");
  browser.navigate("review/w/done"); await flush();
  browser.button("Commit anyway").click(); await flush();
  assert.equal(browser.node("commit-message").textContent, "Change want_bytes and quiet, add base64_encode and remove old_helper", "every entity, in one sentence with one final and");
  browser.navigate("review/w/1"); await flush(); await mark(browser, "fine");
  browser.navigate("review/w/done"); await flush();
  assert.equal(browser.node("commit-message").textContent, "Change want_bytes", "once the commit holds only what looks right, the draft says so");
  const toggle = browser.node("commit-only-fine"); toggle.checked = false; toggle.emit("change");
  assert.equal(browser.node("commit-message").textContent, "Change want_bytes and quiet, add base64_encode and remove old_helper");
  browser.type("commit-message", "My own words");
  const again = browser.node("commit-only-fine"); again.checked = true; again.emit("change");
  assert.equal(browser.node("commit-message").textContent, "My own words", "the reader's own message is never replaced");
  // Three names in one clause read "a, b and c", never "a and b and c".
  const three = fixture({ hash: "#/review/w/done" });
  three.data["/api/guide"] = working({ modified: 3 }, [changed(2, "want_bytes"), changed(5, "payload_size"), changed(7, "quiet")]);
  await flush();
  three.button("Commit anyway").click(); await flush();
  assert.equal(three.node("commit-message").textContent, "Change want_bytes, payload_size and quiet");
});

test("a chosen entity tab is brought inside the tab row's own scroll box", async () => {
  const browser = fixture({ hash: `#/entity/${uuid(9)}` });
  browser.data["/api/workspace/entity"] = () => entityV2(uuid(9));
  await flush();
  const history = browser.node("entity-tab-history");
  history.click();
  assert.equal(browser.document.scrolledTo, browser.node("entity-tab-history"));
  assert.equal(browser.document.activeElement, browser.node("entity-tab-history"));
});

test("an entity's history marks where Kin's record starts while the repository's history is partial", async () => {
  const id = uuid(9);
  const browser = fixture({ hash: `#/entity/${id}` });
  browser.data["/api/workspace/entity"] = () => entityV2(id, { history: { ok: true, items: [{ change_id: "import-1", title: "docs: clarify the parser", author: "Dana Reader", time: "2026-03-11T10:57:14Z" }], total: 1, shown: 1, record_start: "import-1" } });
  await flush();
  const text = browser.node("entity-history").textContent;
  assert.match(text, /Kin's record starts here, with the repository as it was/);
  assert.match(text, /earlier changes to this entity are not listed yet/);
  browser.data["/api/workspace/entity"] = () => entityV2(id);
  browser.refresh(); await flush();
  assert.doesNotMatch(browser.node("entity-history").textContent, /record starts here|not listed yet/, "a complete history carries no marker");
});

test("a pull still being checked says so, then appears once Kin knows it can work", async () => {
  const payload = { remotes: [{ name: "origin", kind: "git", can_pull: true, can_push: false, can_fetch: false, standing: { state: "behind", summary: "origin has 2 changes this branch does not." } }] };
  const browser = fixture({ hash: "#/branches", capabilities: { remote_pull: false, remote_pull_checking: true, remote_pull_unavailable_reason: "Kin is checking whether this branch can update from Git." } });
  browser.data["/api/workspace/remotes"] = payload;
  let answer = { remote_pull: false, remote_pull_unavailable_reason: "Kin is checking whether this branch can update from Git.", remote_pull_checking: true };
  browser.data["/api/workspace/session/pull"] = () => answer;
  await flush();
  assert.equal(browser.button("Pull"), undefined);
  assert.ok(browser.button("Checking…"), "the pull says it is being checked rather than disappearing");
  answer = { remote_pull: true, remote_pull_unavailable_reason: null, remote_pull_checking: false, remote_fetch: false };
  browser.refresh(); await flush();
  assert.ok(browser.button("Pull"), "the pull appears once the check answers");
  assert.ok(browser.count("/api/workspace/session/pull") >= 1);
});
