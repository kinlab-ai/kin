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
  /** A number as it reads inside a sentence: "three", or "12". */
  const spokenInside = (n) => n < NUMBER_WORD.length ? NUMBER_WORD[n].toLowerCase() : count(n);
  // The server lists at most this many earlier sessions, so a full list is a lower bound.
  const EARLIER_SHOWN = 12;

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
  // The calls that open, fill, commit or drop a change: when one fails, whether a change is left open matters.
  const CHANGING = new Set(["begin", "stage", "commit", "abort", "mutate"]);
  // A failed call that ran this long waited on Kin rather than being refused by it.
  const WAITED_MS = 10_000;

  /**
   * One call in plain words, as parts: strings, and entities the page links to
   * their own pages. "Looked up Signer.verify_signature and 4 related entities".
   * A call that failed reads as what it tried, then why it did not happen:
   * "Tried to drop the change it had started; Kin did not answer within 28 s".
   */
  function describe(call, names = {}) {
    const known = names && typeof names === "object" ? names : {};
    const ids = array(call?.entity_ids).filter((id) => UUID.test(text(id)));
    const named = ids.filter((id) => text(known[id]?.label));
    const entity = (id) => ({ id, label: known[id].label });
    const first = named[0] ? entity(named[0]) : null;
    const second = named[1] ? entity(named[1]) : null;
    const found = isCount(call?.result_count) ? call.result_count : ids.length;
    const some = first || "an entity";
    const failed = Boolean(call?.is_error);
    let parts;
    switch (family(call?.tool)) {
      case "locate":
        parts = failed ? (first ? ["Tried to look up ", first] : ["Tried to look something up"])
          : first ? ["Looked up ", first, found > 1 ? ` and ${plural(found - 1, "related entity", "related entities")}` : ""]
            : [found ? `Looked something up and found ${plural(found, "entity", "entities")}` : "Looked something up and found no entity"];
        break;
      case "refs": parts = [failed ? "Tried to check what uses " : "Checked what uses ", some]; break;
      case "impact": parts = [failed ? "Tried to check what a change to " : "Checked what a change to ", some, " would touch"]; break;
      case "source": parts = [failed ? "Tried to read " : "Read ", some]; break;
      case "context": parts = [failed ? "Tried to read the context around " : "Read the context around ", some]; break;
      case "neighbors": parts = [failed ? "Tried to look at what " : "Looked at what ", some, " connects to"]; break;
      case "trace": parts = first && second ? [failed ? "Tried to trace how " : "Traced how ", first, " reaches ", second] : [failed ? "Tried to trace where " : "Traced where ", some, " leads"]; break;
      case "history": parts = [failed ? "Tried to read the history of " : "Read the history of ", some]; break;
      case "status": parts = [failed ? "Tried to check the repository's status" : "Checked the repository's status"]; break;
      case "begin": parts = [failed ? "Tried to start a change" : "Started a change"]; break;
      case "stage": parts = first ? [failed ? "Tried to stage an edit to " : "Staged an edit to ", first] : [failed ? "Tried to stage an edit" : "Staged an edit"]; break;
      // A commit's answer also names entities that only moved with the edit,
      // so a commit is described by the entity it was made to, alone.
      case "commit": parts = first ? [failed ? "Tried to commit a change to " : "Committed a change to ", first] : [failed ? "Tried to commit a change" : "Committed a change"]; break;
      case "abort": parts = [failed ? "Tried to drop the change it had started" : "Dropped a change it had started"]; break;
      case "mutate": parts = first ? [failed ? "Tried to change " : "Changed ", first] : [failed ? "Tried to change the code" : "Changed the code"]; break;
      default: parts = first ? [failed ? "Tried to ask Kin about " : "Asked Kin about ", first] : [failed ? "Tried to ask Kin a question" : "Asked Kin a question"];
    }
    if (failed) parts.push(whyFailed(call));
    // A call that finished after the agent stopped waiting for it still did what it did; the agent was never told.
    else if (call?.caller_left === true) parts.push(", after it had stopped waiting for the answer, so it was not told");
    return parts.filter((part) => part !== "");
  }
  /** Why a failed call did not happen, as far as its record says: Kin took too long, or Kin could not do it. */
  function whyFailed(call) {
    const ms = call?.duration_ms;
    return typeof ms === "number" && Number.isFinite(ms) && ms >= WAITED_MS ? `; Kin did not answer within ${count(Math.round(ms / 1000))} s` : "; Kin could not do it";
  }
  const plainText = (parts) => parts.map((part) => typeof part === "string" ? part : shortLabel(part.label)).join("");

  /** The change a commit call names, when its record carries one and it is one of the agent's own; null otherwise. */
  function committedChange(call, agent) {
    // A mutate commits in one call, as a commit does.
    if (!["commit", "mutate"].includes(family(call?.tool)) || call?.is_error) return null;
    const id = [call.change_id, call.change].map(text).find((value) => /^[\w-]{1,128}$/.test(value));
    if (!id) return null;
    return array(agent?.changes).find((change) => text(change?.id).toLowerCase() === id.toLowerCase()) || null;
  }

  /** Calls in a row that did the same thing to the same entity, read as one. */
  function group(calls) {
    const groups = [];
    for (const call of array(calls)) {
      if (!call || typeof call !== "object") continue;
      const last = groups.at(-1);
      const same = last && family(last.call.tool) === family(call.tool) && text(last.call.entity_ids?.[0]) === text(call.entity_ids?.[0]) && Boolean(last.call.is_error) === Boolean(call.is_error)
        && text(last.call.change_id) === text(call.change_id);
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

  /** The window Kin keeps agents that have gone for, as words: "in the last 30 days". Nothing when Kin did not say. */
  function keptWords(data) {
    const days = data?.kept?.days;
    return isCount(days) && days > 0 ? `in the last ${plural(days, "day")}` : "";
  }
  /** What Kin keeps of an agent once it has gone, in a sentence. Nothing when Kin did not say. */
  function keptSentence(data) {
    const days = data?.kept?.days, sessions = data?.kept?.sessions;
    if (!isCount(days) || days <= 0) return "";
    return `Kin keeps what each agent did here for ${plural(days, "day")} after it leaves${isCount(sessions) && sessions > 0 ? `, the latest ${count(sessions)} at most` : ""}.`;
  }

  /**
   * Sessions as the agents that ran them: one agent that connected three
   * times is one agent in three sessions. Agents are told apart by name.
   */
  function whoRan(sessions) {
    const byName = new Map();
    for (const agent of array(sessions)) {
      const name = text(agent?.name).trim();
      if (!byName.has(name.toLowerCase())) byName.set(name.toLowerCase(), name);
    }
    return { agents: byName.size, sessions: array(sessions).length, name: byName.size === 1 ? [...byName.values()][0] : "" };
  }

  /** Who worked here before, as agents and the sessions they ran: "Claude Code worked here in three sessions in the last 30 days." */
  function earlierSentence(data) {
    const earlier = array(data?.earlier);
    if (!earlier.length) return "";
    const { agents, sessions, name } = whoRan(earlier);
    // A full list is the most the server sends, so its counts are at least these.
    const atLeast = earlier.length >= EARLIER_SHOWN;
    const who = agents === 1 ? name || "One agent" : `${atLeast ? `At least ${spokenInside(agents)}` : spoken(agents)} agents`;
    const runs = sessions > agents || atLeast ? ` in ${atLeast ? "at least " : ""}${spokenInside(sessions)} sessions` : "";
    return `${who} worked here${runs} ${keptWords(data) || "earlier"}.`;
  }

  /** The Agents lede: who is connected now, in a sentence of real counts of agents, not of the sessions they run. */
  function headline(data) {
    const agents = array(data?.agents);
    const working = agents.filter((agent) => agent.state === "active").length;
    if (!agents.length) return array(data?.earlier).length ? `No agent is connected right now. ${earlierSentence(data)}` : "No agent is connected right now.";
    if (agents.length === 1) return working ? "One agent is connected and working." : "One agent is connected. It is idle.";
    const { agents: who, name } = whoRan(agents);
    const connected = who === 1 ? `${name || "One agent"} is connected in ${spokenInside(agents.length)} sessions.`
      : who < agents.length ? `${spoken(who)} agents are connected in ${spokenInside(agents.length)} sessions.`
        : `${spoken(agents.length)} agents are connected.`;
    if (!working) return `${connected} None is working right now.`;
    if (working === agents.length) return `${connected} ${agents.length === 2 ? "Both are" : "Each one is"} working.`;
    return `${connected} ${spoken(working)} ${working === 1 ? "is" : "are"} working now.`;
  }

  // ── What a session came to ───────────────────────────────────────────────

  /** A change it opened and has not committed, in a few words. */
  const openWords = (edits) => edits ? `Has ${plural(edits, "edit")} staged, not committed yet` : "Started a change, nothing staged yet";
  // How a committed change's review stands, said of it alone, or of the latest of several.
  const REVIEW_WORDS = {
    pending: [", waiting for your review", ", the latest waiting for your review"],
    approved: [", and you marked it as looking right", ", and you marked the latest as looking right"],
    needs_work: [", and you flagged it", ", and you flagged the latest"],
  };

  /**
   * What a session came to, and where that came from: a change it committed,
   * then a change it has open, then the entities it edited, then its last
   * call. Null when Kin holds nothing it did.
   */
  function settle(agent, names, review) {
    if (!agent || typeof agent !== "object") return null;
    const known = names && typeof names === "object" ? names : {};
    const changes = array(agent.changes).filter((change) => text(change?.id));
    const committed = Math.max(changes.length, isCount(agent.changes_total) ? agent.changes_total : 0);
    if (committed > 0) {
      const latest = changes[0] || null;
      const title = text(latest?.title).trim();
      const others = committed - 1;
      const parts = title ? [`Committed “${title}”`, others ? ` and ${plural(others, "other")}` : ""] : [others ? `Committed ${plural(committed, "change")}` : "Committed a change"];
      let state = null;
      if (latest && typeof review === "function") { try { state = review(latest.id); } catch (_) { state = null; } }
      const said = REVIEW_WORDS[state === "needswork" ? "needs_work" : state];
      if (said) parts.push(said[others ? 1 : 0]);
      return { kind: "committed", change: latest, parts: parts.filter(Boolean) };
    }
    const open = array(agent.open_changes).filter((row) => row && typeof row === "object");
    if (open.length) {
      const edits = open.reduce((sum, row) => sum + (isCount(row.staged_edits) ? row.staged_edits : 0), 0);
      return { kind: "open", parts: [openWords(edits)] };
    }
    const edited = array(agent.edited).filter((id) => UUID.test(text(id)));
    const editedTotal = Math.max(edited.length, isCount(agent.edited_total) ? agent.edited_total : 0);
    if (editedTotal > 0) {
      const named = [...new Set(edited.filter((id) => text(known[id]?.label)))];
      const entity = (id) => ({ id, label: known[id].label });
      if (!named.length) return { kind: "edited", parts: [`Edited ${plural(editedTotal, "entity", "entities")}`] };
      if (editedTotal === 2 && named.length === 2) return { kind: "edited", parts: ["Edited ", entity(named[0]), " and ", entity(named[1])] };
      return { kind: "edited", parts: ["Edited ", entity(named[0]), editedTotal > 1 ? ` and ${plural(editedTotal - 1, "other entity", "other entities")}` : ""].filter(Boolean) };
    }
    const last = array(agent.recent_calls).find((call) => call && typeof call === "object");
    return last ? { kind: "call", call: last, parts: describe(last, known) } : null;
  }

  /**
   * What a session came to, in the parts describe() returns: strings, and
   * { id, label } entities. `review(changeId)` says how a committed change's
   * review stands: "pending", "approved", "needs_work", or null when that is
   * not known; without it a commit is said without its review. Null when Kin
   * holds nothing the session did.
   */
  function outcome(agent, names, review) {
    return settle(agent, names, review)?.parts || null;
  }

  /**
   * Whether a change of an agent's is open now, as far as Kin can say; null
   * when its record does not say. A session that has ended no longer reports
   * what it holds.
   */
  /**
   * The change an agent committed after it last started one and before
   * `call`, when its record says so: a drop that fails after the change it
   * started was committed had nothing left to drop.
   */
  function committedBefore(call, calls, agent) {
    const at = Date.parse(text(call?.at));
    if (!Number.isFinite(at)) return null;
    const begun = Math.max(-Infinity, ...array(calls).filter((other) => family(other?.tool) === "begin" && !other.is_error).map((other) => Date.parse(text(other.at))).filter((when) => Number.isFinite(when) && when <= at));
    if (!Number.isFinite(begun)) return null;
    return array(agent?.changes).find((change) => { const when = Date.parse(text(change?.time)); return Number.isFinite(when) && when >= begun && when <= at; }) || null;
  }
  function openNow(agent) {
    if (agent?.state === "ended") return "Kin cannot say now whether that change is still open.";
    if (!["active", "idle"].includes(agent?.state) || !Array.isArray(agent.open_changes)) return null;
    const open = agent.open_changes.filter((row) => row && typeof row === "object");
    if (!open.length) return "No change of its is open now.";
    const counted = open.every((row) => isCount(row.staged_edits));
    const edits = open.reduce((sum, row) => sum + (isCount(row.staged_edits) ? row.staged_edits : 0), 0);
    const staged = counted ? `, with ${edits ? `${plural(edits, "edit")} staged` : "nothing staged"}` : "";
    return open.length === 1 ? `Its change is still open${staged}.` : `It has ${spokenInside(open.length)} changes open${staged}.`;
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
    return parts.map((part) => typeof part === "string" ? part : ctx.h("a", { class: "agent-entity", href: ctx.entityHref(part.id), title: part.label }, shortLabel(part.label)));
  }

  /** What an agent has open: a change it is staging, and the work it said it is doing. */
  function openWork(ctx, agent, names) {
    const rows = [];
    for (const open of array(agent.open_changes).slice(0, 3)) {
      const edits = isCount(open.staged_edits) ? open.staged_edits : 0;
      const age = isCount(open.age_secs) ? span(Date.now() - open.age_secs * 1000) : null;
      // Said in the words a session's outcome uses, so Home and this page agree.
      rows.push(ctx.h("p", { class: "agent-open" }, ctx.h("span", { class: "agent-open-dot", "aria-hidden": "true" }),
        openWords(edits),
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

  function connectHint(ctx) {
    return ctx.h("p", { class: "agent-hint" }, "Coding agents such as Claude Code, Codex and Cursor work here through Kin once they are connected. ", ctx.h("a", { href: "/#/agents" }, "Connect an agent"), ".");
  }

  // ── The Agents page ──────────────────────────────────────────────────────

  const timeline = { key: null, data: null, error: null, busy: false, at: 0, timer: null, slow: false, tries: 0, retry: null };
  // A busy machine can take minutes to read an agent's calls, so the read waits for them.
  const TIMELINE_PATIENCE = 180_000;
  // A read that failed only because Kin took too long is asked again on its own, after these waits.
  const TIMELINE_RETRY = [3_000, 8_000, 20_000];
  const TOO_SLOW = /did not answer within|took longer than|timed out|timeout/i;

  /** Stop waiting to read an agent's calls again. */
  function stopRetry() {
    clearTimeout(timeline.retry);
    timeline.retry = null;
  }

  /**
   * Read what an agent did. The last answer stays on the page while it is
   * read again. A read that only took too long is asked again on its own a
   * few times, further apart each time, before the page says it failed.
   */
  function loadTimeline(ctx, key, quiet = false) {
    if (!KEY.test(text(key))) return;
    if (timeline.key !== key) { stopRetry(); Object.assign(timeline, { key, data: null, error: null, slow: false, tries: 0 }); }
    if (!quiet) timeline.tries = 0;
    if (timeline.busy) { timeline.again = true; return; }
    stopRetry();
    timeline.busy = true;
    if (!quiet) ctx.paint("agents-detail");
    ctx.fetchJSON(`/api/workspace/agents/timeline?agent=${encodeURIComponent(key)}&limit=100`, { patience: TIMELINE_PATIENCE })
      .then((data) => { if (timeline.key === key) Object.assign(timeline, { data, error: null, slow: false, tries: 0, at: Date.now() }); })
      .catch((error) => {
        if (timeline.key !== key) return;
        const message = text(error?.message) || "unreadable";
        if (error?.status !== 404 && TOO_SLOW.test(message) && timeline.tries < TIMELINE_RETRY.length) {
          const wait = TIMELINE_RETRY[timeline.tries];
          timeline.tries += 1;
          timeline.slow = true;
          timeline.error = null;
          timeline.retry = setTimeout(() => {
            timeline.retry = null;
            // Nobody is looking at this agent any more: the read waits until someone is.
            if (timeline.key !== key || (ctx.shown && !ctx.shown("agents-detail"))) { timeline.slow = false; return; }
            loadTimeline(ctx, key, true);
          }, wait);
          return;
        }
        timeline.slow = false;
        timeline.error = { message, status: error?.status };
      })
      .finally(() => {
        timeline.busy = false;
        ctx.paint("agents-detail");
        if (timeline.again) { timeline.again = false; loadTimeline(ctx, timeline.key, true); }
      });
  }

  function everyAgent(data) { return [...array(data?.agents), ...array(data?.earlier)]; }
  /** When a session started, as a time of day, so two sessions of one agent can be told apart; with its day when that was not today. */
  function started(agent) {
    const date = new Date(agent?.started_at);
    if (Number.isNaN(date.getTime())) return "";
    const today = date.toDateString() === new Date().toDateString();
    return date.toLocaleString(undefined, today ? { hour: "numeric", minute: "2-digit" } : { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
  }

  function selectedKey(ctx, data) {
    const wanted = text(ctx.params().agent);
    if (KEY.test(wanted)) return wanted;
    return everyAgent(data)[0]?.key || null;
  }

  /** The entities an agent has been looking at lately, each once, newest first. */
  function touching(agent, names) {
    const seen = new Map();
    for (const call of array(agent.recent_calls).slice(0, 4)) {
      // A write names the entity it was made to; the others its answer lists only moved with it.
      for (const id of array(call?.entity_ids).slice(0, WRITES.has(family(call?.tool)) ? 1 : 6)) {
        const label = shortLabel(names[id]?.label);
        // Two entities that read the same here are one chip; the first leads to its page.
        if (label && !seen.has(label.toLowerCase())) seen.set(label.toLowerCase(), { id, label });
      }
      if (seen.size >= 8) break;
    }
    return [...seen.values()].slice(0, 8);
  }
  /** A name as a reader would say it: a local inside a function is named for itself. */
  function shortLabel(label) { return text(label).replace(/^.*<locals>\./, ""); }

  /** Entities by name, each once, the first of two that read the same leading to its page; with how many more there are. */
  function namedChips(ids, names, total, shown = 6) {
    const seen = new Map();
    for (const id of array(ids)) {
      const label = shortLabel(names[id]?.label);
      if (label && !seen.has(label.toLowerCase())) seen.set(label.toLowerCase(), { id, label });
    }
    const chips = [...seen.values()].slice(0, shown);
    const all = Math.max(isCount(total) ? total : 0, array(ids).length);
    return { chips, more: Math.max(0, all - chips.length) };
  }

  /** What an agent that has gone did: what it read and what it changed. What it committed is its lane's outcome line. */
  function pastRows(ctx, agent, names) {
    const row = (label, ...content) => ctx.h("div", { class: "lane-row" }, ctx.h("span", { class: "lane-label" }, label), ctx.h("span", { class: "lane-chips" }, ...content));
    const chipsOf = ({ chips, more }) => [chips.map((held) => ctx.h("a", { class: "lane-chip", href: ctx.entityHref(held.id), title: names[held.id]?.label }, held.label)),
      more ? ctx.h("span", { class: "agent-quiet lane-more" }, chips.length ? `and ${count(more)} more` : plural(more, "entity", "entities")) : null];
    const rows = [];
    if (agent.reads_known === false) rows.push(ctx.h("p", { class: "agent-quiet" }, "Kin kept what it wrote, not what it read."));
    else {
      const read = Array.isArray(agent.read) ? namedChips(agent.read, names, agent.read_total) : { chips: touching(agent, names), more: 0 };
      if (read.chips.length || read.more) rows.push(row("Read", ...chipsOf(read)));
    }
    const edited = namedChips(agent.edited, names, agent.edited_total);
    if (edited.chips.length || edited.more) rows.push(row("Edited", ...chipsOf(edited)));
    return rows;
  }

  /**
   * Whether a closed lane gives its outcome a line of its own. A committed
   * change always does. Otherwise a row the lane already shows says the same:
   * a connected agent's open change and its last call, and a gone agent's
   * edits and what it read.
   */
  function ownsLine(settled, here) {
    if (!settled) return false;
    if (settled.kind === "committed") return true;
    if (settled.kind === "edited") return here;
    if (settled.kind === "call") return !here && (Boolean(settled.call.is_error) || !READS.has(family(settled.call.tool)));
    return false;
  }

  /** A session's outcome as a line: what it came to, and for a commit the way to its review. */
  function outcomeLine(ctx, settled) {
    // Drawn as a lane's last-call row is, its entities in the sentence's own type.
    return ctx.h("p", { class: "lane-outcome lane-last" }, sentence(ctx, settled.parts),
      settled.kind === "committed" && settled.change ? [" · ", ctx.h("a", { class: "more-link", href: ctx.changeURL(settled.change.id) }, "Review")] : null);
  }

  /**
   * One lane to an agent. A connected agent's lane shows what it has open, the
   * entities it is looking at and the last things it did; the open lane holds
   * everything it did. An agent that has gone is a line until it is opened.
   */
  function lanes(ctx, data, selected) {
    const rows = everyAgent(data);
    const unread = data?.earlier_unread ? ctx.h("p", { class: "gap" }, "Kin couldn't read which agents worked here before. It reads again on its own.") : null;
    if (!rows.length) {
      const within = keptWords(data);
      return ctx.h("div", { class: "agent-empty agent-empty-page" },
        ctx.h("div", {}, unread || ctx.h("p", { class: "agent-empty-title" }, within ? `No agent has worked here ${within}.` : "No agent has worked here that Kin has a record of."),
          keptSentence(data) ? ctx.h("p", { class: "agent-hint" }, keptSentence(data)) : null, connectHint(ctx)));
    }
    const names = data.names || {};
    const lane = (agent) => {
      const [word, tone] = stateWords(agent);
      const open = agent.key === selected;
      const here = agent.state !== "ended";
      const calls = array(agent.recent_calls);
      const looking = here ? touching(agent, names) : [];
      const last = calls[0];
      const when = here ? (started(agent) ? `since ${started(agent)}` : "") : started(agent);
      const tally = here || agent.reads_known !== false ? plural(agent.calls_total || 0, "call") : "";
      // A closed lane says first what its session came to; an open one says it at the head of what it did.
      const settled = open ? null : settle(agent, names, ctx.reviewState);
      const lead = ownsLine(settled, here) ? outcomeLine(ctx, settled) : null;
      return ctx.h("li", { class: `lane${open ? " is-open" : ""}`, "data-state": agent.state === "active" ? "working" : here ? "idle" : "ended" },
        ctx.h("a", { class: "lane-head", href: `#/agents/${agent.key}`, "aria-current": open ? "true" : undefined, "aria-expanded": String(open) },
          ctx.h("span", { class: "pulse", "aria-hidden": "true" }),
          ctx.h("span", { class: "lane-name" }, agent.name),
          ctx.h("span", { class: "lane-when" }, [when, tally, agent.changes_total ? `${plural(agent.changes_total, "change")} committed` : "", !here ? ctx.ago(last?.at || agent.last_activity_at) : ""].filter(Boolean).join(" · ")),
          here ? ctx.h("span", { class: `state${tone === "live" ? " state-good" : ""}` }, word) : null),
        ctx.h("div", { class: "lane-body" },
          lead,
          here ? [
            openWork(ctx, agent, names) || (calls.length ? ctx.h("p", { class: "agent-quiet" }, "It has not told Kin what it is working on.") : null),
            looking.length ? ctx.h("div", { class: "lane-row" }, ctx.h("span", { class: "lane-label" }, "Looking at"),
              ctx.h("span", { class: "lane-chips" }, looking.map((held) => ctx.h("a", { class: "lane-chip", href: ctx.entityHref(held.id), title: names[held.id]?.label }, held.label)))) : null,
            !open && last ? ctx.h("div", { class: "lane-row" }, ctx.h("span", { class: "lane-label" }, "Last"),
              ctx.h("span", { class: "lane-last" }, sentence(ctx, describe(last, names)), ctx.h("span", { class: "agent-quiet" }, ` · ${ctx.ago(last.at)}`))) : null,
            !open && !last && !lead ? ctx.h("p", { class: "agent-quiet" }, "Connected. It has not asked Kin for anything yet.") : null]
            : pastRows(ctx, agent, names),
          open ? ctx.section("agents-detail", ["standing"], () => detail(ctx, agent.key), { class: "lane-detail" }) : null));
    };
    const live = array(data.agents);
    const earlier = array(data.earlier).slice(0, 12);
    return [
      live.length ? ctx.h("ul", { class: "lanes", "aria-label": "Connected now" }, live.map(lane)) : null,
      earlier.length || unread ? ctx.h("section", { class: live.length ? "section" : "" },
        live.length ? ctx.h("div", { class: "section-head" }, ctx.h("h2", {}, "Earlier"), keptWords(data) ? ctx.h("span", { class: "agent-quiet" }, keptWords(data).replace(/^in t/, "T")) : null) : null,
        unread,
        earlier.length ? ctx.h("ul", { class: "lanes lanes-earlier", "aria-label": "Earlier sessions" }, earlier.map(lane)) : null) : null,
      ctx.h("p", { class: "agent-quiet agent-privacy" }, [keptSentence(data), "Kin records which of its tools an agent called and which entities came back. It does not keep the agent's prompts."].filter(Boolean).join(" "))];
  }

  function timelineRows(ctx, calls, names, agent) {
    const groups = group(calls);
    const SHOWN = 60;
    if (!groups.length) return ctx.h("p", { class: "gap" }, "It has not asked Kin anything yet.");
    // Under the newest failed attempt to open, fill, commit or drop a change: whether a change is open now, when Kin can say.
    const failedChange = groups.find((entry) => entry.call.is_error && CHANGING.has(family(entry.call.tool)));
    // A failed drop after the change it started was committed is said as that, which also says nothing is open from it.
    const before = failedChange && family(failedChange.call.tool) === "abort" ? committedBefore(failedChange.call, calls, agent) : null;
    const openLine = before ? `It had already committed the change it started${text(before.title) ? `, “${text(before.title)}”,` : ""} at ${stamp(before.time)}.` : failedChange ? openNow(agent) : null;
    return ctx.h("ol", { class: "agent-timeline" }, groups.slice(0, SHOWN).map((entry) => {
      const call = entry.call;
      const kind = family(call.tool);
      const change = committedChange(call, agent);
      // A write names the entity it was made to; the others its answer lists only moved with it.
      const listed = WRITES.has(kind) ? array(call.entity_ids).slice(0, 1) : array(call.entity_ids);
      const ids = listed.filter((id) => text(names[id]?.label));
      const unnamed = listed.length - ids.length;
      const meta = [];
      if (entry.times > 1) meta.push(`${count(entry.times)} times`);
      if (READS.has(kind) && kind !== "source" && isCount(call.result_count)) meta.push(call.result_count ? `${plural(call.result_count, "entity", "entities")} returned` : "No entity returned");
      return ctx.h("li", { class: `agent-step agent-step-${kind}${call.is_error ? " agent-step-error" : ""}` },
        ctx.h("time", { class: "agent-step-time", datetime: text(call.at) || undefined }, stamp(call.at)),
        ctx.h("span", { class: "agent-step-dot", "aria-hidden": "true" }),
        ctx.h("div", { class: "agent-step-body" },
          ctx.h("p", { class: "agent-step-text" }, sentence(ctx, describe(call, names))),
          ids.length > 1 || unnamed > 0 ? ctx.h("div", { class: "agent-step-chips" }, ids.slice(1, 6).map((id) => ctx.h("a", { class: "chip chip-small", href: ctx.entityHref(id) }, names[id].label)),
            ids.length > 6 || unnamed > 0 ? ctx.h("span", { class: "chip chip-small chip-more" }, `${count(Math.max(0, ids.length - 6) + unnamed)} more`) : null) : null,
          meta.length ? ctx.h("p", { class: "agent-quiet agent-step-meta" }, meta.join(" · ")) : null,
          entry === failedChange && openLine ? ctx.h("p", { class: "agent-quiet agent-step-meta agent-step-open" }, openLine) : null,
          change ? ctx.h("p", { class: "agent-step-meta" }, ctx.h("a", { class: "more-link", href: ctx.changeURL(change.id) }, text(change.title) ? `Review “${text(change.title)}”` : "Review this change")) : null));
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
      // A change older than the history Kin read is shown by its review alone.
      ctx.h("div", {}, ctx.h("span", { class: "change-title" }, "title" in change ? text(change.title) || "Untitled change" : "A change it committed"),
        ctx.h("span", { class: "change-meta" }, changedWords(ctx, change), change.time ? ctx.h("time", { datetime: change.time }, ctx.ago(change.time)) : null)),
      ctx.h("span", { class: "change-side" }, ctx.h("span", { class: "more-link" }, "Review"))))));
  }

  function detail(ctx, selected) {
    if (!selected) return null;
    // A read that was left when nobody was looking is made now.
    const unread = timeline.key === selected && !timeline.data && !timeline.error && !timeline.slow && !timeline.retry;
    if ((timeline.key !== selected || unread) && !timeline.busy) loadTimeline(ctx, selected, true);
    const data = timeline.key === selected ? timeline.data : null;
    const stillReading = timeline.key === selected && timeline.slow ? ctx.h("p", { class: "agent-quiet", role: "status" }, "Kin is still reading what this agent did.") : null;
    if (!data) {
      if (timeline.key === selected && timeline.error) {
        if (timeline.error.status === 404) return ctx.h("div", { class: "section" }, ctx.h("p", { class: "gap" }, "Kin no longer holds this agent's record."));
        return ctx.h("div", { class: "section" }, ctx.failed("Kin couldn't load what this agent did.", () => loadTimeline(ctx, selected)));
      }
      return ctx.h("div", { class: "agent-detail", "aria-busy": "true" }, stillReading, ctx.skeleton("skeleton-lede"), ctx.skeletonLines(6));
    }
    const agent = data.agent && typeof data.agent === "object" ? data.agent : {};
    const names = data.names && typeof data.names === "object" ? data.names : {};
    const calls = array(data.calls).filter((call) => call && typeof call === "object");
    // What the session came to leads, so the summary and the lane's head agree.
    const settled = settle(array(agent.recent_calls).length ? agent : { ...agent, recent_calls: calls }, names, ctx.reviewState);
    const total = isCount(agent.calls_total) ? agent.calls_total : calls.length;
    const kept = isCount(data.kept) ? data.kept : calls.length;
    const reads = calls.filter((call) => READS.has(family(call.tool)));
    const touched = new Set(reads.flatMap((call) => array(call.entity_ids)));
    const readsKnown = agent.reads_known !== false;
    const summary = [
      readsKnown ? `${spoken(total)} ${total === 1 ? "call" : "calls"} to Kin` : null,
      readsKnown && touched.size ? `${plural(touched.size, "entity", "entities")} looked at${kept < total ? " in the latest " + count(kept) : ""}` : null,
      !readsKnown && isCount(agent.edited_total) && agent.edited_total ? `${plural(agent.edited_total, "entity", "entities")} written` : null,
      agent.changes_total ? plural(agent.changes_total, "change") + " committed" : "no change committed",
    ].filter(Boolean).join(", ");
    return ctx.h("div", { class: "agent-detail" },
      ctx.h("p", { class: "agent-summary" }, settled ? [ctx.h("span", { class: "agent-outcome" }, sentence(ctx, settled.parts), "."), " "] : null,
        `${summary.charAt(0).toUpperCase()}${summary.slice(1)}. `,
        ctx.h("span", { class: "agent-quiet" }, [presence(agent), agent.through === "mcp" ? "through Kin’s MCP server" : null].filter(Boolean).join(" · "))),
      timeline.slow ? stillReading : null,
      array(agent.changes).length ? ctx.h("section", { class: "section", "aria-labelledby": "agent-changes-title" }, ctx.h("div", { class: "section-head" }, ctx.h("h2", { id: "agent-changes-title" }, agent.changes_total === 1 ? "The change it committed" : "Changes it committed")), changeRows(ctx, array(agent.changes))) : null,
      ctx.h("section", { class: "section", "aria-labelledby": "agent-steps-title" },
        ctx.h("div", { class: "section-head" }, ctx.h("h2", { id: "agent-steps-title" }, "What it did"), readsKnown ? ctx.h("span", { class: "agent-quiet" }, "Newest first") : null),
        readsKnown ? timelineRows(ctx, calls, names, agent) : ctx.h("p", { class: "gap" }, "Kin kept the changes this agent wrote, not the calls it made."),
        kept < total ? ctx.h("p", { class: "agent-quiet agent-foot" }, `Showing its latest ${count(kept)} of ${count(total)} calls. Kin keeps a bounded record of each agent's calls.`) : null,
        timeline.error && !timeline.slow ? ctx.h("p", { class: "agent-quiet" }, "Showing what Kin last saw. ", ctx.button("Refresh", () => loadTimeline(ctx, selected), { class: "button button-small" })) : null));
  }

  function view(ctx) {
    return [
      ctx.h("header", { class: "page-head" },
        ctx.h("div", { class: "page-title-row" }, ctx.h("h1", {}, "Agents"), ctx.h("div", { class: "page-actions" }, ctx.h("a", { class: "button", href: "/#/agents" }, "Connect an agent"))),
        ctx.section("agents-lede", ["agents"], () => {
          const resource = ctx.read();
          if (resource.missing) return ctx.h("p", { class: "lede" }, "This version of Kin does not report agent activity.");
          if (!resource.data) return resource.error ? null : ctx.skeleton("skeleton-lede");
          return ctx.h("p", { class: "lede" }, headline(resource.data), " ", liveBadge(ctx));
        })),
      ctx.section("agents-meets", ["agents", "working"], () => ctx.overlaps?.() || null),
      // Review decisions are read too: a lane says whether its committed change waits for review.
      ctx.section("agents-list", ["agents", "standing"], () => {
        const resource = ctx.read();
        if (resource.missing) return null;
        if (!resource.data) return resource.error ? ctx.failed("Kin couldn't load which agents are connected.", () => ctx.refresh(["agents"])) : ctx.skeletonLines(3);
        return lanes(ctx, resource.data, selectedKey(ctx, resource.data));
      }, { class: "agents-lanes" })];
  }

  // ── Following Kin live ───────────────────────────────────────────────────

  const live = { source: null, state: "off", retry: 0, timer: null, poll: null, ctx: null };

  function liveBadge(ctx) {
    if (live.state === "off") return null;
    const words = { on: "Updates live", waiting: "Reconnecting", polling: "Checking every 15 seconds" }[live.state] || "";
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
    return () => { live.source?.close(); clearTimeout(live.timer); clearTimeout(timeline.timer); stopRetry(); stopPolling(); live.state = "off"; };
  }
  function startPolling(ctx) { if (!live.poll) live.poll = setInterval(() => { if (!document.hidden) ctx.pulse(["agents"]); }, 15_000); }
  function stopPolling() { if (live.poll) { clearInterval(live.poll); live.poll = null; } }

  // ── Who made a change ────────────────────────────────────────────────────

  /** A change's author: an agent with its mark, or a person by name. */
  function author(ctx, change, name) {
    const agent = change?.agent;
    if (agent && text(agent.name)) return ctx.h("span", { class: "author-agent", title: "Made by an agent through Kin" }, mark(12), agent.name);
    return name ? ctx.h("span", {}, name) : null;
  }

  globalThis.KinWorkspaceAgents = { describe, outcome, openNow, plainText, shortLabel, group, family, span, presence, headline, earlierSentence, keptWords, view, follow, author, mark, timeline, live };
})();
