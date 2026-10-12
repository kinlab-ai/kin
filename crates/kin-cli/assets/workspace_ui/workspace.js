// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

"use strict";

// The local workspace: a repository's entities, what relies on them, and the
// changes that moved them. Every number, name and connection on the page comes
// from a read of this repository. A read that fails says so in its own place
// and offers to try again; the rest of the page keeps working.
(() => {
  const byId = (id) => document.getElementById(id);
  const rawBase = document.body.dataset.apiBase || "";
  const apiBase = rawBase === "__KIN_BASE__" ? "" : rawBase.replace(/\/$/, "");
  const graphApi = () => globalThis.KinWorkspaceGraph || null;
  const agentsApi = () => globalThis.KinWorkspaceAgents || null;
  const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  const ROUTES = ["home", "agents", "explore", "entity", "changes", "branches"];
  const TITLES = { home: "Home", agents: "Agents", explore: "Explore", entity: "Entity", changes: "Changes", branches: "Branches" };
  const AGENT_KEY = /^[0-9a-f]{16}$/;

  // Reads shared by every view. Optional reads are newer endpoints: when the
  // server does not offer one, the page uses the older read it replaces.
  const READS = {
    session: "/api/workspace/session",
    changes: "/api/changes",
    standing: "/api/standing",
    branches: "/api/branches",
    branchState: "/api/workspace/branches",
    conflicts: "/api/workspace/conflicts/details",
    working: "/api/guide?working=1",
    graph: "/api/workspace/graph?query=&offset=0",
    overview: "/api/workspace/overview",
    map: "/api/workspace/map",
    agents: "/api/workspace/agents",
  };
  const OPTIONAL = new Set(["overview", "map", "agents"]);
  // The graph sample is the older server's stand-in for the overview and the
  // map. It is read only when one of those is not offered.
  const needsGraph = () => read("overview").missing || read("map").missing;
  const readKeys = () => Object.keys(READS).filter((key) => key !== "graph" || needsGraph());
  const FAILED_WORDS = {
    session: "Kin can't reach this repository right now.",
    changes: "Kin couldn't load the changes.",
    standing: "Kin couldn't load review decisions.",
    branches: "Kin couldn't load the branches.",
    branchState: "Kin couldn't load the branches.",
    conflicts: "Kin couldn't load the merge in progress.",
    working: "Kin couldn't load your uncommitted work.",
    graph: "Kin couldn't load this code's entities.",
    overview: "Kin couldn't load the summary.",
    map: "Kin couldn't load the map.",
    agents: "Kin couldn't load which agents are connected.",
  };
  /** Whether a failure only means Kin took too long, so trying again later may work. */
  const isSlow = (detail) => /did not answer within|took longer than|timed out|timeout/i.test(String(detail || ""));

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
    rangeHead: "HEAD",
    explore: { query: "", results: null, error: null, busy: false, request: 0, offset: 0, active: -1 },
    entity: { id: null, data: null, error: null, busy: false, request: 0, expanded: false, showAll: {} },
    merge: { source: "", preview: null, result: null, error: null, remedy: null, busy: null, operationId: null, reportedOperationId: null, unknown: false, confirmed: false, receiptRestored: false },
    branch: { target: "", preview: null, result: null, error: null, diagnostic: null, busy: null, confirmed: false, operationId: null, unknown: false, receiptRestored: false },
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
    branch: "M6 3.5v13M18 8.5c0 4.5-6 3.5-12 7.5M6 21a2 2 0 1 0 0-4 2 2 0 0 0 0 4zM18 9a2 2 0 1 0 0-4 2 2 0 0 0 0 4zM6 5.5a2 2 0 1 0 0-4 2 2 0 0 0 0 4z",
    search: "M11 17.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM20 20l-4.3-4.3",
    editor: "M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5",
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
  const short = (value) => String(value || "").slice(0, 10);
  const NUMBER_WORD = ["No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten", "Eleven", "Twelve"];
  const spoken = (n) => n < NUMBER_WORD.length ? NUMBER_WORD[n] : n.toLocaleString();
  const count = (n) => Number(n).toLocaleString();
  const plural = (n, one, many) => `${count(n)} ${n === 1 ? one : many || `${one}s`}`;
  const kindWord = (kind) => text(kind).replace(/_/g, " ").replace(/^\w/, (c) => c.toUpperCase());
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
    const parts = [`${spoken(changed)} ${noun(changed)} changed.`];
    const outside = summary.recorded_consumers || 0;
    if (!outside) parts.push(`Nothing outside the change relies on ${it}.`);
    else {
      parts.push(`${spoken(outside)} other ${outside === 1 ? "place relies" : "places rely"} on ${it}.`);
      const behind = isCount(summary.consumers_not_updated) ? summary.consumers_not_updated : 0;
      // The guided review's own verdicts, when it gives them; an older guide reads as before.
      const check = isCount(summary.consumers_may_need_review) ? summary.consumers_may_need_review : 0;
      const fine = isCount(summary.consumers_need_no_update) ? summary.consumers_need_no_update : null;
      if (behind && behind === outside) parts.push(outside === 1 ? "It was not updated." : outside === 2 ? "Neither was updated." : "None were updated.");
      else if (behind) { parts.push(`${spoken(behind)} ${behind === 1 ? "was" : "were"} not updated.`); if (check) parts.push(`Check ${spoken(check).toLowerCase()} more.`); }
      else if (check) parts.push(check === outside ? (outside === 1 ? "Check it." : outside === 2 ? "Check both." : "Check each of them.") : `Check ${spoken(check).toLowerCase()} of them.`);
      else if (fine !== null && fine === outside) parts.push(outside === 1 ? "It needs no update." : "None need an update.");
      else parts.push(outside === 1 ? "It did not change." : "None of them changed.");
    }
    const tests = summary.recorded_tests || 0;
    parts.push(tests ? `${spoken(tests)} ${tests === 1 ? "test reaches" : "tests reach"} ${it}.` : `No test reaches ${it}.`);
    return parts.join(" ");
  }

  /**
   * The repository in one sentence, from whichever counts the reads returned.
   * The latest change is the latest one a person made: Kin's own updates to
   * its understanding of the code are left out by the caller.
   */
  function repositorySentence(facts) {
    const parts = [];
    if (isCount(facts.entities)) {
      const holds = isCount(facts.containers) && facts.containers > 0 ? ` in ${plural(facts.containers, facts.containerWord || "module", facts.containerPlural)}` : "";
      parts.push(isCount(facts.relations) && facts.relations > 0
        ? `Kin keeps track of how this code fits together: ${plural(facts.entities, "entity", "entities")}${holds}, ${plural(facts.relations, "connection")}.`
        : `Kin keeps track of how this code fits together: ${plural(facts.entities, "entity", "entities")}${holds}.`);
    }
    if (isCount(facts.tests) && facts.tests > 0) parts.push(`${spoken(facts.tests)} ${facts.tests === 1 ? "test reaches" : "tests reach"} into the code.`);
    if (facts.latest) {
      const when = ago(facts.latest);
      if (when) parts.push(`The latest change was ${when}.`);
    }
    return parts.join(" ");
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
  function repositoryTitle(repository, place) {
    const owner = text(repository?.owner);
    const name = text(repository?.name) || "repository";
    return owner
      ? [h("span", { class: `repo-owner repo-owner-${place}` }, owner), h("span", { class: "repo-sep", "aria-hidden": "true" }, place === "title" ? "/" : " / "), h("span", { class: "repo-name" }, name)]
      : [h("span", { class: "repo-name" }, name)];
  }

  const read = (key) => state.reads[key];
  const session = () => read("session").data;
  function announce(message) { const node = byId("workspace-announcer"); if (node) node.textContent = message; }
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
  const changeURL = (id) => reviewURL(`#/c/${encodeURIComponent(id)}`);
  const workingURL = () => reviewURL("#/w");
  const entityHref = (id) => `#/entity/${encodeURIComponent(id)}`;
  const branchRows = () => array(read("branches").data?.branches);
  const activeBranch = () => branchRows().find((branch) => branch.active);
  const defaultBranch = () => branchRows().find((branch) => branch.default);
  const shortRef = (name) => text(name).replace(/^refs\/heads\//, "");

  function button(label, handler, attrs = {}) { return h("button", { type: "button", class: "button", on: { click: handler }, ...attrs }, label); }
  function link(label, href, primary = false, attrs = {}) { return h("a", { class: `button${primary ? " button-primary" : ""}`, href, ...attrs }, label, icon("arrow")); }
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

  // ── Reading ──────────────────────────────────────────────────────────────

  async function fetchJSON(path, options = {}) {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 60_000);
    try {
      const response = await fetch(`${apiBase}${path}`, { ...options, signal: controller.signal, headers: { accept: "application/json", ...(options.headers || {}) } });
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
        throw error;
      }
      if (!body || typeof body !== "object") throw new Error("The local server returned nothing readable.");
      return body;
    } catch (error) {
      if (error.name === "AbortError") throw new Error("This read took longer than a minute.");
      throw error;
    } finally { clearTimeout(timer); }
  }

  function pathFor(key) {
    return key === "changes" ? `${READS.changes}?count=${state.requested}` : READS[key];
  }

  async function load(key) {
    const resource = read(key);
    const epoch = state.readEpoch;
    if (resource.busy) return;
    if (resource.missing && OPTIONAL.has(key)) return;
    resource.busy = true;
    try {
      const data = await fetchJSON(pathFor(key));
      if (epoch !== state.readEpoch) return;
      if (key === "session" && (!data.repository || typeof data.repository.name !== "string")) throw new Error("The server did not name the repository.");
      if (key === "changes" && !Array.isArray(data.changes)) throw new Error("Changes were not returned in a readable form.");
      if (key === "branches" && !Array.isArray(data.branches)) throw new Error("Branches were not returned in a readable form.");
      if (key === "working" && !data.summary) throw new Error("Uncommitted work was not returned in a readable form.");
      if (key === "standing" && (!data.standing || typeof data.standing !== "object")) throw new Error("Review decisions were not returned in a readable form.");
      if (key === "graph") graphApi()?.graphData?.(data);
      if (key === "map" && (!Array.isArray(data.nodes) || (data.schema && data.schema !== "kin.workspace.map.v1"))) throw new Error("The repository map was not returned in a readable form.");
      if (key === "overview" && (data.schema && data.schema !== "kin.workspace.overview.v1")) throw new Error("The repository summary was not returned in a readable form.");
      if (key === "agents" && (!Array.isArray(data.agents) || data.schema !== "kin.workspace.agents.v1")) throw new Error("The agents were not returned in a readable form.");
      const sig = JSON.stringify(data);
      resource.changed = sig !== resource.sig;
      resource.sig = sig;
      resource.data = data;
      resource.at = Date.now();
      resource.error = null;
      if (key === "session") { restoreMergeReceipt(); restoreBranchReceipt(); }
    } catch (error) {
      if (epoch !== state.readEpoch) return;
      if (OPTIONAL.has(key) && [404, 405, 501].includes(error.status)) { resource.missing = true; resource.error = null; resource.changed = true; }
      else { resource.error = error.message || "This read could not be completed."; resource.changed = true; }
    } finally { if (epoch === state.readEpoch) resource.busy = false; }
  }

  async function refresh(keys = readKeys()) {
    if (state.refreshing || state.branch.busy || state.merge.busy) return;
    state.refreshing = true;
    setRefreshDisabled(true);
    updateChrome();
    const hadSession = Boolean(session());
    const everything = keys.length >= readKeys().length;
    // Each section draws as soon as its own read lands, so a slow read never
    // holds back the ones that are ready.
    const drawn = new Set();
    const settle = async (key) => {
      await load(key);
      if (key === "changes") loadCounts();
      if (!hadSession || !session()) return;
      syncAll([key]);
      drawn.add(key);
      if (key === "branches") updateChrome();
      if (["branches", "branchState", "conflicts"].includes(key)) repaintPanels();
    };
    await Promise.all(keys.map(settle));
    if (needsGraph() && !keys.includes("graph") && (everything || keys.some((key) => OPTIONAL.has(key))) && !read("graph").data) { keys = [...keys, "graph"]; await settle("graph"); }
    state.refreshing = false;
    setRefreshDisabled(false);
    updateChrome();
    if (!hadSession || !session()) render();
    else syncAll(keys.filter((key) => !drawn.has(key)));
    repaintPanels();
    if (state.route === "entity" && everything) loadEntity(state.params.id, true);
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
        // An older server cannot count; a failed count is asked again on the next read.
        if ([404, 405, 501].includes(error.status)) state.countsMissing = true;
        else state.countsFailed = true;
        paint("home-changes");
        return;
      } finally {
        batch.forEach((id) => state.countsAsked.delete(id));
      }
      for (const id of ["home-changes", "changes-results", "home-agents", "agents-detail"]) paint(id);
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
    for (const key of keys) if (key in READS && !(key === "graph" && !needsGraph())) pulsing.keys.add(key);
    if (pulsing.timer || !pulsing.keys.size) return;
    pulsing.timer = setTimeout(async () => {
      pulsing.timer = null;
      if (!session() || state.destroyed) { pulsing.keys.clear(); return; }
      const ready = [...pulsing.keys].filter((key) => !read(key).busy);
      pulsing.keys = new Set([...pulsing.keys].filter((key) => read(key).busy));
      await Promise.all(ready.map(async (key) => { await load(key); syncAll([key]); }));
      if (ready.some((key) => ["changes", "branches"].includes(key))) updateChrome();
      if (pulsing.keys.size) pulse([], 800);
    }, delay);
  }

  function setRefreshDisabled(disabled) {
    const node = byId("workspace-refresh");
    if (node) node.disabled = Boolean(disabled || state.branch.busy || state.merge.busy);
  }

  // ── Sections ─────────────────────────────────────────────────────────────
  // A view is a set of sections, each bound to the reads it shows. When a read
  // returns, only the sections bound to it are drawn again, so a map keeps
  // drifting and a half-typed search keeps its caret while history refreshes.

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
    const selection = focused instanceof HTMLInputElement && ["search", "text"].includes(focused.type) ? [focused.selectionStart, focused.selectionEnd] : null;
    const built = entry.build((cleanup) => { entry.cleanup = cleanup; });
    entry.node.replaceChildren(...[built].flat().filter(Boolean));
    if (focusId && byId(focusId)) {
      byId(focusId).focus({ preventScroll: true });
      if (selection && byId(focusId) instanceof HTMLInputElement) byId(focusId).setSelectionRange?.(...selection);
    }
  }
  /** Draw again, once each, the sections bound to any of these reads that changed. */
  function syncAll(keys) {
    const changed = keys.filter((key) => read(key).changed !== false);
    // The most relied-on list names the map's labels; it re-labels the map in place.
    if (changed.includes("overview") && state.homeMap?.relabel) state.homeMap.relabel(topEntities().map((row) => row.id));
    // Uncommitted work is ringed on the map, in place as it changes.
    if (changed.includes("working") && state.homeMap?.mark) state.homeMap.mark(workIds());
    for (const key of changed) read(key).changed = false;
    if (changed.includes("agents")) { updateAgentsNav(); state.homeMap?.agentMark?.(agentsApi()?.marks(read("agents").data) || []); }
    for (const [id, entry] of [...state.sections]) if (state.sections.get(id) === entry && entry.reads.some((key) => changed.includes(key))) paint(id);
  }
  function clearSections() {
    for (const entry of state.sections.values()) entry.cleanup?.();
    state.sections.clear();
    for (const dispose of state.disposers.splice(0)) dispose();
  }

  // ── Shared facts ─────────────────────────────────────────────────────────

  const section_ok = (value) => value && typeof value === "object" && value.ok !== false;
  const PLURAL_KIND = { module: "modules", package: "packages", namespace: "namespaces", class: "classes", crate: "crates" };

  /** The drawable map and where it came from: the map read when offered, otherwise the graph sample. */
  function mapModel() {
    const G = graphApi();
    if (!G) return null;
    const map = read("map");
    if (map.data && !map.missing) {
      const model = G.mapFromContainers?.(map.data);
      if (!model) return null;
      const kinds = new Set(model.groups.map((group) => group.kind).filter(Boolean));
      const drawn = model.nodes.filter((node) => !node.container).length;
      return { model, drawn, total: isCount(map.data.totals?.entities) ? map.data.totals.entities : null, groupWord: kinds.size === 1 ? [...kinds][0] : "top-level container" };
    }
    if (!map.missing) return null;
    const graph = read("graph").data?.graph;
    if (!graph || !G.mapFromGraph) return null;
    const model = G.mapFromGraph(graph);
    return { model, drawn: model.nodes.length, total: isCount(graph.entity_count) ? graph.entity_count : null, groupWord: "module" };
  }
  function mapRead() { return read("map").missing ? read("graph") : read("map"); }

  /** Where each sampled entity lives, by the module the older graph read says contains it. */
  function containerIndex() {
    const graph = read("graph").data?.graph;
    const cached = state.containerCache;
    if (cached && cached.graph === graph) return cached.index;
    const index = new Map();
    const model = graph && graphApi()?.mapFromGraph ? graphApi().mapFromGraph(graph) : null;
    if (model) {
      const names = new Map(model.groups.map((group) => [group.id, group.name]));
      for (const node of model.nodes) if (names.get(node.group)) index.set(node.id, names.get(node.group));
    }
    state.containerCache = { graph, index };
    return index;
  }
  function containerOf(row) {
    if (!row) return "";
    const container = row.container && typeof row.container === "object" ? text(row.container.name) : text(row.container);
    const explicit = container || text(row.container_name) || text(row.semantic_owners?.[0]?.name) || text(row.module);
    if (explicit) return explicit;
    const sampled = containerIndex().get(row.id);
    if (sampled) return sampled;
    const name = text(row.name);
    return name.includes(".") ? name.slice(0, name.lastIndexOf(".")) : "";
  }
  const reliedOn = (row) => isCount(row?.dependents) ? row.dependents : isCount(row?.relied_on_by) ? row.relied_on_by : null;

  function topEntities() {
    const overview = read("overview").data;
    const listed = section_ok(overview?.most_relied_on) && Array.isArray(overview.most_relied_on.items) ? overview.most_relied_on.items : null;
    const rows = listed || (read("overview").missing ? array(read("graph").data?.inventory?.entities) : []);
    return rows.filter((row) => row && UUID.test(row.id) && text(row.kind).toLowerCase() !== "module" && (reliedOn(row) ?? 0) > 0);
  }
  /** Whether the most relied-on list is still on its way, failed, or settled. */
  function topState() {
    const overview = read("overview");
    if (!overview.missing) {
      if (overview.data) return section_ok(overview.data.most_relied_on) ? "ready" : "failed";
      return overview.error ? "error" : "reading";
    }
    const graph = read("graph");
    return graph.data ? "ready" : graph.error ? "error" : "reading";
  }

  /** Whether Kin recorded a change for itself: an update to its understanding that changes no code. */
  const isKinUpdate = (change) => change?.kin_checkpoint === true;
  /** The changes people made, newest first, without Kin's own updates. */
  const peopleChanges = () => array(read("changes").data?.changes).filter((change) => !isKinUpdate(change));

  function repositoryFacts() {
    const overview = read("overview").data;
    const counts = section_ok(overview?.counts) ? overview.counts : null;
    const graph = read("graph").data?.graph;
    const changes = read("changes").data;
    const map = read("map").data;
    const containerKinds = new Set(array(map?.nodes).filter((row) => row?.role === "container").map((row) => text(row.kind)));
    const containerKind = containerKinds.size === 1 ? [...containerKinds][0] : null;
    return {
      entities: isCount(counts?.entities) ? counts.entities : graph?.entity_count,
      relations: isCount(counts?.relationships) ? counts.relationships : graph?.relation_count,
      containers: PLURAL_KIND[containerKind] && isCount(map?.totals?.containers) ? map.totals.containers : null,
      containerWord: containerKind || "module",
      containerPlural: PLURAL_KIND[containerKind] || "modules",
      latest: peopleChanges()[0]?.timestamp,
    };
  }

  // ── Agents ───────────────────────────────────────────────────────────────
  // The agents panel and page live in agents.js; this is what they draw with.

  const agentsCtx = {
    h, icon, ago, clock, kindWord, entityHref, changeURL, button, link, moreLink, pill, skeleton, skeletonLines, failed, workSentence, fetchJSON, announce, apiBase,
    read: () => read("agents"),
    refresh: (keys) => refresh(keys),
    pulse: (keys, delay) => pulse(keys, delay),
    paint: (id) => paint(id),
    section: (id, reads, build, attrs) => section(id, reads, build, attrs),
    params: () => state.params,
    // The one count a change is shown with: its own review's, as Changes shows it.
    changedCount: (id) => changedCount({ id }),
    repaintAgents: () => { for (const id of ["home-agents", "agents-lede", "agents-list", "agents-detail"]) paint(id); },
  };

  // ── Home ─────────────────────────────────────────────────────────────────

  function home() {
    const repository = session().repository;
    return [
      h("header", { class: "home-head" }, h("div", {},
        h("h1", { class: "repo-title" }, repositoryTitle(repository, "title"), !text(repository.host) ? h("span", { class: "pill pill-local" }, "Local") : null),
        section("home-lede", ["overview", "graph", "changes", "map"], () => {
          const sentence = repositorySentence(repositoryFacts());
          if (sentence) return h("p", { class: "lede" }, sentence);
          const source = read("overview").missing ? "graph" : "overview";
          if (read(source).error) return h("div", { class: "section" }, failed(isSlow(read(source).error) ? "Kin is still counting this repository. Try again in a moment." : "Kin couldn't load this repository's counts.", () => refresh([source, "changes"]), read(source).error));
          if (read("overview").data && !section_ok(read("overview").data.counts)) return h("p", { class: "lede lede-soft" }, read("overview").data.counts?.reason || "Kin couldn't load this repository's counts.");
          return skeleton("skeleton-lede");
        }))),
      section("home-work", ["working", "changes"], workCard),
      // The agents sit right above the map whose rings show what they touch.
      section("home-agents", ["agents"], () => agentsApi()?.panel(agentsCtx) || null),
      h("section", { class: "map-card", "aria-labelledby": "home-map-title" },
        section("home-map", ["map", "graph"], (onCleanup) => {
          const title = h("h2", { class: "map-title", id: "home-map-title" }, "Repository map");
          const stage = h("div", { class: "map-stage" });
          const resource = mapRead();
          const map = mapModel();
          if (!map) {
            stage.append(h("div", { class: "map-empty" }, resource.error ? failed(FAILED_WORDS.map, () => refresh(read("map").missing ? ["graph"] : ["map"]), resource.error) : skeleton("skeleton-block")));
            return [title, stage];
          }
          if (!map.model.nodes.length) {
            stage.append(h("div", { class: "map-empty" }, h("p", { class: "gap" }, "No entities to draw yet.")));
            return [title, stage];
          }
          const plural_group = PLURAL_KIND[map.groupWord] ? map.groupWord : "top-level container";
          const caption = map.total && map.total > map.drawn
            ? `Showing the ${count(map.drawn)} most connected of ${count(map.total)} entities, grouped by the ${plural_group} that holds them`
            : `${plural(map.drawn, "entity", "entities")}, grouped by the ${plural_group} that holds them`;
          const labelIds = topEntities().map((row) => row.id);
          // One phrase for how relied on an entity is, the tiles' own number.
          const relied = new Map(topEntities().map((row) => [row.id, reliedOn(row)]));
          const G = graphApi();
          const mount = () => {
            if (stage.isConnected === false) return;
            try {
              const controller = G.createMap(stage, map.model, {
                mode: "clusters", labelCount: 9, labelIds, markIds: workIds(), tip: byId("workspace-tip"), reserveBottom: 30,
                summary: `Repository map. ${caption}. The most relied-on entities are named.`,
                onOpen: (id) => { if (UUID.test(id)) location.hash = entityHref(id); },
                describe: (node) => node.container ? kindWord(node.kind) : [kindWord(node.kind), relyWords(relied.get(node.id)), workIds().includes(node.id) ? "Changed in your uncommitted work" : ""].filter(Boolean).join(" · "),
              });
              state.homeMap = controller;
              controller.agentMark?.(agentsApi()?.marks(read("agents").data) || []);
              paint("home-map-agents");
              onCleanup(() => { if (state.homeMap === controller) state.homeMap = null; controller.dispose(); });
            } catch (_) { stage.replaceChildren(h("div", { class: "map-empty" }, h("p", { class: "gap" }, "The map could not be drawn in this browser."))); }
          };
          if (globalThis.requestAnimationFrame) requestAnimationFrame(mount); else mount();
          return [title, stage, h("p", { class: "map-caption" }, caption)];
        }),
        section("home-map-agents", ["agents", "map"], () => agentsApi()?.legend(agentsCtx, read("agents").data, state.homeMap?.agentMarked?.length || 0) || null)),
      h("div", { class: "home-grid" },
        h("section", { class: "section", "aria-labelledby": "home-top-title" },
          h("div", { class: "section-head" }, h("h2", { id: "home-top-title" }, "Most relied on"), moreLink("Explore", "#/explore")),
          section("home-top", ["overview", "graph"], () => {
            const rows = topEntities().slice(0, 6);
            const status = topState();
            if (rows.length) return h("div", { class: "tiles" }, rows.map(tile));
            if (status === "error") return readFailure(read("overview").missing ? "graph" : "overview");
            if (status === "failed") return failed(read("overview").data.most_relied_on?.reason || "Kin couldn't load what is most relied on.", () => refresh(["overview"]));
            if (status === "ready") return h("p", { class: "gap" }, "Nothing relies on another entity yet.");
            return h("div", { class: "tiles" }, Array.from({ length: 6 }, () => h("div", { class: "tile" }, skeleton("skeleton-line"), skeleton("skeleton-line"))));
          })),
        h("section", { class: "section", "aria-labelledby": "home-changes-title" },
          h("div", { class: "section-head" }, h("h2", { id: "home-changes-title" }, "Recent changes"), moreLink("See changes", "#/changes")),
          section("home-changes", ["changes", "standing"], () => {
            const rows = recentForHome();
            if (rows === null) return h("ul", { class: "list list-card", "aria-busy": "true" }, Array.from({ length: 5 }, () => h("li", { class: "change-row" }, h("div", {}, skeleton("skeleton-line")))));
            return changeList(rows, { compact: true });
          })))];
  }

  /** "21 rely on it": the one way every count of what relies on an entity is said. */
  function relyWords(n) { return isCount(n) ? `${count(n)} ${n === 1 ? "relies on it" : "rely on it"}` : ""; }

  /**
   * The changes Home leads with: the latest ones people made that changed an
   * entity, as each change's own review counts it. A merge that brought
   * nothing new waits on the Changes page. When none of them changed an
   * entity, the latest is still shown. Null while the counts are on their way.
   */
  function recentForHome() {
    const people = peopleChanges();
    if (!read("changes").data || !people.length) return people;
    if (state.countsMissing || state.countsFailed) return people.slice(0, 5);
    const wanted = people.slice(0, 100);
    if (wanted.some((change) => !state.counts.has(change.id))) return null;
    // A change Kin could not count is shown rather than hidden.
    const changed = wanted.filter((change) => changedCount(change) !== 0);
    return (changed.length ? changed : people).slice(0, 5);
  }

  function tile(row) {
    const n = reliedOn(row);
    const where = containerOf(row);
    return h("a", { class: "tile", href: entityHref(row.id) },
      h("span", { class: "tile-name" }, row.name),
      h("span", { class: "tile-meta" }, [kindWord(row.kind), where && !row.name.startsWith(`${where}.`) ? `in ${where}` : ""].filter(Boolean).join(" ")),
      isCount(n) ? h("span", { class: "tile-count" }, h("strong", {}, count(n)), n === 1 ? "relies on it" : "rely on it") : null);
  }

  /** The entities uncommitted work changed, as the guided review lists them. */
  function workIds() {
    return array(read("working").data?.chapters).flatMap((chapter) => array(chapter.steps)).map((step) => text(step?.entity_id)).filter((id) => UUID.test(id));
  }

  /** The changed entities themselves, each a way into its own page. */
  function workChips(guide) {
    const steps = array(guide?.chapters).flatMap((chapter) => array(chapter.steps)).filter((step) => step && typeof step.name === "string");
    if (!steps.length) return null;
    const SHOWN = 4;
    const CHANGE = { added: "Added", modified: "Changed", removed: "Removed" };
    return h("div", { class: "chips", "aria-label": "Entities in this work" },
      steps.slice(0, SHOWN).map((step) => {
        const label = [h("span", { class: `chip-dot chip-${text(step.change) || "modified"}`, "aria-hidden": "true" }), step.name];
        const title = `${CHANGE[step.change] || "Changed"} ${text(step.kind)}`.trim();
        return UUID.test(text(step.entity_id)) && step.change !== "removed"
          ? h("a", { class: "chip", href: entityHref(step.entity_id), title }, label)
          : h("span", { class: "chip", title }, label);
      }),
      steps.length > SHOWN ? h("span", { class: "chip chip-more" }, `${count(steps.length - SHOWN)} more`) : null);
  }

  function workCard() {
    const resource = read("working");
    const guide = resource.data;
    if (!guide) {
      if (resource.error) return h("div", { class: "work card" }, readFailure("working"));
      return h("div", { class: "work card", "aria-busy": "true" }, h("div", { class: "work-text" }, skeleton("skeleton-line"), skeleton("skeleton-lede")));
    }
    const sentence = workSentence(guide);
    const tree = guide.working_tree || {};
    const latest = peopleChanges()[0];
    let body, action, hasWork = true;
    if (sentence) {
      body = sentence;
      action = link("Review uncommitted work", workingURL(), true);
    } else if (tree.differs) {
      body = "Your code differs from your last commit, but no entity changed.";
      action = link("Review uncommitted work", workingURL(), true);
    } else {
      hasWork = false;
      body = "No uncommitted work. Your code matches your last commit.";
      action = latest ? link("Review the latest change", changeURL(latest.id), true) : link("Open the guided review", workingURL(), true);
    }
    return h("section", { class: "work card", "aria-labelledby": "home-work-title" },
      h("div", { class: "work-text" },
        h("span", { class: `pill ${hasWork ? "pill-work" : "pill-quiet"}`, id: "home-work-title" }, hasWork ? "Uncommitted work" : "Up to date"),
        h("p", { class: "work-sentence" }, body),
        workChips(guide),
        resource.error ? h("p", { class: "work-sub" }, "Showing what Kin last saw. ", button("Refresh", () => refresh(["working"]), { class: "button button-small" })) : null),
      h("div", { class: "work-actions" }, action));
  }

  // ── Changes ──────────────────────────────────────────────────────────────

  const DECISION = { approved: ["Approved", "good"], needswork: ["Needs work", "warn"], needs_work: ["Needs work", "warn"], blocked: ["Blocked", "bad"], pending: ["To review", ""] };
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
    return value === "pending" ? h("span", { class: "decision-none" }, label) : pill(label, tone);
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

  function changeRow(change) {
    const author = authorOf(change);
    const when = ago(change.timestamp);
    const kin = isKinUpdate(change);
    const n = changedCount(change);
    const entities = kin ? null : isCount(n) ? (n ? `${plural(n, "entity", "entities")} changed` : "No entity changed") : null;
    const quiet = kin || n === 0;
    return h("li", {}, h("a", { class: `change-row${quiet ? " change-row-quiet" : ""}`, href: changeURL(change.id) },
      h("div", {}, h("span", { class: "change-title" }, change.title || "Untitled change"),
        h("span", { class: "change-meta" }, agentsApi() ? agentsApi().author(agentsCtx, change, author) : author ? h("span", {}, author) : null, when ? h("time", { datetime: change.timestamp || undefined }, when) : null, entities ? h("span", {}, entities) : null)),
      h("span", { class: "change-side" }, kin ? pill("Kin update", "quiet") : decisionPill(change))));
  }

  function changeList(changes, { compact = false } = {}) {
    const resource = read("changes");
    if (!resource.data) {
      if (resource.error) return readFailure("changes");
      return h("ul", { class: "list list-card", "aria-busy": "true" }, Array.from({ length: compact ? 5 : 8 }, () => h("li", { class: "change-row" }, h("div", {}, skeleton("skeleton-line")))));
    }
    if (!changes.length) {
      return h("p", { class: "gap" }, array(resource.data.changes).length ? "No change matches." : "No changes yet.");
    }
    const list = h("ul", { class: "list list-card" }, changes.map(changeRow));
    if (compact) return list;
    const truncated = resource.data.truncated;
    return h("div", { class: "list-card" }, h("ul", { class: "list" }, changes.map(changeRow)),
      h("div", { class: "list-foot" }, h("span", {}, resource.error ? "Showing what Kin last saw." : `${plural(changes.length, "change")} shown`),
        truncated && state.requested < 500 ? button("Show older changes", () => { state.requested = Math.min(500, state.requested * 2); refresh(["changes"]); }, { class: "button button-small", disabled: state.refreshing }) : null));
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
    if (!tally.approved && !tally.needs) return `${head} None reviewed yet.`;
    const parts = [];
    if (tally.approved) parts.push(`${spoken(tally.approved)} approved`);
    if (tally.needs) parts.push(`${spoken(tally.needs).toLowerCase()} ${tally.needs === 1 ? "needs" : "need"} work`);
    return `${head} ${parts.join(", ").replace(/^\w/, (c) => c.toUpperCase())}.`;
  }

  function compareForm() {
    const branches = branchRows();
    const options = h("datalist", { id: "workspace-branch-options" }, branches.map((branch) => h("option", { value: branch.name })));
    const base = h("input", { id: "workspace-range-base", name: "base", list: "workspace-branch-options", placeholder: "main", required: true, value: state.rangeBase ?? defaultBranch()?.name ?? "", autocomplete: "off", spellcheck: "false", on: { input: (event) => { state.rangeBase = event.target.value; } } });
    const head = h("input", { id: "workspace-range-head", name: "head", list: "workspace-branch-options", placeholder: "HEAD", value: state.rangeHead, autocomplete: "off", spellcheck: "false", on: { input: (event) => { state.rangeHead = event.target.value; } } });
    return h("form", { class: "compare card workspace-compare", "aria-label": "Compare branches", on: { submit: (event) => { event.preventDefault(); const from = base.value.trim(); const to = head.value.trim(); if (from) location.assign(reviewURL(`#/r/${encodeURIComponent(from)}..${to === "HEAD" ? "" : encodeURIComponent(to)}`)); } } },
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

  function changes() {
    const query = h("input", { id: "workspace-review-search", type: "search", placeholder: "Title, author or ID", value: state.reviewQuery, autocomplete: "off", on: { input: (event) => { state.reviewQuery = event.target.value; updateReviewResults(); } } });
    const FILTERS = [["all", "All"], ["pending", "To review"], ["needswork", "Needs work"], ["approved", "Approved"]];
    return [
      h("header", {}, h("h1", {}, "Changes"),
        section("changes-lede", ["changes", "standing"], () => { const sentence = changesSentence(); return sentence ? h("p", { class: "lede lede-soft" }, sentence) : skeleton("skeleton-lede"); })),
      section("changes-work", ["working"], () => {
        const guide = read("working").data;
        if (!guide) return read("working").error ? h("div", { class: "section" }, readFailure("working")) : null;
        const sentence = workSentence(guide);
        if (!sentence && !guide.working_tree?.differs) return null;
        return h("div", { class: "section-tight" }, h("ul", { class: "list list-card" }, h("li", {}, h("a", { class: "change-row change-row-work", href: workingURL() },
          h("div", {}, h("span", { class: "change-title" }, "Uncommitted work"), h("span", { class: "change-meta" }, sentence || "Your code differs from your last commit.")),
          h("span", { class: "change-side" }, pill("Review", "accent"))))));
      }),
      h("section", { class: "section", "aria-labelledby": "changes-list-title" },
        h("div", { class: "toolbar" },
          h("div", { class: "field" }, h("label", { for: "workspace-review-search", id: "changes-list-title" }, "Changes"), query),
          h("button", { type: "button", class: "button button-toolbar", id: "changes-compare-toggle", "aria-expanded": String(Boolean(state.compareOpen)), "aria-controls": "changes-compare", on: { click: () => { state.compareOpen = !state.compareOpen; byId("changes-compare-toggle")?.setAttribute("aria-expanded", String(state.compareOpen)); paint("changes-compare"); if (state.compareOpen) byId("workspace-range-base")?.focus(); } } }, "Compare branches"),
          section("changes-filter", ["standing"], () => {
            if (!read("standing").data && state.reviewFilter !== "all") state.reviewFilter = "all";
            return h("div", { class: "segmented", role: "group", "aria-label": "Show by review decision", id: "workspace-review-filter" },
              FILTERS.map(([value, label]) => h("button", { type: "button", "aria-pressed": String(state.reviewFilter === value), disabled: value !== "all" && !read("standing").data, on: { click: () => { state.reviewFilter = value; paint("changes-filter"); updateReviewResults(); } } }, label)));
          })),
        section("changes-authors", ["changes"], authorFilter),
        section("changes-compare", ["branches"], () => state.compareOpen ? compareForm() : null, { class: "compare-host" }),
        section("changes-results", ["changes", "standing"], () => changeList(filteredChanges()), { "aria-live": "polite" }))];
  }

  // ── Explore ──────────────────────────────────────────────────────────────

  const askAvailable = () => !read("overview").missing && !state.askMissing;

  function examplePlaceholder() {
    const top = topEntities()[0];
    return top ? `Ask about this code, like “${top.name}”` : "Ask about this code";
  }

  function explore() {
    const input = h("input", { id: "explore-input", class: "ask-input", type: "search", name: "q", autocomplete: "off", spellcheck: "false", maxlength: "256",
      placeholder: examplePlaceholder(), "aria-label": askAvailable() ? "Ask about this code by meaning or by name" : "Find an entity by name", "aria-controls": "explore-results", value: state.explore.query,
      on: { input: (event) => queueAsk(event.target.value), keydown: exploreKeys } });
    return [
      h("header", {}, h("h1", {}, "Explore"),
        section("explore-lede", ["overview", "graph"], () => {
          const field = byId("explore-input");
          if (field) { field.setAttribute("placeholder", examplePlaceholder()); field.setAttribute("aria-label", askAvailable() ? "Ask about this code by meaning or by name" : "Find an entity by name"); }
          return h("p", { class: "lede lede-soft" }, askAvailable()
          ? "Ask by meaning or by name, then open an entity to see what relies on it."
          : "Find an entity by name, then open it to see what relies on it.");
        })),
      h("form", { class: "ask", role: "search", on: { submit: (event) => { event.preventDefault(); runAsk(input.value, true); } } },
        h("div", { class: "ask-box" }, icon("search"), input, h("kbd", { "aria-hidden": "true" }, "/"))),
      h("p", { class: "ask-hint" }, h("span", {}, h("kbd", {}, "↑"), " ", h("kbd", {}, "↓"), " to move"), h("span", {}, h("kbd", {}, "Enter"), " to open"), h("span", {}, h("kbd", {}, "Esc"), " to clear")),
      section("explore-results", ["graph", "overview"], exploreResults, { class: "results", role: "region", "aria-label": "Results", "aria-live": "polite" })];
  }

  function entityRow(row) {
    const where = containerOf(row);
    const n = reliedOn(row);
    const signature = row.overloads > 1 ? "" : text(row.signature);
    const summary = text(row.summary);
    const detail = summary && (!signature || signature.length < 28) ? summary : signature;
    // A meaning match is what a search is for, so only a name match is said.
    const how = array(row.matched).includes("name") ? "Name match" : "";
    // A module is relied on through what it holds, so it carries no count of its own, as on Home.
    const side = isCount(n) && n > 0 && text(row.kind).toLowerCase() !== "module" ? relyWords(n) : how;
    return h("li", {}, h("a", { class: "entity-row", href: entityHref(row.id), "data-entity": row.id, title: text(row.why) || undefined },
      h("div", { class: "entity-main" },
        h("div", { class: "entity-line" }, h("span", { class: "entity-name" }, row.name), row.overloads > 1 ? h("span", { class: "entity-overloads" }, `· ${count(row.overloads)} overloads`) : null, pill(kindWord(row.kind) || "Entity"), where && !row.name.startsWith(`${where}.`) ? h("span", { class: "entity-container" }, `in ${where}`) : null),
        detail ? h("div", { class: detail === signature ? "entity-signature" : "entity-why" }, detail) : null),
      h("span", { class: "entity-side" }, side)));
  }

  function exploreResults() {
    const ex = state.explore;
    if (!ex.query.trim()) {
      const rows = topEntities();
      const status = topState();
      if (!rows.length) {
        if (status === "error") return readFailure(read("overview").missing ? "graph" : "overview");
        if (status === "reading") return skeletonLines(6);
        return h("p", { class: "gap" }, "Nothing to suggest yet. Ask about the code above.");
      }
      return h("div", { class: "result-group" }, h("div", { class: "result-head" }, h("h2", {}, "Most relied on"), h("span", { class: "aside" }, "A good place to start")),
        h("ul", { class: "list list-card", id: "explore-list" }, rows.slice(0, 12).map((row) => entityRow(row))));
    }
    if (ex.error) return failed(isSlow(ex.error) ? "Kin is still working on that search. Try again in a moment." : "Kin couldn't run that search.", () => runAsk(ex.query, true), ex.error);
    if (!ex.results) return skeletonLines(5);
    const groups = ex.results.groups.filter((group) => group.rows.length);
    if (!groups.length) return h("p", { class: "gap" }, askAvailable() ? `Nothing matched “${ex.query.trim()}”. Try a name, or describe what the code does.` : `No entity is named like “${ex.query.trim()}”.`);
    return [ex.results.notice ? h("p", { class: "note" }, ex.results.notice) : null, ...groups.map((group) => h("div", { class: "result-group" },
      h("div", { class: "result-head" }, h("h2", {}, group.title), h("span", { class: "aside" }, (() => {
        // A folded row stands for each of its overloads.
        const shown = group.rows.reduce((sum, row) => sum + (row.overloads || 1), 0);
        return group.total > shown ? `${count(shown)} of ${count(group.total)}` : plural(shown, "entity", "entities");
      })())),
      h("ul", { class: "list list-card" }, group.rows.map((row) => entityRow(row)))))];
  }

  function queueAsk(value) {
    state.explore.query = value;
    clearTimeout(state.exploreTimer);
    state.exploreTimer = setTimeout(() => runAsk(value, false), 260);
    try { history.replaceState(null, "", value.trim() ? `#/explore?q=${encodeURIComponent(value)}` : "#/explore"); } catch (_) { /* The query still runs. */ }
  }

  /**
   * Normalise an answer into result groups. The ask read (`kin.workspace.ask.v1`)
   * is already ranked, names and meaning together; the older graph read gives
   * name matches only.
   */
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
    return out;
  }

  function askGroups(data) {
    const rows = (list) => foldOverloads(array(list).filter((row) => row && UUID.test(row.id) && typeof row.name === "string"));
    if (data?.schema === "kin.workspace.ask.v1" || Array.isArray(data?.results)) {
      const list = rows(data.results);
      const title = data.mode === "name_only" ? "By name" : data.mode === "meaning_only" ? "By meaning" : "Best matches";
      return { groups: [{ title, rows: list, total: isCount(data.total) ? Math.max(data.total, list.length) : list.length }],
        notice: data.mode && data.mode !== "meaning_and_name" ? text(data.notice) : "" };
    }
    if (data?.inventory) {
      const list = rows(data.inventory.entities);
      return { groups: [{ title: "By name", rows: list, total: isCount(data.inventory.total) ? data.inventory.total : list.length }], notice: "" };
    }
    return { groups: [], notice: "" };
  }

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
      let data = null;
      if (askAvailable()) {
        try { data = await fetchJSON(`/api/workspace/ask?q=${encodeURIComponent(query)}&limit=20`); }
        catch (error) { if ([404, 405, 501].includes(error.status)) { state.askMissing = true; paint("explore-lede"); } else throw error; }
      }
      if (!data) data = await fetchJSON(`/api/workspace/graph?query=${encodeURIComponent(query)}&offset=0`);
      if (mine !== ex.request) return;
      ex.results = askGroups(data);
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
   * The entity read, normalised so the sectioned read (`kin.workspace.entity.v2`)
   * and the older source-and-neighbourhood read draw the same page. Each part
   * keeps its own `ok` and `reason`, so one failed part never blanks the rest.
   */
  function entityView(response, pending = new Set()) {
    const id = response.id;
    const waiting = (key) => !(key in response) && pending.has(key);
    if (response.schema === "kin.workspace.entity.v2" || response.header) {
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
              // An older server does not say; every proven row then counts.
              relies: row.relies === undefined ? row.confirmed !== false : row.relies === true,
              declarations: isCount(row.declarations) && row.declarations > 1 ? row.declarations : 0 })),
            total: isCount(reliers.total) ? reliers.total : listOf(reliers).length,
            reliesTotal: isCount(reliers.relies_total) ? reliers.relies_total : null,
            unproven: isCount(reliers.total) && isCount(reliers.confirmed) ? reliers.total - reliers.confirmed : 0,
            bound: text(reliers.bound), notice: text(reliers.notice) }
          : { ok: false, items: [], total: 0, reason: reasonOf(reliers, "What relies on it could not be read.") },
        uses: section_ok(uses)
          ? { ok: true, items: dedupe(listOf(uses)).map((row) => ({ ...row, relation: text(row.relationship), through: array(row.through).filter((name) => typeof name === "string") })), total: isCount(uses.total) ? uses.total : listOf(uses).length,
            throughMembers: uses.through_members === true, notice: text(uses.notice) }
          : { ok: false, items: [], total: 0, reason: reasonOf(uses, "What it relies on could not be read.") },
        tests: section_ok(tests)
          ? { ok: true, items: dedupe(listOf(tests)).map((row) => ({ ...row, test: true })), total: isCount(tests.total) ? tests.total : listOf(tests).length, bound: text(tests.bound) }
          : { ok: false, items: [], total: 0, reason: reasonOf(tests, "The tests that reach it could not be read.") },
        members: section_ok(response.members) ? { ok: true, items: listOf(response.members), total: isCount(response.members.total) ? response.members.total : listOf(response.members).length } : null,
        impact: impact ? section_ok(impact)
          ? { ok: true, direct: isCount(impact.direct) ? impact.direct : null, transitive: isCount(impact.transitive) ? impact.transitive : null, tests: isCount(impact.tests) ? impact.tests : null, depth: isCount(impact.depth) ? impact.depth : null,
            top: array(impact.top).filter((row) => row && UUID.test(row.id)).map((row) => ({ ...row, test: isTest(row) })), bound: text(impact.bound), notice: text(impact.notice) }
          : { ok: false, reason: reasonOf(impact, "What a change would touch could not be read.") } : null,
        history: waiting("history") ? { ok: false, pending: true, items: [] } : history ? section_ok(history)
          ? { ok: true, items: array(history.items).filter((row) => row && (row.change_id || row.id)).map((row) => ({ id: row.change_id || row.id, title: text(row.title), author: authorOf(row), time: row.time || row.timestamp })), total: isCount(history.total) ? history.total : array(history.items).length }
          : { ok: false, reason: reasonOf(history, "This entity's history could not be read.") } : null,
        editor: section_ok(editor) ? graphApi()?.editorUri?.(editor.uri, id) || null : null,
        editorReason: !section_ok(editor) ? text(editor.reason) : "",
      };
    }
    // The older read: a source read and a one-step neighbourhood.
    const G = graphApi();
    const source = response.source && typeof response.source === "object" ? response.source : null;
    const neighborhood = response.neighborhood && typeof response.neighborhood === "object" ? response.neighborhood : null;
    let rows = [];
    let relationsError = response.neighborhood_error || (neighborhood ? null : "What it connects to could not be read.");
    if (neighborhood && G?.neighborRows) {
      try { rows = G.neighborRows(neighborhood, id); } catch (error) { relationsError = error.message; }
    }
    const dedupe = (list) => { const seen = new Set(); return list.filter((row) => !seen.has(row.id) && seen.add(row.id)); };
    const incoming = dedupe(rows.filter((row) => row.direction === "incoming" && row.id && row.relation.toLowerCase() !== "contains")).map((row) => ({ ...row, kind: row.entityKind, test: isTest(row), confirmed: true, relies: true }));
    const outgoing = dedupe(rows.filter((row) => row.direction === "outgoing" && row.id && row.relation.toLowerCase() !== "contains")).map((row) => ({ ...row, kind: row.entityKind }));
    const meta = { ...(array(neighborhood?.entities).find((row) => row?.id === id) || {}), ...(source || {}) };
    const failedPart = (reason) => ({ ok: false, items: [], total: 0, reason });
    return {
      id,
      name: text(meta.name) || "Entity",
      kind: text(meta.kind),
      container: containerOf(meta),
      summary: text(meta.summary),
      signature: text(meta.signature),
      language: text(source?.language),
      source: typeof source?.body === "string" ? { ok: true, text: source.body, startLine: null } : { ok: false, reason: "No source was returned for this entity." },
      reliers: relationsError ? failedPart(relationsError) : { ok: true, items: incoming, total: incoming.length },
      uses: relationsError ? failedPart(relationsError) : { ok: true, items: outgoing, total: outgoing.length },
      tests: relationsError ? failedPart(relationsError) : { ok: true, items: incoming.filter((row) => row.test), total: incoming.filter((row) => row.test).length },
      members: null,
      impact: null,
      history: null,
      editor: G?.editorUri?.(response.editor_uri, id) || null,
      editorReason: text(response.editor_unavailable),
    };
  }

  // The quick parts of an entity answer in about a second; what relies on it
  // and its history are slower walks, read second and drawn when they land.
  const ENTITY_FIRST = "header,source,relies_on,members,impact,tests,editor";
  const ENTITY_LATER = ["relied_on_by", "history"];

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
      let first;
      try { first = await fetchJSON(`${base}&sections=${ENTITY_FIRST}`); }
      catch (error) {
        // A server that does not read sections answers the whole entity at once.
        if (error.status !== 400) throw error;
        first = await fetchJSON(base);
      }
      if (mine !== entity.request) return;
      const later = ENTITY_LATER.filter((key) => !(key in first) && Array.isArray(first.sections));
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
    for (const id of ["entity-head", "entity-map", "entity-source", "entity-impact", "entity-relations"]) paint(id);
    // An entity the current code does not hold has nothing to draw below its line.
    for (const id of ["entity-map-card", "entity-impact-card", "entity-layout"]) { const node = byId(id); if (node) node.hidden = Boolean(state.entity.missing); }
    if (state.entity.data) document.title = `${state.entity.data.name} · ${session() ? repositoryLabel(session().repository) : "Kin"}`;
  }

  function entity() {
    const missing = Boolean(state.entity.missing);
    return [
      section("entity-head", [], entityHead),
      h("section", { class: "map-card", id: "entity-map-card", hidden: missing, "aria-labelledby": "entity-map-title" }, section("entity-map", [], entityMap)),
      h("section", { class: "section", id: "entity-impact-card", hidden: missing, "aria-labelledby": "entity-impact-title" }, section("entity-impact", [], entityImpact)),
      h("div", { class: "entity-layout", id: "entity-layout", hidden: missing },
        h("section", { class: "section", "aria-labelledby": "entity-source-title" }, section("entity-source", [], entitySource)),
        h("aside", { class: "section", "aria-label": "Connections and history" }, section("entity-relations", [], entityRelations)))];
  }

  const retryEntity = () => loadEntity(state.entity.id);
  /** Whether the source's own first line already reads as the signature. */
  function sourceShowsSignature(data) {
    if (!data.source?.ok || !data.signature) return false;
    const squash = (value) => String(value).replace(/\s+/g, " ").trim();
    const first = squash(data.source.text.split("\n").find((line) => line.trim()) || "");
    const signature = squash(data.signature);
    return first.startsWith(signature) || signature.startsWith(first.replace(/:$/, ""));
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
   * Kin's own ranking number when the read carried it (Home's tile says the
   * same), otherwise the rows that rely on it.
   */
  function relyCount(data) {
    if (isCount(data.dependents)) return data.dependents;
    if (!data.reliers.ok) return null;
    return isCount(data.reliers.reliesTotal) ? data.reliers.reliesTotal : entityCount(relyingRows(data));
  }

  function entityHead() {
    const e = state.entity;
    const data = e.data;
    const crumbs = h("nav", { class: "crumbs", "aria-label": "Breadcrumb" }, h("a", { href: "#/explore" }, "Explore"), data?.container ? [h("span", { "aria-hidden": "true" }, "/"), h("span", {}, data.container)] : null);
    if (!data) {
      if (e.missing) {
        return [crumbs, h("h1", { class: "entity-title entity-title-plain" }, "This entity isn't in the current code."),
          h("p", { class: "lede lede-soft" }, "It may have been renamed or removed."),
          h("div", { class: "entity-actions" }, link("Search for it", "#/explore", true),
            h("details", { class: "more" }, h("summary", {}, "Show details"), h("p", { class: "detail-text" }, e.error)))];
      }
      if (e.error) return [crumbs, h("div", { class: "section" }, failed(isSlow(e.error) ? "Kin is still working on this entity. Try again in a moment." : "Kin couldn't load this entity.", retryEntity, e.error))];
      return [crumbs, skeleton("skeleton-lede"), skeleton("skeleton-line")];
    }
    const fact = (n, words, ok = true) => ok ? h("span", { class: "fact" }, h("strong", {}, count(n)), ` ${words}`) : null;
    const relying = relyCount(data);
    const facts = h("div", { class: "entity-facts" },
      data.kind ? pill(kindWord(data.kind), "accent") : null,
      data.container && !data.name.startsWith(`${data.container}.`) ? h("span", { class: "fact" }, "in ", h("strong", {}, data.container)) : null,
      fact(relying, relying === 1 ? "relies on it" : "rely on it", isCount(relying)),
      fact(data.tests.total, data.tests.total === 1 ? "test reaches it" : "tests reach it", data.tests.ok),
      data.uses.ok && data.uses.total ? h("span", { class: "fact" }, "relies on ", h("strong", {}, count(data.uses.total))) : null,
      data.members?.ok && data.members.total ? fact(data.members.total, data.members.total === 1 ? "member" : "members") : null);
    const feedback = h("span", { class: "note", role: "status" });
    const actions = h("div", { class: "entity-actions" });
    if (data.editor) {
      actions.append(h("a", { class: "button button-primary", href: data.editor, on: { click: () => { feedback.textContent = "Sent to VS Code. If nothing opens, install Kin for VS Code and open this repository there."; } } }, icon("editor"), "Open in VS Code"));
    }
    actions.append(button("Copy entity ID", async () => {
      try { await navigator.clipboard.writeText(data.id); feedback.textContent = "Copied."; }
      catch (_) { feedback.textContent = data.id; }
    }, { class: "button" }), feedback);
    if (!data.editor) feedback.textContent = data.editorReason || "This entity can't be opened in an editor from here.";
    return [crumbs, h("h1", { class: "entity-title" }, data.name),
      data.summary ? h("p", { class: "entity-summary" }, docText(data.summary)) : null,
      data.headerReason ? h("p", { class: "note" }, data.headerReason) : null,
      facts,
      data.signature && !sourceShowsSignature(data) ? h("pre", { class: "entity-signature-block", tabindex: "0", "aria-label": "Signature" }, data.signature) : null,
      actions,
      e.error ? h("div", { class: "section" }, failed("Kin couldn't refresh this entity. Showing what it last saw.", retryEntity, e.error)) : null];
  }

  function entityMap(onCleanup) {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    const title = h("h2", { class: "map-title", id: "entity-map-title" }, data ? `Around ${data.name}` : "Around this entity");
    const stage = h("div", { class: "map-stage map-stage-focus" });
    if (!data) {
      stage.append(h("div", { class: "map-empty" }, state.entity.error ? h("p", { class: "gap" }, "Nothing to draw until this entity is read.") : skeleton("skeleton-block")));
      return [title, stage];
    }
    // The left column shows what relies on it and, when both exist, keeps
    // room for a few of the tests that reach it.
    const plainReliers = data.reliers.items.filter((row) => !row.test);
    const testRows = [...data.reliers.items.filter((row) => row.test), ...data.tests.items];
    const leftRows = [...plainReliers.slice(0, testRows.length ? 8 : 12), ...testRows, ...plainReliers.slice(testRows.length ? 8 : 12)];
    if (!leftRows.length && !data.uses.items.length) {
      if (data.reliers.pending) {
        stage.append(h("div", { class: "map-empty" }, skeleton("skeleton-block")));
        return [title, stage];
      }
      const reason = !data.reliers.ok ? data.reliers.reason : !data.uses.ok ? data.uses.reason : !data.tests.ok ? data.tests.reason : null;
      stage.append(h("div", { class: "map-empty" }, reason ? failed(reason, retryEntity) : h("p", { class: "gap" }, "Nothing connects to this entity yet.")));
      return [title, stage];
    }
    const PER_SIDE = 12;
    const nodes = [{ id: data.id, name: data.name, kind: data.kind, weight: 0, role: "focal", group: "" }];
    const seen = new Set([data.id]);
    for (const row of leftRows) if (!seen.has(row.id)) { seen.add(row.id); nodes.push({ id: row.id, name: row.name, kind: row.kind, weight: isCount(row.sites) ? row.sites : 0, role: row.test ? "test" : "in", group: "" }); }
    for (const row of data.uses.items) if (!seen.has(row.id)) { seen.add(row.id); nodes.push({ id: row.id, name: row.name, kind: row.kind, weight: 0, role: "out", group: "" }); }
    const edges = nodes.filter((node) => node.id !== data.id).map((node) => ({ source: node.role === "out" ? data.id : node.id, target: node.role === "out" ? node.id : data.id }));
    const inCount = nodes.filter((node) => node.role === "in" || node.role === "test").length;
    const outCount = nodes.filter((node) => node.role === "out").length;
    const hidden = Math.max(0, inCount - PER_SIDE) + Math.max(0, outCount - PER_SIDE);
    const G = graphApi();
    const legend = h("p", { class: "map-caption" },
      h("span", { class: "map-legend" }, h("span", { class: "swatch swatch-in" }), "Relies on it"),
      h("span", { class: "map-legend" }, h("span", { class: "swatch swatch-test" }), "Tests"),
      h("span", { class: "map-legend" }, h("span", { class: "swatch swatch-out" }), "It relies on"),
      hidden ? h("span", {}, `${count(hidden)} more listed below`) : null);
    const mount = () => {
      if (stage.isConnected === false) return;
      try {
        const controller = G.createMap(stage, { nodes, edges, groups: [] }, {
          mode: "focus", focalId: data.id, perSide: PER_SIDE, tip: byId("workspace-tip"), reserveBottom: 30,
          summary: `${data.name} at the centre. ${relyWords(relyCount(data)) || "What relies on it is still loading"}, and it relies on ${plural(data.uses.total, "entity", "entities")}.`,
          onOpen: (id) => { location.hash = entityHref(id); },
          describe: (node) => [kindWord(node.kind), node.role === "out" ? "It relies on this" : node.role === "test" ? "A test that relies on it" : node.role === "in" ? "Relies on it" : ""].filter(Boolean).join(" · "),
        });
        onCleanup(() => controller.dispose());
      } catch (_) { stage.replaceChildren(h("div", { class: "map-empty" }, h("p", { class: "gap" }, "The map could not be drawn in this browser."))); }
    };
    if (G?.createMap) { if (globalThis.requestAnimationFrame) requestAnimationFrame(mount); else mount(); }
    return [title, stage, legend];
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
    const head = h("div", { class: "section-head" }, h("h2", { id: "entity-source-title" }, "Source"), data?.language ? h("span", { class: "aside" }, kindWord(data.language)) : null);
    if (!data) return [head, state.entity.error ? null : skeletonLines(8)];
    if (!data.source.ok) return [head, failed(data.source.reason, retryEntity)];
    return [head, codeView(data.source.text, data.language, data.source.startLine, state.entity.expanded, () => { state.entity.expanded = true; paint("entity-source"); })];
  }

  const IMPACT_WORD = { contract_consumer: "uses its contract", runtime_caller: "calls it", type_dependent: "uses its type", dependent: "relies on it", test: "Test", derived: "derived from it" };
  const steps = (hops) => isCount(hops) && hops > 1 ? `${hops} steps away` : "";

  function relierItem(row, caption) {
    return h("li", {}, h("a", { class: `relier${row.test ? " relier-test" : ""}`, href: entityHref(row.id), title: row.name }, h("span", { class: "relier-name" }, row.name), h("span", { class: "relier-caption" }, caption)));
  }
  /**
   * One branch of the tree: a title with its count, the first rows, and a
   * button that shows the rest in place. Rows Kin counted but did not list
   * are said as such.
   */
  function relierCard(key, title, rows, total, caption, shown = 6) {
    const all = Boolean(state.entity.showAll?.[key]);
    const visible = all ? rows : rows.slice(0, shown);
    const hidden = rows.length - visible.length;
    const unlisted = Math.max(0, total - entityCount(rows));
    // In a narrow branch an overloaded name keeps its room: the count is its caption.
    const said = (row) => row.declarations ? `${count(row.declarations)} overloads` : caption(row);
    return h("li", {}, h("div", { class: "reliers" }, h("p", { class: "reliers-head" }, title, isCount(total) ? h("span", { class: "count" }, count(total)) : null),
      h("ul", { class: "reliers-list" }, visible.map((row) => relierItem(row, said(row))),
        hidden > 0 ? h("li", { class: "relier-more" }, h("button", { type: "button", class: "relier relier-more-button", on: { click: () => { state.entity.showAll = { ...(state.entity.showAll || {}), [key]: true }; paint("entity-impact"); } } }, h("span", { class: "relier-name" }, `Show ${count(hidden)} more`))) : null,
        !hidden && unlisted > 0 ? h("li", { class: "relier-more" }, h("span", { class: "relier" }, h("span", { class: "relier-name" }, `${count(unlisted)} more not listed`))) : null)));
  }

  function entityImpact() {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    const head = h("div", { class: "section-head" }, h("h2", { id: "entity-impact-title" }, "What a change would touch"));
    if (!data) return [head, state.entity.error ? null : skeletonLines(4)];
    if (data.impact && !data.impact.ok) return [head, failed(data.impact.reason, retryEntity)];
    if (!data.impact && !data.reliers.ok) return [head, failed(data.reliers.reason, retryEntity)];
    const impact = data.impact;
    const relying = relyCount(data);
    const reach = impact?.transitive ?? null;
    const tests = impact?.tests ?? data.tests.total;
    const testWords = tests ? `${spoken(tests)} ${tests === 1 ? "test reaches" : "tests reach"} it.` : "No test reaches it.";
    let sentence;
    if (isCount(reach) && reach > 0) sentence = `A change here would reach ${plural(reach, "entity", "entities")}${impact.depth ? ` within ${impact.depth} steps` : ""}. ${testWords}`;
    else if (reach === 0 || relying === 0) sentence = `Nothing relies on ${data.name}. A change here stays inside it.`;
    else if (isCount(relying)) sentence = `${relyWords(relying)}. ${testWords}`.replace(/^\w/, (c) => c.toUpperCase());
    else sentence = "Kin is still working out what relies on it.";
    const SHOWN = 6;
    const relyingList = relyingRows(data);
    const cards = [];
    if (data.reliers.pending) cards.push(h("li", {}, h("div", { class: "reliers", "aria-busy": "true" }, h("p", { class: "reliers-head" }, "Relies on it"), skeletonLines(3))));
    else if (!data.reliers.ok) cards.push(h("li", {}, h("div", { class: "reliers" }, h("p", { class: "reliers-head" }, "Relies on it"), failed(data.reliers.reason, retryEntity))));
    else if (relyingList.length) cards.push(relierCard("relies", "Relies on it", relyingList, relying ?? entityCount(relyingList), (row) => row.test ? "Test" : RELATION_WORD[text(row.relation).toLowerCase()] || kindWord(row.kind) || "Entity", SHOWN));
    const further = array(impact?.top).filter((row) => isCount(row.hops) ? row.hops > 1 : !data.reliers.items.some((other) => other.id === row.id));
    if (further.length) cards.push(relierCard("further", "Further out", further, further.length, (row) => row.test ? "Test" : steps(row.hops) || IMPACT_WORD[row.reason] || kindWord(row.kind), SHOWN));
    if (data.tests.items.length) cards.push(relierCard("tests", "Tests that reach it", data.tests.items, data.tests.total, (row) => steps(row.hops) || "Test", SHOWN));
    const notice = impact?.notice || data.reliers.notice || "";
    return [head, h("div", { class: "impact" }, h("p", { class: "impact-sentence" }, sentence),
      cards.length ? h("div", { class: "impact-scroll" }, h("ul", { class: "tree" }, h("li", {},
        h("div", { class: "node" }, h("span", { class: "node-name" }, data.name), h("span", { class: "node-caption" }, "This entity")),
        h("ul", {}, cards)))) : null,
      notice ? h("p", { class: "note" }, notice) : null)];
  }

  const RELATION_WORD = { calls: "calls it", references: "refers to it", overrides: "overrides it", extends: "extends it", implements: "implements it", imports: "imports it", includes: "includes it", uses_type: "uses its type", instantiates: "constructs it", tests: "tests it", covers: "covers it" };
  const USES_WORD = { calls: "calls", references: "refers to", overrides: "overrides", extends: "extends", implements: "implements", imports: "imports", includes: "includes", uses_type: "uses type", instantiates: "constructs" };

  function relGroup(key, title, part, caption, empty) {
    const LIMIT = 12;
    const all = state.entity.showAll?.[key];
    const rows = part.items;
    const visible = all ? rows : rows.slice(0, LIMIT);
    const head = h("h3", { class: "rel-head" }, title, part.ok ? h("span", { class: "count" }, count(part.total)) : null);
    if (part.pending) return h("div", { class: "rel-group", "aria-busy": "true" }, head, skeletonLines(3));
    if (!part.ok) return h("div", { class: "rel-group" }, head, failed(part.reason, retryEntity));
    if (!rows.length) return h("div", { class: "rel-group" }, head, h("p", { class: "gap" }, empty));
    const more = rows.length - visible.length;
    const unlisted = Math.max(0, part.total - entityCount(rows));
    return h("div", { class: "rel-group" }, head,
      h("div", { class: "rel-list" }, visible.map((row) => h("a", { class: `rel-row${row.test ? " rel-row-test" : ""}${row.confirmed === false ? " rel-row-unproven" : ""}`, href: entityHref(row.id),
        title: row.confirmed === false ? `${row.name}: Kin matched this by name and could not prove it uses this entity.` : row.name },
        h("span", { class: "rel-name" }, row.name, row.declarations ? h("span", { class: "rel-extra" }, ` · ${count(row.declarations)} overloads`) : null),
        h("span", { class: "rel-how" }, row.confirmed === false ? "by name only" : caption(row)))),
        more > 0 ? h("button", { type: "button", class: "rel-row rel-more-button", on: { click: () => { state.entity.showAll = { ...(state.entity.showAll || {}), [key]: true }; paint("entity-relations"); } } }, `Show ${count(more)} more`) : null,
        unlisted > 0 && !more ? h("div", { class: "rel-row" }, h("span", { class: "rel-more" }, `${count(unlisted)} more not listed`)) : null),
      part.notice ? h("p", { class: "note" }, part.notice) : null);
  }

  function entityRelations() {
    if (state.entity.missing) return null;
    const data = state.entity.data;
    if (!data) return state.entity.error ? null : skeletonLines(10);
    // What relies on it and the tests that reach it are listed once, in the
    // tree above. This column holds what the tree does not.
    const uses = { ...data.uses, items: data.uses.items };
    const usesCaption = (row) => row.through?.length
      ? `through ${row.through[0].split(".").pop()}${row.through.length > 1 ? ` and ${count(row.through.length - 1)} more` : ""}`
      : USES_WORD[text(row.relation).toLowerCase()] || "relies on";
    const out = [
      relGroup("uses", data.uses.throughMembers ? "It and its members rely on" : "It relies on", uses, usesCaption,
        data.members?.ok && data.members.total ? "Neither it nor its members rely on anything else in this code." : "It relies on nothing else in this code."),
    ];
    const builds = data.reliers.ok ? data.reliers.items.filter((row) => row.confirmed && !row.relies) : [];
    if (builds.length) out.push(relGroup("builds", "Builds on it", { ok: true, items: builds, total: entityCount(builds) }, (row) => RELATION_WORD[text(row.relation).toLowerCase()] || "builds on it", ""));
    const guessed = data.reliers.ok ? data.reliers.items.filter((row) => !row.confirmed) : [];
    if (guessed.length) out.push(relGroup("guessed", "Matched by name only", { ok: true, items: guessed, total: entityCount(guessed), notice: "Kin found these by name and couldn't confirm they use it." }, () => "by name only", ""));
    if (data.members?.ok && data.members.items.length) out.push(relGroup("members", "Members", data.members, (row) => kindWord(row.kind), ""));
    const history = data.history;
    const head = h("h3", { class: "rel-head" }, "History", history?.ok ? h("span", { class: "count" }, count(history.total)) : null);
    let body;
    if (history?.pending) body = skeletonLines(3);
    else if (!history) body = h("p", { class: "gap" }, "This version of Kin does not list an entity's history here.");
    else if (!history.ok) body = failed(history.reason, retryEntity);
    else if (!history.items.length) body = h("p", { class: "gap" }, "No change has touched it yet.");
    else body = h("div", { class: "rel-list" }, history.items.slice(0, 10).map((change) => h("a", { class: "history-row", href: changeURL(change.id) },
      h("div", { class: "history-title" }, change.title || "Untitled change"),
      h("div", { class: "history-meta" }, [change.author, ago(change.time)].filter(Boolean).join(" · ")))));
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
        mergeChange ? h("p", {}, h("a", { href: changeURL(mergeChange) }, "Review the merge")) : report.merge_change != null ? h("p", {}, "Kin couldn't read which change the merge made. Check the repository for the result.") : null,
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
        branch.result = result;
        branch.preview = null;
        branch.confirmed = false;
        clearBranchReceipt();
        // Every view reads again after a switch. Old source and old merge
        // expectations cannot carry over to the new branch.
        state.readEpoch += 1;
        for (const resource of Object.values(state.reads)) { resource.data = null; resource.error = null; resource.at = null; resource.busy = false; resource.sig = null; resource.changed = true; }
        state.entity.data = null;
        state.explore.results = null;
        state.merge.preview = null;
        state.merge.result = null;
        state.merge.confirmed = false;
        announce("Branch switched. Loading the new branch.");
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
  function branchPanel() {
    const branch = state.branch;
    if (!switchShown()) return null;
    const choices = branchChoices();
    // Previewing only reads; confirming waits until the page's own reads settle.
    const busy = workspaceWriteBusy() || !session()?.capabilities?.branch_switch;
    const confirmLocked = busy || state.refreshing;
    const current = choices.find((row) => row.active);
    const others = choices.filter((row) => !row.active);
    const select = h("select", { id: "workspace-branch-target", disabled: busy || Boolean(read("branchState").error), on: { change: (event) => {
      branch.target = event.target.value; branch.preview = null; branch.confirmed = false; branch.result = null; branch.error = null;
      updateBranchPanel();
      byId("workspace-branch-target")?.focus();
    } } }, h("option", { value: "" }, "Choose a branch"), others.map((row) => h("option", { value: row.name?.bytes_hex, selected: row.name?.bytes_hex === branch.target }, shortRef(mergeRef(row.name)) || "Unreadable branch name")));
    const evidence = h("div", { class: "evidence", id: "workspace-branch-evidence", tabindex: "-1" });
    if (branch.error) evidence.append(h("div", { class: "message message-warn" }, h("div", {}, h("strong", {}, branch.error),
      !branch.unknown && !branch.operationId ? h("p", {}, workInTheWay(branch.diagnostic) || "Nothing was switched.") : null,
      branch.diagnostic || branch.operationId ? h("details", { class: "more" }, h("summary", {}, "Show details"), branch.diagnostic ? h("p", {}, branch.diagnostic) : null, branch.operationId ? h("p", {}, "Operation ", h("code", {}, branch.operationId)) : null) : null)));
    if (branch.preview) {
      const preview = branch.preview;
      const pending = preview.pending_work;
      const checkbox = h("input", { type: "checkbox", id: "workspace-branch-confirm", checked: branch.confirmed, disabled: confirmLocked, on: { change: (event) => {
        branch.confirmed = event.target.checked; byId("workspace-branch-submit").disabled = !branch.confirmed || confirmLocked;
      } } });
      evidence.append(h("div", { class: "preview" }, h("h3", {}, `Switch to ${shortRef(preview.target.display)}`),
        h("dl", { class: "heads" }, detail("From", shortRef(preview.source?.display) || "No branch", true), detail("To", shortRef(preview.target.display), true)),
        h("details", { class: "more" }, h("summary", {}, "Show details"), h("dl", { class: "heads" }, detail("Its latest change", mergeHash(preview.target.change), true))),
        h("div", { class: `message message-${pending.present ? "warn" : "good"}` }, h("div", {}, h("strong", {}, pending.present ? "Uncommitted work comes along" : "No uncommitted work"), h("p", {}, pending.detail))),
        preview.limitations.length ? h("ul", { class: "limits" }, preview.limitations.map((line) => h("li", {}, line))) : null,
        h("div", { class: "confirm" },
          h("label", { class: "check", for: "workspace-branch-confirm" }, checkbox, h("span", {}, `I've checked what switching to ${shortRef(preview.target.display)} does to my uncommitted work.`)),
          button(branch.busy === "confirm" ? "Switching…" : "Switch branch", () => branchRequest("confirm"), { id: "workspace-branch-submit", class: "button button-primary", disabled: confirmLocked || !branch.confirmed }))));
    }
    if (branch.result) {
      const selected = branch.result.report.branches.find((row) => row.active);
      evidence.append(h("div", { class: "message message-good" }, h("div", {}, h("strong", {}, `Switched to ${selected ? shortRef(mergeRef(selected.name)) : "the chosen branch"}`), h("p", {}, "This page is loading the new branch."), operationDetails(branch.result.operation_id))));
    }
    const close = !branch.busy && !branch.unknown ? closeButton("Close", () => { state.switchOpen = false; branch.preview = null; branch.result = null; branch.error = null; branch.confirmed = false; updateBranchPanel(); }) : null;
    return h("section", { class: "card panel", "aria-labelledby": "switch-title" }, h("div", { class: "panel-head" }, h("h2", { id: "switch-title" }, "Switch branch"), close),
      h("p", {}, others.length ? "Preview what a switch does to your uncommitted work, then confirm." : `You are on ${current ? shortRef(mergeRef(current.name)) : shortRef(activeBranch()?.name) || "no branch"}, the only branch.`),
      read("branchState").error ? h("div", { class: "section" }, readFailure("branchState")) : null,
      !session()?.capabilities?.branch_switch ? h("p", { class: "note" }, session()?.branch_unavailable_reason || "Switching is not available in this session.") : null,
      !read("branchState").data ? (read("branchState").error ? null : skeletonLines(1)) : !others.length ? null : h("form", { class: "panel-form workspace-branch-form", on: { submit: (event) => { event.preventDefault(); branchRequest("preview"); } } },
        h("div", { class: "field" }, h("label", { for: "workspace-branch-target" }, "Switch to"), select),
        h("button", { type: "submit", class: "button", disabled: busy || !branchTarget() || Boolean(read("branchState").error) }, branch.busy === "preview" ? "Loading…" : "Preview switch")),
      state.merge.unknown || state.merge.busy ? h("p", { class: "note" }, "Finish checking the pending merge before switching.") : null,
      evidence);
  }

  const conflictHash = (value) => typeof value === "string" && /^[0-9a-f]{64}$/.test(value) ? value : mergeHash(value);
  function shellArgument(value) { return `'${String(value).replaceAll("'", "'\"'\"'")}'`; }
  function conflictPanel() {
    const resource = read("conflicts");
    const held = resource.data;
    const wrap = (...children) => h("section", { class: "card panel", id: "workspace-conflicts", "aria-labelledby": "conflict-title" }, h("h2", { id: "conflict-title" }, "Merge in progress"), ...children);
    if (!held) return wrap(resource.error ? h("div", { class: "section" }, readFailure("conflicts")) : skeletonLines(2));
    if (held.schema !== "kin.workspace.conflicts.v1" || !held.report || !held.state) return wrap(h("p", {}, "The merge record could not be read. Resolving is unavailable."));
    const report = held.report;
    const active = held.state.active === true;
    if (!report.record) return h("p", { class: "note quiet-line", id: "workspace-conflicts" }, "No merge is waiting to be resolved.");
    const labels = { none: "No merge waiting", in_progress: "Needs resolution", committed: "Resolved", aborted: "Abandoned" };
    const content = [h("p", {}, pill(labels[held.state.status] || "Unknown state", active ? "warn" : ""), " ",
      active ? `${report.unresolved_count} unresolved, ${report.resolved_count} resolved. The branches are not merged yet.` : "The last merge's record, kept for reference."),
      h("details", { class: "more" }, h("summary", {}, "Show details"), h("dl", { class: "heads" },
        detail("Record", report.record_hash, true), detail("Target branch", mergeRef(report.record.binding?.target_ref), true), detail("Incoming branch", mergeRef(report.record.binding?.source_ref), true), detail("Common base", conflictHash(held.provenance?.base_change), true),
        detail("Target input", conflictHash(held.provenance?.ours_change), true), detail("Source input", conflictHash(held.provenance?.theirs_change), true)))];
    const bodies = array(report.bodies);
    for (const entry of array(report.record.entries)) {
      const subject = entry.subject || {};
      const id = subject.entity || subject.relation || subject.artifact;
      const body = bodies.find((row) => row.subject === `${subject.subject} ${id}`);
      const kind = { entity: "Entity", artifact: "Unparsed entity", relation: "Connection", path: "Location" }[subject.subject] || "Conflict";
      const title = subject.subject === "entity" || subject.subject === "relation" ? (entry.label || body?.label || kind) : kind;
      const resolved = entry.resolution?.resolution && entry.resolution.resolution !== "unresolved";
      const card = h("details", { class: "conflict-entry" }, h("summary", {}, h("strong", {}, title), pill(resolved ? "Resolved" : "Unresolved", resolved ? "good" : "warn")),
        h("p", { class: "note" }, `${kind} · ${String(entry.divergence?.divergence || "reason not recorded").replaceAll("_", " ")}`));
      if (subject.subject === "entity" && typeof id === "string" && UUID.test(id)) card.append(h("a", { class: "button button-small", href: entityHref(id) }, "Open the current entity"));
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
    const handoff = held.handoff || {};
    const commands = array(handoff.commands);
    if (commands.length) content.push(h("h3", { class: "section rel-head" }, "Resolve in a terminal"), h("p", { class: "note" }, "Run them in this repository. Copying a command does not run it. Read the page again after each step."));
    for (const unavailable of array(handoff.unavailable_subjects)) content.push(h("div", { class: "message message-warn" }, h("div", {}, h("strong", {}, "This conflict needs a different selector"), h("p", {}, unavailable.reason), h("code", {}, unavailable.selector))));
    for (const command of commands) {
      if (!Array.isArray(command.argv) || command.argv.some((arg) => typeof arg !== "string")) continue;
      const line = command.argv.map(shellArgument).join(" ");
      const status = h("span", { class: "note", role: "status" });
      const copy = button("Copy command", async () => {
        try { await navigator.clipboard.writeText(line); status.textContent = " Copied. Read it before running."; }
        catch (_) { status.textContent = " Select the command text to copy it."; }
      }, { class: "button button-small", disabled: Boolean(resource.error) || (command.modifies && !active) });
      content.push(h("div", { class: "command" }, h("strong", {}, command.label || "Command"), h("pre", { tabindex: "0" }, line), copy, status));
    }
    return wrap(...content);
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
    if (!held) { Promise.resolve().then(() => loadBranchGuide(row.name, base)); return h("div", { class: "branch-detail" }, skeleton("skeleton-line")); }
    if (held.busy) return h("div", { class: "branch-detail" }, skeleton("skeleton-line"));
    if (held.error) return h("div", { class: "branch-detail branch-failed" }, failed(isSlow(held.error) ? "Kin is still reading this branch." : "Kin couldn't load what this branch holds.", () => { state.branchGuides.delete(key); paint("branches-list"); }, held.error));
    const guide = held.data;
    const range = guide.range || {};
    const changes = array(range.changes);
    const latest = changes[changes.length - 1] || changes[0];
    const ahead = isCount(range.changes_total) ? range.changes_total : changes.length;
    const sentence = workSentence(guide) || (ahead ? "No entity changed." : `Nothing beyond ${base}.`);
    return h("div", { class: "branch-detail" },
      h("p", { class: "branch-sentence" }, sentence),
      h("p", { class: "branch-meta" }, ahead ? `${plural(ahead, "change")} ahead of ${base}` : `Even with ${base}`,
        latest ? [" · ", h("span", { class: "branch-latest" }, latest.title || "Untitled change"), [authorOf(latest), ago(latest.timestamp)].filter(Boolean).length ? ` · ${[authorOf(latest), ago(latest.timestamp)].filter(Boolean).join(", ")}` : ""] : null));
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
      return h("li", { class: "branch-row" },
        h("div", { class: "branch-main" },
          h("span", { class: "branch-name" }, icon("branch"), row.name, row.active ? pill("Current", "accent") : null, row.default ? pill("Default") : null),
          detail),
        row.active ? null : h("span", { class: "branch-actions" },
          current ? h("a", { class: "button button-small button-primary", href: reviewURL(`#/r/${encodeURIComponent(current)}..${encodeURIComponent(row.name)}`) }, "Review") : null,
          // Offered before the branch list's exact names are read; the panel
          // then shows that read, or its failure, until it can preview.
          session()?.capabilities?.branch_switch ? button("Switch", () => {
            state.switchOpen = true;
            if (choice) { state.branch.target = choice.name.bytes_hex; updateBranchPanel(); branchRequest("preview"); } else updateBranchPanel();
            byId("workspace-branch-host")?.scrollIntoView?.({ block: "start", behavior: "smooth" });
          }, { class: "button button-small", disabled: workspaceWriteBusy() }) : null,
          button(`Merge into ${current || "current"}`, () => { state.mergeOpen = true; state.merge.source = row.name; updateMergePanel(); mergeRequest("preview"); byId("workspace-merge-host")?.scrollIntoView?.({ block: "start", behavior: "smooth" }); }, { class: "button button-small", disabled: workspaceWriteBusy() || !session()?.token })));
    }));
  }

  function branches() {
    return [
      h("header", {}, h("h1", {}, "Branches"),
        section("branches-lede", ["branches"], () => {
          if (!read("branches").data) return read("branches").error ? null : skeleton("skeleton-lede");
          const active = activeBranch();
          const rows = branchRows();
          const others = rows.filter((row) => !row.active).map((row) => row.name);
          const which = others.length === 1 ? `One other branch: ${others[0]}.` : others.length <= 3 ? `${spoken(others.length)} other branches: ${others.slice(0, -1).join(", ")} and ${others.at(-1)}.` : `${spoken(others.length)} other branches.`;
          return h("p", { class: "lede lede-soft" }, active ? (others.length ? `You're on ${active.name}. ${which}` : `You're on ${active.name}, the only branch.`) : `${spoken(rows.length)} ${rows.length === 1 ? "branch" : "branches"}.`);
        })),
      h("section", { class: "section", "aria-label": "Branches" }, section("branches-list", ["branches", "branchState"], branchList)),
      section("branches-review", ["branches"], () => {
        const active = activeBranch();
        return active && defaultBranch() && active.name !== defaultBranch().name ? h("p", { class: "note" }, moreLink(`Review ${active.name} against ${defaultBranch().name}`, reviewURL(`#/r/${encodeURIComponent(defaultBranch().name)}..`))) : null;
      }),
      h("div", { id: "workspace-branch-host" }, branchPanel()),
      h("div", { id: "workspace-merge-host" }, mergePanel()),
      section("branches-conflicts", ["conflicts"], conflictPanel)];
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
    try { document.dispatchEvent?.(new CustomEvent("kin-theme")); } catch (_) { /* Maps reread colours on their next frame. */ }
  }

  function updateChrome() {
    const held = session();
    const failedReads = Object.entries(state.reads).filter(([, resource]) => resource.error);
    const status = byId("workspace-status");
    const words = byId("workspace-status-text");
    const latest = Math.max(0, ...Object.values(state.reads).map((resource) => resource.at || 0));
    if (status) status.dataset.state = state.refreshing ? "loading" : !held ? (read("session").error ? "error" : "loading") : failedReads.length ? "stale" : "ready";
    if (words) words.textContent = state.refreshing ? "Loading" : !held ? (read("session").error ? "Not connected" : "Loading") : failedReads.length ? "Some parts didn't load" : `Up to date ${clock(latest || Date.now())}`;
    if (!held) return;
    const repository = held.repository;
    const name = repositoryLabel(repository);
    byId("repository-name").replaceChildren(...repositoryTitle(repository, "side"));
    byId("repository-name").setAttribute("title", name);
    const branch = byId("repository-branch");
    const elsewhere = text(repository.host) && repository.host !== "github.com" ? repository.host : "";
    branch.replaceChildren(...[icon("branch"), activeBranch()?.name || (read("branches").error ? "Branch not read" : ""),
      elsewhere ? h("span", { class: "repo-host" }, ` · ${elsewhere}`) : null,
      !text(repository.host) ? h("span", { class: "pill pill-local" }, "Local") : null].filter(Boolean));
    byId("workspace-version").textContent = held.version ? `Kin ${held.version}` : "Kin";
    if (state.route !== "entity" || !state.entity.data) document.title = `${TITLES[state.route] || "Home"} · ${name}`;
    const brand = byId("workspace-brand");
    if (brand) brand.setAttribute("aria-label", `${repositoryLabel(repository, " / ")}, Home`);
    const reviewLink = byId("workspace-review-link");
    if (reviewLink) reviewLink.setAttribute("href", reviewURL());
    const repositories = array(held.repositories);
    // The picker is under the name on a wide window and in the menu on a narrow one.
    for (const id of ["workspace-repository-picker", "workspace-menu-picker"]) {
      const picker = byId(id);
      if (!picker) continue;
      picker.replaceChildren();
      if (repositories.length > 1) {
        picker.append(h("select", { class: "workspace-repository-select", "aria-label": "Open another repository", on: { change: (event) => { const target = safeLocalURL(event.target.value, null); if (target) location.assign(target); } } },
          repositories.map((repo) => h("option", { value: safeLocalURL(repo.url, ""), selected: repo.selected, disabled: !safeLocalURL(repo.url, null) }, repositoryLabel(repo, " / ")))));
      }
    }
  }

  /** The Agents nav item counts the agents working now. */
  function updateAgentsNav() {
    const badge = byId("workspace-nav-agents");
    if (!badge) return;
    const working = array(read("agents").data?.agents).filter((agent) => agent.state === "active").length;
    badge.hidden = !working;
    badge.textContent = working ? String(working) : "";
    badge.setAttribute("aria-label", working ? `${working} working now` : "");
  }

  /** On a narrow window, status, refresh, version, theme and the repository picker sit in one menu. */
  function setMenu(open) {
    state.menuOpen = Boolean(open);
    const toggle = byId("workspace-menu-toggle");
    toggle?.setAttribute("aria-expanded", String(state.menuOpen));
    const sidebar = toggle?.closest?.(".workspace-sidebar");
    if (sidebar) sidebar.dataset.menu = state.menuOpen ? "open" : "closed";
  }

  function render() {
    if (state.destroyed) return;
    clearSections();
    const main = byId("workspace-main");
    if (!session()) {
      main.replaceChildren(read("session").error
        ? h("div", {}, h("h1", {}, "This repository isn't answering."), h("div", { class: "section" }, readFailure("session")))
        : h("div", { "aria-busy": "true" }, skeleton("skeleton-lede"), skeleton("skeleton-line")));
      return;
    }
    const views = { home, agents: () => agentsApi() ? agentsApi().view(agentsCtx) : [h("h1", {}, "Agents"), h("p", { class: "gap" }, "This page could not load its agents view.")], explore, entity, changes, branches };
    main.replaceChildren(...views[state.route]().filter(Boolean));
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
    let name = parts[0] || "home";
    if (name === "overview") name = "home";
    if (name === "review") name = "changes";
    if (name === "repository") name = "branches";
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
    return ROUTES.includes(name) ? { name } : { name: "home" };
  }

  function routeChanged(initial = false) {
    const next = parseRoute(location.hash);
    const nav = next.name === "entity" ? "explore" : next.name;
    if (!initial && next.name === state.route && (next.name !== "entity" || next.id === state.params.id) && (next.name !== "agents" || next.agent === state.params.agent) && next.name !== "explore") return;
    if (!initial && next.name === "explore" && state.route === "explore") {
      if ((next.q || "") !== state.explore.query) { state.explore.query = next.q || ""; const input = byId("explore-input"); if (input) input.value = state.explore.query; runAsk(state.explore.query, true); }
      return;
    }
    state.route = next.name;
    state.params = next;
    setMenu(false);
    document.querySelectorAll("#workspace-nav a").forEach((node) => node.dataset.route === nav ? node.setAttribute("aria-current", "page") : node.removeAttribute("aria-current"));
    byId("workspace-tip") && (byId("workspace-tip").hidden = true);
    if (next.name === "explore") { state.explore.query = next.q || ""; state.explore.results = null; state.explore.active = -1; }
    updateChrome();
    render();
    if (next.name === "explore" && state.explore.query.trim() && session()) runAsk(state.explore.query, true);
    if (next.name === "entity" && session()) loadEntity(next.id);
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
  globalThis.addEventListener("hashchange", () => routeChanged());
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && state.menuOpen) { setMenu(false); byId("workspace-menu-toggle")?.focus(); return; }
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.target?.closest?.("input, textarea, select, [contenteditable=true]")) return;
    if (event.key === "/") {
      event.preventDefault();
      if (state.route !== "explore") location.hash = "#/explore";
      setTimeout(() => byId("explore-input")?.focus(), 0);
    }
  });
  // Coming back to the window reads uncommitted work and history again, so
  // the page follows the repository without a timer running behind it.
  let lastFocusRead = Date.now();
  const reread = () => {
    if (document.hidden || state.refreshing || workspaceWriteBusy() || !session() || Date.now() - lastFocusRead < 15_000) return;
    lastFocusRead = Date.now();
    refresh(["working", "changes", "standing", "branches"]);
  };
  globalThis.addEventListener("focus", reread);
  document.addEventListener("visibilitychange", reread);

  const started = (async () => {
    routeChanged(true);
    await load("session");
    updateChrome();
    render();
    if (state.route === "entity" && session()) loadEntity(state.params.id);
    if (state.route === "explore" && state.explore.query.trim() && session()) runAsk(state.explore.query, true);
    await refresh(readKeys().filter((key) => key !== "session"));
    if (!read("agents").missing && agentsApi()) {
      state.disposeLive = agentsApi().follow(agentsCtx);
      // Times like "connected for 4 minutes" are read from the clock; draw them again now and then.
      setInterval(() => { if (!document.hidden && ["home", "agents"].includes(state.route)) agentsCtx.repaintAgents(); }, 30_000);
    }
  })();

  globalThis.KinWorkspace = { workSentence, repositorySentence, personName, parseRoute, bodyLines, askGroups, isTest, relyWords, started, pulse };
})();
