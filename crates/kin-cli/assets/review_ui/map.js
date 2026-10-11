// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The change map: what a change touched and what relies on it, drawn as a
// family tree. What others rely on sits above what relies on it, so the tree
// reads top down the way the steps read first to last.
//
// It is a tree on purpose. Every entity is drawn once, under the first thing
// it relies on, and anything else it relies on is said on its own label. A
// graph with every edge drawn would cross itself on the first change that has
// two callers in common, and a reader would have to untangle it before
// learning anything. The lists below are real lists, so the connectors are
// borders and a screen reader hears the same hierarchy the picture shows.
//
// Only what changed gets a box of its own. What relies on a changed entity
// from outside the change is listed in one card under it, a row each, so a
// function with eight callers stays one column wide and the map stays a
// picture of the change rather than of everything around it.

"use strict";

(() => {
  const root = (globalThis.kinReview = globalThis.kinReview || {});

  /** How many unchanged consumers and tests one changed entity shows before the rest are counted. */
  const CONSUMERS_SHOWN = 5;
  const TESTS_SHOWN = 3;

  function el(tag, className, ...children) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    for (const child of children.flat()) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  const plural = (count, one, many) => (count === 1 ? `1 ${one}` : `${count} ${many}`);

  /**
   * An entity's name, allowed to wrap after a `.`, `:` or `,` and inside a
   * snake_case word, so a long qualified name breaks where a reader would
   * split it instead of being cut short. A leading or doubled underscore,
   * as in `__init__`, and a `::` stay whole.
   */
  function nameNodes(name) {
    const text = String(name);
    const word = /[A-Za-z0-9]/;
    const nodes = [];
    let start = 0;
    for (let at = 0; at < text.length - 1; at += 1) {
      const here = text[at];
      const next = text[at + 1];
      const breaks =
        here === "." || here === "," || (here === ":" && next !== ":") || (here === "_" && word.test(text[at - 1] || "") && word.test(next));
      if (!breaks) continue;
      nodes.push(text.slice(start, at + 1), document.createElement("wbr"));
      start = at + 1;
    }
    nodes.push(text.slice(start));
    return nodes;
  }

  /**
   * A name and, when another row of the same list carries the same name, the
   * parameters that tell the two apart.
   */
  function labelNodes(name, parameters) {
    return [...nameNodes(name), parameters ? el("span", "params", nameNodes(parameters)) : null];
  }

  /**
   * The tree a guide draws. Changed entities are placed under the first
   * changed entity they rely on, so a function sits above the callers that
   * changed with it. Under each go the unchanged consumers and tests that
   * reach it, each shown once across the whole map.
   */
  function buildTree(guide) {
    const steps = guide.chapters.flatMap((chapter) => chapter.steps);
    const order = new Map(steps.map((step, index) => [step.entity_id, index]));
    const nodes = new Map(
      steps.map((step, index) => [
        step.entity_id,
        {
          kind: "changed",
          id: step.entity_id,
          index,
          name: step.name,
          parameters: step.parameters || null,
          change: step.change,
          risk: step.risk,
          // A signature that moved while a caller stood still is the case a
          // reviewer most needs pointed at.
          breaking: step.findings.some((finding) => finding.severity === "error"),
          children: [],
          also: [],
        },
      ]),
    );

    // A changed entity relies on the step that lists it as a consumer. The
    // steps are already ordered dependency first, so taking only parents that
    // come earlier keeps the tree free of cycles.
    const parent = new Map();
    for (const step of steps) {
      const reliers = [...step.connections.consumers, ...step.connections.tests].filter((link) => link.changed_here);
      for (const link of reliers) {
        const child = link.entity.id;
        if (!nodes.has(child) || order.get(child) <= order.get(step.entity_id)) continue;
        if (parent.has(child)) nodes.get(child).also.push(step.name);
        else parent.set(child, step.entity_id);
      }
    }
    for (const [child, above] of parent) nodes.get(above).children.push(nodes.get(child));
    for (const node of nodes.values()) node.children.sort((a, b) => a.index - b.index);

    const placed = new Map();
    for (const step of steps) {
      const node = nodes.get(step.entity_id);
      node.reliers = [];
      node.more = 0;
      const outside = (links, kind, shown, total) => {
        const unchanged = links.filter((link) => !link.changed_here);
        const fresh = [];
        // Entities of one name in one container, such as the overload
        // declarations of one callable, read as one row that counts them.
        // Each keeps its own place, so a later step finds the row it is in.
        // A member's name carries its owner, so a qualified name is its own
        // place; a bare name is placed by what contains it.
        const named = new Map();
        const placeOf = (entity) => (/\.|::/.test(entity.name) ? "" : entity.within || "");
        for (const link of unchanged) {
          const seen = placed.get(link.entity.id);
          if (seen) {
            if (!seen.also.includes(step.name)) seen.also.push(step.name);
            // Drawn once, under the first step it reaches. If a later step
            // it reaches broke, it is not updated whichever box it sits in.
            if (node.breaking && kind === "consumer") seen.unsettled = true;
            continue;
          }
          const key = [link.entity.kind, placeOf(link.entity), link.entity.name].join("\u0000");
          const same = named.get(key);
          if (same) {
            same.alike += 1 + (link.overloads || 0);
            same.parameters = null;
            placed.set(link.entity.id, same);
            continue;
          }
          const leaf = {
            kind,
            id: link.entity.id,
            parent: step.entity_id,
            name: link.entity.name,
            parameters: link.entity.parameters || null,
            overloads: link.overloads || 0,
            alike: 0,
            unsettled: node.breaking && kind === "consumer",
            // What the step's change asks of this place, when the guide says.
            verdict: kind === "consumer" ? step.consumer_verdict?.verdict || null : null,
            also: [],
          };
          named.set(key, leaf);
          placed.set(link.entity.id, leaf);
          fresh.push(leaf);
        }
        node.reliers.push(...fresh.slice(0, shown));
        // What the step did not list at all, and what it listed past the cap.
        const unlisted = Math.max(0, total - links.length);
        node.more += unlisted + Math.max(0, fresh.length - shown);
      };
      outside(step.connections.consumers, "consumer", CONSUMERS_SHOWN, step.connections.consumers_total);
      outside(step.connections.tests, "test", TESTS_SHOWN, step.connections.tests_total);
    }

    return steps.filter((step) => !parent.has(step.entity_id)).map((step) => nodes.get(step.entity_id));
  }

  const CHANGE_WORD = { added: "Added", modified: "Modified", removed: "Removed" };

  const VERDICT_CAPTION = { needs_no_update: "No update needed", may_need_review: "Check this", not_updated: "Not updated" };

  function leafCaption(leaf) {
    if (leaf.kind === "test") return "Test";
    if (leaf.unsettled) return "Not updated";
    return VERDICT_CAPTION[leaf.verdict] || "Unchanged";
  }

  /** What a guide calls the thing it walks. */
  const nounOf = (guide) => (guide.kind === "working" ? "work" : guide.kind === "range" ? "comparison" : "change");

  function changedButton(node, handlers) {
    const classes = ["node", "node-changed", `node-${node.change}`];
    const word = CHANGE_WORD[node.change] || node.change;
    const button = el(
      "button",
      classes.join(" "),
      el("span", "node-name", labelNodes(node.name, node.parameters)),
      el("span", "node-caption", word),
      node.also.length ? el("span", "node-also", `also relies on ${node.also.join(", ")}`) : null,
    );
    button.type = "button";
    button.setAttribute("aria-label", `${node.name}, ${word.toLowerCase()} here. Go to its step.`);
    button.addEventListener("click", () => handlers.open(node));
    return button;
  }

  /** What relies on one changed entity from outside the change: a row each, in one card. */
  function reliersCard(node, handlers) {
    const rows = node.reliers.map((leaf) => {
      const declarations = 1 + leaf.overloads + leaf.alike;
      // Overloads of one callable read as one row, without their signatures.
      const folded = declarations > 1;
      const row = el(
        "button",
        `relier relier-${leaf.kind} ${leaf.unsettled ? "relier-unsettled" : ""}`,
        el("span", "relier-name", folded ? nameNodes(leaf.name) : labelNodes(leaf.name, leaf.parameters)),
        folded ? el("span", "relier-count", `${declarations} overloads`) : null,
        el("span", "relier-caption", leafCaption(leaf)),
      );
      row.type = "button";
      row.setAttribute(
        "aria-label",
        `${leaf.name}${folded ? `, ${declarations} overloads` : leaf.parameters || ""}, ${leafCaption(leaf).toLowerCase()}. Open it under ${node.name}.`,
      );
      const said = [
        folded ? `${declarations} overloads of ${leaf.name} read as one row` : null,
        leaf.also.length ? `Also relies on ${leaf.also.join(", ")}` : null,
      ].filter(Boolean);
      if (said.length) row.title = said.join(". ");
      row.addEventListener("click", () => handlers.open(leaf));
      return el("li", null, row);
    });
    if (node.more > 0) {
      const more = el("button", "relier relier-more", el("span", "relier-name", `and ${node.more} more`));
      more.type = "button";
      more.setAttribute("aria-label", `${node.more} more rely on ${node.name}. Go to its step.`);
      more.addEventListener("click", () => handlers.open(node));
      rows.push(el("li", null, more));
    }
    // Said in sentence case with the name as written: never "RELIES ON WANT_BYTES".
    return el("div", "reliers", el("p", "reliers-head", "Relies on ", el("code", null, nameNodes(node.name))), el("ul", "reliers-list", rows));
  }

  function branch(node, handlers) {
    const item = el("li", null, changedButton(node, handlers));
    const below = node.children.map((child) => branch(child, handlers));
    if (node.reliers.length || node.more > 0) below.push(el("li", null, reliersCard(node, handlers)));
    if (below.length) item.append(el("ul", null, below));
    return item;
  }

  /** Count what a tree shows, for the sentences beside it. */
  function tally(roots) {
    const counts = { changed: 0, consumer: 0, test: 0, unsettled: 0, more: 0 };
    const walk = (node) => {
      counts.changed += 1;
      counts.more += node.more;
      for (const leaf of node.reliers) {
        counts[leaf.kind] += 1;
        if (leaf.unsettled) counts.unsettled += 1;
      }
      node.children.forEach(walk);
    };
    roots.forEach(walk);
    return counts;
  }

  /** Mark which sides of a scrolled map hide more, and show the hint while any does. */
  function watchOverflow(frame, scroller, hint) {
    const update = () => {
      const width = scroller.scrollWidth;
      const shown = scroller.clientWidth;
      if (!width || !shown) return;
      const left = scroller.scrollLeft > 2;
      const right = scroller.scrollLeft + shown < width - 2;
      frame.classList.toggle("can-left", left);
      frame.classList.toggle("can-right", right);
      hint.hidden = !(left || right);
      hint.firstChild.textContent = right ? "Scroll to see more" : "Back to the start";
      hint.lastChild.textContent = right ? " \u2192" : " \u2190";
    };
    scroller.addEventListener("scroll", update);
    if (typeof ResizeObserver === "function") new ResizeObserver(update).observe(scroller);
    else if (typeof requestAnimationFrame === "function") requestAnimationFrame(update);
  }

  /**
   * Draw the map. `handlers.open(node)` is called when a node is chosen.
   * Returns null when the change holds nothing to draw.
   */
  function renderMap(guide, handlers) {
    const roots = buildTree(guide);
    if (!roots.length) return null;
    const counts = tally(roots);
    const legend = el(
      "ul",
      "map-legend",
      el("li", null, el("span", "swatch swatch-changed"), "Changed here"),
      counts.consumer + counts.test + counts.more ? el("li", null, el("span", "swatch swatch-consumer"), `Relies on it from outside the ${nounOf(guide)}`) : null,
      counts.unsettled ? el("li", null, el("span", "swatch swatch-unsettled"), "Not updated with it") : null,
    );
    const tree = el("ul", "tree", roots.map((node) => branch(node, handlers)));
    tree.setAttribute("aria-label", `What this ${nounOf(guide)} touched and what relies on it`);
    const scroller = el("div", "map-scroll", tree);
    // A map wider than its card scrolls sideways. The edge that hides more
    // fades, and a hint says so, so nothing is cut off without a word.
    const hint = el("button", "map-more", "Scroll to see more", el("span", null, " \u2192"));
    hint.type = "button";
    hint.hidden = true;
    hint.addEventListener("click", () => {
      const end = scroller.scrollLeft + scroller.clientWidth >= scroller.scrollWidth - 2;
      scroller.scrollBy({ left: end ? -scroller.scrollWidth : scroller.clientWidth * 0.7, behavior: "smooth" });
    });
    const frame = el("div", "map-frame", scroller);
    watchOverflow(frame, scroller, hint);
    return {
      node: el(
        "section",
        "map",
        el(
          "header",
          "map-head",
          el("div", "map-title", el("h2", null, `What this ${nounOf(guide)} touches`), hint),
          el(
            "p",
            "map-sub",
            counts.consumer + counts.test + counts.more
              ? `${plural(counts.changed, "entity", "entities")} changed. What relies on ${counts.changed === 1 ? "it" : "them"} is drawn underneath.`
              : `${plural(counts.changed, "entity", "entities")} changed. Nothing else relies on ${counts.changed === 1 ? "it" : "them"}.`,
          ),
        ),
        frame,
        legend,
      ),
      counts,
    };
  }

  root.map = { buildTree, renderMap, nameNodes };
})();
