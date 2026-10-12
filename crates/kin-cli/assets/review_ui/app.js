// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The reader for `kin review open`. It draws what the local server answers and
// decides nothing about the repository itself. Every node is built with
// textContent, never markup, because the text it shows is repository content.

"use strict";

(() => {
  const { diff, map } = globalThis.kinReview;
  const { nameNodes } = map;

  const POLL_MS = 5000;
  /** How many changes the list shows first, and the most one read asks for. */
  const CHANGES_FIRST = 60;
  const CHANGES_PAGE_MAX = 500;
  /** How many changes one count request names. */
  const COUNTS_ASKED = 100;
  /** How long the list waits before asking for counts again after a failed ask. */
  const COUNTS_RETRY_MS = 30000;
  const API_BASE = (document.body.dataset.apiBase || "").replace(/\/$/, "");

  const state = {
    session: null,
    changes: [],
    changesRequest: 0,
    // Whether history holds changes past the last one listed.
    truncated: false,
    loadingOlder: false,
    // How many entities each listed change's guide walks, by change id, as
    // the server answers it. A recorded change never changes, so each is
    // asked for once.
    counts: new Map(),
    countsAsked: new Set(),
    countsHeldUntil: 0,
    guide: null,
    guideLoadingKind: null,
    // What the shown guide's chapters read as, to tell when the working tree moved.
    guideText: "",
    // Stored reviews of the change shown, and how many it holds in all.
    reviews: [],
    reviewsTotal: 0,
    // Where each listed change stands, when the server can say.
    standing: {},
    standingKnown: false,
    // Writes go one at a time, each after the one before it has answered.
    writes: Promise.resolve(),
    // What the working tree held when it was last read, for the list's first entry.
    working: null,
    branches: [],
    editor: "off",
    polls: 0,
    workingBusy: false,
    workingNext: 0,
    workingRequest: 0,
    workingRead: { lastSuccess: null, displayedAt: null, error: null, pending: false, loading: false },
    changesError: null,
    sessionError: null,
    starting: false,
    polling: false,
    drafts: new Map(),
    draftFields: new Map(),
    panel: null,
    bar: null,
    read: new Set(),
    steps: [],
    current: -1,
    request: 0,
    filter: "all",
    split: false,
    wrap: false,
    observer: null,
  };

  const byId = (id) => document.getElementById(id);

  /** Build an element. `attrs` sets attributes; children are nodes or text. */
  function h(tag, attrs, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(attrs || {})) {
      if (value === false || value === null || value === undefined) continue;
      if (key === "class") node.className = value;
      else if (key === "on") {
        for (const [event, handler] of Object.entries(value)) node.addEventListener(event, handler);
      } else if (key === "hidden") node.hidden = Boolean(value);
      else node.setAttribute(key, value === true ? "" : String(value));
    }
    for (const child of children.flat()) {
      if (child === false || child === null || child === undefined) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  async function answer(response) {
    let body = null;
    try {
      body = await response.json();
    } catch (_) {
      body = null;
    }
    if (!response.ok) {
      const error = new Error(body && body.error ? body.error : `The local server answered ${response.status}.`);
      error.status = response.status;
      throw error;
    }
    return body;
  }

  const getJson = async (path) => answer(await fetch(`${API_BASE}${path}`, { headers: { accept: "application/json" } }));

  /** Send a write. The token is this run's own, handed to this page only. */
  const postJson = async (path, payload) =>
    answer(
      await fetch(`${API_BASE}${path}`, {
        method: "POST",
        headers: {
          accept: "application/json",
          "content-type": "application/json",
          "x-kin-review-token": state.session ? state.session.token : "",
        },
        body: JSON.stringify(payload),
      }),
    );

  /** Storage that may be unavailable. A preference then lasts for this page only. */
  const store = {
    get(key, fallback) {
      try {
        const value = localStorage.getItem(key);
        return value === null ? fallback : JSON.parse(value);
      } catch (_) {
        return fallback;
      }
    },
    set(key, value) {
      try {
        localStorage.setItem(key, JSON.stringify(value));
      } catch (_) {
        // Nothing to do: the value is still held in memory.
      }
    },
  };

  // ── Words ────────────────────────────────────────────────────────────────

  const short = (id) => (id || "").slice(0, 12);
  const plural = (count, one, many) => (count === 1 ? `1 ${one}` : `${count} ${many}`);
  const words = (label) => (label || "").replace(/_/g, " ");
  const NUMBER_WORD = ["No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten", "Eleven", "Twelve"];
  const spoken = (count) => (count < NUMBER_WORD.length ? NUMBER_WORD[count] : String(count));

  const CHANGE_WORD = { added: "Added", modified: "Modified", removed: "Removed" };
  const RISK_WORD = { low: "Low risk", medium: "Medium risk", high: "High risk", critical: "Critical risk" };
  const REACH_WORD = {
    calls: "calls it",
    references: "references it",
    instantiates: "constructs it",
    uses_type: "uses its type",
    uses_macro: "uses it as a macro",
    imports: "imports it",
    includes: "includes it",
    extends: "extends it",
    implements: "implements it",
    overrides: "overrides it",
    depends_on: "depends on it",
    consumes_contract: "consumes its contract",
    subscribes_to: "subscribes to it",
    sends_message: "sends to it",
    spawns: "spawns it",
    tests: "tests it",
    covers: "covers it",
    derived_from: "is derived from it",
  };
  const ORIGIN_WORD = {
    parsed: "Parsed from source",
    lsp: "Resolved by a language server",
    inferred: "Inferred",
    manual: "Added by hand",
  };
  const FIELD_WORD = { doc_summary: "Documentation", fingerprint: "Fingerprint" };
  const STANDING_WORD = { pending: "No decision yet", approved: "Approved", needswork: "Needs work", blocked: "Blocked" };
  const DECISIONS = [
    ["blocked", "Block"],
    ["needs_work", "Needs work"],
    ["approved", "Approve"],
  ];
  const FILTERS = [
    ["all", "All"],
    ["open", "To review"],
    ["needswork", "Needs work"],
    ["approved", "Approved"],
  ];

  function when(timestamp) {
    const date = new Date(timestamp);
    if (Number.isNaN(date.getTime())) return timestamp || "";
    return date.toLocaleString(undefined, { dateStyle: "medium", timeStyle: "short" });
  }

  /** The day alone, as a header says it: "Apr 16, 2024". */
  function day(timestamp) {
    const date = new Date(timestamp);
    if (Number.isNaN(date.getTime())) return timestamp || "";
    return date.toLocaleDateString(undefined, { dateStyle: "medium" });
  }

  /** Who made a change and when: "David Lord", "2 years ago". */
  const byline = (change) => [change.author, ago(change.timestamp)].filter(Boolean);

  /** What a change is called when it has no title of its own. Never its ID. */
  const untitled = (change) => change.title || "Untitled change";

  /** Parts of one line set apart by a middle dot, as "David Lord · 2 years ago". */
  function dotted(...parts) {
    return parts
      .flat()
      .filter((part) => part !== null && part !== undefined && part !== false && part !== "")
      .flatMap((part, index) => (index ? [h("span", { class: "dot-sep", "aria-hidden": "true" }, "·"), part] : [part]));
  }

  /**
   * A failure said the same way everywhere: one plain line, one action, and
   * what the server said behind Show details for whoever needs it.
   */
  function problem(line, action, detail, tag = "div") {
    return h(
      tag,
      { class: "problem", role: "status" },
      h("p", { class: "problem-line" }, line),
      action ? h("button", { class: "write-button problem-action", type: "button", on: { click: action.run } }, action.label) : null,
      detail ? h("details", { class: "problem-details" }, h("summary", {}, "Show details"), h("p", { class: "problem-text" }, detail)) : null,
    );
  }

  // The SVG namespace is a name, not a place the page loads from. It is
  // spelled in parts so the page holds no network address at all.
  const SVG_NS = ["http:", "", "www.w3.org", "2000", "svg"].join("/");

  /** The circular arrows the workspace uses for Refresh. Text where SVG cannot be built. */
  function refreshIcon() {
    if (typeof document.createElementNS !== "function") return "\u21bb";
    const svg = document.createElementNS(SVG_NS, "svg");
    for (const [key, value] of Object.entries({ viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "1.7",
      "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true", width: "16", height: "16" })) svg.setAttribute(key, value);
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", "M20 7v5h-5M4 17v-5h5M6.1 8a7 7 0 0 1 11.6-1.7L20 9M4 15l2.3 2.7A7 7 0 0 0 17.9 16");
    svg.append(path);
    return svg;
  }

  /** How many places rely on what changed, as a chip says it. */
  function relyWords(count, it) {
    if (!count) return `Nothing relies on ${it}`;
    return count === 1 ? `1 relies on ${it}` : `${count} rely on ${it}`;
  }

  /** How many tests reach what changed, as a chip says it. */
  function testWords(count, it) {
    if (!count) return `No test reaches ${it}`;
    return count === 1 ? `1 test reaches ${it}` : `${count} tests reach ${it}`;
  }

  function ago(timestamp) {
    const date = new Date(timestamp);
    if (Number.isNaN(date.getTime())) return "";
    const seconds = Math.round((date.getTime() - Date.now()) / 1000);
    const format = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
    const spans = [
      ["year", 31_536_000],
      ["month", 2_592_000],
      ["day", 86_400],
      ["hour", 3_600],
      ["minute", 60],
    ];
    for (const [unit, size] of spans) {
      if (Math.abs(seconds) >= size) return format.format(Math.round(seconds / size), unit);
    }
    return "just now";
  }

  /**
   * An entity's name, free to wrap where a reader would split it, and its
   * parameters when the guide gives them because another entity in the same
   * list carries the same name.
   */
  function nameWithParameters(entity) {
    return [...nameNodes(entity.name), entity.parameters ? h("span", { class: "params" }, nameNodes(entity.parameters)) : null];
  }

  /** Join names as a sentence does: `a`, `a and b`, `a, b and c`. */
  function joinAnd(parts) {
    if (parts.length < 2) return parts;
    return parts.flatMap((part, index) => (index === 0 ? [part] : [index === parts.length - 1 ? " and " : ", ", part]));
  }

  /**
   * The containers, such as a class, whose own text held still while members
   * in them changed. They are read through those members, and the guide says
   * which they are, as the terminal does.
   */
  function throughMembers(summary) {
    const count = summary.containers_through_members || 0;
    if (!count) return null;
    const names = (summary.through_members || []).map((name) => h("code", {}, nameNodes(name)));
    if (count > names.length) names.push(plural(count - names.length, "other", "others"));
    return h(
      "p",
      { class: "caveat" },
      joinAnd(names),
      ` changed only through what ${count === 1 ? "it holds" : "they hold"}, shown above.`,
    );
  }

  /**
   * How many entities Kin is still mapping, when the guide says. A newer
   * server gives the number; an older one only says it in its message.
   */
  function pendingEntities(completeness) {
    if (!completeness) return null;
    if (typeof completeness.pending_entities === "number") return completeness.pending_entities;
    const said = /Not settled:\s*(\d+)\s+entit/i.exec(completeness.message || "");
    return said ? Number(said[1]) : null;
  }

  /**
   * What a reader should know about the counts above, in one plain line, and
   * everything else behind Show details. When nothing is pending there is no
   * line at all.
   */
  function statusNotes(guide) {
    const { summary } = guide;
    const tree = guide.working_tree;
    const completeness = summary.completeness || {};
    const pending = pendingEntities(completeness);
    const lines = [];
    if (pending) {
      lines.push(h("p", { class: "caveat status-line" }, `Kin is still mapping ${plural(pending, "entity", "entities")}, so these counts may grow.`));
    } else if (completeness.state === "not_settled") {
      lines.push(h("p", { class: "caveat status-line" }, "Kin is still mapping this repository, so these counts may grow."));
    } else if (completeness.state === "unavailable") {
      lines.push(h("p", { class: "caveat status-line" }, "Kin can't say yet whether these counts are final."));
    }
    const behind = tree && (tree.unmeasured || !tree.caught_up);
    if (behind) lines.push(h("p", { class: "caveat status-line caveat-warn" }, "This may be a moment behind your latest edits."));

    const plain = (count, one, many) => h("p", { class: "caveat" }, count === 1 ? one : many.replace("{n}", count));
    const details = [
      summary.module_containers
        ? plain(summary.module_containers, "1 module changed along with what it holds.", "{n} modules changed along with what they hold.")
        : null,
      throughMembers(summary),
      summary.provenance_only_entity_changes
        ? plain(summary.provenance_only_entity_changes, "1 entity only moved within its module and isn't shown.", "{n} entities only moved within their module and aren't shown.")
        : null,
      summary.relation_changes_position_only
        ? plain(summary.relation_changes_position_only, "1 connection only moved and isn't shown.", "{n} connections only moved and aren't shown.")
        : null,
      tree && tree.unmeasured ? h("p", { class: "caveat" }, `Your edits were not measured before this was loaded: ${tree.unmeasured}`) : null,
      tree && !tree.caught_up ? h("p", { class: "caveat" }, "Kin was still catching up with your edits when this was loaded.") : null,
      completeness.state === "no_recorded_debt" ? h("p", { class: "caveat" }, "Kin has nothing left to map for these counts.") : null,
    ].filter(Boolean);
    if (!lines.length && !details.length) return null;
    return h(
      "div",
      { class: "caveats" },
      lines,
      details.length ? h("details", { class: "status-details" }, h("summary", {}, "Show details"), details) : null,
    );
  }

  /** A sentence whose `backticked` names are set as code. */
  function prose(text) {
    return String(text || "")
      .split("`")
      .map((part, index) => (index % 2 === 1 ? h("code", {}, part) : part));
  }

  /**
   * What the change means, in the fewest words that stay true. The numbers are
   * the guide's own: what changed, what relies on it from outside the change,
   * and how much of that the review flagged as left behind.
   */
  function headline(guide, mapCounts) {
    const { summary } = guide;
    const changed = summary.added + summary.modified + summary.removed;
    if (!changed) return `This ${nounOf(guide)} holds no entity a review can act on.`;
    const steps = guide.chapters.flatMap((chapter) => chapter.steps);
    const kinds = new Set(steps.map((step) => step.kind));
    const noun = (count) => {
      const kind = kinds.size === 1 && !guide.not_walked ? words([...kinds][0]) : "entity";
      if (count === 1) return kind;
      return kind.endsWith("y") ? `${kind.slice(0, -1)}ies` : kind.endsWith("s") ? `${kind}es` : `${kind}s`;
    };
    const it = changed === 1 ? "it" : "them";
    const parts = [`${spoken(changed)} ${noun(changed)} changed.`];
    const outside = summary.recorded_consumers;
    if (!outside) {
      parts.push(`Nothing outside the change relies on ${it}.`);
    } else {
      parts.push(`${spoken(outside)} other ${outside === 1 ? "place relies" : "places rely"} on ${it}.`);
      // The guide's own count, over every consumer and not only the ones the
      // map has room to draw. A page served by an older guide falls back to the map.
      const behind = typeof summary.consumers_not_updated === "number" ? summary.consumers_not_updated : mapCounts ? mapCounts.unsettled : 0;
      // What the change asks of the places that rely on it, when the guide
      // says: none need an update, or some should be checked. A guide that
      // carries neither count reads as it always did.
      const check = typeof summary.consumers_may_need_review === "number" ? summary.consumers_may_need_review : null;
      const fine = typeof summary.consumers_need_no_update === "number" ? summary.consumers_need_no_update : null;
      if (behind && behind === outside) {
        parts.push(outside === 1 ? "It was not updated." : outside === 2 ? "Neither was updated." : "None were updated.");
      } else if (behind) {
        parts.push(`${spoken(behind)} ${behind === 1 ? "was" : "were"} not updated.`);
        if (check) parts.push(`Check ${spoken(check).toLowerCase()} more.`);
      } else if (check) {
        parts.push(check === outside ? (outside === 1 ? "Check it." : outside === 2 ? "Check both." : "Check each of them.") : `Check ${spoken(check).toLowerCase()} of them.`);
      } else if (fine !== null && fine === outside) {
        parts.push(outside === 1 ? "It needs no update." : "None need an update.");
      } else {
        parts.push(outside === 1 ? "It did not change." : "None of them changed.");
      }
    }
    const tests = summary.recorded_tests;
    parts.push(tests ? `${spoken(tests)} ${tests === 1 ? "test reaches" : "tests reach"} ${it}.` : `No test reaches ${it}.`);
    return parts.join(" ");
  }

  // ── Theme ────────────────────────────────────────────────────────────────
  // Follows the system unless the reader picks one. The choice is kept in this
  // browser, like every other preference here.

  const THEMES = ["auto", "light", "dark"];
  const THEME_WORD = { auto: "Auto", light: "Light", dark: "Dark" };

  function applyTheme(theme) {
    if (theme === "auto") document.documentElement.removeAttribute("data-theme");
    else document.documentElement.setAttribute("data-theme", theme);
    const button = byId("theme");
    button.textContent = THEME_WORD[theme];
    button.setAttribute("aria-label", `Colour theme: ${THEME_WORD[theme]}. Change it.`);
  }

  function cycleTheme() {
    const next = THEMES[(THEMES.indexOf(store.get("kin.review.theme", "auto")) + 1) % THEMES.length];
    store.set("kin.review.theme", next);
    applyTheme(next);
  }

  // ── Toasts ───────────────────────────────────────────────────────────────

  function toast(message, action) {
    const note = h("div", { class: "toast", role: "status" }, h("span", {}, message));
    if (action) {
      note.append(
        h(
          "button",
          {
            class: "toast-action",
            type: "button",
            on: {
              click: () => {
                action.run();
                note.remove();
              },
            },
          },
          action.label,
        ),
      );
    }
    byId("toasts").append(note);
    setTimeout(() => note.remove(), action ? 12_000 : 3_500);
  }

  async function copy(text, what) {
    try {
      await navigator.clipboard.writeText(text);
      toast(`${what} copied`);
    } catch (_) {
      toast(`Could not copy the ${what.toLowerCase()}`);
    }
  }

  // ── What this browser has marked read ────────────────────────────────────
  // A convenience kept in this browser only. The record of the change is the
  // repository's; which steps one person has read is not part of it.

  const readKey = (subject) => `kin.review.read:${state.session ? state.session.repository : ""}:${subject}`;
  const loadRead = (subject) => new Set(store.get(readKey(subject), []));
  // The working tree keeps moving under one name, so marks made on it are
  // not kept: a step read an hour ago may hold different text now.
  const saveRead = () => {
    if (state.guide && !isWorking()) store.set(readKey(subjectOf(state.guide)), [...state.read]);
  };

  // Drafts belong to the resolved immutable endpoints, never to a moving ref.
  // Keep an in-memory copy too, for browsers which refuse local storage.
  const draftBindings = new WeakMap();
  function draftFor(key) {
    if (!state.drafts.has(key)) state.drafts.set(key, store.get(key, { text: "", outcome: null }));
    return state.drafts.get(key);
  }

  function saveDraft(key, value) {
    state.drafts.set(key, value);
    store.set(key, value);
  }

  function showDraftOutcome(binding) {
    const draft = draftFor(binding.key);
    const outcome = draft.outcome;
    binding.status.replaceChildren();
    if (!outcome) return;
    const recorded = outcome.state === "recorded";
    binding.status.append(h("p", { class: "notice", role: "status" },
      h("strong", {}, recorded ? "Saved." : "Not confirmed."), " ", outcome.message ||
      "The last send may have been saved. Check the history before sending again. Nothing is sent again on its own.",
      recorded && draft.text ? " The text in this box is an unsent draft." : ""));
  }

  function bindDraft(text, scope, status) {
    if (!state.guide || isWorking()) return;
    const session = state.session || {};
    const repository = session.repository_key || session.repository_id || [API_BASE, session.repository];
    const target = [state.guide.kind, state.guide.change.base || null, state.guide.change.id];
    const key = `kin.review.draft:${JSON.stringify([repository, target, scope])}`;
    const binding = { key, text, status };
    text.value = draftFor(key).text || "";
    draftBindings.set(text, binding);
    state.draftFields.set(key, binding);
    text.addEventListener("input", () => saveDraft(key, { ...draftFor(key), text: text.value }));
    showDraftOutcome(binding);
  }

  function updateDraftOutcome(binding, outcome, sent, clear) {
    if (!binding) return;
    const draft = draftFor(binding.key);
    const cleared = clear && draft.text === sent;
    saveDraft(binding.key, { text: cleared ? "" : draft.text, outcome });
    const visible = state.draftFields.get(binding.key);
    if (visible && document.contains(visible.text)) {
      if (cleared && visible.text.value === sent) visible.text.value = "";
      showDraftOutcome(visible);
    }
  }

  // ── Step parts ───────────────────────────────────────────────────────────

  const recorded = (body) => Boolean(body && body.state === "recorded" && typeof body.text === "string");

  function sourceNotice(side, body) {
    if (!body || recorded(body)) return null;
    if (body.state === "no_source") return h("p", { class: "notice" }, `No code ${side} this ${nounOf(state.guide)}.`);
    return problem(`Kin couldn't show the code ${side} this ${nounOf(state.guide)}.`, null, body.reason || null);
  }

  /** Where the code beside a step is from, in words and never an ID. */
  function sourceWords(before, after) {
    const noun = nounOf(state.guide);
    const what = noun === "work" ? "your edits" : noun === "comparison" ? "these changes" : "this change";
    if (before && after) return `Before and after ${what}.`;
    if (after) return isWorking() ? "As it is now." : `After ${what}.`;
    return `Before ${what}.`;
  }

  /** Whether a line names `name` as a whole identifier. */
  function namesIt(name) {
    const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const pattern = new RegExp(`(^|[^A-Za-z0-9_$])${escaped}($|[^A-Za-z0-9_$])`);
    return (line) => pattern.test(line);
  }

  let sourcePanelNumber = 0;
  function sourcePanel(step) {
    const { before, after } = step;
    const panel = h("div", { class: "source" });
    const notices = [sourceNotice("before", before), sourceNotice("after", after)].filter(Boolean);
    const provenance = recorded(before) || recorded(after) ? sourceWords(recorded(before), recorded(after)) : null;
    let redraw = null;

    if (recorded(before) && recorded(after)) {
      if (before.text === after.text) {
        panel.append(
          h("p", { class: "notice" }, "The code is the same before and after. What changed is listed above."),
          diff.plainView(after, step.language, "", null),
        );
      } else {
        const rows = diff.rows(before.text, after.text);
        const side = diff.sides(before, after, step.language);
        const views = {
          changes: h("div", { class: "view" }),
          before: diff.plainView(before, step.language, "", null),
          after: diff.plainView(after, step.language, "", null),
        };
        redraw = () => views.changes.replaceChildren(state.split ? diff.splitView(rows, side) : diff.unifiedView(rows, side));
        redraw();
        const panelId = `source-${++sourcePanelNumber}`;
        const tabs = h("div", { class: "tabs", role: "tablist", "aria-label": `Source views for ${step.name}` });
        const show = (name) => {
          for (const [key, view] of Object.entries(views)) view.hidden = key !== name;
          for (const tab of tabs.children) {
            const selected = tab.dataset.view === name;
            tab.setAttribute("aria-selected", String(selected));
            tab.setAttribute("tabindex", selected ? "0" : "-1");
          }
        };
        for (const [name, title] of [
          ["changes", "Changes"],
          ["before", "Before"],
          ["after", "After"],
        ]) {
          const tab = h("button", { class: "tab", type: "button", role: "tab", id: `${panelId}-${name}-tab`,
            "aria-controls": `${panelId}-${name}`, "data-view": name, on: { click: () => show(name) } }, title);
          tab.addEventListener("keydown", (event) => {
            const all = [...tabs.children];
            const at = all.indexOf(tab);
            const next = event.key === "Home" ? 0 : event.key === "End" ? all.length - 1
              : event.key === "ArrowRight" ? (at + 1) % all.length
                : event.key === "ArrowLeft" ? (at + all.length - 1) % all.length : null;
            if (next === null) return;
            event.preventDefault();
            show(all[next].dataset.view);
            all[next].focus();
          });
          views[name].setAttribute("id", `${panelId}-${name}`);
          views[name].setAttribute("role", "tabpanel");
          views[name].setAttribute("aria-labelledby", `${panelId}-${name}-tab`);
          views[name].setAttribute("tabindex", "0");
          tabs.append(tab);
        }
        const removed = rows.filter((row) => row.type === "del").length;
        const added = rows.filter((row) => row.type === "add").length;
        panel.append(
          h(
            "div",
            { class: "source-head" },
            tabs,
            h(
              "span",
              { class: "source-tools" },
              h("span", { class: "tally" }, h("span", { class: "minus" }, `-${removed}`), " ", h("span", { class: "plus" }, `+${added}`)),
              h(
                "button",
                { class: "split-toggle layout-toggle", type: "button", "aria-pressed": String(state.split), title: "Side by side (s)", on: { click: toggleSplit } },
                "Side by side",
              ),
              h("button", { class: "split-toggle wrap-toggle", type: "button", "aria-pressed": String(state.wrap),
                on: { click: toggleWrap } }, "Wrap lines"),
            ),
          ),
          views.changes,
          views.before,
          views.after,
        );
        show("changes");
      }
    } else if (recorded(after)) {
      panel.append(diff.plainView(after, step.language, "tone-add", null));
    } else if (recorded(before)) {
      panel.append(diff.plainView(before, step.language, "tone-del", null));
    }

    notices.forEach((notice) => panel.append(notice));
    if (provenance) panel.append(h("p", { class: "provenance" }, provenance));
    return { node: panel.children.length ? panel : null, redraw };
  }

  function toggleWrap() {
    state.wrap = !state.wrap;
    store.set("kin.review.wrap", state.wrap);
    document.body.classList.toggle("wrap-source", state.wrap);
    document.querySelectorAll(".wrap-toggle").forEach((button) => button.setAttribute("aria-pressed", String(state.wrap)));
  }

  function toggleSplit() {
    state.split = !state.split;
    store.set("kin.review.split", state.split);
    state.steps.forEach((step) => step.redraw && step.redraw());
    document.querySelectorAll(".layout-toggle").forEach((button) => button.setAttribute("aria-pressed", String(state.split)));
  }

  function fieldRows(step) {
    const shown = step.fields.filter((field) => field.before !== undefined || field.after !== undefined);
    if (!shown.length) return null;
    const list = h("dl", { class: "fields" });
    for (const field of shown) {
      list.append(
        h("dt", {}, FIELD_WORD[field.field] || words(field.field).replace(/^./, (c) => c.toUpperCase())),
        h(
          "dd",
          {},
          h("code", { class: "was" }, field.before === undefined ? "(none)" : field.before),
          h("span", { class: "arrow", "aria-hidden": "true" }, "→"),
          h("span", { class: "sr" }, "became"),
          h("code", { class: "now" }, field.after === undefined ? "(none)" : field.after),
        ),
      );
    }
    return list;
  }

  /** Where an entity is, as the page says it: what contains it. */
  const placeWords = (entity) => (entity.within ? `in ${entity.within}` : "");

  // ── Editor ───────────────────────────────────────────────────────────────
  // An entity is placed by what contains it. With an editor chosen it also
  // gets an Open link, which turns "not updated" into the next thing to fix.
  // The checkout's coordinate behind that link is handed to the editor and
  // never shown. The page only builds the address; the browser asks before
  // it hands one on.

  const EDITORS = ["off", "vscode", "cursor", "zed"];
  const EDITOR_WORD = { off: "not set", vscode: "VS Code", cursor: "Cursor", zed: "Zed" };

  function editorHref(entity) {
    const editor = state.editor;
    if (!editor || editor === "off" || !entity.file || !state.session || !state.session.root) return null;
    // One spelling for every platform: forward slashes, and a leading one.
    let root = state.session.root.replace(/\\/g, "/").replace(/\/+$/, "");
    if (!root.startsWith("/")) root = `/${root}`;
    const path = encodeURI(`${root}/${entity.file}`).replace(/#/g, "%23").replace(/\?/g, "%3F");
    return `${editor}://file${path}${entity.line ? `:${entity.line}` : ""}`;
  }

  /**
   * Whether an entity can be opened where the checkout holds it. Its
   * coordinate is recorded at the guide's head, which is where an editor
   * would find it only when the head is what is checked out: the uncommitted
   * work, or the change the working tree stands on. For any other change the
   * entity may have moved since, or not exist here at all.
   */
  function placesLocal() {
    if (!state.guide) return false;
    if (state.guide.kind === "working") return true;
    return Boolean(state.working && state.working.base && state.working.base === state.guide.change.id);
  }

  /** Build every place on the page again, after the editor or what is checked out changed. */
  function rebuildPlaces() {
    for (const old of document.querySelectorAll("[data-class]")) {
      const entity = { file: old.dataset.file || null, line: Number(old.dataset.line) || null, within: old.dataset.within || null };
      const again = placeNode(entity, old.dataset.class, old.dataset.gone === "1");
      if (again) old.replaceWith(again);
    }
  }

  /**
   * Where an entity is, in words, and an Open link into the chosen editor
   * when it can be opened. `gone` is set for one no longer there.
   */
  function placeNode(entity, className, gone) {
    if (!entity.within && !entity.file) return null;
    const href = gone || !placesLocal() ? null : editorHref(entity);
    // What the node was built from, so a change of editor can build it again
    // without drawing the page again around what the reader has typed.
    const built = {
      "data-file": entity.file || "",
      "data-line": entity.line || "",
      "data-within": entity.within || "",
      "data-gone": gone ? "1" : "",
      "data-class": className,
    };
    const node = h("span", { class: className, ...built }, placeWords(entity));
    if (href) {
      const link = h("a", { class: "open-link", href, title: `Open in ${EDITOR_WORD[state.editor]}` }, "Open");
      // A link inside a row that opens on click must not also open the row.
      link.addEventListener("click", (event) => event.stopPropagation());
      node.append(link);
    }
    return node;
  }

  function applyEditor(editor) {
    state.editor = EDITORS.includes(editor) ? editor : "off";
    const button = byId("editor");
    button.textContent = `Open code in: ${EDITOR_WORD[state.editor]}`;
    button.setAttribute("aria-label", `Open code in: ${EDITOR_WORD[state.editor]}. Change it.`);
  }

  function cycleEditor() {
    const next = EDITORS[(EDITORS.indexOf(state.editor) + 1) % EDITORS.length];
    store.set("kin.review.editor", next);
    applyEditor(next);
    rebuildPlaces();
    toast(
      next === "off"
        ? "Code no longer opens in an editor"
        : placesLocal()
          ? `Code opens in ${EDITOR_WORD[next]}`
          : `Code opens in ${EDITOR_WORD[next]} for the change you have checked out, not this one.`,
    );
  }

  /** The text of something that reaches a step, read when its row is opened. */
  async function loadReachText(holder, connection, step) {
    const change = step.evidence.head_change;
    holder.replaceChildren(h("p", { class: "none" }, "Loading its code."));
    const index = state.steps.findIndex((other) => other.entity === connection.entity.id);
    const jump =
      index >= 0
        ? h(
            "p",
            { class: "provenance" },
            h(
              "a",
              {
                href: `#step-${index}`,
                on: {
                  click: (event) => {
                    event.preventDefault();
                    setCurrent(index, true);
                  },
                },
              },
              "See its own step in this change",
            ),
          )
        : null;
    try {
      const query = `entity=${encodeURIComponent(connection.entity.id)}&change=${encodeURIComponent(change)}`;
      const source = await getJson(`/api/source?${query}`);
      if (recorded(source.body)) {
        holder.replaceChildren(
          ...[
            diff.plainView(source.body, step.language, "", namesIt(step.name)),
            h(
              "p",
              { class: "provenance" },
              source.body.working ? "Its code as it is now." : `Its code ${isRange() ? "after these changes" : "after this change"}.`,
              " Lines that name ",
              h("code", {}, step.name),
              " are marked.",
            ),
            jump,
          ].filter(Boolean),
        );
      } else {
        holder.replaceChildren(...[problem("Kin has no code to show for it.", null, source.body.reason || null), jump].filter(Boolean));
      }
    } catch (error) {
      holder.replaceChildren(
        problem("Kin couldn't load its code.", { label: "Try again", run: () => loadReachText(holder, connection, step) }, error.message),
      );
    }
  }

  /**
   * Rows of one name in one container, such as the overloads of one callable,
   * read as one row that counts them, as the map draws them. A member's name
   * carries its owner, so a qualified name is its own place; a bare name is
   * placed by what contains it.
   */
  function groupConnections(connections) {
    const groups = [];
    const byKey = new Map();
    const placeOf = (entity) => (/\.|::/.test(entity.name) ? "" : entity.within || "");
    for (const connection of connections) {
      const key = [connection.entity.kind, placeOf(connection.entity), connection.entity.name, Boolean(connection.changed_here)].join("\u0000");
      const declarations = 1 + (connection.overloads || 0);
      const same = byKey.get(key);
      if (same) {
        same.members.push(connection);
        same.declarations += declarations;
        continue;
      }
      const group = { connection, members: [connection], declarations };
      byKey.set(key, group);
      groups.push(group);
    }
    return groups;
  }

  /**
   * What a step's change asks of a place that relies on it and did not
   * change with it, in the words a row says it with: its label and its tag.
   */
  const VERDICT_WORD = {
    needs_no_update: ["No update needed", "tag-fine"],
    may_need_review: ["Check this", "tag-check"],
    not_updated: ["Not updated", "tag-behind"],
  };

  function connectionRow(group, step, kind) {
    const { connection, members, declarations } = group;
    const text = h("div", { class: "reach-text" });
    // The same words the map uses: a caller left as it was while the thing it
    // calls broke its promise is not merely unchanged.
    const verdict = kind === "consumer" && !connection.changed_here ? VERDICT_WORD[step.consumer_verdict?.verdict] || null : null;
    const behind = kind === "consumer" && !connection.changed_here && (verdict ? step.consumer_verdict.verdict === "not_updated" : step.findings.some((finding) => finding.severity === "error"));
    const standing = connection.changed_here ? "Changed here" : verdict ? verdict[0] : behind ? "Not updated" : "Unchanged";
    const folded = declarations > 1;
    const proof = h(
      "dl",
      { class: "proof" },
      h("dt", {}, "Connection"),
      h("dd", {}, h("code", {}, connection.relation_id)),
      h("dt", {}, "How Kin knows"),
      h("dd", {}, `${ORIGIN_WORD[connection.origin] || words(connection.origin)}, confidence ${Number(connection.confidence).toFixed(2)}`),
      connection.recorded_in ? h("dt", {}, "First seen in") : null,
      connection.recorded_in
        ? h("dd", {}, h("a", { href: `#/c/${connection.recorded_in}` }, "that change"))
        : null,
      h("dt", {}, members.length > 1 ? "Entities" : "Entity"),
      h("dd", {}, members.map((member, index) => [index ? ", " : null, h("code", {}, member.entity.id)])),
    );
    const row = h(
      "details",
      { class: "connection", "data-entity": connection.entity.id, "data-entities": members.map((member) => member.entity.id).join(" ") },
      h(
        "summary",
        {},
        h(
          "span",
          { class: "who" },
          // Folded rows drop the signatures: the count says what they share.
          h("code", {}, folded ? nameNodes(connection.entity.name) : nameWithParameters(connection.entity)),
          folded ? h("span", { class: "does overloads", title: "Declarations of one callable, such as its overloads, read as one row" }, `${declarations} overloads`) : null,
          h("span", { class: "does" }, REACH_WORD[connection.relation] || words(connection.relation)),
        ),
        placeNode(connection.entity, "where", false),
        h("span", { class: `tag ${connection.changed_here ? "tag-changed" : verdict ? verdict[1] : behind ? "tag-behind" : ""}` }, standing),
      ),
      text,
      proof,
    );
    let asked = false;
    row.addEventListener("toggle", () => {
      if (row.open && !asked) {
        asked = true;
        loadReachText(text, connection, step);
      }
    });
    return row;
  }

  function connectionGroup(title, rows, total, empty, step, kind) {
    const group = h("section", { class: "group" });
    group.append(h("h4", {}, title, total ? h("span", { class: "count" }, total) : null));
    if (!total) {
      group.append(h("p", { class: "none" }, empty));
      return group;
    }
    groupConnections(rows).forEach((row) => group.append(connectionRow(row, step, kind)));
    if (rows.length < total) group.append(h("p", { class: "none" }, `And ${total - rows.length} more.`));
    return group;
  }

  function importers(connections) {
    if (!connections.importing_modules_total) return null;
    const names = connections.importing_modules.map((module) => module.name);
    const more = connections.importing_modules_total - names.length;
    return h(
      "p",
      { class: "importers" },
      `Imported by ${plural(connections.importing_modules_total, "module", "modules")}: `,
      names.flatMap((name, index) => (index ? [", ", h("code", {}, name)] : [h("code", {}, name)])),
      more > 0 ? ` and ${more} more` : null,
      ".",
    );
  }

  function relationChanges(connections) {
    if (!connections.relation_changes_total) return null;
    const list = h("ul", { class: "edges" });
    for (const edge of connections.relation_changes) {
      const sentence =
        edge.direction === "inbound" ? `${edge.other} ${words(edge.relation)} this` : `this ${words(edge.relation)} ${edge.other}`;
      list.append(h("li", { class: `edge edge-${edge.change}` }, h("span", { class: "edge-change" }, CHANGE_WORD[edge.change] || edge.change), h("code", {}, sentence)));
    }
    const more = connections.relation_changes_total - connections.relation_changes.length;
    return h(
      "section",
      { class: "group" },
      h("h4", {}, "Connections that changed", h("span", { class: "count" }, connections.relation_changes_total)),
      list,
      more > 0 ? h("p", { class: "none" }, `And ${more} more.`) : null,
    );
  }

  function evidence(step) {
    const proof = step.evidence;
    const rows = h("dl", { class: "proof" });
    rows.append(h("dt", {}, "Entity"), h("dd", {}, h("code", {}, step.entity_id)));
    if (proof.base_change) rows.append(h("dt", {}, "Before read at"), h("dd", {}, h("code", {}, proof.base_change)));
    rows.append(
      h("dt", {}, proof.base_change ? "After read at" : "Read at"),
      h("dd", {}, isWorking() ? "Your edits, not committed yet" : h("code", {}, proof.head_change)),
    );
    for (const [side, body] of [
      ["before", step.before],
      ["after", step.after],
    ]) {
      if (body && body.source_blob) rows.append(h("dt", {}, `Source ${side}`), h("dd", {}, h("code", {}, body.source_blob)));
    }
    const before = proof.fingerprint_before;
    const after = proof.fingerprint_after;
    for (const [key, title] of [
      ["signature", "Signature fingerprint"],
      ["behavior", "Behavior fingerprint"],
      ["ast", "Syntax fingerprint"],
    ]) {
      if (before && after) {
        const same = before[key] === after[key];
        rows.append(
          h("dt", {}, title),
          h(
            "dd",
            {},
            h("span", { class: `tag ${same ? "" : "tag-changed"}` }, same ? "Same" : "Changed"),
            " ",
            h("code", {}, same ? short(after[key]) : `${short(before[key])} → ${short(after[key])}`),
          ),
        );
      } else if (before || after) {
        rows.append(h("dt", {}, title), h("dd", {}, h("code", {}, short((after || before)[key]))));
      }
    }
    return h("details", { class: "evidence" }, h("summary", {}, "Why we know"), rows);
  }

  /**
   * How a step's entity got here: the changes that changed it, asked for when
   * the reader opens it. Each is a way into that change's own walk.
   */
  function historyPanel(step) {
    const body = h("div", { class: "history-body" }, h("p", { class: "caveat" }, "Loading its history."));
    const panel = h("details", { class: "evidence history" }, h("summary", {}, "How it got here"), body);
    let asked = false;
    const load = async () => {
      if (!panel.open || asked) return;
      asked = true;
      body.replaceChildren(h("p", { class: "caveat" }, "Loading its history."));
      try {
        // Read on the line of history the guide ends at. A change on another
        // branch is not on the current one, and its entity's history is not there.
        // Uncommitted work stands on the last recorded change, and in a
        // repository with none yet there is no history to read at all.
        const head = !state.guide ? null : isWorking() ? state.guide.change.base : state.guide.change.id;
        // In a range the server marks which revisions belong to it, since the
        // range's own list of changes is only its newest.
        const since = isRange() ? `&since=${encodeURIComponent(state.guide.range.merge_base)}` : "";
        const held = head
          ? await getJson(`/api/history?entity=${encodeURIComponent(step.entity_id)}&at=${encodeURIComponent(head)}${since}`)
          : { revisions: [], revisions_total: 0, unchanged_revisions: 0 };
        const shown = state.guide && state.guide.kind !== "working" ? state.guide.change.id : null;
        const inRange = new Set(held.in_range || []);
        const rows = held.revisions.map((revision, index) => {
          const here = revision.id === shown || inRange.has(revision.id);
          const first = index === held.revisions.length - 1 && held.revisions.length === held.revisions_total;
          return h(
            "li",
            {},
            h(
              "a",
              { class: `range-change ${here ? "is-here" : ""}`, href: `#/c/${revision.id}` },
              h("span", { class: "range-title" }, untitled(revision)),
              h(
                "span",
                { class: "range-meta" },
                here ? h("span", { class: "tag tag-changed" }, isRange() ? "In this comparison" : "This change") : null,
                first ? h("span", { class: "tag" }, "Introduced it") : null,
                dotted(byline(revision).map((part) => h("span", {}, part))),
              ),
            ),
          );
        });
        const parts = [];
        if (held.removed_by) {
          parts.push(
            h("p", { class: "caveat" }, "Removed by ", h("a", { href: `#/c/${held.removed_by.id}` }, held.removed_by.title || `change ${short(held.removed_by.id)}`), "."),
          );
        }
        if (rows.length) {
          parts.push(
            h("p", { class: "caveat" }, `${plural(held.revisions_total, "change", "changes")} on this branch changed it, newest first.`),
            h("ol", { class: "range-list" }, rows),
          );
          if (held.revisions_total > held.revisions.length) {
            parts.push(h("p", { class: "caveat" }, `The ${held.revisions_total - held.revisions.length} oldest are in `, h("code", {}, `kin history ${step.name}`), "."));
          }
        } else {
          parts.push(h("p", { class: "caveat" }, isWorking() ? "No commit holds it yet. It is new in your edits." : "No change on this branch changed it."));
        }
        if (held.unchanged_revisions) {
          parts.push(
            h(
              "p",
              { class: "caveat" },
              `${plural(held.unchanged_revisions, "other change", "other changes")} touched it without changing it.`,
            ),
          );
        }
        body.replaceChildren(...parts);
      } catch (error) {
        asked = false;
        body.replaceChildren(problem("Kin couldn't load its history.", { label: "Try again", run: load }, error.message));
      }
    };
    panel.addEventListener("toggle", load);
    return panel;
  }

  function findings(step) {
    // The step's own first line already says an entity is new.
    const shown = step.findings.filter((finding) => finding.kind !== "added");
    if (!shown.length) return null;
    const list = h("ul", { class: "findings" });
    for (const finding of shown) {
      list.append(h("li", { class: `finding finding-${finding.severity}` }, prose(finding.message)));
    }
    return list;
  }

  /**
   * The risk, and when something relies on what changed, how many places do:
   * "High risk: 16 places rely on it". The count is the one its chip shows.
   */
  function riskWords(risk, count, it = "it") {
    if (!RISK_WORD[risk]) return null;
    if (!count) return RISK_WORD[risk];
    return `${RISK_WORD[risk]}: ${count} ${count === 1 ? "place relies" : "places rely"} on ${it}`;
  }

  function riskPill(risk, count, it) {
    const said = riskWords(risk, count, it);
    return said ? h("span", { class: `pill risk-${risk}` }, said) : null;
  }

  function stepCard(step, number) {
    const index = state.steps.length;
    const toggle = h("button", { class: "read", type: "button", "aria-pressed": "false" }, "Mark read");
    toggle.addEventListener("click", (event) => {
      event.stopPropagation();
      toggleRead(index);
    });
    const notesCount = h("span", { class: "count", hidden: true });
    const notesList = h("ul", { class: "notes" });
    const form = noteForm(step.entity_id, `Note on ${step.name}`);
    const notes = isWorking() ? null : h("section", { class: "group step-notes" }, h("h4", {}, "Notes", notesCount), notesList, form.nodes);
    const source = sourcePanel(step);
    const card = h(
      "article",
      { class: "step", id: `step-${index}`, tabindex: "-1", "data-entity": step.entity_id },
      h(
        "header",
        { class: "step-head" },
        h("span", { class: "step-no" }, number),
        h(
          "div",
          { class: "step-id" },
          h("h3", {}, h("code", {}, nameWithParameters(step))),
          h(
            "p",
            { class: "step-meta" },
            h("span", { class: `badge badge-${step.change}` }, CHANGE_WORD[step.change] || step.change),
            h("span", {}, words(step.kind)),
            placeNode(step, "where", step.change === "removed"),
            step.risk !== "low" ? riskPill(step.risk, step.connections.consumers_total) : null,
          ),
        ),
        toggle,
      ),
      h("p", { class: "narration" }, prose(step.narration)),
      fieldRows(step),
      source.node,
      findings(step),
      h(
        "div",
        { class: "connects" },
        connectionGroup("What reaches it", step.connections.consumers, step.connections.consumers_total, "Nothing else relies on it.", step, "consumer"),
        connectionGroup("Tests", step.connections.tests, step.connections.tests_total, "No test reaches it.", step, "test"),
        relationChanges(step.connections),
      ),
      importers(step.connections),
      notes,
      h("div", { class: "step-foot" }, evidence(step), historyPanel(step)),
    );
    card.addEventListener("focusin", () => setCurrent(index, false));
    state.steps.push({
      card,
      toggle,
      notesCount,
      notesList,
      noteBox: form.text,
      redraw: source.redraw,
      entity: step.entity_id,
      name: step.name,
      number,
      change: step.change,
    });
    return card;
  }

  // ── Recorded reviews ─────────────────────────────────────────────────────
  // Decisions and notes are repository records. The page writes them through
  // the same path the terminal commands use and shows what reads back.
  //
  // The boxes a reader types in are built once per change and never rebuilt,
  // so recording one thing cannot discard what was typed into another. Only
  // the recorded history beside them is redrawn.

  /** What a write is recorded against: this change, and how to open its review. */
  function reviewTarget() {
    const change = state.guide.change;
    return { change: change.id, base: change.base || null, title: change.title || null, range: isRange() };
  }

  /** Where the stored reviews of what is shown are read from. */
  function reviewsUrl() {
    const change = state.guide.change;
    const range = isRange() && change.base ? `&range_base=${encodeURIComponent(change.base)}` : "";
    return `/api/reviews?change=${encodeURIComponent(change.id)}${range}`;
  }

  function takeReviews(held) {
    state.reviews = held.reviews;
    state.reviewsTotal = held.reviews_total || held.reviews.length;
    if (state.guide && !isWorking()) {
      // A review of a range reads its last change too, so that change's
      // standing moves with it, as it does when the list is next read.
      const newest = state.reviews[0];
      if (newest) state.standing[state.guide.change.id] = { state: newest.state, reviews: state.reviewsTotal };
    }
    renderReviews();
    renderRail();
  }

  /**
   * Send one write. `status` is where its outcome is said, `text` the box it
   * came from.
   *
   * Writes go one at a time. Each answer carries the review as it read after
   * that write, so two in flight together could land out of order and leave
   * the older reading on the page. In a line, the last answer is the latest.
   */
  function record(path, payload, status, button, text) {
    if (button.disabled || !state.guide || isWorking()) return;
    const subject = subjectOf(state.guide);
    const request = state.request;
    const target = reviewTarget();
    const reread = reviewsUrl();
    const sent = text ? text.value : null;
    const binding = text ? draftBindings.get(text) : null;
    if (binding) saveDraft(binding.key, { text: sent, outcome: { state: "sending" } });
    status.replaceChildren();
    button.disabled = true;
    const send = async () => {
      try {
        const held = await postJson(path, { ...target, ...payload });
        updateDraftOutcome(binding, { state: "recorded", message: Array.isArray(held.reviews)
          ? "The repository confirmed the previous send."
          : "The page couldn't load the review again. Refresh to see its history." }, sent, true);
        // Cleared only if it still holds what was sent. Anything typed while
        // the write was on its way is the reader's next thought, not this one.
        if (text && text.value === sent) text.value = "";
        // The reader may have moved to another change while this was in flight.
        // What it recorded belongs to the change it was sent for, not this one.
        if (request !== state.request || subjectOf(state.guide) !== subject) return;
        if (Array.isArray(held.reviews)) {
          takeReviews(held);
        } else {
          status.replaceChildren(
            h(
              "p",
              { class: "notice" },
              h("strong", {}, "Saved."),
              " The page couldn't load the review again, so what is shown is from before.",
            ),
          );
        }
      } catch (error) {
        // A refusal does not prove nothing was written: the daemon can commit
        // a write and fail afterwards. So the page does not say "not
        // recorded". It rereads the review and shows what the repository
        // holds, and the box keeps its text in case it does need sending.
        let rereadOk = false;
        try {
          const held = await getJson(reread);
          if (request === state.request && subjectOf(state.guide) === subject && Array.isArray(held.reviews)) {
            takeReviews(held);
            rereadOk = true;
          }
        } catch (_) {
          rereadOk = false;
        }
        updateDraftOutcome(binding, { state: "unknown", message: `${rereadOk
          ? "The history shown is current. Check it before sending again."
          : "Refresh the history before sending again."} Nothing is sent again on its own.` }, sent, false);
        if (request === state.request && subjectOf(state.guide) === subject) {
          status.replaceChildren(
            h(
              "p",
              { class: "notice" },
              h("strong", {}, "Not confirmed."),
              " ",
              rereadOk
                ? "The history shown is current. Check it before sending this again."
                : "The page couldn't load the review either. Reload it before sending this again.",
              h("details", { class: "problem-details" }, h("summary", {}, "Show details"), h("p", { class: "problem-text" }, error.message)),
            ),
          );
        }
      } finally {
        button.disabled = false;
      }
    };
    state.writes = state.writes.then(send, send);
    return state.writes;
  }

  /** The scopes this page shows under a step, so every other note stays with the change. */
  const walkedScopes = () => new Set(state.steps.map((step) => `entity:${step.entity}`));

  /** One decision or note. `showScope` names what a note is about; `review` names its review. */
  function eventItem(event, showScope, review) {
    const isDecision = event.kind === "decision";
    return h(
      "li",
      { class: "note" },
      isDecision ? h("span", { class: `pill standing-${event.state}` }, STANDING_WORD[event.state] || event.state) : null,
      h("span", { class: "note-by" }, event.by),
      h("time", { class: "note-at", datetime: event.at, title: when(event.at) }, ago(event.at)),
      review ? h("code", { class: "note-scope" }, `review ${review.review_id.slice(0, 8)}`) : null,
      showScope && event.scope ? h("code", { class: "note-scope" }, event.scope) : null,
      event.text || event.text_cut
        ? h("span", { class: "note-body" }, event.text || "", event.text_cut ? h("span", { class: "none" }, " (cut short here)") : null)
        : null,
    );
  }

  /** A note box. `entity` scopes the note to one step; without it the note is about the change. */
  function noteForm(entity, prompt) {
    const status = h("div", { class: "write-status" });
    const text = h("textarea", {
      class: "write-text",
      name: entity ? "step-note" : "change-note",
      rows: "2",
      placeholder: prompt,
      "aria-label": prompt,
    });
    bindDraft(text, entity ? `entity:${entity}` : "change-note", status);
    const save = h("button", { class: "write-button", type: "button" }, "Add note");
    const send = () => {
      if (!text.value.trim()) {
        text.focus();
        return;
      }
      record("/api/note", entity ? { body: text.value, entity } : { body: text.value }, status, save, text);
    };
    save.addEventListener("click", send);
    text.addEventListener("keydown", (event) => {
      if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
        event.preventDefault();
        send();
      }
    });
    return { nodes: [h("div", { class: "write-row" }, text, save), status], text };
  }

  /** The review record's fixed parts, built once for the change being shown. */
  function buildReviewPanel() {
    if (isWorking()) {
      // A review names a change, and this work is not one yet.
      state.panel = null;
      return h(
        "section",
        { class: "review-panel", id: "review-panel", "aria-label": "Review record" },
        h("div", { class: "review-head" }, h("h2", {}, "Not committed yet")),
        h("p", { class: "caveat" }, "Commit this work to approve it or ask for changes."),
      );
    }
    const standing = h("span", { class: "pill standing-pending" });
    const history = h("div", { class: "review-history" });
    const footer = h("p", { class: "caveat" });
    const form = noteForm(null, `Note on this ${nounOf(state.guide)}`);
    state.panel = { standing, history, footer };
    return h(
      "section",
      { class: "review-panel", id: "review-panel", "aria-label": `Review record of this ${nounOf(state.guide)}` },
      h(
        "div",
        { class: "review-head" },
        h("h2", {}, "Review record"),
        standing,
        state.session.actor ? h("span", { class: "recorded-as" }, `Signed as ${state.session.actor}`) : null,
      ),
      history,
      form.nodes,
      footer,
    );
  }

  /**
   * The bar that stays in view while a change is read: how far the reader is,
   * where the review stands, and the decision. Built once per change.
   */
  function buildReviewBar() {
    const status = h("div", { class: "write-status" });
    const label = h("span", { class: "bar-progress-label" });
    const meter = h("progress", { class: "bar-progress", max: 1, value: 0, "aria-label": "Steps read" });
    const standing = h("span", { class: "pill standing-pending" });
    const comment = h("input", {
      class: "bar-comment",
      name: "decision-comment",
      type: "text",
      autocomplete: "off",
      placeholder: "Comment on your decision (optional)",
      "aria-label": "Comment on your decision",
    });
    bindDraft(comment, "decision-comment", status);
    const buttons = {};
    const actions = h("div", { class: "decide" });
    for (const [value, title] of DECISIONS) {
      const button = h("button", { class: `write-button decide-${value}`, type: "button" }, title);
      button.addEventListener("click", () => record("/api/decide", { state: value, comment: comment.value }, status, button, comment));
      buttons[value] = button;
      actions.append(button);
    }
    if (isWorking()) {
      // Nothing to decide on work that is not a change. The bar keeps what
      // still applies: how far the reader is, where to start, and a refresh.
      state.bar = { label, meter, standing: null, comment: null, buttons: {} };
      const refresh = h("button", { class: "bar-icon", type: "button", "aria-label": "Refresh", title: "Refresh" }, refreshIcon());
      refresh.addEventListener("click", () => refreshWorking(true));
      const first = state.steps[0];
      const startButton = first
        ? h("button", { class: "write-button decide-approved bar-start", type: "button", on: { click: () => setCurrent(0, true) } }, "Start with ", h("code", {}, first.name))
        : null;
      return h(
        "div",
        { class: "reviewbar is-working", id: "reviewbar", role: "region", "aria-label": "Your progress" },
        h(
          "div",
          { class: "bar-row" },
          h("div", { class: "bar-read" }, meter, label),
          h("span", { class: "bar-note" }, "Updates as you edit."),
          startButton,
          refresh,
        ),
      );
    }
    const toggle = h("button", { class: "review-decision-toggle write-button", type: "button", "aria-expanded": "false",
      "aria-controls": "decision-comment decision-actions" }, "Leave a decision");
    comment.setAttribute("id", "decision-comment");
    actions.setAttribute("id", "decision-actions");
    const bar = h("div", { class: "reviewbar", id: "reviewbar", role: "region", "aria-label": "Your review" },
      h("div", { class: "bar-row" }, h("div", { class: "bar-read" }, meter, label), standing, toggle, comment, actions), status);
    const open = (expanded = true) => {
      bar.classList.toggle("decision-open", expanded);
      toggle.setAttribute("aria-expanded", String(expanded));
      toggle.textContent = expanded ? "Hide decision controls" : "Leave a decision";
    };
    toggle.addEventListener("click", () => open(toggle.getAttribute("aria-expanded") !== "true"));
    state.bar = { label, meter, standing, comment, buttons, open };
    return bar;
  }

  /**
   * The recorded history, one block per stored review, each in the order it
   * was recorded. A change can hold more than one review, and a decision
   * reads differently depending on which it was made on, so their histories
   * are never merged into one list.
   */
  function renderReviewPanel() {
    if (!state.panel || !state.guide) return;
    const { standing, history, footer } = state.panel;
    const newest = state.reviews[0];
    const several = state.reviews.length > 1;
    const walked = walkedScopes();
    const where = newest ? newest.state : "pending";
    for (const pill of [standing, state.bar && state.bar.standing].filter(Boolean)) {
      pill.className = `pill standing-${where}`;
      pill.textContent = STANDING_WORD[where] || where;
    }

    const blocks = state.reviews.map((review, index) => {
      // A note on an entity this page walks is shown under that step. Every
      // other entry, scoped or not, stays here so none is hidden.
      const entries = review.events.filter((event) => event.kind !== "note" || !event.scope || !walked.has(event.scope));
      const earlier = review.events_total - review.events.length;
      return h(
        "div",
        { class: "review-block" },
        several
          ? h(
              "p",
              { class: "review-block-head" },
              h("code", {}, review.review_id.slice(0, 8)),
              h("span", { class: `pill standing-${review.state}` }, STANDING_WORD[review.state] || review.state),
              h("span", {}, review.title),
              index === 0 ? h("span", { class: "joins" }, "New decisions and notes join this one") : null,
            )
          : null,
        earlier > 0 ? h("p", { class: "none" }, `${earlier} earlier entries are in `, h("code", {}, `kin review show ${review.review_id}`), ".") : null,
        entries.length
          ? h("ul", { class: "notes" }, entries.map((event) => eventItem(event, true, null)))
          : several
            ? h("p", { class: "none" }, "Nothing on this review yet.")
            : null,
      );
    });
    const older = state.reviewsTotal - state.reviews.length;
    if (older > 0) {
      blocks.push(h("p", { class: "none" }, `${older} older reviews of this ${nounOf(state.guide)} are in `, h("code", {}, "kin review list"), "."));
    }
    history.replaceChildren(...blocks);
    footer.replaceChildren(
      ...(newest
        ? [
            "Saved in this repository. ",
            h("button", { class: "id-copy", type: "button", title: "Copy the review ID", on: { click: () => copy(newest.review_id, "Review ID") } }, "Copy ID"),
          ]
        : [`No decision or note on this ${nounOf(state.guide)} yet. Your first one starts its review.`]),
    );
  }

  function renderStepNotes() {
    const several = state.reviews.length > 1;
    state.steps.forEach((step) => {
      const scope = `entity:${step.entity}`;
      const mine = state.reviews.flatMap((review) =>
        review.events
          .filter((event) => event.kind === "note" && event.scope === scope)
          .map((event) => eventItem(event, false, several ? review : null)),
      );
      step.notesCount.textContent = mine.length ? String(mine.length) : "";
      step.notesCount.hidden = !mine.length;
      step.notesList.replaceChildren(...mine);
    });
  }

  function renderReviews() {
    renderReviewPanel();
    renderStepNotes();
  }

  // ── The page ─────────────────────────────────────────────────────────────

  function renderProgress() {
    const total = state.steps.length;
    const done = state.steps.filter((step) => state.read.has(step.entity)).length;
    const text = total ? `${done} of ${plural(total, "step", "steps")} read` : "";
    if (state.bar) {
      state.bar.label.textContent = text;
      state.bar.meter.max = Math.max(total, 1);
      state.bar.meter.value = done;
    }
    const outline = byId("outline-progress");
    if (outline) outline.textContent = text;
    state.steps.forEach((step, index) => {
      const isRead = state.read.has(step.entity);
      step.card.classList.toggle("is-read", isRead);
      step.toggle.setAttribute("aria-pressed", String(isRead));
      step.toggle.textContent = isRead ? "Read" : "Mark read";
      const link = byId(`outline-${index}`);
      if (link) link.classList.toggle("is-read", isRead);
    });
  }

  function toggleRead(index) {
    const step = state.steps[index];
    if (!step || !state.guide) return;
    if (state.read.has(step.entity)) state.read.delete(step.entity);
    else state.read.add(step.entity);
    saveRead();
    renderProgress();
  }

  function markCurrent(index) {
    state.current = index;
    state.steps.forEach((step, other) => {
      step.card.classList.toggle("is-current", other === index);
      const link = byId(`outline-${other}`);
      if (link) {
        link.classList.toggle("is-current", other === index);
        if (other === index) link.setAttribute("aria-current", "step");
        else link.removeAttribute("aria-current");
      }
    });
  }

  function setCurrent(index, scroll) {
    if (index < 0 || index >= state.steps.length) return;
    markCurrent(index);
    if (scroll) {
      const card = state.steps[index].card;
      card.scrollIntoView({ block: "start", behavior: "smooth" });
      card.focus({ preventScroll: true });
    }
  }

  /** Follow the reader: the step nearest the top of the view is the current one. */
  function watchSteps() {
    if (state.observer) state.observer.disconnect();
    if (!("IntersectionObserver" in globalThis) || !state.steps.length) return;
    const visible = new Set();
    state.observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          const index = state.steps.findIndex((step) => step.card === entry.target);
          if (entry.isIntersecting) visible.add(index);
          else visible.delete(index);
        }
        if (visible.size) markCurrent(Math.min(...visible));
      },
      { rootMargin: "-15% 0px -55% 0px" },
    );
    state.steps.forEach((step) => state.observer.observe(step.card));
  }

  function findingList(title, items, tone) {
    if (!items.length) return null;
    return h(
      "section",
      { class: "group" },
      h("h4", {}, title, h("span", { class: "count" }, items.length)),
      h("ul", { class: "findings" }, items.map((item) => h("li", { class: `finding finding-${tone}` }, prose(item)))),
    );
  }

  /** Open what a map node stands for: its own step, or its row under the step it reaches. */
  function openMapNode(node) {
    const target = node.kind === "changed" ? node.id : node.parent;
    const index = state.steps.findIndex((step) => step.entity === target);
    if (index < 0) return;
    setCurrent(index, true);
    if (node.kind === "consumer" || node.kind === "test") {
      // A folded row stands for every declaration it counts.
      const row = [...state.steps[index].card.querySelectorAll("details.connection")].find((candidate) => (candidate.dataset.entities || candidate.dataset.entity || "").split(" ").includes(node.id));
      if (row) {
        row.open = true;
        row.classList.add("is-pointed");
        setTimeout(() => row.classList.remove("is-pointed"), 1800);
      }
    }
  }

  function renderOutline(guide) {
    const outline = byId("outline");
    if (!state.steps.length) {
      outline.replaceChildren();
      return;
    }
    let index = 0;
    const chapters = guide.chapters.map((chapter, chapterIndex) => {
      const items = chapter.steps.map((step) => {
        const at = index;
        index += 1;
        return h(
          "li",
          {},
          h(
            "a",
            {
              id: `outline-${at}`,
              class: "outline-step",
              href: `#step-${at}`,
              on: {
                click: (event) => {
                  event.preventDefault();
                  setCurrent(at, true);
                },
              },
            },
            h("span", { class: `dot dot-${step.change}`, "aria-hidden": "true" }),
            h("code", {}, nameWithParameters(step)),
          ),
        );
      });
      return h(
        "li",
        { class: "outline-chapter" },
        h("p", { class: "outline-title" }, h("span", { class: "outline-no" }, chapterIndex + 1), chapter.title),
        h("ol", { class: "outline-steps" }, items),
      );
    });
    outline.replaceChildren(
      h("p", { class: "outline-head" }, `In this ${nounOf(guide)}`),
      h("p", { class: "outline-progress", id: "outline-progress" }),
      h("ol", { class: "outline-chapters" }, chapters),
    );
  }

  /**
   * The guide as Markdown, for a pull request description or a message. It is
   * the same sentences the page shows, in the order it shows them.
   */
  function summaryMarkdown(guide, lede) {
    const { change, summary } = guide;
    const lines = [`## ${titleOf(guide)}`, "", lede, ""];
    const counts = [
      summary.modified ? `${summary.modified} modified` : null,
      summary.added ? `${summary.added} added` : null,
      summary.removed ? `${summary.removed} removed` : null,
    ].filter(Boolean);
    const it = summary.added + summary.modified + summary.removed === 1 ? "it" : "them";
    lines.push(
      `${RISK_WORD[summary.risk] || summary.risk}. ${counts.join(", ")}. ${relyWords(summary.recorded_consumers, it)}, ${testWords(summary.recorded_tests, it).replace(/^No/, "no")}.`,
      "",
    );
    guide.chapters.forEach((chapter, index) => {
      lines.push(`### ${index + 1}. ${chapter.title}`, "", chapter.reason, "");
      for (const step of chapter.steps) {
        const where = step.within ? ` in ${step.within}` : "";
        lines.push(`- \`${step.name}\` (${words(step.kind)}${where}, ${step.change}): ${step.narration}`);
        const broke = step.findings.some((finding) => finding.severity === "error");
        const outside = step.connections.consumers.filter((consumer) => !consumer.changed_here).map((consumer) => `\`${consumer.entity.name}\``);
        if (outside.length) lines.push(`  - ${broke ? "Not updated" : "Unchanged callers"}: ${outside.join(", ")}`);
        const tests = step.connections.tests.map((test) => `\`${test.entity.name}\``);
        if (tests.length) lines.push(`  - Tests: ${tests.join(", ")}`);
        for (const finding of step.findings.filter((item) => item.severity === "error")) lines.push(`  - ${finding.message}`);
      }
      lines.push("");
    });
    for (const [title, items] of [
      ["Breaking changes", guide.findings.breaking_changes],
      ["Contract violations", guide.findings.contract_violations],
      ["Missing tests", guide.findings.test_coverage_gaps],
    ]) {
      if (!items.length) continue;
      lines.push(`**${title}**`, "", ...items.map((item) => `- ${item}`), "");
    }
    if (guide.not_walked) lines.push(guide.not_walked.message, "");
    const asked = guide.kind === "working" ? "--working" : guide.kind === "range" && guide.range ? `${guide.range.base_ref}..${guide.range.head_ref === "HEAD" ? "" : guide.range.head_ref}` : short(change.id);
    if (guide.kind === "working") {
      lines.push("Changes not committed yet. The same walk prints with `kin review guide --working`.");
    } else {
      lines.push(`The same walk prints with \`kin review guide ${asked}\`.`);
    }
    return lines.join("\n");
  }

  /**
   * A ref as a reader says it: a branch by its name, a change by its title,
   * and never sixty-four characters of ID.
   */
  function refWords(reference) {
    if (reference === "HEAD") {
      const on = state.branches.find((branch) => branch.active);
      return on ? on.name : "your current branch";
    }
    const bare = reference.replace(/^(change|kin):/, "");
    if (state.branches.some((branch) => branch.name === bare)) return bare;
    const listed = /^[0-9a-f]{7,64}$/i.test(bare) ? state.changes.find((change) => change.id.startsWith(bare.toLowerCase())) : null;
    if (listed) return `\u201c${untitled(listed)}\u201d`;
    return /^[0-9a-f]{12,64}$/i.test(bare) ? "an earlier change" : bare;
  }

  /** The line above a guide's title: what it is, whose, and when. */
  function eyebrow(guide) {
    const { change } = guide;
    if (guide.kind === "working") {
      return h(
        "p",
        { class: "eyebrow" },
        dotted(h("span", { class: "eyebrow-lead" }, "Since your last commit"), h("span", { class: "live-read", id: "live-read" }, "checked just now")),
      );
    }
    if (guide.kind === "range" && guide.range) {
      const { range } = guide;
      // A title that already names both sides needs only the count here.
      const title = titleOf(guide);
      const head = refWords(range.head_ref);
      const named = title.includes(head) && title.includes(refWords(range.base_ref));
      return h(
        "p",
        { class: "eyebrow eyebrow-range" },
        named ? plural(range.changes_total, "change", "changes") : `${head} compared with ${refWords(range.base_ref)}`,
      );
    }
    const ids = change.git_commit ? `Change ${change.id}\nGit commit ${change.git_commit}` : `Change ${change.id}`;
    return h(
      "p",
      { class: "eyebrow" },
      dotted(
        change.author ? h("span", { title: change.author_recorded || null }, change.author) : null,
        change.timestamp ? h("time", { datetime: change.timestamp, title: when(change.timestamp) }, day(change.timestamp)) : null,
      ),
      h("button", { class: "id-copy", type: "button", title: ids, on: { click: () => copy(change.id, "Change ID") } }, "Copy ID"),
    );
  }

  /**
   * Which agent made this change, and what it read through Kin first, when an
   * agent session made it. The entities link to their workspace pages when
   * this review is opened from the workspace. A person's change has no line.
   */
  function agentLine(guide) {
    const agent = guide.kind === "change" && guide.change ? guide.change.agent : null;
    if (!agent || typeof agent.name !== "string" || !agent.name) return null;
    const spark = () => h("span", { class: "agent-line-mark", "aria-hidden": "true" });
    const line = h("p", { class: "agent-line" }, spark(), h("span", {}, `${agent.name} made this change.`));
    const workspace = document.body.dataset.workspaceUrl;
    getJson(`/api/change-agent?change=${encodeURIComponent(guide.change.id)}`)
      .then((found) => {
        const looked = found && found.looked_at;
        if (!looked || !Number.isSafeInteger(looked.count)) return;
        const names = (found && found.names) || {};
        const shown = (looked.entity_ids || []).filter((id) => names[id] && names[id].label).slice(0, 6);
        const amount = `${looked.complete ? "" : "at least "}${plural(looked.count, "entity", "entities")}`;
        const parts = [looked.count ? `${agent.name} made this change after looking at ${amount}` : `${agent.name} made this change without looking anything up through Kin first`];
        shown.forEach((id, index) => {
          parts.push(index === 0 ? ": " : index === shown.length - 1 && looked.count === shown.length ? " and " : ", ");
          const label = h("code", {}, names[id].label);
          parts.push(workspace ? h("a", { class: "agent-line-entity", href: `${workspace}#/entity/${encodeURIComponent(id)}` }, label) : label);
        });
        if (shown.length && looked.count > shown.length) parts.push(` and ${plural(looked.count - shown.length, "other", "others")}`);
        parts.push(".");
        line.replaceChildren(spark(), h("span", {}, ...parts));
      })
      .catch(() => {
        // The name above stands on its own; what the agent read is extra.
      });
    return line;
  }

  /**
   * What a guide is called at the top of the page. A range is asked for by
   * two refs, and a reader holds a branch name or a change's own title more
   * easily than sixty-four characters of id.
   */
  function titleOf(guide) {
    const { change, range } = guide;
    if (guide.kind !== "range" || !range) return untitled(change);
    const id = (reference) => reference.replace(/^(change|kin):/, "");
    const base = state.changes.find((listed) => listed.id === id(range.base_ref));
    if (base && base.title && range.head_ref === "HEAD") return `Everything since \u201c${base.title}\u201d`;
    const on = state.branches.find((branch) => branch.active);
    const head = range.head_ref === "HEAD" ? (on ? on.name : "HEAD") : null;
    const known = state.branches.some((branch) => branch.name === range.base_ref);
    return head && known ? `${head} against ${range.base_ref}` : untitled(change);
  }

  /** Say the title again once the list or the branches it is read from arrive. */
  function retitle() {
    // History can move while the reader is searching. Keep offered actions
    // tied to the same current ancestry as the header without clearing input.
    if (!byId("palette").hidden) {
      palette.items = paletteItems();
      renderPalette();
    }
    if (!state.guide) return;
    const title = byId("main").querySelector(".change-head h1");
    if (!title) return;
    title.textContent = titleOf(state.guide);
    document.title = `${titleOf(state.guide)} · Kin review`;
    const above = byId("main").querySelector(".change-head .eyebrow-range");
    if (above && isRange()) above.replaceWith(eyebrow(state.guide));
    const since = byId("main").querySelector(".since-change");
    if (since) since.hidden = !canWalkSince(state.guide);
  }

  /**
   * The history list contains ancestors of the current head. A change named
   * by a route can be on another branch, so its age alone proves nothing.
   * An ancestor beyond the loaded list is left unoffered until it is read.
   */
  function canWalkSince(guide) {
    return Boolean(guide && guide.kind === "change" && state.changes.slice(1).some((change) => change.id === guide.change.id));
  }

  /** The changes a range holds, each a way into its own walk. */
  function rangeChanges(range) {
    if (!range.changes.length) return null;
    const rows = range.changes.map((change) =>
      h(
        "li",
        {},
        h(
          "a",
          { class: "range-change", href: `#/c/${change.id}` },
          h("span", { class: "range-title" }, untitled(change)),
          h("span", { class: "range-meta" }, dotted(byline(change).map((part) => h("span", {}, part)))),
        ),
      ),
    );
    const left = range.changes_total - range.changes.length;
    return h(
      "details",
      { class: "range-changes", open: range.changes.length <= 6 },
      h("summary", {}, range.changes_total === 1 ? "The change in this comparison" : `The ${range.changes_total} changes in this comparison`),
      h("ol", { class: "range-list" }, rows),
      left > 0 ? h("p", { class: "caveat" }, `The ${left} oldest aren't listed here. What follows covers them too.`) : null,
    );
  }

  /**
   * What the page says when the working tree differs and no entity changed
   * with it. That is still uncommitted work, and saying nothing would read as
   * a clean tree.
   */
  function noEntityChanged(guide) {
    const tree = guide.working_tree;
    return h(
      "section",
      { class: "quiet" },
      h("h2", {}, "No entity changed"),
      h(
        "p",
        {},
        tree && !tree.derived
          ? "You have uncommitted edits, and Kin couldn't tell yet which entities they change."
          : "You have uncommitted edits, and none of them changes an entity. They go in with your next commit.",
      ),
    );
  }

  /** What a page with nothing uncommitted says in place of a walk. */
  function nothingUncommitted(guide) {
    const base = guide.change.base;
    return h(
      "section",
      { class: "quiet" },
      h("h2", {}, "Nothing uncommitted"),
      h(
        "p",
        {},
        "Everything is committed. Edit something and this page walks it as you go: what changed, what relies on it, and what you haven't updated yet.",
      ),
      base ? h("p", {}, h("a", { class: "write-button", href: `#/c/${base}` }, "Open your last commit")) : null,
    );
  }

  /**
   * Draw a guide. `keep` is given when the same subject is drawn again because
   * it moved underneath the reader, as the working tree does: the reader's
   * place and the marks on steps that did not move are kept.
   */
  function renderGuide(guide, keep) {
    const main = byId("main");
    const { change, summary } = guide;
    state.steps = [];
    state.draftFields.clear();
    state.current = -1;
    state.read = keep ? keep.read : guide.kind === "working" ? new Set() : loadRead(subjectOf(guide));
    state.reviews = guide.reviews || [];
    state.reviewsTotal = guide.reviews_total || state.reviews.length;
    document.title = `${titleOf(guide)} · Kin review`;

    const drawn = map.renderMap(guide, { open: openMapNode });
    // Uncommitted work is said in the page's own words, not the server's.
    const rest = guide.kind === "working" ? "Changes you haven't committed yet." : change.message.split("\n").slice(1).join("\n").trim();
    const counts = [
      summary.modified ? `${summary.modified} modified` : null,
      summary.added ? `${summary.added} added` : null,
      summary.removed ? `${summary.removed} removed` : null,
    ].filter(Boolean);

    const working = guide.kind === "working";
    const changed = summary.added + summary.modified + summary.removed;
    const it = changed === 1 ? "it" : "them";
    const tree = guide.working_tree;
    // No entity changed. Whether the tree is clean is a separate question,
    // answered by its files.
    const noEntity = working && !changed && !guide.not_walked;
    const idle = noEntity && (!tree || (!tree.differs && tree.derived));
    // History can arrive after this guide, so retain the shortcut and update
    // its visibility whenever the current head's ancestry is read again.
    const since =
      guide.kind === "change"
        ? h("a", { class: "fact fact-link since-change", hidden: !canWalkSince(guide), href: `#/r/${change.id}..`, title: "Review everything after this change together" }, "Everything since this change")
        : null;
    const lede = headline(guide, drawn ? drawn.counts : null);
    const copySummary = h("button", { class: "fact fact-link", type: "button", title: "Copy this walk as Markdown, for a pull request or a message" }, "Copy summary");
    copySummary.addEventListener("click", () => copy(summaryMarkdown(guide, lede), "Summary"));
    const head = h(
      "header",
      { class: "change-head" },
      eyebrow(guide),
      agentLine(guide),
      h("h1", {}, titleOf(guide)),
      rest ? h("p", { class: "message" }, rest) : null,
      noEntity ? null : h("p", { class: "lede" }, lede),
      noEntity
        ? null
        : h(
            "div",
            { class: "facts" },
            riskPill(summary.risk, summary.recorded_consumers, it),
            counts.length ? h("span", { class: "fact" }, counts.join(", ")) : null,
            // The risk chip already says how many places rely on it when it can.
            RISK_WORD[summary.risk] && summary.recorded_consumers ? null : h("span", { class: "fact" }, relyWords(summary.recorded_consumers, it)),
            h("span", { class: "fact" }, testWords(summary.recorded_tests, it)),
            summary.relation_changes ? h("span", { class: "fact" }, plural(summary.relation_changes, "connection changed", "connections changed")) : null,
            since,
            copySummary,
          ),
      guide.kind === "range" && guide.range ? rangeChanges(guide.range) : null,
    );

    const caveats = statusNotes(guide);

    const chapters = h("div", { class: "chapters" });
    guide.chapters.forEach((chapter, chapterIndex) => {
      const section = h(
        "section",
        { class: "chapter", id: `chapter-${chapterIndex}` },
        h(
          "header",
          { class: "chapter-head" },
          h("p", { class: "chapter-no" }, `Chapter ${chapterIndex + 1} of ${guide.chapters.length}`),
          h("h2", {}, chapter.title),
          h("p", { class: "chapter-why" }, prose(chapter.reason)),
        ),
      );
      chapter.steps.forEach((step, stepIndex) => section.append(stepCard(step, `${chapterIndex + 1}.${stepIndex + 1}`)));
      chapters.append(section);
    });

    const overall = [
      findingList("Breaking changes", guide.findings.breaking_changes, "error"),
      findingList("Contract violations", guide.findings.contract_violations, "error"),
      findingList("Work in progress affected", guide.findings.work_risks, "warning"),
      findingList("Missing tests", guide.findings.test_coverage_gaps, "warning"),
      findingList("Notes", guide.findings.notes, "info"),
    ].filter(Boolean);

    const terminal = working
      ? "kin review guide --working"
      : guide.kind === "range" && guide.range
        ? `kin review guide ${guide.range.base_ref}..${guide.range.head_ref === "HEAD" ? "" : guide.range.head_ref}`
        : `kin review ${short(change.id)} --details`;
    const parts = idle
      ? [head, nothingUncommitted(guide)]
      : noEntity
        ? [head, noEntityChanged(guide), caveats]
        : [
          head,
          drawn ? drawn.node : h("p", { class: "notice" }, `This ${nounOf(guide)} holds no entity a review can act on.`),
          caveats,
          buildReviewPanel(),
          chapters,
          guide.not_walked ? h("p", { class: "notice" }, prose(guide.not_walked.message)) : null,
          overall.length ? h("section", { class: "overall" }, h("h2", {}, "Also worth checking"), overall) : null,
          h(
            "footer",
            { class: "foot" },
            working ? "Before is your last commit and after is your edits. The same walk prints with " : "The same walk prints with ",
            h("code", {}, terminal),
            ".",
          ),
          buildReviewBar(),
        ];
    if (idle || noEntity) {
      state.panel = null;
      state.bar = null;
    }
    // replaceChildren would print a missing part as the word "null".
    if (working) parts.splice(1, 0, h("div", { id: "working-read-status", class: "read-status", role: "status", "aria-live": "polite", hidden: true }));
    main.replaceChildren(...parts.filter(Boolean));
    renderWorkingReadStatus();
    renderOutline(guide);
    renderReviews();
    renderProgress();
    watchSteps();
    if (keep) {
      window.scrollTo(0, keep.scroll);
    } else {
      window.scrollTo(0, 0);
    }
  }

  // ── The rail ─────────────────────────────────────────────────────────────

  const standingOf = (change) => (state.standing[change.id] ? state.standing[change.id].state : null);

  function shownChanges() {
    if (state.filter === "all" || !state.standingKnown) return state.changes;
    return state.changes.filter((change) => {
      const where = standingOf(change);
      if (state.filter === "open") return !where || where === "pending";
      return where === state.filter;
    });
  }

  function renderFilters() {
    const filters = byId("filters");
    filters.hidden = !state.standingKnown;
    filters.replaceChildren(
      ...FILTERS.map(([value, title]) =>
        h(
          "button",
          {
            class: "filter",
            type: "button",
            "aria-pressed": String(state.filter === value),
            on: {
              click: () => {
                state.filter = value;
                renderFilters();
                renderRail();
              },
            },
          },
          title,
        ),
      ),
    );
  }

  /** The branch the working tree is on, when it is not the default one. */
  function activeBranch() {
    const active = state.branches.find((branch) => branch.active);
    const base = state.branches.find((branch) => branch.default);
    return active && base && active.name !== base.name ? { active, base } : null;
  }

  /** The second pinned entry: everything this branch holds that the default one does not. */
  function branchEntry() {
    const on = activeBranch();
    if (!on) return null;
    const href = `#/r/${encodeURIComponent(on.base.name)}..`;
    const selected = isRange() && state.guide.range.base_ref === on.base.name && state.guide.range.head_ref === "HEAD";
    return h(
      "a",
      { class: `change change-branch ${selected ? "is-selected" : ""}`, href, "aria-current": selected ? "page" : false },
      h("span", { class: "change-title" }, "This branch"),
      h("span", { class: "change-meta" }, h("code", {}, on.active.name), h("span", {}, `against ${on.base.name}`)),
    );
  }

  async function loadBranches() {
    try {
      const held = await getJson("/api/branches");
      state.branches = Array.isArray(held.branches) ? held.branches : [];
    } catch (_) {
      state.branches = [];
    }
    const context = byId("review-branch");
    if (context) context.textContent = state.branches.find((branch) => branch.active)?.name || "Branch unknown";
    renderRail();
    retitle();
  }

  /**
   * The changes known to belong to the range shown. All of them when the
   * range is small enough to carry whole, and otherwise only its newest, so
   * a change marked as in the range always is.
   */
  function rangeMembers() {
    if (!isRange()) return new Set();
    const { range } = state.guide;
    if (range.members_complete && Array.isArray(range.members)) return new Set(range.members);
    return new Set(range.changes.map((change) => change.id));
  }

  /** The entry that stays at the top of the list: what is not recorded yet. */
  function workingEntry() {
    const held = state.working;
    const count = held ? held.changed : null;
    const differs = Boolean(held && held.differs);
    const selected = isWorking();
    const meta =
      count === null
        ? "Changes you haven't committed yet"
        : count
          ? `${plural(count, "entity", "entities")} changed`
          : differs
            ? "Uncommitted, no entity changed"
            : "Nothing uncommitted";
    return h(
      "a",
      {
        class: `change change-working ${selected ? "is-selected" : ""} ${count || differs ? "has-work" : ""}`,
        href: "#/w",
        "aria-current": selected ? "page" : false,
      },
      h("span", { class: "change-title" }, h("span", { class: "work-dot", "aria-hidden": "true" }), "Uncommitted work"),
      h(
        "span",
        { class: "change-meta" },
        h("span", {}, meta),
        state.workingRead.error ? h("span", { class: "caveat-warn" }, held ? "Couldn't refresh" : "Couldn't load") : null,
      ),
    );
  }

  function renderRail() {
    const list = byId("changes");
    const selected = state.guide && state.guide.kind === "change" ? state.guide.change.id : null;
    const inRange = rangeMembers();
    const rows = shownChanges().map((change) => {
      const where = standingOf(change);
      // A change Kin recorded for itself, such as an upgrade checkpoint,
      // changes no code. It stays listed, quieter, and says what it is.
      const checkpoint = change.kin_checkpoint === true;
      return h(
        "a",
        {
          class: `change ${change.id === selected ? "is-selected" : ""} ${inRange.has(change.id) ? "in-range" : ""} ${checkpoint ? "is-checkpoint" : ""}`,
          href: `#/c/${change.id}`,
          "aria-current": change.id === selected ? "page" : false,
        },
        h("span", { class: "change-title" }, untitled(change)),
        h(
          "span",
          { class: "change-meta" },
          dotted(
            byline(change).map((part) => h("span", {}, part)),
            // The count the change's own guide walks, once the server has
            // said it. The history's delta count also holds module containers
            // and classes read through their members, so it is never shown here.
            checkpoint ? h("span", { title: "Kin made this for itself. It changes no code." }, "Kin update") : countWords(change),
          ),
          where && where !== "pending" ? h("span", { class: `standing-dot standing-${where}` }, STANDING_WORD[where]) : null,
        ),
      );
    });
    if (!rows.length) {
      rows.push(
        h("p", { class: "none rail-empty" }, state.changes.length ? "No change matches this filter." : "This repository records no changes yet."),
      );
    }
    if (state.truncated && state.filter === "all") {
      rows.push(
        h(
          "button",
          { class: "more", type: "button", disabled: state.loadingOlder, on: { click: loadOlder } },
          state.loadingOlder ? "Loading older changes" : "Older changes",
        ),
      );
    }
    list.replaceChildren(...[workingEntry(), branchEntry(), ...rows].filter(Boolean));
  }

  /** Open the newest change, or what the server opens first when no list is read yet. */
  function openLatest() {
    const latest = state.changes[0];
    if (latest) location.hash = `#/c/${latest.id}`;
    else loadGuide(null);
  }

  /** Whether a failed read says Kin does not hold what was asked for. */
  const notHeld = (error) =>
    error.status === 404 || /cannot resolve|never imported|not found|no such|unknown (change|ref|revision)/i.test(error.message || "");

  /**
   * What the page shows when a guide cannot be read: one plain line, one
   * action, and what the server said behind Show details.
   */
  function renderRefusal(error, route, retry) {
    document.title = "Kin review";
    byId("outline").replaceChildren();
    const kind = route ? route.kind : "change";
    const missing = notHeld(error);
    const [line, action] =
      kind === "session"
        ? ["Kin can't reach this repository right now.", { label: "Try again", run: retry }]
        : missing && kind === "change"
          ? ["Kin doesn't have this change.", { label: "Open the latest change", run: openLatest }]
          : missing && kind === "range"
            ? ["Kin can't find one side of this comparison.", { label: "Open the latest change", run: openLatest }]
            : kind === "working"
              ? ["Kin couldn't load your uncommitted changes.", { label: "Try again", run: retry }]
              : kind === "range"
                ? ["Kin couldn't open this comparison.", { label: "Try again", run: retry }]
                : ["Kin couldn't open this change.", { label: "Try again", run: retry }];
    byId("main").replaceChildren(h("div", { class: "refusal" }, problem(line, action, error.message, "section")));
  }

  /**
   * The repository as the workspace names it: "pallets / itsdangerous" with
   * the owner muted, or its name alone when the session gives no owner. A
   * host other than GitHub is said after the branch.
   */
  function renderRepository(session) {
    const text = (value) => (typeof value === "string" ? value.trim() : "");
    const owner = text(session.owner);
    const name = text(session.repository) || "repository";
    const repo = byId("repo");
    repo.replaceChildren(
      ...(owner
        ? [h("span", { class: "repo-owner" }, owner), h("span", { class: "repo-sep", "aria-hidden": "true" }, " / "), h("span", { class: "repo-name" }, name)]
        : [h("span", { class: "repo-name" }, name)]),
    );
    repo.setAttribute("title", owner ? `${owner}/${name}` : name);
    const host = byId("review-host");
    if (!host) return;
    const where = text(session.host);
    // Only a session that says it has no host is local; an older one says nothing.
    host.replaceChildren(
      ...(where && where !== "github.com"
        ? [h("span", { class: "repo-host" }, ` · ${where}`)]
        : "host" in session && !where
          ? [h("span", { class: "pill-local" }, "Local")]
          : []),
    );
  }

  /** Where the listed changes stand, when this server can say. An older one cannot. */
  async function loadStanding() {
    try {
      const held = await getJson("/api/standing");
      state.standing = held.standing || {};
      state.standingKnown = true;
    } catch (_) {
      state.standingKnown = false;
    }
    renderFilters();
    renderRail();
  }

  /**
   * What the list says a change holds: its guide's count, merges included.
   * A merge the server did not count, from an older server or one Kin could
   * not walk, says it is a merge rather than show a count it does not have.
   */
  function countWords(change) {
    const counted = state.counts.get(change.id);
    if (!counted) return null;
    if (counted.merge && counted.merge_counted !== true) {
      return h("span", { title: "Its review walks what it brought into the branch it joined" }, "merge");
    }
    if (typeof counted.entities !== "number") return null;
    return h("span", {}, plural(counted.entities, "entity", "entities"));
  }

  /**
   * Ask how many entities each listed change's guide walks, for the changes
   * not asked about yet. It runs beside the history poll and never holds it.
   */
  async function loadCounts() {
    if (Date.now() < state.countsHeldUntil) return;
    const wanted = state.changes.map((change) => change.id).filter((id) => !state.counts.has(id) && !state.countsAsked.has(id));
    for (let at = 0; at < wanted.length; at += COUNTS_ASKED) {
      const batch = wanted.slice(at, at + COUNTS_ASKED);
      batch.forEach((id) => state.countsAsked.add(id));
      try {
        const held = await getJson(`/api/counts?changes=${batch.join(",")}`);
        const counts = (held && held.counts) || {};
        // A change the server could not count is not asked about again.
        for (const id of batch) state.counts.set(id, counts[id] || null);
      } catch (_) {
        // The list reads without counts, and asks again a little later.
        state.countsHeldUntil = Date.now() + COUNTS_RETRY_MS;
        return;
      } finally {
        batch.forEach((id) => state.countsAsked.delete(id));
      }
      renderRail();
    }
  }

  /**
   * The newest page of history, as a poll reads it, with the older pages the
   * reader already asked for kept below it while they still follow from it.
   * When the newest page still holds the change that headed the list, history
   * only grew and what was read below stays. Otherwise the line moved, and
   * the list starts again from the newest page.
   */
  function withOlderPages(newest, truncated) {
    const previous = state.changes;
    const grew = previous.length > newest.length && newest.some((change) => change.id === previous[0].id);
    if (!truncated || !grew) return { changes: newest, truncated };
    const listed = new Set(newest.map((change) => change.id));
    return { changes: newest.concat(previous.filter((change) => !listed.has(change.id))), truncated: state.truncated };
  }

  /**
   * The next page of history past the last change listed. Each page is cut
   * from the same walk, so pages join; one that overlaps a change recorded
   * since the list was read is kept once.
   */
  async function loadOlder() {
    if (state.loadingOlder) return;
    state.loadingOlder = true;
    renderRail();
    const skip = state.changes.length;
    const count = Math.min(Math.max(skip, CHANGES_FIRST), CHANGES_PAGE_MAX);
    try {
      const held = await getJson(`/api/changes?count=${count}&skip=${skip}`);
      const listed = new Set(state.changes.map((change) => change.id));
      const fresh = held.changes.filter((change) => !listed.has(change.id));
      state.changes = state.changes.concat(fresh);
      // A page that adds nothing ends the list, so the button cannot ask forever.
      state.truncated = Boolean(held.truncated) && fresh.length > 0;
    } catch (error) {
      toast("Kin couldn't load older changes. Try again in a moment.");
    } finally {
      state.loadingOlder = false;
    }
    renderRail();
    retitle();
    loadCounts();
  }

  async function loadChanges() {
    const request = ++state.changesRequest;
    // The newest page, as long as what is listed and never past one read.
    const count = Math.min(Math.max(state.changes.length, CHANGES_FIRST), CHANGES_PAGE_MAX);
    try {
      const held = await getJson(`/api/changes?count=${count}`);
      if (request !== state.changesRequest) return true;
      const merged = withOlderPages(held.changes, Boolean(held.truncated));
      state.changes = merged.changes;
      state.truncated = merged.truncated;
      state.changesError = null;
    } catch (error) {
      if (request !== state.changesRequest) return true;
      state.changes = [];
      state.truncated = false;
      state.changesError = error.message;
      renderConnection();
      retitle();
      byId("changes").replaceChildren(
        problem("Kin couldn't load the list of changes.", { label: "Try again", run: () => loadChanges() }, error.message),
      );
      return false;
    }
    renderRail();
    retitle();
    renderConnection();
    loadCounts();
    return true;
  }

  /**
   * The shape of a guide while it is read. It fades in after a moment, so a
   * guide that answers at once never shows it.
   */
  function skeleton(saying) {
    const bar = (kind) => h("div", { class: `sk ${kind}`, "aria-hidden": "true" });
    return h(
      "div",
      { class: "skeleton", "aria-busy": "true" },
      h("p", { class: "loading" }, saying),
      bar("sk-eyebrow"),
      bar("sk-title"),
      bar("sk-lede"),
      bar("sk-lede sk-short"),
      bar("sk-map"),
      bar("sk-card"),
    );
  }

  async function loadGuide(route) {
    const request = (state.request += 1);
    state.guideLoadingKind = route ? route.kind : "change";
    const workingRequest = route && route.kind === "working" ? ++state.workingRequest : null;
    state.guide = null;
    state.steps = [];
    state.panel = null;
    state.bar = null;
    byId("main").replaceChildren(
      skeleton(route && route.kind === "working" ? "Loading your uncommitted changes." : route && route.kind === "range" ? "Loading the comparison." : "Loading the change."),
    );
    byId("outline").replaceChildren();
    // A guide read while a write is still on its way could be taken before
    // the write lands and drawn after it, putting the older history back on
    // the page. Reading only once every sent write has answered rules that
    // out: the guide then holds whatever they recorded.
    await state.writes;
    if (request !== state.request) return;
    try {
      const guide = await getJson(guideUrl(route));
      if (request !== state.request) return;
      state.guide = guide;
      state.guideText = guideText(guide);
      if (guide.kind === "working") {
        noteWorking(guide);
        Object.assign(state.workingRead, { lastSuccess: Date.now(), displayedAt: Date.now(), error: null, pending: false, loading: false });
      }
      const hash = hashOf(guide);
      if (location.hash !== hash) history.replaceState(null, "", hash);
      renderGuide(guide);
    } catch (error) {
      if (request !== state.request) return;
      state.guide = null;
      if (workingRequest === state.workingRequest) state.workingRead.error = error.message;
      renderRefusal(error, route, () => loadGuide(route));
    }
    state.guideLoadingKind = null;
    renderRail();
    renderConnection();
    const current = byId("changes").querySelector(".is-selected");
    if (current) current.scrollIntoView({ block: "nearest" });
  }

  // ── Routes ───────────────────────────────────────────────────────────────
  // #/c/<change> is a recorded change, #/w the working tree, and
  // #/r/<base>..<head> a range. A route is what the reader asked for; the
  // guide that comes back says what it resolved to.

  function routeFromHash() {
    const hash = location.hash;
    if (hash === "#/w") return { kind: "working" };
    const range = /^#\/r\/(.+?)\.\.(.*)$/.exec(hash);
    if (range) return { kind: "range", base: decodeURIComponent(range[1]), change: decodeURIComponent(range[2]) || null };
    const change = /^#\/c\/([^/]+)/.exec(hash);
    return change ? { kind: "change", change: decodeURIComponent(change[1]) } : null;
  }

  function hashOf(guide) {
    if (guide.kind === "working") return "#/w";
    if (guide.kind === "range" && guide.range) {
      const head = guide.range.head_ref === "HEAD" ? "" : encodeURIComponent(guide.range.head_ref);
      return `#/r/${encodeURIComponent(guide.range.base_ref)}..${head}`;
    }
    return `#/c/${guide.change.id}`;
  }

  function guideUrl(route) {
    if (!route) return "/api/guide";
    if (route.kind === "working") return "/api/guide?working=1";
    const query = [];
    if (route.change) query.push(`change=${encodeURIComponent(route.change)}`);
    if (route.kind === "range") query.push(`base=${encodeURIComponent(route.base)}`);
    return `/api/guide${query.length ? `?${query.join("&")}` : ""}`;
  }

  const isWorking = () => Boolean(state.guide && state.guide.kind === "working");
  /** What a guide calls the thing it walks. */
  const nounOf = (guide) => (guide && guide.kind === "working" ? "work" : guide && guide.kind === "range" ? "comparison" : "change");
  const isRange = () => Boolean(state.guide && state.guide.kind === "range" && state.guide.range);

  /**
   * What a guide is about, as one string. A range and its last change share a
   * change id, and what is read or recorded on one is not about the other.
   */
  function subjectOf(guide) {
    if (!guide) return null;
    if (guide.kind === "working") return "WORKSPACE";
    if (guide.kind === "range" && guide.range) return `${guide.range.merge_base}..${guide.change.id}`;
    return guide.change.id;
  }

  /** What decides whether a guide reads the same as the one shown. Its clock does not. */
  const guideText = (guide) =>
    JSON.stringify([guide.chapters, guide.summary, guide.findings, guide.not_walked || null, guide.change.message, guide.change.base, guide.working_tree || null]);

  const readTime = (at) => new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" });

  function renderConnection() {
    const live = byId("live");
    const working = state.workingRead;
    const down = Boolean(state.sessionError || state.changesError);
    live.textContent = down ? "Not connected" : working.error ? "Couldn't refresh" : "Connected";
    live.classList.toggle("is-down", down || Boolean(working.error));
    live.setAttribute("role", "status");
    live.setAttribute("title", down ? state.sessionError || state.changesError : working.error
      ? `Kin couldn't refresh your uncommitted changes. ${working.error}`
      : "Kin is answering for this repository.");
  }

  /** A clock time as the page says it: "8:40 PM". */
  const clockTime = (at) => new Date(at).toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit" });

  function renderWorkingReadStatus() {
    if (!isWorking()) return;
    const read = state.workingRead;
    const stamp = byId("live-read");
    if (stamp) {
      stamp.textContent = read.displayedAt !== null ? `checked ${clockTime(read.displayedAt)}` : "checked just now";
      stamp.setAttribute("title", read.displayedAt !== null ? `Kin last checked your edits at ${readTime(read.displayedAt)}` : "");
      stamp.classList.toggle("is-stale", Boolean(read.error || read.pending));
    }
    const notice = byId("working-read-status");
    if (!notice) return;
    // A background check in progress says nothing: the stamp above already
    // says when Kin last looked, and a line that flashes every few seconds
    // would only be noise.
    const since = read.displayedAt !== null ? ` This shows what it saw at ${clockTime(read.displayedAt)}.` : "";
    const message = read.error ? `Kin couldn't refresh your uncommitted changes.${since}`
      : read.pending ? "Your code changed since this was loaded." : "";
    notice.hidden = !message;
    notice.classList.toggle("is-stale", Boolean(read.error || read.pending));
    const key = `${message}\u0000${read.error || ""}`;
    if (notice.dataset.message === key) return;
    notice.dataset.message = key;
    notice.replaceChildren(
      message
        ? problem(message, { label: read.error ? "Try again" : "Refresh", run: () => refreshWorking(true) }, read.error || null)
        : "",
    );
  }

  function noteWorking(guide) {
    const { summary } = guide;
    const before = state.working ? state.working.base : null;
    // `base` is the change the working tree stands on, which is what decides
    // whether a recorded guide's places are places in the checkout.
    state.working = {
      changed: summary.added + summary.modified + summary.removed,
      differs: Boolean(guide.working_tree && guide.working_tree.differs),
      base: guide.change.base || null,
    };
    if (state.working.base !== before) rebuildPlaces();
  }

  /**
   * Read the working tree again without moving the reader. Asked on a timer,
   * and paced by how long the last read took, so a large repository is asked
   * less often than a small one.
   */
  async function refreshWorking(force = false) {
    if (state.guideLoadingKind === "working" || state.workingBusy || (!force && Date.now() < state.workingNext)) return;
    const shown = isWorking();
    state.workingBusy = true;
    const started = Date.now();
    const request = state.request;
    const workingRequest = ++state.workingRequest;
    state.workingRead.loading = true;
    renderWorkingReadStatus();
    try {
      const guide = await getJson("/api/guide?working=1");
      // An earlier working response must not change the repository context or
      // freshness of a guide opened after it was requested.
      if (request !== state.request || workingRequest !== state.workingRequest) return;
      Object.assign(state.workingRead, { lastSuccess: Date.now(), error: null, loading: false });
      noteWorking(guide);
      if (isWorking() && request === state.request) {
        const text = guideText(guide);
        if (text !== state.guideText) {
          // A step keeps its mark only if it reads exactly as it did.
          const before = new Map(state.guide.chapters.flatMap((chapter) => chapter.steps).map((step) => [step.entity_id, JSON.stringify(step)]));
          const read = new Set(
            guide.chapters
              .flatMap((chapter) => chapter.steps)
              .filter((step) => state.read.has(step.entity_id) && before.get(step.entity_id) === JSON.stringify(step))
              .map((step) => step.entity_id),
          );
          // Not while the reader is typing or has something open.
          const typing = document.activeElement instanceof HTMLElement && document.contains(document.activeElement)
            && /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement.tagName);
          const overlay = !byId("palette").hidden || !byId("help").hidden;
          if (force || (!typing && !overlay)) {
            state.guide = guide;
            state.guideText = text;
            Object.assign(state.workingRead, { displayedAt: Date.now(), pending: false });
            renderGuide(guide, { scroll: window.scrollY, read });
            toast("Your code changed. This page is up to date again.");
          } else {
            state.workingRead.pending = true;
          }
        } else {
          Object.assign(state.workingRead, { displayedAt: Date.now(), pending: false });
        }
      }
      renderRail();
    } catch (error) {
      if (request !== state.request || workingRequest !== state.workingRequest) return;
      state.workingRead.error = error.message;
      renderRail();
    } finally {
      const took = Date.now() - started;
      // Shown, it follows closely. Unshown, it is only a count in the list.
      if (workingRequest === state.workingRequest) {
        state.workingNext = Date.now() + (shown ? Math.max(POLL_MS - 500, took * 4) : Math.max(30_000, took * 20));
        state.workingRead.loading = false;
      }
      state.workingBusy = false;
      renderWorkingReadStatus();
      renderConnection();
    }
  }

  function stepBy(delta) {
    if (!state.steps.length) return;
    const next = state.current < 0 ? (delta > 0 ? 0 : state.steps.length - 1) : state.current + delta;
    setCurrent(Math.max(0, Math.min(state.steps.length - 1, next)), true);
  }

  function changeBy(delta) {
    if (!state.guide || !state.changes.length) return;
    const shown = shownChanges();
    // The uncommitted work sits above the newest change, as it does in the list.
    if (isWorking()) {
      if (delta > 0 && shown[0]) location.hash = `#/c/${shown[0].id}`;
      return;
    }
    if (state.guide.kind !== "change") return;
    const index = shown.findIndex((change) => change.id === state.guide.change.id);
    if (index === 0 && delta < 0) {
      location.hash = "#/w";
      return;
    }
    const next = shown[index + delta];
    if (index >= 0 && next) location.hash = `#/c/${next.id}`;
  }

  // ── Live ─────────────────────────────────────────────────────────────────
  // The list follows the repository. A change recorded while the page is open
  // shows up without a reload, and the reader is told rather than moved.

  async function follow() {
    if (document.hidden || !state.session) return;
    const before = state.changes[0] ? state.changes[0].id : null;
    const ok = await loadChanges();
    if (!ok) return;
    await loadStanding();
    const newest = state.changes[0];
    const moved = newest && before && newest.id !== before;
    state.polls += 1;
    if (moved) {
      // A commit is the usual reason the newest change moved, and it empties
      // the working tree's entry, so that is read again now and not in turn.
      state.workingNext = 0;
    }
    await refreshWorking();
    if (moved || state.polls % 6 === 0) await loadBranches();
    if (newest && before && newest.id !== before && (!state.guide || state.guide.change.id !== newest.id)) {
      toast(`New change: ${untitled(newest)}`, {
        label: "Open",
        run: () => {
          location.hash = `#/c/${newest.id}`;
        },
      });
    }
  }

  // Schedule only after the complete cycle settles, including the standing
  // index. A slow repository must not collect overlapping history reads.
  async function poll() {
    try {
      await follow();
    } finally {
      setTimeout(poll, POLL_MS);
    }
  }

  // ── Search ───────────────────────────────────────────────────────────────

  const palette = { items: [], shown: [], at: 0 };

  function paletteItems() {
    const items = [];
    state.steps.forEach((step, index) => {
      items.push({
        kind: "Step",
        label: step.name,
        detail: `${step.number} · ${CHANGE_WORD[step.change] || step.change}`,
        run: () => setCurrent(index, true),
      });
    });
    for (const change of state.changes) {
      items.push({
        kind: "Change",
        label: untitled(change),
        detail: byline(change).join(" · "),
        // Still found by its ID, which is never shown.
        find: change.id,
        run: () => {
          location.hash = `#/c/${change.id}`;
        },
      });
    }
    if (state.bar && state.bar.comment) {
      items.push(
        { kind: "Action", label: `Approve this ${nounOf(state.guide)}`, detail: "a", run: () => { state.bar.open(); state.bar.buttons.approved.focus(); } },
        { kind: "Action", label: `Mark this ${nounOf(state.guide)} as needing work`, detail: "w", run: () => { state.bar.open(); state.bar.buttons.needs_work.focus(); } },
        { kind: "Action", label: "Comment on your decision", detail: "c", run: () => { state.bar.open(); state.bar.comment.focus(); } },
      );
    }
    items.push({
      kind: "Action",
      label: "Open the uncommitted work",
      detail: "u",
      run: () => {
        location.hash = "#/w";
      },
    });
    const base = state.branches.find((branch) => branch.default);
    if (base) {
      for (const branch of state.branches) {
        if (branch.name === base.name) continue;
        items.push({
          kind: "Branch",
          label: `Walk ${branch.name} compared with ${base.name}`,
          detail: branch.active ? "this branch" : "",
          run: () => {
            location.hash = `#/r/${encodeURIComponent(base.name)}..${branch.active ? "" : encodeURIComponent(branch.name)}`;
          },
        });
      }
    }
    items.push({ kind: "Action", label: `Choose which editor opens code (now ${EDITOR_WORD[state.editor]})`, detail: "e", run: cycleEditor });
    if (state.guide && !(isWorking() && !state.steps.length)) {
      items.push({
        kind: "Action",
        label: "Copy this walk as Markdown",
        detail: "y",
        run: () => copy(summaryMarkdown(state.guide, headline(state.guide, null)), "Summary"),
      });
    }
    if (canWalkSince(state.guide)) {
      const from = state.guide.change.id;
      items.push({
        kind: "Action",
        label: "Walk everything since this change",
        detail: `${short(from).slice(0, 8)}..`,
        run: () => {
          location.hash = `#/r/${from}..`;
        },
      });
    }
    items.push(
      { kind: "Action", label: state.split ? "Show changes in one column" : "Show changes side by side", detail: "s", run: toggleSplit },
      { kind: "Action", label: "Change colour theme", detail: "t", run: cycleTheme },
      { kind: "Action", label: "Keyboard shortcuts", detail: "?", run: () => openOverlay("help") },
    );
    return items;
  }

  function renderPalette() {
    const typed = byId("palette-input").value.trim();
    const query = typed.toLowerCase().split(/\s+/).filter(Boolean);
    palette.shown = palette.items
      .filter((item) => {
        const hay = `${item.kind} ${item.label} ${item.detail} ${item.find || ""}`.toLowerCase();
        return query.every((word) => hay.includes(word));
      })
      .slice(0, 40);
    // Two refs with two dots between them is a range, as it is in a terminal.
    const range = /^(\S+?)\.\.(\S*)$/.exec(typed);
    if (range) {
      palette.shown.unshift({
        kind: "Range",
        label: `Walk ${range[1]}..${range[2] || "HEAD"}`,
        detail: "Enter",
        run: () => {
          location.hash = `#/r/${encodeURIComponent(range[1])}..${encodeURIComponent(range[2])}`;
        },
      });
    }
    palette.at = Math.min(palette.at, Math.max(0, palette.shown.length - 1));
    const list = byId("palette-list");
    list.replaceChildren(
      ...(palette.shown.length
        ? palette.shown.map((item, index) =>
            h(
              "li",
              {
                class: `palette-item ${index === palette.at ? "is-active" : ""}`,
                id: `palette-result-${index}`,
                role: "option",
                "aria-selected": String(index === palette.at),
                on: {
                  click: () => runPalette(index),
                  mousemove: () => {
                    if (palette.at !== index) {
                      palette.at = index;
                      renderPalette();
                    }
                  },
                },
              },
              h("span", { class: "palette-kind" }, item.kind),
              h("span", { class: "palette-label" }, item.label),
              h("span", { class: "palette-detail" }, item.detail),
            ),
          )
        : [h("li", { class: "palette-none" }, "Nothing matches.")]),
    );
    const active = list.querySelector(".is-active");
    const input = byId("palette-input");
    if (active) input.setAttribute("aria-activedescendant", active.getAttribute("id"));
    else input.removeAttribute("aria-activedescendant");
    if (active) active.scrollIntoView({ block: "nearest" });
  }

  function runPalette(index) {
    const item = palette.shown[index];
    if (!item) return;
    closeOverlays();
    item.run();
  }

  // ── Overlays ─────────────────────────────────────────────────────────────

  let returnFocus = null;
  let inertBackground = [];

  function overlayControls(overlay) {
    return [...overlay.querySelectorAll("*")].filter((node) => {
      if (node.disabled || node.getAttribute("tabindex") === "-1") return false;
      for (let parent = node; parent && parent !== overlay; parent = parent.parentNode) {
        if (parent.hidden || parent.inert) return false;
      }
      return /^(BUTTON|INPUT|TEXTAREA|SELECT)$/.test(node.tagName)
        || (node.tagName === "A" && node.getAttribute("href")) || node.getAttribute("tabindex") !== null;
    });
  }

  function openOverlay(id) {
    closeOverlays();
    returnFocus = document.activeElement;
    const overlay = byId(id);
    overlay.hidden = false;
    inertBackground = [...document.body.children].filter((node) => node !== overlay).map((node) => [node, node.inert]);
    for (const [node] of inertBackground) node.inert = true;
    if (id === "palette") {
      palette.items = paletteItems();
      palette.at = 0;
      const input = byId("palette-input");
      input.value = "";
      input.setAttribute("aria-expanded", "true");
      renderPalette();
      input.focus();
    } else {
      byId("help-close").focus();
    }
  }

  function closeOverlays() {
    const open = ["palette", "help"].filter((id) => !byId(id).hidden);
    open.forEach((id) => {
      byId(id).hidden = true;
    });
    for (const [node, wasInert] of inertBackground) node.inert = wasInert;
    inertBackground = [];
    byId("palette-input").setAttribute("aria-expanded", "false");
    if (open.length) {
      if (returnFocus instanceof HTMLElement && document.contains(returnFocus)) returnFocus.focus();
      else byId("main").focus();
    }
    returnFocus = null;
  }

  const KEYS = [
    ["j / k", "Next and previous step"],
    ["x", "Mark the current step read"],
    ["n", "Write a note on the current step"],
    ["[ / ]", "Newer and older change"],
    ["u", "Open the uncommitted work"],
    ["/", "Search steps, changes and actions, or type a range like main.."],
    ["a / w", "Go to Approve or Needs work"],
    ["c", "Comment on your decision"],
    ["h", "How the current step's entity got here"],
    ["e", "Choose which editor opens code"],
    ["s", "Changes side by side or in one column"],
    ["t", "Change colour theme"],
    ["?", "This list"],
  ];

  // ── Keys ─────────────────────────────────────────────────────────────────

  function onKey(event) {
    const overlayOpen = !byId("palette").hidden || !byId("help").hidden;
    if (event.key === "Escape" && overlayOpen) {
      event.preventDefault();
      closeOverlays();
      return;
    }
    if (event.key === "Tab" && overlayOpen) {
      const overlay = byId(!byId("palette").hidden ? "palette" : "help");
      const controls = overlayControls(overlay);
      const at = controls.indexOf(document.activeElement);
      if (!controls.length) {
        event.preventDefault();
      } else if (at < 0 || (event.shiftKey ? at === 0 : at === controls.length - 1)) {
        event.preventDefault();
        controls[event.shiftKey ? controls.length - 1 : 0].focus();
      }
      return;
    }
    if (!byId("palette").hidden) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        const count = palette.shown.length;
        if (count) palette.at = (palette.at + (event.key === "ArrowDown" ? 1 : count - 1)) % count;
        renderPalette();
      } else if (event.key === "Enter") {
        event.preventDefault();
        runPalette(palette.at);
      }
      return;
    }
    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      openOverlay("palette");
      return;
    }
    if (overlayOpen || event.metaKey || event.ctrlKey || event.altKey) return;
    const target = event.target;
    if (target instanceof HTMLElement && (target.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))) {
      if (event.key === "Escape") target.blur();
      return;
    }
    const act = {
      j: () => stepBy(1),
      k: () => stepBy(-1),
      x: () => state.current >= 0 && toggleRead(state.current),
      n: () => state.current >= 0 && !isWorking() && state.steps[state.current].noteBox.focus(),
      u: () => {
        location.hash = "#/w";
      },
      "]": () => changeBy(1),
      "[": () => changeBy(-1),
      "/": () => openOverlay("palette"),
      "?": () => openOverlay("help"),
      a: () => { if (state.bar?.comment) { state.bar.open(); state.bar.buttons.approved.focus(); } },
      w: () => { if (state.bar?.comment) { state.bar.open(); state.bar.buttons.needs_work.focus(); } },
      c: () => { if (state.bar?.comment) { state.bar.open(); state.bar.comment.focus(); } },
      s: toggleSplit,
      t: cycleTheme,
      e: cycleEditor,
      h: () => {
        if (state.current < 0) return;
        const panel = state.steps[state.current].card.querySelector("details.history");
        if (panel) {
          panel.open = !panel.open;
          panel.scrollIntoView({ block: "nearest" });
        }
      },
    }[event.key];
    if (!act) return;
    event.preventDefault();
    act();
  }

  // ── Start ────────────────────────────────────────────────────────────────

  document.addEventListener("keydown", onKey);
  document.addEventListener("focusin", (event) => {
    const overlay = ["palette", "help"].map(byId).find((node) => !node.hidden);
    if (overlay && !overlay.contains(event.target)) {
      const first = overlayControls(overlay)[0];
      if (first) first.focus();
    }
  });
  window.addEventListener("hashchange", () => {
    const wasInRail = byId("rail").contains(document.activeElement);
    byId("rail").classList.remove("is-expanded");
    if (wasInRail && window.matchMedia("(max-width: 920px)").matches) byId("main").focus({ preventScroll: true });
    byId("rail-toggle")?.setAttribute("aria-expanded", "false");
    if (byId("rail-toggle")) byId("rail-toggle").textContent = "Browse changes";
    if (!state.session) return;
    const route = routeFromHash();
    if (route && (!state.guide || location.hash !== hashOf(state.guide))) loadGuide(route);
  });
  byId("rail-toggle")?.addEventListener("click", () => {
    const expanded = byId("rail").classList.toggle("is-expanded");
    byId("rail-toggle").setAttribute("aria-expanded", String(expanded));
    byId("rail-toggle").textContent = expanded ? "Hide changes" : "Browse changes";
  });
  byId("theme").addEventListener("click", cycleTheme);
  byId("editor").addEventListener("click", cycleEditor);
  byId("palette-open").addEventListener("click", () => openOverlay("palette"));
  byId("palette-input").addEventListener("input", () => {
    palette.at = 0;
    renderPalette();
  });
  byId("palette-input").setAttribute("role", "combobox");
  byId("palette-input").setAttribute("aria-controls", "palette-list");
  byId("palette-input").setAttribute("aria-autocomplete", "list");
  byId("palette-input").setAttribute("aria-expanded", "false");
  byId("help-open").addEventListener("click", () => openOverlay("help"));
  byId("help-close").addEventListener("click", closeOverlays);
  for (const id of ["palette", "help"]) {
    byId(id).addEventListener("mousedown", (event) => {
      if (event.target === byId(id)) closeOverlays();
    });
  }
  byId("keys-list").replaceChildren(...KEYS.flatMap(([keys, what]) => [h("dt", {}, keys.split(" / ").flatMap((key, index) => (index ? [" ", h("kbd", {}, key)] : [h("kbd", {}, key)]))), h("dd", {}, what)]));

  state.split = store.get("kin.review.split", false);
  state.wrap = store.get("kin.review.wrap", window.matchMedia("(max-width: 600px)").matches);
  document.body.classList.toggle("wrap-source", state.wrap);
  applyTheme(store.get("kin.review.theme", "auto"));

  async function start() {
    if (state.starting || state.polling) return;
    state.starting = true;
    state.sessionError = null;
    byId("main").replaceChildren(skeleton("Loading this repository."));
    try {
      state.session = await getJson("/api/session");
    } catch (error) {
      state.sessionError = error.message;
      state.starting = false;
      renderConnection();
      renderRefusal(error, { kind: "session" }, start);
      return;
    }
    renderRepository(state.session);
    const workspaceUrl = document.body.dataset.workspaceUrl;
    if (workspaceUrl && !byId("workspace-back")) {
      byId("repo").parentNode.append(h("a", { id: "workspace-back", class: "workspace-back", href: workspaceUrl }, "Back to workspace"));
    }
    // The reader's own choice wins. Without one, the editor the shell names.
    applyEditor(store.get("kin.review.editor", state.session.editor || "off"));
    renderFilters();
    // The first reads share the same no-overlap boundary as later cycles.
    // What is checked out also decides which places can open in an editor.
    const asked = state.session.initial_working
      ? { kind: "working" }
      : state.session.initial_base
        ? { kind: "range", base: state.session.initial_base, change: state.session.initial_change || null }
        : state.session.initial_change
          ? { kind: "change", change: state.session.initial_change }
          : null;
    const initialGuide = loadGuide(routeFromHash() || asked);
    const initialReads = Promise.all([loadChanges().then(() => loadStanding()), loadBranches(), refreshWorking()]);
    await initialGuide;
    await initialReads;
    state.starting = false;
    state.polling = true;
    renderConnection();
    setTimeout(poll, POLL_MS);
  }
  start();
})();
