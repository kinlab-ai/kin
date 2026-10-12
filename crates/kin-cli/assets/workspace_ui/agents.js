// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

"use strict";

// The agents working in this repository: who is connected, what each one is
// doing in plain words, what it looked at, what it is changing, and the
// changes it recorded. Everything here comes from Kin's own record of each
// agent's calls. Kin keeps the tool an agent used and the entities it was
// about, never the words it asked with, so neither does this page.
(() => {
  const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  const KEY = /^[0-9a-f]{16}$/;
  const text = (value) => typeof value === "string" ? value : "";
  const array = (value) => Array.isArray(value) ? value : [];
  const isCount = (value) => Number.isSafeInteger(value) && value >= 0;
  const count = (n) => Number(n).toLocaleString();
  const plural = (n, one, many) => `${count(n)} ${n === 1 ? one : many || `${one}s`}`;
  const NUMBER_WORD = ["No", "One", "Two", "Three", "Four", "Five", "Six", "Seven", "Eight", "Nine", "Ten"];
  const spoken = (n) => n < NUMBER_WORD.length ? NUMBER_WORD[n] : count(n);

  // ── Words ────────────────────────────────────────────────────────────────

  /** What kind of thing a tool does, from the name the daemon recorded. */
  function family(tool) {
    const name = text(tool).toLowerCase();
    if (name === "unknown" || name === "unrecognized_tool") return "unknown";
    if (/^(semantic_locate|locate|semantic_search|search|lexical_lookup|lookup)$/.test(name)) return "locate";
    if (/^(find_references|refs|references)$/.test(name)) return "refs";
    if (/^(impact_analysis|impact)$/.test(name)) return "impact";
    if (/^(get_entity_source|source)$/.test(name)) return "source";
    if (/^(get_context_pack|context)$/.test(name)) return "context";
    if (/^(graph_neighborhood|neighbors|neighborhood)$/.test(name)) return "neighbors";
    if (/^(trace_path|trace_data_flow|trace)$/.test(name)) return "trace";
    if (/^(kin_provenance_query|history|provenance)$/.test(name)) return "history";
    if (/^(kin_graph_status|status)$/.test(name)) return "status";
    if (name === "kin_transaction_begin") return "begin";
    if (name === "kin_transaction_stage") return "stage";
    if (name === "kin_transaction_commit") return "commit";
    if (name === "kin_transaction_abort") return "abort";
    if (/^(kin_mutate|mutate)$/.test(name)) return "mutate";
    return "other";
  }
  const READS = new Set(["locate", "refs", "impact", "source", "context", "neighbors", "trace", "history", "other"]);
  const WRITES = new Set(["stage", "commit", "mutate"]);

  /**
   * One call in plain words, as parts: strings, and entities the page links to
   * their own pages. "Looked up Signer.verify_signature and 4 related entities".
   */
  function describe(call, names = {}) {
    const ids = array(call?.entity_ids).filter((id) => UUID.test(text(id)));
    const named = ids.filter((id) => text(names[id]?.label));
    const entity = (id) => ({ id, label: names[id].label });
    const first = named[0] ? entity(named[0]) : null;
    const second = named[1] ? entity(named[1]) : null;
    const found = isCount(call?.result_count) ? call.result_count : ids.length;
    const some = first || "an entity";
    let parts;
    switch (family(call?.tool)) {
      case "locate":
        parts = first
          ? ["Looked up ", first, found > 1 ? ` and ${plural(found - 1, "related entity", "related entities")}` : ""]
          : [found ? `Looked something up and found ${plural(found, "entity", "entities")}` : "Looked something up and found no entity"];
        break;
      case "refs": parts = ["Checked what uses ", some]; break;
      case "impact": parts = ["Checked what a change to ", some, " would touch"]; break;
      case "source": parts = ["Read ", some]; break;
      case "context": parts = ["Read the context around ", some]; break;
      case "neighbors": parts = ["Looked at what ", some, " connects to"]; break;
      case "trace": parts = first && second ? ["Traced how ", first, " reaches ", second] : ["Traced where ", some, " leads"]; break;
      case "history": parts = ["Read the history of ", some]; break;
      case "status": parts = ["Checked the repository's status"]; break;
      case "begin": parts = ["Started a change"]; break;
      case "stage": parts = first ? ["Staged an edit to ", first] : ["Staged an edit"]; break;
      // A commit's answer also names entities that only moved with the edit,
      // so a commit is described by the entity it was made to, alone.
      case "commit": parts = first ? ["Committed a change to ", first] : ["Committed a change"]; break;
      case "abort": parts = ["Dropped a change it had started"]; break;
      case "mutate": parts = first ? ["Changed ", first] : ["Changed the code"]; break;
      default: parts = first ? ["Asked Kin about ", first] : ["Asked Kin a question"];
    }
    if (call?.is_error) parts.push(", but Kin could not answer");
    return parts.filter((part) => part !== "");
  }
  const plainText = (parts) => parts.map((part) => typeof part === "string" ? part : part.label).join("");

  /** Calls in a row that did the same thing to the same entity, read as one. */
  function group(calls) {
    const groups = [];
    for (const call of array(calls)) {
      const last = groups.at(-1);
      const same = last && family(last.call.tool) === family(call.tool) && text(last.call.entity_ids?.[0]) === text(call.entity_ids?.[0]) && Boolean(last.call.is_error) === Boolean(call.is_error);
      if (same) { last.times += 1; last.oldest = call; }
      else groups.push({ call, times: 1, oldest: call });
    }
    return groups;
  }

  /** A call's time to the second: agents make several calls a minute. */
  function stamp(at) {
    const date = new Date(at);
    return Number.isNaN(date.getTime()) ? "" : date.toLocaleTimeString(undefined, { hour: "numeric", minute: "2-digit", second: "2-digit" });
  }

  /** How long ago a time was, in a few words. */
  function span(from, to = Date.now()) {
    const start = new Date(from).getTime();
    if (!Number.isFinite(start)) return null;
    const seconds = Math.max(0, Math.round((to - start) / 1000));
    if (seconds < 60) return seconds < 10 ? "a few seconds" : `${seconds} seconds`;
    const minutes = Math.round(seconds / 60);
    if (minutes < 60) return plural(minutes, "minute");
    const hours = Math.round(minutes / 60);
    if (hours < 48) return plural(hours, "hour");
    return plural(Math.round(hours / 24), "day");
  }

  function stateWords(agent) {
    if (agent.state === "active") return agent.busy ? ["Working now", "live"] : ["Active", "live"];
    if (agent.state === "idle") return ["Idle", "idle"];
    return ["Disconnected", "ended"];
  }

  /** The line under an agent's name: how long it has been here, or since it was. */
  function presence(agent) {
    if (agent.state === "ended") { const gone = span(agent.last_activity_at); return gone ? `Last active ${gone} ago` : "No longer connected"; }
    const here = span(agent.started_at);
    if (agent.state === "idle") { const quiet = span(agent.last_activity_at); return [here ? `Connected for ${here}` : "", quiet ? `quiet for ${quiet}` : ""].filter(Boolean).join(", "); }
    return here ? `Connected for ${here}` : "Connected";
  }

  /** The entities an agent's calls were about, newest first and each once: what it asked about, then what the answers named. */
  function lookedAt(agent, names) {
    const reads = array(agent.recent_calls).filter((call) => READS.has(family(call.tool)));
    const seen = new Set();
    const ids = [];
    const take = (id) => { if (UUID.test(text(id)) && !seen.has(id)) { seen.add(id); ids.push(id); } };
    for (const call of reads) take(array(call.entity_ids)[0]);
    for (const call of reads) array(call.entity_ids).forEach(take);
    return { named: ids.filter((id) => text(names[id]?.label)), total: ids.length };
  }

  /** The Agents lede: who is connected now, in a sentence of real counts. */
  function headline(data) {
    const agents = array(data?.agents);
    const working = agents.filter((agent) => agent.state === "active").length;
    const earlier = array(data?.earlier).length;
    if (!agents.length) return earlier ? `No agent is connected right now. ${spoken(earlier)} worked here earlier.` : "No agent is connected right now.";
    if (agents.length === 1) return working ? "One agent is connected and working." : "One agent is connected. It is idle.";
    const connected = `${spoken(agents.length)} agents are connected.`;
    if (!working) return `${connected} None is working right now.`;
    if (working === agents.length) return `${connected} ${agents.length === 2 ? "Both are" : "Each one is"} working.`;
    return `${connected} ${spoken(working)} ${working === 1 ? "is" : "are"} working now.`;
  }

  // ── Drawing ──────────────────────────────────────────────────────────────

  /** A small spark that marks an agent, beside its name wherever it appears. */
  function mark(size = 14) {
    const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    for (const [key, value] of Object.entries({ viewBox: "0 0 24 24", width: String(size), height: String(size), class: "agent-mark", "aria-hidden": "true", focusable: "false" })) svg.setAttribute(key, value);
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", "M12 2.5c.5 4.6 2.9 7 7.5 7.5-4.6.5-7 2.9-7.5 7.5-.5-4.6-2.9-7-7.5-7.5 4.6-.5 7-2.9 7.5-7.5zM19 15.5c.2 1.8 1.1 2.7 2.9 2.9-1.8.2-2.7 1.1-2.9 2.9-.2-1.8-1.1-2.7-2.9-2.9 1.8-.2 2.7-1.1 2.9-2.9z");
    path.setAttribute("fill", "currentColor");
    svg.append(path);
    return svg;
  }

  function sentence(ctx, parts) {
    return parts.map((part) => typeof part === "string" ? part : ctx.h("a", { class: "agent-entity", href: ctx.entityHref(part.id) }, part.label));
  }

  function chips(ctx, ids, names, total, label) {
    const SHOWN = 6;
    if (!ids.length) return null;
    const shown = ids.slice(0, SHOWN);
    const more = total - shown.length;
    return ctx.h("div", { class: "agent-chips", "aria-label": label },
      ctx.h("span", { class: "agent-chips-label" }, label),
      shown.map((id) => ctx.h("a", { class: "chip", href: ctx.entityHref(id), title: names[id]?.kind ? ctx.kindWord(names[id].kind) : undefined }, names[id].label)),
      more > 0 ? ctx.h("span", { class: "chip chip-more" }, `${count(more)} more`) : null);
  }

  /** What an agent has open: a change it is staging, and the work it said it is doing. */
  function openWork(ctx, agent, names) {
    const rows = [];
    for (const open of array(agent.open_changes).slice(0, 3)) {
      const edits = isCount(open.staged_edits) ? open.staged_edits : 0;
      const age = isCount(open.age_secs) ? span(Date.now() - open.age_secs * 1000) : null;
      rows.push(ctx.h("p", { class: "agent-open" }, ctx.h("span", { class: "agent-open-dot", "aria-hidden": "true" }),
        edits ? `Changing the code: ${plural(edits, "edit")} staged, not committed yet` : "Started a change, nothing staged yet",
        age ? ctx.h("span", { class: "agent-quiet" }, ` · open for ${age}`) : null));
    }
    for (const intent of array(agent.intents).slice(0, 2)) {
      const ids = array(intent.entity_ids).filter((id) => text(names[id]?.label));
      rows.push(ctx.h("p", { class: "agent-open" }, ctx.h("span", { class: "agent-open-dot agent-open-intent", "aria-hidden": "true" }),
        text(intent.task) ? `Working on “${intent.task}”` : "Holding entities while it works",
        ids.length ? [ctx.h("span", { class: "agent-quiet" }, ": "), ids.slice(0, 3).flatMap((id, index) => [index ? ", " : "", ctx.h("a", { class: "agent-entity", href: ctx.entityHref(id) }, names[id].label)])] : null));
    }
    return rows.length ? ctx.h("div", { class: "agent-open-list" }, rows) : null;
  }

  // The guide of each agent's latest change, read once: a recorded change
  // never changes, so its review sentence never does either.
  const guides = new Map();
  function guideOf(ctx, id) {
    const known = guides.get(id);
    if (known) return known;
    const entry = { guide: null, error: null };
    guides.set(id, entry);
    if (guides.size > 32) guides.delete(guides.keys().next().value);
    ctx.fetchJSON(`/api/guide?change=${encodeURIComponent(id)}`)
      .then((guide) => { entry.guide = guide; })
      .catch((error) => { entry.error = error.message || "unreadable"; })
      .finally(() => ctx.repaintAgents());
    return entry;
  }

  function latestChange(ctx, agent) {
    const change = array(agent.changes)[0];
    if (!change || !text(change.id)) return null;
    const read = guideOf(ctx, change.id);
    const said = read.guide ? ctx.workSentence(read.guide) : null;
    const counted = ctx.changedCount?.(change.id);
    const fallback = isCount(counted) ? (counted ? `${plural(counted, "entity", "entities")} changed.` : "No entity changed.") : null;
    const when = ctx.ago(change.time);
    return ctx.h("div", { class: "agent-change" },
      ctx.h("div", { class: "agent-change-text" },
        ctx.h("span", { class: "agent-change-label" }, agent.changes_total > 1 ? `Latest of ${count(agent.changes_total)} changes` : "Its change"),
        ctx.h("p", { class: "agent-change-title" }, text(change.title) || "Untitled change"),
        said ? ctx.h("p", { class: "agent-change-sentence" }, said)
          : read.error || !read.guide ? (read.error ? (fallback ? ctx.h("p", { class: "agent-change-sentence" }, fallback) : null) : ctx.skeleton("skeleton-line")) : fallback ? ctx.h("p", { class: "agent-change-sentence" }, fallback) : null,
        when ? ctx.h("time", { class: "agent-quiet", datetime: change.time }, when) : null),
      ctx.link("Review", ctx.changeURL(change.id), true, { "aria-label": `Review ${text(change.title) || "this change"}` }));
  }

  function nowRows(ctx, agent, names, shown = 3) {
    const groups = group(agent.recent_calls).slice(0, shown);
    if (!groups.length) return ctx.h("p", { class: "agent-quiet agent-none" }, "Connected. It has not asked Kin anything yet.");
    return ctx.h("ol", { class: "agent-now", "aria-label": "What it did last" }, groups.map((entry, index) =>
      ctx.h("li", { class: index === 0 && agent.state === "active" ? "agent-now-latest" : "" },
        ctx.h("span", { class: "agent-now-text" }, sentence(ctx, describe(entry.call, names)), entry.times > 1 ? ctx.h("span", { class: "agent-quiet" }, ` ×${entry.times}`) : null),
        ctx.h("time", { class: "agent-quiet", datetime: text(entry.call.at) || undefined }, ctx.ago(entry.call.at)))));
  }

  /** One agent as a card: who, what it is doing now, what it looked at, what it has open, what it changed. */
  function card(ctx, agent, data, { compact = false } = {}) {
    const names = data?.names || {};
    const [word, tone] = stateWords(agent);
    const looked = lookedAt(agent, names);
    return ctx.h("article", { class: `agent-card agent-${tone}`, "aria-label": `${agent.name}, ${word.toLowerCase()}` },
      ctx.h("header", { class: "agent-head" },
        ctx.h("span", { class: "agent-avatar", "aria-hidden": "true" }, mark(18)),
        ctx.h("div", { class: "agent-who" },
          ctx.h("a", { class: "agent-name", href: `#/agents/${agent.key}` }, agent.name),
          ctx.h("span", { class: "agent-presence" }, presence(agent))),
        ctx.h("span", { class: `agent-state agent-state-${tone}` }, ctx.h("span", { class: "agent-state-dot", "aria-hidden": "true" }), word)),
      nowRows(ctx, agent, names, compact ? 3 : 4),
      chips(ctx, looked.named, names, looked.total, "Looked at"),
      openWork(ctx, agent, names),
      latestChange(ctx, agent));
  }

  function connectHint(ctx) {
    return ctx.h("p", { class: "agent-hint" }, "Coding agents such as Claude Code, Codex and Cursor connect through Kin’s MCP server once you add them in Kin for Mac or with ", ctx.h("code", {}, "kin setup"), ".");
  }

  /** Home's panel: the agents connected now, above the repository map. */
  function panel(ctx) {
    const resource = ctx.read();
    const head = (extra) => ctx.h("div", { class: "section-head agents-head" },
      ctx.h("h2", { id: "home-agents-title" }, "Agents", liveBadge(ctx)), extra);
    if (resource.missing) return null;
    if (!resource.data) {
      if (resource.error) return ctx.h("section", { class: "agents-panel", "aria-labelledby": "home-agents-title" }, head(), ctx.failed("Kin couldn't load which agents are connected.", () => ctx.refresh(["agents"])));
      return ctx.h("section", { class: "agents-panel", "aria-labelledby": "home-agents-title", "aria-busy": "true" }, head(), ctx.h("div", { class: "agent-card agent-skeleton" }, ctx.skeleton("skeleton-line"), ctx.skeleton("skeleton-line"), ctx.skeleton("skeleton-line")));
    }
    const data = resource.data;
    const agents = array(data.agents);
    const earlier = array(data.earlier);
    const SHOWN = 4;
    if (!agents.length) {
      const last = earlier[0];
      return ctx.h("section", { class: "agents-panel agents-panel-empty", "aria-labelledby": "home-agents-title" }, head(earlier.length ? ctx.moreLink("See what agents did", `#/agents`) : null),
        ctx.h("div", { class: "agent-empty" },
          ctx.h("span", { class: "agent-avatar agent-avatar-quiet", "aria-hidden": "true" }, mark(18)),
          ctx.h("div", {},
            ctx.h("p", { class: "agent-empty-title" }, "No agent is working in this repository right now."),
            last ? ctx.h("p", { class: "agent-quiet" }, `${last.name} was here ${span(last.last_activity_at) ? `${span(last.last_activity_at)} ago` : "earlier"}. `, ctx.h("a", { href: `#/agents/${last.key}` }, "See what it did"), ".") : null,
            connectHint(ctx))),
        resource.error ? ctx.h("p", { class: "agent-quiet" }, "Showing what Kin last saw.") : null);
    }
    return ctx.h("section", { class: "agents-panel", "aria-labelledby": "home-agents-title" },
      head(ctx.moreLink(agents.length > SHOWN ? `See the other ${count(agents.length - SHOWN)}` : "Open the timeline", "#/agents")),
      ctx.h("div", { class: `agent-cards agent-cards-${Math.min(agents.length, 2)}` }, agents.slice(0, SHOWN).map((agent) => card(ctx, agent, data, { compact: true }))),
      resource.error ? ctx.h("p", { class: "agent-quiet" }, "Showing what Kin last saw. ", ctx.button("Refresh", () => ctx.refresh(["agents"]), { class: "button button-small" })) : null);
  }

  // ── The Agents page ──────────────────────────────────────────────────────

  const timeline = { key: null, data: null, error: null, busy: false, at: 0, timer: null };

  function loadTimeline(ctx, key, quiet = false) {
    if (!KEY.test(text(key))) return;
    if (timeline.key !== key) { timeline.key = key; timeline.data = null; timeline.error = null; }
    if (timeline.busy) { timeline.again = true; return; }
    timeline.busy = true;
    if (!quiet) ctx.paint("agents-detail");
    ctx.fetchJSON(`/api/workspace/agents/timeline?agent=${encodeURIComponent(key)}&limit=100`)
      .then((data) => { if (timeline.key === key) { timeline.data = data; timeline.error = null; timeline.at = Date.now(); } })
      .catch((error) => { if (timeline.key === key) timeline.error = { message: error.message || "unreadable", status: error.status }; })
      .finally(() => {
        timeline.busy = false;
        ctx.paint("agents-detail");
        if (timeline.again) { timeline.again = false; loadTimeline(ctx, timeline.key, true); }
      });
  }

  function everyAgent(data) { return [...array(data?.agents), ...array(data?.earlier)]; }

  function selectedKey(ctx, data) {
    const wanted = text(ctx.params().agent);
    if (KEY.test(wanted)) return wanted;
    return everyAgent(data)[0]?.key || null;
  }

  function agentList(ctx, data, selected) {
    const rows = everyAgent(data);
    if (!rows.length) return ctx.h("div", { class: "agent-empty agent-empty-page" },
      ctx.h("span", { class: "agent-avatar agent-avatar-quiet", "aria-hidden": "true" }, mark(18)),
      ctx.h("div", {}, ctx.h("p", { class: "agent-empty-title" }, "No agent has worked here since this page started."), connectHint(ctx)));
    return ctx.h("ul", { class: "agent-roster", "aria-label": "Agents" }, rows.slice(0, 12).map((agent) => {
      const [word, tone] = stateWords(agent);
      const last = array(agent.recent_calls)[0];
      return ctx.h("li", {}, ctx.h("a", { class: `agent-roster-row${agent.key === selected ? " is-selected" : ""}`, href: `#/agents/${agent.key}`, "aria-current": agent.key === selected ? "true" : undefined },
        ctx.h("span", { class: "agent-avatar", "aria-hidden": "true" }, mark(16)),
        ctx.h("span", { class: "agent-roster-text" },
          ctx.h("span", { class: "agent-roster-name" }, agent.name),
          ctx.h("span", { class: "agent-quiet" }, [plural(agent.calls_total || 0, "call"), agent.changes_total ? plural(agent.changes_total, "change") : null, last ? ctx.ago(last.at) : null].filter(Boolean).join(" · "))),
        ctx.h("span", { class: `agent-state agent-state-${tone}` }, ctx.h("span", { class: "agent-state-dot", "aria-hidden": "true" }), word)));
    }));
  }

  function timelineRows(ctx, calls, names) {
    const groups = group(calls);
    const SHOWN = 60;
    if (!groups.length) return ctx.h("p", { class: "gap" }, "It has not asked Kin anything yet.");
    return ctx.h("ol", { class: "agent-timeline" }, groups.slice(0, SHOWN).map((entry) => {
      const call = entry.call;
      const kind = family(call.tool);
      // A write names the entity it was made to; the others its answer lists only moved with it.
      const listed = WRITES.has(kind) ? array(call.entity_ids).slice(0, 1) : array(call.entity_ids);
      const ids = listed.filter((id) => text(names[id]?.label));
      const unnamed = listed.length - ids.length;
      const meta = [];
      if (entry.times > 1) meta.push(`${count(entry.times)} times`);
      if (READS.has(kind) && kind !== "source" && isCount(call.result_count)) meta.push(call.result_count ? `answer named ${plural(call.result_count, "entity", "entities")}` : "answer named no entity");
      if (isCount(call.duration_ms) && call.duration_ms >= 1000) meta.push(`took ${(call.duration_ms / 1000).toFixed(call.duration_ms < 10_000 ? 1 : 0)} s`);
      return ctx.h("li", { class: `agent-step agent-step-${kind}${call.is_error ? " agent-step-error" : ""}` },
        ctx.h("time", { class: "agent-step-time", datetime: text(call.at) || undefined }, stamp(call.at)),
        ctx.h("span", { class: "agent-step-dot", "aria-hidden": "true" }),
        ctx.h("div", { class: "agent-step-body" },
          ctx.h("p", { class: "agent-step-text" }, sentence(ctx, describe(call, names))),
          ids.length > 1 || unnamed > 0 ? ctx.h("div", { class: "agent-step-chips" }, ids.slice(1, 6).map((id) => ctx.h("a", { class: "chip chip-small", href: ctx.entityHref(id) }, names[id].label)),
            ids.length > 6 || unnamed > 0 ? ctx.h("span", { class: "chip chip-small chip-more" }, `${count(Math.max(0, ids.length - 6) + unnamed)} more`) : null) : null,
          meta.length ? ctx.h("p", { class: "agent-quiet agent-step-meta" }, meta.join(" · ")) : null));
    }), groups.length > SHOWN ? ctx.h("li", { class: "agent-step agent-step-more" }, ctx.h("p", { class: "agent-quiet" }, `${count(groups.length - SHOWN)} earlier steps not shown.`)) : null);
  }

  /** A change's count as its own review reads it, the number Changes shows; none until it is counted. */
  function changedWords(ctx, change) {
    const n = ctx.changedCount?.(change.id);
    return isCount(n) ? ctx.h("span", {}, n ? `${plural(n, "entity", "entities")} changed` : "No entity changed") : null;
  }

  function changeRows(ctx, changes) {
    if (!changes.length) return ctx.h("p", { class: "gap" }, "It has not committed a change.");
    return ctx.h("ul", { class: "list list-card" }, changes.slice(0, 16).map((change) => ctx.h("li", {}, ctx.h("a", { class: "change-row", href: ctx.changeURL(change.id) },
      ctx.h("div", {}, ctx.h("span", { class: "change-title" }, text(change.title) || "Untitled change"),
        ctx.h("span", { class: "change-meta" }, changedWords(ctx, change), ctx.h("time", { datetime: change.time }, ctx.ago(change.time)))),
      ctx.h("span", { class: "change-side" }, ctx.h("span", { class: "more-link" }, "Review"))))));
  }

  function detail(ctx, selected) {
    if (!selected) return null;
    if (timeline.key !== selected && !timeline.busy) loadTimeline(ctx, selected, true);
    const data = timeline.key === selected ? timeline.data : null;
    if (!data) {
      if (timeline.key === selected && timeline.error) {
        if (timeline.error.status === 404) return ctx.h("div", { class: "section" }, ctx.h("p", { class: "gap" }, "Kin no longer holds this agent's record. Its session ended before this page saw it."));
        return ctx.h("div", { class: "section" }, ctx.failed("Kin couldn't load what this agent did.", () => loadTimeline(ctx, selected)));
      }
      return ctx.h("div", { class: "agent-detail", "aria-busy": "true" }, ctx.skeleton("skeleton-lede"), ctx.skeletonLines(6));
    }
    const agent = data.agent || {};
    const names = data.names || {};
    const [word, tone] = stateWords(agent);
    const calls = array(data.calls);
    const total = isCount(agent.calls_total) ? agent.calls_total : calls.length;
    const kept = isCount(data.kept) ? data.kept : calls.length;
    const reads = calls.filter((call) => READS.has(family(call.tool)));
    const touched = new Set(reads.flatMap((call) => array(call.entity_ids)));
    const summary = [
      `${spoken(total)} ${total === 1 ? "call" : "calls"} to Kin`,
      touched.size ? `${plural(touched.size, "entity", "entities")} looked at${kept < total ? " in the latest " + count(kept) : ""}` : null,
      agent.changes_total ? plural(agent.changes_total, "change") + " committed" : "no change committed",
    ].filter(Boolean).join(", ");
    return ctx.h("div", { class: "agent-detail" },
      ctx.h("header", { class: "agent-detail-head" },
        ctx.h("span", { class: "agent-avatar agent-avatar-large", "aria-hidden": "true" }, mark(22)),
        ctx.h("div", {}, ctx.h("h2", { class: "agent-detail-name" }, agent.name || "Agent"),
          ctx.h("p", { class: "agent-presence" }, [presence(agent), agent.through === "mcp" ? "through Kin’s MCP server" : null].filter(Boolean).join(" · "))),
        ctx.h("span", { class: `agent-state agent-state-${tone}` }, ctx.h("span", { class: "agent-state-dot", "aria-hidden": "true" }), word)),
      ctx.h("p", { class: "lede lede-soft agent-summary" }, `${summary.charAt(0).toUpperCase()}${summary.slice(1)}.`),
      openWork(ctx, agent, names),
      array(agent.changes).length ? ctx.h("section", { class: "section", "aria-labelledby": "agent-changes-title" }, ctx.h("div", { class: "section-head" }, ctx.h("h2", { id: "agent-changes-title" }, agent.changes_total === 1 ? "The change it committed" : "Changes it committed")), changeRows(ctx, array(agent.changes))) : null,
      ctx.h("section", { class: "section", "aria-labelledby": "agent-steps-title" },
        ctx.h("div", { class: "section-head" }, ctx.h("h2", { id: "agent-steps-title" }, "What it did"), ctx.h("span", { class: "agent-quiet" }, "Newest first")),
        timelineRows(ctx, calls, names),
        ctx.h("p", { class: "agent-quiet agent-foot" }, kept < total ? `Showing its latest ${count(kept)} of ${count(total)} calls. Kin keeps a bounded record of each agent's calls.` : total === 1 ? "Its one call since it connected." : `Its ${count(total)} calls since it connected.`),
        timeline.error ? ctx.h("p", { class: "agent-quiet" }, "Showing what Kin last saw. ", ctx.button("Refresh", () => loadTimeline(ctx, selected), { class: "button button-small" })) : null));
  }

  function view(ctx) {
    return [
      ctx.h("header", {}, ctx.h("h1", {}, "Agents"),
        ctx.section("agents-lede", ["agents"], () => {
          const resource = ctx.read();
          if (resource.missing) return ctx.h("p", { class: "lede lede-soft" }, "This version of Kin does not report agent activity.");
          if (!resource.data) return resource.error ? null : ctx.skeleton("skeleton-lede");
          return ctx.h("p", { class: "lede lede-soft" }, liveBadge(ctx), headline(resource.data));
        })),
      ctx.h("div", { class: "agents-layout" },
        ctx.h("section", { class: "agents-roster-wrap", "aria-label": "Agents now and earlier" }, ctx.section("agents-list", ["agents"], () => {
          const resource = ctx.read();
          if (resource.missing) return null;
          if (!resource.data) return resource.error ? ctx.failed("Kin couldn't load which agents are connected.", () => ctx.refresh(["agents"])) : ctx.skeletonLines(3);
          return [agentList(ctx, resource.data, selectedKey(ctx, resource.data)),
            ctx.h("p", { class: "agent-quiet agent-privacy" }, "Kin records which tools each agent uses and the entities they were about. It never keeps the words an agent asks with.")];
        })),
        ctx.section("agents-detail", ["agents"], () => {
          const resource = ctx.read();
          if (!resource.data) return null;
          return detail(ctx, selectedKey(ctx, resource.data));
        }, { class: "agents-detail-wrap" }))];
  }

  // ── Following Kin live ───────────────────────────────────────────────────

  const live = { source: null, state: "off", retry: 0, timer: null, poll: null, ctx: null };

  function liveBadge(ctx) {
    if (live.state === "off") return null;
    const words = { on: "Live", waiting: "Reconnecting", polling: "Checking every 15 seconds" }[live.state] || "";
    return ctx.h("span", { class: `live-badge live-${live.state}`, title: live.state === "on" ? "This updates as agents work." : "The live feed is not connected. The page reads again on its own." }, ctx.h("span", { class: "live-dot", "aria-hidden": "true" }), words);
  }

  function setLive(next) {
    if (live.state === next) return;
    live.state = next;
    live.ctx?.repaintAgents();
  }

  /** Follow Kin's live feed. A pulse reads the agents again; a dropped feed is reopened politely, reading on a timer meanwhile. */
  function follow(ctx) {
    live.ctx = ctx;
    if (typeof EventSource !== "function") { setLive("off"); return () => {}; }
    const open = () => {
      live.timer = null;
      let source;
      try { source = new EventSource(`${ctx.apiBase}/api/workspace/events`); } catch (_) { schedule(); return; }
      live.source = source;
      source.addEventListener("ready", () => { live.retry = 0; setLive("on"); stopPolling(); ctx.pulse(["agents"]); });
      source.addEventListener("agent", (event) => {
        ctx.pulse(["agents"]);
        let data = null;
        try { data = JSON.parse(event.data); } catch (_) { /* A pulse needs no body. */ }
        if (data?.agent && data.agent === timeline.key) { clearTimeout(timeline.timer); timeline.timer = setTimeout(() => loadTimeline(ctx, timeline.key, true), 700); }
      });
      source.addEventListener("change", (event) => {
        ctx.pulse(["agents", "changes", "overview", "working"]);
        let data = null;
        try { data = JSON.parse(event.data); } catch (_) { /* A pulse needs no body. */ }
        const who = everyAgent(ctx.read().data).find((agent) => agent.key === data?.agent);
        ctx.announce(`${who ? who.name : "An agent"} committed a change.`);
        if (timeline.key) loadTimeline(ctx, timeline.key, true);
      });
      source.addEventListener("entities", (event) => {
        let data = null;
        try { data = JSON.parse(event.data); } catch (_) { /* A pulse needs no body. */ }
        ctx.pulse(data?.agent ? ["agents"] : ["working"], data?.agent ? 500 : 4000);
      });
      source.addEventListener("resync", () => ctx.pulse(["agents", "changes"]));
      source.addEventListener("closed", () => { source.close(); schedule(); });
      source.onerror = () => { source.close(); schedule(); };
    };
    const schedule = () => {
      live.source = null;
      setLive(live.retry > 0 ? "polling" : "waiting");
      startPolling(ctx);
      const delay = Math.min(60_000, 5_000 * 2 ** Math.min(live.retry, 4));
      live.retry += 1;
      clearTimeout(live.timer);
      live.timer = setTimeout(open, delay);
    };
    open();
    return () => { live.source?.close(); clearTimeout(live.timer); stopPolling(); live.state = "off"; };
  }
  function startPolling(ctx) { if (!live.poll) live.poll = setInterval(() => { if (!document.hidden) ctx.pulse(["agents"]); }, 15_000); }
  function stopPolling() { if (live.poll) { clearInterval(live.poll); live.poll = null; } }

  // ── On the map ───────────────────────────────────────────────────────────

  /**
   * What the connected agents are touching, for the repository map: what an
   * agent is editing or just committed, then what it read most recently.
   */
  function marks(data, limit = 12) {
    const tones = new Map();
    for (const agent of array(data?.agents)) {
      for (const call of array(agent.recent_calls)) {
        const kind = family(call.tool);
        const tone = ["stage", "commit", "mutate"].includes(kind) ? "changing" : READS.has(kind) ? "looked" : null;
        if (!tone) continue;
        for (const id of array(call.entity_ids).slice(0, tone === "changing" ? 1 : 3)) {
          if (!UUID.test(text(id))) continue;
          if (tone === "changing" || !tones.has(id)) tones.set(id, tone);
        }
      }
    }
    return [...tones].sort((a, b) => (a[1] === "changing" ? 0 : 1) - (b[1] === "changing" ? 0 : 1)).slice(0, limit);
  }

  /** A line under the map saying what its rings mean, when any are drawn. */
  function legend(ctx, data, shown) {
    const agents = array(data?.agents);
    if (!shown || !agents.length) return null;
    const who = agents.length === 1 ? agents[0].name : "Agents";
    const tones = new Set(marks(data).map(([, tone]) => tone));
    return ctx.h("p", { class: "map-agent-legend" },
      ctx.h("span", { class: "map-agent-key map-agent-key-looked", "aria-hidden": "true" }), `What ${who} looked at`,
      tones.has("changing") ? [ctx.h("span", { class: "map-agent-key map-agent-key-changing", "aria-hidden": "true" }), `what ${agents.length === 1 ? "it is" : "they are"} changing`] : null);
  }

  // ── Who made a change ────────────────────────────────────────────────────

  /** A change's author: an agent with its mark, or a person by name. */
  function author(ctx, change, name) {
    const agent = change?.agent;
    if (agent && text(agent.name)) return ctx.h("span", { class: "author-agent", title: "Made by an agent through Kin" }, mark(12), agent.name);
    return name ? ctx.h("span", {}, name) : null;
  }

  globalThis.KinWorkspaceAgents = { describe, plainText, group, family, span, presence, headline, panel, view, follow, author, mark, marks, legend, timeline, live };
})();
