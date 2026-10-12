// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Run with: node --test scripts/review-ui.test.cjs
// Execute the complete browser entry point with controlled network responses and
// time. The tiny DOM supplies browser primitives, not application state or hooks.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const appPath = path.resolve(__dirname, "../crates/kin-cli/assets/review_ui/app.js");
const diffPath = path.resolve(__dirname, "../crates/kin-cli/assets/review_ui/diff.js");
const mapPath = path.resolve(__dirname, "../crates/kin-cli/assets/review_ui/map.js");
const stylePath = path.resolve(__dirname, "../crates/kin-cli/assets/review_ui/style.css");
const flush = () => new Promise(setImmediate);

class Element {
  constructor(tag, document) {
    this.tagName = tag.toUpperCase();
    this.ownerDocument = document;
    this.parentNode = null;
    this.childNodes = [];
    this.attributes = new Map();
    this.listeners = new Map();
    this.hidden = false;
    this.inert = false;
    this.disabled = false;
    this.value = "";
    this.dataset = {};
    this.classList = {
      contains: (name) => this.className.split(/\s+/).includes(name),
      toggle: (name, force) => {
        const names = new Set(this.className.split(/\s+/).filter(Boolean));
        const add = force === undefined ? !names.has(name) : force;
        if (add) names.add(name);
        else names.delete(name);
        this.className = [...names].join(" ");
        return add;
      },
      add: (...names) => names.forEach((name) => this.classList.toggle(name, true)),
      remove: (...names) => names.forEach((name) => this.classList.toggle(name, false)),
    };
  }
  get children() { return this.childNodes.filter((child) => child.tagName !== "#TEXT"); }
  get isConnected() { return this.ownerDocument.contains(this); }
  get tabIndex() {
    const explicit = this.getAttribute("tabindex");
    return explicit === null ? (/^(BUTTON|INPUT|SELECT|TEXTAREA)$/.test(this.tagName) || this.getAttribute("href") !== null ? 0 : -1) : Number(explicit);
  }
  get className() { return this.getAttribute("class") || ""; }
  set className(value) { this.setAttribute("class", value); }
  get textContent() { return this.tagName === "#TEXT" ? this.text : this.childNodes.map((child) => child.textContent).join(""); }
  set textContent(value) {
    if (this.tagName === "#TEXT") this.text = String(value);
    else this.replaceChildren(String(value));
  }
  setAttribute(name, value) {
    this.attributes.set(name, String(value));
    if (name === "hidden") this.hidden = true;
    if (name === "inert") this.inert = true;
    if (name === "disabled") this.disabled = true;
    if (name.startsWith("data-")) this.dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = String(value);
  }
  getAttribute(name) { return this.attributes.get(name) ?? null; }
  hasAttribute(name) { return this.attributes.has(name); }
  removeAttribute(name) {
    this.attributes.delete(name);
    if (name === "hidden") this.hidden = false;
    if (name === "inert") this.inert = false;
    if (name === "disabled") this.disabled = false;
  }
  append(...children) {
    for (const child of children) {
      const node = child instanceof Element ? child : this.ownerDocument.createTextNode(String(child));
      node.remove();
      node.parentNode = this;
      this.childNodes.push(node);
    }
  }
  replaceChildren(...children) {
    this.childNodes.forEach((child) => { child.parentNode = null; });
    this.childNodes = [];
    this.append(...children);
  }
  remove() {
    if (!this.parentNode) return;
    const siblings = this.parentNode.childNodes;
    siblings.splice(siblings.indexOf(this), 1);
    this.parentNode = null;
  }
  replaceWith(node) {
    const parent = this.parentNode;
    if (!parent) return;
    const index = parent.childNodes.indexOf(this);
    node.remove();
    parent.childNodes.splice(index, 1, node);
    node.parentNode = parent;
    this.parentNode = null;
  }
  matches(selector) {
    const tag = /^[a-z][\w-]*/i.exec(selector);
    if (tag && this.tagName !== tag[0].toUpperCase()) return false;
    for (const [, name] of selector.matchAll(/\.([\w-]+)/g)) if (!this.classList.contains(name)) return false;
    for (const [, name] of selector.matchAll(/#([\w-]+)/g)) if (this.getAttribute("id") !== name) return false;
    for (const [, name, value] of selector.matchAll(/\[([\w-]+)(?:="([^"]*)")?\]/g)) {
      if (value === undefined ? this.getAttribute(name) === null : this.getAttribute(name) !== value) return false;
    }
    return true;
  }
  querySelectorAll(selector) {
    const parts = selector.trim().split(/\s+/);
    const found = [];
    const visit = (node) => {
      for (const child of node.children) {
        if (child.matches(parts.at(-1))) {
          let ancestor = child.parentNode;
          let index = parts.length - 2;
          while (index >= 0 && ancestor) {
            if (ancestor.matches(parts[index])) index -= 1;
            ancestor = ancestor.parentNode;
          }
          if (index < 0) found.push(child);
        }
        visit(child);
      }
    };
    visit(this);
    return found;
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
  contains(node) { return node === this || this.childNodes.some((child) => child.contains(node)); }
  addEventListener(type, listener) {
    if (!this.listeners.has(type)) this.listeners.set(type, []);
    this.listeners.get(type).push(listener);
  }
  dispatchEvent(event) {
    event.target ??= this;
    event.preventDefault ??= () => { event.defaultPrevented = true; };
    event.stopPropagation ??= () => {};
    for (const listener of this.listeners.get(event.type) || []) listener(event);
  }
  click() { this.dispatchEvent({ type: "click" }); }
  focus() { this.ownerDocument.activeElement = this; }
  blur() { this.ownerDocument.activeElement = this.ownerDocument.body; }
  scrollIntoView() {}
}

function documentFixture(apiBase) {
  const document = new Element("document", null);
  document.ownerDocument = document;
  document.createElement = (tag) => new Element(tag, document);
  document.createTextNode = (text) => {
    const node = new Element("#text", document);
    node.textContent = text;
    return node;
  };
  document.getElementById = (id) => document.querySelector(`#${id}`);
  document.documentElement = document.createElement("html");
  document.body = document.createElement("body");
  if (apiBase) document.body.setAttribute("data-api-base", apiBase);
  document.append(document.documentElement);
  document.documentElement.append(document.body);
  document.activeElement = document.body;
  document.hidden = false;
  const add = (parent, tag, id) => {
    const node = document.createElement(tag);
    node.setAttribute("id", id);
    parent.append(node);
    return node;
  };
  const app = add(document.body, "div", "app");
  const rail = add(app, "aside", "rail");
  for (const id of ["repo", "theme", "editor", "filters", "changes", "live", "palette-open", "help-open"]) {
    add(rail, ["theme", "editor", "palette-open", "help-open"].includes(id) ? "button" : "div", id);
  }
  add(app, "main", "main").setAttribute("tabindex", "-1");
  add(app, "aside", "outline");
  for (const id of ["palette", "help"]) {
    const overlay = add(document.body, "div", id);
    overlay.hidden = true;
    const dialog = document.createElement("div");
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    overlay.append(dialog);
    if (id === "palette") {
      add(dialog, "input", "palette-input");
      add(dialog, "ul", "palette-list");
    } else {
      add(dialog, "dl", "keys-list");
      add(dialog, "button", "help-close");
    }
  }
  add(document.body, "div", "toasts");
  return document;
}

function clockFixture() {
  let now = Date.parse("2026-01-01T00:00:00Z");
  let nextId = 0;
  const tasks = new Map();
  const schedule = (run, delay, repeat) => {
    const id = ++nextId;
    tasks.set(id, { run, due: now + delay, delay, repeat });
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

const change = (id) => ({ id, title: `Change ${id}`, message: `Change ${id}`, author: "Reader", timestamp: "2026-01-01T00:00:00Z", entity_deltas: 11 });
function guide(kind, id = "ancestor") {
  return {
    kind,
    change: { ...change(id), base: "head" },
    chapters: [],
    reviews: [],
    summary: { added: 0, modified: 0, removed: 0, risk: "low", recorded_consumers: 0, recorded_tests: 0, completeness: { message: "Complete" } },
    findings: { breaking_changes: [], contract_violations: [], work_risks: [], test_coverage_gaps: [], notes: [] },
    ...(kind === "working" ? { working_tree: { differs: false, derived: true, caught_up: true } } : {}),
    ...(kind === "range" ? { range: { base_ref: "ancestor", head_ref: "HEAD", merge_base: "ancestor", changes_total: 1, changes: [change("head")] } } : {}),
  };
}

function browserFixture({ initial = "ancestor", history = ["head", "ancestor"], counts = {}, shapeGuide = (body) => body, repository = "Fixture", repositoryKey, storage = new Map(), apiBase = "", hash = "", withSteps = false, withSource = false, changeFields = {}, session = {}, branches = [] } = {}) {
  const document = documentFixture(apiBase);
  const clock = clockFixture();
  const queued = new Map();
  const requests = [];
  const network = {
    history,
    counts,
    changeFields,
    requests,
    asked: (name) => requests.filter((request) => request.name === name).map((request) => new URL(request.url, "http://local.test").searchParams),
    count: (name) => requests.filter((request) => request.name === name).length,
    active: (name) => requests.filter((request) => request.name === name && !request.done).length,
    hold(name) {
      let resolve, reject;
      const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
      if (!queued.has(name)) queued.set(name, []);
      queued.get(name).push(promise);
      return { resolve, reject };
    },
    async fetch(url, options = {}) {
      const parsed = new URL(url, "http://local.test");
      const requestPath = parsed.pathname.slice(apiBase.length);
      assert.equal(parsed.pathname, `${apiBase}${requestPath}`, "request uses the mounted repository API");
      assert.ok(requestPath.startsWith("/api/"), `unexpected API path: ${parsed.pathname}`);
      const name = requestPath === "/api/guide" ? (parsed.searchParams.has("working") ? "working" : "guide") : requestPath.slice(5);
      const request = { name, url, method: options.method || "GET", done: false };
      requests.push(request);
      try {
        const held = queued.get(name)?.shift();
        const override = held ? await held : undefined;
        let body = override;
        if (body === undefined) {
          if (name === "session") body = { repository, repository_key: repositoryKey, actor: "Reader", initial_change: initial, editor: "off", ...session };
          else if (name === "changes") {
            const count = Number(parsed.searchParams.get("count") || 60);
            const skip = Number(parsed.searchParams.get("skip") || 0);
            body = { changes: network.history.slice(skip, skip + count).map((id) => ({ ...change(id), ...(network.changeFields[id] || {}) })), truncated: skip + count < network.history.length };
          } else if (name === "counts") {
            const asked = (parsed.searchParams.get("changes") || "").split(",").filter(Boolean);
            body = { counts: Object.fromEntries(asked.filter((id) => network.counts[id]).map((id) => [id, network.counts[id]])) };
          }
          else if (name === "standing") body = { standing: {} };
          else if (name === "branches") body = { branches };
          else if (name === "working") body = guide("working", "WORKSPACE");
          else if (name === "guide") body = guide(parsed.searchParams.has("base") ? "range" : "change", parsed.searchParams.get("change") || "head");
          else if (name === "reviews") body = { reviews: [], reviews_total: 0 };
          else if (name === "change-agent") body = network.changeAgent || { agent: null };
          else throw new Error(`Unexpected request: ${url}`);
        }
        if ((withSteps || withSource) && (name === "guide" || name === "working")) {
          body = { ...body, chapters: [{ title: "Changed function", reason: "It changed", steps: [{
            entity_id: "entity-one", name: "example", kind: "function", change: "modified", risk: "low",
            narration: "The function changed.", fields: [], findings: [], evidence: { head_change: body.change.id },
            connections: { consumers: [], consumers_total: 0, tests: [], tests_total: 0 },
            ...(withSource ? {
              language: "javascript",
              before: { state: "recorded", text: "return 1;\n", change_id: "base" },
              after: { state: "recorded", text: "return 2;\n", change_id: body.change.id },
            } : {}),
          }] }] };
        }
        if (name === "guide" || name === "working") body = shapeGuide(body);
        return { ok: true, json: async () => body };
      } finally { request.done = true; }
    },
  };
  const window = new Element("window", document);
  window.scrollTo = () => {};
  window.scrollY = 0;
  window.matchMedia = () => ({ matches: false });
  const location = { hash };
  const context = vm.createContext({
    document, window, location, Node: Element, HTMLElement: Element,
    history: { replaceState: (_, __, hash) => { location.hash = hash; } },
    localStorage: { getItem: (key) => storage.get(key) ?? null, setItem: (key, value) => storage.set(key, value), removeItem: (key) => storage.delete(key) },
    navigator: { clipboard: { writeText: async () => {} } },
    kinReview: {},
    fetch: network.fetch,
    Date: clock.Date, setTimeout: clock.setTimeout, clearTimeout: clock.clearTimeout,
    setInterval: clock.setInterval, clearInterval: clock.clearInterval,
  });
  return {
    document, clock, network, location, storage,
    async start() {
      for (const script of [diffPath, mapPath, appPath]) vm.runInContext(fs.readFileSync(script, "utf8"), context, { filename: script });
      await flush();
    },
    async navigate(hash) { location.hash = hash; window.dispatchEvent({ type: "hashchange" }); await flush(); },
    key(key, options = {}) {
      const event = { type: "keydown", key, target: document.activeElement, ...options };
      document.dispatchEvent(event);
      return event;
    },
    sinceLink() { return document.getElementById("main").querySelectorAll("a").find((node) => node.textContent === "Everything since this change" && !node.hidden); },
    sinceAction() {
      document.getElementById("palette-open").click();
      const input = document.getElementById("palette-input");
      input.value = "everything since";
      input.dispatchEvent({ type: "input" });
      return document.getElementById("palette-list").querySelectorAll("li").find((node) => node.textContent.includes("Walk everything since this change"));
    },
  };
}

test("since-change shortcuts require an older change in the current ancestry", async () => {
  const browser = browserFixture({ initial: "divergent" });
  await browser.start();
  for (const [route, available] of [["#/c/divergent", false], ["#/c/head", false], ["#/c/ancestor", true], ["#/r/ancestor..", false], ["#/w", false]]) {
    await browser.navigate(route);
    assert.equal(Boolean(browser.sinceLink()), available, `header shortcut for ${route}`);
    assert.equal(Boolean(browser.sinceAction()), available, `palette shortcut for ${route}`);
    if (available) assert.equal(browser.sinceLink().getAttribute("href"), "#/r/ancestor..");
  }
  await browser.navigate("#/c/ancestor");
  browser.sinceAction().click();
  assert.equal(browser.location.hash, "#/r/ancestor..", "the palette opens the same range as the header");
});

test("history arriving after the guide adds and later removes the shortcut without reloading the guide", async () => {
  const browser = browserFixture();
  const history = browser.network.hold("changes");
  await browser.start();
  assert.equal(browser.document.getElementById("main").querySelector("h1").textContent, "Change ancestor");
  assert.equal(Boolean(browser.sinceLink()), false);
  assert.equal(Boolean(browser.sinceAction()), false);
  history.resolve();
  await flush();
  assert.ok(browser.sinceLink(), "the existing guide gets its shortcut when ancestry arrives");
  assert.ok(browser.sinceAction());
  assert.equal(browser.network.count("guide"), 1);

  browser.network.history = ["head", "other-ancestor"];
  await browser.clock.advance(5000);
  assert.equal(Boolean(browser.sinceLink()), false, "a changed ancestry withdraws the shortcut");
  assert.equal(Boolean(browser.sinceAction()), false);
  assert.equal(browser.network.count("guide"), 1);
});

test("an open palette follows ancestry changes and failed history reads", async () => {
  const browser = browserFixture();
  await browser.start();
  assert.ok(browser.sinceAction());
  const visibleAction = () => browser.document.getElementById("palette-list").querySelectorAll("li")
    .find((node) => node.textContent.includes("Walk everything since this change"));

  browser.network.history = ["other-head", "other-ancestor"];
  await browser.clock.advance(5000);
  assert.equal(browser.document.getElementById("palette").hidden, false);
  assert.equal(Boolean(visibleAction()), false, "the open palette withdraws a now-divergent range");

  browser.network.history = ["head", "ancestor"];
  await browser.clock.advance(5000);
  assert.ok(visibleAction(), "the current query gains the shortcut when ancestry confirms it again");

  const failure = browser.network.hold("changes");
  await browser.clock.advance(5000);
  failure.reject(new Error("Temporarily disconnected"));
  await flush();
  assert.equal(Boolean(visibleAction()), false, "a failed history read removes stale open actions");
  assert.equal(browser.document.getElementById("palette-input").value, "everything since", "refresh keeps the reader's query");
});

test("startup drains guide, history, standing, working tree and branches before polling", async () => {
  const browser = browserFixture();
  const held = Object.fromEntries(["guide", "changes", "standing", "working", "branches"].map((name) => [name, browser.network.hold(name)]));
  await browser.start();
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 1, "a slow startup must not start live polls");
  held.changes.resolve();
  await flush();
  assert.equal(browser.network.count("standing"), 1);
  for (const name of ["guide", "working", "branches"]) held[name].resolve();
  await flush();
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 1, "standing is part of startup too");
  held.standing.resolve();
  await flush();
  await browser.clock.advance(4999);
  assert.equal(browser.network.count("changes"), 1);
  await browser.clock.advance(1);
  assert.equal(browser.network.count("changes"), 2);
});

test("live polling waits for the complete slow cycle and resumes after a failed read", async () => {
  const browser = browserFixture();
  await browser.start();
  browser.network.history = ["new-head", "head", "ancestor"];
  const held = Object.fromEntries(["changes", "standing", "working", "branches"].map((name) => [name, browser.network.hold(name)]));
  await browser.clock.advance(5000);
  assert.equal(browser.network.active("changes"), 1);
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 2, "slow history cannot overlap another poll");
  held.changes.resolve();
  await flush();
  assert.equal(browser.network.active("standing"), 1);
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 2, "slow standing cannot overlap another poll");
  held.standing.resolve();
  await flush();
  assert.equal(browser.network.active("working"), 1, "a new head refreshes the working tree");
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 2, "the working read belongs to the same cycle");
  held.working.resolve();
  await flush();
  assert.equal(browser.network.active("branches"), 1);
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 2, "branch refresh belongs to the same cycle");
  held.branches.resolve();
  await flush();

  const failure = browser.network.hold("changes");
  await browser.clock.advance(4999);
  assert.equal(browser.network.count("changes"), 2);
  await browser.clock.advance(1);
  failure.reject(new Error("Temporarily disconnected"));
  await flush();
  assert.equal(browser.document.getElementById("live").textContent, "Not connected");
  assert.equal(Boolean(browser.sinceLink()), false, "failed history cannot leave a stale shortcut");
  await browser.clock.advance(4999);
  assert.equal(browser.network.count("changes"), 3);
  await browser.clock.advance(1);
  assert.equal(browser.network.count("changes"), 4, "the next cycle still runs after failure");
  assert.equal(browser.document.getElementById("live").textContent, "Connected");
  assert.ok(browser.sinceLink());
});

const mainOf = (browser) => browser.document.getElementById("main");
const buttonNamed = (root, text) => root.querySelectorAll("button").find((button) => button.textContent === text);
const fieldOf = (browser, name) => mainOf(browser).querySelector(`[name="${name}"]`);
function typeDraft(browser, name, value) {
  const field = fieldOf(browser, name);
  assert.ok(field, `the ${name} field is available`);
  field.value = value;
  field.dispatchEvent({ type: "input" });
}
async function nextWorkingPoll(browser) {
  for (let turn = 0; turn < 6 && !browser.network.active("working"); turn += 1) await browser.clock.advance(5000);
  assert.equal(browser.network.active("working"), 1, "the next working read has started");
}

test("a failed session can be retried in place and starts one polling loop", async () => {
  const browser = browserFixture();
  const failed = browser.network.hold("session");
  await browser.start();
  failed.reject(new Error("Session temporarily unavailable"));
  await flush();
  assert.match(mainOf(browser).textContent, /Session temporarily unavailable/);
  assert.match(mainOf(browser).querySelector(".problem-line").textContent, /^Kin can't reach this repository right now\.$/, "one plain line");
  const retry = buttonNamed(mainOf(browser), "Try again");
  assert.ok(retry, "startup failure offers a retry in the main pane");
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("changes"), 0);

  retry.click();
  await flush();
  assert.equal(browser.network.count("session"), 2);
  assert.equal(mainOf(browser).querySelector("h1").textContent, "Change ancestor");
  assert.equal(browser.network.count("changes"), 1);
  await browser.clock.advance(5000);
  assert.equal(browser.network.count("changes"), 2, "recovery starts one complete polling cycle");
  await browser.clock.advance(5000);
  assert.equal(browser.network.count("changes"), 3);
});

test("working read failure stays visible in main through successful history reads until working recovers", async () => {
  const browser = browserFixture({ hash: "#/w" });
  await browser.start();
  const heading = mainOf(browser).querySelector("h1");
  const failed = browser.network.hold("working");
  await nextWorkingPoll(browser);
  failed.reject(new Error("Working read interrupted"));
  await flush();
  const stale = () => mainOf(browser).querySelector("#working-read-status");
  assert.equal(mainOf(browser).querySelector("h1"), heading, "the last successful working guide stays readable");
  assert.ok(stale() && !stale().hidden, "staleness is visible outside the desktop rail");
  assert.match(stale().textContent, /Working read interrupted/);

  const recovery = browser.network.hold("working");
  const historyCount = browser.network.count("changes");
  await browser.clock.advance(5000);
  assert.equal(browser.network.count("changes"), historyCount + 1);
  assert.ok(stale() && !stale().hidden, "successful history cannot clear a working failure");
  assert.equal(browser.network.active("working"), 1);
  recovery.resolve();
  await flush();
  assert.ok(!stale() || stale().hidden || !stale().textContent, "a successful working read clears the warning");
});

test("retry working read recovers immediately without waiting for the timer", async () => {
  const browser = browserFixture({ hash: "#/w" });
  await browser.start();
  const failed = browser.network.hold("working");
  await nextWorkingPoll(browser);
  failed.reject(new Error("Working read interrupted"));
  await flush();
  const retry = buttonNamed(mainOf(browser), "Try again");
  assert.ok(retry);
  const before = browser.network.count("working");
  retry.click();
  await flush();
  assert.equal(browser.network.count("working"), before + 1);
  assert.equal(mainOf(browser).querySelector("h1").textContent, "Change WORKSPACE");
  const stale = mainOf(browser).querySelector("#working-read-status");
  assert.ok(!stale || stale.hidden || !stale.textContent);
});

test("changed working evidence waits while the reader searches and applies only on explicit refresh", async () => {
  const browser = browserFixture({ hash: "#/w" });
  await browser.start();
  const heading = mainOf(browser).querySelector("h1");
  // The stamp says the minute; its title says the second it was checked.
  const timestamp = browser.document.getElementById("live-read").getAttribute("title");
  assert.match(browser.document.getElementById("live-read").textContent, /^checked /);
  const opener = browser.document.getElementById("palette-open");
  opener.focus();
  opener.click();
  assert.equal(browser.document.activeElement, browser.document.getElementById("palette-input"));

  const next = guide("working", "updated-work");
  const pending = browser.network.hold("working");
  await nextWorkingPoll(browser);
  pending.resolve(next);
  await flush();
  assert.equal(mainOf(browser).querySelector("h1"), heading, "a background read does not redraw content behind the search dialog");
  assert.equal(heading.textContent, "Change WORKSPACE");
  assert.equal(browser.document.getElementById("live-read").getAttribute("title"), timestamp, "freshness describes the evidence still displayed");
  const warning = mainOf(browser).querySelector("#working-read-status");
  assert.equal(warning.hidden, false);
  assert.equal(warning.querySelector(".problem-line").textContent, "Your code changed since this was loaded.");
  browser.key("Escape");
  assert.equal(mainOf(browser).querySelector("h1"), heading, "closing search keeps the reader in control of the pending update");

  const forced = browser.network.hold("working");
  const before = browser.network.count("working");
  buttonNamed(warning, "Refresh").click();
  assert.equal(browser.network.count("working"), before + 1, "explicit refresh bypasses the polling delay");
  forced.resolve(next);
  await flush();
  assert.equal(mainOf(browser).querySelector("h1").textContent, "Change updated-work");
  assert.notEqual(browser.document.getElementById("live-read").getAttribute("title"), timestamp);
  assert.equal(mainOf(browser).querySelector("#working-read-status").hidden, true);
});

test("late working success and failure cannot replace or mark a newly selected change stale", async () => {
  for (const succeeds of [true, false]) {
    const browser = browserFixture({ hash: "#/w" });
    await browser.start();
    const late = browser.network.hold("working");
    await nextWorkingPoll(browser);
    assert.equal(browser.network.active("working"), 1);
    await browser.navigate("#/c/head");
    const heading = mainOf(browser).querySelector("h1");
    if (succeeds) late.resolve({ ...guide("working", "late-work"), change: { ...change("late-work"), base: "other-head" } });
    else late.reject(new Error("Old working read failed"));
    await flush();
    assert.equal(browser.location.hash, "#/c/head");
    assert.equal(mainOf(browser).querySelector("h1"), heading);
    assert.equal(heading.textContent, "Change head");
    const stale = mainOf(browser).querySelector("#working-read-status");
    assert.ok(!stale || stale.hidden || !stale.textContent, "the warning belongs to the working guide");
  }
});

test("a late foreground guide cannot overwrite a later navigation", async () => {
  for (const succeeds of [true, false]) {
    const browser = browserFixture();
    await browser.start();
    const late = browser.network.hold("guide");
    await browser.navigate("#/c/slow");
    await browser.navigate("#/c/head");
    if (succeeds) late.resolve(guide("change", "slow"));
    else late.reject(new Error("The previously selected change could not be read"));
    await flush();
    assert.equal(browser.location.hash, "#/c/head");
    assert.equal(mainOf(browser).querySelector("h1").textContent, "Change head");
  }
});

test("note and decision drafts survive navigation and reload without crossing repository identity", async () => {
  const storage = new Map();
  const options = { storage, repository: "Same display name", repositoryKey: "repo-one", withSteps: true };
  const browser = browserFixture(options);
  await browser.start();
  const drafts = { "change-note": "Change-wide question", "step-note": "Question about this entity", "decision-comment": "Decision still in progress" };
  for (const [name, value] of Object.entries(drafts)) typeDraft(browser, name, value);
  await browser.navigate("#/c/head");
  for (const name of Object.keys(drafts)) assert.equal(fieldOf(browser, name).value, "", `${name} stays with its change`);
  await browser.navigate("#/c/ancestor");
  for (const [name, value] of Object.entries(drafts)) assert.equal(fieldOf(browser, name).value, value);

  const reloaded = browserFixture(options);
  await reloaded.start();
  for (const [name, value] of Object.entries(drafts)) assert.equal(fieldOf(reloaded, name).value, value, `${name} survives reload`);
  const other = browserFixture({ ...options, repositoryKey: "repo-two" });
  await other.start();
  for (const name of Object.keys(drafts)) assert.equal(fieldOf(other, name).value, "", `${name} cannot leak to an equally named repository`);
  assert.ok(browser.network.requests.every((request) => request.method === "GET"), "typing and navigation record no review writes");
});

test("range drafts follow immutable base and head, separately from the head change", async () => {
  const storage = new Map();
  const browser = browserFixture({ storage, hash: "#/r/main.." });
  const first = browser.network.hold("guide");
  const rangeGuide = (base, head) => ({
    ...guide("range", head),
    change: { ...change(head), base },
    range: { base_ref: "main", head_ref: "HEAD", merge_base: base, changes_total: 1, changes: [change(head)] },
  });
  first.resolve(rangeGuide("base-one", "head-one"));
  await browser.start();
  typeDraft(browser, "change-note", "Question about this range");
  typeDraft(browser, "decision-comment", "Range decision draft");
  await browser.navigate("#/c/head-one");
  assert.equal(fieldOf(browser, "change-note").value, "");
  assert.equal(fieldOf(browser, "decision-comment").value, "");
  for (const [base, head, expected] of [["base-one", "head-two", ""], ["base-two", "head-one", ""], ["base-one", "head-one", "Question about this range"]]) {
    const next = browser.network.hold("guide");
    next.resolve(rangeGuide(base, head));
    await browser.navigate("#/r/main..");
    assert.equal(fieldOf(browser, "change-note").value, expected, `draft for ${base}..${head}`);
    assert.equal(fieldOf(browser, "decision-comment").value, expected ? "Range decision draft" : "");
    await browser.navigate("#/c/head-one");
  }
  const reloaded = browserFixture({ storage, hash: "#/r/main.." });
  reloaded.network.hold("guide").resolve(rangeGuide("base-one", "head-one"));
  await reloaded.start();
  assert.equal(fieldOf(reloaded, "change-note").value, "Question about this range");
  assert.equal(fieldOf(reloaded, "decision-comment").value, "Range decision draft");
});

test("an unconfirmed note preserves its draft and warning through navigation and reload without retrying the write", async () => {
  const storage = new Map();
  const browser = browserFixture({ storage });
  await browser.start();
  typeDraft(browser, "change-note", "Keep this until its outcome is checked");
  const write = browser.network.hold("note");
  buttonNamed(mainOf(browser), "Add note").click();
  await flush();
  write.reject(new Error("Connection lost after sending"));
  await flush();
  assert.match(mainOf(browser).textContent, /Not confirmed/);
  assert.equal(fieldOf(browser, "change-note").value, "Keep this until its outcome is checked");
  await browser.navigate("#/c/head");
  assert.doesNotMatch(mainOf(browser).textContent, /Not confirmed/);
  await browser.navigate("#/c/ancestor");
  assert.match(mainOf(browser).textContent, /Not confirmed/);
  assert.equal(browser.network.count("note"), 1, "navigation does not repeat an uncertain write");
  const reloaded = browserFixture({ storage });
  await reloaded.start();
  assert.equal(fieldOf(reloaded, "change-note").value, "Keep this until its outcome is checked");
  assert.match(mainOf(reloaded).textContent, /Not confirmed/);
  assert.equal(reloaded.network.count("note"), 0, "reload never retries a write automatically");
});

test("a successful note after navigation clears only the sent draft and preserves any newer text", async () => {
  for (const newer of ["", "A newer thought typed while saving"]) {
    const storage = new Map();
    const browser = browserFixture({ storage });
    await browser.start();
    typeDraft(browser, "change-note", "The note being sent");
    const write = browser.network.hold("note");
    buttonNamed(mainOf(browser), "Add note").click();
    await flush();
    assert.equal(browser.network.active("note"), 1);
    if (newer) typeDraft(browser, "change-note", newer);
    await browser.navigate("#/c/head");
    write.resolve({ reviews: [], reviews_total: 0 });
    await flush();
    assert.equal(mainOf(browser).querySelector("h1").textContent, "Change head");
    assert.equal(fieldOf(browser, "change-note").value, "", "the completed write does not alter a different change's draft");
    await browser.navigate("#/c/ancestor");
    assert.equal(fieldOf(browser, "change-note").value, newer, newer ? "the newer thought survives the prior send" : "the sent text is cleared from its saved draft");
    assert.match(mainOf(browser).textContent, /Saved\./);
    const reloaded = browserFixture({ storage });
    await reloaded.start();
    assert.equal(fieldOf(reloaded, "change-note").value, newer, "the completion updates the saved draft as well as the original field");
    assert.equal(browser.network.count("note"), 1);
    assert.equal(reloaded.network.count("note"), 0);
  }
});

test("source tabs expose their panels and support roving keyboard selection", async () => {
  const browser = browserFixture({ withSource: true });
  await browser.start();
  const tabs = mainOf(browser).querySelectorAll('[role="tab"]');
  assert.deepEqual(tabs.map((tab) => tab.textContent), ["Changes", "Before", "After"]);
  const panels = tabs.map((tab) => {
    const panel = browser.document.getElementById(tab.getAttribute("aria-controls"));
    assert.ok(panel, "each tab controls an existing panel");
    assert.equal(panel.getAttribute("role"), "tabpanel");
    assert.equal(panel.getAttribute("aria-labelledby"), tab.getAttribute("id"));
    return panel;
  });
  const selected = (at) => {
    tabs.forEach((tab, index) => {
      assert.equal(tab.getAttribute("aria-selected"), String(index === at));
      assert.equal(tab.tabIndex, index === at ? 0 : -1, "only the selected tab enters the tab order");
      assert.equal(panels[index].hidden, index !== at);
    });
  };
  selected(0);
  tabs[0].focus();
  for (const [key, at] of [["ArrowRight", 1], ["End", 2], ["Home", 0], ["ArrowLeft", 2], ["ArrowRight", 0]]) {
    const event = { type: "keydown", key };
    browser.document.activeElement.dispatchEvent(event);
    assert.equal(event.defaultPrevented, true);
    assert.equal(browser.document.activeElement, tabs[at]);
    selected(at);
  }
});

test("dialogs make the app inert, contain Tab focus, and restore their opener", async () => {
  const browser = browserFixture();
  await browser.start();
  const { document } = browser;
  const app = document.getElementById("app");
  for (const [overlay, openerId, focusId] of [["palette", "palette-open", "palette-input"], ["help", "help-open", "help-close"]]) {
    const opener = document.getElementById(openerId);
    opener.focus();
    opener.click();
    const control = document.getElementById(focusId);
    assert.equal(document.getElementById(overlay).hidden, false);
    assert.equal(app.inert, true, `${overlay} isolates the app`);
    assert.equal(document.activeElement, control);
    for (const shiftKey of [false, true]) {
      assert.equal(browser.key("Tab", { shiftKey }).defaultPrevented, true, "one-control dialog wraps in both directions");
      assert.equal(document.activeElement, control);
    }
    browser.key("Escape");
    assert.equal(document.getElementById(overlay).hidden, true);
    assert.equal(app.inert, false);
    assert.equal(document.activeElement, opener);
  }
});

test("a repository API prefix applies to startup, navigation and live polling", async () => {
  const browser = browserFixture({ apiBase: "/repositories/repo-one" });
  await browser.start();
  assert.equal(mainOf(browser).querySelector("h1").textContent, "Change ancestor");
  await browser.navigate("#/c/head");
  await browser.clock.advance(5000);
  assert.equal(browser.network.count("changes"), 2);
  assert.ok(browser.network.requests.length > 5);
  assert.ok(browser.network.requests.every((request) => request.url.startsWith("/repositories/repo-one/api/")));
});


test("source wrapping retains its announced state independently from diff layout", async () => {
  const browser = browserFixture({ withSource: true }); await browser.start();
  const wrap = browser.document.querySelector(".wrap-toggle");
  const layout = browser.document.querySelector(".layout-toggle");
  wrap.click(); assert.equal(wrap.getAttribute("aria-pressed"), "true");
  layout.click(); layout.click();
  assert.equal(layout.getAttribute("aria-pressed"), "false");
  assert.equal(wrap.getAttribute("aria-pressed"), "true");
  assert.equal(browser.document.body.classList.contains("wrap-source"), true);
});

// ── The history list, the map and the counts a reader sees ─────────────────

const railRows = (browser) => browser.document.getElementById("changes").querySelectorAll("a").filter((row) => row.getAttribute("href").startsWith("#/c/"));
const railMeta = (browser, id) => {
  const row = railRows(browser).find((candidate) => candidate.getAttribute("href") === `#/c/${id}`);
  assert.ok(row, `${id} is listed`);
  return row.querySelector(".change-meta").textContent;
};
const olderButton = (browser) => browser.document.getElementById("changes").querySelectorAll("button").find((button) => button.textContent === "Older changes");
const ids = (count, prefix = "c") => Array.from({ length: count }, (_, index) => `${prefix}${String(index).padStart(4, "0")}`);

test("the list says what each change's guide walks, never the history's delta count", async () => {
  const browser = browserFixture({
    history: ["head", "merged", "ancestor"],
    counts: { head: { entities: 4, module_containers: 4, containers_through_members: 3 }, merged: { entities: 0, merge: true }, ancestor: { entities: 1 } },
  });
  await browser.start();
  await flush();
  assert.match(railMeta(browser, "head"), /4 entities/, "the guide's own count");
  assert.doesNotMatch(railMeta(browser, "head"), /11 entities/, "the log's count holds module containers and classes read through members");
  assert.match(railMeta(browser, "ancestor"), /1 entity\b/);
  assert.match(railMeta(browser, "merged"), /merge/, "a merge's walk is not its own record, so no count is given");
  assert.doesNotMatch(railMeta(browser, "merged"), /entit/);

  const asked = browser.network.count("counts");
  assert.equal(asked, 1, "the listed changes are counted in one request");
  await browser.clock.advance(15000);
  assert.equal(browser.network.count("counts"), asked, "a recorded change is counted once, not on every poll");
  browser.network.history = ["newest", "head", "merged", "ancestor"];
  browser.network.counts.newest = { entities: 2 };
  await browser.clock.advance(5000);
  await flush();
  assert.equal(browser.network.count("counts"), asked + 1);
  assert.deepEqual(browser.network.asked("counts").at(-1).get("changes").split(","), ["newest"], "only the new change is asked about");
  assert.match(railMeta(browser, "newest"), /2 entities/);
});

test("a failed count leaves the list without counts and asks again later", async () => {
  const browser = browserFixture({ counts: { head: { entities: 3 }, ancestor: { entities: 1 } } });
  const failed = browser.network.hold("counts");
  await browser.start();
  failed.reject(new Error("The daemon could not count"));
  await flush();
  assert.doesNotMatch(railMeta(browser, "head"), /entit/, "no count is shown rather than a wrong one");
  await browser.clock.advance(5000);
  assert.equal(browser.network.count("counts"), 1, "a failed ask waits before asking again");
  await browser.clock.advance(30000);
  await flush();
  assert.ok(browser.network.count("counts") >= 2);
  assert.match(railMeta(browser, "head"), /3 entities/);
});

test("older changes page past the first five hundred without a gap or a repeat", async () => {
  const history = ids(1200);
  const browser = browserFixture({ history, initial: history[0] });
  await browser.start();
  assert.equal(railRows(browser).length, 60);
  const skips = [];
  while (olderButton(browser)) {
    olderButton(browser).click();
    await flush();
    const asked = browser.network.asked("changes").at(-1);
    skips.push([Number(asked.get("skip")), Number(asked.get("count"))]);
  }
  assert.deepEqual(skips, [[60, 60], [120, 120], [240, 240], [480, 480], [960, 500]], "each page starts where the list ends and no read asks past 500");
  assert.deepEqual(railRows(browser).map((row) => row.getAttribute("href").slice(4)), history, "every change once, in history order");

  await browser.clock.advance(5000);
  assert.equal(Number(browser.network.asked("changes").at(-1).get("count")), 500, "a poll reads one page");
  assert.equal(railRows(browser).length, 1200, "a poll keeps the older pages the reader read");
  assert.equal(olderButton(browser), undefined);

  browser.network.history = ["recorded-now", ...history];
  await browser.clock.advance(5000);
  assert.deepEqual(railRows(browser).map((row) => row.getAttribute("href").slice(4)), ["recorded-now", ...history], "a new change joins the top and nothing below moves");

  browser.network.history = ids(700, "other");
  await browser.clock.advance(5000);
  assert.equal(railRows(browser).length, 500, "another line of history starts the list again from its newest page");
  assert.ok(olderButton(browser), "and offers its older changes");
});

test("an older page that overlaps changes recorded since keeps each change once", async () => {
  const history = ids(150);
  const browser = browserFixture({ history, initial: history[0] });
  await browser.start();
  browser.network.history = ["new-one", "new-two", ...history];
  olderButton(browser).click();
  await flush();
  const listed = railRows(browser).map((row) => row.getAttribute("href").slice(4));
  assert.equal(new Set(listed).size, listed.length, "no change is listed twice");
  assert.deepEqual(listed.slice(0, 118), history.slice(0, 118));
});

function mapGuide() {
  const ref = (id, name, extra = {}) => ({ id, name, kind: "method", role: "source", ...extra });
  const link = (entity, extra = {}) => ({ entity, relation: "calls", relation_id: `r-${entity.id}`, origin: "parsed", confidence: 1, changed_here: false, ...extra });
  return {
    kind: "change",
    chapters: [{ title: "One", reason: "", steps: [{
      entity_id: "step", name: "URLSafeSerializerMixin.load_payload", parameters: "(self, payload, *args, serializer=None, **kwargs)",
      kind: "method", change: "modified", risk: "low", findings: [],
      connections: {
        consumers: [
          link(ref("init-a", "Serializer.__init__", { parameters: "(self, secret_key, salt)" }), { overloads: 4 }),
          link(ref("init-b", "Serializer.__init__", { parameters: "(self, secret_key)" })),
        ],
        consumers_total: 2,
        tests: [link(ref("t", "TestSerializer.test_iter_unsigners_with_fallbacks"))],
        tests_total: 1,
      },
    }] }],
  };
}

function mapContext() {
  const document = documentFixture("");
  const context = vm.createContext({ document, Node: Element, globalThis: undefined });
  context.globalThis = context;
  vm.runInContext(fs.readFileSync(mapPath, "utf8"), context, { filename: mapPath });
  return { document, map: context.kinReview.map };
}

test("map names wrap where a reader would split them and are never cut short", () => {
  const { map } = mapContext();
  const drawn = map.renderMap(mapGuide(), { open: () => {} });
  const names = drawn.node.querySelectorAll(".node-name");
  assert.equal(names.length, 1);
  assert.equal(names[0].textContent, "URLSafeSerializerMixin.load_payload(self, payload, *args, serializer=None, **kwargs)", "the whole name and its parameters");
  assert.ok(names[0].querySelectorAll("wbr").length >= 2, "the name may wrap after its dot and inside its words");
  const reliers = drawn.node.querySelectorAll(".relier-name").map((name) => name.textContent);
  // Declarations of one name in one place, such as the overloads of one
  // callable, read as one row that counts them: 1 + 4 overloads + 1 alike.
  assert.deepEqual(reliers, ["Serializer.__init__", "TestSerializer.test_iter_unsigners_with_fallbacks"], "same-name declarations fold into one row");
  const folded = drawn.node.querySelectorAll(".relier").find((row) => row.textContent.startsWith("Serializer.__init__"));
  assert.equal(folded.querySelector(".relier-count").textContent, "6 overloads");
  assert.equal(folded.querySelector(".relier-name").textContent, "Serializer.__init__", "a folded row shows no signature");
  assert.match(folded.title, /6 overloads of Serializer\.__init__ read as one row/);
  assert.equal(drawn.node.querySelector(".reliers-head").textContent, "Relies on URLSafeSerializerMixin.load_payload");
  assert.equal(drawn.node.querySelector(".reliers-head code").textContent, "URLSafeSerializerMixin.load_payload", "the name is set as code, as written");

  // Copied into this realm, since the script ran in its own.
  const pieces = (name) => Array.from(map.nameNodes(name)).filter((node) => typeof node === "string");
  assert.deepEqual(pieces("Serializer.__init__"), ["Serializer.", "__init__"], "a dunder stays whole");
  assert.deepEqual(pieces("a::b_c"), ["a::", "b_", "c"]);
});

function cssRule(css, selector) {
  const bare = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const rules = [...bare.matchAll(/([^{}]+)\{([^}]*)\}/g)].filter(([, selectors]) => selectors.split(",").map((part) => part.trim()).includes(selector));
  assert.ok(rules.length, `style.css has a rule for ${selector}`);
  return rules.map(([, , body]) => body).join("\n");
}

test("a map wider than its card scrolls instead of clipping, and names wrap instead of ending in an ellipsis", () => {
  const css = fs.readFileSync(stylePath, "utf8");
  // A centred row wider than its scroller overflows on both sides, and the
  // left side cannot be scrolled to. Safe centring starts it at the left.
  for (const selector of [".tree", ".tree ul"]) assert.match(cssRule(css, selector), /justify-content:\s*safe center/, selector);
  // A branch keeps the width of what it holds, so what hangs under it never
  // overflows it to both sides either.
  assert.match(cssRule(css, ".tree li"), /flex:\s*none/);
  for (const selector of [".node-name", ".relier-name", ".reliers-head", ".outline-step code"]) {
    const rule = cssRule(css, selector);
    assert.doesNotMatch(rule, /text-overflow:\s*ellipsis/, `${selector} is not cut short`);
    assert.doesNotMatch(rule, /white-space:\s*nowrap/, `${selector} may wrap`);
  }
});

test("a guide counts relations that changed apart from ones that only moved, and names classes read through their members", async () => {
  const browser = browserFixture({
    withSteps: true,
    shapeGuide: (body) => ({
      ...body,
      change: { ...body.change, author: "Armin Ronacher", author_recorded: "Armin Ronacher <armin@example.invalid> 1540586649 +0200" },
      summary: { ...body.summary, modified: 4, relation_changes: 35, relation_changes_position_only: 32, containers_through_members: 4, through_members: ["Signer", "TimestampSigner", "URLSafeSerializerMixin"] },
      chapters: body.chapters.map((chapter) => ({ ...chapter, steps: chapter.steps.map((step) => ({
        ...step,
        parameters: "(value, sig)",
        connections: { ...step.connections, consumers: [{ entity: { id: "init", name: "Serializer.__init__" }, relation: "calls", relation_id: "r", origin: "parsed", confidence: 1, changed_here: false, overloads: 1 }], consumers_total: 1 },
      })) })),
    }),
  });
  await browser.start();
  const text = mainOf(browser).textContent;
  const facts = mainOf(browser).querySelectorAll(".fact").map((fact) => fact.textContent);
  assert.ok(facts.includes("35 connections changed"), "the chip counts connections that changed");
  assert.ok(!facts.some((fact) => /moved/.test(fact)), "no chip calls a change a move");
  const details = mainOf(browser).querySelector(".status-details");
  assert.ok(details, "what only moved sits behind Show details");
  assert.match(details.textContent, /32 connections only moved and aren't shown\./);
  assert.match(details.textContent, /Signer, TimestampSigner, URLSafeSerializerMixin and 1 other changed only through what they hold, shown above\./);
  assert.match(text, /2 overloads/, "a row that stands for overload stubs counts them");
  assert.equal(mainOf(browser).querySelector(".step-id h3").textContent, "example(value, sig)");
  const author = mainOf(browser).querySelector(".eyebrow").querySelectorAll("span").find((span) => span.textContent === "Armin Ronacher");
  assert.ok(author, "the author reads as a person");
  assert.match(author.getAttribute("title"), /armin@example.invalid/, "the recorded signature stays one hover away");
});

test("an agent's change names the agent and what it looked at first, and a person's change has no such line", async () => {
  const id = "c".repeat(64);
  const [first, second, third] = ["5a1c7a4e-8d55-4d0b-a7a4-0b1f1b8f6d10", "9e2b7a4e-8d55-4d0b-a7a4-0b1f1b8f6d10", "0c3d7a4e-8d55-4d0b-a7a4-0b1f1b8f6d10"];
  const browser = browserFixture({ initial: id, history: [id], shapeGuide: (body) => body.kind === "change" ? { ...body, change: { ...body.change, author: "Claude Code", author_recorded: null, agent: { name: "Claude Code", key: "00000000000000a1" } } } : body });
  browser.network.changeAgent = { agent: { name: "Claude Code", key: "00000000000000a1", state: "idle" },
    looked_at: { count: 6, complete: true, entity_ids: [first, second, third] },
    names: { [first]: { label: "Signer.verify_signature" }, [second]: { label: "want_bytes" }, [third]: { label: "Signer" } } };
  await browser.start();
  await browser.clock.advance(10);
  const line = browser.document.getElementById("main").querySelectorAll("p").find((node) => node.className === "agent-line");
  assert.ok(line, "the agent line is drawn under the eyebrow");
  assert.equal(line.textContent, "Claude Code made this change after looking at 6 entities: Signer.verify_signature, want_bytes, Signer and 3 others.");
  assert.equal(browser.network.count("change-agent"), 1);
  assert.doesNotMatch(line.textContent, /a1b2c3d4|mcp-agent|session/);

  const person = browserFixture({ initial: id, history: [id] });
  await person.start();
  await person.clock.advance(10);
  assert.equal(person.document.getElementById("main").querySelectorAll("p").find((node) => node.className === "agent-line"), undefined);
  assert.equal(person.network.count("change-agent"), 0);
});

// ── Plain words: what the page says, and what it never says ───────────────

/** A step that something outside the change relies on, for the words tests. */
function reliedOnStep(id, name, { consumers = [], tests = [], consumersTotal, testsTotal, risk = "high" } = {}) {
  return {
    entity_id: id, name, kind: "function", change: "modified", risk, within: "module encoding",
    narration: "The function changed.", fields: [], findings: [], evidence: { head_change: "head" },
    language: "python",
    before: { state: "recorded", text: "return 1\n", change_id: "base" },
    after: { state: "recorded", text: "return 2\n", change_id: "head" },
    connections: {
      consumers, consumers_total: consumersTotal ?? consumers.length,
      tests, tests_total: testsTotal ?? tests.length,
      relation_changes: [], relation_changes_total: 0, importing_modules: [], importing_modules_total: 0,
    },
  };
}
const caller = (id, name, extra = {}) => ({
  entity: { id, name, kind: "method", within: "class Serializer", ...extra.entity },
  relation: "calls", relation_id: `r-${id}`, origin: "parsed", confidence: 1, changed_here: false, ...extra.link,
});
const withStep = (step, summary = {}) => (body) => ({
  ...body,
  chapters: [{ title: "One", reason: "It changed", steps: [step] }],
  summary: { ...body.summary, modified: 1, risk: "high", ...summary },
});
const eyebrowOf = (browser) => mainOf(browser).querySelector(".eyebrow");
const factsOf = (browser) => [...mainOf(browser).querySelectorAll(".facts .pill"), ...mainOf(browser).querySelectorAll(".fact")].map((fact) => fact.textContent);

test("the list names each change by who made it and when, never by a short ID", async () => {
  const browser = browserFixture({
    history: ["head", "merged", "counted-merge", "checkpoint", "ancestor"],
    counts: {
      head: { entities: 4 },
      merged: { entities: 0, merge: true },
      "counted-merge": { entities: 7, merge: true, merge_counted: true },
      checkpoint: { entities: 10 },
      ancestor: { entities: 1 },
    },
    changeFields: { checkpoint: { kin_checkpoint: true, title: "Kin 0.8.2 checkpoint" } },
  });
  await browser.start();
  await flush();
  assert.match(railMeta(browser, "head"), /^Reader·.+·4 entities$/, "author, then time, then the count");
  for (const id of ["head", "merged", "counted-merge", "checkpoint", "ancestor"]) {
    assert.doesNotMatch(railMeta(browser, id), new RegExp(id), `${id} is not named by its ID`);
    const row = railRows(browser).find((candidate) => candidate.getAttribute("href") === `#/c/${id}`);
    assert.equal(row.querySelectorAll("code").length, 0, "no row carries an ID set as code");
  }
  assert.match(railMeta(browser, "counted-merge"), /7 entities/, "a merge the server counted shows its count");
  assert.doesNotMatch(railMeta(browser, "counted-merge"), /merge/);
  assert.match(railMeta(browser, "merged"), /merge$/, "a merge without a count says merge");
  const checkpoint = railRows(browser).find((row) => row.getAttribute("href") === "#/c/checkpoint");
  assert.ok(checkpoint.classList.contains("is-checkpoint"), "a change Kin made for itself reads quieter");
  assert.match(railMeta(browser, "checkpoint"), /Kin update$/, "it says Kin update in place of a count");
  assert.doesNotMatch(railMeta(browser, "checkpoint"), /entit/);
  const plain = railRows(browser).find((row) => row.getAttribute("href") === "#/c/head");
  assert.ok(!plain.classList.contains("is-checkpoint"));
});

test("chips say how many rely on what changed and how many tests reach it, in plain words", async () => {
  const many = Array.from({ length: 16 }, (_, index) => caller(`c${index}`, `Serializer.m${index}`));
  const browser = browserFixture({
    shapeGuide: withStep(reliedOnStep("s", "want_bytes", { consumers: many, tests: [caller("t1", "test_one"), caller("t2", "test_two")] }),
      { recorded_consumers: 16, recorded_tests: 2, relation_changes: 5 }),
  });
  await browser.start();
  const facts = factsOf(browser);
  for (const said of ["High risk: 16 places rely on it", "2 tests reach it", "5 connections changed"]) {
    assert.ok(facts.includes(said), `a chip says "${said}"; chips: ${facts.join(" | ")}`);
  }
  assert.ok(!facts.includes("16 rely on it"), "the risk chip already says how many rely on it, so no second chip repeats it");
  assert.ok(!facts.some((fact) => /recorded|consumer|relation/i.test(fact)), "no chip says recorded, consumers or relations");

  const one = browserFixture({
    shapeGuide: withStep(reliedOnStep("s", "want_bytes", { consumers: [caller("c", "Serializer.m")], tests: [caller("t", "test_one")], risk: "medium" }),
      { risk: "medium", recorded_consumers: 1, recorded_tests: 1, relation_changes: 1 }),
  });
  await one.start();
  for (const said of ["Medium risk: 1 place relies on it", "1 test reaches it", "1 connection changed"]) {
    assert.ok(factsOf(one).includes(said), `a chip says "${said}"`);
  }
  assert.ok(!factsOf(one).includes("1 relies on it"));
  const none = browserFixture({ shapeGuide: withStep(reliedOnStep("s", "want_bytes"), { recorded_consumers: 0, recorded_tests: 0 }) });
  await none.start();
  assert.ok(factsOf(none).includes("High risk"), "with nothing relying on it the risk stands alone");
  assert.ok(factsOf(none).includes("No test reaches it"));
});

test("uncommitted work is said in the page's own words, with the time Kin last checked it", async () => {
  const browser = browserFixture({
    hash: "#/w",
    shapeGuide: (body) => (body.kind === "working"
      ? withStep(reliedOnStep("s", "want_bytes", { consumers: [caller("c", "Serializer.m")] }), { recorded_consumers: 1 })({
        ...body, change: { ...body.change, author: "troyfortinjr", title: "Uncommitted work", message: "Uncommitted work\n\nWhat the working tree holds beyond the last recorded change." },
      })
      : body),
  });
  await browser.start();
  const eyebrow = eyebrowOf(browser).textContent;
  assert.match(eyebrow, /^Since your last commit·checked /, "the header says since when, and when Kin last checked");
  assert.doesNotMatch(eyebrow, /troyfortinjr|working tree|compared with|head/i, "no account name, jargon or ID");
  assert.equal(mainOf(browser).querySelector(".message").textContent, "Changes you haven't committed yet.");
  assert.doesNotMatch(mainOf(browser).textContent, /working tree/i, "the server's message is not shown");
});

test("status notes collapse to one plain line, with the rest behind Show details", async () => {
  const summary = {
    module_containers: 2, provenance_only_entity_changes: 38, relation_changes_position_only: 25,
    completeness: { state: "not_settled", pending_entities: 162, message: "Not settled: 162 entities pending enrichment. Impact counts are lower bounds and risk may change." },
  };
  const browser = browserFixture({ shapeGuide: withStep(reliedOnStep("s", "want_bytes"), summary) });
  await browser.start();
  const notes = mainOf(browser).querySelector(".caveats");
  const visible = notes.children.filter((child) => child.tagName === "P").map((line) => line.textContent);
  assert.deepEqual(visible, ["Kin is still mapping 162 entities, so these counts may grow."], "one plain line");
  const details = notes.querySelector("details.status-details");
  assert.equal(details.querySelector("summary").textContent, "Show details");
  const said = details.querySelectorAll("p").map((line) => line.textContent);
  assert.deepEqual(said, [
    "2 modules changed along with what they hold.",
    "38 entities only moved within their module and aren't shown.",
    "25 connections only moved and aren't shown.",
  ]);
  assert.doesNotMatch(notes.textContent, /enrichment|lower bounds|recorded|Not settled/, "the server's jargon stays out");

  // An older server says the number only in its message.
  const older = browserFixture({ shapeGuide: withStep(reliedOnStep("s", "want_bytes"), { completeness: { state: "not_settled", message: "Not settled: 7 entities pending enrichment." } }) });
  await older.start();
  assert.equal(mainOf(older).querySelector(".status-line").textContent, "Kin is still mapping 7 entities, so these counts may grow.");

  // Nothing pending: no line, and what is left stays behind Show details.
  const settled = browserFixture({ shapeGuide: withStep(reliedOnStep("s", "want_bytes"), { module_containers: 1, completeness: { state: "no_recorded_debt", message: "No recorded call-site debt." } }) });
  await settled.start();
  assert.equal(mainOf(settled).querySelector(".status-line"), null, "no line when nothing is pending");
  assert.match(mainOf(settled).querySelector(".status-details").textContent, /1 module changed along with what it holds\./);
});

test("findings for the whole change are titled Also worth checking", async () => {
  const browser = browserFixture({
    shapeGuide: (body) => withStep(reliedOnStep("s", "want_bytes"))({ ...body, findings: { ...body.findings, test_coverage_gaps: ["No test reaches `signer`."] } }),
  });
  await browser.start();
  const titles = mainOf(browser).querySelectorAll("h2").map((title) => title.textContent);
  assert.ok(titles.includes("Also worth checking"));
  assert.ok(!titles.some((title) => /Recorded findings/.test(title)));
});

test("an unknown change says so plainly, offers the latest change, and keeps the raw text behind Show details", async () => {
  const browser = browserFixture({ hash: "#/c/0000000000000000000000000000000000000000000000000000000000000000" });
  const refused = browser.network.hold("guide");
  await browser.start();
  const raw = "kin review refused (HTTP 400): cannot resolve ref '0000': Git object '0000' was never imported into this repository; import it with `kin init`";
  refused.reject(new Error(raw));
  await flush();
  const main = mainOf(browser);
  assert.equal(main.querySelector(".problem-line").textContent, "Kin doesn't have this change.");
  const buttons = main.querySelectorAll("button");
  assert.deepEqual(buttons.map((button) => button.textContent), ["Open the latest change"], "one action");
  const details = main.querySelector("details.problem-details");
  assert.equal(details.querySelector("summary").textContent, "Show details");
  assert.equal(details.querySelector(".problem-text").textContent, raw, "the raw text is kept, behind Show details");
  await flush();
  buttons[0].click();
  assert.equal(browser.location.hash, "#/c/head", "it opens the newest listed change");

  // Any other failed read takes the same shape with a retry.
  const failing = browserFixture({ hash: "#/c/head" });
  const broken = failing.network.hold("guide");
  await failing.start();
  broken.reject(new Error("The local server answered 500."));
  await flush();
  assert.equal(mainOf(failing).querySelector(".problem-line").textContent, "Kin couldn't open this change.");
  const retry = buttonNamed(mainOf(failing), "Try again");
  retry.click();
  await flush();
  assert.equal(mainOf(failing).querySelector("h1").textContent, "Change head");
});

const indexPath = path.resolve(__dirname, "../crates/kin-cli/assets/review_ui/index.html");

test("a change's header names who and when, with its ID only behind Copy ID", async () => {
  const browser = browserFixture({
    withSource: true,
    shapeGuide: (body) => ({ ...body, change: { ...body.change, id: "83a2081f0a914189b642c9a929dc8bfc1ed6e54fa34d2d0c0db9841dda1bae1e", author: "David Lord", timestamp: "2024-04-16T20:09:22Z", git_commit: "2f69e841d2a979c616a55b226e444694f5d9c962" } }),
  });
  await browser.start();
  const eyebrow = eyebrowOf(browser);
  assert.match(eyebrow.textContent, /^David Lord·Apr 16, 2024Copy ID$/);
  assert.doesNotMatch(eyebrow.textContent, /83a2081f|2f69e841|Git commit/, "no raw ID is primary text");
  const copyId = buttonNamed(eyebrow, "Copy ID");
  assert.match(copyId.getAttribute("title"), /83a2081f0a914189b642c9a929dc8bfc1ed6e54fa34d2d0c0db9841dda1bae1e/, "the ID is one hover or click away");
  assert.equal(mainOf(browser).querySelector(".provenance").textContent, "Before and after this change.");
});

test("a comparison's header names its two sides", async () => {
  const browser = browserFixture({
    hash: "#/r/main..bound-decode",
    branches: [{ name: "main", default: true, active: true }, { name: "bound-decode", default: false, active: false }],
    shapeGuide: (body) => (body.kind === "range"
      ? { ...body, change: { ...body.change, title: "Bound base64 input before decoding" }, range: { ...body.range, base_ref: "main", head_ref: "bound-decode", merge_base: "f2aa114a8852" } }
      : body),
  });
  await browser.start();
  await flush();
  assert.equal(eyebrowOf(browser).textContent, "bound-decode compared with main");
  assert.doesNotMatch(mainOf(browser).querySelector(".change-head").textContent, /f2aa114a8852|before read at|after at/i);
});

test("the header names the repository as the workspace does, owner muted, only when the session gives one", async () => {
  const browser = browserFixture({ repository: "itsdangerous", session: { owner: "pallets", host: "github.com" } });
  await browser.start();
  const repo = browser.document.getElementById("repo");
  assert.equal(repo.textContent, "pallets / itsdangerous");
  assert.equal(repo.querySelector(".repo-owner").textContent, "pallets");
  const bare = browserFixture({ repository: "itsdangerous" });
  await bare.start();
  assert.equal(bare.document.getElementById("repo").textContent, "itsdangerous", "an older session without an owner shows the name alone");

  const html = fs.readFileSync(indexPath, "utf8");
  const head = /<header class="rail-head">([\s\S]*?)<\/header>/.exec(html)[1];
  const foot = /<footer class="rail-foot">([\s\S]*?)<\/footer>/.exec(html)[1];
  assert.doesNotMatch(head, /id="theme"/, "the theme control left the header");
  assert.match(foot, /<button class="rail-theme" id="theme"[^>]*>Auto<\/button>\s*<\/div>\s*$/, "it sits at the right end of the sidebar's last line");
  assert.doesNotMatch(html, /Reading/, "loading says Loading");
});

test("the review panel says how to approve uncommitted work and who decisions are signed as", async () => {
  const modified = (body) => ({ ...body, summary: { ...body.summary, modified: 1 } });
  const browser = browserFixture({ withSteps: true, session: { actor: "Troy Fortin" }, shapeGuide: modified });
  await browser.start();
  assert.equal(mainOf(browser).querySelector(".recorded-as").textContent, "Signed as Troy Fortin");
  await browser.navigate("#/w");
  const panel = mainOf(browser).querySelector("#review-panel");
  assert.match(panel.textContent, /Commit this work to approve it or ask for changes\./);
  assert.doesNotMatch(panel.textContent, /recorded|kin commit/i);
});

test("the uncommitted bar starts with the first step and refreshes from an icon", async () => {
  const browser = browserFixture({ withSteps: true, hash: "#/w", shapeGuide: (body) => ({ ...body, summary: { ...body.summary, modified: 1 } }) });
  await browser.start();
  const bar = mainOf(browser).querySelector("#reviewbar");
  const start = bar.querySelectorAll("button").find((button) => button.textContent === "Start with example");
  assert.ok(start, "the primary button names the first step");
  const refresh = bar.querySelector('[aria-label="Refresh"]');
  assert.ok(refresh, "refresh is an icon button labelled Refresh");
  assert.ok(!bar.querySelectorAll("button").some((button) => /working tree/i.test(button.textContent)));
  start.click();
  assert.ok(mainOf(browser).querySelector("#step-0").classList.contains("is-current"), "it jumps to the first step");
  const before = browser.network.count("working");
  refresh.click();
  await flush();
  assert.equal(browser.network.count("working"), before + 1);
  const css = fs.readFileSync(stylePath, "utf8");
  assert.match(cssRule(css, ".main"), /padding-bottom:\s*\d{2,}px/, "the main column leaves room under the bar");
});

test("overloads of one callable read as one row without signatures", async () => {
  const init = (id, parameters, overloads = 0) => caller(id, "Serializer.__init__", { entity: { parameters }, link: { overloads } });
  const browser = browserFixture({
    shapeGuide: withStep(reliedOnStep("s", "want_bytes", { consumers: [init("a", "(self, key)"), init("b", "(self, key, salt)"), init("c", "(self)", 3), caller("d", "Serializer.dumps")] })),
  });
  await browser.start();
  const rows = mainOf(browser).querySelectorAll("details.connection").map((row) => row.querySelector("summary").textContent);
  assert.equal(rows.length, 2, "four connections, two names");
  assert.match(rows[0], /^Serializer\.__init__6 overloadscalls it/, "one row counts every declaration");
  assert.doesNotMatch(rows[0], /\(self/, "no signature in the folded row");
  assert.match(rows[1], /^Serializer\.dumpscalls it/);
});

test("no CSS rule uppercases text that can hold a name", () => {
  const css = fs.readFileSync(stylePath, "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  // Labels made only of the page's own fixed words.
  const fixed = new Set([".outline-head", ".chapter-no", "h4", ".palette-kind", ".kind-chip"]);
  for (const [, selectors, body] of css.matchAll(/([^{}]+)\{([^}]*)\}/g)) {
    if (!/text-transform:\s*uppercase/.test(body)) continue;
    for (const selector of selectors.split(",").map((part) => part.trim())) {
      assert.ok(fixed.has(selector), `${selector} uppercases text and may hold a name`);
    }
  }
  assert.doesNotMatch(cssRule(css, ".reliers-head"), /uppercase/, "Relies on want_bytes keeps the name as written");
});

test("the editor control and shortcut say where code opens", async () => {
  const browser = browserFixture();
  await browser.start();
  const editor = browser.document.getElementById("editor");
  assert.equal(editor.textContent, "Open code in: not set");
  editor.click();
  assert.equal(editor.textContent, "Open code in: VS Code");
  assert.match(browser.document.getElementById("keys-list").textContent, /Choose which editor opens code/);
});

test("the page never says working tree, consumers, relations, enrichment, evidence or recorded, and uses no em dash", async () => {
  const banned = /working tree|consumers?\b|relations?\b|enrichment|evidence|recorded|unverified|—/i;
  const step = reliedOnStep("s", "want_bytes", { consumers: [caller("c", "Serializer.m"), caller("c2", "Serializer.__init__", { link: { overloads: 2 } })], tests: [caller("t", "test_one")] });
  const summary = {
    recorded_consumers: 2, recorded_tests: 1, relation_changes: 3, relation_changes_position_only: 2, module_containers: 1, provenance_only_entity_changes: 4,
    containers_through_members: 1, through_members: ["Signer"],
    completeness: { state: "not_settled", pending_entities: 9, message: "Not settled: 9 entities pending enrichment. Impact counts are lower bounds and risk may change." },
  };
  for (const hash of ["#/c/head", "#/w", "#/r/main.."]) {
    const browser = browserFixture({
      hash,
      shapeGuide: (body) => withStep(step, summary)({
        ...body,
        findings: { ...body.findings, test_coverage_gaps: ["No test reaches `signer`."] },
        ...(body.kind === "working" ? { change: { ...body.change, message: "Uncommitted work\n\nWhat the working tree holds beyond the last recorded change." } } : {}),
      }),
    });
    await browser.start();
    await flush();
    for (const id of ["main", "changes", "keys-list"]) {
      const text = browser.document.getElementById(id).textContent;
      assert.doesNotMatch(text, banned, `${hash} ${id}: ${(banned.exec(text) || [])[0]}`);
    }
  }
});

// ── Verdicts: what a change asks of the places that rely on it ────────────

/** A guide of one step with three callers, with or without the guide's verdicts. */
function verdictFixture(verdict, summary) {
  const callers = [caller("c1", "Serializer.dumps"), caller("c2", "Serializer.loads"), caller("c3", "Signer.sign")];
  const step = { ...reliedOnStep("s", "want_bytes", { consumers: callers }), ...(verdict ? { consumer_verdict: { verdict, reason: "comments_or_layout_only", consumers: 3 } } : {}) };
  return browserFixture({ shapeGuide: withStep(step, { recorded_consumers: 3, recorded_tests: 0, consumers_not_updated: 0, ...summary }) });
}
const ledeOf = (browser) => mainOf(browser).querySelector(".lede").textContent;
const tagsOf = (browser) => mainOf(browser).querySelectorAll(".tag").map((tag) => tag.textContent);

test("a change that asks nothing of the places that rely on it says none need an update", async () => {
  const browser = verdictFixture("needs_no_update", { consumers_need_no_update: 3, consumers_may_need_review: 0 });
  await browser.start();
  assert.match(ledeOf(browser), /Three other places rely on it\. None need an update\./);
  assert.doesNotMatch(ledeOf(browser), /None of them changed|not updated/);
  assert.deepEqual(tagsOf(browser).filter((tag) => tag !== "Test"), ["No update needed", "No update needed", "No update needed"]);
});

test("a body that changed under the same signature asks the reader to check the places that rely on it", async () => {
  const browser = verdictFixture("may_need_review", { consumers_need_no_update: 0, consumers_may_need_review: 3 });
  await browser.start();
  assert.match(ledeOf(browser), /Three other places rely on it\. Check each of them\./);
  assert.deepEqual(tagsOf(browser), ["Check this", "Check this", "Check this"]);
  const some = verdictFixture("may_need_review", { consumers_need_no_update: 2, consumers_may_need_review: 1 });
  await some.start();
  assert.match(ledeOf(some), /Check one of them\./);
});

test("a signature that changed under places that did not change says they were not updated", async () => {
  const browser = verdictFixture("not_updated", { consumers_need_no_update: 0, consumers_may_need_review: 0, consumers_not_updated: 3 });
  await browser.start();
  assert.match(ledeOf(browser), /Three other places rely on it\. None were updated\./);
  assert.deepEqual(tagsOf(browser), ["Not updated", "Not updated", "Not updated"]);
  assert.equal(mainOf(browser).querySelectorAll(".tag-behind").length, 3);
});

test("a guide written before verdicts reads exactly as it did", async () => {
  const browser = verdictFixture(null, {});
  await browser.start();
  assert.match(ledeOf(browser), /Three other places rely on it\. None of them changed\./);
  assert.deepEqual(tagsOf(browser), ["Unchanged", "Unchanged", "Unchanged"]);
  assert.equal(mainOf(browser).querySelectorAll(".tag-fine, .tag-check").length, 0);
});
