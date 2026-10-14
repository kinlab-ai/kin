// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

"use strict";

// The local Kin workspace. One page serves two scopes. At the server's root it
// is the hub: every repository on this machine, what is in motion in each, and
// a way to bring another one in. Inside a repository it is the workbench: the
// work in flight, the changes already recorded, the code's entities and what
// relies on them, and its branches. Every number, name and connection comes
// from a read of Kin. A read that fails says so in its own place and offers to
// try again; the rest of the page keeps working.
(() => {
  const byId = (id) => document.getElementById(id);
  const rawBase = document.body.dataset.apiBase || "";
  const apiBase = rawBase === "__KIN_BASE__" ? "" : rawBase.replace(/\/$/, "");
  // With no repository in its address the page is the hub.
  const MACHINE = apiBase === "";
  const graphApi = () => globalThis.KinWorkspaceGraph || null;
  const agentsApi = () => globalThis.KinWorkspaceAgents || null;
  const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  const ROUTES = MACHINE ? ["hub", "clients"] : ["uncommitted", "review", "history", "branches", "agents", "explore", "entity"];
  const TITLES = { hub: "Home", clients: "Coding agents", uncommitted: "Uncommitted", review: "Review", history: "History", branches: "Branches", agents: "Agents", explore: "Code", entity: "Entity" };
  const AGENT_KEY = /^[0-9a-f]{16}$/;

  // Reads shared by every view. A path that starts at the server's root is the
  // machine's own; every other path belongs to the repository in the address.
  const READS = MACHINE
    ? { session: "root:/api/home/session", home: "root:/api/home", clients: "root:/api/home/clients" }
    : {
      session: "/api/workspace/session",
      changes: "/api/changes",
      standing: "/api/standing",
      branches: "/api/branches",
      branchState: "/api/workspace/branches",
      conflicts: "/api/workspace/conflicts/details",
      working: "/api/guide?working=1",
      overview: "/api/workspace/overview",
      agents: "/api/workspace/agents",
      home: "root:/api/home",
      remotes: "/api/workspace/remotes",
      flags: "/api/workspace/flags",
    };
  // A server started for one repository alone has no hub to read.
  const OPTIONAL = new Set(MACHINE ? ["clients"] : ["agents", "home", "remotes", "flags"]);
  const readKeys = () => Object.keys(READS);
  const FAILED_WORDS = {
    session: MACHINE ? "Kin isn't answering on this machine right now." : "Kin can't reach this repository right now.",
    changes: "Kin couldn't load the changes.",
    standing: "Kin couldn't load review decisions.",
    branches: "Kin couldn't load the branches.",
    branchState: "Kin couldn't load the branches.",
    conflicts: "Kin couldn't load the merge in progress.",
    working: "Kin couldn't load your uncommitted work.",
    overview: "Kin couldn't load the summary.",
    agents: "Kin couldn't load which agents are connected.",
    home: "Kin couldn't load the repositories on this machine.",
    clients: "Kin couldn't load which coding agents are connected.",
    remotes: "Kin couldn't load this repository's remotes.",
    flags: "Kin couldn't load the flags on this work.",
  };
  /** Whether a failure only means Kin took too long, so trying again later may work. */
  const isSlow = (detail) => /did not answer within|took longer than|timed out|timeout/i.test(String(detail || ""));
  /** Whether Kin declined a read only because its graph was being written while the read ran, so the same read can answer once it holds still. */
  const isUnsettled = (detail) => /no settled graph authority|graph-authority writer|was mutating the graph|picked up working-copy changes|enrichment sweep was installing/i.test(String(detail || ""));

  const state = {
    reads: Object.fromEntries(Object.keys(READS).map((key) => [key, { data: null, error: null, at: null, busy: false, missing: false, sig: null }])),
    route: null,
    params: {},
    refreshing: false,
    requested: 60,
    sections: new Map(),
    disposers: [],
    reviewQuery: "",
    reviewFilter: "all",
    authorFilter: "all",
    rangeBase: null,
    rangeHead: "",
    explore: { query: "", results: null, error: null, busy: false, request: 0, offset: 0, active: -1 },
    entity: { id: null, data: null, error: null, busy: false, request: 0, expanded: false, showAll: {}, tab: "connections" },
    merge: { source: "", preview: null, result: null, error: null, remedy: null, busy: null, operationId: null, reportedOperationId: null, unknown: false, confirmed: false, receiptRestored: false },
    branch: { target: "", preview: null, result: null, error: null, diagnostic: null, busy: null, confirmed: false, operationId: null, unknown: false, receiptRestored: false },
    commit: { open: false, message: "", busy: false, error: null, remedy: null, unknown: false, result: null, operationId: null, onlyFine: true },
    restore: { asking: null, busy: false, error: null, refused: false, unknown: false, operationId: null },
    verify: { busy: false, operation: null, error: null, timer: null, unknown: false },
    remote: { busy: null, result: null, error: null, unknown: false, operationId: null },
    add: { open: false, mode: "convert", path: "", source: "", inspect: null, busy: null, error: null, operation: null, timer: null },
    connect: { busy: null, error: null, timer: null },
    newBranch: { open: false, name: "", busy: false, error: null, created: null },
    settle: { busy: null, error: null, unknown: false },
    flightSelected: null,
    review: { target: null, guide: null, error: null, busy: false, request: 0, marks: { fine: new Set(), flagged: new Set(), notes: {} }, decision: { busy: false, error: null, done: null, comment: "" } },
    diffOpen: new Set(),
    testsOpen: new Set(),
    hubQuery: "",
    readEpoch: 0,
    destroyed: false,
    // Each change's count as its own guided review gives it, by change id.
    counts: new Map(),
    countsAsked: new Set(),
    countsMissing: false,
    countsFailed: false,
    menuOpen: false,
    exploreTimer: null,
    switchOpen: false,
    branchGuides: new Map(),
    mergeOpen: false,
    compareOpen: false,
  };

  // ── Elements ─────────────────────────────────────────────────────────────

  function h(tag, attrs, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs || {})) {
      if (value === undefined || value === null || value === false) continue;
      if (key === "class") node.className = value;
      else if (key === "on") for (const [event, handler] of Object.entries(value)) node.addEventListener(event, handler);
      // The page's policy refuses style attributes, so a measured size is set as a property.
      else if (key === "css") for (const [property, amount] of Object.entries(value)) node.style.setProperty(property, amount);
      else node.setAttribute(key, value === true ? "" : String(value));
    }
    for (const child of children.flat(Infinity)) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  const ICONS = {
    arrow: "M5 12h14m-5-5 5 5-5 5",
    back: "M19 12H5m5 5-5-5 5-5",
    branch: "M6 3.5v13M18 8.5c0 4.5-6 3.5-12 7.5M6 21a2 2 0 1 0 0-4 2 2 0 0 0 0 4zM18 9a2 2 0 1 0 0-4 2 2 0 0 0 0 4zM6 5.5a2 2 0 1 0 0-4 2 2 0 0 0 0 4z",
    search: "M11 17.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM20 20l-4.3-4.3",
    editor: "M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5",
    home: "M4 11.2 12 4.5l8 6.7V19a1 1 0 0 1-1 1h-4.5v-5.5h-5V20H5a1 1 0 0 1-1-1z",
    code: "m8.5 7.5-5 4.5 5 4.5M15.5 7.5l5 4.5-5 4.5",
    uncommitted: "M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4",
    history: "M12 7.5V12l3 2M4.5 12a7.5 7.5 0 1 0 2.4-5.5M4 4.5v3.5h3.5",
    work: "M4 7.5h16M4 12h10M4 16.5h6M17.5 14.5l1.8 1.8 3.2-3.6",
    changes: "M12 3v5.5M12 15.5V21M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7z",
    agents: "M12 3.5c.5 4.3 2.7 6.5 7 7-4.3.5-6.5 2.7-7 7-.5-4.3-2.7-6.5-7-7 4.3-.5 6.5-2.7 7-7z",
    plus: "M12 5v14M5 12h14",
    check: "m5 12.5 4.5 4.5L19 7.5",
  };
  function icon(name) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    for (const [key, value] of Object.entries({ viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "1.7", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" })) svg.setAttribute(key, value);
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", ICONS[name] || ICONS.arrow);
    svg.append(path);
    return svg;
  }

  // ── Words ────────────────────────────────────────────────────────────────

  const text = (value) => typeof value === "string" ? value : "";
  const array = (value) => Array.isArray(value) ? value : [];
  const isCount = (value) => Number.isSafeInteger(value) && value >= 0;
  const NUMBER_WORD = ["No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten", "Eleven", "Twelve"];
  const spoken = (n) => n < NUMBER_WORD.length ? NUMBER_WORD[n] : n.toLocaleString();
  const count = (n) => Number(n).toLocaleString();
  const plural = (n, one, many) => `${count(n)} ${n === 1 ? one : many || `${one}s`}`;
  // Kinds whose word is not their code with its underscores read as spaces. The run-together
  // spellings are what an older server still sends for the same kinds.
  const ENTITY_KIND_WORDS = { type_alias: "Type alias", typealias: "Type alias", trait_def: "Trait", traitdef: "Trait", enum_def: "Enum", enumdef: "Enum",
    enum_variant: "Enum variant", enumvariant: "Enum variant", static_var: "Static", staticvar: "Static", api_endpoint: "API endpoint", apiendpoint: "API endpoint",
    event_contract: "Event contract", eventcontract: "Event contract", document_node: "Document section", documentnode: "Document section" };
  const kindWord = (kind) => ENTITY_KIND_WORDS[text(kind).toLowerCase()] || text(kind).replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
  /**
   * Kin records a member of a type as a method whether it is called or holds
   * a value, so a member whose own declaration reads `port?: number` or
   * `getter: () => T` is said to be the property it is.
   */
  const PROPERTY_DECLARATION = /^\s*(?:(?:readonly|static|public|private|protected|declare|abstract|override)\s+)*[#\w$]+[?!]?\s*:(?!:)/;
  const kindOf = (row) => text(row?.kind).toLowerCase() === "method" && PROPERTY_DECLARATION.test(text(row?.signature)) ? "property" : text(row?.kind);
  const LANGUAGE_WORDS = { typescript: "TypeScript", javascript: "JavaScript", cpp: "C++", csharp: "C#", php: "PHP", sql: "SQL", html: "HTML", css: "CSS", json: "JSON", yaml: "YAML", toml: "TOML", objc: "Objective-C" };
  const languageWord = (language) => LANGUAGE_WORDS[text(language).toLowerCase()] || kindWord(language);
  function clock(value) {
    return new Date(value).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });
  }
  function ago(timestamp) {
    const date = new Date(timestamp);
    if (Number.isNaN(date.getTime())) return "";
    const seconds = Math.round((date.getTime() - Date.now()) / 1000);
    const spans = [["year", 31_536_000], ["month", 2_592_000], ["week", 604_800], ["day", 86_400], ["hour", 3_600], ["minute", 60]];
    const format = typeof Intl !== "undefined" && Intl.RelativeTimeFormat ? new Intl.RelativeTimeFormat(undefined, { numeric: "auto" }) : null;
    for (const [unit, size] of spans) {
      if (Math.abs(seconds) >= size) {
        const value = Math.round(seconds / size);
        return format ? format.format(value, unit) : `${Math.abs(value)} ${unit}${Math.abs(value) === 1 ? "" : "s"} ago`;
      }
    }
    return "just now";
  }

  /** A person's name from a recorded author: `Name <email> 1749933342 -0700` reads as `Name`. */
  function personName(author) {
    if (author && typeof author === "object") return personName(author.name || author.email || "");
    const value = text(author).trim();
    if (!value) return "";
    const angle = value.indexOf("<");
    if (angle > 0) return value.slice(0, angle).trim();
    if (angle === 0) { const email = value.slice(1, value.indexOf(">") > 0 ? value.indexOf(">") : undefined); return email.split("@")[0]; }
    if (/^[^\s@]+@[^\s@]+$/.test(value)) return value.split("@")[0];
    return value.replace(/\s+\d{9,}\s+[+-]\d{4}$/, "").trim();
  }
  const authorOf = (change) => text(change?.author_name) || personName(change?.author);
  const limitedGuide = (guide) => {
    const completeness = guide?.summary?.completeness;
    return Boolean(completeness && (completeness.state !== "settled" || completeness.unresolved_entities > 0));
  };
  const graphCount = (n, limited) => `${limited ? "at least " : ""}${count(n)}`;
  const graphBadge = (n, limited) => `${limited && n > 0 ? "≥" : ""}${count(n)}`;
  /** How far Kin looked for the tests it counted, finishing that sentence: " within 3 steps". Nothing when the guide does not say. */
  const withinSteps = (depth) => isCount(depth) && depth > 0 ? ` within ${count(depth)} ${depth === 1 ? "step" : "steps"}` : "";

  /**
   * What uncommitted work means, in the guided review's words: what changed,
   * what relies on it from outside the work, how much of that was left behind,
   * and which tests reach it. Every number is the guide's own.
   */
  function workSentence(guide) {
    const summary = guide?.summary || {};
    const changed = (summary.added || 0) + (summary.modified || 0) + (summary.removed || 0);
    if (!changed) return null;
    const steps = array(guide.chapters).flatMap((chapter) => array(chapter.steps));
    const kinds = new Set(steps.map((step) => step.kind).filter(Boolean));
    const noun = (n) => {
      const kind = kinds.size === 1 && !guide.not_walked ? text([...kinds][0]).replace(/_/g, " ").toLowerCase() : "entity";
      if (n === 1) return kind;
      return kind.endsWith("y") ? `${kind.slice(0, -1)}ies` : kind.endsWith("s") ? `${kind}es` : `${kind}s`;
    };
    const it = changed === 1 ? "it" : "them";
    const quantity = (n) => limitedGuide(guide) ? `At least ${spoken(n).toLowerCase()}` : spoken(n);
    const parts = [`${spoken(changed)} ${noun(changed)} changed.`];
    const outside = summary.recorded_consumers || 0;
    if (!outside) parts.push("Kin found no connections outside this change.");
    else {
      parts.push(`${quantity(outside)} other ${outside === 1 ? "place relies" : "places rely"} on ${it}.`);
      const behind = isCount(summary.consumers_not_updated) ? summary.consumers_not_updated : 0;
      // The guided review's own verdicts, when it gives them; an older guide reads as before.
      const check = isCount(summary.consumers_may_need_review) ? summary.consumers_may_need_review : 0;
      const fine = isCount(summary.consumers_need_no_update) ? summary.consumers_need_no_update : null;
      if (behind && behind === outside) parts.push(limitedGuide(guide) ? outside === 1 ? "The one Kin found was not updated." : "The ones Kin found were not updated." : outside === 1 ? "It was not updated." : outside === 2 ? "Neither was updated." : "None were updated.");
      else if (behind) { parts.push(`${spoken(behind)} ${behind === 1 ? "was" : "were"} not updated.`); if (check) parts.push(`Check ${spoken(check).toLowerCase()} more.`); }
      else if (check) parts.push(check === outside ? limitedGuide(guide) ? "Check the ones Kin found." : (outside === 1 ? "Check it." : outside === 2 ? "Check both." : "Check each of them.") : `Check ${spoken(check).toLowerCase()} of them.`);
      else if (fine !== null && fine === outside) parts.push(limitedGuide(guide) ? "The ones Kin found need no update." : outside === 1 ? "It needs no update." : "None need an update.");
      else parts.push(limitedGuide(guide) ? "The ones Kin found did not change." : outside === 1 ? "It did not change." : "None of them changed.");
    }
    const tests = summary.recorded_tests || 0;
    const depths = new Set(steps.map((step) => step.connections?.tests_depth).filter((depth) => isCount(depth) && depth > 0));
    const depth = depths.size === 1 ? [...depths][0] : null;
    parts.push(tests ? `${quantity(tests)} ${tests === 1 ? "test reaches" : "tests reach"} ${it}${withinSteps(depth)}.` : depths.size === 1 && steps.length && steps.every((step) => isCount(step.connections?.tests_depth))
      ? `No test found within ${count([...depths][0])} ${[...depths][0] === 1 ? "step" : "steps"} of ${it}.` : "Kin found no tests that reach this work.");
    return parts.join(" ");
  }

  /** How many entities a piece of work changed, as its guided review counts them. */
  function workChanged(guide) {
    const summary = guide?.summary || {};
    return (summary.added || 0) + (summary.modified || 0) + (summary.removed || 0);
  }

  // ── Places ───────────────────────────────────────────────────────────────

  /**
   * A repository as its remote names it: owner and name, "pallets / itsdangerous",
   * with the owner muted. Without a remote it is its name alone.
   */
  function repositoryLabel(repository, separator = "/") {
    const owner = text(repository?.owner);
    const name = text(repository?.name) || "repository";
    return owner ? `${owner}${separator}${name}` : name;
  }
  function repositoryTitle(repository) {
    const owner = text(repository?.owner);
    const name = text(repository?.name) || "repository";
    return owner ? [h("span", { class: "repo-item-owner" }, `${owner} / `), name] : [name];
  }

  const read = (key) => state.reads[key] || { data: null, error: null, missing: true };
  const session = () => read("session").data;
  let announcedAt = 0;
  function announce(message) {
    const node = byId("workspace-announcer");
    if (!node) return;
    // A routine "Up to date" does not talk over an outcome the reader has just been told.
    const routine = message === "Up to date.";
    if (routine && Date.now() - announcedAt < 4000) return;
    if (!routine) announcedAt = Date.now();
    node.textContent = message;
  }
  function safeLocalURL(value, fallback) {
    if (typeof value !== "string" || !value) return fallback;
    try {
      const url = new URL(value, location.href);
      return url.origin === location.origin && /^https?:$/.test(url.protocol) ? `${url.pathname}${url.search}${url.hash}` : fallback;
    } catch (_) { return fallback; }
  }
  function reviewURL(hash = "") {
    const base = safeLocalURL(session()?.review_url, `${apiBase}/review/`).split("#")[0];
    return `${base}${hash}`;
  }
  // A change read on one page from top to bottom, with its notes and its record.
  const changeURL = (id) => reviewURL(`#/c/${encodeURIComponent(id)}`);
  // The guided review of a change or of uncommitted work, one thing at a time.
  const reviewHref = (id) => `#/review/c/${encodeURIComponent(id)}`;
  const UNCOMMITTED = "#/uncommitted";
  const entityHref = (id) => `#/entity/${encodeURIComponent(id)}`;
  const branchRows = () => array(read("branches").data?.branches);
  const activeBranch = () => branchRows().find((branch) => branch.active);
  const defaultBranch = () => branchRows().find((branch) => branch.default);
  const shortRef = (name) => text(name).replace(/^refs\/heads\//, "");

  function button(label, handler, attrs = {}) { return h("button", { type: "button", class: "button", on: { click: handler }, ...attrs }, label); }
  function link(label, href, primary = false, attrs = {}) { return h("a", { class: `button${primary ? " button-primary" : ""}`, href, ...attrs }, label); }
  function moreLink(label, href) { return h("a", { class: "more-link", href }, label, icon("arrow")); }
  function pill(label, tone = "") { return h("span", { class: `pill${tone ? ` pill-${tone}` : ""}` }, label); }
  function skeleton(...classes) { return h("span", { class: `skeleton ${classes.join(" ")}`, "aria-hidden": "true" }); }
  function skeletonLines(n) { return h("div", { "aria-busy": "true", "aria-label": "Loading" }, Array.from({ length: n }, (_, i) => h("span", { class: "skeleton skeleton-line", "aria-hidden": "true", "data-w": String(i % 3) }))); }
  /** A problem in one plain line, one action, and Kin's own words behind Show details. */
  function failed(message, retry, detail, action = "Try again") {
    // A part that only took too long says so plainly, with Kin's words below.
    if (!text(detail).trim() && isSlow(message)) { detail = message; message = "Kin is still working on this part. Try again in a moment."; }
    const said = text(detail).trim();
    return h("div", { class: "read-failed", role: "alert" },
      h("div", { class: "read-failed-text" }, h("p", {}, message),
        said && said !== message ? h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", { class: "detail-text" }, said)) : null),
      retry ? button(action, retry, { class: "button button-small" }) : null);
  }
  function readFailure(key) {
    const detail = read(key).error;
    return failed(isSlow(detail) ? "Kin is still working on this. Try again in a moment." : FAILED_WORDS[key] || "Kin couldn't load this.", () => refresh([key]), detail);
  }

  /** An entity by name: a way into its own page when Kin holds it. */
  /** An entity's name as a reader says it: a binding imported from another module is named for itself. */
  function shownName(name) {
    const imported = /^"[^"]*"\.(.+)$/.exec(text(name));
    return imported ? imported[1] : text(name);
  }
  const importedName = (name) => /^"[^"]*"\./.test(text(name));
  function ent(row, { linked = true } = {}) {
    const id = text(row?.id) || text(row?.entity_id);
    const name = h("span", { class: "ent-name" }, shownName(row?.name) || "Entity", importedName(row?.name) ? h("span", { class: "ent-tag" }, " imported") : null);
    return linked && UUID.test(id) ? h("a", { class: "ent", href: entityHref(id), title: text(row?.name) || undefined }, name) : h("span", { class: "ent", title: text(row?.name) || undefined }, name);
  }
  // ── Reading ──────────────────────────────────────────────────────────────

  async function fetchJSON(path, options = {}, attempt = 0) {
    const controller = new AbortController();
    // A read waits a minute, or longer when its caller says nothing waits on it.
    const { patience = 60_000, ...sent } = options;
    const timer = setTimeout(() => controller.abort(), patience);
    const url = path.startsWith("root:") ? path.slice(5) : `${apiBase}${path}`;
    try {
      const response = await fetch(url, { ...sent, signal: controller.signal, headers: { accept: "application/json", ...(sent.headers || {}) } });
      // Kin answers "too many at once" when several heavy reads land together. A read
      // changes nothing, so it waits its turn and asks again; a write is never resent.
      if (response.status === 429 && attempt < 4 && (!options.method || options.method === "GET")) {
        clearTimeout(timer);
        const wait = Number(response.headers?.get?.("retry-after"));
        await new Promise((resolve) => setTimeout(resolve, Number.isFinite(wait) && wait > 0 ? Math.min(wait, 10) * 1000 : 800 * (attempt + 1)));
        return fetchJSON(path, options, attempt + 1);
      }
      let body;
      try { body = await response.json(); } catch (_) {
        const error = new Error(response.ok ? "The local server returned an unreadable response." : `The local server answered ${response.status}.`);
        error.status = response.status;
        throw error;
      }
      if (!response.ok) {
        const error = new Error(body?.error || `The local server answered ${response.status}.`);
        error.status = response.status;
        if (typeof body?.outcome === "string") error.outcome = body.outcome;
        if (typeof body?.operation_id === "string") error.operationId = body.operation_id;
        if (typeof body?.remedy === "string") error.remedy = body.remedy;
        if (typeof body?.detail === "string") error.detail = body.detail;
        if (body?.operation && typeof body.operation === "object") error.operation = body.operation;
        throw error;
      }
      if (!body || typeof body !== "object") throw new Error("The local server returned nothing readable.");
      return body;
    } catch (error) {
      if (error.name === "AbortError") throw new Error(`This read took longer than ${patience === 60_000 ? "a minute" : patience % 60_000 ? `${Math.round(patience / 1000)} seconds` : `${patience / 60_000} minutes`}.`);
      throw error;
    } finally { clearTimeout(timer); }
  }
  /** A request that changes something, carrying this page's own write token. */
  function send(path, body) {
    return fetchJSON(path, { method: "POST", headers: { "content-type": "application/json", "x-kin-review-token": session()?.token || "" }, body: JSON.stringify(body) });
  }

  const workReceiptKey = (kind) => session()?.repository ? `kin.workspace.pending-${kind}:${JSON.stringify([apiBase, session().repository.key || null])}` : null;
  function rememberWorkReceipt(kind, operationId) {
    try { sessionStorage.setItem(workReceiptKey(kind), JSON.stringify({ operationId })); } catch (_) { /* The in-memory lockout remains. */ }
  }
  function forgetWorkReceipt(kind) {
    try { sessionStorage.removeItem(workReceiptKey(kind)); } catch (_) { /* Preserve the observed result. */ }
  }
  function restoreWorkReceipts() {
    for (const kind of ["commit", "restore", "remote"]) {
      const held = state[kind];
      if (held.receiptRestored) continue;
      held.receiptRestored = true;
      try {
        const receipt = JSON.parse(sessionStorage.getItem(workReceiptKey(kind)));
        if (!UUID.test(text(receipt?.operationId))) continue;
        held.operationId = receipt.operationId; held.unknown = true;
        held.error = "An earlier request was sent from this tab. Check its result before starting another.";
      } catch (_) { /* No readable receipt to restore. */ }
    }
  }

  // A committed change's guide, once read in this tab by Home or by a review,
  // is handed to the next page that wants it, so the same change is not read
  // twice. A recorded change does not move, so a recent reading is the one.
  const changeGuideKey = (base, id) => `kin.workspace.change-guide:${base}:${id}`;
  const CHANGE_GUIDE_KEPT = 30 * 60_000;
  function heldChangeGuide(base, id) {
    try {
      const held = JSON.parse(sessionStorage.getItem(changeGuideKey(base, id)));
      if (held?.data?.summary && Number.isFinite(held.at) && Date.now() - held.at < CHANGE_GUIDE_KEPT && (!held.data.change?.id || held.data.change.id === id)) return held.data;
    } catch (_) { /* Nothing kept; the read answers. */ }
    return null;
  }
  function keepChangeGuide(base, id, data) {
    if (!data?.summary) return;
    try {
      const body = JSON.stringify({ at: Date.now(), data });
      if (body.length < 600_000) sessionStorage.setItem(changeGuideKey(base, id), body);
    } catch (_) { /* The page still holds it. */ }
  }

  const workingKey = () => `kin.workspace.working:${apiBase}`;
  /** The last uncommitted-work answer this tab saw, shown while a new one is read. */
  function recallWorking() {
    const resource = read("working");
    if (MACHINE || resource.data || resource.recalled) return;
    resource.recalled = true;
    try {
      const held = JSON.parse(sessionStorage.getItem(workingKey()));
      if (held?.data?.summary && Number.isFinite(held.at)) { resource.data = held.data; resource.at = held.at; resource.stale = true; resource.changed = true; }
    } catch (_) { /* Nothing kept; the read answers. */ }
  }
  function keepWorking(data) {
    try {
      const body = JSON.stringify({ at: Date.now(), data });
      if (body.length < 600_000) sessionStorage.setItem(workingKey(), body); else sessionStorage.removeItem(workingKey());
    } catch (_) { /* The page still holds it. */ }
  }

  function pathFor(key) {
    return key === "changes" ? `${READS.changes}?count=${state.requested}` : READS[key];
  }

  /**
   * The list of repositories as this page keeps it: a repository whose daemon
   * did not answer this time keeps what the reading before said of its
   * branch, its work, its merge and its agents, with when that was known,
   * rather than dropping to nothing in motion. A daemon that has stopped
   * holds no agent session, so none is kept for it, and a count of work not
   * arrived yet for a daemon that answers shows the last count until it does.
   */
  function carryHome(next, previous, previousAt) {
    const before = new Map(array(previous?.repositories).filter((row) => row && typeof row.key === "string").map((row) => [row.key, row]));
    if (!before.size || !Array.isArray(next?.repositories)) return next;
    const then = Number.isFinite(previousAt) ? new Date(previousAt).toISOString() : null;
    const counted = (working) => Boolean(working) && typeof working === "object" && working.unknown !== true;
    return { ...next, repositories: next.repositories.map((row) => {
      const held = row && before.get(row.key);
      if (!held) return row;
      const out = { ...row };
      const reach = text(row.reach);
      if (!counted(row.working) && counted(held.working)) out.working = held.working;
      if (reach === "answering") return out;
      let carried = false;
      for (const field of ["branch", "merge_in_progress", ...(reach === "stopped" ? [] : ["agents"])]) {
        if (out[field] === undefined && held[field] !== undefined) { out[field] = held[field]; carried = true; }
      }
      if (out.working !== row.working) carried = true;
      if (carried || text(row.known_at)) out.known_at = text(row.known_at) || text(held.known_at) || then;
      // A server that does not say whether the daemon answered is taken at its word that it said nothing.
      if (carried && !reach) out.reach = "not_answering";
      return out;
    }) };
  }

  async function load(key) {
    const resource = read(key);
    const epoch = state.readEpoch;
    if (!(key in READS) || resource.busy) return;
    if (resource.missing && OPTIONAL.has(key)) return;
    resource.busy = true;
    try {
      const answer = await fetchJSON(pathFor(key));
      const data = key === "home" && Array.isArray(answer?.repositories) ? carryHome(answer, resource.data, resource.at) : answer;
      if (epoch !== state.readEpoch) return;
      if (key === "session" && !MACHINE && (!data.repository || typeof data.repository.name !== "string")) throw new Error("The server did not name the repository.");
      if (key === "changes" && !Array.isArray(data.changes)) throw new Error("Changes were not returned in a readable form.");
      if (key === "branches" && !Array.isArray(data.branches)) throw new Error("Branches were not returned in a readable form.");
      if (key === "working" && !data.summary) throw new Error("Uncommitted work was not returned in a readable form.");
      if (key === "standing" && (!data.standing || typeof data.standing !== "object")) throw new Error("Review decisions were not returned in a readable form.");
      if (key === "overview" && (data.schema && data.schema !== "kin.workspace.overview.v1")) throw new Error("The repository summary was not returned in a readable form.");
      if (key === "agents" && (!Array.isArray(data.agents) || data.schema !== "kin.workspace.agents.v1")) throw new Error("The agents were not returned in a readable form.");
      if (key === "home" && !Array.isArray(data.repositories)) throw new Error("The repositories were not returned in a readable form.");
      if (key === "clients" && !Array.isArray(data.clients)) throw new Error("The coding agents were not returned in a readable form.");
      const sig = JSON.stringify(key === "home" ? { ...data, read_at: null } : data);
      resource.changed = sig !== resource.sig;
      resource.sig = sig;
      resource.data = data;
      resource.at = Date.now();
      resource.error = null;
      resource.missing = false;
      if (key === "working") { if (resource.stale) resource.changed = true; resource.stale = false; keepWorking(data); }
      if (key === "session" && !MACHINE) { restoreMergeReceipt(); restoreBranchReceipt(); restoreWorkReceipts(); if (data.capabilities?.remote_pull_checking === true) checkPull(epoch); }
    } catch (error) {
      if (epoch !== state.readEpoch) return;
      if (OPTIONAL.has(key) && [404, 405, 501].includes(error.status)) { resource.missing = true; resource.error = null; resource.changed = true; }
      else { resource.error = error.message || "This read could not be completed."; resource.changed = true; }
    } finally { if (epoch === state.readEpoch) resource.busy = false; }
  }

  /**
   * A session that answered before Kin knew whether a pull can work here asks
   * for that answer on its own, and the remotes redraw when it lands. Until
   * then the pull says it is being checked rather than disappearing.
   */
  async function checkPull(epoch) {
    if (state.pullChecking) return;
    state.pullChecking = true;
    try {
      const answer = await fetchJSON("/api/workspace/session/pull");
      const data = read("session").data;
      if (epoch !== state.readEpoch || !data?.capabilities || answer?.remote_pull_checking === true) return;
      data.capabilities = { ...data.capabilities, remote_pull: answer.remote_pull === true, remote_pull_checking: false,
        remote_pull_unavailable_reason: typeof answer.remote_pull_unavailable_reason === "string" ? answer.remote_pull_unavailable_reason : null,
        ...(typeof answer.remote_fetch === "boolean" ? { remote_fetch: answer.remote_fetch } : {}) };
      paint("branches-remotes");
    } catch { /* The session's own answer stands, and the next refresh asks again. */ }
    finally { state.pullChecking = false; }
  }

  async function refresh(keys = readKeys()) {
    if (state.refreshing || state.branch.busy || state.merge.busy || state.commit.busy) return;
    state.refreshing = true;
    for (const key of keys) if (OPTIONAL.has(key)) read(key).missing = false;
    setRefreshDisabled(true);
    updateChrome();
    const hadSession = Boolean(session());
    // Each section draws as soon as its own read lands, so a slow read never
    // holds back the ones that are ready.
    const drawn = new Set();
    const settle = async (key) => {
      await load(key);
      if (key === "changes") loadCounts();
      if (key === "home") loadHomeDetails();
      if (!hadSession || !session()) return;
      syncAll([key]);
      drawn.add(key);
      if (["branches", "working", "agents", "home", "changes", "standing"].includes(key)) updateChrome();
      if (["branches", "branchState", "conflicts"].includes(key)) repaintPanels();
    };
    await Promise.all(keys.map(settle));
    state.refreshing = false;
    setRefreshDisabled(false);
    updateChrome();
    if (!hadSession || !session()) render();
    else syncAll(keys.filter((key) => !drawn.has(key)));
    repaintPanels();
    if (state.route === "entity" && keys.length >= readKeys().length) loadEntity(state.params.id, true);
    const failedKeys = keys.filter((key) => read(key).error);
    announce(failedKeys.length ? `${failedKeys.length === 1 ? FAILED_WORDS[failedKeys[0]] : "Some reads failed."}` : "Up to date.");
  }

  /**
   * Each listed change's count as its own guided review reads it, so a row
   * here and the review it opens say the same number, merges included. A
   * change not counted yet shows no count rather than a different one.
   */
  async function loadCounts() {
    if (state.countsMissing) return;
    const wanted = array(read("changes").data?.changes).map((change) => change?.id)
      .filter((id) => typeof id === "string" && /^[\w-]{1,128}$/.test(id) && !state.counts.has(id) && !state.countsAsked.has(id));
    const epoch = state.readEpoch;
    for (let at = 0; at < wanted.length; at += 100) {
      const batch = wanted.slice(at, at + 100);
      batch.forEach((id) => state.countsAsked.add(id));
      try {
        const held = await fetchJSON(`/api/counts?changes=${batch.join(",")}`);
        if (epoch !== state.readEpoch) return;
        for (const id of batch) state.counts.set(id, held?.counts?.[id] || null);
        state.countsFailed = false;
      } catch (error) {
        // A failed count is asked again on the next read.
        if ([404, 405, 501].includes(error.status)) state.countsMissing = true;
        else state.countsFailed = true;
        paint("review-body");
        return;
      } finally {
        batch.forEach((id) => state.countsAsked.delete(id));
      }
      for (const id of ["review-body", "changes-results", "agents-detail"]) paint(id);
    }
  }

  /** Draw the branch panels again from the latest reads, never from under a field the reader is typing in. */
  function repaintPanels() {
    if (state.route !== "branches" || !session()) return;
    const typing = document.activeElement?.closest?.("input, select, textarea");
    for (const [id, panel] of [["workspace-branch-host", branchPanel], ["workspace-merge-host", mergePanel]]) {
      const host = byId(id);
      if (host && !(typing && host.contains?.(typing))) host.replaceChildren(...[panel()].filter(Boolean));
    }
  }

  // A live pulse reads again only what it names, outside the full refresh,
  // and pulses that arrive close together are read once.
  const pulsing = { keys: new Set(), timer: null };
  function pulse(keys, delay = 500) {
    for (const key of keys) if (key in READS) pulsing.keys.add(key);
    if (pulsing.timer || !pulsing.keys.size) return;
    pulsing.timer = setTimeout(async () => {
      pulsing.timer = null;
      if (!session() || state.destroyed) { pulsing.keys.clear(); return; }
      const ready = [...pulsing.keys].filter((key) => !read(key).busy);
      pulsing.keys = new Set([...pulsing.keys].filter((key) => read(key).busy));
      await Promise.all(ready.map(async (key) => { await load(key); syncAll([key]); if (key === "home") loadHomeDetails(); }));
      if (ready.some((key) => ["changes", "standing", "branches", "working", "agents", "home"].includes(key))) updateChrome();
      if (pulsing.keys.size) pulse([], 800);
    }, delay);
  }

  function setRefreshDisabled(disabled) {
    const node = byId("workspace-refresh");
    if (node) node.disabled = Boolean(disabled || state.branch.busy || state.merge.busy || state.commit.busy);
  }

  // ── Sections ─────────────────────────────────────────────────────────────
  // A view is a set of sections, each bound to the reads it shows. When a read
  // returns, only the sections bound to it are drawn again, so a half-typed
  // search keeps its caret while history refreshes.

  function section(id, reads, build, attrs = {}) {
    const node = h("div", { id, ...attrs });
    state.sections.set(id, { reads, build, node, cleanup: null });
    paint(id);
    return node;
  }
  function paint(id) {
    const entry = state.sections.get(id);
    if (!entry) return;
    entry.cleanup?.();
    entry.cleanup = null;
    const focused = document.activeElement;
    const focusId = entry.node.contains?.(focused) ? focused.id : null;
    const selection = (focused instanceof HTMLInputElement && ["search", "text"].includes(focused.type)) || focused instanceof HTMLTextAreaElement ? [focused.selectionStart, focused.selectionEnd] : null;
    const built = entry.build((cleanup) => { entry.cleanup = cleanup; });
    entry.node.replaceChildren(...[built].flat(Infinity).filter(Boolean));
    if (focusId && byId(focusId)) {
      byId(focusId).focus({ preventScroll: true });
      if (selection) byId(focusId).setSelectionRange?.(...selection);
    }
  }
  /** Draw again, once each, the sections bound to any of these reads that changed. */
  function syncAll(keys) {
    const changed = keys.filter((key) => read(key).changed !== false);
    for (const key of changed) read(key).changed = false;
    for (const [id, entry] of [...state.sections]) if (state.sections.get(id) === entry && entry.reads.some((key) => changed.includes(key))) paint(id);
  }
  function clearSections() {
    for (const entry of state.sections.values()) entry.cleanup?.();
    state.sections.clear();
    for (const dispose of state.disposers.splice(0)) dispose();
  }

  // ── Shared facts ─────────────────────────────────────────────────────────

  const section_ok = (value) => value && typeof value === "object" && value.ok !== false;

  const MODULE_KINDS = new Set(["module", "package", "namespace"]);
  // A module that only says which module holds it, such as an index, is named for that one.
  const HOLDER_ONLY = new Set(["index", "mod", "__init__", "lib", "main", "init"]);
  // A suffix that is part of a module's own name, as `apply.test` is.
  const NAME_SUFFIX = new Set(["test", "tests", "spec", "d", "config"]);
  /**
   * A module as code names it: `schemas`, not the dotted place
   * `zod.src.v4.classic.schemas` it is recorded under, and `ws`, not
   * `extract.ws`. A suffix that is part of the module's own name stays:
   * `apply.test` is the module's name.
   */
  function moduleName(name) {
    const parts = text(name).split(/::|[./\\]/).filter(Boolean);
    if (parts.length < 2) return text(name);
    if (HOLDER_ONLY.has(parts.at(-1))) return parts.at(-2);
    return NAME_SUFFIX.has(parts.at(-1)) ? parts.slice(-2).join(".") : parts.at(-1);
  }
  function containerOf(row) {
    if (!row) return "";
    const owner = row.container && typeof row.container === "object" ? row.container : row.semantic_owners?.[0] && typeof row.semantic_owners[0] === "object" ? row.semantic_owners[0] : null;
    const container = row.container && typeof row.container === "object" ? text(row.container.name) : text(row.container);
    const explicit = container || text(row.container_name) || text(row.semantic_owners?.[0]?.name);
    if (explicit) return owner && MODULE_KINDS.has(text(owner.kind).toLowerCase()) ? moduleName(explicit) : explicit;
    // The module a row is recorded under is always a module, so it is named as code names it.
    if (text(row.module)) return moduleName(row.module);
    const name = text(row.name);
    return name.includes(".") ? name.slice(0, name.lastIndexOf(".")) : "";
  }
  const reliedOn = (row) => isCount(row?.dependents) ? row.dependents : isCount(row?.relied_on_by) ? row.relied_on_by : null;

  function topEntities() {
    const overview = read("overview").data;
    const rows = section_ok(overview?.most_relied_on) && Array.isArray(overview.most_relied_on.items) ? overview.most_relied_on.items : [];
    return rows.filter((row) => row && UUID.test(row.id) && text(row.kind).toLowerCase() !== "module" && (reliedOn(row) ?? 0) > 0);
  }
  /** Whether the most relied-on list is still on its way, failed, or settled. */
  function topState() {
    const overview = read("overview");
    if (overview.data) return section_ok(overview.data.most_relied_on) ? "ready" : "failed";
    return overview.error ? "error" : "reading";
  }

  /** Whether Kin recorded a change for itself: an update to its understanding that changes no code. */
  const isKinUpdate = (change) => change?.kin_checkpoint === true;
  /** The changes people made, newest first, without Kin's own updates. */
  const peopleChanges = () => array(read("changes").data?.changes).filter((change) => !isKinUpdate(change));

  // ── Agents ───────────────────────────────────────────────────────────────
  // The agents panel and page live in agents.js; this is what they draw with.

  const agentsCtx = {
    h, icon, ago, clock, kindWord, entityHref, changeURL: (id) => reviewHref(id), button, link, moreLink, pill, skeleton, skeletonLines, failed, workSentence, fetchJSON, announce, apiBase, ent,
    read: () => read("agents"),
    // Where an agent's work meets uncommitted work, as Uncommitted lists it.
    overlaps: () => overlapList(),
    refresh: (keys) => refresh(keys),
    pulse: (keys, delay) => pulse(keys, delay),
    paint: (id) => paint(id),
    section: (id, reads, build, attrs) => section(id, reads, build, attrs),
    params: () => state.params,
    // The one count a change is shown with: its own review's, as Changes shows it.
    changedCount: (id) => changedCount({ id }),
    // How a change's review stands: "pending" until someone decides, and null while review decisions are not read.
    reviewState: (id) => {
      const standing = read("standing").data?.standing;
      if (!standing || typeof standing !== "object") return null;
      const value = text(standing[id]?.state);
      return value === "needswork" ? "needs_work" : value || "pending";
    },
    // Whether a section is on the page now, so work for one nobody can see waits.
    shown: (id) => Boolean(state.sections.get(id)?.node?.isConnected),
    repaintAgents: () => { for (const id of ["agents-lede", "agents-list", "agents-detail"]) paint(id); updateChrome(); },
  };

  // ── Hub ──────────────────────────────────────────────────────────────────
  // Every repository Kin holds on this machine. A row says only what was read
  // without waking that repository: its name, its branch and what is in motion
  // when its own service is already running, and how large it was when last
  // recorded.

  const homeRows = () => array(read("home").data?.repositories);
  const ACTIVITY_WORDS = { starting: "Starting", reconciling: "Reading your edits", indexing: "Building the graph", enriching: "Linking references", reading_history: "Reading its history" };
  /** What Kin is busy with in a repository, in a few words; nothing when it is idle. */
  const repoActivity = (row) => ACTIVITY_WORDS[text(row?.activity?.phase)] || "";
  /**
   * What is in motion in a repository, as its dot and its place in the list:
   * work of yours waiting first, then an agent's committed change nobody has
   * reviewed, then an agent working.
   */
  function repoTone(row) {
    // Home says it from the one reading every part of Home is drawn from, so the dot and the card agree.
    if (MACHINE) { const view = homeView(row); if (view) return view.tone; }
    // Marked only once Kin has counted something changed: its quick reading also differs for Kin's own linking.
    if ((isCount(row?.working?.changed) && row.working.changed > 0) || row?.merge_in_progress) return "work";
    if (awaitingReview(row).length) return "review";
    if (isCount(row?.agents?.active) && row.agents.active > 0) return "live";
    return "";
  }
  const TONE_RANK = { work: 0, review: 1, live: 2, "": 3 };

  // Which agent changes nobody has reviewed is read from each repository by
  // Home, and by a repository's own page for itself. What this tab last read
  // is kept so every page's sidebar says the same thing about a repository
  // until it is read again.
  const reviewKey = (key) => `kin.workspace.awaiting-review:${key}`;
  function keepAwaiting(key, ids) {
    try { sessionStorage.setItem(reviewKey(key), JSON.stringify(ids)); } catch (_) { /* The page still holds it. */ }
  }
  function keptAwaiting(key) {
    try { const held = JSON.parse(sessionStorage.getItem(reviewKey(key))); return Array.isArray(held) ? held.filter((id) => typeof id === "string") : []; } catch (_) { return []; }
  }
  const TONES_KEY = "kin.workspace.side-tones";
  function keepTones(tones) {
    try { sessionStorage.setItem(TONES_KEY, JSON.stringify(tones)); } catch (_) { /* The page still holds them. */ }
  }
  function keptTones() {
    try { const held = JSON.parse(sessionStorage.getItem(TONES_KEY)); return held && typeof held === "object" ? Object.fromEntries(Object.entries(held).filter(([, tone]) => tone in TONE_RANK)) : {}; } catch (_) { return {}; }
  }
  /** The ids of a repository's agent changes nobody has reviewed, from the freshest reading this page has. */
  function awaitingReview(row) {
    const key = text(row?.key);
    if (!key) return [];
    if (MACHINE) return awaitingChanges(row).map((change) => change.id);
    if (key === text(session()?.repository?.key)) {
      const own = ownAwaiting();
      return own ?? keptAwaiting(key);
    }
    return keptAwaiting(key);
  }
  /**
   * This repository's agent changes nobody has reviewed, among its newest few
   * as Home reads them, from its own changes and review standing; null until
   * both are read.
   */
  function ownAwaiting() {
    const standing = read("standing").data?.standing;
    const changes = read("changes").data?.changes;
    if (!standing || typeof standing !== "object" || !Array.isArray(changes)) return null;
    const ids = changes.slice(0, OVERVIEW_CHANGES)
      .filter((change) => !isKinUpdate(change) && change.agent && typeof change.id === "string" && (standing[change.id]?.state || "pending") === "pending").map((change) => change.id);
    const key = text(session()?.repository?.key);
    if (key) keepAwaiting(key, ids);
    return ids;
  }

  /** Connect one coding agent to Kin, then read the list again when Kin says it is done. */
  async function connectClient(id) {
    const connect = state.connect;
    if (connect.busy) return;
    connect.busy = id; connect.error = null;
    paint("hub-clients");
    const finish = (error) => { connect.busy = null; connect.error = error || null; paint("hub-clients"); refresh(["clients"]); };
    const follow = (operation, misses = 0) => {
      if (operation.state === "done") return finish();
      if (operation.state === "failed") return finish(text(operation.error) || "Kin could not connect it.");
      clearTimeout(connect.timer);
      connect.timer = setTimeout(async () => {
        try { follow(await fetchJSON(`root:/api/home/operations/${encodeURIComponent(operation.operation_id)}`)); }
        catch (error) { if (misses >= 5) finish(error.message || "Kin stopped answering."); else follow(operation, misses + 1); }
      }, 900);
    };
    try {
      const started = await send("root:/api/home/clients", { connect: [id] });
      follow(started.state ? started : { operation_id: started.operation_id, state: "running" });
    } catch (error) { finish(error.message || "Kin could not connect it."); }
  }

  /** When a coding agent last worked through Kin, and where; nothing is said when the server does not report it. */
  function lastUsed(row) {
    if (!("last_session" in row)) return null;
    const last = row.last_session;
    if (!last) return h("span", { class: "client-state" }, "Has not worked through Kin yet");
    const where = text(last.repository?.name) || "a repository";
    const place = safeLocalURL(last.repository?.url, null) ? h("a", { class: "link", href: safeLocalURL(last.repository.url, "/") }, where) : where;
    return h("span", { class: "client-state" }, ...(last.state === "active" ? ["Working in ", place, " now"] : last.state === "idle" ? ["Connected to ", place] : ["Last worked in ", place, ` ${ago(last.at)}`]));
  }

  function hubClients() {
    const resource = read("clients");
    if (resource.missing) return h("p", { class: "gap" }, "This version of Kin can't list coding agents here.");
    if (!resource.data) return resource.error ? readFailure("clients") : skeletonLines(3);
    const rows = array(resource.data.clients).filter((row) => row && typeof row.id === "string" && typeof row.name === "string");
    // What is here and connected first, then what is here and waiting, then what is not installed.
    const rank = (row) => row.detected && row.connected ? 0 : row.detected ? 1 : 2;
    rows.sort((a, b) => rank(a) - rank(b));
    const connect = state.connect;
    return h("div", {},
      connect.error ? h("div", { class: "message message-bad section-tight", role: "alert" }, h("div", {}, h("strong", {}, "That agent was not connected"), h("p", {}, connect.error))) : null,
      h("ul", { class: "list list-card" }, rows.map((row) => h("li", { class: "client-row" },
        h("span", { class: "client-name" }, agentsApi()?.mark?.(14) || null, row.name),
        !row.detected ? h("span", { class: "client-state" }, "Not installed on this machine")
          : row.connected ? [lastUsed(row), h("span", { class: "client-state client-state-on" }, "Configured")]
          : [h("span", { class: "client-state" }, "Installed, not configured"),
            button(connect.busy === row.id ? "Connecting…" : "Connect", () => connectClient(row.id), { class: "button button-small", disabled: Boolean(connect.busy) || !session()?.token })]))));
  }

  // ── Adding a repository ──────────────────────────────────────────────────

  const ADD_MODES = [["convert", "A project on this machine"], ["create", "A new repository"], ["clone", "Clone a repository"]];
  const KIND_WORDS = { kin: "Kin repository", git: "Git project", plain: "Project without history", missing: "Nothing here yet", not_a_directory: "Not a project" };
  const ACTION_WORDS = { convert: ["Convert to Kin", "Converting"], create: ["Create repository", "Creating"], open: ["Open in Kin", "Opening"], clone: ["Clone", "Cloning"] };

  const candidates = { data: null, busy: false, missing: false };
  async function loadCandidates(again = false) {
    if (candidates.busy || candidates.missing || (candidates.data && !again)) return;
    candidates.busy = true;
    try { candidates.data = await fetchJSON("root:/api/home/candidates?limit=30"); }
    catch (_) { if (!candidates.data) candidates.missing = true; }
    candidates.busy = false;
    paint("hub-candidates");
    // Commit counts arrive after the list; ask again while some are still being counted and the form is open.
    if (isCount(candidates.data?.counting) && candidates.data.counting > 0 && state.add.open) setTimeout(() => loadCandidates(true), 1500);
  }
  function openAdd(mode) {
    state.add.open = true;
    loadCandidates();
    if (mode) state.add.mode = mode;
    paint("hub-add"); paint("hub-add-toggle");
    byId("add-path")?.focus();
  }
  function resetAdd(keepOpen = true) {
    clearTimeout(state.add.timer);
    state.add = { ...state.add, open: keepOpen, inspect: null, busy: null, error: null, operation: null, timer: null };
  }

  async function inspectPath() {
    const add = state.add;
    const path = add.path.trim();
    if (!path || add.busy) return;
    add.busy = "inspect"; add.error = null; add.inspect = null; add.operation = null;
    paint("hub-add");
    try { add.inspect = await send("root:/api/home/inspect", { path }); }
    catch (error) { add.error = [404, 405, 501].includes(error.status) ? "This version of Kin can't add a repository from here." : error.message || "Kin couldn't look at that place."; }
    add.busy = null;
    paint("hub-add");
  }

  async function runAdd(action) {
    const add = state.add;
    if (add.busy) return;
    const body = action === "clone" ? { action, source: add.source.trim(), path: add.path.trim() } : { action, path: text(add.inspect?.path) || add.path.trim() };
    add.busy = action; add.error = null;
    paint("hub-add");
    try {
      const started = await send("root:/api/home/repositories", body);
      add.operation = started.state ? started : { operation_id: started.operation_id, action, state: "running", phase: "Starting", lines: [] };
      add.busy = null;
      paint("hub-add");
      if (add.operation.state === "running") followOperation(add.operation.operation_id);
      else refresh(["home"]);
    } catch (error) {
      add.busy = null;
      add.error = error.message || "Kin couldn't start that.";
      paint("hub-add");
    }
  }

  /** Read an operation until it is done, and draw each reading. A missed read is tried again. */
  function followOperation(id, misses = 0) {
    clearTimeout(state.add.timer);
    state.add.timer = setTimeout(async () => {
      if (state.add.operation?.operation_id !== id) return;
      try {
        state.add.operation = await fetchJSON(`root:/api/home/operations/${encodeURIComponent(id)}`);
        paint("hub-add");
        if (state.add.operation.state === "running") return followOperation(id);
        announce(state.add.operation.state === "done" ? "The repository is ready." : "That did not finish.");
        refresh(["home"]);
      } catch (error) {
        if (misses >= 5) { state.add.operation = { ...state.add.operation, state: "failed", error: "Kin stopped answering about this. Read the list again to see whether it finished.", detail: error.message }; paint("hub-add"); return; }
        followOperation(id, misses + 1);
      }
    }, 900);
  }

  /** One line of Kin's progress in a few plain words. */
  function phaseWords(operation) {
    const STAGE = { starting: "Starting Kin for this repository", reading: "Reading the project", graph: "Building the graph", references: "Linking references", history: "Reading its history", finishing: "Finishing" };
    const stage = STAGE[text(operation.stage)];
    if (stage) {
      const progress = operation.progress;
      return isCount(progress?.done) && isCount(progress?.total) && progress.total > 0 ? `${stage}, ${count(progress.done)} of ${count(progress.total)}` : stage;
    }
    const line = text(operation.phase);
    const counted = line.match(/checked ([\d,]+) of ([\d,]+)/i);
    if (counted) return `Linking references, ${counted[1]} of ${counted[2]}`;
    if (/linking references|language server/i.test(line)) return "Linking references";
    if (/daemon/i.test(line)) return "Starting Kin for this repository";
    if (/history/i.test(line)) return "Reading its history";
    if (/clon/i.test(line)) return "Copying the repository";
    if (/admit|import|pars|graph|entit/i.test(line)) return "Building the graph";
    return operation.action === "clone" ? "Copying the repository" : "Reading the project";
  }

  function operationBox(operation) {
    const [, doing] = ACTION_WORDS[operation.action] || ["", "Working"];
    // A repository is named by its own name here; where it lives was shown when it was looked at.
    const where = text(operation.repository?.name) || text(state.add.inspect?.name) || (text(operation.location) || state.add.path.trim()).replace(/\/+$/, "").split("/").pop() || "the repository";
    const done = operation.state === "done";
    const failedState = operation.state === "failed";
    const repository = operation.repository;
    const lines = array(operation.lines).filter((line) => typeof line === "string");
    return h("div", { class: "op", "data-state": operation.state, role: "status" },
      h("div", { class: "op-title" }, done ? "Ready" : failedState ? "This did not finish" : `${doing} ${where}`, done ? pill("Done", "good") : failedState ? pill("Stopped", "bad") : null),
      h("p", { class: "op-phase" }, done ? [`${text(repository?.name) || where} is in Kin.`, text(operation.note) ? ` ${operation.note}` : ""] : failedState ? text(operation.error) || "Kin stopped before finishing." : phaseWords(operation)),
      h("div", { class: "op-bar", "aria-hidden": "true" }),
      failedState && (lines.length || text(operation.detail)) ? h("details", { class: "more" }, h("summary", {}, "Show Kin's own words"), h("pre", { class: "op-lines" }, [text(operation.detail), ...lines].filter(Boolean).join("\n"))) : null,
      done || failedState ? h("div", { class: "page-actions" },
        done && safeLocalURL(repository?.url, null) ? link("Open repository", safeLocalURL(repository.url, "/"), true) : null,
        button(done ? "Add another" : "Start over", () => { resetAdd(); state.add.path = ""; state.add.source = ""; paint("hub-add"); byId("add-path")?.focus(); })) : null);
  }

  function inspectBox(found) {
    const mode = state.add.mode;
    const kind = text(found.kind);
    const accepts = array(found.actions).filter((action) => typeof action === "string");
    const offered = (action) => button(ACTION_WORDS[action][0], () => runAdd(action), { class: "button button-primary", disabled: Boolean(state.add.busy) });
    const actions = [];
    if (safeLocalURL(found.repository?.url, null)) actions.push(link("Open repository", safeLocalURL(found.repository.url, "/"), true));
    else if (accepts.includes("open")) actions.push(offered("open"));
    else if (mode === "convert" && accepts.includes("convert")) actions.push(offered("convert"));
    else if (mode === "create" && accepts.includes("create")) actions.push(offered("create"));
    const inside = found.inside && safeLocalURL(found.inside.url, null) ? found.inside : null;
    const commits = found.git?.commits;
    const facts = [text(found.location) || text(found.path), isCount(commits) ? `${plural(commits, "commit")} of history` : "", text(found.git?.branch) ? `on ${found.git.branch}` : ""].filter(Boolean);
    return h("div", { class: "inspect" },
      h("div", { class: "inspect-title" }, text(found.name) || "This place", pill(found.registered ? "Already in Kin" : KIND_WORDS[kind] || "Unknown", found.registered || kind === "kin" ? "accent" : kind === "missing" || kind === "not_a_directory" ? "warn" : "")),
      h("p", { class: "mono note" }, facts.join(" · ")),
      text(found.note) ? h("p", {}, found.note) : null,
      inside ? h("p", {}, "It sits inside ", h("a", { class: "link", href: safeLocalURL(inside.url, "/") }, text(inside.name) || "a repository"), ", which is already in Kin.") : null,
      actions.length ? h("div", { class: "page-actions" }, actions) : null);
  }

  function addPanel() {
    const add = state.add;
    if (!add.open) return null;
    const clone = add.mode === "clone";
    const locked = Boolean(add.busy) || add.operation?.state === "running";
    const ready = () => Boolean(add.path.trim()) && (!clone || Boolean(add.source.trim()));
    const change = (field) => (event) => {
      add[field] = event.target.value;
      if (add.inspect || add.error) { add.inspect = null; add.error = null; paint("hub-add"); return; }
      const submit = byId("add-submit");
      if (submit) submit.disabled = locked || !ready();
    };
    const path = h("input", { id: "add-path", type: "text", value: add.path, disabled: locked, autocomplete: "off", spellcheck: "false", autocapitalize: "off",
      placeholder: clone ? "~/code/project" : add.mode === "create" ? "~/code/new-project" : "~/code/project", on: { input: change("path") } });
    const source = clone ? h("input", { id: "add-source", type: "text", value: add.source, disabled: locked, autocomplete: "off", spellcheck: "false", autocapitalize: "off", placeholder: "Where to clone from", on: { input: change("source") } }) : null;
    const help = { convert: "Kin reads the project where it is, keeps its Git history and builds its graph. Nothing in the project is moved or rewritten.", create: "Kin starts an empty repository there. The place must be empty or not exist yet.", clone: "Kin copies a repository from somewhere else and builds its graph here." }[add.mode];
    return h("section", { class: "add", "aria-labelledby": "add-title" },
      h("div", { class: "add-head" }, h("h2", { id: "add-title" }, "Add a repository"),
        h("div", { class: "segmented", role: "group", "aria-label": "What to add" }, ADD_MODES.map(([value, label]) => h("button", { type: "button", "aria-pressed": String(add.mode === value), disabled: locked,
          on: { click: () => { resetAdd(); state.add.mode = value; paint("hub-add"); byId(value === "clone" ? "add-source" : "add-path")?.focus(); } } }, label))),
        button("Close", () => { if (state.add.operation?.state === "running") return; resetAdd(false); paint("hub-add"); paint("hub-add-toggle"); if (location.hash.replace(/^#\/?/, "") === "add") location.hash = "#/"; }, { class: "button button-quiet button-small add-close" })),
      h("p", { class: "add-help" }, help),
      h("form", { class: "add-form", on: { submit: (event) => { event.preventDefault(); if (clone) runAdd("clone"); else inspectPath(); } } },
        source ? h("div", { class: "field" }, h("label", { for: "add-source" }, "Clone from"), source) : null,
        h("div", { class: "field" }, h("label", { for: "add-path" }, clone ? "Put it here on this machine" : add.mode === "create" ? "Where to create it" : "Where the project is"), path),
        h("button", { type: "submit", id: "add-submit", class: `button${clone ? " button-primary" : ""}`, disabled: locked || !ready() }, add.busy === "inspect" ? "Checking…" : add.busy === "clone" ? "Starting…" : clone ? "Clone" : "Check")),
      add.error ? h("div", { class: "message message-bad section-tight", role: "alert" }, h("div", {}, h("strong", {}, "Kin couldn't do that"), h("p", {}, add.error))) : null,
      add.operation ? operationBox(add.operation) : add.inspect ? inspectBox(add.inspect) : null,
      !add.operation && !add.inspect && add.mode === "convert" ? section("hub-candidates", [], candidateList) : null);
  }

  /** Projects Kin found on this machine that are not in it yet, most recently worked on first. */
  function candidateList() {
    if (candidates.missing) return null;
    if (!candidates.data) { loadCandidates(); return h("p", { class: "note aside-reading" }, "Looking for projects on this machine"); }
    const rows = array(candidates.data.candidates).filter((row) => row && typeof row.name === "string" && typeof row.path === "string" && !row.registered && array(row.actions).some((action) => action === "convert" || action === "create"));
    if (!rows.length) return null;
    const locked = Boolean(state.add.busy) || state.add.operation?.state === "running";
    return h("div", { class: "candidates" },
      h("h3", { class: "sites-title" }, "Projects on this machine"),
      h("ul", { class: "list list-card" }, rows.slice(0, 12).map((row) => h("li", { class: "branch-row" },
        h("div", {}, h("div", { class: "branch-name" }, row.name, text(row.git?.branch) ? h("span", { class: "pill" }, row.git.branch) : null),
          h("div", { class: "branch-meta" }, [row.kind === "git" ? (isCount(row.git?.commits) ? `${plural(row.git.commits, "commit")} of history` : "Git project") : "No Git history", row.modified_at ? `worked on ${ago(row.modified_at)}` : ""].filter(Boolean).join(" · "))),
        h("div", { class: "branch-actions" }, button("Add", () => { state.add.path = row.path; state.add.inspect = row; runAdd(array(row.actions).includes("convert") ? "convert" : "create"); }, { class: "button button-small", disabled: locked }))))));
  }

  // ── Home ─────────────────────────────────────────────────────────────────
  // What needs you, what your agents are doing and what each repository
  // holds, across this machine. The list of repositories is read without
  // waking any of them; Home then opens a few itself, the most recently
  // changed first, so each can say what is in it. The rest open when you go
  // to them.

  const HOME_OPEN = 6;
  /** How many of its newest changes a repository's summary carries, which is where Home finds an agent's. */
  const OVERVIEW_CHANGES = 5;
  const HOME_PARTS = { overview: "/api/workspace/overview", agents: "/api/workspace/agents", working: "/api/guide?working=1", standing: "/api/standing" };
  const homeDetail = new Map();
  const heldWorkingKey = (key) => `kin.workspace.working:/repositories/${key}`;
  // What Home last read of each repository's summary, agents and review
  // standing is kept for this tab, so a reload, or a repository that is slow
  // to answer, shows what was last known rather than nothing until it does.
  const heldDetailKey = (key) => `kin.workspace.home-detail:${key}`;
  function keepDetail(key, detail) {
    try {
      const body = JSON.stringify({ at: Date.now(), overview: detail.overview, agents: detail.agents, standing: detail.standing });
      if (body.length < 300_000) sessionStorage.setItem(heldDetailKey(key), body); else sessionStorage.removeItem(heldDetailKey(key));
    } catch (_) { /* The page still holds it. */ }
  }
  /**
   * A summary read again keeps any part of the one before that it could not
   * read this time, so a part that timed out under load does not take what
   * was known with it.
   */
  function keepGoodSections(next, previous) {
    if (!previous || typeof previous !== "object" || !next || typeof next !== "object") return next;
    const out = { ...next };
    for (const [name, part] of Object.entries(previous)) {
      const fresh = next[name];
      if (section_ok(part) && part && typeof part === "object" && "ok" in part && fresh && typeof fresh === "object" && fresh.ok === false) out[name] = { ...part, kept: true };
    }
    return out;
  }
  /** The changes agents made among a repository's recent ones, newest first, past Kin's own updates. */
  function agentChanges(overview) {
    const recent = overview?.recent_changes;
    if (!section_ok(recent)) return [];
    return array(recent.items).filter((change) => change && !isKinUpdate(change) && change.agent && typeof change.id === "string");
  }
  /**
   * The agent changes among a repository's recent ones that nobody has
   * reviewed, once Home has read its review standing. Any of them, not only
   * the newest: a change of yours on top does not review the agent's.
   */
  function unreviewedAgentChanges(row) {
    const detail = homeDetail.get(row.key);
    const standing = detail?.standing?.standing;
    if (!standing || typeof standing !== "object") return [];
    return agentChanges(detail?.overview).filter((change) => (standing[change.id]?.state || "pending") === "pending");
  }
  function detailFor(key) {
    let detail = homeDetail.get(key);
    if (detail) return detail;
    detail = { overview: null, agents: null, working: null, standing: null, stale: false, clean: false, busy: new Set(), failed: {}, sigs: {}, chain: Promise.resolve() };
    // The last reading of its uncommitted work this tab saw, shown while a new one is read.
    try {
      const held = JSON.parse(sessionStorage.getItem(heldWorkingKey(key)));
      if (held?.data?.summary) { detail.working = held.data; detail.stale = true; }
    } catch (_) { /* Nothing kept; the read answers. */ }
    // Its summary and review standing as this tab last read them. Who was connected then is not
    // carried over a reload, since it is not known to hold now; who worked here earlier is.
    try {
      const kept = JSON.parse(sessionStorage.getItem(heldDetailKey(key)));
      if (kept?.overview && typeof kept.overview === "object") detail.overview = kept.overview;
      if (kept?.standing?.standing && typeof kept.standing.standing === "object") detail.standing = kept.standing;
      if (kept?.agents && typeof kept.agents === "object") detail.agents = { ...kept.agents, agents: [], restored: true };
    } catch (_) { /* Nothing kept; the reads answer. */ }
    homeDetail.set(key, detail);
    return detail;
  }
  const repoHref = (row, hash = "") => `${safeLocalURL(row.url, "/")}${hash}`;
  const homeListed = () => homeRows().filter((row) => row && row.available !== false && /^[0-9a-f]{64}$/.test(text(row.key)));
  /** The repositories Home reads in full: the ones already open, then the most recently changed. */
  function homeWanted() {
    const recent = (row) => Date.parse(row.last_change_at || "") || 0;
    return [...homeListed()].sort((a, b) => Number(b.runtime === "running") - Number(a.runtime === "running") || recent(b) - recent(a)).slice(0, HOME_OPEN);
  }

  /** Ask one repository for one part of what Home shows. A repository's reads run one after another. */
  function askDetail(row, part) {
    const detail = detailFor(row.key);
    if (detail.busy.has(part)) return;
    detail.busy.add(part);
    const epoch = state.readEpoch;
    detail.chain = detail.chain.then(async () => {
      let moved = false;
      try {
        const answer = await fetchJSON(`root:/repositories/${row.key}${HOME_PARTS[part]}`);
        if (epoch !== state.readEpoch) return;
        if (part === "working" && !answer.summary) throw new Error("Uncommitted work was not returned in a readable form.");
        const data = part === "overview" ? keepGoodSections(answer, detail.overview) : answer;
        const sig = JSON.stringify({ ...data, read_at: null });
        moved = sig !== detail.sigs[part] || Boolean(detail.failed[part]) || (part === "working" && detail.stale);
        detail.sigs[part] = sig;
        detail[part] = data;
        delete detail.failed[part];
        if (part === "standing") detail.standingAt = Date.now();
        if (part === "agents") detail.agentsAt = Date.now();
        if (part === "overview" && agentChanges(data).length && (moved || !detail.standing)) askDetail(row, "standing");
        // A change the agents say they committed waits on how its review stands, as one the summary lists does.
        if (part === "agents" && agentsCommitted(row).length && (moved || !detail.standing)) askDetail(row, "standing");
        // What waits for a review is kept for this tab, so a repository's own page says the same of it.
        if (part === "overview" && !agentChanges(data).length) keepAwaiting(row.key, []);
        else if (detail.standing && ["overview", "standing"].includes(part)) keepAwaiting(row.key, unreviewedAgentChanges(row).map((change) => change.id));
        if (part !== "working") keepDetail(row.key, detail);
        if (part === "working") {
          detail.stale = false;
          detail.clean = !hasWork(data);
          try { const body = JSON.stringify({ at: Date.now(), data }); if (body.length < 600_000) sessionStorage.setItem(heldWorkingKey(row.key), body); } catch (_) { /* The page still holds it. */ }
        }
      } catch (error) {
        if (epoch !== state.readEpoch) return;
        moved = detail.failed[part] !== (error.message || "failed");
        detail.failed[part] = error.message || "This read could not be completed.";
      } finally {
        detail.busy.delete(part);
        if (epoch === state.readEpoch && (moved || !detail.busy.size)) repaintHome();
      }
    });
  }

  /** Read again whatever the list says has moved in each repository Home shows. */
  function loadHomeDetails() {
    if (!MACHINE || state.route !== "hub") return;
    if (!read("home").data) { repaintHome(); return; }
    for (const row of homeWanted()) {
      const detail = detailFor(row.key);
      const sig = JSON.stringify([row.working || null, row.agents || null, row.last_change_at || null, row.branch || null, row.runtime || null]);
      const first = detail.rowSig === undefined;
      const moved = detail.rowSig !== sig;
      detail.rowSig = sig;
      if (first || (moved && row.last_change_at !== detail.lastChange)) askDetail(row, "overview");
      // A review recorded elsewhere moves no change, so what nobody has reviewed is read again now and then.
      else if (detail.standing && unreviewedAgentChanges(row).length && Date.now() - (detail.standingAt || 0) > 30_000) askDetail(row, "standing");
      detail.lastChange = row.last_change_at;
      const connected = (row.agents?.connected || 0) + (row.agents?.active || 0) > 0;
      if (first || moved || connected) askDetail(row, "agents");
      // An agent's change waiting for review is drawn on its card; a read that failed is tried again now and then.
      const waiting = awaitingChanges(row)[0];
      if (waiting && repoWork(row).state !== "work" && !row.merge_in_progress) askChange(row, waiting.id);
      // The list says a repository is clean without anyone reading its work again.
      const told = row.working && row.working.unknown !== true ? row.working : null;
      if (told && !told.differs && !(told.changed > 0)) {
        if (detail.working || !detail.clean) { detail.working = null; detail.stale = false; detail.clean = true; detail.sigs.working = undefined; }
      } else if (first || moved) askDetail(row, "working");
    }
    repaintHome();
  }

  const homeSigs = {};
  /**
   * Draw again each part of Home whose own facts changed, and leave the rest
   * as it stands. Every part's facts are read from one snapshot of what Home
   * knows, the same one each part is drawn from, so a part is never left
   * saying something another part no longer says.
   */
  function homeFacts() {
    const snap = homeSnapshot();
    const minute = Math.floor(Date.now() / 60_000);
    const facts = (view) => {
      const detail = homeDetail.get(view.key);
      return [view.key, view.row.branch, view.row.name, view.work.state, view.work.changed ?? null, Boolean(view.work.stale), detail?.sigs.working?.length, detail?.busy.has("working"), detail?.failed.working || null,
        Boolean(view.row.merge_in_progress), view.awaiting.map((change) => change.id).join(","), changeFacts(view.row, view.awaiting[0]?.id), view.unreachable ? view.knownAt : null, placesRead];
    };
    const agents = snap.views.map((view) => [view.key, view.agents.active, view.agents.connected, view.agents.named, view.agents.listed, homeDetail.get(view.key)?.sigs.agents, homeDetail.get(view.key)?.sigs.standing]);
    const shape = snap.views.map((view) => { const detail = homeDetail.get(view.key); return [view.key, view.row.branch, view.row.runtime, view.row.entities, view.row.last_change_at, detail?.sigs.overview?.length, view.work.state, view.work.changed ?? null, view.row.activity?.phase || null, view.awaiting.length, view.agents.active, view.agents.named, view.unreachable ? view.knownAt : null]; });
    return {
      "home-headline": JSON.stringify([Boolean(read("home").data), read("home").error, snap.views.length, snap.needs.map((view) => [view.key, view.row.name]), snap.active, snap.connected, snap.unreachable.map((view) => [view.key, view.knownAt])]),
      "home-ask-box": JSON.stringify(snap.views.length > 0),
      "home-attention": JSON.stringify(snap.needs.map(facts)),
      "home-agents": JSON.stringify([agents, minute]),
      "home-repos": JSON.stringify([shape, minute, read("home").data?.unavailable, read("home").data?.warning]),
      "home-recent": JSON.stringify([shape.map((held) => [held[0], held[5]]), minute]),
    };
  }
  function repaintHome() {
    if (!MACHINE || state.route !== "hub" || !state.sections.has("home-headline")) return;
    let drew = false;
    for (const [id, sig] of Object.entries(homeFacts())) {
      if (homeSigs[id] === sig) continue;
      homeSigs[id] = sig;
      paint(id);
      drew = true;
    }
    if (drew) paintSide();
  }

  /** Where a repository's uncommitted work stands, from the fullest reading Home has of it. */
  function repoWork(row) {
    const detail = homeDetail.get(row.key);
    const guide = detail?.working || null;
    if (guide && hasWork(guide)) return { state: "work", changed: workChanged(guide), guide, stale: detail.stale };
    const told = row.working && row.working.unknown !== true ? row.working : null;
    if (told && told.changed > 0) return { state: "work", changed: null, guide: null };
    if (detail?.clean) return { state: "clean" };
    // Kin's quick reading says the code differs before it has counted what changed;
    // until the count arrives that may be only Kin's own linking, so nothing is asked of you yet.
    if (told && told.differs && !isCount(told.changed)) return { state: "reading" };
    if (told) return { state: "clean" };
    if (detail?.busy.has("working") || detail?.busy.has("overview")) return { state: row.runtime === "running" ? "reading" : "opening" };
    return { state: row.runtime === "running" ? "unknown" : "closed" };
  }
  /** How much a repository's waiting work asks of you: left behind first, then what may need review, then untested. */
  function needWeight(row) {
    const summary = repoWork(row).guide?.summary || {};
    const num = (value) => isCount(value) ? value : 0;
    return [row.merge_in_progress ? 1 : 0, num(summary.consumers_not_updated), num(summary.consumers_may_need_review), num(summary.recorded_tests) ? 0 : 1, num(summary.added) + num(summary.modified) + num(summary.removed)];
  }
  /**
   * A repository's agent changes nobody has reviewed, among the newest Home
   * read: by the review standing Home read, or until that is read, by what
   * this tab last knew of it. Every part of Home counts these the same way.
   */
  function awaitingChanges(row) {
    const detail = homeDetail.get(row.key);
    const changes = [...agentChanges(detail?.overview), ...agentsCommitted(row)];
    if (!changes.length) return [];
    const standing = detail?.standing?.standing;
    if (standing && typeof standing === "object") return changes.filter((change) => (standing[change.id]?.state || "pending") === "pending");
    const kept = new Set(keptAwaiting(row.key));
    return changes.filter((change) => kept.has(change.id));
  }
  /** How long ago a change an agent says it committed may be, to be waiting on the reader before the repository's summary lists it. */
  const LISTED_CHANGE_WINDOW = 86_400_000;
  /**
   * The changes the agents a repository lists say they committed, that are
   * newer than every change its summary lists. A busy daemon can answer who
   * worked here well before it can read its own recent history, and an
   * agent's commit waits on the reader from the moment it lands.
   */
  function agentsCommitted(row) {
    const detail = homeDetail.get(row.key);
    const data = detail?.agents;
    if (!data) return [];
    const listed = new Set(array(detail.overview?.recent_changes?.items).map((change) => text(change?.id)));
    const newest = Math.max(0, ...array(detail.overview?.recent_changes?.items).map((change) => Date.parse(change?.time || change?.timestamp || "") || 0));
    const seen = new Set();
    const out = [];
    for (const agent of [...array(data.agents), ...array(data.earlier)]) {
      for (const change of array(agent?.changes)) {
        const id = text(change?.id);
        if (!/^[0-9a-f]{64}$/i.test(id) || listed.has(id) || seen.has(id)) continue;
        const at = Date.parse(change.time || "") || Date.parse(agent.last_activity_at || "") || 0;
        if (at <= newest || Date.now() - at > LISTED_CHANGE_WINDOW) continue;
        seen.add(id);
        out.push({ id, title: text(change.title), time: change.time || agent.last_activity_at, agent: { name: text(agent.name) || "An agent", key: agent.key }, listed: true });
      }
    }
    return out;
  }
  /** How the reader left an agent's change, as Home last read it; null until Home has read the repository's standing. */
  function reviewStateIn(row, id) {
    const standing = homeDetail.get(row.key)?.standing?.standing;
    if (!standing || typeof standing !== "object") return null;
    return text(standing[id]?.state) || "pending";
  }
  /** How long a repository's own list of agents stays the newer word after the machine's list is read again. */
  const AGENTS_LEAD = 15_000;
  /**
   * Who is connected in a repository, from the newer of two readings: its own
   * list of agents, which names each session and what it did, and the
   * machine's list, which only counts them. A daemon that is not running
   * holds no agent session.
   */
  function repoAgents(row) {
    const detail = homeDetail.get(row.key);
    const earlier = array(detail?.agents?.earlier);
    const names = detail?.agents?.names || {};
    if (row.reach === "stopped") return { live: [], earlier, names, active: 0, connected: 0, named: [], listed: false };
    const data = detail?.agents && !detail.agents.restored ? detail.agents : null;
    const listAt = Date.parse(text(row.known_at)) || read("home").at || 0;
    if (data && Array.isArray(data.agents) && (!row.agents || (detail.agentsAt || 0) >= listAt - AGENTS_LEAD)) {
      const live = data.agents.filter((agent) => agent && typeof agent.name === "string");
      return { live, earlier, names, active: live.filter((agent) => agent.state === "active").length, connected: live.length, named: [...new Set(live.map((agent) => agent.name))], listed: true };
    }
    const told = row.agents || {};
    return { live: [], earlier, names, active: isCount(told.active) ? told.active : 0, connected: isCount(told.connected) ? told.connected : 0, named: array(told.names).filter((name) => typeof name === "string"), listed: false };
  }
  /** One repository as every part of Home says it. */
  function repoView(row) {
    const work = repoWork(row);
    const awaiting = awaitingChanges(row);
    const agents = repoAgents(row);
    const knownAt = text(row.known_at);
    // What a repository's daemon last said, when it is not answering now or has stopped.
    const unreachable = ["not_answering", "stopped"].includes(text(row.reach)) && !Number.isNaN(Date.parse(knownAt));
    const waiting = work.state === "work" || Boolean(row.merge_in_progress);
    return { row, key: row.key, work, awaiting, agents, knownAt, unreachable, needs: waiting || awaiting.length > 0, tone: waiting ? "work" : awaiting.length ? "review" : agents.active > 0 ? "live" : "" };
  }
  const homeView = (row) => (MACHINE && row ? repoView(row) : null);
  /**
   * Everything Home says, read at once: each repository's view, what needs
   * the reader, who is at work, and which repositories Kin cannot reach.
   * Every part of Home and the sidebar is drawn from this.
   */
  function homeSnapshot() {
    const views = homeListed().map(repoView);
    const needs = views.filter((view) => view.needs).sort((a, b) => { const [x, y] = [needWeight(a.row), needWeight(b.row)]; for (let i = 0; i < x.length; i++) if (x[i] !== y[i]) return y[i] - x[i]; return 0; });
    const live = [], earlier = [];
    for (const view of views) {
      const { agents } = view;
      for (const agent of agents.live) live.push({ agent, row: view.row, names: agents.names });
      // An agent the machine's list counts before its repository has named it is still said, by the name the list gives.
      if (!agents.listed) for (let at = 0; at < agents.connected; at++) live.push({ agent: { name: agents.named[at] || agents.named[0] || "An agent", state: at < agents.active ? "active" : "idle", counted: true }, row: view.row, names: {} });
      for (const agent of agents.earlier) earlier.push({ agent, row: view.row, names: agents.names });
    }
    const when = (held) => Date.parse(held.agent.last_activity_at || "") || 0;
    live.sort((a, b) => when(b) - when(a));
    earlier.sort((a, b) => when(b) - when(a));
    return { views, needs, live, earlier, active: views.reduce((sum, view) => sum + view.agents.active, 0), connected: views.reduce((sum, view) => sum + view.agents.connected, 0), unreachable: views.filter((view) => view.unreachable) };
  }
  const homeNeeds = () => homeSnapshot().needs.map((view) => view.row);
  /** Every agent session Home knows of, each with the repository it is in. */
  function homeSessions() {
    const { live, earlier } = homeSnapshot();
    return { live, earlier };
  }
  /** When Home last heard from a repository, as a time of day, with its date when that was not today. */
  function knownWhen(at) {
    const date = new Date(at);
    if (Number.isNaN(date.getTime())) return "";
    return date.toDateString() === new Date().toDateString() ? clock(date) : date.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
  }
  /** What Home says of a repository it shows from what Kin last knew. */
  const unreachableWords = (view) => `Kin can't reach ${repositoryLabel(view.row, " / ")} right now; showing what it knew at ${knownWhen(view.knownAt)}.`;

  // One question, put to every repository Home has open, answered by each from its own graph.
  const homeAsk = { query: "", request: 0, timer: null, asked: [], answers: new Map(), busy: false, opened: new Set() };
  /** How many matches Home asks each repository for, so names repeated across places can fold and still leave five. */
  const HOME_ASK_LIMIT = 10;
  const HOME_ASK_SHOWN = 5;
  // Words a question is phrased with that no entity is named for.
  const ASK_FILLER = new Set(["a", "an", "the", "how", "does", "do", "did", "is", "are", "was", "were", "be", "been", "where", "what", "which", "who", "when", "why",
    "it", "its", "in", "on", "of", "to", "for", "from", "by", "with", "and", "or", "that", "this", "there", "get", "gets", "got", "can", "we", "i", "my", "our", "any", "all"]);
  /** A word cut back to what the forms of it share: `serialized` and `serialization` both begin `serial`. */
  const askStem = (word) => word.length <= 4 ? word : word.endsWith("ies") ? `${word.slice(0, -3)}y` : word.replace(/(?:izations?|ations?|ings?|ized|izes|ize|ed|es|s)$/, "") || word;
  /** The words of a question that could name an entity. */
  const askTerms = (query) => [...new Set(text(query).toLowerCase().split(/[^a-z0-9]+/).filter((word) => word.length > 1 && !ASK_FILLER.has(word)).map(askStem).filter((word) => word.length >= 3))];
  /** An entity's name as the words it is made of: `_ZodString.lowercase` is zod, string and lowercase. */
  const nameWords = (name) => shownName(name).replace(/([a-z0-9])([A-Z])/g, "$1 $2").replace(/([A-Z]+)([A-Z][a-z])/g, "$1 $2").toLowerCase().split(/[^a-z0-9]+/).filter(Boolean);
  /** How many of a question's words an entity's own name holds, whole or begun. */
  function termsNamed(row, terms) {
    const words = nameWords(row?.name);
    const joined = words.slice(1).map((word, at) => `${words[at]}${word}`);
    return terms.filter((term) => [...words, ...joined].some((word) => word.startsWith(term) || (word.length >= 4 && term.startsWith(word)))).length;
  }
  /** Rows for entities of one name and kind in different places, read as the first with a count of the rest. */
  function foldAlike(rows) {
    const at = new Map();
    const out = [];
    for (const row of rows) {
      const key = `${shownName(row.name)}\u0000${text(row.kind)}`;
      const held = at.get(key);
      if (held) { held.alike += row.overloads || 1; continue; }
      const kept = { ...row, alike: 0 };
      at.set(key, kept);
      out.push(kept);
    }
    return out;
  }
  /**
   * How strongly a repository answered: a chain Kin traced through its graph
   * first, then matches whose own names hold the question's words, the more
   * of them the stronger. A repository whose matches name none of the
   * question's words matched only loosely, by meaning.
   */
  /** How many of a question's words a name must hold to answer it: half of them, and at least one. */
  const termsNeeded = (terms) => Math.max(1, Math.ceil(terms.length / 2));
  function answerStrength(answer, terms) {
    if (!answer) return { state: "pending" };
    if (answer.error) return { state: "failed" };
    const rows = array(answer.groups).flatMap((group) => group.rows);
    if (answer.answer) return { state: "strong", best: Infinity, strong: rows.length, rows };
    // Nothing found by a search that only half answered is a question not asked, never a "nothing matches".
    if (!rows.length) return { state: answer.partial ? "failed" : "empty" };
    const named = rows.map((row) => termsNamed(row, terms));
    const best = Math.max(0, ...named);
    const needed = termsNeeded(terms);
    return { state: !terms.length || best >= needed ? "strong" : "weak", best, strong: named.filter((n) => n >= needed).length, rows };
  }
  function queueHomeAsk(value) {
    homeAsk.query = value;
    clearTimeout(homeAsk.timer);
    homeAsk.timer = setTimeout(runHomeAsk, 320);
  }
  async function runHomeAsk() {
    clearTimeout(homeAsk.timer);
    const query = homeAsk.query.trim();
    const mine = ++homeAsk.request;
    homeAsk.answers = new Map();
    homeAsk.opened = new Set();
    const body = byId("home-body");
    if (body) body.hidden = Boolean(query);
    if (!query) { homeAsk.busy = false; homeAsk.asked = []; paint("home-ask"); return; }
    const rows = homeWanted();
    homeAsk.asked = rows.map((row) => row.key);
    homeAsk.busy = true;
    paint("home-ask");
    await Promise.all(rows.map(async (row) => {
      try {
        const data = await fetchJSON(`root:/repositories/${row.key}/api/workspace/ask?q=${encodeURIComponent(query)}&limit=${HOME_ASK_LIMIT}`);
        if (mine !== homeAsk.request) return;
        homeAsk.answers.set(row.key, askGroups(data));
      } catch (error) {
        if (mine !== homeAsk.request) return;
        homeAsk.answers.set(row.key, { error: error.message || "This repository could not answer." });
      }
      paint("home-ask");
    }));
    if (mine !== homeAsk.request) return;
    homeAsk.busy = false;
    paint("home-ask");
    const found = [...homeAsk.answers.values()].reduce((sum, answer) => sum + array(answer.groups).reduce((n, group) => n + group.rows.length, 0), 0);
    announce(found ? `${plural(found, "result")} across ${plural(rows.length, "repository", "repositories")}.` : "No results.");
  }
  function homeAskBox() {
    return h("div", { class: "ask-box home-ask-box" }, icon("search"),
      h("input", { id: "home-ask-input", class: "ask-input", type: "search", autocomplete: "off", spellcheck: "false", maxlength: "256", value: homeAsk.query,
        placeholder: "Ask your code, across every repository", "aria-label": "Ask every repository on this machine by meaning or by name", "aria-controls": "home-ask",
        on: { input: (event) => queueHomeAsk(event.target.value),
          keydown: (event) => {
            if (event.key === "Enter") { event.preventDefault(); homeAsk.query = event.target.value; runHomeAsk(); }
            if (event.key === "Escape") { event.preventDefault(); event.target.value = ""; homeAsk.query = ""; runHomeAsk(); }
          } } }),
      h("kbd", { "aria-hidden": "true" }, "/"));
  }
  function homeAskResults() {
    const query = homeAsk.query.trim();
    if (!query || !homeAsk.asked.length) return null;
    const rows = homeListed().filter((row) => homeAsk.asked.includes(row.key));
    const terms = askTerms(query);
    const strengths = new Map(rows.map((row) => [row.key, answerStrength(homeAsk.answers.get(row.key), terms)]));
    const of = (row) => strengths.get(row.key);
    // A traced answer first, then the strongest matches; a repository still thinking waits below them,
    // and one that matched only loosely is one line under those.
    const ORDER = { strong: 0, pending: 1, weak: 2, empty: 3, failed: 4 };
    const ordered = rows.map((row, at) => ({ row, at })).sort((a, b) => ORDER[of(a.row).state] - ORDER[of(b.row).state]
      || (of(b.row).best ?? 0) - (of(a.row).best ?? 0) || (of(b.row).strong ?? 0) - (of(a.row).strong ?? 0) || a.at - b.at).map(({ row }) => row);
    const inState = (state) => ordered.filter((row) => of(row).state === state);
    const empty = inState("empty");
    const failedRows = inState("failed");
    const names = (list) => list.map((row) => repositoryLabel(row, " / ")).join(", ");
    const more = (row) => h("a", { class: "more-link", href: repoHref(row, `#/explore?q=${encodeURIComponent(query)}`) }, "More here", icon("arrow"));
    const full = (row) => {
      const answer = homeAsk.answers.get(row.key);
      const results = answer ? foldAlike(of(row).rows) : [];
      // In a repository that answered by name, the rows whose names hold the question's words, or that Kin
      // matched by name, lead, and its looser matches are one line under them until the reader asks for them.
      const opened = homeAsk.opened.has(`${row.key}:loose`) || homeAsk.opened.has(row.key);
      const named = answer && !answer.answer && terms.length && of(row).state === "strong" && !opened ? results.filter((result) => termsNamed(result, terms) >= termsNeeded(terms) || array(result.matched).includes("name")) : results;
      const looser = results.filter((result) => !named.includes(result));
      const shown = named.slice(0, HOME_ASK_SHOWN);
      return h("section", { class: "home-answer" },
        h("div", { class: "section-head" }, h("h2", {}, repositoryLabel(row, " / ")), answer ? more(row) : null),
        answer ? [answerChain(answer.answer, repoHref(row)), h("ul", { class: "list list-card" }, shown.map((result) => entityRow(result, 0, repoHref(row)))),
          looser.length ? h("p", { class: "home-answer-loose home-answer-looser" },
            h("span", { class: "home-answer-loose-text" }, `Also ${plural(looser.length + looser.reduce((sum, result) => sum + result.alike, 0), "loose match", "loose matches")}: ${looser.slice(0, 2).map((result) => shownName(result.name)).join(", ")}${looser.length > 2 ? ` and ${count(looser.length - 2)} more` : ""}.`),
            button("Show them", () => { homeAsk.opened.add(`${row.key}:loose`); paint("home-ask"); }, { class: "button button-small button-quiet" })) : null] : skeletonLines(3));
    };
    const loose = (row) => {
      const shown = foldAlike(of(row).rows);
      const first = shown.slice(0, 2).map((result) => shownName(result.name));
      const rest = shown.slice(2).length + shown.reduce((sum, result) => sum + result.alike, 0);
      return h("p", { class: "home-answer-loose" },
        h("span", { class: "home-answer-loose-name" }, repositoryLabel(row, " / ")),
        h("span", { class: "home-answer-loose-text" }, `Only loose matches: ${first.join(", ")}${rest ? ` and ${count(rest)} more` : ""}.`),
        button("Show them", () => { homeAsk.opened.add(row.key); paint("home-ask"); }, { class: "button button-small button-quiet" }));
    };
    return h("div", { class: "home-answers", role: "region", "aria-label": "Answers" },
      [...inState("strong"), ...inState("pending")].map(full),
      inState("weak").length ? h("div", { class: "home-answers-loose" }, inState("weak").map((row) => homeAsk.opened.has(row.key) ? full(row) : loose(row))) : null,
      empty.length && !homeAsk.busy ? h("p", { class: "note quiet-line" }, empty.length === rows.length ? "Nothing in these repositories matches that." : `Nothing in ${names(empty)} matches.`) : null,
      failedRows.length && !homeAsk.busy ? h("p", { class: "note quiet-line" }, failedRows.length === rows.length ? "Kin couldn't ask these repositories just now. " : `Kin couldn't ask ${names(failedRows)} just now. `,
        button("Try again", runHomeAsk, { class: "button button-small" })) : null);
  }

  function homeHeadline() {
    const resource = read("home");
    if (!resource.data) return resource.error ? h("div", { class: "section-tight" }, readFailure("home")) : [skeleton("skeleton-title"), skeleton("skeleton-lede")];
    const rows = homeListed();
    if (!rows.length) return [h("h1", { class: "home-headline" }, "Bring your first project into Kin."),
      h("p", { class: "home-sub" }, "Kin converts a project you already have, keeps its history, and maps how its code connects. Your project stays where it is."),
      state.add.open ? null : h("div", { class: "page-actions home-start" }, button("Add a project", () => openAdd("convert"), { class: "button button-primary" }))];
    // Every count here is the snapshot's, the one the cards, the agents and the sidebar are drawn from.
    const snap = homeSnapshot();
    const needs = snap.needs.map((view) => view.row);
    const headline = needs.length === 1 ? `${repositoryLabel(needs[0], " / ")} needs you.` : needs.length ? `${spoken(needs.length)} repositories need you.` : "Nothing needs you right now.";
    const { active, connected } = snap;
    const agents = active ? `${spoken(active)} ${active === 1 ? "agent is" : "agents are"} working.`
      : connected ? `${spoken(connected)} ${connected === 1 ? "agent is connected and idle." : "agents are connected and idle."}` : "No agent is connected.";
    return [h("h1", { class: "home-headline" }, headline),
      h("p", { class: "home-sub" }, agents, ` ${spoken(rows.length)} ${rows.length === 1 ? "repository" : "repositories"} on this machine.`),
      // A repository Kin cannot reach is shown as it was last known, and said so here once.
      snap.unreachable.length ? h("p", { class: "note home-known", role: "status" }, snap.unreachable.map(unreachableWords).join(" ")) : null];
  }

  /**
   * The picture of a piece of work: what changed on one side, what relies on
   * it and was left as it was on the other, joined where one relies on the
   * other. Every node is an entity the guide named and every line a
   * relationship it named. Links open in `base`, the repository's own page.
   */
  function impactPicture(guide, onCleanup, { base = "", api = "", shown = 6, walk = "#/review/w", marks = false, more = null } = {}) {
    const steps = reviewSteps(guide);
    if (!steps.length) return null;
    const TONE = ["bad", "warn", "quiet"];
    const listed = steps.slice(0, shown);
    const outside = new Map();
    const pairs = [];
    const left = listed.map((step) => {
      const at = steps.indexOf(step);
      const tone = TONE[stepAttention(step)];
      const tag = step.change === "added" ? "New" : step.change === "removed" ? "Removed" : array(step.fields).some((field) => text(field?.field) === "signature") ? "Signature changed" : "";
      const node = h("a", { class: "impact-node", "data-tone": tone, href: `${base}${walk}/${at + 1}`, title: step.name },
        h("span", { class: "impact-dot", "aria-hidden": "true" }),
        h("span", { class: "impact-text" }, h("span", { class: "impact-name" }, step.name), h("span", { class: "impact-place" }, [placeOf(step), tag, [...new Set(authorsOf(step).map((held) => held.name))].join(" and ")].filter(Boolean).join(" · "))),
        marks && markState(step) ? h("span", { class: `state impact-mark ${markState(step) === "flagged" ? "state-warn" : "state-good"}` }, MARK_WORDS[markState(step)]) : null);
      for (const consumer of outsideOf(step).rows) {
        const id = text(consumer.entity.id) || text(consumer.entity.name);
        if (!outside.has(id)) outside.set(id, { entity: consumer.entity, node: null });
        pairs.push({ from: node, to: id, tone });
      }
      return node;
    });
    const OUTSIDE_SHOWN = 8;
    const reliers = [...outside.entries()].slice(0, OUTSIDE_SHOWN);
    // Two that read the same are told apart by where each sits.
    const apart = nameQualifiers(reliers.map(([, held]) => held.entity), api);
    for (const [, held] of reliers) {
      const entity = held.entity;
      const where = [kindWord(entity.kind), withinName(entity.within)].filter(Boolean);
      held.node = h("a", { class: "impact-node impact-node-out", href: UUID.test(text(entity.id)) ? `${base}${entityHref(entity.id)}` : undefined, title: text(entity.name) },
        h("span", { class: "impact-dot", "aria-hidden": "true" }),
        h("span", { class: "impact-text" }, h("span", { class: "impact-name" }, qualified(text(entity.name) || "Entity", apart.get(text(entity.id)))), h("span", { class: "impact-place" }, where.length > 1 ? `${where[0]} in ${where[1]}` : where[0] || "")));
    }
    const edges = pairs.map((pair) => ({ from: pair.from, to: outside.get(pair.to)?.node, tone: pair.tone })).filter((edge) => edge.to);
    const known = isCount(guide.summary?.recorded_consumers) ? guide.summary.recorded_consumers : outside.size;
    const moreOut = Math.max(0, known - reliers.length);
    const moreIn = steps.length - listed.length;
    const reach = known ? `${limitedGuide(guide) ? "at least " : ""}${plural(known, "entity that relies", "entities that rely")} on ${steps.length === 1 ? "it" : "them"} without having changed` : `nothing Kin found relies on ${steps.length === 1 ? "it" : "them"} from outside`;
    const host = h("div", { class: `impact${reliers.length ? "" : " impact-alone"}`, role: "group", "aria-label": `${plural(steps.length, "changed entity", "changed entities")}, and ${reach}` },
      h("div", { class: "impact-col" }, h("div", { class: "impact-col-head" }, "Changed", h("span", { class: "count" }, count(steps.length))), left,
        moreIn > 0 ? (more ? h("a", { class: "impact-more", href: `${base}${more}` }, `and ${count(moreIn)} more`) : h("span", { class: "impact-more" }, `and ${count(moreIn)} more`)) : null),
      reliers.length ? h("div", { class: "impact-gap", "aria-hidden": "true" }) : null,
      reliers.length ? h("div", { class: "impact-col" }, h("div", { class: "impact-col-head" }, steps.length === 1 ? "Rely on it, and did not change" : "Rely on them, and did not change", h("span", { class: "count" }, graphBadge(known, limitedGuide(guide)))), reliers.map(([, held]) => held.node),
        moreOut > 0 ? h("span", { class: "impact-more" }, `and ${count(moreOut)} more`) : null) : null);
    const join = graphApi()?.joinEdges;
    if (join && edges.length && onCleanup) {
      const mount = () => { if (host.isConnected) onCleanup(join(host, edges)); };
      if (globalThis.requestAnimationFrame) requestAnimationFrame(mount); else mount();
    }
    return host;
  }

  /** One highlighted line as elements: each token in its class, plain text as text. */
  const tokenSpans = (parts) => array(parts).map(([type, value]) => type ? h("span", { class: `t-${type}` }, value) : value);

  // ── How what relies on an entity uses it ──
  // The lines of each relier's own source that name the changed entity, read
  // from the graph's source for that relier. The ones that did not change with
  // it come first, since those are the ones to check.
  const callSiteReads = new Map();
  const SITES_SHOWN = 4;
  function siteReliers(step) {
    const rows = array(step.connections?.consumers).filter((consumer) => UUID.test(text(consumer?.entity?.id)));
    return [...rows.filter((consumer) => !consumer.changed_here), ...rows.filter((consumer) => consumer.changed_here)].slice(0, SITES_SHOWN);
  }
  /** The last part of an entity's name, which is how code that uses it names it. */
  const calledAs = (name) => text(name).split(/::|\.|#/).filter(Boolean).pop() || "";
  function loadCallSites(step) {
    const wanted = siteReliers(step).filter((consumer) => !callSiteReads.has(consumer.entity.id));
    if (!wanted.length) return;
    const epoch = state.readEpoch;
    for (const consumer of wanted) {
      const id = consumer.entity.id;
      callSiteReads.set(id, { busy: true });
      fetchJSON(`/api/workspace/entity?id=${encodeURIComponent(id)}&sections=source`)
        .then((data) => { callSiteReads.set(id, data?.source?.ok && typeof data.source.text === "string" ? { text: data.source.text, start: data.source.start_line, language: data.source.language } : { missing: true }); })
        .catch(() => { callSiteReads.set(id, { missing: true }); })
        .finally(() => { if (epoch === state.readEpoch && state.route === "review") paint("review-sites"); });
    }
  }
  function callSites(step) {
    const reliers = siteReliers(step);
    const word = calledAs(step.name);
    if (!reliers.length || !word || step.change === "added") return null;
    Promise.resolve().then(() => loadCallSites(step));
    const naming = new RegExp(`(^|[^\\w$])${word.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}([^\\w$]|$)`);
    const apart = nameQualifiers(reliers.map((consumer) => consumer.entity));
    const blocks = reliers.map((consumer) => {
      const held = callSiteReads.get(consumer.entity.id);
      if (!held || held.busy || held.missing) return null;
      const lines = held.text.split("\n");
      const hits = lines.map((line, at) => ({ line, at })).filter(({ line }) => naming.test(line)).slice(0, 3);
      if (!hits.length) return null;
      let tokens = null;
      try { tokens = globalThis.kinReview?.highlightLines?.(hits.map((hit) => hit.line.trim()).join("\n"), held.language) || null; } catch (_) { tokens = null; }
      return h("div", { class: "site" },
        h("div", { class: "site-head" }, ent(consumer.entity), apart.get(text(consumer.entity.id)) ? h("span", { class: "name-qualifier" }, `in ${apart.get(text(consumer.entity.id))}`) : null,
          h("span", { class: "note" }, consumer.changed_here ? "changed here too" : "did not change")),
        h("div", { class: "codeview site-code" }, h("div", { class: "code-lines" }, hits.flatMap((hit, index) => [
          h("span", { class: "code-no" }, Number.isSafeInteger(held.start) ? String(held.start + hit.at) : ""),
          h("span", { class: "code-text" }, tokens?.[index] ? tokenSpans(tokens[index]) : hit.line.trim())]))));
    }).filter(Boolean);
    if (!blocks.length) return reliers.some((consumer) => callSiteReads.get(consumer.entity.id)?.busy !== false && !callSiteReads.get(consumer.entity.id)?.text && !callSiteReads.get(consumer.entity.id)?.missing)
      ? h("p", { class: "note aside-reading sites-loading" }, "Reading how they use it") : null;
    return h("div", { class: "sites" }, h("h3", { class: "sites-title" }, "How they use it"), blocks);
  }

  /** One changed entity joined to each thing that relies on it, in the colour of where that thing stands. */
  function stepPicture(step, onCleanup) {
    const connections = step.connections || {};
    const consumers = array(connections.consumers).filter((consumer) => consumer?.entity);
    if (!consumers.length) return null;
    const total = isCount(connections.consumers_total) ? connections.consumers_total : consumers.length;
    const verdict = text(step.consumer_verdict?.verdict);
    const toneOf = (consumer) => consumer.changed_here ? "good" : verdict === "needs_no_update" ? "quiet" : verdict === "not_updated" ? "bad" : "warn";
    const WORD = { good: "Changed here too", quiet: "Needs no update", bad: "Not updated", warn: "May need review" };
    const SHOWN = 8;
    const apart = nameQualifiers(consumers.slice(0, SHOWN).map((consumer) => consumer.entity));
    const focus = h("div", { class: "impact-node impact-focus" }, h("span", { class: "impact-text" }, h("span", { class: "impact-name" }, step.name), h("span", { class: "impact-place" }, step.change === "added" ? "New here" : step.change === "removed" ? "Removed here" : "Changed here")));
    const rows = consumers.slice(0, SHOWN).map((consumer) => {
      const tone = toneOf(consumer);
      const node = h("a", { class: "impact-node", "data-tone": tone, href: UUID.test(text(consumer.entity.id)) ? entityHref(consumer.entity.id) : undefined, title: text(consumer.entity.name) },
        h("span", { class: "impact-dot", "aria-hidden": "true" }),
        h("span", { class: "impact-text" }, h("span", { class: "impact-name" }, qualified(text(consumer.entity.name) || "Entity", apart.get(text(consumer.entity.id)))), h("span", { class: "impact-place" }, relationWord(consumer.relation, consumer.entity, step))),
        h("span", { class: `state impact-mark${tone === "good" ? " state-good" : tone === "bad" ? " state-bad" : tone === "warn" ? " state-warn" : ""}` }, WORD[tone]));
      return { node, tone };
    });
    const more = total - rows.length;
    const host = h("div", { class: "impact impact-step", role: "group", "aria-label": `${step.name} and ${plural(total, "entity that relies", "entities that rely")} on it` },
      h("div", { class: "impact-col impact-col-focus" }, focus),
      h("div", { class: "impact-gap", "aria-hidden": "true" }),
      h("div", { class: "impact-col" }, rows.map((row) => row.node), more > 0 ? h("span", { class: "impact-more" }, `and ${count(more)} more that Kin counted and did not list here`) : null));
    const join = graphApi()?.joinEdges;
    if (join && onCleanup) {
      const mount = () => { if (host.isConnected) onCleanup(join(host, rows.map((row) => ({ from: focus, to: row.node, tone: row.tone })))); };
      if (globalThis.requestAnimationFrame) requestAnimationFrame(mount); else mount();
    }
    return host;
  }

  /** One card for each repository with work waiting on you. */
  function homeAttention(onCleanup) {
    if (!read("home").data) return null;
    const needs = homeSnapshot().needs;
    if (!needs.length) return null;
    const cleanups = [];
    onCleanup(() => cleanups.splice(0).forEach((stop) => stop()));
    return h("section", { class: "home-section", "aria-label": "Needs you" }, needs.map((view) => {
      const { row, work } = view;
      const unreviewed = view.awaiting;
      const known = view.unreachable ? h("p", { class: "note focus-known" }, unreachableWords(view)) : null;
      if (unreviewed.length && work.state !== "work" && !row.merge_in_progress) {
        const [first] = unreviewed;
        const who = [...new Set(unreviewed.map((change) => text(change.agent?.name) || "An agent"))];
        const sentence = unreviewed.length === 1
          ? `${who[0]} committed ${text(first.title) ? `“${first.title}”` : "a change"} ${ago(first.time || first.timestamp)}, and nobody has reviewed it yet.`
          : `${who.length === 1 ? who[0] : "Agents"} committed ${spoken(unreviewed.length).toLowerCase()} changes nobody has reviewed yet.`;
        return h("article", { class: "focus-card" },
          h("div", { class: "focus-head" },
            h("a", { class: "focus-repo", href: repoHref(row) }, h("span", { class: "focus-repo-name" }, repositoryTitle(row)), text(row.branch) ? h("span", { class: "focus-repo-branch" }, row.branch) : null,
              h("span", { class: "focus-kind" }, unreviewed.length === 1 ? "Agent commit" : "Agent commits")),
            unreviewed.length === 1 ? h("div", { class: "page-actions" }, link("Review", repoHref(row, reviewHref(first.id)), true)) : null),
          h("p", { class: "focus-sentence" }, sentence),
          known,
          // The newest one drawn as what it changed and what relies on that, as the review will walk it.
          unreviewed.length > 1 ? h("h3", { class: "sites-title focus-caption" }, text(first.title) ? `The newest, “${first.title}”` : "The newest") : null,
          agentChangePicture(row, first, (stop) => cleanups.push(stop)),
          unreviewed.length > 1 ? agentReviewList(row, unreviewed) : null);
      }
      const detail = homeDetail.get(row.key);
      const guide = work.guide;
      const steps = guide ? reviewSteps(guide) : [];
      const pending = guide?.summary?.completeness?.pending_entities;
      const sentence = guide ? workSentence(guide) || "Your code differs from your last commit, but no entity changed." : row.merge_in_progress && work.state !== "work" ? "A merge is waiting for you to settle its conflicts." : "Uncommitted work is waiting.";
      return h("article", { class: "focus-card" },
        h("div", { class: "focus-head" },
          h("a", { class: "focus-repo", href: repoHref(row) }, h("span", { class: "focus-repo-name" }, repositoryTitle(row)), text(row.branch) ? h("span", { class: "focus-repo-branch" }, row.branch) : null,
            h("span", { class: "focus-kind" }, work.state === "work" ? "Uncommitted" : "Merge")),
          h("div", { class: "page-actions" },
            link("Open", repoHref(row, work.state === "work" ? UNCOMMITTED : "#/branches"), false, { class: "button button-quiet" }),
            steps.length ? link(`Review ${count(steps.length)}`, repoHref(row, "#/review/w/1"), true) : row.merge_in_progress ? link("Settle the merge", repoHref(row, "#/branches"), true) : null)),
        h("p", { class: "focus-sentence" }, sentence),
        known,
        guide && unsettled(guide) ? h("p", { class: "note" }, isCount(pending) && pending > 0 ? `Kin is still mapping ${plural(pending, "entity", "entities")}, so these counts may grow.` : "Kin is still mapping this code, so these counts may grow.") : null,
        guide ? impactPicture(guide, (stop) => cleanups.push(stop), { base: repoHref(row), api: `/repositories/${row.key}`, more: UNCOMMITTED })
          : detail?.failed.working ? h("p", { class: "note" }, "Kin couldn't read what changed here yet. Open the repository to see it.")
          : work.state === "work" ? h("div", { class: "focus-reading", "aria-busy": "true" }, skeletonLines(3)) : null,
        // What agents committed here and nobody has reviewed waits beside the uncommitted work, not behind it.
        unreviewed.length ? agentReviewList(row, unreviewed) : null,
        work.stale ? h("p", { class: "note focus-stale" }, "Reading it again") : null);
    }));
  }

  /**
   * Read one committed change for Home: how many entities it changed, from
   * its own record, which answers quickly, and its guided review, which draws
   * the picture. Neither waits on the other or on the repository's other
   * reads, so a slow guide holds back nothing else on Home.
   */
  function askChange(row, id) {
    const detail = detailFor(row.key);
    const changes = detail.changes || (detail.changes = new Map());
    const held = changes.get(id) || { guide: null, counted: undefined, busy: false, failed: null, at: 0 };
    changes.set(id, held);
    if (held.busy || held.guide || (held.failed && Date.now() - held.at < 30_000)) return;
    // A change a review in this tab already read is drawn from that reading.
    const kept = heldChangeGuide(`/repositories/${row.key}`, id);
    if (kept) { held.guide = kept; repaintHome(); return; }
    held.busy = true; held.failed = null; held.at = Date.now();
    const epoch = state.readEpoch;
    const base = `root:/repositories/${row.key}`;
    if (held.counted === undefined) {
      held.counted = null;
      fetchJSON(`${base}/api/counts?changes=${encodeURIComponent(id)}`)
        .then((data) => { if (epoch === state.readEpoch && !held.guide) { held.counted = data?.counts?.[id] || null; repaintHome(); } })
        .catch(() => { /* The count was only a first word; the guide still answers. */ });
    }
    // Nothing on Home waits on the picture, so a guide that takes a while to read is given the time.
    fetchJSON(`${base}/api/guide?change=${encodeURIComponent(id)}`, { patience: CHANGE_PATIENCE })
      // The review this card opens is handed the reading, so the change is not read twice.
      .then((guide) => { keepChangeGuide(`/repositories/${row.key}`, id, guide); if (epoch === state.readEpoch) held.guide = guide; })
      .catch((error) => { if (epoch === state.readEpoch) held.failed = error.message || "This change could not be read."; })
      .finally(() => { held.busy = false; held.at = Date.now(); if (epoch === state.readEpoch) repaintHome(); });
  }
  const CHANGE_PATIENCE = 180_000;
  const changeHeld = (row, id) => homeDetail.get(row.key)?.changes?.get(id) || null;
  /** What Home has read of a change so far, in a few words for its facts. */
  const changeFacts = (row, id) => { const held = changeHeld(row, id); return held ? [Boolean(held.guide), held.busy, held.failed, held.counted?.entities ?? null] : null; };

  /**
   * An agent's committed change on its Home card: the entities it changed
   * joined to what relies on them and did not change, as its review walks
   * them. While the guide is read, the count from the change's own record
   * says how much it holds.
   */
  function agentChangePicture(row, change, onCleanup) {
    Promise.resolve().then(() => askChange(row, change.id));
    const held = changeHeld(row, change.id);
    const guide = held?.guide;
    const review = reviewHref(change.id);
    if (guide) {
      const drawn = impactPicture(guide, onCleanup, { base: repoHref(row), api: `/repositories/${row.key}`, walk: review, more: review });
      return drawn || h("p", { class: "note" }, workChanged(guide) ? `${plural(workChanged(guide), "entity", "entities")} changed. Its review shows them.` : "No entity changed in it.");
    }
    const n = held?.counted?.entities;
    const counted = isCount(n) ? (n ? `${spoken(n)} ${n === 1 ? "entity" : "entities"} changed.` : "No entity changed in it.") : "";
    if (held?.failed) return h("p", { class: "note" }, [counted, "Kin couldn't read what relies on it yet. Its review has the rest."].filter(Boolean).join(" "));
    return h("div", { class: "focus-reading", "aria-busy": "true" },
      h("p", { class: "note aside-reading", role: "status" }, counted ? `${counted} Reading what relies on ${n === 1 ? "it" : "them"}` : "Reading what it changed"),
      skeletonLines(3));
  }

  /** Agent changes nobody has reviewed, the newest few, each with its own way into its review. */
  function agentReviewList(row, changes) {
    const SHOWN = 3;
    return h("div", { class: "focus-agents" },
      h("h3", { class: "sites-title" }, changes.length === 1 ? "Committed by an agent, not reviewed yet" : `Committed by agents, not reviewed yet: ${count(changes.length)}`),
      h("ul", { class: "list list-card" }, changes.slice(0, SHOWN).map((change) => h("li", {}, h("div", { class: "change-row" },
        h("span", {}, h("span", { class: "change-title" }, text(change.title) || (change.listed ? "A change it committed" : "Untitled change")),
          h("span", { class: "change-meta" }, h("span", {}, text(change.agent?.name) || "An agent"), h("span", {}, ago(change.time || change.timestamp)))),
        link("Review", repoHref(row, reviewHref(change.id)), false, { class: "button button-small" }))))),
      changes.length > SHOWN ? h("p", { class: "note quiet-line" }, h("a", { class: "link", href: repoHref(row, "#/history") }, `${count(changes.length - SHOWN)} more in History`)) : null);
  }

  /** Who is working through Kin right now, across every repository Home has open. */
  function homeAgents() {
    if (!read("home").data || !homeListed().length) return null;
    const { live, earlier } = homeSnapshot();
    const api = agentsApi();
    const linked = (held, parts) => parts.map((part) => typeof part === "string" ? part : h("a", { class: "agent-entity", href: repoHref(held.row, entityHref(part.id)), title: part.label }, api.shortLabel(part.label)));
    const action = (held) => {
      const call = array(held.agent.recent_calls)[0];
      if (!call || !api) return null;
      return linked(held, api.describe(call, held.names));
    };
    // What a session came to: the change it committed and whether it waits for review, before the last call it made.
    const outcome = (held) => {
      const parts = api?.outcome?.(held.agent, held.names, (id) => reviewStateIn(held.row, id));
      return Array.isArray(parts) && parts.length ? linked(held, parts) : null;
    };
    const plainOf = (parts) => array(parts).map((part) => typeof part === "string" ? part : part.textContent).join("");
    const earlierActions = (held) => {
      const calls = array(held.agent.recent_calls).slice(1, 3);
      return calls.length && api ? h("ul", { class: "agent-card-trail" }, calls.map((call) => h("li", {}, api.plainText(api.describe(call, held.names))))) : null;
    };
    const head = h("div", { class: "section-head" }, h("h2", {}, "Agents", live.length ? h("span", { class: "count" }, count(live.length)) : null),
      read("clients").missing ? null : h("a", { class: "more-link", href: "#/agents" }, "Connect an agent", icon("arrow")));
    if (live.length) {
      return h("section", { class: "home-section" }, head, h("div", { class: "agent-cards" }, live.slice(0, 6).map((held) => {
        const working = held.agent.state === "active";
        const did = action(held);
        const came = held.agent.counted ? null : outcome(held);
        // An agent the machine's list counts before its repository has named what it did.
        const counted = held.agent.counted ? (working ? "Working through Kin now." : "Connected through Kin, and idle.") : null;
        return h("a", { class: "agent-card", href: repoHref(held.row, "#/agents"), "data-state": working ? "working" : "idle" },
          h("div", { class: "agent-card-head" }, h("span", { class: "pulse", "aria-hidden": "true" }), h("span", { class: "agent-card-name" }, held.agent.name), h("span", { class: "agent-card-repo" }, `in ${repositoryLabel(held.row, " / ")}`),
            h("span", { class: `state${working ? " state-good" : ""}` }, working ? "Working" : "Idle")),
          h("p", { class: "agent-card-action" }, counted || did || "Connected. It has not asked Kin for anything yet."),
          came && plainOf(came) !== plainOf(did) ? h("p", { class: "agent-card-outcome" }, came) : null,
          earlierActions(held),
          h("p", { class: "note" }, [isCount(held.agent.calls_total) ? plural(held.agent.calls_total, "call") : "", api?.presence?.(held.agent) || ""].filter(Boolean).join(" · ")));
      })));
    }
    const last = earlier[0];
    return h("section", { class: "home-section" }, head, h("div", { class: "quiet-card" },
      h("p", { class: "quiet-card-title" }, "No agent is connected."),
      // A session is said by what it came to, never by its last call alone when that call is not what it did.
      last ? h("p", { class: "quiet-card-text" }, `${last.agent.name} last worked in `, h("a", { class: "link", href: repoHref(last.row, "#/agents") }, repositoryLabel(last.row, " / ")), ` ${ago(last.agent.last_activity_at)}. `,
        (outcome(last) || action(last)) ? [outcome(last) || action(last), "."] : null)
        : h("p", { class: "quiet-card-text" }, "A coding agent works in your repositories through Kin once it is connected.")));
  }

  /** Every repository as what it holds: how large it is, what it is written in, and what is in motion in it. */
  function homeRepos() {
    const resource = read("home");
    if (!resource.data) return resource.error ? null : h("section", { class: "home-section" }, skeletonLines(4));
    const views = homeSnapshot().views;
    const rows = views.map((view) => view.row);
    if (!rows.length) return null;
    const ordered = [...views].sort((a, b) => TONE_RANK[a.tone] - TONE_RANK[b.tone] || (Date.parse(b.row.last_change_at || "") || 0) - (Date.parse(a.row.last_change_at || "") || 0) || repositoryLabel(a.row).localeCompare(repositoryLabel(b.row)));
    const stat = (value, label, atLeast = false) => isCount(value) ? h("div", { class: "stat", title: atLeast ? `At least ${count(value)} ${label}: some call sites are not proven yet` : undefined }, h("span", { class: "stat-value" }, `${atLeast && value > 0 ? "≥" : ""}${count(value)}`), h("span", { class: "stat-label" }, label)) : null;
    // A share of the whole that never rounds a part to nothing or the rest to everything.
    const share = (part, whole) => { const percent = part / whole * 100; return percent < 1 ? "under 1%" : `${Math.min(99, Math.round(percent)) === 99 && part < whole ? 99 : Math.round(percent)}%`; };
    const unavailable = resource.data.unavailable;
    return h("section", { class: "home-section" },
      h("div", { class: "section-head" }, h("h2", {}, "Repositories", h("span", { class: "count" }, count(rows.length))),
        section("hub-add-toggle", [], () => state.add.open ? null : h("a", { class: "more-link", href: "#/add" }, "Add repository", icon("arrow")))),
      h("div", { class: "repo-cards" }, ordered.map((view) => {
        const { row, work } = view;
        const detail = homeDetail.get(row.key);
        const overview = detail?.overview;
        const languages = section_ok(overview?.languages) ? array(overview.languages.items).filter((item) => item && typeof item.language === "string" && item.entities > 0) : [];
        const total = languages.reduce((sum, item) => sum + item.entities, 0);
        const entities = section_ok(overview?.counts) && isCount(overview.counts.entities) ? overview.counts.entities : row.entities;
        const connections = section_ok(overview?.counts) ? overview.counts.relationships : null;
        const live = view.agents.active;
        const waiting = view.awaiting.length;
        const stateWords = work.state === "work" ? (isCount(work.changed) && work.changed ? `${count(work.changed)} uncommitted` : "Uncommitted work")
          : row.merge_in_progress ? "A merge to settle" : waiting ? `${count(waiting)} to review` : repoActivity(row) || (work.state === "clean" ? "All committed" : work.state === "opening" ? "Opening" : work.state === "reading" ? "Looking for changes" : "");
        return h("a", { class: "repo-card", id: `repo-card-${row.key.slice(0, 16)}`, href: repoHref(row), "data-state": work.state },
          h("div", { class: "repo-card-head" }, h("span", { class: "repo-card-name" }, repositoryTitle(row)), text(row.branch) ? h("span", { class: "repo-card-branch" }, row.branch) : null),
          live ? h("div", { class: "repo-card-live" }, h("span", { class: "pulse", "aria-hidden": "true" }),
            (view.agents.listed ? [...new Set(view.agents.live.filter((agent) => agent.state === "active").map((agent) => agent.name))] : view.agents.named).slice(0, 2).join(" and ") || plural(live, "agent"), live === 1 ? " is working" : " are working") : null,
          h("div", { class: "stats" }, stat(entities, "entities"), stat(connections, "connections", connectionsAtLeast(overview))),
          languages.length ? h("div", { class: "lang" },
            h("div", { class: "lang-bar", "aria-hidden": "true" }, languages.slice(0, 4).map((item, at) => h("span", { class: "lang-seg", "data-at": String(at), css: { "flex-grow": String(item.entities) } }))),
            h("div", { class: "lang-legend" }, languages.slice(0, 2).filter((item) => item.entities / total >= 0.01).map((item, at) => h("span", { class: "lang-key", "data-at": String(at) }, `${languageWord(item.language)} ${share(item.entities, total)}`))))
            : h("div", { class: "lang lang-pending", "aria-hidden": "true" }),
          h("div", { class: "repo-card-foot" },
            stateWords ? h("span", { class: `state${work.state === "work" || row.merge_in_progress || waiting ? " state-warn" : ""}` }, stateWords) : null,
            // A repository Kin cannot reach says when what it shows was known, in place of when it last changed.
            view.unreachable ? h("span", { class: "repo-card-time repo-card-known", title: unreachableWords(view) }, `As of ${knownWhen(view.knownAt)}`)
              : row.last_change_at ? h("span", { class: "repo-card-time", title: "Last change" }, ago(row.last_change_at)) : null));
      })),
      isCount(unavailable) && unavailable > 0 ? h("p", { class: "note quiet-line" }, `${plural(unavailable, "repository", "repositories")} Kin knew about ${unavailable === 1 ? "is" : "are"} no longer on this machine.`) : null,
      text(resource.data.warning) ? h("p", { class: "note quiet-line" }, resource.data.warning) : null);
  }

  /** What was committed lately, across the repositories Home has open. */
  function homeRecent() {
    if (!read("home").data) return null;
    const changes = [];
    for (const row of homeListed()) {
      const recent = homeDetail.get(row.key)?.overview?.recent_changes;
      if (!section_ok(recent)) continue;
      for (const change of array(recent.items)) if (change && !isKinUpdate(change) && typeof change.id === "string") changes.push({ change, row });
    }
    const when = (held) => Date.parse(held.change.time || held.change.timestamp || "") || 0;
    const recent = changes.filter((held) => Date.now() - when(held) < 14 * 86_400_000);
    if (!recent.length) return null;
    changes.splice(0, changes.length, ...recent);
    changes.sort((a, b) => when(b) - when(a));
    return h("section", { class: "home-section" },
      h("div", { class: "section-head" }, h("h2", {}, "Committed in the last two weeks")),
      h("ul", { class: "list list-card" }, changes.slice(0, 6).map(({ change, row }) => h("li", {}, h("a", { class: "change-row", href: repoHref(row, reviewHref(change.id)) },
        h("span", {}, h("span", { class: "change-title" }, text(change.title) || "Untitled change"),
          h("span", { class: "change-meta" }, h("span", {}, repositoryLabel(row, " / ")),
            agentsApi()?.author?.(agentsCtx, change, authorOf(change)) || (authorOf(change) ? h("span", {}, authorOf(change)) : null),
            h("span", {}, ago(change.time || change.timestamp)))))))));
  }

  function hub() {
    // Each part is drawn here as it stands, so it is drawn again only when its facts move on.
    Object.assign(homeSigs, homeFacts());
    if (read("home").data) Promise.resolve().then(loadHomeDetails);
    return [
      h("header", { class: "home-head" }, section("home-headline", [], homeHeadline),
        // The question waits until there is a repository to put it to.
        section("home-ask-box", [], () => homeListed().length ? homeAskBox() : null)),
      section("home-ask", [], homeAskResults),
      h("div", { id: "home-body", hidden: Boolean(homeAsk.query.trim()) },
        section("hub-add", [], addPanel),
        section("home-attention", [], homeAttention),
        section("home-agents", [], homeAgents),
        section("home-repos", [], homeRepos),
        section("home-recent", [], homeRecent))];
  }

  /** The coding agents on this machine and whether each is connected to Kin. */
  function clients() {
    return [
      h("header", { class: "page-head" },
        h("div", { class: "page-title-row" }, h("h1", {}, "Coding agents")),
      h("p", { class: "lede" }, "Configured agents are ready to use Kin. Start work in a repository and ask the agent to use Kin to read or change its code. Its session appears in Agents when it uses Kin.")),
      section("hub-clients", ["clients"], hubClients)];
  }

  // ── Uncommitted ──────────────────────────────────────────────────────────
  // What is uncommitted in this repository, as the list a review walks: every
  // changed entity, the ones something else relies on first. Reading one
  // through is the review's job; this page says what there is to read, where
  // an agent's activity meets it, and commits it.

  /** "21 rely on it": the one way every count of what relies on an entity is said. A lower bound says so. */
  function relyWords(n, atLeast = false) { return isCount(n) ? `${atLeast && n > 0 ? "at least " : ""}${count(n)} ${n === 1 && !atLeast ? "relies on it" : "rely on it"}` : ""; }
  /** Whether the repository's ranking of what relies on what is a lower bound, as the summary says; unsaid is not a guess. */
  const rankingAtLeast = (overview) => text(overview?.most_relied_on?.bound) === "at_least";
  const connectionsAtLeast = (overview) => text(overview?.counts?.relationships_bound) === "at_least";
  /** Whether Kin says its reading of this work is not finished, so every count is a lower bound. */
  const unsettled = (guide) => { const state = text(guide?.summary?.completeness?.state); return Boolean(state) && state !== "settled"; };
  const workSteps = (guide) => array(guide?.chapters).flatMap((chapter) => array(chapter.steps)).filter((step) => step && typeof step.name === "string");
  const hasWork = (guide) => Boolean(guide && (workChanged(guide) > 0 || guide.working_tree?.differs));
  const canCommit = () => session()?.capabilities?.commit === true && Boolean(session()?.token);
  const can = (name) => session()?.capabilities?.[name] === true && Boolean(session()?.token);

  // ── Who changed an entity ──
  // Attribution comes from a confirmed Kin write or a reported client edit.
  const authorsOf = (step) => array(step?.authored_by).filter((who) => typeof who?.name === "string");
  const knowsAuthors = (guide) => Array.isArray(guide?.summary?.authors);
  /**
   * Who changed an uncommitted entity. Kin names an agent when its write or
   * its client's report says so. With no such word the edit is a person's or
   * a tool's that told Kin nothing, and nothing the page reads tells those
   * apart (the sessions it lists are only the ones seen lately), so the gap
   * is said as a gap rather than guessed.
   */
  function authorWords(step, guide) {
    const who = authorsOf(step);
    if (who.length) return `changed by ${[...new Set(who.map((held) => held.name))].slice(0, 2).join(" and ")}`;
    if (!guide?.working_tree) return knowsAuthors(guide) ? "No agent reported this edit" : "";
    return "No agent reported this edit";
  }
  /** The uncommitted work's authorship in one sentence, when no step names an agent or some steps name none. */
  function authorGapWords(guide) {
    const steps = workSteps(guide);
    if (!guide?.working_tree || !steps.length) return "";
    const unnamed = steps.filter((step) => !authorsOf(step).length).length;
    if (!unnamed) return "";
    if (unnamed < steps.length) return `No agent reported the other ${unnamed === 1 ? "one" : spoken(unnamed).toLowerCase()}.`;
    return `No agent reported writing ${steps.length === 1 ? "it" : "any of them"}, so Kin cannot say whether ${steps.length === 1 ? "it is" : "they are"} yours or an agent's.`;
  }

  // ── Test results ──
  const RESULT_WORDS = { passed: ["Run passed", "good"], failed: ["Run failed", "bad"], not_run: ["Not run", ""], stale: ["Out of date", "warn"] };
  function resultWord(result) {
    const state = text(result?.state);
    const [words, tone] = RESULT_WORDS[state] || [];
    // A test that has not run but has a time was tried by a command that could not run.
    return words ? h("span", { class: `state${tone ? ` state-${tone}` : ""}`, title: [text(result.summary), result.at ? `${state === "not_run" ? "Tried" : "Run"} ${ago(result.at)}` : ""].filter(Boolean).join(" ") || undefined }, words) : null;
  }
  /** The tests that reach a piece of work, by their latest result, as one short sentence. */
  function testsSentence(guide) {
    const tally = { passed: 0, failed: 0, not_run: 0, stale: 0 };
    let any = false;
    for (const step of workSteps(guide)) {
      const held = step.tests_result;
      if (!held) continue;
      any = true;
      for (const key of Object.keys(tally)) tally[key] += isCount(held[key]) ? held[key] : 0;
    }
    if (!any) return "";
    const parts = [];
    if (tally.failed) parts.push("A test command failed");
    if (tally.passed) parts.push("A test command passed");
    if (tally.stale) parts.push("Some test runs predate the latest edits");
    if (tally.not_run) parts.push("Some tests have not run yet");
    return parts.length ? `${parts.join(". ")}.` : "";
  }
  /** Run the tests that reach the uncommitted work, then read the work again. */
  async function runTests() {
    const verify = state.verify;
    if (verify.busy || verify.unknown || !globalThis.crypto?.randomUUID) return;
    verify.busy = true; verify.error = null; verify.operation = null;
    verify.operationId = crypto.randomUUID();
    paint("work-verify"); paint("work-actions");
    try {
      const started = await send("/api/workspace/verify", { operation_id: verify.operationId });
      verify.operation = started.state ? started : { ...started, state: "running" };
      followVerify(verify.operation.operation_id);
    } catch (error) {
      verify.busy = false;
      if (error.status === 409 && error.operation?.operation_id) {
        verify.operation = error.operation;
        verify.busy = verify.operation.state === "running";
        followVerify(verify.operation.operation_id);
      } else {
        verify.error = error.remedy || error.message || "Kin could not confirm whether the tests started.";
        verify.unknown = ![400, 403, 404, 405, 409, 422, 501].includes(error.status);
      }
    }
    paint("work-verify"); paint("work-actions");
  }
  function followVerify(id, misses = 0) {
    const verify = state.verify;
    clearTimeout(verify.timer);
    if (!id || verify.operation?.state !== "running") { verify.busy = false; return; }
    verify.timer = setTimeout(async () => {
      try {
        verify.operation = await fetchJSON(`/api/workspace/operations/${encodeURIComponent(id)}`);
        if (verify.operation.state === "running") { paint("work-verify"); return followVerify(id); }
        verify.busy = false;
        announce(verify.operation.state === "done" ? VERIFY_ANNOUNCED[verify.operation.outcome] || "No tests ran." : "The test run did not finish.");
        refresh(["working"]);
      } catch (error) {
        if (misses >= 5) { verify.busy = false; verify.unknown = true; verify.error = "Kin stopped answering about the test run."; }
        else return followVerify(id, misses + 1);
      }
      paint("work-verify"); paint("work-actions");
    }, 1000);
  }
  const VERIFY_ANNOUNCED = { passed: "The test command passed.", failed: "The test command failed.", could_not_run: "The test command could not run." };
  /** What Kin kept of a finished test command: its exit code, the end of what it printed, and the operation that ran it. */
  function runDetails(operation) {
    const run = operation.result?.run;
    const tail = text(run?.output_tail);
    const exit = Number.isInteger(run?.exit_code) ? `Exit code ${run.exit_code}` : run ? run.outcome === "could_not_run" ? "It never started, so it has no exit code." : "It ended without an exit code." : "";
    if (!exit && !tail && !operation.operation_id) return null;
    return h("details", { class: "more" }, h("summary", {}, "Show details"),
      exit ? h("p", {}, exit) : null,
      tail ? [h("p", {}, "The end of its output"), h("pre", { class: "op-lines", tabindex: "0", "aria-label": "The end of the test command's output" }, tail)] : null,
      operation.operation_id ? h("p", {}, "Operation ", h("code", {}, operation.operation_id)) : null);
  }
  function verifyNote() {
    const verify = state.verify;
    const guide = read("working").data;
    if (verify.unknown) return h("div", { class: "message message-warn section-tight", role: "alert" },
      h("div", {}, h("strong", {}, "The test run's outcome is unknown"), h("p", {}, verify.error), operationDetails(verify.operation?.operation_id || verify.operationId)),
      button("Check the run", () => { const id = verify.operation?.operation_id || verify.operationId; verify.unknown = false; verify.error = null; verify.busy = true; verify.operation = { ...verify.operation, operation_id: id, state: "running" }; followVerify(id); paint("work-verify"); paint("work-actions"); }, { class: "button button-small" }));
    if (verify.error) return h("div", { class: "message message-bad section-tight", role: "alert" }, h("div", {}, h("strong", {}, "The test run could not start"), h("p", {}, verify.error)));
    if (verify.busy) {
      const progress = verify.operation?.progress;
      const words = text(verify.operation?.phase) || (isCount(progress?.done) && isCount(progress?.total) && progress.total ? `Running the tests that reach this work, ${count(progress.done)} of ${count(progress.total)}` : "Running the tests that reach this work");
      return h("p", { class: "note aside-reading", role: "status" }, words);
    }
    const operation = verify.operation;
    if (operation?.state === "unavailable" || operation?.state === "failed") return h("div", { class: "message message-warn section-tight", role: "alert" }, h("div", {},
      h("strong", {}, operation.state === "unavailable" ? "Tests are unavailable" : "The test run did not finish"), h("p", {}, text(operation.reason) || text(operation.error) || "Kin could not complete the test run.")));
    if (operation?.state === "done") {
      const outcome = operation.outcome;
      const title = outcome === "passed" ? "Test command passed" : outcome === "failed" ? "Test command failed" : outcome === "could_not_run" ? "Test command could not run" : "No tests ran";
      const result = operation.result;
      const run = result?.run;
      const exited = Number.isInteger(run?.exit_code) ? ` It exited ${run.exit_code}.` : "";
      const said = outcome === "no_tests" ? "Kin found no tests it could run for this work. This work has not passed a test run."
        // It never reached a test, so it is said apart from a run that failed, and no test is said to have failed.
        : outcome === "could_not_run" ? `${text(run?.reason) || "Kin could not run the test command."}${exited} No test passed or failed, so the tests that reach this work have not run.`
        : `${result?.reach === "whole" ? "Kin ran the repository's full test suite." : "Kin ran the test command selected for this work."}${outcome === "failed" ? exited : ""} It did not report individual test outcomes.`;
      return h("div", { class: `message message-${outcome === "passed" ? "good" : outcome === "failed" ? "bad" : "warn"} section-tight`, role: "status" }, h("div", {},
        h("strong", {}, title), h("p", {}, said), runDetails(operation)), button("Dismiss", () => { verify.operation = null; paint("work-verify"); }, { class: "button button-small button-quiet" }));
    }
    const sentence = guide ? testsSentence(guide) : "";
    return sentence ? h("p", { class: "tests-line" }, sentence) : null;
  }

  // ── Undoing one entity's change ──
  const restoreGuard = (step) => step.change === "removed" ? "absent" : /^[0-9a-f]{64}$/i.test(text(step.after?.body_hash)) ? `source:${step.after.body_hash}` : null;
  /** Put one entity back as it was at the last commit, then read the work again. */
  async function restoreEntity(step) {
    const restore = state.restore;
    const expected = restoreGuard(step);
    if (restore.busy || restore.unknown || state.commit.busy || workspaceWriteBusy() || read("working").stale || !expected || !globalThis.crypto?.randomUUID) return;
    restore.busy = true; restore.error = null; restore.refused = false; restore.done = null;
    restore.operationId = crypto.randomUUID();
    rememberWorkReceipt("restore", restore.operationId);
    paint("review-body");
    try {
      const result = await send("/api/workspace/entity/restore", { entity: step.entity_id, operation_id: restore.operationId, expected });
      restore.asking = null;
      forgetWorkReceipt("restore");
      // The receipt stays where the undo was taken, and on Uncommitted, until it is dismissed.
      restore.done = { entity: step.entity_id, name: text(result?.name) || step.name, change: step.change, to: text(result?.to), base: text(read("working").data?.change?.base) };
      state.commit.result = null;
      announce(restoreReceiptText(restore.done));
      await refresh(["working"]);
      // The undone entity leaves the walk, so the page it stood on now holds the next one, or the walk is over.
      if (state.route === "review" && isWorkingReview() && state.params.step) {
        const left = reviewSteps(reviewGuide()).length;
        if (state.params.step > left) location.hash = left ? "#/review/w/done" : UNCOMMITTED;
      }
    } catch (error) {
      // A refusal is Kin's own sentence about this entity, such as its source
      // changing after this reading, so it is said as Kin wrote it.
      restore.refused = error.status === 409 && error.outcome === "refused";
      restore.error = (restore.refused ? error.message || error.remedy : error.remedy || error.message) || "It was not undone.";
      restore.unknown = !restore.refused && ![400, 403, 404, 405, 422, 501].includes(error.status);
      if (restore.unknown) read("working").stale = true;
      else forgetWorkReceipt("restore");
    } finally {
      restore.busy = false;
      paint("review-body");
    }
  }
  /** What an undo did, in one sentence. */
  function restoreReceiptText(done) {
    const name = shownName(done.name);
    const at = /^[0-9a-f]{64}$/.test(done.to) ? ` at change ${done.to.slice(0, 8)}` : " at the last commit";
    if (done.change === "added") return `${name} is gone again: it was new in this work, so undoing it removed it.`;
    if (done.change === "removed") return `${name} is back, as it was${at}.`;
    return `${name} is back as it was${at}. The rest of the work stays as it is.`;
  }
  function restoreReceipt() {
    const done = state.restore.done;
    if (!done || state.restore.unknown) return null;
    return h("div", { class: "message message-good section-tight", role: "status", id: "restore-receipt" },
      h("div", {}, h("strong", {}, done.change === "added" ? `Removed ${shownName(done.name)}` : `Undid the change to ${shownName(done.name)}`), h("p", {}, restoreReceiptText(done))),
      button("Dismiss", () => { state.restore.done = null; paint("work-restore"); paint("review-body"); }, { class: "button button-small button-quiet" }));
  }
  function restoreRecovery() {
    const restore = state.restore;
    if (!restore.unknown) return restoreReceipt();
    const working = read("working");
    return h("div", { class: "message message-warn section-tight", role: "alert" }, h("div", {}, h("strong", {}, "The undo's outcome is unknown"), h("p", {}, "The undo may have happened. This page will not send it again. Read the work again to see.")),
      h("div", { class: "page-actions" }, button("Read the work again", () => refresh(["working"]), { class: "button button-small" }),
        button("I've checked", () => { if (!working.data || working.error || working.stale || working.busy) return; restore.unknown = false; restore.error = null; restore.asking = null; forgetWorkReceipt("restore"); paint("work-restore"); paint("work-actions"); repaintCommit(); }, { class: "button button-small button-quiet", disabled: Boolean(!working.data || working.error || working.stale || working.busy) })), operationDetails(restore.operationId));
  }
  function restoreControls(step) {
    if (!can("entity_restore") || !isWorkingReview() || !UUID.test(text(step.entity_id))) return null;
    const restore = state.restore;
    const key = markOf(step);
    if (restore.unknown) return null;
    const guarded = Boolean(restoreGuard(step)) && !read("working").stale && !read("working").error;
    if (restore.asking !== key) return h("div", { class: "step-undo" }, button(step.change === "added" ? "Remove this entity" : "Undo this change", () => { restore.asking = key; restore.error = null; restore.refused = false; paint("review-body"); }, { class: "button button-quiet button-small", id: "review-undo", disabled: !guarded || state.commit.busy || workspaceWriteBusy() }),
      !guarded ? h("p", { class: "note" }, "Kin needs a fresh, exact reading of this entity before it can undo it safely. ", button("Refresh", () => refresh(["working"]), { class: "button button-small" })) : null);
    return h("div", { class: "message message-warn step-undo-confirm", role: "alertdialog", "aria-label": "Undo this change" },
      h("div", {}, h("strong", {}, step.change === "added" ? `Remove ${step.name}?` : step.change === "removed" ? `Bring ${step.name} back?` : `Put ${step.name} back as it was?`),
        h("p", {}, step.change === "added" ? "It is new in this work, so undoing it takes it out. The rest of the work stays as it is." : "It goes back to how it was at the last commit. The rest of the work stays as it is."),
        restore.error ? h("p", { class: "state state-bad" }, restore.error) : null),
      h("div", { class: "page-actions" },
        button("Keep it", () => { restore.asking = null; restore.error = null; restore.refused = false; paint("review-body"); }, { class: "button button-small button-quiet", disabled: restore.busy }),
        // A refused undo is undone from a fresh reading, never by resending the old one.
        restore.refused && restore.error ? button("Read the work again", () => { restore.asking = null; restore.error = null; restore.refused = false; paint("review-body"); refresh(["working"]); }, { class: "button button-small button-quiet", id: "review-undo-reread", disabled: restore.busy }) : null,
        button(restore.busy ? "Undoing…" : "Undo it", () => restoreEntity(step), { class: "button button-small", id: "review-undo-confirm", disabled: restore.busy || !guarded || state.commit.busy || workspaceWriteBusy() })));
  }

  function workLede() {
    const resource = read("working");
    const guide = resource.data;
    if (!guide) return resource.error ? h("div", { class: "section-tight" }, readFailure("working")) : skeleton("skeleton-lede");
    const sentence = workSentence(guide);
    const body = sentence || (guide.working_tree?.differs ? "Your code differs from your last commit, but no entity changed." : "Nothing in flight. Your code matches your last commit.");
    const completeness = guide.summary?.completeness;
    const caveat = sentence && unsettled(guide) ? (isCount(completeness?.pending_entities) && completeness.pending_entities > 0
      ? `Kin is still mapping ${plural(completeness.pending_entities, "entity", "entities")}, so these counts may grow.` : "Kin is still mapping this code, so these counts may grow.") : "";
    const authors = array(guide.summary?.authors).filter((who) => typeof who?.name === "string" && isCount(who.entities));
    const byAgents = [authors.length ? `${authors.slice(0, 3).map((who) => `${who.name} changed ${plural(who.entities, "entity", "entities")}`).join(", ")}.` : "", sentence ? authorGapWords(guide) : ""].filter(Boolean).join(" ");
    return [h("p", { class: `lede${sentence ? " lede-strong" : ""}` }, body),
      byAgents ? h("p", { class: "work-authors" }, byAgents) : null,
      caveat ? h("p", { class: "work-caveat" }, caveat) : null,
      resource.error ? h("p", { class: "note quiet-line" }, "Showing what Kin last saw. ", button("Refresh", () => refresh(["working"]), { class: "button button-small" })) : null];
  }

  function workActions() {
    const guide = read("working").data;
    if (!hasWork(guide)) return null;
    state.review.target = { kind: "working" };
    const steps = reviewSteps(reviewGuide());
    const next = steps.findIndex((step) => !markState(step));
    const looked = steps.filter((step) => markState(step)).length;
    // A commit acts on what is there now, so it waits for the fresh read.
    return [can("verify_run") && !read("working").stale ? button(state.verify.busy ? "Running tests…" : "Run tests", runTests, { id: "work-run-tests", class: "button button-quiet", disabled: state.verify.busy || state.verify.unknown }) : null,
      canCommit() && !state.commit.open && !read("working").stale ? button("Commit", () => { state.commit.open = true; state.commit.result = null; state.commit.error = null; repaintCommit(); byId("commit-message")?.focus(); }, { disabled: state.commit.unknown || state.restore.unknown || state.restore.busy || workspaceWriteBusy() }) : null,
      steps.length ? link(looked === 0 ? "Start review" : next < 0 ? "Finish review" : "Continue review", next < 0 ? "#/review/w/done" : `#/review/w/${next + 1}`, true) : null];
  }

  /** The commit form is drawn on Uncommitted and at the end of a review of uncommitted work; draw whichever is on the page. */
  function repaintCommit() { for (const id of ["work-commit", "work-actions", "review-body"]) paint(id); }

  async function commitWork() {
    const commit = state.commit;
    const message = commit.message.trim();
    if (!message || commit.busy || commit.unknown || state.restore.busy || state.restore.unknown || workspaceWriteBusy() || state.refreshing || read("working").stale || read("working").error) return;
    if (!globalThis.crypto?.randomUUID) { commit.error = "This browser could not create an operation ID. Reload the page before committing."; repaintCommit(); return; }
    commit.operationId = crypto.randomUUID();
    rememberWorkReceipt("commit", commit.operationId);
    commit.busy = true; commit.error = null; commit.remedy = null;
    repaintCommit();
    try {
      const only = commitOnly();
      const held = workSteps(read("working").data);
      const chosen = only ? held.filter((step) => only.includes(step.entity_id)) : held;
      const changedBefore = workChanged(read("working").data);
      const result = await send("/api/workspace/commit", { message, operation_id: commit.operationId, ...(only ? { entities: only } : {}) });
      // The receipt names what this commit took and what it left, as the page saw the work when it was sent.
      commit.result = { ...result, names: chosen.map((step) => shownName(step.name)), took: only ? only.length : changedBefore, left: only ? Math.max(0, changedBefore - only.length) : 0 };
      state.restore.done = null;
      forgetWorkReceipt("commit");
      commit.open = false;
      commit.message = "";
      commit.drafted = false;
      if (result.outcome === "committed" && state.route === "review" && isWorkingReview()) carryMarks(result.change_id, only);
      announce(result.outcome === "committed" ? "Committed." : "Nothing to commit.");
      // What the page holds as in flight was read before the commit, so it is marked as such until the new read lands.
      if (read("working").data) { read("working").stale = true; paint("work-flight"); }
    } catch (error) {
      commit.error = error.message || "The commit failed.";
      commit.remedy = error.remedy || null;
      // A refusal wrote nothing. Anything else may have landed, so this page does not send it again.
      commit.unknown = !(error.status === 409 && error.outcome === "refused") && ![400, 403, 404, 405, 422, 501].includes(error.status);
      if (!commit.unknown) forgetWorkReceipt("commit");
      announce(commit.unknown ? "The commit's outcome is unknown." : "The commit was not made.");
    } finally {
      commit.busy = false;
      repaintCommit();
      setRefreshDisabled(false);
      if (commit.result || commit.unknown) refresh(["working", "changes", "standing", "overview", "branches"]);
    }
  }

  /** What a commit took, as one change, and what it left uncommitted. */
  function commitReceiptWords(result) {
    const names = array(result.names).filter((name) => typeof name === "string" && name);
    const took = isCount(result.took) ? result.took : names.length;
    const some = names.length <= 3 ? names.join(names.length === 2 ? " and " : ", ").replace(/, ([^,]*)$/, " and $1") : `${names.slice(0, 2).join(", ")} and ${count(names.length - 2)} more`;
    const what = names.length ? some : took ? plural(took, "entity", "entities") : "Your work";
    const id = /^[0-9a-f]{64}$/.test(text(result.change_id)) ? [" as change ", h("code", {}, result.change_id.slice(0, 8))] : [" as one change"];
    const title = text(result.title) ? [`, “${result.title}”`] : [];
    const left = isCount(result.left) && result.left > 0 ? ` ${plural(result.left, "entity", "entities")} ${result.left === 1 ? "stays" : "stay"} uncommitted.` : "";
    return [`${what} ${names.length === 1 || (!names.length && took === 1) ? "is" : "are"} committed`, ...id, ...title, `.${left}`];
  }

  /** A first line for the commit, drafted from the entities it holds: what was added, changed and removed, by name. */
  function draftMessage(steps) {
    const by = { added: [], modified: [], removed: [] };
    for (const step of steps) (by[step.change] || by.modified).push(shownName(step.name));
    const some = (names) => names.length <= 3 ? names.join(names.length === 2 ? " and " : ", ").replace(/, ([^,]*)$/, " and $1") : `${names.slice(0, 2).join(", ")} and ${names.length - 2} more`;
    const parts = [];
    if (by.modified.length) parts.push(`Change ${some(by.modified)}`);
    if (by.added.length) parts.push(`${parts.length ? "add" : "Add"} ${some(by.added)}`);
    if (by.removed.length) parts.push(`${parts.length ? "remove" : "Remove"} ${some(by.removed)}`);
    // The clauses join with "and" only between them, never inside a list of names.
    return parts.length > 1 ? `${parts.slice(0, -1).join(", ")} and ${parts.at(-1)}` : parts.join("");
  }

  function prepareCommitDraft(steps) {
    const selected = commitOnly();
    const key = JSON.stringify(selected);
    // Kin's own draft follows the choice of what to commit until the reader writes their own.
    if (state.commit.drafted && state.commit.message.trim() && state.commit.draftKey === key) return;
    if (state.commit.message.trim() && !state.commit.drafted) return;
    state.commit.message = draftMessage(selected ? steps.filter((step) => selected.includes(step.entity_id)) : steps);
    state.commit.drafted = true;
    state.commit.draftKey = key;
  }

  /** When the reader flagged some entities and the server can split a commit, the entities that looked right. */
  function commitOnly() {
    if (!state.commit.onlyFine || !can("entity_commit") || state.route !== "review" || !isWorkingReview()) return null;
    const steps = reviewSteps(reviewGuide());
    const fine = steps.filter((step) => markState(step) === "fine" && UUID.test(text(step.entity_id)));
    const flagged = steps.filter((step) => markState(step) === "flagged");
    return flagged.length && fine.length ? fine.map((step) => step.entity_id) : null;
  }

  function commitBox() {
    const commit = state.commit;
    const guide = read("working").data;
    if (commit.result) {
      const done = commit.result.outcome === "committed";
      return h("div", { class: `message message-${done ? "good" : "warn"} section-tight`, role: "status", id: "commit-receipt" },
        h("div", {}, h("strong", {}, done ? "Committed" : "Nothing to commit"),
          h("p", {}, done ? commitReceiptWords(commit.result) : "Your code already matches your last commit.")),
        h("div", { class: "page-actions" },
          done && /^[0-9a-f]{64}$/.test(text(commit.result.change_id)) ? link("Review it", reviewHref(commit.result.change_id), false, { class: "button button-small" }) : null,
          button("Dismiss", () => { commit.result = null; repaintCommit(); }, { class: "button button-small button-quiet" })));
    }
    if (commit.unknown) {
      return h("div", { class: "message message-warn section-tight", role: "alert" }, h("div", {},
        h("strong", {}, "The commit's outcome is unknown"),
        h("p", {}, "Kin stopped answering after the request was sent. This page will not send it again. Check History to see whether it landed."),
        h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", { class: "detail-text" }, commit.error), commit.operationId ? h("p", {}, "Operation ", h("code", {}, commit.operationId)) : null)),
        button("I've checked", () => { commit.unknown = false; commit.error = null; forgetWorkReceipt("commit"); repaintCommit(); }, { class: "button button-small" }));
    }
    if (!commit.open) return null;
    const changed = workChanged(guide);
    const atFinish = state.route === "review";
    const locked = () => commit.busy || state.restore.unknown || state.restore.busy || workspaceWriteBusy() || read("working").stale || Boolean(read("working").error);
    const submit = button(commit.busy ? "Committing…" : "Commit", commitWork, { id: "commit-submit", class: "button button-primary", disabled: locked() || !commit.message.trim() });
    const field = h("textarea", { id: "commit-message", rows: "3", maxlength: "4000", placeholder: "What changed, and why", "aria-label": "Commit message", disabled: commit.busy,
      on: { input: (event) => { commit.message = event.target.value; commit.drafted = false; submit.disabled = locked() || !commit.message.trim(); needed.hidden = Boolean(commit.message.trim()); const note = byId("commit-drafted"); if (note) note.hidden = true; },
        keydown: (event) => { if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) { event.preventDefault(); commitWork(); } } } }, commit.message);
    const needed = h("span", { class: "note" }, "A message is needed to commit.");
    needed.hidden = Boolean(commit.message.trim());
    const flaggedCount = atFinish && isWorkingReview() ? reviewSteps(reviewGuide()).filter((step) => markState(step) === "flagged").length : 0;
    const fineCount = atFinish && isWorkingReview() ? reviewSteps(reviewGuide()).filter((step) => markState(step) === "fine").length : 0;
    const unmarkedCount = atFinish && isWorkingReview() ? reviewSteps(reviewGuide()).filter((step) => !markState(step)).length : 0;
    const splitting = Boolean(commitOnly());
    // Everything not marked as looking right stays: the flagged, to send back, and the unmarked, to look at.
    const staying = [`the ${spoken(flaggedCount).toLowerCase()} you flagged`, unmarkedCount ? `the ${spoken(unmarkedCount).toLowerCase()} you did not mark` : ""].filter(Boolean).join(" and ");
    const choice = flaggedCount && fineCount && can("entity_commit") ? h("label", { class: "check commit-only" },
      h("input", { type: "checkbox", id: "commit-only-fine", checked: commit.onlyFine, disabled: commit.busy, on: { change: (event) => { commit.onlyFine = event.target.checked; prepareCommitDraft(reviewSteps(reviewGuide())); repaintCommit(); } } }),
      h("span", {}, `Commit only the ${spoken(fineCount).toLowerCase()} that ${fineCount === 1 ? "looks" : "look"} right. ${staying.replace(/^\w/, (c) => c.toUpperCase())} ${flaggedCount + unmarkedCount === 1 ? "stays" : "stay"} uncommitted.`)) : null;
    return h("section", { class: "commit-box", "aria-labelledby": "commit-title" },
      h("h2", { id: "commit-title" }, splitting ? "Commit what looks right" : "Commit this work"),
      h("p", {}, splitting ? `${plural(fineCount, "entity", "entities")} become${fineCount === 1 ? "s" : ""} one change.` : changed ? `Everything uncommitted becomes one change: ${plural(changed, "entity", "entities")}.` : "Everything uncommitted becomes one change."),
      choice,
      field,
      commit.error ? h("div", { class: "message message-bad section-tight", role: "alert" }, h("div", {}, h("strong", {}, "The commit was not made"), h("p", {}, commit.remedy || commit.error),
        commit.remedy ? h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", { class: "detail-text" }, commit.error)) : null)) : null,
      h("div", { class: "page-actions" }, submit,
        // At the end of a review the form is the page, so there is nothing to cancel back to.
        atFinish ? null : button("Cancel", () => { commit.open = false; commit.error = null; repaintCommit(); }, { class: "button button-quiet", disabled: commit.busy }),
        needed,
        commit.drafted && commit.message.trim() ? h("span", { class: "note", id: "commit-drafted" }, "Kin drafted this from what changed. Say why in your own words.") : null));
  }

  const RELATION_WORD = { calls: "calls it", references: "refers to it", overrides: "overrides it", extends: "extends it", implements: "implements it", imports: "imports it", includes: "includes it", uses_type: "uses its type", instantiates: "constructs it", tests: "tests it", covers: "covers it" };
  // Kinds of entity that are types, which something can use the type of.
  const TYPE_KINDS = new Set(["class", "interface", "type_alias", "type", "struct", "enum", "trait", "union", "protocol", "record", "typedef"]);
  /**
   * How one entity relies on another, from the relation's kind and the two
   * entities' kinds. A module relies on what it holds a binding to by
   * importing it, and only a type has a type to use: a module that imports a
   * function never "uses its type".
   */
  function relationWord(relation, from, to) {
    const kind = text(relation).toLowerCase();
    const fromKind = text(from?.kind || from).toLowerCase();
    const toKind = text(to?.kind || to).toLowerCase();
    if (fromKind === "module" && ["uses_type", "references", "imports"].includes(kind)) return "imports it";
    if (kind === "uses_type" && toKind && !TYPE_KINDS.has(toKind)) return "refers to it";
    return RELATION_WORD[kind] || "relies on it";
  }

  /** Lines two versions of an entity share, in order: the longest run both keep. */
  function lineDiff(before, after) {
    const a = String(before).replace(/\s+$/, "").split("\n");
    const b = String(after).replace(/\s+$/, "").split("\n");
    // A very long body is shown as it is now rather than compared line by line.
    if (a.length * b.length > 400_000) return null;
    const width = b.length + 1;
    const table = new Uint32Array((a.length + 1) * width);
    for (let i = a.length - 1; i >= 0; i--) for (let j = b.length - 1; j >= 0; j--) {
      table[i * width + j] = a[i] === b[j] ? table[(i + 1) * width + j + 1] + 1 : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
    const rows = [];
    let i = 0, j = 0;
    while (i < a.length && j < b.length) {
      if (a[i] === b[j]) { rows.push({ type: "same", text: b[j], a: i, b: j }); i++; j++; }
      else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) { rows.push({ type: "del", text: a[i], a: i, b: null }); i++; }
      else { rows.push({ type: "add", text: b[j], a: null, b: j }); j++; }
    }
    for (; i < a.length; i++) rows.push({ type: "del", text: a[i], a: i, b: null });
    for (; j < b.length; j++) rows.push({ type: "add", text: b[j], a: null, b: j });
    return rows;
  }

  /** An entity's body before and after, as one column of kept, removed and added lines. */
  function diffView(step) {
    const before = typeof step.before?.text === "string" ? step.before.text : null;
    const after = typeof step.after?.text === "string" ? step.after.text : null;
    if (before === null && after === null) return null;
    const language = text(step.language);
    const tokensOf = (body) => { try { return globalThis.kinReview?.highlightLines?.(String(body).replace(/\s+$/, ""), language) || null; } catch (_) { return null; } };
    const rows = before !== null && after !== null ? lineDiff(before, after)
      : (after ?? before).replace(/\s+$/, "").split("\n").map((line, index) => ({ type: after !== null ? "add" : "del", text: line, a: after !== null ? null : index, b: after !== null ? index : null }));
    if (!rows) return codeView(after, language, step.after?.start_line, state.diffOpen.has(stepKey(step)), () => { state.diffOpen.add(stepKey(step)); paint("review-body"); });
    const beforeTokens = before !== null ? tokensOf(before) : null;
    const afterTokens = after !== null ? tokensOf(after) : null;
    const startA = Number.isSafeInteger(step.before?.start_line) ? step.before.start_line : null;
    const startB = Number.isSafeInteger(step.after?.start_line) ? step.after.start_line : null;
    const MARK = { same: " ", add: "+", del: "−" };
    const grid = h("div", { class: "diff-lines" });
    // A long body shows its changed lines with three kept lines around each;
    // the runs between them fold into one row that opens them all.
    const CONTEXT = 3;
    const near = new Array(rows.length).fill(false);
    rows.forEach((row, index) => { if (row.type !== "same") for (let at = Math.max(0, index - CONTEXT); at <= Math.min(rows.length - 1, index + CONTEXT); at++) near[at] = true; });
    const changedAny = near.some(Boolean);
    const folding = changedAny && !state.diffOpen.has(stepKey(step)) && rows.length > 24;
    let folded = 0;
    const closeFold = () => {
      if (!folded) return;
      grid.append(h("button", { type: "button", class: "diff-fold", on: { click: () => { state.diffOpen.add(stepKey(step)); paint("review-body"); } } }, `${plural(folded, "unchanged line")}`));
      folded = 0;
    };
    for (const [index, row] of rows.entries()) {
      if (folding && !near[index]) { folded += 1; continue; }
      closeFold();
      const tokens = row.type === "del" ? beforeTokens?.[row.a] : afterTokens?.[row.b];
      const cell = h("span", { class: `diff-text diff-${row.type}` });
      if (Array.isArray(tokens) && tokens.length) for (const [type, value] of tokens) cell.append(type ? h("span", { class: `t-${type}` }, value) : document.createTextNode(value));
      else cell.append(document.createTextNode(row.text || " "));
      if (!cell.textContent) cell.append(document.createTextNode(" "));
      grid.append(
        h("span", { class: `diff-no diff-${row.type}`, "aria-hidden": "true" }, row.a !== null && startA !== null ? String(startA + row.a) : ""),
        h("span", { class: `diff-no diff-${row.type}`, "aria-hidden": "true" }, row.b !== null && startB !== null ? String(startB + row.b) : ""),
        h("span", { class: `diff-mark diff-${row.type}`, "aria-hidden": "true" }, MARK[row.type]),
        cell);
    }
    closeFold();
    const added = rows.filter((row) => row.type === "add").length;
    const removed = rows.filter((row) => row.type === "del").length;
    return h("div", { class: "codeview", tabindex: "0", role: "region", "aria-label": `Source, ${plural(added, "line")} added and ${plural(removed, "line")} removed` }, grid);
  }

  /** What relies on a changed entity from outside the work, as the guide lists and counts it. */
  function outsideOf(step) {
    const connections = step.connections || {};
    const rows = array(connections.consumers).filter((consumer) => consumer && !consumer.changed_here && consumer.entity);
    const total = isCount(connections.consumers_total) && isCount(connections.consumers_changed_here) ? Math.max(0, connections.consumers_total - connections.consumers_changed_here) : rows.length;
    return { rows, total };
  }
  const stepKey = (step) => text(step?.entity_id) || `${text(step?.name)}\u0000${text(step?.within)}`;
  const withinName = (within) => text(within).replace(/^(module|class|interface|namespace|package|struct|trait|impl|crate) /, "");

  // ── Telling apart entities that read the same ──
  // Two entities of one name and kind in one view are told apart by what
  // holds each. The guide says what holds most of them; an entity it places
  // in nothing, such as a module, is read once for the place Kin records it
  // under, and the part of that place where the two differ is said.
  const placeReads = new Map();
  // Bumped as each place read lands, so a part of Home drawn from the places is drawn again.
  let placesRead = 0;
  /** Where Kin records an entity, read once. Home reads it from the entity's own repository, at `api`. */
  function readPlace(id, api = "") {
    if ((MACHINE && !api) || placeReads.has(id) || !UUID.test(id)) return;
    placeReads.set(id, { busy: true });
    const epoch = state.readEpoch;
    fetchJSON(`${api ? `root:${api}` : ""}/api/workspace/entity?id=${encodeURIComponent(id)}&sections=header`)
      .then((data) => { const header = data?.header || {}; placeReads.set(id, { place: text(header.module) || text(header.container?.name) }); })
      .catch(() => { placeReads.set(id, { place: "" }); })
      .finally(() => {
        placesRead += 1;
        if (epoch !== state.readEpoch) return;
        if (MACHINE) repaintHome();
        else for (const section of ["review-body", "work-flight", "review-sites"]) paint(section);
      });
  }

  /**
   * The shortest end of each place that no other place in the list shares:
   * `classic` and `mini` for two `checks` modules, `test_body.test_tutorial001`
   * beside another `test_tutorial001`. A place that names the entity itself,
   * as a module's does, is read from what holds it. A place that cannot be
   * told apart gives nothing.
   */
  function differingParts(places, name) {
    const parts = places.map((place) => { const list = text(place).split(/::|[./\\]/).filter(Boolean); return list.at(-1) === name ? list.slice(0, -1) : list; });
    return parts.map((list, at) => {
      for (let back = 1; back <= list.length; back++) {
        const end = list.slice(-back).join(".");
        if (parts.every((other, index) => index === at || other.slice(-back).join(".") !== end)) return end;
      }
      return "";
    });
  }
  /**
   * For the entities one view shows, what to add after each name that
   * another in the view shares: what holds it, or where its place differs.
   * Nothing is added for a name that is not shared, or that Kin cannot tell
   * apart.
   */
  function nameQualifiers(entities, api = "") {
    const out = new Map();
    const groups = new Map();
    for (const entity of entities) {
      const key = `${shownName(entity?.name)}\u0000${text(entity?.kind).toLowerCase()}`;
      if (!groups.has(key)) groups.set(key, Object.assign([], { name: shownName(entity?.name) }));
      if (!groups.get(key).some((held) => held.id === text(entity.id))) groups.get(key).push({ id: text(entity.id), within: withinName(entity.within) });
    }
    for (const group of groups.values()) {
      if (group.length < 2) continue;
      if (new Set(group.map((held) => held.within)).size === group.length && group.every((held) => held.within)) { for (const held of group) out.set(held.id, held.within); continue; }
      group.forEach((held) => readPlace(held.id, api));
      const places = group.map((held) => placeReads.get(held.id)?.place);
      if (places.some((place) => place === undefined)) continue;
      differingParts(places, group.name).forEach((part, at) => { if (part) out.set(group[at].id, part); });
    }
    return out;
  }
  const qualified = (name, qualifier) => [name, qualifier ? h("span", { class: "name-qualifier" }, ` in ${qualifier}`) : null];
  const placeOf = (step) => {
    const where = withinName(step.within);
    return [kindWord(step.kind), where && where !== step.name ? `in ${where}` : ""].filter(Boolean).join(" ");
  };
  /** What happened to an entity, in one short sentence read from the fields Kin says changed. */
  function changeSentence(step) {
    const kind = text(step.kind).replace(/_/g, " ").toLowerCase() || "entity";
    if (step.change === "added") return `New ${kind}.`;
    if (step.change === "removed") return `This ${kind} was removed.`;
    const fields = new Set(array(step.fields).map((field) => text(field?.field)));
    if (fields.has("signature") && fields.has("body")) return "Its signature and its body changed.";
    if (fields.has("signature")) return "Its signature changed.";
    if (fields.has("body")) return "Its body changed. Its signature did not.";
    // The comment above it is not in its code, so a documentation edit leaves the code reading the same token for token.
    if (fields.has("doc_summary") && fields.size === 1) return "Its documentation changed. Its code reads the same token for token.";
    return "It changed.";
  }
  /** What its documentation said before and after, when that is what changed. */
  function documentationChange(step) {
    const field = array(step.fields).find((item) => text(item?.field) === "doc_summary");
    if (!field) return null;
    const said = (value) => text(value) ? h("q", {}, text(value)) : h("span", { class: "aside" }, "none");
    return h("dl", { class: "doc-change" },
      h("dt", {}, "Documentation before"), h("dd", {}, said(field.before)),
      h("dt", {}, "Documentation after"), h("dd", {}, said(field.after)));
  }
  /** The entities whose documentation Kin only read again: said once, quietly, never walked. */
  function rereadLine(guide) {
    const summary = guide?.summary || {};
    const total = isCount(summary.documentation_rereads) ? summary.documentation_rereads : 0;
    if (!total) return null;
    const names = array(summary.documentation_reread_names).map(text).filter(Boolean);
    const who = total === 1 && names.length === 1 ? names[0] : plural(total, "entity", "entities");
    return h("p", { class: "note quiet-line", id: "review-rereads" }, `Kin re-read the documentation of ${who}; ${total === 1 ? "the code is" : "their code is"} identical.`);
  }

  /** One changed entity in full: what happened, what that asks of the reader, its source before and after, what relies on it, and the tests that reach it. */
  function stepDetail(step, guide, onCleanup) {
    const connections = step.connections || {};
    const consumers = array(connections.consumers).filter((consumer) => consumer?.entity);
    const consumersTotal = isCount(connections.consumers_total) ? connections.consumers_total : consumers.length;
    const tests = array(connections.tests).filter((test) => test?.entity || test?.name);
    const testsTotal = isCount(connections.tests_total) ? connections.tests_total : tests.length;
    const rank = stepAttention(step);
    return h("article", { class: "step", "aria-label": step.name },
      h("header", { class: "step-head" },
        h("h1", { class: "step-name" }, ent({ id: step.entity_id, name: step.name, kind: step.kind }, { linked: step.change !== "removed" })),
        h("p", { class: "step-place" }, [placeOf(step), authorWords(step, guide)].filter(Boolean).join(" · ")),
        h("p", { class: `step-verdict${rank === 0 ? " step-verdict-bad" : rank === 1 ? " step-verdict-warn" : ""}` }, [`${changeSentence(step)} ${stepReason(step, guide)}.`, stepTestsWords(step, guide)].filter(Boolean).join(" "))),
      h("section", { class: "step-part", "aria-labelledby": "step-source-title" },
        h("h2", { id: "step-source-title" }, step.change === "added" ? "New source" : step.change === "removed" ? "Removed source" : "Before and after"),
        documentationChange(step),
        diffView(step) || h("p", { class: "gap" }, "No source was returned for this entity.")),
      h("section", { class: "step-part", "aria-labelledby": "step-relies-title" },
        h("h2", { id: "step-relies-title" }, "What relies on it", h("span", { class: "count" }, graphBadge(consumersTotal, limitedGuide(guide)))),
        consumers.length ? [stepPicture(step, onCleanup), section("review-sites", [], () => callSites(step))]
          : h("p", { class: "gap" }, "Kin found no entities that rely on it.")),
      h("section", { class: "step-part", "aria-labelledby": "step-tests-title" },
        h("h2", { id: "step-tests-title" }, "Tests that reach it", testsTotal ? h("span", { class: "count" }, graphBadge(testsTotal, limitedGuide(guide))) : null),
        stepTests(step, tests, testsTotal, guide),
        tests.some((test) => ["passed", "failed"].includes(test.result?.state)) ? h("p", { class: "note" }, "These results describe the test command. Kin did not report individual test outcomes.") : null));
  }

  /** How far a test is from what it reaches: Kin counts the recorded steps between them, and a direct one is one step. */
  const hopsOf = (row) => isCount(row?.hops) && row.hops > 0 ? row.hops : 1;
  const stepsWords = (n) => n === 1 ? "1 step away" : `${count(n)} steps away`;
  /** How a test reaches what changed: how far, and whether every step on the way is a call. */
  const testRoute = (row) => {
    const hops = hopsOf(row);
    const byCall = isCount(row?.call_hops) && row.call_hops >= hops;
    return byCall ? `${stepsWords(hops)}, ${hops === 1 ? "calls it" : "through calls"}` : stepsWords(hops);
  };
  /** How far Kin looked for tests, when the guide says. */
  const testDepth = (step) => isCount(step?.connections?.tests_depth) && step.connections.tests_depth > 0 ? step.connections.tests_depth : null;
  /** No test, said with how far Kin looked, so a bounded search never reads as a bare zero. */
  function noTestWords(depth, what = "it") {
    return depth ? `No test found within ${count(depth)} ${depth === 1 ? "step" : "steps"} of ${what}.` : `Kin found no tests that reach ${what}.`;
  }

  /** The tests that reach a changed entity, each with how far it is and its latest result; the rest open in place. */
  function stepTests(step, tests, total, guide) {
    if (!tests.length) return h("p", { class: "gap" }, total ? `Kin counted ${graphCount(total, limitedGuide(guide))} ${total === 1 ? "test" : "tests"} that reach it and listed none here.` : noTestWords(testDepth(step)));
    const SHOWN = 8;
    const key = stepKey(step);
    const open = state.testsOpen.has(key);
    const shown = open ? tests : tests.slice(0, SHOWN);
    const hidden = tests.length - shown.length;
    const unlisted = Math.max(0, total - tests.length);
    const ofAll = limitedGuide(guide) ? `of at least ${count(total)}` : `of the ${count(total)}`;
    const apart = nameQualifiers(shown.map((test) => test.entity || test));
    return h("ul", { class: "rows", id: "step-tests" }, shown.map((test) => h("li", { class: "rows-item" }, ent(test.entity || test),
      apart.get(text((test.entity || test).id)) ? h("span", { class: "name-qualifier" }, `in ${apart.get(text((test.entity || test).id))}`) : null,
      h("span", { class: "rows-how" }, testRoute(test)), resultWord(test.result))),
      hidden > 0 ? h("li", { class: "rows-item rows-more" }, button(unlisted ? `Show ${count(hidden)} more ${ofAll}` : `Show ${count(hidden)} more`, () => { state.testsOpen.add(key); paint("review-body"); }, { class: "button button-small button-quiet", id: "step-tests-more" })) : null,
      !hidden && unlisted ? h("li", { class: "rows-item rows-more" }, `Kin lists ${count(tests.length)} ${ofAll} here.`) : null);
  }

  /**
   * Where an agent's own activity meets uncommitted work, read from the two
   * records Kin already keeps: an agent changing an entity this work also
   * changed, an agent changing something that relies on one, and an agent
   * whose last read of a changed entity came before the latest edit to this
   * work, so what it saw may no longer be what is there.
   */
  function workOverlaps() {
    const guide = read("working").data;
    const agents = array(read("agents").data?.agents);
    const family = agentsApi()?.family;
    const steps = workSteps(guide).filter((step) => UUID.test(text(step.entity_id)));
    if (!steps.length || !agents.length || !family) return [];
    const mine = new Map(steps.map((step) => [step.entity_id, step]));
    const relying = new Map();
    for (const step of steps) for (const consumer of array(step.connections?.consumers)) {
      const id = text(consumer?.entity?.id);
      if (UUID.test(id) && !mine.has(id) && !relying.has(id)) relying.set(id, { step, entity: consumer.entity });
    }
    const edited = new Date(text(guide.working_tree?.changed_at)).getTime();
    const WRITES = ["stage", "commit", "mutate"];
    const ABOUT = ["source", "context", "refs", "impact", "neighbors", "trace", "history"];
    const found = new Map();
    const keep = (key, overlap) => { if (!found.has(key)) found.set(key, overlap); };
    for (const agent of agents) {
      // Calls are newest first, so the first one kept for an entity is the agent's latest.
      const latestRead = new Set();
      for (const call of array(agent.recent_calls)) {
        const kind = family(call.tool);
        const id = text(array(call.entity_ids)[0]);
        if (!UUID.test(id) || call.is_error) continue;
        if (WRITES.includes(kind)) {
          if (mine.has(id)) keep(`${agent.key} same ${id}`, { tone: "bad", agent, at: call.at, step: mine.get(id), kind: "same" });
          else if (relying.has(id)) keep(`${agent.key} relying ${id}`, { tone: "warn", agent, at: call.at, step: relying.get(id).step, entity: relying.get(id).entity, kind: "relying" });
        } else if (ABOUT.includes(kind) && mine.has(id) && !latestRead.has(id)) {
          latestRead.add(id);
          const when = new Date(call.at).getTime();
          if (Number.isFinite(edited) && Number.isFinite(when) && when < edited) keep(`${agent.key} stale ${id}`, { tone: "warn", agent, at: call.at, step: mine.get(id), kind: "stale" });
        }
      }
    }
    const order = { same: 0, relying: 1, stale: 2 };
    return [...found.values()].sort((a, b) => order[a.kind] - order[b.kind]);
  }

  function overlapList() {
    const overlaps = workOverlaps();
    if (!overlaps.length) return null;
    const who = (agent) => h("a", { class: "author-agent", href: `#/agents/${agent.key}` }, agentsApi()?.mark?.(12) || null, agent.name);
    const named = (step) => ent({ id: step.entity_id, name: step.name, kind: step.kind }, { linked: step.change !== "removed" });
    return h("section", { class: "section", "aria-labelledby": "work-overlaps-title" },
      h("div", { class: "section-head" }, h("h2", { id: "work-overlaps-title" }, "Overlaps with agents", h("span", { class: "count" }, count(overlaps.length))),
        h("span", { class: "aside" }, "Where an agent's work meets yours, before either is committed")),
      h("ul", { class: "list list-card" }, overlaps.slice(0, 8).map((overlap) => h("li", { class: `overlap overlap-${overlap.tone}` },
        h("p", { class: "overlap-text" },
          overlap.kind === "same" ? [who(overlap.agent), " changed ", named(overlap.step), ", which your uncommitted work also changed."]
            : overlap.kind === "relying" ? [who(overlap.agent), " changed ", ent(overlap.entity), ", which relies on ", named(overlap.step), " in your uncommitted work."]
            : [who(overlap.agent), " last read ", named(overlap.step), " before your latest edit to this work, so what it saw may be out of date."]),
        h("time", { class: "aside", datetime: text(overlap.at) || undefined }, ago(overlap.at))))),
      overlaps.length > 8 ? h("p", { class: "note quiet-line" }, `${count(overlaps.length - 8)} more.`) : null);
  }

  function uncommittedList(onCleanup) {
    const resource = read("working");
    const guide = resource.data;
    if (!guide) return resource.error ? null : skeletonLines(5);
    state.review.target = { kind: "working" };
    const steps = reviewSteps(reviewGuide());
    if (!steps.length && !workChanged(guide)) {
      // The sentence above already says the code differs with no entity changed.
      if (guide.working_tree?.differs) return null;
      const latest = peopleChanges()[0];
      return h("div", { class: "empty" },
        h("h3", {}, "Nothing is uncommitted"),
        h("p", {}, "When you or an agent change this code, the entities that changed are listed here before anything is committed, with what relies on each of them."),
        latest ? h("div", { class: "page-actions" }, link("Review the latest change", reviewHref(latest.id))) : null);
    }
    return [resource.stale ? h("p", { class: "note aside-reading" }, `As of ${clock(resource.at)}. Refreshing`) : null,
      reviewOverview(guide, steps, { kind: "working" }, onCleanup)];
  }

  /**
   * Work is one section with three views of the same thing: a change before
   * it is committed, a line of work parked on a branch, and what has landed.
   * Each view keeps its own address, so a tab is a link.
   */
  function workHead(actions) {
    return h("header", { class: "page-head page-head-tabs" },
      h("div", { class: "page-title-row" }, h("h1", {}, "Work"), actions),
      section("work-tabs", ["working", "branches"], () => {
        const changed = uncommittedCount();
        const others = branchRows().filter((row) => !row.active).length;
        const tab = (route, label, badge, tone) => h("a", { class: "tab", href: `#/${route}`, "aria-current": state.route === route ? "page" : undefined }, label,
          badge ? h("span", { class: `tab-count${tone ? ` tab-count-${tone}` : ""}` }, badge) : null);
        return h("nav", { class: "tabs", "aria-label": "Work" },
          tab("uncommitted", "Uncommitted", changed === null ? "•" : changed ? count(changed) : null, "work"),
          tab("branches", "Branches", others ? count(others) : null),
          tab("history", "History"));
      }));
  }

  function uncommitted() {
    return [
      workHead(section("work-actions", ["working", "changes", "session"], workActions, { class: "page-actions" })),
      section("work-lede", ["working"], workLede),
      section("work-verify", ["working"], verifyNote),
      section("work-restore", ["working"], restoreRecovery),
      section("work-commit", ["working", "session"], commitBox),
      section("work-overlaps", ["working", "agents"], overlapList),
      section("work-flight", ["working", "changes"], uncommittedList)];
  }

  // ── Review ───────────────────────────────────────────────────────────────
  // A change read as the things in it that matter, one at a time. A review
  // opens on its list: every changed entity, the ones something else relies on
  // first. Each entity then has a page of its own: what happened to it, its
  // source before and after, what relies on it and whether that was updated,
  // and the tests that reach it. The last page says what was flagged and takes
  // the reader's view of the change. A review is the reader's own record; it
  // stops and starts nothing. Every fact is the guided review's own, and this
  // page only orders it and remembers what was looked at.

  const reviewTargetKey = (target) => target?.kind === "working" ? "w" : target?.kind === "range" ? `r:${target.base}..${target.head}` : text(target?.id);
  const reviewBase = (target) => target?.kind === "working" ? "#/review/w" : target?.kind === "range" ? `#/review/r/${encodeURIComponent(target.base)}..${encodeURIComponent(target.head)}` : `#/review/c/${encodeURIComponent(target.id)}`;
  const rangeHref = (base, head) => `#/review/r/${encodeURIComponent(base)}..${encodeURIComponent(head)}`;
  const reviewList = (target) => target?.kind === "working" ? UNCOMMITTED : reviewBase(target);
  const isWorkingReview = () => state.review.target?.kind === "working";

  /** How much a changed entity asks of the reader: what was left behind first, then what may need a look, then the rest. */
  function stepAttention(step) {
    const { total } = outsideOf(step);
    const verdict = text(step.consumer_verdict?.verdict);
    if (total && verdict === "not_updated") return 0;
    if (total && verdict === "may_need_review") return 1;
    return 2;
  }
  /** The things to review, in the order they are walked: by attention, then in the guide's own order. */
  function reviewSteps(guide) {
    return workSteps(guide).map((step, index) => ({ step, index })).sort((a, b) => stepAttention(a.step) - stepAttention(b.step) || a.index - b.index).map(({ step }) => step);
  }
  /** What a changed entity asks of the reader, in a few words that finish the sentence saying what happened to it. */
  function stepReason(step, guide) {
    const { total } = outsideOf(step);
    const rank = stepAttention(step);
    // Said at the start of a sentence, so a lower bound reads "At least".
    const places = `${graphCount(total, limitedGuide(guide))} ${total === 1 ? "entity relies" : "entities rely"} on it`.replace(/^at/, "At");
    if (rank === 0) return `${places} and ${total === 1 ? "was" : "were"} not updated`;
    if (rank === 1) return `${places} and did not change here`;
    return total ? `${places} and ${total === 1 ? "needs" : "need"} no update` : "Kin found no connections outside this change";
  }

  /** Whether a test reaches a changed entity, said where the reader starts. */
  function stepTestsWords(step, guide) {
    // Something removed has nothing left for a test to reach.
    if (step.change === "removed") return "";
    const connections = step.connections || {};
    const total = isCount(connections.tests_total) ? connections.tests_total : array(connections.tests).length;
    const failedTests = isCount(step.tests_result?.failed) ? step.tests_result.failed : 0;
    // Counts are numerals here, as the sentence before this one says its own.
    if (failedTests) return `${count(failedTests)} ${failedTests === 1 ? "test is" : "tests are"} included in a failed run.`;
    if (total) return `${graphCount(total, limitedGuide(guide)).replace(/^at/, "At")} ${total === 1 ? "test reaches" : "tests reach"} it${withinSteps(testDepth(step))}.`;
    return noTestWords(testDepth(step));
  }

  /** A row's reason: what is unusual about the change, when something is, then what it asks of the reader. */
  function rowReason(step, guide) {
    const fields = new Set(array(step.fields).map((field) => text(field?.field)));
    const lead = step.change === "added" ? "New. " : step.change === "removed" ? "Removed. " : fields.has("signature") ? "Signature changed. " : "";
    return `${lead}${stepReason(step, guide)}.`;
  }

  // A source edit makes an entity new to the review again, including a change
  // to a literal or a comment that leaves its syntax shape unchanged.
  const markOf = (step) => {
    const body = text(step.after?.body_hash) || text(step.before?.body_hash);
    const behavior = text(step.evidence?.fingerprint_after?.behavior) || text(step.evidence?.fingerprint_before?.behavior);
    const shape = text(step.evidence?.fingerprint_after?.ast) || text(step.evidence?.fingerprint_before?.ast);
    return `${text(step.entity_id) || text(step.name)}:${body || (/^[0-9a-f]{64}$/i.test(behavior) ? behavior : shape)}`;
  };
  function marksKey(guide) {
    const subject = isWorkingReview() ? `w:${text(guide?.change?.base)}` : reviewTargetKey(state.review.target);
    return `kin.workspace.review:${apiBase}:${subject}`;
  }
  function loadMarks(guide) {
    const marks = { fine: new Set(), flagged: new Set(), notes: {} };
    try {
      const held = JSON.parse(localStorage.getItem(marksKey(guide)));
      for (const key of array(held?.fine)) if (typeof key === "string") marks.fine.add(key);
      for (const key of array(held?.flagged)) if (typeof key === "string") marks.flagged.add(key);
      for (const [key, note] of Object.entries(held?.notes || {})) if (typeof note === "string") marks.notes[key] = note.slice(0, 400);
    } catch (_) { /* Nothing kept; every entity is new. */ }
    return marks;
  }
  const marksBody = (marks) => JSON.stringify({ fine: [...marks.fine], flagged: [...marks.flagged], notes: marks.notes || {} });
  function saveMarks() {
    const { marks, guide } = state.review;
    try { localStorage.setItem(marksKey(guide), marksBody(marks)); } catch (_) { /* Held for this page. */ }
  }
  function setMark(step, kind, note = "") {
    const { marks } = state.review;
    const key = markOf(step);
    marks.fine.delete(key); marks.flagged.delete(key);
    delete marks.notes[key];
    if (kind) marks[kind].add(key);
    if (kind === "flagged" && note.trim()) marks.notes[key] = note.trim().slice(0, 400);
    saveMarks();
  }
  const noteOf = (step) => text(state.review.marks.notes?.[markOf(step)]) || (state.review.marks.fine.has(markOf(step)) ? "" : text(keptFlag(step)?.note));
  // A flag is also kept in the repository when the server can keep it, so the
  // agent that changed the entity sees it the next time it touches it. The
  // mark on this machine stays what the walk reads; the repository's copy is
  // what reaches the agent.
  const HASH = /^[0-9a-f]{64}$/i;
  /**
   * The repository's open flag on this step's entity. In a review of a
   * recorded change, only a flag set on the version that change left counts:
   * one set on a later version is about something else.
   */
  function flagFor(step) {
    const flag = array(read("flags").data?.flags).find((one) => one && one.entity_id === step.entity_id) || null;
    if (!flag || isWorkingReview()) return flag;
    const reviewed = text(step.after?.body_hash);
    return HASH.test(reviewed) && text(flag.fingerprint) === `source:${reviewed}` ? flag : null;
  }
  // What became of sharing a mark made while reviewing a recorded change, by the mark it is about.
  const shareOf = (step) => state.review.shares?.get(markOf(step)) || null;
  const SHARE_WORDS = { sending: "Sharing with agents", kept: "Kept in this browser", changed: "Not shared: it changed since", failed: "Not shared" };
  /**
   * A mark on a recorded change reaches agents through the same flag as a
   * mark on uncommitted work, but only while the entity is still what the
   * change left: a flag on a version that is gone would tell an agent about
   * code it can no longer meet. Kin's own reading of the entity now decides.
   */
  async function shareRecordedMark(step, kind, note) {
    const review = state.review;
    const key = markOf(step);
    const shares = review.shares || (review.shares = new Map());
    const ticket = {};
    const name = shownName(step.name);
    const reviewed = text(step.after?.body_hash);
    const said = (status, words = "") => { if (shares.get(key)?.ticket === ticket || status === "sending") { shares.set(key, { ticket, state: status, words }); paint("review-body"); } };
    if (kind !== "flagged") {
      shares.delete(key);
      const open = flagFor(step);
      if (open) send("/api/workspace/flags/resolve", { entity: step.entity_id, operation_id: crypto.randomUUID() }).then(() => pulse(["flags"], 0)).catch(() => { /* The open flag stays; the next read shows it. */ });
      return;
    }
    if (step.change === "removed") { shares.set(key, { ticket, state: "kept", words: `${name} was removed in this change, so no agent can meet it. The flag stays in this browser.` }); paint("review-body"); return; }
    if (!HASH.test(reviewed)) { shares.set(key, { ticket, state: "kept", words: `Kin has no exact reading of ${name} as this change left it, so the flag stays in this browser.` }); paint("review-body"); return; }
    said("sending");
    try {
      const current = await fetchJSON(`/api/workspace/entity?id=${encodeURIComponent(step.entity_id)}&sections=source`);
      const now = text(current?.source?.body_hash);
      if (!HASH.test(now)) return said("kept", `Kin could not read ${name} exactly as it is now, so it cannot tell whether an agent would meet the version you flagged. The flag stays in this browser.`);
      if (now.toLowerCase() !== reviewed.toLowerCase()) return said("changed", `${name} changed after this change, so the flag was not shared: an agent would be told about a version that is gone. Flag it where it changed next.`);
      await send("/api/workspace/flags", { entity: step.entity_id, note: note.trim().slice(0, 400) || undefined, fingerprint: `source:${reviewed}`, operation_id: crypto.randomUUID() });
      said("shared");
      pulse(["flags"], 0);
    } catch (error) {
      said("failed", error.remedy || error.message || "The flag was not kept in the repository.");
    }
  }
  /** Whether a flagged entity's flag reached agents, in a few words; the full reason when it did not. */
  function shareWords(step, full = false) {
    const shared = isWorkingReview() ? null : shareOf(step);
    if (shared && shared.state !== "shared") return full ? shared.words || SHARE_WORDS[shared.state] : SHARE_WORDS[shared.state];
    const flag = flagFor(step);
    if (!flag && !shared && !isWorkingReview() && markState(step) === "flagged" && can("review_flags") && read("flags").data) {
      return full ? "This flag is kept in this browser. The repository holds no flag on this version, so no agent has been told." : SHARE_WORDS.kept;
    }
    return seenWords(flag);
  }
  function shareMark(step, kind, note) {
    if (!can("review_flags") || !UUID.test(text(step.entity_id)) || !globalThis.crypto?.randomUUID) return;
    if (!isWorkingReview()) { shareRecordedMark(step, kind, note); return; }
    const bodyHash = text(step.after?.body_hash);
    if (kind === "flagged" && !/^[0-9a-f]{64}$/i.test(bodyHash)) { state.review.flagError = "Kin needs an exact source reading before it can save this flag for agents."; paint("review-body"); return; }
    const fingerprint = `source:${bodyHash}`;
    state.review.flagError = null;
    const request = kind === "flagged" ? send("/api/workspace/flags", { entity: step.entity_id, note: note.trim().slice(0, 400) || undefined, fingerprint, operation_id: crypto.randomUUID() })
      : flagFor(step) ? send("/api/workspace/flags/resolve", { entity: step.entity_id, operation_id: crypto.randomUUID() }) : null;
    request?.then(() => { state.review.flagError = null; pulse(["flags"], 0); }).catch((error) => { state.review.flagError = error.remedy || error.message || "The flag was not kept in the repository."; paint("review-body"); });
  }
  /** Whether an agent has seen a flag yet, in a few words. */
  function seenWords(flag) {
    if (!flag) return "";
    const seen = array(flag.seen_by).filter((who) => typeof who?.agent === "string");
    if (flag.source_unavailable) return "Kin could not check whether this flag is still current";
    if (flag.changed_since) return "Changed since you flagged it";
    return seen.length ? `Seen by ${seen[0].agent} ${ago(seen[0].at)}` : "No agent has seen it yet";
  }
  /** What was looked at in uncommitted work stays looked at in the change it was committed as. */
  function carryMarks(changeId, selected = null) {
    if (!/^[\w-]{1,128}$/.test(text(changeId))) return;
    const marks = state.review.marks;
    const filtered = (chosen) => {
      const includes = (key) => selected.some((id) => key.startsWith(`${id}:`)) === chosen;
      return { fine: new Set([...marks.fine].filter(includes)), flagged: new Set([...marks.flagged].filter(includes)), notes: Object.fromEntries(Object.entries(marks.notes).filter(([key]) => includes(key))) };
    };
    try {
      localStorage.setItem(`kin.workspace.review:${apiBase}:${changeId}`, marksBody(selected ? filtered(true) : marks));
      if (selected) localStorage.setItem(`kin.workspace.review:${apiBase}:w:${changeId}`, marksBody(filtered(false)));
    } catch (_) { /* The current page still holds its marks. */ }
  }
  /**
   * The repository's open flag on exactly the version this step shows. A
   * flag is kept in the repository, so it reads the same in any browser; a
   * mark of "looks right" has no record there and stays in this browser.
   */
  function keptFlag(step) {
    const reviewed = text(step?.after?.body_hash);
    if (!HASH.test(reviewed)) return null;
    return array(read("flags").data?.flags).find((one) => one && one.entity_id === step.entity_id && text(one.fingerprint).toLowerCase() === `source:${reviewed.toLowerCase()}`) || null;
  }
  // The mark made in this browser first; otherwise the flag the repository keeps on this version.
  const markState = (step) => state.review.marks.flagged.has(markOf(step)) ? "flagged" : state.review.marks.fine.has(markOf(step)) ? "fine" : keptFlag(step) ? "flagged" : "";

  /** The guide the open review reads: the page's own working read for uncommitted work, otherwise the change's. Its marks follow it. */
  function reviewGuide() {
    const review = state.review;
    if (state.route === "review" && state.params.target && reviewTargetKey(state.params.target) !== reviewTargetKey(review.target)) return null;
    if (review.target?.kind === "working") review.guide = read("working").data;
    const guide = review.guide;
    const key = guide ? marksKey(guide) : null;
    if (key !== review.marksKey) { review.marksKey = key; review.marks = guide ? loadMarks(guide) : { fine: new Set(), flagged: new Set(), notes: {} }; }
    return guide;
  }

  // A guide Kin could not read yet, because it took too long or the graph was being written as it read, is asked for
  // again by itself: soon at first, then less often. Each attempt waits longer than the one before, from a minute
  // and a half up to five minutes, since the machine is slowest when agents are busy and that is when their work
  // waits for review. Kin keeps reading a committed change while the page waits, so an attempt asked again joins
  // the read already under way rather than starting it over.
  const REVIEW_ATTEMPTS = 8;
  const reviewRetryWait = (attempt) => Math.min(15_000, 2000 * 2 ** Math.max(0, attempt - 1));
  const reviewPatience = (attempt) => Math.min(300_000, 90_000 + 60_000 * Math.max(0, attempt));
  const canReadAgain = (detail) => isSlow(detail) || isUnsettled(detail);
  /**
   * Whether an answer says Kin is still reading the guide rather than giving
   * it: a guide has a summary, and a server that is still working says so
   * with its state. Such an answer is not a failed attempt.
   */
  const stillReading = (body) => Boolean(body) && !body.summary && (["reading", "pending", "working", "running"].includes(text(body.state)) || body.pending === true);
  /** How long to wait before asking about a read Kin says is still running, in milliseconds. */
  const askAgainIn = (body) => { const seconds = Number(body?.retry_after_secs ?? body?.retry_after); return Number.isFinite(seconds) && seconds > 0 ? Math.min(30, seconds) * 1000 : 3000; };

  async function loadReview(target) {
    const review = state.review;
    const same = reviewTargetKey(review.target) === reviewTargetKey(target);
    review.target = target;
    if (!same) { review.request++; review.busy = false; review.guide = null; review.error = null; review.lastError = null; review.attempt = 0; review.since = null; review.working = false; review.marksKey = null; review.shares = new Map(); review.decision = { busy: false, error: null, done: null, comment: "" }; }
    clearTimeout(review.retry); review.retryAt = null;
    // Uncommitted work is the page's own read; the review follows it as it changes.
    if (target.kind === "working") { reviewGuide(); paint("review-head"); paint("review-body"); return; }
    if (same && review.guide) return;
    // A change Home or another review in this tab already read opens as it was read.
    const held = target.kind === "change" ? heldChangeGuide(apiBase, target.id) : null;
    if (held) { review.guide = held; review.error = null; review.lastError = null; reviewGuide(); paint("review-body"); paint("review-head"); return; }
    const mine = ++review.request;
    review.busy = true;
    // A new attempt shows itself at once; what the last one met stays behind Show details until this one answers.
    review.error = null;
    review.startedAt = Date.now();
    // How long the reader has waited, across every attempt.
    if (!review.since) review.since = review.startedAt;
    paint("review-body"); paint("review-head");
    tickReview();
    try {
      const path = target.kind === "range" ? `/api/guide?base=${encodeURIComponent(target.base)}&change=${encodeURIComponent(target.head)}` : `/api/guide?change=${encodeURIComponent(target.id)}`;
      const guide = await fetchJSON(path, { patience: reviewPatience(review.attempt || 0) });
      if (mine !== review.request) return;
      if (stillReading(guide)) {
        // Kin says it is still reading: this attempt goes on, and nothing failed.
        review.working = true;
        review.retryAt = Date.now() + askAgainIn(guide);
        review.retry = setTimeout(() => { if (mine === review.request && state.route === "review" && !review.guide) loadReview(target); }, askAgainIn(guide));
        return;
      }
      review.guide = guide;
      review.error = null; review.lastError = null; review.attempt = 0; review.since = null; review.working = false;
      if (target.kind === "change") keepChangeGuide(apiBase, target.id, guide);
      reviewGuide();
    } catch (error) {
      if (mine !== review.request) return;
      const said = error.message || "This change could not be read.";
      review.error = said; review.lastError = said;
      review.attempt = (review.attempt || 0) + 1;
      if (canReadAgain(said) && review.attempt < REVIEW_ATTEMPTS) {
        const wait = reviewRetryWait(review.attempt);
        review.retryAt = Date.now() + wait;
        review.retry = setTimeout(() => { if (mine === review.request && state.route === "review" && review.error) loadReview(target); }, wait);
        // What the repository is busy with is read again, so the wait names it as it stands.
        pulse(["session"], 0);
      } else review.since = null;
    } finally {
      if (mine === review.request) { review.busy = false; paint("review-body"); paint("review-head"); tickReview(); }
    }
  }

  /** Keep the seconds on a waiting review current, without drawing the rest of it again. */
  function tickReview() {
    const review = state.review;
    clearTimeout(review.tick);
    if (review.guide || !(review.busy || review.retryAt)) return;
    review.tick = setTimeout(() => {
      if (state.route !== "review" || review.guide) return;
      const clock = byId("review-wait-clock");
      const said = reviewClock();
      // The first time there is something to say, the waiting state is drawn with it; after that only the seconds change.
      if (clock) clock.textContent = said;
      else if (said) paint("review-body");
      tickReview();
    }, 1000);
  }

  /** A wait in a few words: "45 s", "2 min 10 s". */
  function waitWords(ms) {
    const seconds = Math.max(0, Math.floor(ms / 1000));
    return seconds < 120 ? `${seconds} s` : `${Math.floor(seconds / 60)} min${seconds % 60 ? ` ${seconds % 60} s` : ""}`;
  }
  /** How long the reader has waited for this read across every attempt, or when the next attempt starts. */
  function reviewClock() {
    const review = state.review;
    const seconds = (ms) => Math.max(1, Math.round(ms / 1000));
    const waited = Date.now() - (review.since || review.startedAt || Date.now());
    // Kin said it is still reading: the attempt goes on, and the clock with it.
    if (review.busy || review.working) {
      const so = waited >= 1000 ? `, ${waitWords(waited)} so far` : "";
      if (review.lastError) return `Reading it again${review.attempt ? `, attempt ${review.attempt + 1}` : ""}${so}.`;
      if (review.working) return `Kin is still reading ${review.target?.kind === "range" ? "them" : "it"}${so}.`;
      return waited >= 3000 ? `${waitWords(waited)} so far.` : "";
    }
    if (review.retryAt) return `Kin reads ${review.target?.kind === "range" ? "them" : "it"} again in ${seconds(review.retryAt - Date.now())} s, attempt ${review.attempt + 1}.`;
    return "";
  }

  /** A guide Kin is still reading, or reading again: what is happening in plain words, how far it has got, and Kin's own words behind Show details. */
  function reviewWaiting() {
    const review = state.review;
    const range = review.target?.kind === "range";
    const what = range ? "these branches" : "this change";
    const said = text(review.lastError).trim();
    const name = text(session()?.repository?.name) || "This repository";
    const lead = !said ? (range ? "Comparing these branches." : "Reading this change.")
      : isUnsettled(said) ? `Kin was taking in new work in ${name} while it read ${what}, so it is reading ${range ? "them" : "it"} again.`
      : range ? "Kin is still comparing these branches. This fills in when it is ready." : "Kin is still reading this change. This fills in when it is ready.";
    // A repository that is reading its older history says so, since Home does, and says this read does not wait for it.
    const history = session()?.history;
    const step = isCount(history?.step) && isCount(history?.steps) && history.steps > 0 ? `, step ${count(history.step)} of ${count(history.steps)}` : "";
    const reading = history?.deriving ? h("p", { class: "note" }, `${name} is reading its older history in the background${step}. This review reads ${range ? "the changes as they were" : "the change as it was"} committed, so it does not wait for that.`) : null;
    const clock = reviewClock();
    const showClock = Boolean(said) || Boolean(clock);
    return [h("div", { class: "section-tight" },
      showClock || reading ? h("p", { class: "note aside-reading", role: "status" }, lead) : null,
      reading,
      showClock ? h("p", { class: "note", id: "review-wait-clock" }, clock) : null,
      said ? h("div", { class: "page-actions" }, review.busy ? null : button("Try now", () => loadReview(review.target), { class: "button button-small" }),
        h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", { class: "detail-text" }, said))) : null),
      skeletonLines(said ? 4 : 6)];
  }

  const MARK_WORDS = { fine: "Looks right", flagged: "Flagged" };
  function reviewProgress(steps) {
    const looked = steps.filter((step) => markState(step)).length;
    const flagged = steps.filter((step) => markState(step) === "flagged").length;
    return { looked, flagged, total: steps.length };
  }

  /** The list a review walks: every changed entity under what it asks of the reader, numbered in the order it is read. */
  function reviewOverview(guide, steps, target, onCleanup) {
    if (!steps.length) {
      const changed = workChanged(guide);
      return h("div", { class: "empty" }, h("h3", {}, changed ? "This change is too large to read one entity at a time" : "No entity changed"),
        changed ? h("p", {}, `${plural(changed, "entity", "entities")} changed. Kin can still show it whole.`) : null,
        changed ? h("div", { class: "page-actions" }, link("Read it on one page", target.kind === "working" ? reviewURL("#/w") : target.kind === "range" ? reviewURL(`#/r/${encodeURIComponent(target.base)}..${encodeURIComponent(target.head)}`) : changeURL(target.id), true)) : null);
    }
    const GROUPS = [["Relied on, and not updated", "What relies on these needed a change and did not get one"], ["Relied on by code that did not change", "Check that what relies on these still holds"], ["No known follow-up", "No known connections need follow-up"]];
    const row = (step) => {
      const at = steps.indexOf(step);
      const mark = markState(step);
      return h("li", {}, h("a", { class: "table-row review-grid", href: `${reviewBase(target)}/${at + 1}` },
        h("span", { class: "cell cell-num" }, String(at + 1)),
        h("span", { class: "cell-main" }, ent({ name: step.name, kind: step.kind }, { linked: false })),
        h("span", { class: "cell" }, placeOf(step)),
        h("span", { class: "cell" }, rowReason(step, guide)),
        h("span", { class: `cell state${mark === "flagged" ? " state-warn" : mark === "fine" ? " state-good" : ""}` }, mark ? MARK_WORDS[mark] : "")));
    };
    const SHOWN = 200;
    const PICTURED = 8;
    const picture = h("div", { class: "picture-card" }, impactPicture(guide, onCleanup, { walk: reviewBase(target), shown: PICTURED, marks: true }));
    // The picture is the list while it can hold every entity; a longer change is listed in full under it.
    if (steps.length <= PICTURED) return picture;
    return [picture, GROUPS.map(([title, aside], rank) => {
      const rows = steps.filter((step) => stepAttention(step) === rank);
      if (!rows.length) return null;
      return h("section", { class: "section", "aria-label": title },
        h("div", { class: "section-head" }, h("h2", {}, title, h("span", { class: "count" }, count(rows.length))), h("span", { class: "aside" }, aside)),
        h("ul", { class: "table" }, rows.slice(0, SHOWN).map(row)),
        rows.length > SHOWN ? h("p", { class: "note quiet-line" }, `${count(rows.length - SHOWN)} more follow these when you read the review.`) : null);
    })];
  }

  function reviewHead() {
    const params = state.params;
    const target = params.target;
    if (!target) return null;
    const guide = reviewGuide();
    const working = target.kind === "working";
    // A range named from where you stand reads as the branch you are on, not as Git's word for it.
    const rangeSide = (name) => name === "HEAD" ? "the current branch" : name;
    const title = working ? "Uncommitted" : target.kind === "range" ? `${rangeSide(target.head).replace(/^the/, "The")} against ${rangeSide(target.base)}` : text(guide?.change?.title) || "Change";
    const crumbs = h("nav", { class: "crumbs", "aria-label": "Breadcrumb" },
      h("a", { href: working ? UNCOMMITTED : target.kind === "range" ? "#/branches" : "#/history" }, working ? "Uncommitted" : target.kind === "range" ? "Branches" : "History"),
      !working && (params.step || params.done) ? [h("span", { "aria-hidden": "true" }, "/"), h("a", { href: reviewBase(target) }, title)] : null);
    if (params.step || params.done) return [crumbs];
    const steps = guide ? reviewSteps(guide) : [];
    const sentence = guide ? workSentence(guide) : null;
    const { looked } = reviewProgress(steps);
    const next = steps.findIndex((step) => !markState(step));
    const author = guide && target.kind === "change" ? [authorOf(guide.change), ago(guide.change?.timestamp)].filter(Boolean).join(", ") : "";
    return [crumbs,
      h("div", { class: "page-title-row" }, h("h1", {}, title),
        h("div", { class: "page-actions" },
          steps.length ? link(looked === 0 ? "Start review" : next < 0 ? "Finish review" : "Continue review", next < 0 ? `${reviewBase(target)}/done` : `${reviewBase(target)}/${next + 1}`, true) : null)),
      sentence ? h("p", { class: "lede lede-strong" }, sentence) : guide ? h("p", { class: "lede" }, "No entity changed in this change.") : null,
      author ? h("p", { class: "note" }, author) : null,
      rereadLine(guide)];
  }

  function reviewStep(guide, steps, at, onCleanup) {
    const target = state.params.target;
    const step = steps[at];
    if (!step) return h("div", { class: "empty" }, h("h3", {}, "That entity is not in this review"),
      steps.length ? h("p", {}, `It holds ${plural(steps.length, "entity", "entities")} now.`) : null,
      h("div", { class: "page-actions" }, link("Back to the list", reviewList(target), true), steps.length ? link("Finish review", `${reviewBase(target)}/done`) : null));
    const base = reviewBase(target);
    const previous = at > 0 ? `${base}/${at}` : null;
    const next = at + 1 < steps.length ? `${base}/${at + 2}` : `${base}/done`;
    const mark = markState(step);
    const note = h("input", { id: "review-note", type: "text", maxlength: "400", placeholder: "Why you are flagging it (optional)", "aria-label": "A note for the flag", value: noteOf(step) });
    const decide = (kind) => { setMark(step, kind, note.value); shareMark(step, kind, note.value); location.hash = next; };
    return [
      h("div", { class: "review-bar" },
        h("span", { class: "review-position" }, `${count(at + 1)} of ${count(steps.length)}`),
        h("span", { class: "review-meter", "aria-hidden": "true" }, steps.length <= 60 ? steps.map((other, index) => h("span", { class: `review-tick${index === at ? " is-here" : ""}${markState(other) ? ` is-${markState(other)}` : ""}` })) : null),
        mark ? h("span", { class: `state${mark === "flagged" ? " state-warn" : " state-good"}` }, MARK_WORDS[mark]) : null),
      stepDetail(step, guide, onCleanup),
      isWorkingReview() || markState(step) === "flagged" ? (shareWords(step, true) ? h("p", { class: "note", id: "review-shared" }, shareWords(step, true)) : null) : null,
      restoreControls(step),
      h("div", { class: "review-decide" },
        h("div", { class: "field review-note" }, note),
        h("span", { class: "page-actions" },
          previous ? h("a", { class: "button button-quiet", href: previous, id: "review-previous" }, h("kbd", { "aria-hidden": "true" }, "←"), "Previous") : null,
          h("a", { class: "button button-quiet", href: next, id: "review-next" }, "Skip", h("kbd", { "aria-hidden": "true" }, "→")),
          button(["Flag it", h("kbd", { "aria-hidden": "true" }, "F")], () => decide("flagged"), { id: "review-flag", "aria-keyshortcuts": "f" }),
          button(["Looks right", h("kbd", { "aria-hidden": "true" }, "↵")], () => decide("fine"), { id: "review-fine", class: "button button-primary", "aria-keyshortcuts": "Enter" })))];
  }

  async function recordDecision(value) {
    const review = state.review;
    const decision = review.decision;
    if (decision.busy || isWorkingReview() || !review.guide) return;
    decision.busy = true; decision.error = null;
    paint("review-body");
    try {
      const about = review.target.kind === "range" ? { change: review.target.head, base: review.target.base, range: true } : { change: review.target.id, title: text(review.guide.change?.title) || undefined };
      await send("/api/decide", { ...about, state: value, comment: decision.comment.trim() || undefined });
      decision.done = value;
      // A view that the change needs work reaches its agent through the flags on its entities,
      // so any flag this page has not shared yet, such as one marked before a reload, is shared now.
      if (value === "needs_work" && can("review_flags") && globalThis.crypto?.randomUUID) {
        for (const step of reviewSteps(review.guide)) if (markState(step) === "flagged" && !shareOf(step) && !flagFor(step)) shareRecordedMark(step, "flagged", noteOf(step));
      }
      announce("Saved.");
      refresh(["standing"]);
    } catch (error) { decision.error = error.message || "Your view was not saved."; }
    decision.busy = false;
    paint("review-body");
  }

  /** What was flagged, as plain text to hand to whoever, or whatever, made the change. */
  function flagsText(guide, steps) {
    const flagged = steps.filter((step) => markState(step) === "flagged");
    const line = (step) => `${step.name} (${placeOf(step)}): ${noteOf(step) ? `${noteOf(step)} ` : ""}${changeSentence(step)} ${stepReason(step, guide)}.`;
    // Flags go back to whoever made the change, so they are grouped by the agent Kin saw write each one.
    const groups = new Map();
    for (const step of flagged) {
      const who = [...new Set(authorsOf(step).map((held) => held.name))].join(" and ") || (knowsAuthors(guide) ? "No agent reported this edit" : "");
      if (!groups.has(who)) groups.set(who, []);
      groups.get(who).push(line(step));
    }
    if (groups.size === 1 && groups.has("")) return groups.get("").join("\n");
    return [...groups].map(([who, lines]) => `${who || "Unknown"}:\n${lines.join("\n")}`).join("\n\n");
  }

  function reviewDone(guide, steps) {
    const target = state.params.target;
    const { looked, flagged, total } = reviewProgress(steps);
    const fine = looked - flagged;
    const flaggedSteps = steps.filter((step) => markState(step) === "flagged");
    const unlooked = total - looked;
    const working = target.kind === "working";
    const head = h("header", { class: "page-head" },
      h("div", { class: "page-title-row" }, h("h1", {}, !total ? "Nothing to look at here" : unlooked ? `${count(looked)} of ${count(total)} marked` : flagged ? (fine ? `${spoken(fine)} ${fine === 1 ? "looks" : "look"} right, ${spoken(flagged).toLowerCase()} flagged` : total === 1 ? "Flagged" : flagged === total ? `${total === 2 ? "Both" : `All ${spoken(total).toLowerCase()}`} flagged` : `${spoken(flagged)} flagged`)
          : total === 1 ? "It looks right" : total === 2 ? "Both look right" : `All ${spoken(total).toLowerCase()} look right`)),
      unlooked ? h("p", { class: "lede" }, `${plural(unlooked, "entity", "entities")} left unmarked.`) : null);
    const status = h("span", { class: "note", role: "status" });
    const flaggedList = flaggedSteps.length ? h("section", { class: "section", "aria-label": "Flagged" },
      h("div", { class: "section-head" }, h("h2", {}, "Flagged", h("span", { class: "count" }, count(flaggedSteps.length))),
        h("span", { class: "page-actions" }, status, button("Copy the flags", async () => {
          try { await navigator.clipboard.writeText(flagsText(guide, steps)); status.textContent = "Copied. Paste it to the agent or person who made the change."; }
          catch (_) { status.textContent = "This browser would not copy it."; }
        }, { class: "button button-small" }))),
      h("ul", { class: "table" }, flaggedSteps.map((step) => h("li", {}, h("a", { class: "table-row review-grid", href: `${reviewBase(target)}/${steps.indexOf(step) + 1}` },
        h("span", { class: "cell cell-num" }, String(steps.indexOf(step) + 1)), h("span", { class: "cell-main" }, ent({ name: step.name, kind: step.kind }, { linked: false })),
        h("span", { class: "cell" }, placeOf(step)), h("span", { class: "cell" }, noteOf(step) || rowReason(step, guide)), h("span", { class: "cell state state-warn", title: shareWords(step, true) || undefined }, shareWords(step) || "Flagged"))))),
      // Why a flag on a recorded change stayed here, said in full, since the row holds only a few words.
      working ? null : flaggedSteps.filter((step) => shareOf(step) && !["shared", "sending"].includes(shareOf(step).state)).map((step) => h("p", { class: "note quiet-line" }, shareWords(step, true)))) : null;
    if (working && !total) {
      return [state.commit.result ? commitBox() : null, h("div", { class: "empty" }, h("h3", {}, "Nothing is uncommitted"), h("div", { class: "page-actions" }, link("Open History", "#/history")))];
    }
    if (working) {
      // A commit made here is said here, above what it left, until it is dismissed.
      const receipt = state.commit.result ? [commitBox(), read("working").stale ? h("p", { class: "note aside-reading" }, "Reading what is still uncommitted") : null] : null;
      if (receipt && !unlooked) return [receipt, head, flaggedList];
      if (unlooked && !state.commit.open) {
        return [receipt, head, flaggedList, h("div", { class: "page-actions section" },
          link("Continue review", `${reviewBase(target)}/${steps.findIndex((step) => !markState(step)) + 1}`, true),
          canCommit() ? button("Commit anyway", () => { prepareCommitDraft(steps); state.commit.open = true; paint("review-body"); byId("commit-message")?.focus(); }, { class: "button button-quiet" }) : null)];
      }
      // Committing is how uncommitted work is accepted, so the form is here at the end of reading it.
      if (canCommit()) {
        // The message starts as what the commit holds, for the reader to say why in their own words,
        // and Kin's draft follows what the commit holds until the reader writes their own.
        prepareCommitDraft(steps);
        state.commit.open = true;
      }
      return [head, flaggedList,
        h("section", { class: "section" }, canCommit() ? commitBox() : h("p", { class: "gap" }, "This version of Kin can't commit from this page."))];
    }
    const decision = state.review.decision;
    const standing = text(read("standing").data?.standing?.[target.id]?.state);
    const recorded = decision.done || (DECISION[standing] && standing !== "pending" ? standing : "");
    const comment = h("textarea", { id: "review-comment", rows: "2", maxlength: "4000", placeholder: "A note with your view (optional)", "aria-label": "A note with your view", disabled: decision.busy,
      on: { input: (event) => { decision.comment = event.target.value; } } }, decision.comment);
    const choose = (value, label, look = "") => button(decision.busy ? "Saving…" : label, () => recordDecision(value), { class: `button ${look}`, disabled: decision.busy || !session()?.token });
    return [head, flaggedList,
      h("section", { class: "section review-finish", "aria-label": "Your view of this change" },
        h("div", { class: "section-head" }, h("h2", {}, "Your view of this change"), h("span", { class: "aside" }, "Your own record of how it reads. Saving it blocks nothing and changes no code.")),
        recorded ? h("p", { class: "note" }, `Your view now: ${(DECISION[recorded] || DECISION[recorded.replace("_", "")] || ["Saved"])[0].toLowerCase()}. A new one replaces it.`) : null,
        can("review_flags") ? h("p", { class: "note", id: "review-reach" }, flaggedSteps.length
          ? "An agent is told about each flag above that was shared, the next time it touches that entity."
          : "No entity is flagged, so no agent is told what to change. Flag an entity to tell the agent that made it.") : null,
        decision.error ? h("div", { class: "message message-bad", role: "alert" }, h("div", {}, h("strong", {}, "Your view was not saved"), h("p", {}, decision.error))) : null,
        comment,
        h("div", { class: "page-actions" }, choose("needs_work", "Flag it"), choose("approved", "Looks right", "button-primary")))];
  }

  function reviewBody(onCleanup) { return [state.restore.unknown ? restoreRecovery() : isWorkingReview() ? restoreReceipt() : null, reviewContents(onCleanup)]; }
  function reviewContents(onCleanup) {
    const params = state.params;
    if (!params.target) return null;
    const review = state.review;
    const guide = reviewGuide();
    if (!guide) {
      if (params.target.kind === "working") return read("working").error ? readFailure("working") : skeletonLines(6);
      // Still reading, or reading again by itself after Kin could not settle it: the page says which and how far it has got.
      if (review.busy || review.retryAt) return reviewWaiting();
      if (review.error) return failed(review.target?.kind === "range" ? "Kin couldn't compare these branches." : "Kin couldn't load this change.", () => { review.guide = null; review.attempt = 0; loadReview(params.target); }, review.error);
      return skeletonLines(6);
    }
    const steps = reviewSteps(guide);
    const flagError = review.flagError ? h("div", { class: "message message-warn section-tight", role: "alert" }, h("div", {}, h("strong", {}, "Kin could not save the flag for agents"), h("p", {}, review.flagError), h("p", {}, "Your review mark is still kept in this browser. Read the entity again and flag it to try saving it.")), button("Dismiss", () => { review.flagError = null; paint("review-body"); }, { class: "button button-small" })) : null;
    if (params.done) return [flagError, reviewDone(guide, steps)];
    if (params.step) return [flagError, reviewStep(guide, steps, params.step - 1, onCleanup)];
    return [flagError, reviewOverview(guide, steps, params.target, onCleanup)];
  }

  function review() {
    return [
      section("review-head", ["working", "changes", "standing", "branches"], reviewHead, { class: "page-head" }),
      section("review-body", ["working", "changes", "standing", "flags", "session"], reviewBody)];
  }

  // ── Changes ──────────────────────────────────────────────────────────────

  const DECISION = { approved: ["Looks right", "good"], needswork: ["Flagged", "warn"], needs_work: ["Flagged", "warn"], blocked: ["Blocked", "bad"], pending: ["Not reviewed", ""] };
  function decisionOf(change) {
    const resource = read("standing");
    if (!resource.data) return null;
    const value = resource.data.standing?.[change.id]?.state || "pending";
    return DECISION[value] ? value : "pending";
  }
  /** Whether any loaded change carries a review decision; until one does, the column stays empty. */
  function anyDecision() {
    const standing = read("standing").data?.standing;
    return Boolean(standing) && Object.values(standing).some((row) => row?.state && row.state !== "pending");
  }
  function decisionPill(change) {
    const value = decisionOf(change);
    if (!value || (value === "pending" && !anyDecision())) return null;
    const [label, tone] = DECISION[value];
    return h("span", { class: `state${tone ? ` state-${tone}` : ""}` }, label);
  }

  /**
   * How many entities a change changed, as its own guided review counts
   * them, merges included. Null until that count is read, so a row never
   * shows a number its review would contradict.
   */
  function changedCount(change) {
    const counted = state.counts.get(change?.id);
    if (!counted || !isCount(counted.entities) || (counted.merge && !counted.merge_counted)) return null;
    return counted.entities;
  }

  /**
   * What a committed change left behind, read from its own guided review: the
   * newest few are read one after another while History is open, the rest when
   * opened. A change too large to read quickly is left to its review.
   */
  const consequences = new Map();
  let consequenceBusy = false;
  async function loadConsequences() {
    if (consequenceBusy || MACHINE || state.route !== "history") return;
    const wanted = filteredChanges().filter((change) => !isKinUpdate(change) && /^[\w-]{1,128}$/.test(text(change.id)))
      .filter((change) => { const n = changedCount(change); return isCount(n) && n > 0 && n <= 200; }).slice(0, 6)
      .filter((change) => !consequences.has(change.id));
    if (!wanted.length) return;
    consequenceBusy = true;
    const epoch = state.readEpoch;
    try {
      for (const change of wanted) {
        if (state.route !== "history" || epoch !== state.readEpoch) break;
        try {
          const guide = await fetchJSON(`/api/guide?change=${encodeURIComponent(change.id)}`);
          consequences.set(change.id, guide?.summary || null);
        } catch (_) { consequences.set(change.id, null); }
        paint("changes-results");
      }
    } finally { consequenceBusy = false; }
  }
  /** One line on what a change left for others: who relies on it and did not change with it. */
  function consequenceLine(change) {
    const summary = consequences.get(change.id);
    if (!summary) return null;
    const outside = isCount(summary.recorded_consumers) ? summary.recorded_consumers : 0;
    const behind = isCount(summary.consumers_not_updated) ? summary.consumers_not_updated : 0;
    const check = isCount(summary.consumers_may_need_review) ? summary.consumers_may_need_review : 0;
    const tests = isCount(summary.recorded_tests) ? summary.recorded_tests : 0;
    const tone = behind ? "bad" : check ? "warn" : "good";
    const limited = limitedGuide({ summary });
    const words = !outside ? "Kin found no connections outside this change"
      : behind ? `${plural(behind, "entity", "entities")} that ${behind === 1 ? "relies" : "rely"} on it ${behind === 1 ? "was" : "were"} not updated`
      : check ? `${plural(check, "entity", "entities")} ${check === 1 ? "relies" : "rely"} on it and did not change`
      : `${limited ? "At least " : ""}${plural(outside, "entity", "entities")} ${outside === 1 ? "relies" : "rely"} on it and ${outside === 1 ? "needs" : "need"} no update`;
    return h("span", { class: `change-consequence change-consequence-${tone}` }, h("span", { class: "impact-dot", "aria-hidden": "true" }), `${words}. ${tests ? `${graphCount(tests, limited).replace(/^at/, "At")} ${tests === 1 ? "test reaches" : "tests reach"} it${withinSteps(summary.tests_depth)}.` : noTestWords(isCount(summary.tests_depth) && summary.tests_depth > 0 ? summary.tests_depth : null)}`);
  }

  function changeRow(change) {
    const author = authorOf(change);
    const when = ago(change.timestamp);
    const kin = isKinUpdate(change);
    const n = changedCount(change);
    const listed = array(read("changes").data?.changes);
    const partial = text(session()?.history?.state) === "partial";
    // The oldest change of a store that has not read its history yet holds the whole repository as Kin first read it.
    const start = partial && !read("changes").data?.truncated && listed.length && listed[listed.length - 1]?.id === change.id;
    const entities = kin || start ? null : isCount(n) ? (n ? `${plural(n, "entity", "entities")} changed` : "No entity changed") : null;
    const quiet = kin || n === 0;
    return h("li", {}, h("a", { class: `change-row${quiet ? " change-row-quiet" : ""}`, href: reviewHref(change.id) },
      h("div", {}, h("span", { class: "change-title" }, change.title || "Untitled change"),
        consequenceLine(change),
        h("span", { class: "change-meta" }, agentsApi() ? agentsApi().author(agentsCtx, change, author) : author ? h("span", {}, author) : null, when ? h("time", { datetime: change.timestamp || undefined }, when) : null, entities ? h("span", {}, entities) : null),
        start ? h("span", { class: "change-start" }, "Kin's record starts here, with the repository as it was") : null),
      h("span", { class: "change-side" }, kin ? h("span", { class: "state" }, "Kin update") : decisionPill(change))));
  }

  function changeList(changes, { compact = false } = {}) {
    const resource = read("changes");
    if (!resource.data) {
      if (resource.error) return readFailure("changes");
      return h("ul", { class: "list list-card", "aria-busy": "true" }, Array.from({ length: compact ? 4 : 8 }, () => h("li", { class: "change-row" }, h("div", {}, skeleton("skeleton-line")))));
    }
    if (!changes.length) {
      return h("p", { class: "gap" }, array(resource.data.changes).length ? "No change matches." : "No changes yet.");
    }
    if (compact) return h("ul", { class: "list list-card" }, changes.map(changeRow));
    const truncated = resource.data.truncated;
    return h("div", { class: "list-card" }, h("ul", { class: "list" }, changes.map(changeRow)),
      resource.error || (truncated && state.requested < 500) ? h("div", { class: "list-foot" }, h("span", {}, resource.error ? "Showing what Kin last saw." : `${plural(changes.length, "change")} shown`),
        truncated && state.requested < 500 ? button("Show older changes", () => { state.requested = Math.min(500, state.requested * 2); refresh(["changes"]); }, { class: "button button-small", disabled: state.refreshing }) : null) : null);
  }

  function filteredChanges() {
    const query = state.reviewQuery.toLowerCase().trim();
    return array(read("changes").data?.changes).filter((change) => {
      if (query && !`${change.title} ${change.id} ${authorOf(change)} ${change.agent?.name || ""}`.toLowerCase().includes(query)) return false;
      if (state.authorFilter === "people" && (change.agent || isKinUpdate(change))) return false;
      if (state.authorFilter.startsWith("agent:") && change.agent?.name !== state.authorFilter.slice(6)) return false;
      if (state.reviewFilter === "all") return true;
      const value = decisionOf(change);
      if (!value) return false;
      return state.reviewFilter === "pending" ? value === "pending" : state.reviewFilter === "needswork" ? ["needswork", "needs_work", "blocked"].includes(value) : value === state.reviewFilter;
    });
  }

  /** A line that says the record is still filling in, while Kin reads a converted project's history. */
  function historyNote() {
    const history = session()?.history;
    if (!history || text(history.state) !== "partial") return null;
    const counted = isCount(history.admitted) && isCount(history.total) && history.total > 0;
    return h("p", { class: "work-caveat" }, counted
      ? `Kin has read ${count(history.admitted)} of ${plural(history.total, "commit")} from this repository's history. This list includes the history available so far.`
      : "This repository's imported history is incomplete. This list includes the history available so far.");
  }

  function changesSentence() {
    const changes = array(read("changes").data?.changes);
    if (!read("changes").data) return null;
    const head = read("changes").data.truncated ? `${count(changes.length)} most recent changes.` : `${spoken(changes.length)} ${changes.length === 1 ? "change" : "changes"}.`;
    const standing = read("standing").data?.standing;
    if (!standing || !changes.length) return head;
    const tally = { approved: 0, needs: 0 };
    for (const change of changes) {
      const value = standing[change.id]?.state;
      if (value === "approved") tally.approved++;
      else if (["needswork", "needs_work", "blocked"].includes(value)) tally.needs++;
    }
    if (!tally.approved && !tally.needs) return head;
    const parts = [];
    if (tally.approved) parts.push(`${spoken(tally.approved)} ${tally.approved === 1 ? "looks" : "look"} right`);
    if (tally.needs) parts.push(`${spoken(tally.needs).toLowerCase()} ${tally.needs === 1 ? "is" : "are"} flagged`);
    return `${head} ${parts.join(", ").replace(/^\w/, (c) => c.toUpperCase())}.`;
  }

  function compareForm() {
    const branches = branchRows();
    const options = h("datalist", { id: "workspace-branch-options" }, branches.map((branch) => h("option", { value: branch.name })));
    const base = h("input", { id: "workspace-range-base", name: "base", list: "workspace-branch-options", placeholder: "main", required: true, value: state.rangeBase ?? defaultBranch()?.name ?? "", autocomplete: "off", spellcheck: "false", on: { input: (event) => { state.rangeBase = event.target.value; } } });
    const head = h("input", { id: "workspace-range-head", name: "head", list: "workspace-branch-options", placeholder: "The current branch", value: state.rangeHead, autocomplete: "off", spellcheck: "false", on: { input: (event) => { state.rangeHead = event.target.value; } } });
    return h("form", { class: "compare", "aria-label": "Compare branches", on: { submit: (event) => { event.preventDefault(); const from = base.value.trim(); const to = head.value.trim(); if (from) location.hash = rangeHref(from, !to || to === "HEAD" ? (activeBranch()?.name || "HEAD") : to); } } },
      h("div", { class: "field" }, h("label", { for: "workspace-range-base" }, "From"), base),
      h("span", { class: "compare-arrow", "aria-hidden": "true" }, "→"),
      h("div", { class: "field" }, h("label", { for: "workspace-range-head" }, "To"), head),
      h("button", { class: "button", type: "submit" }, "Compare"), options);
  }

  function updateReviewResults() { paint("changes-results"); }

  /** Who made the loaded changes: everyone, people, or one agent. Offered once an agent made one. */
  function authorFilter() {
    const changes = array(read("changes").data?.changes);
    const agents = [...new Set(changes.map((change) => text(change.agent?.name)).filter(Boolean))].slice(0, 6);
    if (!agents.length) { state.authorFilter = "all"; return null; }
    if (state.authorFilter.startsWith("agent:") && !agents.includes(state.authorFilter.slice(6))) state.authorFilter = "all";
    const madeBy = (name) => changes.filter((change) => change.agent?.name === name).length;
    const options = [["all", "Everyone", null], ["people", "People", null], ...agents.map((name) => [`agent:${name}`, name, madeBy(name)])];
    return h("div", { class: "segmented segmented-authors", role: "group", "aria-label": "Show by who made the change", id: "workspace-author-filter" },
      options.map(([value, label, made]) => h("button", { type: "button", "aria-pressed": String(state.authorFilter === value), on: { click: () => { state.authorFilter = value; paint("changes-authors"); updateReviewResults(); } } },
        value.startsWith("agent:") && agentsApi() ? agentsApi().mark(12) : null, label, made ? h("span", { class: "segmented-count" }, count(made)) : null)));
  }

  function history() {
    const query = h("input", { id: "workspace-review-search", type: "search", placeholder: "Find a change by title, author or ID", "aria-label": "Find a change", value: state.reviewQuery, autocomplete: "off", on: { input: (event) => { state.reviewQuery = event.target.value; updateReviewResults(); } } });
    const FILTERS = [["all", "All"], ["pending", "Not reviewed"], ["needswork", "Flagged"], ["approved", "Looks right"]];
    return [
      workHead(null),
      section("changes-lede", ["changes", "standing", "session"], () => { const sentence = changesSentence(); return sentence ? [h("p", { class: "lede" }, sentence), historyNote()] : skeleton("skeleton-lede"); }, { class: "tab-lede" }),
      h("div", { class: "toolbar" },
        h("div", { class: "field" }, query),
        h("span", { class: "toolbar-gap" }),
        section("changes-filter", ["standing"], () => {
          if ((!read("standing").data || !anyDecision()) && state.reviewFilter !== "all") state.reviewFilter = "all";
          if (!anyDecision()) return null;
          return h("div", { class: "segmented", role: "group", "aria-label": "Show by your view of each change", id: "workspace-review-filter" },
            FILTERS.map(([value, label]) => h("button", { type: "button", "aria-pressed": String(state.reviewFilter === value), disabled: value !== "all" && !read("standing").data, on: { click: () => { state.reviewFilter = value; paint("changes-filter"); updateReviewResults(); } } }, label)));
        })),
      section("changes-authors", ["changes"], authorFilter),
      section("changes-results", ["changes", "standing"], () => { setTimeout(loadConsequences, 0); return changeList(filteredChanges()); }, { "aria-live": "polite" })];
  }

  // ── Explore ──────────────────────────────────────────────────────────────

  function examplePlaceholder() {
    const top = topEntities()[0];
    return top ? `Ask about this code, like “${top.name}”` : "Ask about this code";
  }

  function explore() {
    const input = h("input", { id: "explore-input", class: "ask-input", type: "search", name: "q", autocomplete: "off", spellcheck: "false", maxlength: "256",
      placeholder: examplePlaceholder(), "aria-label": "Ask about this code by meaning or by name", "aria-controls": "explore-results", value: state.explore.query,
      on: { input: (event) => queueAsk(event.target.value), keydown: exploreKeys } });
    return [
      h("header", { class: "page-head" },
        h("div", { class: "page-title-row" }, h("h1", {}, "Code")),
        section("explore-lede", ["overview"], () => {
          byId("explore-input")?.setAttribute("placeholder", examplePlaceholder());
          const overview = read("overview").data;
          const counts = section_ok(overview?.counts) ? overview.counts : null;
          return h("p", { class: "lede" }, counts && isCount(counts.entities)
            ? `${plural(counts.entities, "entity", "entities")} and ${connectionsAtLeast(overview) ? "at least " : ""}${plural(counts.relationships || 0, "connection")}. Ask by meaning or by name, then open an entity to see what relies on it.`
            : "Ask by meaning or by name, then open an entity to see what relies on it.");
        })),
      h("form", { class: "ask", role: "search", on: { submit: (event) => { event.preventDefault(); runAsk(input.value, true); } } },
        h("div", { class: "ask-box" }, icon("search"), input, h("kbd", { "aria-hidden": "true" }, "/"))),
      h("p", { class: "ask-hint" }, h("span", {}, h("kbd", {}, "↑"), h("kbd", {}, "↓"), "move"), h("span", {}, h("kbd", {}, "Enter"), "open"), h("span", {}, h("kbd", {}, "Esc"), "clear")),
      section("explore-results", ["overview"], exploreResults, { class: "results", role: "region", "aria-label": "Results", "aria-live": "polite" })];
  }

  function entityRow(row, most = 0, base = "", atLeast = false) {
    const where = containerOf(row);
    const n = reliedOn(row);
    const signature = row.overloads > 1 ? "" : text(row.signature);
    const summary = text(row.summary);
    const detail = summary && (!signature || signature.length < 28) ? summary : signature;
    const how = "";
    // A module is relied on through what it holds, so it carries no count of its own.
    const counted = isCount(n) && n > 0 && text(row.kind).toLowerCase() !== "module";
    return h("li", {}, h("a", { class: "entity-row", href: `${base}${entityHref(row.id)}`, "data-entity": row.id, title: text(row.why) || undefined },
      h("div", { class: "entity-main" },
        h("div", { class: "entity-line" }, ent(row, { linked: false }), row.overloads > 1 ? h("span", { class: "entity-overloads" }, `${count(row.overloads)} overloads`) : null,
          h("span", { class: "entity-container" }, [kindWord(kindOf(row)), where && !row.name.startsWith(`${where}.`) ? `in ${where}` : "", row.outer ? `of ${row.outer}` : ""].filter(Boolean).join(" ")),
          row.alike > 0 ? h("span", { class: "entity-overloads" }, `and ${count(row.alike)} more of this name`) : null),
        detail ? h("div", { class: detail === signature ? "entity-signature" : "entity-why" }, detail) : null),
      h("span", { class: "entity-side" }, counted ? relyWords(n, atLeast) : how)));
  }

  /** What the repository holds at its top level, as rows into each container's own page. */
  function structureList() {
    const overview = read("overview").data;
    const held = section_ok(overview?.containers) ? array(overview.containers.items).filter((row) => row && UUID.test(row.id) && !row.container) : [];
    const languages = section_ok(overview?.languages) ? array(overview.languages.items).filter((row) => row && typeof row.language === "string") : [];
    // What many entities rely on and no recorded test reaches, directly or through what it uses: where a change is most likely to break something unseen.
    const untested = section_ok(overview?.untested) ? array(overview.untested.items).filter((row) => row && UUID.test(row.id) && typeof row.name === "string") : [];
    if (!held.length && !languages.length && !untested.length) return null;
    return [
      untested.length ? h("div", { class: "result-group" },
        h("div", { class: "result-head" }, h("h2", {}, "Relied on, with no tests found that reach them", isCount(overview.untested.total) ? h("span", { class: "count" }, count(overview.untested.total)) : null)),
        h("ul", { class: "list list-card" }, untested.slice(0, 10).map((row) => h("li", {}, h("a", { class: "structure-row", href: entityHref(row.id) },
          h("span", { class: "structure-name" }, ent(row, { linked: false }), h("span", { class: "entity-container" }, kindWord(row.kind))),
          h("span", { class: "entity-side state-warn" }, isCount(reliedOn(row)) ? relyWords(reliedOn(row), rankingAtLeast(overview)) : "")))))) : null,
      held.length && !untested.length ? h("div", { class: "result-group" },
        h("div", { class: "result-head" }, h("h2", {}, "Largest containers")),
        h("ul", { class: "list list-card" }, held.slice(0, 10).map((row, _, shown) => h("li", {}, h("a", { class: "structure-row", href: entityHref(row.id) },
          // Two containers of one name are told apart by what they are written in.
          h("span", { class: "structure-name" }, ent(row, { linked: false }),
            text(row.language) && shown.filter((other) => other.name === row.name).length > 1 ? h("span", { class: "entity-container" }, languageWord(row.language)) : null),
          h("span", { class: "entity-side" }, plural(row.entities || 0, "entity", "entities"))))))) : null,
      languages.length ? h("div", { class: "result-group" },
        h("div", { class: "result-head" }, h("h2", {}, "Languages")),
        h("ul", { class: "list list-card" }, languages.slice(0, 8).map((row) => h("li", { class: "lang-row" }, h("span", {}, languageWord(row.language)), h("span", { class: "entity-side" }, plural(row.entities || 0, "entity", "entities")))))) : null];
  }

  function exploreResults() {
    const ex = state.explore;
    if (!ex.query.trim()) {
      const rows = topEntities();
      const status = topState();
      if (!rows.length) {
        if (status === "error") return readFailure("overview");
        if (status === "reading") return skeletonLines(6);
        return [h("p", { class: "gap" }, "Kin found no connections between entities yet. Ask about the code above."), structureList()];
      }
      const most = Math.max(...rows.map((row) => reliedOn(row) || 0));
      return h("div", { class: "explore-grid" },
        h("div", { class: "result-group" }, h("div", { class: "result-head" }, h("h2", {}, "Most relied on")),
          h("ul", { class: "list list-card", id: "explore-list" }, rows.slice(0, 12).map((row) => entityRow(row, most, "", rankingAtLeast(read("overview").data))))),
        h("div", {}, structureList()));
    }
    if (ex.error) return failed(isSlow(ex.error) ? "Kin is still working on that search. Try again in a moment." : "Kin couldn't run that search.", () => runAsk(ex.query, true), ex.error);
    if (!ex.results) return skeletonLines(5);
    const groups = ex.results.groups.filter((group) => group.rows.length);
    if (!groups.length) return h("p", { class: "gap" }, `Nothing matched “${ex.query.trim()}”. Try a name, or describe what the code does.`);
    return [answerChain(ex.results.answer), ex.results.notice ? h("p", { class: "note" }, ex.results.notice) : null, ...groups.map((group) => h("div", { class: "result-group" },
      h("div", { class: "result-head" }, h("h2", {}, group.title), h("span", { class: "aside" }, (() => {
        // A folded row stands for each of its overloads.
        const shown = group.rows.reduce((sum, row) => sum + (row.overloads || 1), 0);
        return group.total > shown ? `${count(shown)} of ${count(group.total)}` : group.top ? `The closest ${count(shown)}` : plural(shown, "entity", "entities");
      })())),
      h("ul", { class: "list list-card" }, group.rows.map((row) => entityRow(row)))))];
  }

  function queueAsk(value) {
    state.explore.query = value;
    clearTimeout(state.exploreTimer);
    state.exploreTimer = setTimeout(() => runAsk(value, false), 260);
    try { globalThis.history.replaceState(null, "", value.trim() ? `#/explore?q=${encodeURIComponent(value)}` : "#/explore"); } catch (_) { /* The query still runs. */ }
  }

  /**
   * Rows for declarations of one name and kind in one place, such as the
   * overloads of one callable, read as one row that counts them.
   */
  function foldOverloads(list) {
    const at = new Map();
    const out = [];
    for (const row of list) {
      const place = row.container && typeof row.container === "object" ? text(row.container.id) : text(row.module);
      const key = `${row.name}\u0000${text(row.kind)}\u0000${place}`;
      const held = at.get(key);
      if (held) { held.overloads = (held.overloads || 1) + 1; continue; }
      const kept = { ...row };
      at.set(key, kept);
      out.push(kept);
    }
    // Two different entities that read the same, name and place alike, are told apart by where their container sits.
    const reads = new Map();
    const label = (row) => `${row.name}\u0000${text(row.kind)}\u0000${containerOf(row)}`;
    for (const row of out) reads.set(label(row), (reads.get(label(row)) || 0) + 1);
    for (const row of out) if (reads.get(label(row)) > 1 && text(row.container?.within?.name)) row.outer = row.container.within.name;
    return out;
  }

  /** An answer as result groups. The ask read is already ranked, names and meaning together. */
  function askGroups(data, limit = null) {
    const list = foldOverloads(array(data?.results).filter((row) => row && UUID.test(row.id) && typeof row.name === "string"));
    if (!Array.isArray(data?.results)) return { groups: [], notice: "", answer: null };
    const title = data.mode === "name_only" ? "By name" : data.mode === "meaning_only" ? "By meaning" : "Best matches";
    const chain = array(data.answer?.steps).filter((step) => step && UUID.test(text(step.id)) && typeof step.name === "string");
    // How many matched is known only for names; a ranking by meaning has no end, so a full page is its closest few.
    const named = data.mode === "name_only" && isCount(data.names?.total) ? data.names.total : null;
    const full = isCount(limit) && array(data.results).length >= limit;
    return { groups: [{ title, rows: list, total: named !== null ? Math.max(named, list.length) : list.length, top: named === null && full }],
      notice: data.mode && data.mode !== "meaning_and_name" ? text(data.notice) : "",
      // One of the two searches did not answer, so finding nothing proves nothing.
      partial: Boolean(data.mode) && data.mode !== "meaning_and_name",
      // An answer is a chain Kin traced through the graph, never a guess: at least two entities joined by how one reaches the next.
      answer: chain.length >= 2 ? { sentence: text(data.answer.sentence), chain } : null };
  }
  /** A question's answer as Kin traced it: the entities the work passes through, in order, each joined to the next by how it gets there. */
  function answerChain(answer, base = "") {
    if (!answer) return null;
    return h("div", { class: "answer", role: "group", "aria-label": "How it happens" },
      answer.sentence ? h("p", { class: "answer-sentence" }, answer.sentence) : null,
      h("ol", { class: "answer-chain" }, answer.chain.map((step, at) => h("li", { class: "answer-step" },
        h("a", { class: "impact-node answer-node", href: `${base}${entityHref(step.id)}`, title: step.name },
          h("span", { class: "impact-text" }, h("span", { class: "impact-name" }, shownName(step.name)), h("span", { class: "impact-place" }, kindWord(step.kind)))),
        at < answer.chain.length - 1 ? h("span", { class: "answer-link" }, RELATION_NEXT[text(step.relation_to_next).toLowerCase()] || "leads to") : null))));
  }
  const RELATION_NEXT = { calls: "calls", references: "refers to", instantiates: "constructs", imports: "imports", uses_type: "uses", extends: "extends", implements: "implements", overrides: "overrides", contains: "holds", returns: "returns" };

  async function runAsk(value, immediate) {
    const ex = state.explore;
    const query = String(value || "").trim();
    ex.query = String(value || "");
    clearTimeout(state.exploreTimer);
    const mine = ++ex.request;
    if (!query) { ex.results = null; ex.error = null; ex.active = -1; paint("explore-results"); return; }
    ex.busy = true;
    if (immediate || !ex.results) { ex.results = null; ex.error = null; paint("explore-results"); }
    try {
      const data = await fetchJSON(`/api/workspace/ask?q=${encodeURIComponent(query)}&limit=20`);
      if (mine !== ex.request) return;
      ex.results = askGroups(data, 20);
      ex.error = null;
      ex.active = -1;
      const total = ex.results.groups.reduce((sum, group) => sum + group.rows.length, 0);
      announce(total ? `${plural(total, "result")}.` : "No results.");
    } catch (error) {
      if (mine !== ex.request) return;
      ex.error = error.message || "The search failed.";
    } finally {
      if (mine === ex.request) { ex.busy = false; paint("explore-results"); }
    }
  }

  function exploreKeys(event) {
    const rows = [...(byId("explore-results")?.querySelectorAll(".entity-row") || [])];
    if (event.key === "Escape") { event.preventDefault(); event.target.value = ""; queueAsk(""); runAsk("", true); return; }
    if (!rows.length || !["ArrowDown", "ArrowUp", "Enter"].includes(event.key)) return;
    const ex = state.explore;
    if (event.key === "Enter") {
      if (ex.active >= 0 && rows[ex.active]) { event.preventDefault(); location.hash = rows[ex.active].getAttribute("href"); }
      return;
    }
    event.preventDefault();
    ex.active = Math.max(0, Math.min(rows.length - 1, ex.active + (event.key === "ArrowDown" ? 1 : -1)));
    rows.forEach((row, i) => row.setAttribute("aria-selected", String(i === ex.active)));
    rows[ex.active].scrollIntoView?.({ block: "nearest" });
  }

  // ── Entity ───────────────────────────────────────────────────────────────

  function isTest(row) {
    if (!row) return false;
    if (row.test === true || row.is_test === true || text(row.role) === "test" || text(row.kind).toLowerCase() === "test") return true;
    if (row.is_test === false) return false;
    if (["tests", "covers"].includes(text(row.relation).toLowerCase())) return true;
    const name = text(row.name);
    const last = name.slice(name.lastIndexOf(".") + 1);
    return /^test_|^Test[A-Z_]/.test(name) || /^test_/.test(last);
  }

  const listOf = (value) => array(value?.items).filter((row) => row && UUID.test(row.id));
  const reasonOf = (value, fallback) => text(value?.reason) || fallback;

  /**
   * The entity read, laid out for the page. Each part keeps its own `ok` and
   * `reason`, so one failed part never blanks the rest.
   */
  function entityView(response, pending = new Set()) {
    const id = response.id;
    const waiting = (key) => !(key in response) && pending.has(key);
    const header = section_ok(response.header) ? response.header : {};
    const source = response.source || {};
    const reliers = response.relied_on_by || {};
    const uses = response.relies_on || {};
    const tests = response.tests || {};
    const impact = response.impact || null;
    const history = response.history || null;
    const editor = response.editor || {};
    const dedupe = (list) => { const seen = new Set(); return list.filter((row) => !seen.has(row.id) && seen.add(row.id)); };
    return {
      id,
      headerReason: response.header && !section_ok(response.header) ? reasonOf(response.header, "This entity's name and kind could not be read.") : null,
      name: text(header.name) || "Entity",
      kind: text(header.kind),
      container: containerOf(header),
      containerId: UUID.test(text(header.container?.id)) ? header.container.id : null,
      dependents: isCount(header.dependents) ? header.dependents : null,
      summary: text(header.summary),
      signature: text(header.signature),
      language: text(source.language) || text(header.language),
      source: section_ok(source) && typeof source.text === "string"
        ? { ok: true, text: source.text, startLine: Number.isSafeInteger(source.start_line) ? source.start_line : null }
        : { ok: false, reason: reasonOf(source, "No source was returned for this entity.") },
      reliers: waiting("relied_on_by") ? { ok: false, pending: true, items: [], total: 0 }
        : section_ok(reliers) ? {
          ok: true,
          // A row Kin matched by name alone is shown as such; a row for
          // several declarations of one name is one row that counts them.
          items: dedupe(listOf(reliers)).map((row) => ({ ...row, relation: text(row.relationship),
            test: row.is_test === true || (row.is_test == null && isTest(row)), confirmed: row.confirmed !== false,
            relies: row.relies === undefined ? row.confirmed !== false : row.relies === true,
            declarations: isCount(row.declarations) && row.declarations > 1 ? row.declarations : 0 })),
          total: isCount(reliers.total) ? reliers.total : listOf(reliers).length,
          reliesTotal: isCount(reliers.relies_total) ? reliers.relies_total : null,
          unproven: isCount(reliers.total) && isCount(reliers.confirmed) ? reliers.total - reliers.confirmed : 0,
          bound: text(reliers.bound), notice: text(reliers.notice) }
        : { ok: false, items: [], total: 0, reason: reasonOf(reliers, "What relies on it could not be read.") },
      uses: section_ok(uses)
        ? { ok: true, items: dedupe(listOf(uses)).map((row) => ({ ...row, relation: text(row.relationship), through: array(row.through).filter((name) => typeof name === "string") })), total: isCount(uses.total) ? uses.total : listOf(uses).length, bound: text(uses.bound),
          throughMembers: uses.through_members === true, notice: text(uses.notice) }
        : { ok: false, items: [], total: 0, reason: reasonOf(uses, "What it relies on could not be read.") },
      tests: waiting("tests") ? { ok: false, pending: true, items: [], total: 0 }
        : section_ok(tests)
        ? { ok: true, items: dedupe(listOf(tests)).map((row) => ({ ...row, test: true })), total: isCount(tests.total) ? tests.total : listOf(tests).length, bound: text(tests.bound), depth: isCount(tests.depth) ? tests.depth : null }
        : { ok: false, items: [], total: 0, reason: reasonOf(tests, "The tests that reach it could not be read.") },
      members: section_ok(response.members) ? { ok: true, items: listOf(response.members), total: isCount(response.members.total) ? response.members.total : listOf(response.members).length } : null,
      impact: impact ? section_ok(impact)
        ? { ok: true, direct: isCount(impact.direct) ? impact.direct : null, transitive: isCount(impact.transitive) ? impact.transitive : null, tests: isCount(impact.tests) ? impact.tests : null, depth: isCount(impact.depth) ? impact.depth : null,
          top: array(impact.top).filter((row) => row && UUID.test(row.id)).map((row) => ({ ...row, test: isTest(row) })), bound: text(impact.bound), notice: text(impact.notice) }
        : { ok: false, reason: reasonOf(impact, "What a change would touch could not be read.") } : null,
      history: waiting("history") ? { ok: false, pending: true, items: [] } : history ? section_ok(history)
        ? { ok: true, items: array(history.items).filter((row) => row && (row.change_id || row.id)).map((row) => ({ id: row.change_id || row.id, title: text(row.title), author: authorOf(row), time: row.time || row.timestamp })), total: isCount(history.total) ? history.total : array(history.items).length, recordStart: text(history.record_start) }
        : { ok: false, reason: reasonOf(history, "This entity's history could not be read.") } : null,
      editor: section_ok(editor) ? graphApi()?.editorUri?.(editor.uri, id) || null : null,
      editorReason: !section_ok(editor) ? text(editor.reason) : "",
    };
  }

  // The quick parts of an entity answer in about a second; what relies on it
  // and its history are slower walks, read second and drawn when they land.
  const ENTITY_FIRST = "header,source,relies_on,members,impact,tests,editor";
  const ENTITY_LATER = ["relied_on_by", "history"];
  const ENTITY_SECTIONS = ["entity-head", "entity-reach", "entity-tabs", "entity-connections", "entity-impact", "entity-relations", "entity-source", "entity-members", "entity-history"];

  async function loadEntity(id, quiet = false) {
    const entity = state.entity;
    if (!UUID.test(id || "")) return;
    const mine = ++entity.request;
    if (entity.id !== id) { entity.data = null; entity.raw = null; entity.expanded = false; entity.showAll = {}; }
    entity.id = id;
    entity.error = null;
    entity.missing = false;
    entity.busy = true;
    if (!quiet) paintEntity();
    const base = `/api/workspace/entity?id=${encodeURIComponent(id)}`;
    const draw = (raw, pending) => {
      if (!raw || raw.id !== id) throw new Error("The entity read named a different entity.");
      entity.raw = raw;
      entity.data = entityView(raw, pending);
      entity.error = null;
      paintEntity();
    };
    try {
      const first = await fetchJSON(`${base}&sections=${ENTITY_FIRST}`);
      if (mine !== entity.request) return;
      const later = ENTITY_LATER.filter((key) => !(key in first));
      draw(first, new Set(later));
      announce(`${entity.data.name} read.`);
      if (later.length) {
        try {
          const rest = await fetchJSON(`${base}&sections=${later.join(",")}`);
          if (mine !== entity.request) return;
          draw({ ...first, ...Object.fromEntries(later.map((key) => [key, rest[key] ?? { ok: false, reason: "Kin did not answer this part." }])) }, new Set());
        } catch (error) {
          if (mine !== entity.request) return;
          const reason = error.message || "This part could not be read.";
          draw({ ...first, ...Object.fromEntries(later.map((key) => [key, { ok: false, reason }])) }, new Set());
        }
      }
    } catch (error) {
      if (mine !== entity.request) return;
      entity.error = error.message || "The entity could not be read.";
      // An id the current code does not hold is a page of its own, not a failed read.
      entity.missing = error.status === 404 && !entity.data;
    } finally {
      if (mine === entity.request) { entity.busy = false; paintEntity(); }
    }
  }
  function paintEntity() {
    for (const id of ENTITY_SECTIONS) paint(id);
    // An entity the current code does not hold has nothing to draw below its line.
    const body = byId("entity-body");
    if (body) body.hidden = Boolean(state.entity.missing);
    if (state.entity.data) document.title = `${state.entity.data.name} · ${session() ? repositoryLabel(session().repository) : "Kin"}`;
  }

  const ENTITY_TABS = [["connections", "Connections"], ["source", "Source"], ["members", "Members"], ["history", "History"]];
  /** The tabs an entity has: every entity is joined to others and has source and a history; only some hold members. */
  function entityTabs() {
    const data = state.entity.data;
    if (state.entity.missing) return null;
    const members = data?.members?.ok ? (isCount(data.members.total) ? data.members.total : data.members.items.length) : 0;
    const changes = data?.history?.ok && isCount(data.history.total) ? data.history.total : null;
    const tabs = ENTITY_TABS.filter(([key]) => key !== "members" || members > 0);
    if (!tabs.some(([key]) => key === state.entity.tab)) state.entity.tab = "connections";
    showEntityPanel();
    return h("div", { class: "tabs", role: "tablist", "aria-label": "About this entity" }, tabs.map(([key, label]) => h("button", { type: "button", class: "tab", role: "tab", id: `entity-tab-${key}`,
      "aria-selected": String(state.entity.tab === key), "aria-controls": `entity-panel-${key}`, tabindex: state.entity.tab === key ? "0" : "-1",
      on: { click: () => { state.entity.tab = key; paint("entity-tabs"); showTab(key); },
        keydown: (event) => {
          if (!["ArrowRight", "ArrowLeft"].includes(event.key)) return;
          event.preventDefault(); event.stopPropagation();
          const at = tabs.findIndex(([name]) => name === state.entity.tab);
          state.entity.tab = tabs[(at + (event.key === "ArrowRight" ? 1 : tabs.length - 1)) % tabs.length][0];
          paint("entity-tabs"); showTab(state.entity.tab);
        } } },
      label, key === "members" ? h("span", { class: "tab-count" }, count(members)) : key === "history" && changes ? h("span", { class: "tab-count" }, count(changes)) : null)));
  }
  /** Focus a tab and bring it inside the row's own scroll box, so a narrow window never hides the one chosen. */
  function showTab(key) {
    const tab = byId(`entity-tab-${key}`);
    tab?.focus({ preventScroll: true });
    tab?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }
  function showEntityPanel() {
    for (const [key] of ENTITY_TABS) { const panel = byId(`entity-panel-${key}`); if (panel) panel.hidden = key !== state.entity.tab; }
  }

  function entity() {
    const panel = (key, ...children) => h("div", { id: `entity-panel-${key}`, class: "tab-panel", role: "tabpanel", "aria-labelledby": `entity-tab-${key}`, hidden: state.entity.tab !== key }, children);
    return [
      section("entity-head", ["working"], entityHead, { class: "page-head" }),
      h("div", { id: "entity-body", hidden: Boolean(state.entity.missing) },
        section("entity-reach", [], () => entityImpact("sentence"), { class: "entity-reach" }),
        section("entity-tabs", [], entityTabs),
        panel("connections",
          h("section", { class: "section-tight", "aria-labelledby": "entity-connections-title" }, section("entity-connections", [], entityConnections)),
          h("section", { class: "section", "aria-label": "What a change might affect" }, section("entity-impact", [], () => entityImpact("lists"))),
          section("entity-relations", [], () => entityRelations("builds"))),
        panel("source", h("section", { class: "section-tight", "aria-labelledby": "entity-source-title" }, section("entity-source", [], entitySource))),
        panel("members", section("entity-members", [], () => entityRelations("members"))),
        panel("history", section("entity-history", [], () => entityRelations("history"))))];
  }

  const retryEntity = () => loadEntity(state.entity.id);
  const paintReach = () => { paint("entity-reach"); paint("entity-impact"); };
  /** Whether the source's own first line already reads as the signature. */
  function sourceShowsSignature(data) {
    if (!data.source?.ok || !data.signature) return false;
    const squash = (value) => String(value).replace(/\s+/g, "");
    const first = squash(data.source.text.split("\n").find((line) => line.trim()) || "");
    const signature = squash(data.signature);
    // A declaration often opens with a word the signature leaves out, and runs over several lines.
    const opening = squash(data.source.text).slice(0, signature.length + 48);
    return opening.includes(signature) || first.startsWith(signature) || signature.startsWith(first.replace(/:$/, ""));
  }

  /** A documentation sentence with its ``literal`` spans set as code. */
  function docText(value) {
    return String(value).split(/``(.+?)``/).map((part, i) => i % 2 ? h("code", {}, part) : part);
  }
  /** How many entities rows stand for: a row for several declarations of one name counts each. */
  const entityCount = (rows) => rows.reduce((sum, row) => sum + (row.declarations || 1), 0);
  /** The entities that rely on it: each one that calls, imports or refers to it, tests included. */
  const relyingRows = (data) => data.reliers.items.filter((row) => row.relies);
  /**
   * How many entities rely on it, in the one sense every screen counts:
   * Kin's own ranking number when the read carried it, otherwise the rows
   * that rely on it.
   */
  function relyCount(data) {
    if (isCount(data.dependents)) return data.dependents;
    if (!data.reliers.ok) return null;
    return isCount(data.reliers.reliesTotal) ? data.reliers.reliesTotal : entityCount(relyingRows(data));
  }

  function entityHead() {
    const e = state.entity;
    const data = e.data;
    const crumbs = h("nav", { class: "crumbs", "aria-label": "Breadcrumb" }, h("a", { href: "#/explore" }, "Code"),
      data?.container ? [h("span", { "aria-hidden": "true" }, "/"), data.containerId ? h("a", { href: entityHref(data.containerId) }, data.container) : h("span", {}, data.container)] : null);
    if (!data) {
      if (e.missing) {
        return [crumbs, h("h1", { class: "entity-title entity-title-plain" }, "This entity isn't in the current code."),
          h("p", { class: "lede" }, "It may have been renamed or removed."),
          h("div", { class: "entity-actions" }, link("Search for it", "#/explore", true),
            h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", { class: "detail-text" }, e.error)))];
      }
      if (e.error) return [crumbs, h("div", { class: "section-tight" }, failed(isSlow(e.error) ? "Kin is still working on this entity. Try again in a moment." : "Kin couldn't load this entity.", retryEntity, e.error))];
      return [crumbs, skeleton("skeleton-lede"), skeleton("skeleton-line")];
    }
    const place = h("p", { class: "entity-place" }, kindWord(kindOf(data)) || "Entity", data.container && !data.name.startsWith(`${data.container}.`) ? ` in ${data.container}` : "",
      workSteps(read("working").data).some((step) => step.entity_id === data.id) ? [" · ", h("a", { class: "link", href: UNCOMMITTED }, "changed in uncommitted work")] : null);
    const feedback = h("span", { class: "note", role: "status" });
    const actions = h("div", { class: "entity-actions" });
    if (data.editor) {
      actions.append(h("a", { class: "button", href: data.editor, on: { click: () => { feedback.textContent = "If prompted, confirm to open VS Code or cancel to stay here. If nothing opens, install Kin for VS Code and open this repository there."; } } }, icon("editor"), "Open in VS Code"));
    }
    actions.append(button("Copy entity ID", async () => {
      try { await navigator.clipboard.writeText(data.id); feedback.textContent = "Copied."; }
      catch (_) { feedback.textContent = data.id; }
    }, { class: "button button-quiet" }), feedback);
    if (!data.editor) feedback.textContent = "Kin can't open this one in an editor yet.";
    return [crumbs, h("h1", { class: "entity-title" }, data.name), place,
      // A summary is a sentence; comment markup that came through unread is left out.
      data.summary && !/^\s*[*@/]|\*\/|@param|@returns?/.test(data.summary) ? h("p", { class: "entity-summary" }, docText(data.summary)) : null,
      data.headerReason ? h("p", { class: "note" }, data.headerReason) : null,
      data.signature && !sourceShowsSignature(data) ? h("pre", { class: "entity-signature-block", tabindex: "0", "aria-label": "Signature" }, data.signature) : null,
      actions,
      e.error ? h("div", { class: "section-tight" }, failed("Kin couldn't refresh this entity. Showing what it last saw.", retryEntity, e.error)) : null];
  }

  /** The entity between what relies on it and what it relies on, joined by lines. Each side opens in place. */
  function entityConnections(onCleanup) {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    const head = h("h2", { id: "entity-connections-title", class: "sr" }, "Connections");
    if (!data) return [head, state.entity.error ? null : skeleton("skeleton-block")];
    const PER_SIDE = 7;
    const relying = relyingRows(data);
    const ordered = [...relying.filter((row) => !row.test), ...relying.filter((row) => row.test)];
    const relyTotal = relyCount(data);
    const open = state.entity.showAll || {};
    const side = (key, rows, total) => {
      const names = new Map();
      for (const row of rows) names.set(row.name, (names.get(row.name) || 0) + 1);
      const node = (row) => h("a", { class: "conn-node", href: entityHref(row.id), title: row.name }, h("span", { class: "ent-name" }, row.name),
        row.declarations > 1 ? h("span", { class: "conn-tag" }, `${count(row.declarations)} overloads`) : row.test ? h("span", { class: "conn-tag" }, "test") : names.get(row.name) > 1 ? h("span", { class: "conn-tag" }, kindWord(row.kind).toLowerCase()) : null);
      const shown = open[key] ? rows : rows.slice(0, PER_SIDE);
      const hidden = rows.length - shown.length;
      const unlisted = Math.max(0, (total ?? rows.length) - entityCount(rows));
      return { nodes: shown.map(node), tail: [
        hidden > 0 ? h("button", { type: "button", class: "conn-node conn-node-more", on: { click: () => { state.entity.showAll = { ...open, [key]: true }; paint("entity-connections"); } } }, `Show ${count(hidden)} more`) : null,
        !hidden && unlisted > 0 ? h("span", { class: "conn-node conn-node-more" }, `${count(unlisted)} more that Kin counted and did not list`) : null] };
    };
    const leftSide = side("connLeft", ordered, relyTotal);
    const rightSide = side("connRight", data.uses.items, data.uses.total);
    const left = h("div", { class: "conn-col" }, h("div", { class: "conn-col-head" }, "What relies on it", isCount(relyTotal) ? h("span", { class: "count" }, graphBadge(relyTotal, data.reliers.bound === "at_least")) : null),
      data.reliers.pending ? skeletonLines(3) : !data.reliers.ok ? h("p", { class: "conn-empty" }, data.reliers.reason) : leftSide.nodes.length || leftSide.tail.some(Boolean) ? [leftSide.nodes, leftSide.tail] : h("p", { class: "conn-empty" }, "Kin found no entities that rely on it."));
    const focus = h("div", { class: "conn-focus" }, h("span", { class: "ent-name" }, data.name));
    const right = h("div", { class: "conn-col" }, h("div", { class: "conn-col-head" }, data.uses.throughMembers ? "What it and its members rely on" : "What it relies on", data.uses.ok ? h("span", { class: "count" }, graphBadge(data.uses.total, data.uses.bound === "at_least")) : null),
      !data.uses.ok ? h("p", { class: "conn-empty" }, data.uses.reason) : rightSide.nodes.length || rightSide.tail.some(Boolean) ? [rightSide.nodes, rightSide.tail] : h("p", { class: "conn-empty" }, "Kin found no entities it relies on."));
    const host = h("div", { class: "conn", role: "group", "aria-label": `${data.name}, with ${relyWords(relyTotal, data.reliers.bound === "at_least") || "what relies on it still loading"}, and ${data.uses.ok ? `it relies on ${data.uses.bound === "at_least" ? "at least " : ""}${plural(data.uses.total, "entity", "entities")}` : "what it relies on could not be read"}` }, left, focus, right);
    const join = graphApi()?.joinColumns;
    if (join) {
      // The lines are measured, so they are drawn once the boxes are on the page.
      const mount = () => { if (host.isConnected) onCleanup(join(host, { left: leftSide.nodes, focus, right: rightSide.nodes })); };
      if (globalThis.requestAnimationFrame) requestAnimationFrame(mount); else mount();
    }
    const notice = data.reliers.notice || data.uses.notice || "";
    return [head, host, notice ? h("p", { class: "note quiet-line" }, notice) : null];
  }

  /** Lines of an entity body with the indentation it had in place. */
  function bodyLines(body) {
    const lines = body.replace(/\s+$/, "").split("\n");
    if (lines.length < 2 || /^\s/.test(lines[0])) return lines;
    const indents = lines.slice(1).filter((line) => line.trim()).map((line) => line.match(/^[ \t]*/)[0].length);
    if (!indents.length) return lines;
    const least = Math.min(...indents);
    const steps = [...new Set(indents)].sort((a, b) => a - b);
    const step = steps.length > 1 ? Math.max(1, steps[1] - steps[0]) : Math.min(4, least);
    const cut = Math.max(0, least - step);
    return [lines[0], ...lines.slice(1).map((line) => line.slice(Math.min(cut, line.match(/^[ \t]*/)[0].length)))];
  }

  function codeView(body, language, startLine, expanded, expand) {
    const lines = bodyLines(body);
    const LIMIT = 160;
    const shown = expanded ? lines : lines.slice(0, LIMIT);
    let tokens = null;
    try { tokens = globalThis.kinReview?.highlightLines?.(shown.join("\n"), language) || null; } catch (_) { tokens = null; }
    const numbered = Number.isSafeInteger(startLine) && startLine > 0;
    const grid = h("div", { class: `code-lines${numbered ? "" : " code-lines-plain"}` });
    shown.forEach((line, i) => {
      if (numbered) grid.append(h("span", { class: "code-no", "aria-hidden": "true" }, String(startLine + i)));
      const cell = h("span", { class: "code-text" });
      const parts = tokens?.[i];
      if (Array.isArray(parts) && parts.length) for (const [type, value] of parts) cell.append(type ? h("span", { class: `t-${type}` }, value) : document.createTextNode(value));
      else cell.append(document.createTextNode(line || " "));
      if (!cell.textContent) cell.append(document.createTextNode(" "));
      grid.append(cell);
    });
    return h("div", { class: "codeview", tabindex: "0", role: "region", "aria-label": "Source" }, grid,
      !expanded && lines.length > LIMIT ? h("button", { type: "button", class: "code-more", on: { click: expand } }, `Show ${count(lines.length - LIMIT)} more lines`) : null);
  }

  function entitySource() {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    const head = h("div", { class: "section-head section-head-quiet" }, h("h2", { id: "entity-source-title", class: "sr" }, "Source"), data?.language ? h("span", { class: "aside" }, languageWord(data.language)) : null);
    if (!data) return [head, state.entity.error ? null : skeletonLines(8)];
    if (!data.source.ok) return [head, failed(data.source.reason, retryEntity)];
    return [head, codeView(data.source.text, data.language, data.source.startLine, state.entity.expanded, () => { state.entity.expanded = true; paint("entity-source"); })];
  }

  const IMPACT_WORD = { contract_consumer: "uses its contract", runtime_caller: "calls it", type_dependent: "uses its type", dependent: "relies on it", test: "Test", derived: "derived from it" };
  const steps = (hops) => isCount(hops) && hops > 1 ? `${hops} steps away` : "";

  /**
   * One group of what a change would reach: a title with its count, the first
   * rows, and a button that shows the rest in place. Rows Kin counted but did
   * not list are said as such.
   */
  function reachGroup(key, title, rows, total, caption, shown = 8) {
    const all = Boolean(state.entity.showAll?.[key]);
    const visible = all ? rows : rows.slice(0, shown);
    const hidden = rows.length - visible.length;
    const unlisted = Math.max(0, total - entityCount(rows));
    const each = (row) => row.declarations ? `${count(row.declarations)} overloads` : caption(row);
    const same = rows.length > 1 && new Set(rows.map(each)).size === 1;
    const said = (row) => {
      const duplicate = rows.some((other) => other.id !== row.id && other.name === row.name);
      return [duplicate ? [kindWord(row.kind), containerOf(row)].filter(Boolean).join(" in ") : "", same && !duplicate ? "" : each(row)].filter(Boolean).join(" · ") || null;
    };
    return h("div", { class: "reach-group" }, h("h3", {}, title, isCount(total) ? h("span", { class: "count" }, graphBadge(total, key === "tests" && state.entity.data?.tests.bound === "at_least")) : null),
      h("div", { class: "reach-list" }, visible.map((row) => h("a", { class: "relier", href: entityHref(row.id), title: row.name }, ent(row, { linked: false, test: row.test === true }), h("span", { class: "relier-caption" }, said(row)))),
        hidden > 0 ? h("button", { type: "button", class: "relier", on: { click: () => { state.entity.showAll = { ...(state.entity.showAll || {}), [key]: true }; paintReach(); } } }, unlisted > 0 ? `Show ${count(hidden)} more of the ${count(total)}` : `Show ${count(hidden)} more`) : null,
        !hidden && unlisted > 0 ? h("span", { class: "relier note" }, `Kin lists ${count(entityCount(rows))} of the ${count(total)} here`) : null));
  }

  function entityImpact(part) {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    const lists = part === "lists";
    if (!data) return lists || state.entity.error ? null : skeleton("skeleton-lede");
    if (data.impact && !data.impact.ok) return lists ? null : failed(data.impact.reason, retryEntity);
    if (!data.impact && !data.reliers.ok) return lists ? null : failed(data.reliers.reason, retryEntity);
    const impact = data.impact;
    const relying = relyCount(data);
    const reach = impact?.transitive ?? null;
    const tests = impact?.tests ?? data.tests.total;
    const limitedTests = data.tests.bound === "at_least" || impact?.bound === "at_least";
    // Said once per sentence: after "within 3 steps" on what a change might affect, the tests read in the same steps.
    const testWords = (depth) => data.tests.pending ? "Kin is still reading the tests that reach it." : !data.tests.ok ? "Kin could not read the tests that reach it." : tests ? `${limitedTests ? `At least ${spoken(tests).toLowerCase()}` : spoken(tests)} ${tests === 1 ? "test reaches" : "tests reach"} it${withinSteps(depth)}.` : "Kin found no tests that reach it.";
    const testDepthHere = data.tests.depth ?? impact?.depth ?? null;
    let sentence;
    const atLeast = text(impact?.bound) === "at_least" ? "at least " : "";
    if (isCount(reach) && reach > 0) sentence = `A change here might affect ${atLeast}${plural(reach, "entity", "entities")}${impact.depth ? ` within ${impact.depth} steps` : ""}. ${testWords(impact.depth && impact.depth === testDepthHere ? null : testDepthHere)}`;
    else if (reach === 0 || relying === 0) sentence = `Kin found nothing that relies on ${data.name}.`;
    else if (isCount(relying)) sentence = `${relyWords(relying, data.reliers.bound === "at_least")}. ${testWords(testDepthHere)}`.replace(/^\w/, (c) => c.toUpperCase());
    else sentence = "Kin is still working out what relies on it.";
    const groups = [];
    const further = array(impact?.top).filter((row) => isCount(row.hops) ? row.hops > 1 : !data.reliers.items.some((other) => other.id === row.id));
    // A module's reason is its import, and only a type has a type to use, so the word follows both kinds.
    const reasonWord = (row) => row.reason === "type_dependent" || row.reason === "dependent" ? relationWord(row.reason === "type_dependent" ? "uses_type" : "", row, data) : IMPACT_WORD[row.reason];
    if (further.length) groups.push(reachGroup("further", "Further out", further, further.length, (row) => row.test ? "Test" : steps(row.hops) || reasonWord(row) || kindWord(row.kind)));
    if (data.tests.items.length) groups.push(reachGroup("tests", "Tests that reach it", data.tests.items, data.tests.total, (row) => steps(row.hops) || "Test"));
    const notice = impact?.notice || "";
    // The picture above already says when Kin is still mapping; the same line is not said twice.
    const said = data.reliers.notice || data.uses.notice || "";
    if (!lists) return [h("p", { class: "impact-sentence" }, sentence), notice && notice !== said ? h("p", { class: "note" }, notice) : null];
    return groups.length ? h("div", { class: "reach" }, groups) : null;
  }

  function relGroup(key, title, part, caption, empty, tabbed = false) {
    const LIMIT = 12;
    const all = state.entity.showAll?.[key];
    const rows = part.items;
    const visible = all ? rows : rows.slice(0, LIMIT);
    const head = tabbed ? h("h3", { class: "sr" }, title) : h("h3", { class: "rel-head" }, title, part.ok ? h("span", { class: "count" }, count(part.total)) : null);
    if (part.pending) return h("div", { class: "rel-group", "aria-busy": "true" }, head, skeletonLines(3));
    if (!part.ok) return h("div", { class: "rel-group" }, head, failed(part.reason, retryEntity));
    if (!rows.length) return h("div", { class: "rel-group" }, head, h("p", { class: "gap" }, empty));
    const more = rows.length - visible.length;
    const unlisted = Math.max(0, part.total - entityCount(rows));
    const each = (row) => row.declarations ? `${count(row.declarations)} overloads` : row.confirmed === false ? "by name only" : caption(row);
    const same = rows.length > 1 && new Set(rows.map(each)).size === 1;
    return h("div", { class: "rel-group" }, head,
      h("div", { class: "rel-list" }, visible.map((row) => h("a", { class: `rel-row${row.confirmed === false ? " rel-row-unproven" : ""}`, href: entityHref(row.id),
        title: row.confirmed === false ? `${row.name}: Kin matched this by name and could not prove it uses this entity.` : row.name },
        ent(row, { linked: false, test: row.test === true }),
        same ? null : h("span", { class: "rel-how" }, each(row)))),
        more > 0 ? h("button", { type: "button", class: "rel-row", on: { click: () => { state.entity.showAll = { ...(state.entity.showAll || {}), [key]: true }; for (const id of ["entity-relations", "entity-members", "entity-history"]) paint(id); } } }, `Show ${count(more)} more`) : null,
        unlisted > 0 && !more ? h("div", { class: "rel-row" }, h("span", { class: "rel-more" }, `${count(unlisted)} more that Kin counted and did not list`)) : null),
      part.notice ? h("p", { class: "note" }, part.notice) : null);
  }

  function entityRelations(part) {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    if (!data) return state.entity.error || part !== "builds" ? null : skeletonLines(4);
    // What relies on it and the tests that reach it are listed once, beside
    // the source. This column holds what that list does not.
    const out = [];
    const builds = data.reliers.ok ? data.reliers.items.filter((row) => row.confirmed && !row.relies) : [];
    if (part === "builds" && builds.length) out.push(relGroup("builds", "Builds on it", { ok: true, items: builds, total: entityCount(builds) }, (row) => relationWord(row.relation, row, data), ""));
    const proven = new Set([...array(data.tests.ok ? data.tests.items : []), ...array(data.impact?.ok ? data.impact.top : []), ...array(data.reliers.ok ? data.reliers.items : []).filter((row) => row.confirmed)].map((row) => row.id));
    const guessed = data.reliers.ok ? data.reliers.items.filter((row) => !row.confirmed && !proven.has(row.id)) : [];
    if (part === "builds" && guessed.length) out.push(relGroup("guessed", "Matched by name only", { ok: true, items: guessed, total: entityCount(guessed), notice: "Kin found these by name and couldn't confirm they use it." }, () => "by name only", ""));
    if (part === "members" && data.members?.ok && data.members.items.length) out.push(relGroup("members", "Members", data.members, (row) => kindWord(row.kind), "", true));
    if (part !== "history") return out;
    const history = data.history;
    const head = h("h3", { class: "sr" }, "History");
    let body;
    if (history?.pending) body = skeletonLines(3);
    else if (!history) body = h("p", { class: "gap" }, "No history was returned for this entity.");
    else if (!history.ok) body = failed(history.reason, retryEntity);
    else if (!history.items.length) body = h("p", { class: "gap" }, "No change has touched it yet.");
    else body = [h("div", { class: "rel-list" }, history.items.slice(0, 10).map((change) => h("a", { class: "history-row", href: reviewHref(change.id) },
      h("div", { class: "history-title" }, change.title || "Untitled change"),
      h("div", { class: "history-meta" }, [change.author, ago(change.time)].filter(Boolean).join(" · ")),
      history.recordStart && change.id === history.recordStart ? h("div", { class: "change-start" }, "Kin's record starts here, with the repository as it was") : null))),
      // Kin has not read further back yet, so this list is where its record begins, not the entity's whole past.
      history.recordStart ? h("p", { class: "note" }, "Kin has read this repository's history only from where its record starts, so earlier changes to this entity are not listed yet.") : null];
    out.push(h("div", { class: "rel-group", "aria-busy": history?.pending ? "true" : undefined }, head, body));
    return out;
  }

  // ── Branches and merges ──────────────────────────────────────────────────

  // Rust's merge response preserves byte identities. Decode only for display;
  // confirmation sends the original expected object without rewriting it.
  function mergeHash(value) {
    return Array.isArray(value) && value.length === 32 && value.every((byte) => Number.isInteger(byte) && byte >= 0 && byte <= 255)
      ? value.map((byte) => byte.toString(16).padStart(2, "0")).join("") : null;
  }
  function mergeRef(value) {
    if (!value || typeof value !== "object" || Array.isArray(value) || Object.keys(value).length !== 1 ||
        typeof value.bytes_hex !== "string" || !/^(?:[0-9a-f]{2})+$/.test(value.bytes_hex)) return null;
    const bytes = Uint8Array.from(value.bytes_hex.match(/../g), (byte) => parseInt(byte, 16));
    try { return new TextDecoder("utf-8", { fatal: true }).decode(bytes); }
    catch (_) { return `Reference bytes (non-UTF-8): ${value.bytes_hex}`; }
  }
  function mergeIdentity(preview) {
    const expected = preview.expected || {};
    return { source: mergeRef(expected.source_ref), target: mergeRef(expected.target_ref),
      ours: mergeHash(expected.ours_change), theirs: mergeHash(expected.theirs_change), base: mergeHash(preview.base_change) };
  }
  function detail(label, value, code = false) { return h("div", {}, h("dt", {}, label), h("dd", { title: value || undefined }, code && value ? h("code", {}, value) : value || "Not reported")); }

  const MERGE_PREVIEW_WORDS = { already_up_to_date: "Already up to date", fast_forward: "Would move the branch forward", merged: "Would record a merge", conflicted: "Conflicts to resolve" };
  const MERGE_RESULT_WORDS = { already_up_to_date: "Already up to date", fast_forward: "Branch moved forward", merged: "Merged", conflicted: "Conflicts saved to resolve" };

  // A tab may reload while a confirmation is still in flight. This receipt is
  // only a reminder that no result was observed; it cannot establish success.
  // Keep it scoped to this repository and never store an authorization token.
  function mergeReceiptKey() {
    const repo = session()?.repository;
    return repo ? `kin.workspace.pending-merge:${JSON.stringify([apiBase, repo.key || null])}` : null;
  }
  function saveMergeReceipt() {
    const key = mergeReceiptKey();
    if (!key) return;
    const expected = state.merge.preview?.expected;
    try { sessionStorage.setItem(key, JSON.stringify({ source: state.merge.source.trim(), operationId: state.merge.operationId, startedAt: Date.now(), repositoryId: expected?.repository_id || null, workspaceId: expected?.workspace_id || null })); } catch (_) { /* The in-memory lockout remains. */ }
  }
  function clearMergeReceipt() {
    const key = mergeReceiptKey();
    if (!key) return;
    try { sessionStorage.removeItem(key); } catch (_) { /* Do not turn a definite result into another outcome. */ }
  }
  function restoreMergeReceipt() {
    const merge = state.merge;
    if (merge.receiptRestored) return;
    merge.receiptRestored = true;
    try {
      const held = JSON.parse(sessionStorage.getItem(mergeReceiptKey()));
      if (!held || typeof held.source !== "string" || typeof held.operationId !== "string" || !held.operationId) return;
      merge.source = held.source;
      merge.operationId = held.operationId;
      merge.unknown = true;
      merge.error = "This tab sent an earlier merge request and never saw its result. Reloading cannot tell whether it finished.";
      const repo = session().repository;
      if ((repo.repository_id && held.repositoryId && repo.repository_id !== held.repositoryId) ||
          (repo.workspace_id && held.workspaceId && repo.workspace_id !== held.workspaceId)) {
        merge.error += " The repository now reports a different identity than that request expected.";
      }
    } catch (_) { /* Missing tab storage carries no authority. */ }
  }

  async function mergeRequest(kind) {
    const merge = state.merge;
    // A preview only reads, so it may start while the page is reading; a
    // confirmation waits for the page to settle.
    if (workspaceWriteBusy() || (kind === "confirm" && state.refreshing)) return;
    const source = merge.source.trim();
    if (!source || !session()?.token) return;
    if (kind === "confirm" && (!session()?.capabilities?.merge || !merge.preview || !merge.confirmed)) return;
    if (kind === "preview") { merge.preview = null; merge.result = null; merge.operationId = null; merge.reportedOperationId = null; merge.confirmed = false; }
    else {
      if (!globalThis.crypto?.randomUUID) { merge.error = "This browser could not create an operation ID. Reload the page before confirming."; updateMergePanel(); return; }
      merge.operationId = merge.operationId || crypto.randomUUID();
    }
    merge.error = null;
    merge.remedy = null;
    merge.busy = kind;
    updateMergePanel();
    try {
      const payload = kind === "preview" ? { source } : { source, expected: merge.preview.expected, operation_id: merge.operationId };
      if (kind === "confirm") saveMergeReceipt();
      const result = await fetchJSON(`/api/workspace/merge/${kind}`, { method: "POST", headers: { "content-type": "application/json", "x-kin-review-token": session().token }, body: JSON.stringify(payload) });
      if (kind === "preview") {
        if (result.schema !== "kin.merge-preview.v1" || !result.expected || !MERGE_PREVIEW_WORDS[result.outcome] ||
            Object.values(mergeIdentity(result)).some((value) => value === null)) throw new Error("The server returned an incomplete merge preview. No confirmation is available.");
        merge.preview = result;
        announce("Merge preview ready.");
      } else {
        if (!result.report || !MERGE_RESULT_WORDS[result.report.outcome]) throw new Error("The merge response could not be read. Check the repository before trying again.");
        merge.result = result;
        merge.preview = null;
        merge.confirmed = false;
        clearMergeReceipt();
        announce(MERGE_RESULT_WORDS[result.report.outcome]);
      }
    } catch (error) {
      merge.error = error.message || "The merge request failed.";
      merge.diagnostic = null;
      merge.failedKind = kind;
      if (kind === "confirm" && error.status === 409 && error.outcome === "refused") { merge.diagnostic = merge.error; merge.error = "The merge was refused before anything was written. Read a new preview."; }
      merge.remedy = error.remedy || null;
      merge.reportedOperationId = error.operationId || null;
      if (kind === "confirm") {
        merge.confirmed = false;
        merge.preview = null;
        if (error.status === 409 && error.outcome === "refused") {
          merge.unknown = false;
          clearMergeReceipt();
        } else merge.unknown = true;
      }
      announce(kind === "confirm" && merge.unknown ? "The merge outcome is unknown." : "The merge request could not be completed.");
    } finally {
      merge.busy = null;
      updateMergePanel();
      if (kind === "confirm" && merge.result) refresh(["changes", "branches", "branchState", "conflicts", "standing", "working"]);
    }
  }

  /** The merge panel opens from a branch row, and stays open while it has something to say. */
  const mergeShown = () => { const merge = state.merge; return Boolean(state.mergeOpen || merge.preview || merge.result || merge.error || merge.unknown || merge.busy); };
  const switchShown = () => { const branch = state.branch; return Boolean(state.switchOpen || branch.preview || branch.result || branch.error || branch.unknown || branch.busy); };
  /**
   * When a refusal says pending work must be committed or set aside, and the
   * working read shows uncommitted work, say so plainly. The refusal's own
   * words stay under Details.
   */
  function workInTheWay(message) {
    const guide = read("working").data;
    const pending = guide && (workSentence(guide) || guide.working_tree?.differs);
    return pending && /commit|stash/i.test(text(message)) ? "Your uncommitted work is in the way. Commit it or set it aside, then preview again." : "";
  }

  /** An operation's identity, for a reader who needs it, behind Show details. */
  function operationDetails(id, reported) {
    if (!id && !reported) return null;
    return h("details", { class: "more" }, h("summary", {}, "Show details"),
      id ? h("p", {}, "Operation ", h("code", {}, id)) : null,
      reported && reported !== id ? h("p", {}, "Kin's operation ", h("code", {}, reported)) : null);
  }

  /** What a merge brings in, in the guided review's own words, when Kin has read that branch. */
  function mergeBrings(source, target) {
    const held = state.branchGuides.get(`${target}..${source}`);
    if (!held) { if (source && target) Promise.resolve().then(() => loadBranchGuide(source, target)); return ""; }
    if (!held.data) return "";
    const sentence = workSentence(held.data);
    return sentence ? `It brings in this: ${sentence}` : "It brings in no entity change.";
  }

  function closeButton(label, close) {
    return h("button", { type: "button", class: "button button-small panel-close", on: { click: close } }, label);
  }

  function mergePanel() {
    const merge = state.merge;
    if (!mergeShown()) return null;
    const canConfirm = Boolean(session()?.capabilities?.merge) && !state.branch.busy && !state.branch.unknown;
    const canPreview = canConfirm || Boolean(session()?.capabilities?.merge_preview);
    const unavailableReason = state.branch.busy || state.branch.unknown ? "Check the pending branch switch before merging." : session()?.merge_unavailable_reason || "Merging is not available in this session.";
    const into = shortRef(activeBranch()?.name) || "the current branch";
    const mergeable = branchRows().filter((row) => !row.active);
    if (read("branches").data && !mergeable.length && !merge.preview && !merge.result && !merge.error && !merge.unknown) {
      return h("section", { class: "card panel" }, h("h2", {}, `Merge into ${into}`), h("p", {}, "There is no other branch to merge yet."));
    }
    if (!canPreview) return h("section", { class: "card panel" }, h("h2", {}, `Merge into ${into}`), h("p", {}, unavailableReason));
    const source = h("input", { id: "workspace-merge-source", name: "source", list: "workspace-branch-options", placeholder: "Branch to merge", value: merge.source, required: true, disabled: Boolean(merge.busy) || merge.unknown, autocomplete: "off", spellcheck: "false", on: { input: (event) => { merge.source = event.target.value; merge.preview = null; merge.result = null; merge.error = null; merge.operationId = null; merge.confirmed = false; const result = byId("workspace-merge-evidence"); if (result) result.replaceChildren(); } } });
    const form = h("form", { class: "panel-form workspace-merge-form", on: { submit: (event) => { event.preventDefault(); mergeRequest("preview"); } } },
      h("div", { class: "field" }, h("label", { for: "workspace-merge-source" }, "Branch to merge"), source),
      h("button", { type: "submit", class: "button", disabled: Boolean(merge.busy) || merge.unknown || !session()?.token }, merge.busy === "preview" ? "Loading…" : "Preview merge"),
      h("datalist", { id: "workspace-branch-options" }, branchRows().filter((row) => !row.active).map((row) => h("option", { value: row.name }))));
    const evidence = h("div", { class: "evidence", id: "workspace-merge-evidence", tabindex: "-1" });
    const previewFailed = merge.failedKind === "preview" && !merge.unknown;
    if (merge.error) evidence.append(h("div", { class: `message message-${merge.unknown ? "warn" : "bad"}` }, h("div", {},
      h("strong", {}, merge.unknown ? "Merge outcome is unknown" : previewFailed ? "Kin could not preview this merge." : "Merge not completed"),
      previewFailed ? h("p", {}, workInTheWay(merge.error) || merge.remedy || "Nothing was merged.") : h("p", {}, merge.error),
      previewFailed ? h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", {}, merge.error)) : null,
      merge.diagnostic ? h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", {}, merge.diagnostic)) : null,
      merge.unknown ? h("p", {}, "This page will not send it again. Check the repository before starting another merge.") : previewFailed ? null : h("p", {}, merge.remedy || "Read a new preview before confirming."),
      operationDetails(merge.operationId, merge.reportedOperationId))));
    if (merge.preview) {
      const preview = merge.preview;
      const identity = mergeIdentity(preview);
      const conflict = preview.outcome === "conflicted";
      const noop = preview.outcome === "already_up_to_date";
      const checkbox = h("input", { type: "checkbox", id: "workspace-merge-confirm", checked: merge.confirmed, disabled: Boolean(merge.busy), on: { change: (event) => { merge.confirmed = event.target.checked; const action = byId("workspace-merge-submit"); if (action) action.disabled = !merge.confirmed || Boolean(merge.busy); } } });
      const brings = mergeBrings(shortRef(identity.source), shortRef(identity.target));
      evidence.append(h("div", { class: "preview" },
        h("h3", {}, `Merge ${shortRef(identity.source)} into ${shortRef(identity.target)}`, pill(MERGE_PREVIEW_WORDS[preview.outcome], conflict ? "warn" : "accent")),
        brings ? h("p", { class: "preview-sentence" }, brings) : null,
        h("details", { class: "more" }, h("summary", {}, "Show details"), h("dl", { class: "heads" }, detail("Branch", identity.source, true), detail("Its latest change", identity.theirs, true), detail("Into", identity.target, true), detail("Its latest change", identity.ours, true), detail("Where they split", identity.base, true))),
        array(preview.limitations).length ? h("ul", { class: "limits" }, preview.limitations.map((limitation) => h("li", {}, limitation))) : null,
        conflict ? h("div", { class: "message message-warn" }, h("div", {}, h("strong", {}, `${plural(preview.conflicts_total ?? array(preview.conflicts).length, "conflict")}`), h("p", {}, "Confirming records the conflicts for you to resolve. It does not merge the branches."))) : null,
        array(preview.conflicts).length ? h("details", { class: "more" }, h("summary", {}, `Show ${array(preview.conflicts).length} conflicts`), h("ul", { class: "limits" }, preview.conflicts.map((item) => h("li", {}, h("strong", {}, item.name || "Conflict"), " ", item.detail)))) : null,
        noop ? h("p", { class: "note" }, "Nothing to merge.") : !canConfirm ? h("p", { class: "note" }, unavailableReason) : h("div", { class: "confirm" },
          h("label", { for: "workspace-merge-confirm", class: "check" }, checkbox, h("span", {}, `I've reviewed what this merge brings into ${shortRef(identity.target)}.${conflict ? " This records the conflicts to resolve." : ""}`)),
          h("button", { type: "button", class: "button button-primary", id: "workspace-merge-submit", disabled: !merge.confirmed || Boolean(merge.busy), on: { click: () => mergeRequest("confirm") } }, merge.busy === "confirm" ? "Confirming…" : conflict ? "Create conflict record" : "Confirm merge"))));
    }
    if (merge.result) {
      const report = merge.result.report;
      const conflict = report.outcome === "conflicted";
      const mergeChange = mergeHash(report.merge_change);
      evidence.append(h("div", { class: `message message-${conflict ? "warn" : "good"}` }, h("div", {},
        h("strong", {}, MERGE_RESULT_WORDS[report.outcome]),
        h("p", {}, conflict ? "The conflicts are saved for you to resolve. The branches are not merged yet." : report.outcome === "already_up_to_date" ? "The target already holds the source's history." : `Kin merged ${shortRef(mergeRef(report.source_ref)) || "the branch"} into ${shortRef(mergeRef(report.target_ref)) || "the current branch"}.`),
        mergeChange ? h("p", {}, h("a", { href: reviewHref(mergeChange) }, "Review the merge")) : report.merge_change != null ? h("p", {}, "Kin couldn't read which change the merge made. Check the repository for the result.") : null,
        operationDetails(merge.result.operation_id || merge.operationId)),
        conflict ? h("a", { class: "button button-small", href: "#workspace-conflicts", on: { click: (event) => { event.preventDefault(); byId("workspace-conflicts")?.scrollIntoView?.({ block: "start", behavior: "smooth" }); } } }, "Resolve") : null));
    }
    const close = !merge.busy && !merge.unknown ? closeButton("Close", () => { state.mergeOpen = false; merge.preview = null; merge.result = null; merge.error = null; merge.confirmed = false; updateMergePanel(); }) : null;
    return h("section", { class: "card panel", "aria-labelledby": "merge-title" }, h("div", { class: "panel-head" }, h("h2", { id: "merge-title" }, `Merge into ${into}`), close),
      h("p", {}, "Preview first. Confirming acts on exactly what the preview showed."),
      !canConfirm ? h("p", { class: "note" }, unavailableReason) : null,
      !session()?.token ? h("p", { class: "note" }, "This session cannot send merge requests. Reload the page.") : null,
      form, evidence);
  }

  function updateMergePanel() {
    setRefreshDisabled(state.refreshing);
    const host = byId("workspace-merge-host");
    if (host) {
      host.replaceChildren(...[mergePanel()].filter(Boolean));
      paint("branches-list");
      if (!state.merge.busy) byId("workspace-merge-evidence")?.focus({ preventScroll: true });
    }
  }

  function branchReceiptKey() {
    return session()?.repository ? `kin.workspace.pending-branch:${JSON.stringify([apiBase, session().repository.key || null])}` : null;
  }
  function restoreBranchReceipt() {
    const branch = state.branch;
    if (branch.receiptRestored) return;
    branch.receiptRestored = true;
    try {
      const receipt = JSON.parse(sessionStorage.getItem(branchReceiptKey()));
      if (!receipt || typeof receipt.operationId !== "string" || !receipt.operationId) return;
      branch.operationId = receipt.operationId;
      branch.unknown = true;
      branch.error = "This tab sent an earlier branch switch and never saw its result. Check the repository before switching again.";
    } catch (_) { /* Tab storage is a reminder, not repository state. */ }
  }
  function saveBranchReceipt() {
    try { sessionStorage.setItem(branchReceiptKey(), JSON.stringify({ operationId: state.branch.operationId, startedAt: Date.now() })); } catch (_) { /* The in-memory lockout remains. */ }
  }
  function clearBranchReceipt() {
    try { sessionStorage.removeItem(branchReceiptKey()); } catch (_) { /* Preserve the observed result. */ }
  }
  function branchChoices() { return array(read("branchState").data?.branches); }
  function workspaceWriteBusy() { return Boolean(state.merge.busy || state.merge.unknown || state.branch.busy || state.branch.unknown); }
  function branchTarget() { return branchChoices().find((row) => row.name?.bytes_hex === state.branch.target); }
  function updateBranchPanel() {
    setRefreshDisabled(state.refreshing);
    byId("workspace-branch-host")?.replaceChildren(...[branchPanel()].filter(Boolean));
    if (!state.branch.busy) byId("workspace-branch-evidence")?.focus({ preventScroll: true });
    updateMergePanel();
  }
  async function branchRequest(kind) {
    const branch = state.branch;
    if (workspaceWriteBusy() || (kind === "confirm" && state.refreshing) || !session()?.token) return;
    const target = branchTarget();
    if (kind === "preview" && (!target || target.active || read("branchState").error)) return;
    if (kind === "confirm" && (!branch.preview || !branch.confirmed || !session()?.capabilities?.branch_switch)) return;
    branch.error = null;
    branch.diagnostic = null;
    branch.busy = kind;
    if (kind === "preview") { branch.preview = null; branch.result = null; branch.confirmed = false; branch.operationId = null; }
    else {
      if (!globalThis.crypto?.randomUUID) { branch.error = "This browser could not create an operation ID. Reload before switching."; branch.busy = null; updateBranchPanel(); return; }
      branch.operationId = crypto.randomUUID();
      saveBranchReceipt();
    }
    updateBranchPanel();
    try {
      const payload = kind === "preview" ? { name: target.name } : { expected: branch.preview.expected, operation_id: branch.operationId };
      const result = await fetchJSON(`/api/workspace/branch/${kind}`, { method: "POST", headers: { "content-type": "application/json", "x-kin-review-token": session().token }, body: JSON.stringify(payload) });
      if (kind === "preview") {
        if (result.schema !== "kin.branch-switch-preview.v1" || !result.expected || typeof result.target?.display !== "string" || !result.pending_work || !Array.isArray(result.limitations) || result.expected.target_ref?.bytes_hex !== target.name.bytes_hex || result.target.name?.bytes_hex !== target.name.bytes_hex || mergeHash(result.target.change) === null || mergeHash(result.expected.target_change) !== mergeHash(result.target.change)) throw new Error("The branch preview is incomplete. Switching is unavailable.");
        branch.preview = result;
        announce("Switch preview ready.");
      } else {
        if (result.operation_id !== branch.operationId || !result.report || result.report.schema !== "kin.branch-list.v1" || !Array.isArray(result.report.branches) || result.report.repository_id !== payload.expected.repository_id || result.report.workspace_id !== payload.expected.workspace_id || result.report.branches.find((row) => row.active)?.name?.bytes_hex !== payload.expected.target_ref?.bytes_hex) throw new Error("The branch result could not be checked. Look at the repository before trying again.");
        // What came along is named as the page last read it, before the reads start again.
        branch.carried = branch.preview?.pending_work?.present ? workNames() : [];
        branch.result = result;
        branch.preview = null;
        branch.confirmed = false;
        clearBranchReceipt();
        // Every view reads again after a switch. Old source and old merge
        // expectations cannot carry over to the new branch. The session is
        // the repository's, not the branch's, so it stays while the rest is read.
        state.readEpoch += 1;
        for (const [key, resource] of Object.entries(state.reads)) { if (key === "session") continue; resource.data = null; resource.error = null; resource.at = null; resource.busy = false; resource.sig = null; resource.changed = true; }
        state.newBranch.created = null;
        state.entity.data = null;
        state.explore.results = null;
        state.merge.preview = null;
        state.merge.result = null;
        state.merge.confirmed = false;
        announce("Branch switched. Loading the new branch.");
        syncAll(readKeys().filter((key) => key !== "session"));
      }
    } catch (error) {
      branch.diagnostic = error.message;
      branch.error = kind === "preview" ? "The switch preview could not be read." : "The branch switch outcome is unknown. This page will not send it again.";
      if (kind === "confirm") {
        branch.confirmed = false;
        branch.preview = null;
        if (error.status === 409 && error.outcome === "refused") {
          branch.error = "The switch was refused before changing the workspace. Read a new preview.";
          clearBranchReceipt();
        } else branch.unknown = true;
      }
      announce(branch.error);
    } finally {
      branch.busy = null;
      updateBranchPanel();
      if (kind === "confirm" && branch.result) await refresh();
    }
  }
  /** The uncommitted entities by name, as the latest reading of the work lists them. */
  function workNames() { return workSteps(read("working").data).map((step) => shownName(step.name)).filter(Boolean); }
  function nameList(names, most = 4) {
    if (names.length <= most) return names.join(names.length === 2 ? " and " : ", ").replace(/, ([^,]*)$/, " and $1");
    return `${names.slice(0, most - 1).join(", ")} and ${count(names.length - most + 1)} more`;
  }
  /** What comes along when switching, in plain words naming the entities. */
  function carriedWords(present, to) {
    if (!present) return "Nothing is uncommitted, so only the branch changes.";
    const names = workNames();
    if (names.length) return `Your uncommitted ${names.length === 1 ? "change to" : "changes to"} ${nameList(names)} ${names.length === 1 ? "comes" : "come"} along to ${to}.`;
    return read("working").data?.working_tree?.differs ? `Your uncommitted edits come along to ${to}. No entity changed in them.` : `Your uncommitted work comes along to ${to}.`;
  }

  /**
   * Switching is one thing at a time: choosing a branch, reading what a switch
   * does, confirming it, then the switch itself, and its outcome. Each state
   * says only what is true in it, so a switch in flight never sits beside a
   * sentence about the branch it is leaving.
   */
  function branchPanel() {
    const branch = state.branch;
    if (!switchShown()) return null;
    const choices = branchChoices();
    const allowed = Boolean(session()?.capabilities?.branch_switch);
    const busy = workspaceWriteBusy() || !allowed;
    const confirmLocked = busy || state.refreshing;
    const current = choices.find((row) => row.active);
    const others = choices.filter((row) => !row.active);
    const currentName = (current ? shortRef(mergeRef(current.name)) : "") || shortRef(activeBranch()?.name);
    const close = !branch.busy && !branch.unknown ? closeButton("Close", () => { state.switchOpen = false; branch.preview = null; branch.result = null; branch.error = null; branch.confirmed = false; updateBranchPanel(); }) : null;
    const evidence = h("div", { class: "evidence", id: "workspace-branch-evidence", tabindex: "-1" });
    const wrap = (...children) => h("section", { class: "card panel", "aria-labelledby": "switch-title" }, h("div", { class: "panel-head" }, h("h2", { id: "switch-title" }, "Switch branch"), close), ...children, evidence);

    // The switch landed: its receipt, and nothing about the branch it left.
    if (branch.result) {
      const selected = array(branch.result.report?.branches).find((row) => row.active);
      const name = (selected ? shortRef(mergeRef(selected.name)) : "") || "the chosen branch";
      const settled = Boolean(read("branches").data) && shortRef(activeBranch()?.name) === name;
      const carried = array(branch.carried);
      evidence.append(h("div", { class: "message message-good", role: "status" }, h("div", {}, h("strong", {}, `Switched to ${name}`),
        h("p", {}, settled ? `You're on ${name} now.` : `Reading ${name}.`, carried.length ? ` Your uncommitted ${carried.length === 1 ? "change to" : "changes to"} ${nameList(carried)} came along.` : ""),
        operationDetails(branch.result.operation_id))));
      return wrap();
    }
    // An outcome nobody saw: say so; the chooser below stays locked, so nothing sends it again.
    if (branch.unknown) {
      evidence.append(h("div", { class: "message message-warn", role: "alert" }, h("div", {}, h("strong", {}, branch.error || "The branch switch outcome is unknown. This page will not send it again."),
        h("p", {}, "Read the branches again to see which one you are on."),
        branch.diagnostic || branch.operationId ? h("details", { class: "more" }, h("summary", {}, "Show details"), branch.diagnostic ? h("p", {}, branch.diagnostic) : null, branch.operationId ? h("p", {}, "Operation ", h("code", {}, branch.operationId)) : null) : null)));
    }
    if (!session()) return wrap(skeletonLines(1));
    if (!allowed) return wrap(h("p", { class: "note" }, session().branch_unavailable_reason || "This version of Kin can't switch branches from this page."));
    if (read("branchState").error) return wrap(h("div", { class: "section" }, readFailure("branchState")));
    if (!read("branchState").data) return wrap(skeletonLines(1));
    if (!others.length) return wrap(h("p", {}, `You're on ${currentName || "the only branch"}${currentName ? ", the only branch" : ""}. There is nothing to switch to yet.`));

    const select = h("select", { id: "workspace-branch-target", disabled: busy, on: { change: (event) => {
      branch.target = event.target.value; branch.preview = null; branch.confirmed = false; branch.result = null; branch.error = null;
      updateBranchPanel();
      byId("workspace-branch-target")?.focus();
    } } }, h("option", { value: "" }, "Choose a branch"), others.map((row) => h("option", { value: row.name?.bytes_hex, selected: row.name?.bytes_hex === branch.target }, shortRef(mergeRef(row.name)) || "Unreadable branch name")));
    const form = h("form", { class: "panel-form workspace-branch-form", on: { submit: (event) => { event.preventDefault(); branchRequest("preview"); } } },
      h("div", { class: "field" }, h("label", { for: "workspace-branch-target" }, `Switch from ${currentName || "this branch"} to`), select),
      h("button", { type: "submit", class: "button", disabled: busy || !branchTarget() }, branch.busy === "preview" ? "Reading…" : "Preview switch"));
    if (branch.error && !branch.unknown) evidence.append(h("div", { class: "message message-warn", role: "alert" }, h("div", {}, h("strong", {}, branch.error),
      h("p", {}, workInTheWay(branch.diagnostic) || "Nothing was switched."),
      branch.diagnostic ? h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", {}, branch.diagnostic)) : null)));
    if (branch.busy === "preview") evidence.append(h("p", { class: "note aside-reading", role: "status" }, "Reading what switching does"));
    if (branch.preview) {
      const preview = branch.preview;
      const pending = preview.pending_work;
      const to = shortRef(preview.target.display);
      const from = shortRef(preview.source?.display) || currentName || "this branch";
      const held = state.branchGuides.get(`${from}..${to}`)?.data?.range;
      const where = held && isCount(held.changes_total) ? standingWords(held.changes_total, isCount(held.base_changes_total) ? held.base_changes_total : null, from) : "";
      const standing = !where ? "" : where.startsWith("No work") ? `${to} has ${where.replace(/^No/, "no")}.` : `${to} is ${where}.`;
      const checkbox = h("input", { type: "checkbox", id: "workspace-branch-confirm", checked: branch.confirmed, disabled: confirmLocked, on: { change: (event) => {
        branch.confirmed = event.target.checked; byId("workspace-branch-submit").disabled = !branch.confirmed || confirmLocked;
      } } });
      const own = [text(pending.detail), ...array(preview.limitations).filter((line) => typeof line === "string")].filter(Boolean);
      evidence.append(h("div", { class: "preview" }, h("h3", {}, `Switch from ${from} to ${to}`),
        h("p", { class: "preview-sentence" }, `Your code moves to ${to}'s latest change.`, standing ? ` ${standing}` : ""),
        h("div", { class: `message message-${pending.present ? "warn" : "good"}` }, h("div", {}, h("strong", {}, pending.present ? "Your uncommitted work comes along" : "Nothing is uncommitted"), h("p", {}, carriedWords(pending.present, to)))),
        h("p", { class: "note" }, "Kin checks again when you confirm. If anything changed since this preview, nothing is switched and you can preview again."),
        h("details", { class: "more" }, h("summary", {}, "Show Kin's own words"), own.length ? h("ul", { class: "limits" }, own.map((line) => h("li", {}, line))) : null,
          h("dl", { class: "heads" }, detail(`${to}'s latest change`, mergeHash(preview.target.change), true))),
        h("div", { class: "confirm" },
          h("label", { class: "check", for: "workspace-branch-confirm" }, checkbox, h("span", {}, pending.present ? `I've checked what comes along to ${to}.` : `Switch to ${to}.`)),
          button(branch.busy === "confirm" ? "Switching…" : `Switch to ${to}`, () => branchRequest("confirm"), { id: "workspace-branch-submit", class: "button button-primary", disabled: confirmLocked || !branch.confirmed }))));
    }
    return wrap(h("p", {}, "Preview what a switch does to your uncommitted work, then confirm."), form,
      state.merge.unknown || state.merge.busy ? h("p", { class: "note" }, "Finish checking the pending merge before switching.") : null);
  }

  const conflictHash = (value) => typeof value === "string" && /^[0-9a-f]{64}$/.test(value) ? value : mergeHash(value);
  function conflictPanel() {
    const resource = read("conflicts");
    const held = resource.data;
    const wrap = (...children) => h("section", { class: "card panel", id: "workspace-conflicts", "aria-labelledby": "conflict-title" }, h("h2", { id: "conflict-title" }, "Merge in progress"), ...children);
    if (!held) return wrap(resource.error ? h("div", { class: "section" }, readFailure("conflicts")) : skeletonLines(2));
    if (held.schema !== "kin.workspace.conflicts.v1" || !held.report || !held.state) return wrap(h("p", {}, "The merge record could not be read. Resolving is unavailable."));
    const report = held.report;
    const active = held.state.active === true;
    if (!report.record) return null;
    const labels = { none: "No merge waiting", in_progress: "Needs resolution", committed: "Resolved", aborted: "Abandoned" };
    const content = [h("p", {}, pill(labels[held.state.status] || "Unknown state", active ? "warn" : ""), " ",
      active ? `${report.unresolved_count} unresolved, ${report.resolved_count} resolved. The branches are not merged yet.` : "The last merge's record, kept for reference."),
      h("details", { class: "more" }, h("summary", {}, "Show details"), h("dl", { class: "heads" },
        detail("Record", report.record_hash, true), detail("Target branch", mergeRef(report.record.binding?.target_ref), true), detail("Incoming branch", mergeRef(report.record.binding?.source_ref), true), detail("Common base", conflictHash(held.provenance?.base_change), true),
        detail("Target input", conflictHash(held.provenance?.ours_change), true), detail("Source input", conflictHash(held.provenance?.theirs_change), true)))];
    const bodies = array(report.bodies);
    const canSettle = session()?.capabilities?.conflict_settle === true && Boolean(session()?.token);
    const commandsOffered = array(held.handoff?.commands).filter((command) => command && typeof command.id === "string");
    const settleLocked = Boolean(state.settle.busy) || state.settle.unknown || Boolean(resource.error) || workspaceWriteBusy();
    for (const entry of array(report.record.entries)) {
      const subject = entry.subject || {};
      const id = subject.entity || subject.relation || subject.artifact;
      const body = bodies.find((row) => row.subject === `${subject.subject} ${id}`);
      const kind = { entity: "Entity", artifact: "Unparsed entity", relation: "Connection", path: "Location" }[subject.subject] || "Conflict";
      const title = subject.subject === "entity" || subject.subject === "relation" ? (entry.label || body?.label || kind) : kind;
      const resolved = entry.resolution?.resolution && entry.resolution.resolution !== "unresolved";
      const card = h("details", { class: "conflict-entry" }, h("summary", {}, h("strong", {}, title), pill(resolved ? "Resolved" : "Unresolved", resolved ? "good" : "warn")),
        h("p", { class: "note" }, `${kind} · ${String(entry.divergence?.divergence || "reason not given").replaceAll("_", " ")}`));
      if (subject.subject === "entity" && typeof id === "string" && UUID.test(id)) card.append(h("a", { class: "button button-small", href: entityHref(id) }, "Open the current entity"));
      const choices = canSettle && !resolved && typeof id === "string" ? commandsOffered.filter((command) => typeof command.selector === "string" && command.selector.includes(id)) : [];
      if (choices.length) card.append(h("div", { class: "page-actions conflict-choices" }, choices.map((command) =>
        button(state.settle.busy === command.id ? "Working…" : SETTLE_WORDS[text(command.id).split(":")[0]] || command.label || "Choose", () => settleConflict(command), { class: "button button-small", disabled: settleLocked }))));
      if (choices.length) card.open = true;
      if (body) {
        const sides = h("div", { class: "conflict-sides" });
        for (const [side, label] of [["base", "Common base"], ["ours", "Target"], ["theirs", "Incoming"]]) {
          const unverified = array(body.unverified).includes(side);
          const truncated = array(held.display?.truncations).some((row) => row.subject === body.subject && row.side === side);
          sides.append(h("section", {}, h("h4", {}, label),
            unverified ? h("p", { class: "note" }, "This side could not be checked.") : typeof body[side] === "string" ? h("pre", { tabindex: "0", "aria-label": `${title}: ${label}` }, body[side]) : h("p", { class: "note" }, "Not on this side."),
            truncated ? h("p", { class: "note" }, "Shortened for display.") : null));
        }
        card.append(sides);
      }
      content.push(card);
    }
    if (state.settle.error) content.push(h("div", { class: `message message-${state.settle.unknown ? "warn" : "bad"}`, role: "alert" }, h("div", {},
      h("strong", {}, state.settle.unknown ? "That step's outcome is unknown" : "That step was not taken"), h("p", {}, state.settle.error),
      state.settle.unknown ? h("p", {}, "This page will not send it again. It shows the merge as Kin holds it now.") : null)));
    for (const unavailable of array(held.handoff?.unavailable_subjects)) content.push(h("div", { class: "message message-warn" }, h("div", {}, h("strong", {}, "This conflict can't be settled from here yet"), h("p", {}, unavailable.reason))));
    if (active && canSettle) {
      const finish = commandsOffered.find((command) => command.id === "continue");
      const abandon = commandsOffered.find((command) => command.id === "abort");
      content.push(h("div", { class: "confirm" },
        h("span", { class: "check" }, finish ? "Each conflict is settled. Finishing completes the merge." : `${plural(report.unresolved_count || 0, "conflict")} left to settle before the merge can finish.`),
        h("div", { class: "page-actions" },
          abandon ? button(state.settle.busy === "abort" ? "Working…" : "Abandon the merge", () => settleConflict(abandon), { class: "button button-quiet", disabled: settleLocked }) : null,
          finish ? button(state.settle.busy === "continue" ? "Working…" : "Finish the merge", () => settleConflict(finish), { class: "button button-primary", disabled: settleLocked }) : null)));
    } else if (active) content.push(h("p", { class: "note quiet-line" }, "This version of Kin can't settle a merge from this page."));
    return wrap(...content);
  }

  const SETTLE_WORDS = { ours: "Keep the target side", theirs: "Take the incoming side", base: "Go back to the common base", remove: "Remove it from the merge" };
  /** Take one step in settling a merge: a choice for one conflict, finishing, or abandoning. Kin offers the steps; the page names one back. */
  async function settleConflict(command) {
    const settle = state.settle;
    const held = read("conflicts").data;
    if (settle.busy || settle.unknown || !held?.report?.record_hash) return;
    if (!globalThis.crypto?.randomUUID) { settle.error = "This browser could not create an operation ID. Reload the page before settling."; paint("branches-conflicts"); return; }
    settle.busy = command.id; settle.error = null;
    paint("branches-conflicts");
    try {
      const result = await send("/api/workspace/conflicts/settle", { command: command.id, record_hash: held.report.record_hash, operation_id: crypto.randomUUID() });
      if (result.details?.schema === "kin.workspace.conflicts.v1") { read("conflicts").data = result.details; read("conflicts").sig = null; }
      announce(result.outcome === "continued" ? "The merge is done." : result.outcome === "aborted" ? "The merge was abandoned." : "Settled.");
    } catch (error) {
      settle.error = error.remedy || error.message || "Kin could not take that step.";
      settle.unknown = !(error.status === 409 && error.outcome === "refused") && ![400, 403, 404, 405, 422, 501].includes(error.status);
    } finally {
      settle.busy = null;
      paint("branches-conflicts");
      refresh(["conflicts", "branches", "branchState", "changes", "standing", "working"]);
    }
  }

  /**
   * What a branch holds beyond the current one, read as the guided review
   * reads a range. Read for the first few branches only, once each, when the
   * branch list is shown.
   */
  async function loadBranchGuide(name, base) {
    const key = `${base}..${name}`;
    const held = state.branchGuides.get(key) || {};
    if (held.busy || held.data) return;
    state.branchGuides.set(key, { busy: true });
    const epoch = state.readEpoch;
    try {
      const data = await fetchJSON(`/api/guide?base=${encodeURIComponent(base)}&change=${encodeURIComponent(name)}`);
      if (epoch !== state.readEpoch) return;
      state.branchGuides.set(key, { data });
    } catch (error) {
      if (epoch !== state.readEpoch) return;
      state.branchGuides.set(key, { error: error.message || "This branch could not be read." });
    }
    paint("branches-list");
    repaintPanels();
  }

  function branchSummary(row, base, index) {
    if (!base || index >= 4) return null;
    const key = `${base}..${row.name}`;
    const held = state.branchGuides.get(key);
    if (!held) { Promise.resolve().then(() => loadBranchGuide(row.name, base)); return h("div", { class: "branch-detail" }, h("span", { class: "note aside-reading" }, `Reading what ${row.name} changed`)); }
    if (held.busy) return h("div", { class: "branch-detail" }, h("span", { class: "note aside-reading" }, `Reading what ${row.name} changed`));
    if (held.error && isSlow(held.error)) {
      setTimeout(() => { if (state.branchGuides.get(key) === held) { state.branchGuides.delete(key); paint("branches-list"); } }, 4000);
      return h("div", { class: "branch-detail" }, h("span", { class: "note aside-reading" }, `Still reading what ${row.name} changed`));
    }
    if (held.error) return h("div", { class: "branch-detail" }, failed("Kin couldn't load what this branch holds.", () => { state.branchGuides.delete(key); paint("branches-list"); }, held.error));
    const guide = held.data;
    const range = guide.range || {};
    const changes = array(range.changes);
    const latest = changes[changes.length - 1] || changes[0];
    const ahead = isCount(range.changes_total) ? range.changes_total : changes.length;
    // With no change of its own, what differs is only what the current branch moved on to, which is not this branch's work.
    const sentence = ahead ? workSentence(guide) || "No entity changed." : "";
    return h("div", { class: "branch-detail" },
      sentence ? h("p", { class: "branch-sentence" }, sentence) : null,
      h("p", { class: "branch-meta" }, standingWords(ahead, isCount(range.base_changes_total) ? range.base_changes_total : null, base),
        latest ? [" · ", latest.title || "Untitled change", [authorOf(latest), ago(latest.timestamp)].filter(Boolean).length ? ` · ${[authorOf(latest), ago(latest.timestamp)].filter(Boolean).join(", ")}` : ""] : null));
  }

  /**
   * Where a branch stands against another, from both sides of where they
   * split: what it holds that the other does not, and what the other holds
   * that it does not. A side Kin did not count is not said to be even.
   */
  function standingWords(ahead, behind, base) {
    if (ahead && behind) return `${plural(ahead, "change")} ahead of ${base}, ${count(behind)} behind`;
    if (ahead) return `${plural(ahead, "change")} ahead of ${base}`;
    // A branch that holds nothing the other lacks has no work of its own, whatever it is missing.
    if (behind) return `No work of its own, ${plural(behind, "change")} behind ${base}`;
    return behind === 0 ? `No work of its own, even with ${base}` : `No work of its own beyond ${base}`;
  }

  /** How many changes a branch holds beyond another, once Kin has read it; null until then. */
  function branchAhead(row, base) {
    const guide = state.branchGuides.get(`${base}..${row.name}`)?.data;
    if (!guide) return null;
    const range = guide.range || {};
    return isCount(range.changes_total) ? range.changes_total : array(range.changes).length;
  }

  function branchList() {
    const rows = branchRows();
    if (!read("branches").data) return read("branches").error ? readFailure("branches") : skeletonLines(2);
    if (!rows.length) return h("p", { class: "gap" }, "No branches yet.");
    const others = branchChoices().filter((row) => !row.active);
    const current = shortRef(activeBranch()?.name);
    const ordered = [...rows.filter((row) => row.active), ...rows.filter((row) => !row.active)];
    let index = 0;
    return h("ul", { class: "list list-card" }, ordered.map((row) => {
      const choice = others.find((other) => shortRef(mergeRef(other.name)) === row.name);
      const detail = row.active ? null : branchSummary(row, current, index++);
      // A branch with work of its own opens as that work, against the branch you are on.
      const opens = !row.active && current && branchAhead(row, current) !== 0;
      const name = [icon("branch"), h("span", { class: "branch-label" }, row.name), row.active ? pill("Current", "accent") : null, row.default ? pill("Default") : null];
      return h("li", { class: `branch-row${opens ? " branch-row-opens" : ""}` },
        h("div", { class: "branch-main" },
          opens ? h("a", { class: "branch-name branch-open", href: rangeHref(current, row.name), title: `What ${row.name} changed, against ${current}` }, name) : h("span", { class: "branch-name" }, name),
          detail),
        row.active ? null : h("span", { class: "branch-actions" },
          // Offered before the branch list's exact names are read; the panel
          // then shows that read, or its failure, until it can preview.
          session()?.capabilities?.branch_switch ? button("Switch", () => {
            state.switchOpen = true;
            if (choice) { state.branch.target = choice.name.bytes_hex; updateBranchPanel(); branchRequest("preview"); } else updateBranchPanel();
            byId("workspace-branch-host")?.scrollIntoView?.({ block: "start", behavior: "smooth" });
          }, { class: "button button-small", disabled: workspaceWriteBusy() }) : null,
          branchAhead(row, current) !== 0 ? button(`Merge into ${current || "current"}`, () => { state.mergeOpen = true; state.merge.source = row.name; updateMergePanel(); mergeRequest("preview"); byId("workspace-merge-host")?.scrollIntoView?.({ block: "start", behavior: "smooth" }); }, { class: "button button-small button-quiet", disabled: workspaceWriteBusy() || !session()?.token }) : null));
    }));
  }

  async function createBranch() {
    const form = state.newBranch;
    const name = form.name.trim();
    if (!name || form.busy || workspaceWriteBusy()) return;
    if (!globalThis.crypto?.randomUUID) { form.error = "This browser could not create an operation ID. Reload the page before creating a branch."; paint("branches-new"); return; }
    form.busy = true; form.error = null;
    paint("branches-new");
    try {
      await send("/api/workspace/branch/create", { name, operation_id: crypto.randomUUID() });
      // Where it starts is the branch it was made from, said the same way after a switch.
      form.created = name; form.createdFrom = shortRef(activeBranch()?.name) || null; form.name = ""; form.open = false;
      announce(`Branch ${name} created.`);
    } catch (error) {
      form.error = error.remedy || error.message || "The branch was not created.";
    } finally {
      form.busy = false;
      paint("branches-new"); paint("branches-actions");
      if (form.created) refresh(["branches", "branchState"]);
    }
  }

  function newBranchForm() {
    const form = state.newBranch;
    const from = shortRef(activeBranch()?.name) || "the current branch";
    if (!form.open) {
      if (!form.created) return null;
      const started = form.createdFrom ? `${form.createdFrom}'s latest change` : "the latest change of the branch you were on";
      const here = shortRef(activeBranch()?.name) === form.created;
      const choice = branchChoices().find((row) => shortRef(mergeRef(row.name)) === form.created && !row.active);
      return h("div", { class: "message message-good section-tight", role: "status", id: "branch-created" },
        h("div", {}, h("strong", {}, `Created ${form.created}`), h("p", {}, here ? `You're on it now. It started at ${started}.` : `It starts at ${started}. You're still on ${shortRef(activeBranch()?.name) || "the branch you were on"}.`)),
        h("div", { class: "page-actions" },
          !here && choice && can("branch_switch") ? button(`Switch to ${form.created}`, () => { state.switchOpen = true; state.branch.target = choice.name.bytes_hex; updateBranchPanel(); branchRequest("preview"); byId("workspace-branch-host")?.scrollIntoView?.({ block: "start", behavior: "smooth" }); }, { class: "button button-small", disabled: workspaceWriteBusy() }) : null,
          button("Dismiss", () => { form.created = null; paint("branches-new"); }, { class: "button button-small button-quiet" })));
    }
    const submit = h("button", { type: "submit", id: "branch-new-submit", class: "button button-primary", disabled: form.busy || !form.name.trim() }, form.busy ? "Creating…" : "Create branch");
    const input = h("input", { id: "branch-new-name", type: "text", value: form.name, placeholder: "feat/short-name", autocomplete: "off", spellcheck: "false", autocapitalize: "off", maxlength: "200", disabled: form.busy,
      on: { input: (event) => { form.name = event.target.value; submit.disabled = form.busy || !form.name.trim(); } } });
    return h("form", { class: "compare section-tight", "aria-label": "New branch", on: { submit: (event) => { event.preventDefault(); createBranch(); } } },
      h("div", { class: "field" }, h("label", { for: "branch-new-name" }, `New branch, starting at ${from}'s latest change`), input),
      submit,
      button("Cancel", () => { form.open = false; form.error = null; paint("branches-new"); paint("branches-actions"); }, { class: "button button-quiet", disabled: form.busy }),
      form.error ? h("div", { class: "message message-bad compare-wide", role: "alert" }, h("div", {}, h("strong", {}, "The branch was not created"), h("p", {}, form.error))) : null);
  }

  function branches() {
    return [
      workHead(
          section("branches-actions", ["session", "branches"], () => [
            branchRows().length > 1 ? h("button", { type: "button", class: "button button-quiet", id: "branches-compare-toggle", "aria-expanded": String(Boolean(state.compareOpen)), on: { click: () => { state.compareOpen = !state.compareOpen; paint("branches-compare"); paint("branches-actions"); if (state.compareOpen) byId("workspace-range-base")?.focus(); } } }, "Compare two branches") : null,
            session()?.capabilities?.branch_create === true && session()?.token && !state.newBranch.open
              ? button("New branch", () => { state.newBranch.open = true; state.newBranch.created = null; paint("branches-new"); paint("branches-actions"); byId("branch-new-name")?.focus(); }) : null], { class: "page-actions" })),
      section("branches-lede", ["branches"], () => {
          if (!read("branches").data) return read("branches").error ? null : skeleton("skeleton-lede");
          const active = activeBranch();
          const rows = branchRows();
          // A branch name is one word to the reader, so a line never breaks inside it at a hyphen or a slash.
          const named = (name) => h("span", { class: "nobreak", title: name }, name);
          const others = rows.filter((row) => !row.active).map((row) => row.name);
          const which = others.length === 1 ? ["One other branch: ", named(others[0]), "."]
            : others.length <= 3 ? [`${spoken(others.length)} other branches: `, others.slice(0, -1).flatMap((name, at) => [at ? ", " : "", named(name)]), " and ", named(others.at(-1)), "."] : [`${spoken(others.length)} other branches.`];
          return h("p", { class: "lede" }, active ? (others.length ? ["You're on ", named(active.name), ". ", which] : ["You're on ", named(active.name), ", the only branch."]) : `${spoken(rows.length)} ${rows.length === 1 ? "branch" : "branches"}.`);
        }, { class: "tab-lede" }),
      section("branches-new", ["branches", "branchState"], newBranchForm),
      section("branches-compare", ["branches"], () => state.compareOpen ? compareForm() : null),
      h("section", { class: "section-tight", "aria-label": "Branches" }, section("branches-list", ["branches", "branchState"], branchList)),
      section("branches-review", ["branches"], () => {
        const active = activeBranch();
        return active && defaultBranch() && active.name !== defaultBranch().name ? h("p", { class: "note quiet-line" }, moreLink(`Review ${active.name} against ${defaultBranch().name}`, rangeHref(defaultBranch().name, active.name))) : null;
      }),
      h("div", { id: "workspace-branch-host" }, branchPanel()),
      h("div", { id: "workspace-merge-host" }, mergePanel()),
      section("branches-conflicts", ["conflicts"], conflictPanel),
      section("branches-remotes", ["remotes", "branches", "session"], remotesPanel)];
  }

  // ── Frame ────────────────────────────────────────────────────────────────

  const THEMES = ["auto", "light", "dark"];
  const THEME_WORD = { auto: "Auto", light: "Light", dark: "Dark" };
  function applyTheme(theme) {
    if (theme === "auto") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", theme);
    const control = byId("workspace-theme");
    if (control) {
      control.textContent = THEME_WORD[theme];
      control.setAttribute("aria-label", `Colour theme: ${THEME_WORD[theme]}. Change it.`);
      control.dataset.theme = theme;
    }
    try { localStorage.setItem("kin.review.theme", JSON.stringify(theme)); } catch (_) { /* The choice still holds on this page. */ }
  }

  /** How many entities are uncommitted, as the review of that work counts them. */
  function uncommittedCount() {
    const guide = read("working").data;
    return workChanged(guide) || (guide?.working_tree?.differs ? null : 0);
  }

  /** The repositories the sidebar lists: the machine's own list when it was read, otherwise the names this session carries. */
  function sideRepositories() {
    const rows = homeRows();
    if (rows.length) return rows;
    const listed = array(session()?.repositories);
    if (listed.length) return listed;
    return !MACHINE && session()?.repository ? [{ ...session().repository, url: `${apiBase}/workspace/` }] : [];
  }

  /**
   * The sidebar is the map of the machine: every repository, with a dot for
   * what is in motion in it, and the open one unfolded into its pages. The
   * page beside it carries the detail; the sidebar only says where things are.
   */
  function paintSide() {
    const nav = byId("workspace-nav");
    if (!nav) return;
    const held = session();
    const openKey = MACHINE ? null : text(held?.repository?.key);
    const current = state.route === "entity" ? "explore" : ["review", "history", "branches"].includes(state.route) ? "uncommitted" : state.route;
    const page = (route, label, glyph, extra) => h("a", { class: "nav-item", href: `#/${route}`, "data-route": route, "aria-current": route === current ? "page" : undefined }, icon(glyph), h("span", { class: "nav-label" }, label), extra);
    const changed = uncommittedCount();
    const working = array(read("agents").data?.agents).filter((agent) => agent.state === "active").length;
    const pages = () => h("div", { class: "nav repo-pages" },
      page("uncommitted", "Work", "uncommitted", changed === null ? h("span", { class: "nav-count nav-count-work", "aria-label": "uncommitted work" }, "•") : changed ? h("span", { class: "nav-count nav-count-work", "aria-label": `${changed} uncommitted` }, count(changed)) : null),
      page("agents", "Agents", "agents", working ? h("span", { class: "nav-count nav-count-live", "aria-label": `${working} working now` }, count(working)) : null),
      page("explore", "Code", "code"));
    // The ones with something in motion first, then the rest, as many as fit. Opening a
    // repository does not move it, so the list holds still under the pointer.
    const all = sideRepositories();
    const SHOWN = 8;
    // Until the machine's list is read, each repository is said as this tab last saw it, so the list
    // a page opens with is the one the reader just clicked in.
    const listRead = homeRows().length > 0;
    const kept = keptTones();
    const toneOf = (row) => {
      // The open repository is said from its own reads once they land, and as the list says before then.
      if (!openKey || row.key !== openKey || !read("working").data) return listRead || !(row.key in kept) ? repoTone(row) : kept[row.key];
      if (changed !== 0 || row.merge_in_progress) return "work";
      if (awaitingReview(row).length) return "review";
      return working ? "live" : "";
    };
    if (listRead) keepTones(Object.fromEntries(all.map((row) => [row.key, toneOf(row)])));
    const sorted = [...all].sort((a, b) => TONE_RANK[toneOf(a)] - TONE_RANK[toneOf(b)] || repositoryLabel(a).localeCompare(repositoryLabel(b)));
    const listed = sorted.slice(0, SHOWN);
    // The open one is always listed, in its own place among the rest.
    if (openKey && !listed.some((row) => row.key === openKey)) { const open = sorted.find((row) => row.key === openKey); if (open) listed.splice(SHOWN - 1, 1, open); }
    const repository = (row) => {
      const isOpen = Boolean(openKey) && row.key === openKey;
      const tone = toneOf(row);
      const branch = isOpen ? text(activeBranch()?.name) : text(row.branch);
      return [h("a", { class: `repo-item${isOpen ? " is-open" : ""}`, href: isOpen ? UNCOMMITTED : safeLocalURL(row.url, "/"), title: repositoryLabel(row, " / ") },
        h("span", { class: "repo-dot", "data-state": tone || undefined, "aria-hidden": "true" }),
        h("span", { class: "repo-item-name" }, repositoryTitle(row)),
        branch ? h("span", { class: "repo-item-branch" }, branch) : null),
        isOpen ? pages() : null];
    };
    nav.replaceChildren(...[
      h("div", { class: "nav" },
        h("a", { class: "nav-item", href: MACHINE ? "#/" : "/", "aria-current": MACHINE && state.route === "hub" ? "page" : undefined }, icon("home"), h("span", { class: "nav-label" }, "Home")),
        MACHINE && !read("clients").missing ? h("a", { class: "nav-item", href: "#/agents", "aria-current": state.route === "clients" ? "page" : undefined }, icon("agents"), h("span", { class: "nav-label" }, "Coding agents")) : null),
      h("div", { class: "nav-head" },
        h("span", { class: "nav-title" }, "Repositories"),
        h("a", { class: "icon-button", href: MACHINE ? "#/add" : "/#/add", "aria-label": "Add a repository", title: "Add a repository" }, icon("plus"))),
      h("div", { class: "nav" }, listed.map(repository),
        all.length > listed.length ? h("a", { class: "nav-item nav-item-more", href: "/" }, h("span", { class: "nav-label" }, `All ${count(all.length)} repositories`)) : null),
      ].filter(Boolean));
  }

  function updateChrome() {
    const held = session();
    const failedReads = Object.entries(state.reads).filter(([, resource]) => resource.error);
    const status = byId("workspace-status");
    const words = byId("workspace-status-text");
    const latest = Math.max(0, ...Object.values(state.reads).map((resource) => resource.at || 0));
    if (status) status.dataset.state = state.refreshing ? "loading" : !held ? (read("session").error ? "error" : "loading") : failedReads.length ? "stale" : "ready";
    if (words) words.textContent = state.refreshing ? "Loading" : !held ? (read("session").error ? "Not connected" : "Loading") : failedReads.length ? "Some parts didn't load" : `Up to date ${clock(latest || Date.now())}`;
    paintSide();
    if (!held) return;
    byId("workspace-version").textContent = held.version ? `Kin ${held.version}` : "Kin";
    if (MACHINE) { document.title = `${TITLES[state.route] || "Home"} · Kin`; return; }
    const name = repositoryLabel(held.repository);
    if (state.route !== "entity" || !state.entity.data) document.title = `${TITLES[state.route] || "Uncommitted"} · ${name}`;
  }

  // ── Remotes ──────────────────────────────────────────────────────────────
  // Where this repository stands against the places it is shared: what a push
  // would send and what a pull would bring, read without moving anything.

  async function moveRemote(kind, remote) {
    const move = state.remote;
    if (move.busy || move.unknown || workspaceWriteBusy() || state.commit.busy || state.restore.busy || !globalThis.crypto?.randomUUID) return;
    move.busy = `${kind}:${remote.name}`; move.error = null; move.result = null;
    move.operationId = crypto.randomUUID();
    rememberWorkReceipt("remote", move.operationId);
    paint("branches-remotes");
    try {
      move.result = { kind, remote: remote.name, ...(await send(`/api/workspace/${kind}`, { remote: remote.name, operation_id: move.operationId })) };
      forgetWorkReceipt("remote");
      announce(move.result.outcome === "pushed" ? "Pushed." : move.result.outcome === "pulled" ? "Pulled." : move.result.outcome === "fetched" ? "Fetched for review." : kind === "push" ? "Nothing to push." : kind === "fetch" ? "Nothing to fetch." : "Nothing to pull.");
    } catch (error) {
      // Why it did not happen is the server's own sentence; the general advice that follows a refusal adds nothing to it.
      move.error = error.remedy || error.message || (kind === "push" ? "Nothing was pushed." : kind === "fetch" ? "Nothing was fetched." : "Nothing was pulled.");
      // A refusal moved nothing. Anything else may have landed, so this page does not send it again.
      move.unknown = !(error.status === 409 && error.outcome === "refused") && ![400, 403, 404, 405, 422, 501].includes(error.status);
      if (!move.unknown) forgetWorkReceipt("remote");
    } finally {
      move.busy = null;
      paint("branches-remotes");
      state.branchGuides.clear();
      refresh(["remotes", "branches", "branchState", "changes", "working", "flags"]);
    }
  }

  function previewRemoteMerge(branch) {
    if (!text(branch) || workspaceWriteBusy() || state.remote.busy || state.remote.unknown) return;
    state.mergeOpen = true; state.merge.source = branch;
    updateMergePanel(); mergeRequest("preview");
    byId("workspace-merge-host")?.scrollIntoView?.({ block: "start", behavior: "smooth" });
  }

  /** What moved, in changes and entities, as the answer counts them. */
  function movedWords(result) {
    const changes = isCount(result.changes) ? plural(result.changes, "change") : "";
    const entities = isCount(result.entities) && result.entities ? `touching ${plural(result.entities, "entity", "entities")}` : "";
    return [changes, entities].filter(Boolean).join(" ");
  }

  function remotesPanel() {
    const resource = read("remotes");
    if (resource.missing) return null;
    if (!resource.data) return resource.error ? h("section", { class: "section" }, readFailure("remotes")) : null;
    const remotes = array(resource.data.remotes).filter((remote) => remote && typeof remote.name === "string");
    const move = state.remote;
    const head = h("div", { class: "section-head" }, h("h2", {}, "Remotes", remotes.length ? h("span", { class: "count" }, count(remotes.length)) : null));
    if (!remotes.length) return h("section", { class: "section" }, head, h("p", { class: "gap" }, "This repository is not shared anywhere yet."));
    const done = move.result;
    const doneTitle = !done ? "" : done.outcome === "pushed" ? `Pushed to ${done.remote}` : done.outcome === "pulled" ? `Pulled from ${done.remote}` : done.outcome === "fetched" ? `Fetched from ${done.remote}` : done.kind === "push" ? `Nothing to push to ${done.remote}` : done.kind === "fetch" ? `Nothing to fetch from ${done.remote}` : `Nothing to pull from ${done.remote}`;
    return h("section", { class: "section" }, head,
      resource.error ? readFailure("remotes") : null,
      move.unknown ? h("div", { class: "message message-warn section-tight", role: "alert" }, h("div", {}, h("strong", {}, "Kin stopped answering"), h("p", {}, "It may have finished. This page will not send it again. The remotes below show where things stand now.")),
        h("div", { class: "page-actions" }, button("Read the remotes again", () => refresh(["remotes", "branches", "changes", "working"]), { class: "button button-small" }),
          button("I've checked", () => { move.unknown = false; move.error = null; forgetWorkReceipt("remote"); paint("branches-remotes"); }, { class: "button button-small button-quiet", disabled: Boolean(resource.error || resource.busy) })), operationDetails(move.operationId)) : null,
      move.error && !move.unknown ? h("div", { class: "message message-bad section-tight", role: "alert" }, h("div", {}, h("strong", {}, "Nothing moved"), h("p", {}, move.error)),
        button("Dismiss", () => { move.error = null; paint("branches-remotes"); }, { class: "button button-small button-quiet" })) : null,
      done ? h("div", { class: `message message-${done.outcome === "nothing_to_send" ? "warn" : "good"} section-tight`, role: "status" }, h("div", {}, h("strong", {}, doneTitle),
        movedWords(done) && done.outcome !== "nothing_to_send" ? h("p", {}, `${movedWords(done)}.`) : text(done.summary) ? h("p", {}, done.summary) : null,
        done.kind === "fetch" && text(done.into_branch) ? h("p", {}, `The remote's work is on ${done.into_branch}, ready to review and merge.`) : null,
        operationDetails(done.operation_id || move.operationId)),
        h("div", { class: "page-actions" }, done.kind === "fetch" && text(done.into_branch) && (can("merge_preview") || can("merge")) ? button("Preview merge", () => previewRemoteMerge(done.into_branch), { class: "button button-small" }) : null,
          button("Dismiss", () => { move.result = null; paint("branches-remotes"); }, { class: "button button-small button-quiet" }))) : null,
      h("ul", { class: "list list-card" }, remotes.map((remote) => {
        const standing = remote.standing || {};
        const git = remote.kind === "git";
        const since = standing.since_git;
        // A Git remote is where the project came from; Kin says what has changed here since, and what publishing there needs.
        const said = git ? [isCount(since?.changes) && since.changes ? `${plural(since.changes, "change")}${isCount(since.entities) && since.entities ? ` touching ${plural(since.entities, "entity", "entities")}` : ""} here since Kin last took from Git.` : "", text(remote.unavailable_reason)].filter(Boolean).join(" ")
          : text(standing.summary) || text(remote.unavailable_reason) || "";
        // A push or pull that has nothing to move, or that the server says cannot run, is not offered as if it could.
        const nothingToSend = ["nothing_to_send", "blocked"].includes(text(standing.push?.state));
        const nothingToBring = ["nothing_to_bring", "blocked"].includes(text(standing.pull?.state));
        const resolution = text(standing.pull?.resolve);
        const resolutionBranch = text(standing.pull?.resolve_branch);
        const busy = Boolean(resource.error || move.busy || move.unknown || workspaceWriteBusy() || state.commit.busy || state.restore.busy);
        return h("li", { class: "branch-row" },
          h("div", {}, h("div", { class: "branch-name" }, remote.name, h("span", { class: "pill" }, git ? "Git" : "Kin"), remote.default ? h("span", { class: "pill" }, "Default") : null,
            STANDING_WORD[text(standing.state)] ? h("span", { class: `state${["ahead", "behind"].includes(standing.state) ? " state-warn" : standing.state === "diverged" || standing.state === "blocked" ? " state-bad" : ""}` }, STANDING_WORD[standing.state]) : null),
            said ? h("div", { class: "branch-meta" }, said) : null,
            text(remote.pull_unavailable_reason) && !said.includes(text(remote.pull_unavailable_reason)) ? h("div", { class: "branch-meta" }, remote.pull_unavailable_reason) : null,
            ["fetch_then_merge", "merge"].includes(resolution) ? h("div", { class: "branch-meta" }, resolution === "fetch_then_merge" ? "Both sides have work. Bring the remote's branch in beside this one, then review and merge it." : "The remote's work is ready on a branch here. Preview its merge into this branch.") : null),
          h("div", { class: "branch-actions" },
            resolution === "fetch_then_merge" && remote.can_fetch && can("remote_fetch") ? button(move.busy === `fetch:${remote.name}` ? "Fetching…" : "Fetch for review", () => moveRemote("fetch", remote), { class: "button button-small button-primary", disabled: busy }) : null,
            resolution === "merge" && resolutionBranch && (can("merge_preview") || can("merge")) ? button("Preview merge", () => previewRemoteMerge(resolutionBranch), { class: "button button-small button-primary", disabled: busy }) : null,
            remote.can_pull && can("remote_pull") && !resolution ? button(move.busy === `pull:${remote.name}` ? "Pulling…" : "Pull", () => moveRemote("pull", remote), { class: `button button-small${standing.state === "behind" ? " button-primary" : ""}`, disabled: busy || nothingToBring })
              : remote.can_pull && !resolution && session()?.capabilities?.remote_pull_checking === true ? button("Checking…", () => {}, { class: "button button-small", disabled: true, title: "Kin is checking whether this branch can update from Git." }) : null,
            remote.can_push && can("remote_push") ? button(move.busy === `push:${remote.name}` ? "Pushing…" : "Push", () => moveRemote("push", remote), { class: `button button-small${standing.state === "ahead" ? " button-primary" : ""}`, disabled: busy || nothingToSend }) : null));
      })));
  }
  const STANDING_WORD = { up_to_date: "Up to date", ahead: "Ahead", behind: "Behind", diverged: "Diverged", blocked: "Blocked", unknown: "Not known" };

  /** On a narrow window the sidebar folds into one menu. */
  function setMenu(open) {
    state.menuOpen = Boolean(open);
    const toggle = byId("workspace-menu-toggle");
    toggle?.setAttribute("aria-expanded", String(state.menuOpen));
    const side = byId("workspace-side");
    if (side) side.dataset.menu = state.menuOpen ? "open" : "closed";
  }

  function render() {
    if (state.destroyed) return;
    clearSections();
    const main = byId("workspace-main");
    const page = (...children) => main.replaceChildren(h("div", { class: "page" }, children));
    if (!session()) {
      page(read("session").error
        ? [h("header", { class: "page-head" }, h("div", { class: "page-title-row" }, h("h1", {}, MACHINE ? "Kin isn't answering." : "This repository isn't answering."))), readFailure("session")]
        : h("div", { "aria-busy": "true" }, skeleton("skeleton-lede"), skeleton("skeleton-line")));
      return;
    }
    const views = { hub, clients, uncommitted, review, history, agents: () => agentsApi() ? agentsApi().view(agentsCtx) : [h("h1", {}, "Agents"), h("p", { class: "gap" }, "This page could not load its agents view.")], explore, entity, branches };
    page(views[state.route]().filter(Boolean));
  }

  function parseRoute(hash) {
    const raw = String(hash || "").replace(/^#\/?/, "");
    const question = raw.indexOf("?");
    const path = question < 0 ? raw : raw.slice(0, question);
    const query = question < 0 ? "" : raw.slice(question + 1);
    const params = {};
    for (const pair of query.split("&")) {
      if (!pair) continue;
      const equal = pair.indexOf("=");
      try { params[decodeURIComponent(equal < 0 ? pair : pair.slice(0, equal))] = decodeURIComponent((equal < 0 ? "" : pair.slice(equal + 1)).replace(/\+/g, " ")); } catch (_) { /* Ignore a malformed pair. */ }
    }
    const parts = path.split("/").filter(Boolean);
    let name = parts[0] || (MACHINE ? "hub" : "uncommitted");
    if (MACHINE) return { name: name === "agents" || name === "clients" ? "clients" : "hub", add: name === "add" };
    // Addresses the earlier workspace used still open the page that holds them now.
    if (["home", "overview", "work"].includes(name)) name = "uncommitted";
    if (name === "changes") name = "history";
    if (name === "repository") name = "branches";
    if (name === "review") {
      // review/w/3, review/w/done, review/c/<id>, review/c/<id>/3, review/c/<id>/done. The list of uncommitted work is its own page.
      let target = null;
      let rest = [];
      if (parts[1] === "w") { target = { kind: "working" }; rest = parts.slice(2); }
      else if (parts[1] === "c" && /^[\w-]{1,128}$/.test(parts[2] || "")) { target = { kind: "change", id: parts[2] }; rest = parts.slice(3); }
      else if (parts[1] === "r") {
        // A branch name may hold slashes, so the range is everything up to a trailing page number or "done".
        let tail = parts.slice(2);
        if (/^([1-9]\d{0,5}|done)$/.test(tail.at(-1) || "") && tail.length > 1) { rest = [tail.at(-1)]; tail = tail.slice(0, -1); }
        let range = "";
        try { range = decodeURIComponent(tail.join("/")); } catch (_) { range = ""; }
        const at = range.indexOf("..");
        if (at > 0 && at + 2 < range.length) target = { kind: "range", base: range.slice(0, at), head: range.slice(at + 2) };
      }
      const step = /^[1-9]\d{0,5}$/.test(rest[0] || "") ? Number(rest[0]) : null;
      const done = rest[0] === "done";
      if (!target) return { name: "history" };
      if (target.kind === "working" && !step && !done) return { name: "uncommitted" };
      return { name, target, step, done };
    }
    if (name === "entities") {
      let id = params.entity || parts[1] || "";
      try { id = decodeURIComponent(id); } catch (_) { id = ""; }
      return UUID.test(id) ? { name: "entity", id } : { name: "explore", q: params.q || "" };
    }
    if (name === "entity") {
      let id = parts[1] || "";
      try { id = decodeURIComponent(id); } catch (_) { id = ""; }
      return UUID.test(id) ? { name: "entity", id } : { name: "explore", q: "" };
    }
    if (name === "explore") return { name, q: params.q || "" };
    if (name === "agents") return AGENT_KEY.test(parts[1] || "") ? { name, agent: parts[1] } : { name };
    return ROUTES.includes(name) ? { name } : { name: "uncommitted" };
  }

  function routeChanged(initial = false) {
    const next = parseRoute(location.hash);
    if (MACHINE && next.add && !state.add.open) { state.add.open = true; if (!initial) { paint("hub-add"); paint("hub-add-toggle"); byId("add-path")?.focus(); } }
    if (!initial && next.name === state.route && (next.name !== "entity" || next.id === state.params.id) && (next.name !== "agents" || next.agent === state.params.agent) && !["explore", "review"].includes(next.name)) return;
    if (!initial && next.name === "review" && state.route === "review") {
      state.params = next;
      if (next.target) loadReview(next.target);
      updateChrome(); paint("review-head"); paint("review-body");
      byId("workspace-main").focus({ preventScroll: true }); globalThis.scrollTo?.(0, 0);
      return;
    }
    if (!initial && next.name === "explore" && state.route === "explore") {
      if ((next.q || "") !== state.explore.query) { state.explore.query = next.q || ""; const input = byId("explore-input"); if (input) input.value = state.explore.query; runAsk(state.explore.query, true); }
      return;
    }
    state.route = next.name;
    state.params = next;
    setMenu(false);
    if (next.name === "explore") { state.explore.query = next.q || ""; state.explore.results = null; state.explore.active = -1; }
    updateChrome();
    render();
    if (next.name === "explore" && state.explore.query.trim() && session()) runAsk(state.explore.query, true);
    if (next.name === "entity" && session()) loadEntity(next.id);
    if (next.name === "review" && next.target && session()) loadReview(next.target);
    if (!initial) { byId("workspace-main").focus({ preventScroll: true }); globalThis.scrollTo?.(0, 0); }
  }

  let initialTheme = "auto";
  try { const saved = JSON.parse(localStorage.getItem("kin.review.theme")); if (THEMES.includes(saved)) initialTheme = saved; } catch (_) { /* The system theme is the fallback. */ }
  applyTheme(initialTheme);
  byId("workspace-theme")?.addEventListener("click", () => {
    const current = byId("workspace-theme").dataset.theme || "auto";
    applyTheme(THEMES[(THEMES.indexOf(current) + 1) % THEMES.length]);
  });
  byId("workspace-refresh")?.addEventListener("click", () => refresh());
  byId("workspace-menu-toggle")?.addEventListener("click", () => setMenu(!state.menuOpen));
  const openAsk = () => {
    if (MACHINE) return;
    if (state.route !== "explore") location.hash = "#/explore";
    setTimeout(() => { byId("explore-input")?.focus(); byId("explore-input")?.select?.(); }, 0);
  };
  globalThis.addEventListener("hashchange", () => routeChanged());
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && state.menuOpen) { setMenu(false); byId("workspace-menu-toggle")?.focus(); return; }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); if (MACHINE) { byId("home-ask-input")?.focus(); byId("home-ask-input")?.select?.(); } else openAsk(); return; }
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.target?.closest?.("input, textarea, select, [contenteditable=true]")) return;
    if (event.key === "/") { event.preventDefault(); if (MACHINE) byId("home-ask-input")?.focus(); else openAsk(); return; }
    if (state.route === "review" && state.params.step) {
      const press = { ArrowRight: "review-next", ArrowLeft: "review-previous", Enter: "review-fine", f: "review-flag" }[event.key];
      if (event.key === "Enter" && event.target?.closest?.("a, button, summary, [role=button]")) return;
      if (press && byId(press)) { event.preventDefault(); byId(press).click(); }
    }
  });
  // Coming back to the window reads what moves again, so the page follows
  // the repository without a timer running behind it.
  let lastFocusRead = Date.now();
  const reread = () => {
    if (document.hidden || state.refreshing || workspaceWriteBusy() || state.commit.busy || !session() || Date.now() - lastFocusRead < 15_000) return;
    lastFocusRead = Date.now();
    refresh(MACHINE ? ["home", "clients"] : ["session", "working", "changes", "standing", "branches", "home"]);
  };
  globalThis.addEventListener("focus", reread);
  document.addEventListener("visibilitychange", reread);

  const started = (async () => {
    recallWorking();
    routeChanged(true);
    await load("session");
    updateChrome();
    render();
    if (state.route === "entity" && session()) loadEntity(state.params.id);
    if (state.route === "review" && state.params.target && session()) loadReview(state.params.target);
    if (state.route === "explore" && state.explore.query.trim() && session()) runAsk(state.explore.query, true);
    await refresh(readKeys().filter((key) => key !== "session"));
    if (MACHINE) {
      // The hub has no live feed of its own, so it reads the list again while it is being looked at.
      setInterval(() => { if (!document.hidden && session() && !state.refreshing) pulse(["home"], 0); }, 10_000);
      return;
    }
    if (!read("agents").missing && agentsApi()) {
      state.disposeLive = agentsApi().follow(agentsCtx);
      // Times like "connected for 4 minutes" are read from the clock; draw them again now and then.
      setInterval(() => { if (!document.hidden && state.route === "agents") agentsCtx.repaintAgents(); }, 30_000);
    }
  })();

  globalThis.KinWorkspace = { workSentence, personName, parseRoute, bodyLines, askGroups, isTest, relyWords, started, pulse };
})();
