// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The workspace's maps: the repository drawn as clusters of entities by the
// container that holds them, and one entity drawn at the centre of what relies
// on it and what it relies on. Every dot is an entity a graph read returned;
// nothing here invents a node, a name or a connection.
(function (root) {
  "use strict";

  const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  const text = (value) => typeof value === "string" ? value : "";
  const number = (value) => Number.isSafeInteger(value) && value >= 0;
  const object = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
  const words = (value) => text(value).replace(/_/g, " ");
  const GOLDEN = Math.PI * (3 - Math.sqrt(5));
  const CONTAINER_KINDS = new Set(["module", "class", "struct", "interface", "trait", "enum", "impl", "namespace", "package"]);
  const QUIET_KINDS = new Set(["contains"]);

  // ── Reads ──────────────────────────────────────────────────────────────

  function entity(row) {
    if (!object(row) || !UUID.test(row.id) || typeof row.name !== "string" || typeof row.kind !== "string") {
      throw new Error("The graph returned an entity without a valid identity, name, or kind.");
    }
    return row;
  }

  /** The workspace graph read, refused when its counts or endpoints do not hold together. */
  function graphData(data) {
    if (!object(data) || data.schema !== "kin.workspace.graph.v1" || data.scope !== "workspace") {
      throw new Error("The graph read is in a form this page does not know.");
    }
    const graph = data.graph;
    const inventory = data.inventory;
    if (!object(graph) || !Array.isArray(graph.nodes) || !Array.isArray(graph.links) ||
        !object(inventory) || !Array.isArray(inventory.entities) || !number(inventory.total) ||
        inventory.repo_id !== data.repository_id || !number(data.offset) || !number(data.page_size) || data.page_size < 1 ||
        !number(graph.entity_count) || !number(graph.relation_count) || !number(graph.unresolved_links) ||
        !number(graph.filtered_links) || typeof graph.sampled !== "boolean") {
      throw new Error("The graph read left out its counts.");
    }
    graph.nodes.forEach(entity);
    inventory.entities.forEach(entity);
    const ids = new Set(graph.nodes.map((row) => row.id));
    if (ids.size !== graph.nodes.length || graph.nodes.length > graph.entity_count || inventory.entities.length > inventory.total) {
      throw new Error("The graph read has counts or identities that do not agree.");
    }
    for (const link of graph.links) {
      if (!object(link) || !ids.has(link.source) || !ids.has(link.target) || typeof link.kind !== "string") {
        throw new Error("The graph read has a connection without both of its entities.");
      }
    }
    return data;
  }

  function endpoint(value) {
    if (typeof value === "string") return { id: value };
    if (!object(value)) return {};
    if (typeof value.Entity === "string") return { id: value.Entity };
    const keys = Object.keys(value);
    return { external: keys.length === 1 ? keys[0] : "unresolved endpoint" };
  }

  /**
   * The direct relationships of one entity in a neighbourhood read, once each.
   * `incoming` rows rely on the entity; `outgoing` rows are what it relies on.
   */
  function neighborRows(neighborhood, selected) {
    if (!object(neighborhood) || neighborhood.focal_id !== selected || !Array.isArray(neighborhood.entities) ||
        !Array.isArray(neighborhood.relations) || typeof neighborhood.truncated !== "boolean") {
      throw new Error("The connections read named a different entity.");
    }
    const entities = new Map(neighborhood.entities.filter((row) => object(row) && UUID.test(row.id)).map((row) => [row.id, row]));
    const seen = new Set();
    const rows = [];
    for (const relation of neighborhood.relations) {
      if (!object(relation)) continue;
      const src = endpoint(relation.src);
      const dst = endpoint(relation.dst);
      if (src.id !== selected && dst.id !== selected) continue;
      const outgoing = src.id === selected;
      const other = outgoing ? dst : src;
      const key = [src.id || src.external, dst.id || dst.external, relation.kind, relation.resolution].join("|");
      if (seen.has(key)) continue;
      seen.add(key);
      const row = entities.get(other.id);
      rows.push({
        id: UUID.test(other.id || "") ? other.id : undefined,
        name: row?.name || other.external || other.id || "Unresolved endpoint",
        entityKind: text(row?.kind),
        signature: text(row?.signature),
        kind: words(relation.kind) || "relationship",
        relation: text(relation.kind),
        direction: outgoing ? "outgoing" : "incoming",
        resolution: text(relation.resolution),
      });
    }
    return rows;
  }

  // The server provides a semantic intent, but a changed or older server must
  // not turn this anchor into an arbitrary URL, command, or historical selector.
  function editorUri(raw, selected) {
    const prefix = "vscode://firelock.kin-editor/open-entity?";
    const control = /[\u0000-\u001f\u007f]/;
    if (typeof raw !== "string" || raw.length > 16384 || !raw.startsWith(prefix) ||
        raw.includes("#") || control.test(raw)) return null;
    // Match the producer's RFC 3986 encoding, including punctuation left raw by
    // encodeURIComponent. VS Code consumes this outer layer before its handler.
    const encode = value => encodeURIComponent(value).replace(/[!'()*]/g,
      character => `%${character.charCodeAt(0).toString(16).toUpperCase()}`);
    let query = raw.slice(prefix.length);
    const fields = new Map();
    const allowed = ["v", "repository", "workspace", "root", "entity", "scope"];
    try {
      if (!query.includes("=")) {
        const inner = decodeURIComponent(query);
        if (encode(inner) !== query) return null;
        query = inner;
      }
      // Do not repair malformed escapes or turn literal plus signs into spaces.
      for (const pair of query.split("&")) {
        const equal = pair.indexOf("=");
        if (equal < 1) return null;
        const key = decodeURIComponent(pair.slice(0, equal));
        const value = decodeURIComponent(pair.slice(equal + 1));
        if (!allowed.includes(key) || fields.has(key) || !value || control.test(value)) return null;
        fields.set(key, value);
      }
      if (fields.size !== allowed.length || fields.get("v") !== "1" || fields.get("scope") !== "workspace" ||
          fields.get("entity") !== selected || !UUID.test(selected || "") ||
          !UUID.test(fields.get("workspace") || "")) return null;
      // Normalize validated legacy v1 links too, so their value escapes survive
      // the platform decode even when repository identities contain '&' or '%'.
      const transport = prefix + encode(query);
      return transport.length <= 16384 ? transport : null;
    } catch { return null; }
  }

  // ── Map model ──────────────────────────────────────────────────────────

  /**
   * A drawable map from the workspace graph read: code entities grouped by the
   * module that contains them. Modules are drawn as their cluster, not as a
   * dot, and containment is drawn as closeness rather than as a line.
   */
  function mapFromGraph(graph, limit = 2000) {
    const nodes = Array.isArray(graph?.nodes) ? graph.nodes.filter((row) => object(row) && UUID.test(row.id) && typeof row.name === "string") : [];
    const links = Array.isArray(graph?.links) ? graph.links.filter(object) : [];
    const byId = new Map(nodes.map((row) => [row.id, row]));
    const byName = new Map();
    for (const row of nodes) if (!byName.has(row.name)) byName.set(row.name, row);
    const contains = new Map();
    for (const link of links) {
      if (text(link.kind).toLowerCase() !== "contains" || !byId.has(link.source) || !byId.has(link.target)) continue;
      for (const [a, b] of [[link.source, link.target], [link.target, link.source]]) {
        if (!contains.has(a)) contains.set(a, []);
        contains.get(a).push(b);
      }
    }
    const isModule = (row) => text(row?.kind).toLowerCase() === "module";
    const memo = new Map();
    function moduleOf(id, seen = new Set()) {
      if (memo.has(id)) return memo.get(id);
      if (seen.has(id)) return null;
      seen.add(id);
      const row = byId.get(id);
      if (!row) return null;
      if (isModule(row)) return id;
      let found = null;
      const around = contains.get(id) || [];
      for (const other of around) if (isModule(byId.get(other))) { found = other; break; }
      if (!found) {
        for (const other of around) {
          const owner = byId.get(other);
          if (owner && row.name.startsWith(`${owner.name}.`)) { found = moduleOf(other, seen); if (found) break; }
        }
      }
      if (!found && row.name.includes(".")) {
        const owner = byName.get(row.name.slice(0, row.name.indexOf(".")));
        if (owner && owner.id !== id) found = moduleOf(owner.id, seen);
      }
      memo.set(id, found);
      return found;
    }
    const drawn = nodes.filter((row) => !isModule(row))
      .sort((a, b) => (b.degree || 0) - (a.degree || 0) || a.name.localeCompare(b.name))
      .slice(0, limit);
    const kept = new Set(drawn.map((row) => row.id));
    const groups = new Map();
    const out = drawn.map((row) => {
      const module = moduleOf(row.id);
      const key = module || "";
      if (!groups.has(key)) groups.set(key, { id: key, name: module ? byId.get(module).name : "", size: 0 });
      groups.get(key).size += 1;
      return { id: row.id, name: row.name, kind: text(row.kind), weight: number(row.degree) ? row.degree : 0, group: key, signature: text(row.signature) };
    });
    const edges = [];
    const seen = new Set();
    for (const link of links) {
      if (QUIET_KINDS.has(text(link.kind).toLowerCase()) || !kept.has(link.source) || !kept.has(link.target) || link.source === link.target) continue;
      const key = link.source < link.target ? `${link.source}|${link.target}` : `${link.target}|${link.source}`;
      if (seen.has(key)) continue;
      seen.add(key);
      edges.push({ source: link.source, target: link.target, kind: text(link.kind) });
    }
    return { nodes: out, edges, groups: [...groups.values()] };
  }

  /**
   * A drawable model from the server's repository map (`kin.workspace.map.v1`).
   * Top-level containers become clusters and are named, not drawn as dots;
   * a container with none of its entities shown is drawn as its own dot so it
   * still has a place. Container edges tie clusters together; entity edges are
   * the lines between dots.
   */
  function mapFromContainers(map, limit = 2000) {
    const rows = (Array.isArray(map?.nodes) ? map.nodes : []).filter((row) => object(row) && UUID.test(row.id) && typeof row.name === "string");
    const containers = rows.filter((row) => row.role === "container");
    const groupName = new Map(containers.map((row) => [row.id, row.name]));
    const groupKind = new Map(containers.map((row) => [row.id, text(row.kind)]));
    const groupOf = (row) => text(row.group) || text(row.parent) || "";
    const entities = rows.filter((row) => row.role !== "container")
      .map((row) => ({ id: row.id, name: row.name, kind: text(row.kind), weight: number(row.degree) ? row.degree : 0, group: groupOf(row), signature: text(row.signature), test: row.is_test === true || text(row.kind) === "test" }))
      .sort((a, b) => b.weight - a.weight || a.name.localeCompare(b.name)).slice(0, limit);
    const populated = new Set(entities.map((row) => row.group));
    const lone = containers.filter((row) => !populated.has(row.id))
      .map((row) => ({ id: row.id, name: row.name, kind: text(row.kind), weight: number(row.degree) ? row.degree : 0, group: row.id, container: true }));
    const nodes = [...entities, ...lone];
    const kept = new Set(nodes.map((row) => row.id));
    const groups = new Map();
    for (const row of nodes) {
      if (!groups.has(row.group)) groups.set(row.group, { id: row.group, name: groupName.get(row.group) || "", kind: groupKind.get(row.group) || "", size: 0 });
      groups.get(row.group).size += 1;
    }
    const edges = [];
    const ties = [];
    const seen = new Set();
    for (const link of Array.isArray(map?.edges) ? map.edges : []) {
      const source = link?.source, target = link?.target;
      if (!source || !target || source === target) continue;
      if (link.level === "container") {
        if (groups.has(source) && groups.has(target)) ties.push({ source, target, weight: number(link.weight) ? link.weight : 1 });
        continue;
      }
      if (!kept.has(source) || !kept.has(target)) continue;
      const key = source < target ? `${source}|${target}` : `${target}|${source}`;
      if (seen.has(key)) continue;
      seen.add(key);
      edges.push({ source, target, kind: Object.keys(object(link.kinds) ? link.kinds : {})[0] || "" });
    }
    return { nodes, edges, ties, groups: [...groups.values()] };
  }

  // ── Layout ─────────────────────────────────────────────────────────────

  function hash(value) {
    let h = 2166136261;
    for (let i = 0; i < value.length; i++) { h ^= value.charCodeAt(i); h = Math.imul(h, 16777619); }
    return h >>> 0;
  }
  function random(seed) {
    let state = seed || 1;
    return () => { state ^= state << 13; state ^= state >>> 17; state ^= state << 5; return ((state >>> 0) % 100000) / 100000; };
  }

  /**
   * Clusters packed into a wide field. Each container's entities sit in a
   * sunflower with the most connected at its heart; containers that call each
   * other are pulled closer. Returns world coordinates and each group's circle.
   */
  function layoutClusters(model, aspect = 2.6) {
    const members = new Map();
    for (const node of model.nodes) {
      if (!members.has(node.group)) members.set(node.group, []);
      members.get(node.group).push(node);
    }
    const groups = [...members.entries()].map(([id, nodes]) => {
      nodes.sort((a, b) => b.weight - a.weight || a.name.localeCompare(b.name));
      return { id, nodes, r: 1.25 * Math.sqrt(nodes.length) + 0.9, x: 0, y: 0, vx: 0, vy: 0 };
    }).sort((a, b) => b.nodes.length - a.nodes.length || String(a.id).localeCompare(String(b.id)));
    const index = new Map(groups.map((group, i) => [group.id, i]));
    const groupOf = new Map(model.nodes.map((node) => [node.id, index.get(node.group)]));
    const ties = new Map();
    const tie = (a, b, weight) => {
      if (a === undefined || b === undefined || a === b) return;
      const key = a < b ? `${a}|${b}` : `${b}|${a}`;
      ties.set(key, (ties.get(key) || 0) + weight);
    };
    if (Array.isArray(model.ties) && model.ties.length) for (const link of model.ties) tie(index.get(link.source), index.get(link.target), link.weight || 1);
    else for (const edge of model.edges) tie(groupOf.get(edge.source), groupOf.get(edge.target), 1);
    const spread = groups.reduce((sum, group) => sum + group.r, 0) / Math.max(1, groups.length);
    groups.forEach((group, i) => {
      const angle = i * GOLDEN;
      const distance = i === 0 ? 0 : spread * 2.1 * Math.sqrt(i);
      group.x = Math.cos(angle) * distance * Math.sqrt(aspect);
      group.y = Math.sin(angle) * distance / Math.sqrt(aspect);
    });
    const tieList = [...ties.entries()].map(([key, weight]) => { const [a, b] = key.split("|").map(Number); return { a, b, weight }; });
    const gapBetween = 1.4;
    function separate() {
      for (let i = 0; i < groups.length; i++) {
        for (let j = i + 1; j < groups.length; j++) {
          const ga = groups[i], gb = groups[j];
          let dx = gb.x - ga.x, dy = gb.y - ga.y;
          let distance = Math.hypot(dx, dy);
          const rest = ga.r + gb.r + gapBetween;
          if (distance >= rest) continue;
          if (distance < 1e-6) { dx = 0.01 * (j - i); dy = 0.013; distance = Math.hypot(dx, dy); }
          const push = (rest - distance) / 2;
          const share = gb.nodes.length / (ga.nodes.length + gb.nodes.length);
          ga.x -= dx / distance * push * 2 * share; ga.y -= dy / distance * push * 2 * share;
          gb.x += dx / distance * push * 2 * (1 - share); gb.y += dy / distance * push * 2 * (1 - share);
        }
      }
    }
    for (let step = 0; step < 260; step++) {
      const cool = 1 - step / 260;
      for (const { a, b, weight } of tieList) {
        const ga = groups[a], gb = groups[b];
        const dx = gb.x - ga.x, dy = gb.y - ga.y;
        const distance = Math.hypot(dx, dy) || 0.01;
        const rest = ga.r + gb.r + gapBetween;
        if (distance <= rest) continue;
        const pull = Math.min(0.02, 0.0025 * Math.sqrt(weight)) * (distance - rest) * cool;
        ga.x += dx / distance * pull; ga.y += dy / distance * pull;
        gb.x -= dx / distance * pull; gb.y -= dy / distance * pull;
      }
      // Gravity pulls harder vertically than across, so the field spreads
      // into the wide stage it is drawn on instead of a ball in its middle.
      for (const group of groups) {
        group.x -= group.x * 0.006 * cool;
        group.y -= group.y * 0.006 * aspect * aspect * cool;
      }
      separate();
    }
    // Widen to the stage: stretch the centres across, then let the clusters
    // settle back toward the middle line without overlapping.
    const extent = () => {
      let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
      for (const group of groups) { minX = Math.min(minX, group.x - group.r); maxX = Math.max(maxX, group.x + group.r); minY = Math.min(minY, group.y - group.r); maxY = Math.max(maxY, group.y + group.r); }
      return { w: maxX - minX, h: maxY - minY };
    };
    for (let round = 0; round < 24 && groups.length > 2; round++) {
      const box = extent();
      if (box.w / Math.max(1e-6, box.h) >= aspect * 0.9) break;
      for (const group of groups) group.x *= 1.08;
      for (let step = 0; step < 20; step++) {
        for (const group of groups) group.y -= group.y * 0.02;
        separate();
      }
    }
    const positions = new Map();
    for (const group of groups) {
      const turn = (hash(String(group.id)) % 628) / 100;
      group.nodes.forEach((node, k) => {
        const radius = 1.05 * Math.sqrt(k + 0.35);
        const angle = k * GOLDEN + turn;
        positions.set(node.id, { x: group.x + Math.cos(angle) * radius, y: group.y + Math.sin(angle) * radius });
      });
    }
    return { positions, groups: groups.map((group) => ({ id: group.id, x: group.x, y: group.y, r: group.r, size: group.nodes.length })) };
  }

  /**
   * One entity at the centre, drawn as a bow tie: what relies on it stacked on
   * the left, what it relies on stacked on the right, each line flowing into
   * the centre. Columns bend toward the middle at their ends. World units are
   * pixels at scale one.
   */
  function layoutFocus(model, focalId, perSide = 12) {
    const positions = new Map([[focalId, { x: 0, y: 0 }]]);
    const sides = { in: [], out: [] };
    for (const node of model.nodes) {
      if (node.id === focalId) continue;
      (node.role === "out" ? sides.out : sides.in).push(node);
    }
    const SPACING = 30, WIDTH = 270;
    for (const [side, nodes] of Object.entries(sides)) {
      const shown = nodes.slice(0, perSide).sort((a, b) => (a.role === "test") - (b.role === "test"));
      const half = (shown.length - 1) / 2;
      shown.forEach((node, i) => {
        const y = (i - half) * SPACING;
        const reach = half > 0 ? Math.abs(i - half) / half : 0;
        const x = (WIDTH - reach * reach * 46) * (side === "in" ? -1 : 1);
        positions.set(node.id, { x, y });
      });
    }
    return { positions, groups: [] };
  }

  /**
   * Which labels fit. Rectangles are tried in priority order and a label is
   * kept only when it sits inside the bounds and clear of every kept one.
   */
  function placeLabels(rects, bounds, gap = 6) {
    const kept = [];
    const visible = new Set();
    for (const rect of rects) {
      if (!rect.force && (rect.left < bounds.left + 4 || rect.right > bounds.right - 4 || rect.top < bounds.top + 4 || rect.bottom > bounds.bottom - 4)) continue;
      if (!rect.force && kept.some((other) => rect.left < other.right + gap && rect.right + gap > other.left && rect.top < other.bottom + gap && rect.bottom + gap > other.top)) continue;
      kept.push(rect);
      visible.add(rect.id);
    }
    return visible;
  }

  // ── Renderer ───────────────────────────────────────────────────────────

  const TINTS = ["--map-1", "--map-2", "--map-3", "--map-4", "--map-5"];

  /**
   * Draw a model into `host`. The canvas draws dots and lines; names are real
   * buttons laid over it, so each labelled entity is reachable by keyboard.
   * Drift is slow and stops entirely under reduced motion.
   */
  function createMap(host, model, options = {}) {
    const doc = host.ownerDocument || root.document;
    const view = doc.defaultView || root;
    const mode = options.mode === "focus" ? "focus" : "clusters";
    const initial = host.getBoundingClientRect ? host.getBoundingClientRect() : null;
    const stageAspect = initial && initial.width > 0 && initial.height > 0 ? Math.max(1.2, Math.min(3.6, (initial.width - 96) / Math.max(1, initial.height - 100))) : 2.6;
    const layout = mode === "focus" ? layoutFocus(model, options.focalId, options.perSide || 12) : layoutClusters(model, options.aspect || stageAspect);
    const canvas = doc.createElement("canvas");
    canvas.className = "map-canvas";
    canvas.setAttribute("role", "img");
    canvas.setAttribute("aria-label", options.summary || "Repository map");
    const labelLayer = doc.createElement("div");
    labelLayer.className = "map-labels";
    host.append(canvas, labelLayer);
    const context = canvas.getContext ? canvas.getContext("2d") : null;
    const tip = options.tip || null;
    const reduced = Boolean(view.matchMedia?.("(prefers-reduced-motion: reduce)").matches);
    const nodes = model.nodes.filter((node) => layout.positions.has(node.id)).map((node) => {
      const base = layout.positions.get(node.id);
      const seed = random(hash(node.id));
      return { ...node, wx: base.x, wy: base.y, sx: 0, sy: 0, x: 0, y: 0, phase: seed() * Math.PI * 2, fx: seed() * 2 - 1, fy: seed() * 2 - 1,
        radius: node.id === options.focalId ? 9 : Math.min(7.5, 2.1 + Math.sqrt(node.weight || 0) * 0.85) };
    });
    const byId = new Map(nodes.map((node) => [node.id, node]));
    const groupIndex = new Map(layout.groups.map((group, i) => [group.id, i]));
    const groups = layout.groups.map((group) => {
      const seed = random(hash(`group:${group.id}`));
      const name = (model.groups || []).find((row) => row.id === group.id)?.name || "";
      return { ...group, name, phase: seed() * Math.PI * 2, fx: seed() * 2 - 1, fy: seed() * 2 - 1, cx: 0, cy: 0, sr: 0 };
    });
    const edges = model.edges.map((edge) => ({ a: byId.get(edge.source), b: byId.get(edge.target), kind: edge.kind })).filter((edge) => edge.a && edge.b);
    const heaviest = Math.max(1, ...(model.ties || []).map((tie) => tie.weight || 1));
    const ties = (model.ties || []).map((tie) => ({ a: groupIndex.get(tie.source), b: groupIndex.get(tie.target), weight: tie.weight || 1 }))
      .filter((tie) => tie.a !== undefined && tie.b !== undefined && tie.a !== tie.b);
    const neighbours = new Map();
    for (const edge of edges) {
      for (const [a, b] of [[edge.a.id, edge.b.id], [edge.b.id, edge.a.id]]) {
        if (!neighbours.has(a)) neighbours.set(a, new Set());
        neighbours.get(a).add(b);
      }
    }
    let colors = {};
    let width = 0, height = 0, dpr = 1, scale = 1, offsetX = 0, offsetY = 0;
    let hovered = null, frameHandle = 0, lastFrame = 0, running = false, visible = true, disposed = false;
    // A short entrance on the first frames: lines and dots ease in once.
    let bornAt = null, appear = reduced ? 1 : 0;
    let labels = [];
    // Entities an agent is working on, ringed on the map: "looked" for what
    // it read, "changing" for what it is editing or just committed.
    let marks = new Map();

    function readColors() {
      const style = view.getComputedStyle ? view.getComputedStyle(host) : null;
      const value = (name, fallback) => (style?.getPropertyValue(name) || "").trim() || fallback;
      colors = {
        tints: TINTS.map((name, i) => value(name, ["#6c48fa", "#3b74fb", "#ae5aff", "#00b5d8", "#8a76f2"][i])),
        edge: value("--map-edge", "rgba(120,110,200,0.14)"),
        hot: value("--map-edge-hot", "rgba(120,100,255,0.8)"),
        halo: value("--map-halo", "rgba(120,100,255,0.08)"),
        agent: value("--map-agent", "#1f9d55"),
        changing: value("--map-changing", "#d38b00"),
        accent: value("--accent", "#5b45e8"),
        test: value("--map-4", "#00b5d8"),
        surface: value("--surface", "#ffffff"),
        work: value("--warn-ink", "#c08a1e"),
      };
    }
    function tintOf(node) {
      if (mode === "focus") return node.id === options.focalId ? colors.accent : node.role === "test" ? colors.test : node.role === "out" ? colors.tints[1] : colors.tints[0];
      return colors.tints[(groupIndex.get(node.group) || 0) % colors.tints.length];
    }

    function fit() {
      const box = host.getBoundingClientRect ? host.getBoundingClientRect() : { width: 800, height: 400 };
      width = Math.max(1, box.width); height = Math.max(1, box.height);
      dpr = Math.min(2, view.devicePixelRatio || 1);
      canvas.width = Math.round(width * dpr); canvas.height = Math.round(height * dpr);
      let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
      for (const node of nodes) { minX = Math.min(minX, node.wx); maxX = Math.max(maxX, node.wx); minY = Math.min(minY, node.wy); maxY = Math.max(maxY, node.wy); }
      for (const group of groups) { minX = Math.min(minX, group.x - group.r); maxX = Math.max(maxX, group.x + group.r); minY = Math.min(minY, group.y - group.r); maxY = Math.max(maxY, group.y + group.r); }
      if (!Number.isFinite(minX)) { minX = -1; maxX = 1; minY = -1; maxY = 1; }

      const pad = options.padding || (mode === "focus" ? { x: Math.min(240, Math.max(120, width * 0.3)), top: 58, bottom: 52 } : { x: 48, top: 54, bottom: 46 });
      // Names hang off the outer side of each column. A side with nothing on
      // it needs no room for names, so the drawing centres on what is there.
      let padLeft = pad.x, padRight = pad.x;
      if (mode === "focus") {
        if (!nodes.some((node) => node.wx < 0)) padLeft = 90;
        if (!nodes.some((node) => node.wx > 0)) padRight = 90;
      }
      const usableW = Math.max(40, width - padLeft - padRight), usableH = Math.max(40, height - pad.top - pad.bottom);
      scale = Math.min(usableW / Math.max(1, maxX - minX), usableH / Math.max(1, maxY - minY));
      // The bow tie keeps its rows apart on a narrow stage: it narrows across
      // and keeps its height, so names stay readable.
      let scaleX = scale, scaleY = scale;
      if (mode === "focus") {
        scaleX = Math.min(1.15, usableW / Math.max(1, maxX - minX));
        scaleY = Math.min(1.15, usableH / Math.max(1, maxY - minY));
        scale = Math.min(scaleX, scaleY);
      }
      offsetX = padLeft + (usableW - (maxX - minX) * scaleX) / 2 - minX * scaleX;
      offsetY = pad.top + (usableH - (maxY - minY) * scaleY) / 2 - minY * scaleY;
      for (const node of nodes) { node.sx = offsetX + node.wx * scaleX; node.sy = offsetY + node.wy * scaleY; }
      for (const group of groups) { group.cx = offsetX + group.x * scale; group.cy = offsetY + group.y * scale; group.sr = group.r * scale; }
      buildLabels();
    }

    function buildLabels() {
      labelLayer.replaceChildren();
      labels = [];
      // Entities an agent is working on are named first, then those in
      // uncommitted work, then the most relied on.
      const marked = new Set(options.markIds || []);
      const priority = new Map([...marks.keys(), ...marked, ...(options.labelIds || [])].map((id, i) => [id, i]).reverse());
      const rank = (node) => priority.has(node.id) ? priority.get(node.id) : Infinity;
      const ranked = nodes.filter((node) => !node.container).sort((a, b) => (b.id === options.focalId) - (a.id === options.focalId) || rank(a) - rank(b) || b.weight - a.weight || a.name.localeCompare(b.name));
      const wanted = mode === "focus" ? ranked : ranked.slice(0, options.labelCount || 10);
      const candidates = [];
      for (const node of wanted) {
        const focal = node.id === options.focalId;
        const label = doc.createElement(focal ? "span" : "button");
        if (!focal) {
          label.setAttribute("type", "button");
          label.setAttribute("aria-label", `${node.name}, ${words(node.kind).toLowerCase() || "entity"}. Open.`);
          label.addEventListener("click", () => options.onOpen?.(node.id));
          label.addEventListener("pointerenter", () => setHover(node));
          label.addEventListener("pointerleave", () => setHover(null));
          label.addEventListener("focus", () => setHover(node));
          label.addEventListener("blur", () => setHover(null));
        }
        label.className = `map-label${focal ? " map-label-focal" : ""}${node.role === "test" ? " map-label-test" : ""}${(options.markIds || []).includes(node.id) ? " map-label-work" : ""}${marks.has(node.id) ? ` map-label-agent map-label-${marks.get(node.id)}` : ""}`;
        label.textContent = node.name;
        if (appear < 1) label.style.opacity = "0";
        const leftSide = mode === "focus" ? node.role !== "out" : node.sx > width * 0.62;
        // A name never runs off the stage: on a narrow window it is cut
        // short with an ellipsis rather than hidden.
        if (mode === "focus" && !focal) {
          const room = leftSide ? node.sx - node.radius - 13 : width - node.sx - node.radius - 13;
          label.style.maxWidth = `${Math.max(60, Math.min(220, room))}px`;
        }
        labelLayer.append(label);
        const w = label.offsetWidth || node.name.length * 7 + 18, h = label.offsetHeight || 22;
        const left = focal ? -w / 2 : leftSide ? -w - node.radius - 7 : node.radius + 7;
        const top = focal ? -h / 2 : -h / 2;
        candidates.push({ id: node.id, node, label, dx: left, dy: top, w, h, force: focal,
          left: node.sx + left, right: node.sx + left + w, top: node.sy + top, bottom: node.sy + top + h });
      }
      const groupCandidates = [];
      if (mode === "clusters") {
        for (const group of groups.slice().sort((a, b) => b.size - a.size)) {
          if (!group.name || group.size < 3) continue;
          const label = doc.createElement("span");
          label.className = "map-label map-label-quiet";
          label.setAttribute("aria-hidden", "true");
          label.textContent = group.name;
          if (appear < 1) label.style.opacity = "0";
          labelLayer.append(label);
          const w = label.offsetWidth || group.name.length * 7 + 18, h = label.offsetHeight || 20;
          groupCandidates.push({ id: `group:${group.id}`, group, label, dx: -w / 2, dy: -group.sr - h - 2, w, h,
            left: group.cx - w / 2, right: group.cx + w / 2, top: group.cy - group.sr - h - 2, bottom: group.cy - group.sr - 2 });
        }
      }
      const shown = placeLabels([...candidates, ...groupCandidates], { left: 0, top: 0, right: width, bottom: height - (options.reserveBottom || 0) }, mode === "focus" ? 2 : 6);
      for (const item of [...candidates, ...groupCandidates]) {
        if (!shown.has(item.id)) { item.label.remove(); continue; }
        labels.push(item);
      }
    }

    function positionAt(time) {
      const t = reduced ? 0 : time;
      for (const group of groups) {
        group.dx = 5 * Math.sin(t * 0.11 * (0.6 + Math.abs(group.fx)) + group.phase);
        group.dy = 4 * Math.cos(t * 0.09 * (0.6 + Math.abs(group.fy)) + group.phase);
      }
      for (const node of nodes) {
        const group = groups[groupIndex.get(node.group)];
        const still = node.id === options.focalId;
        node.x = node.sx + (group ? group.dx : 0) + (still ? 0 : 2.4 * Math.sin(t * 0.21 * (0.5 + Math.abs(node.fx)) + node.phase) * (mode === "focus" ? 0.8 : 1));
        node.y = node.sy + (group ? group.dy : 0) + (still ? 0 : 2.4 * Math.cos(t * 0.17 * (0.5 + Math.abs(node.fy)) + node.phase) * (mode === "focus" ? 0.8 : 1));
      }
    }

    function draw(time) {
      if (!context) return;
      positionAt(time);
      context.setTransform(dpr, 0, 0, dpr, 0, 0);
      context.clearRect(0, 0, width, height);
      const hot = hovered ? neighbours.get(hovered.id) || new Set() : null;
      for (const group of groups) {
        if (group.size < 2) continue;
        const gradient = context.createRadialGradient(group.cx + group.dx, group.cy + group.dy, 0, group.cx + group.dx, group.cy + group.dy, group.sr + 14);
        gradient.addColorStop(0, colors.halo); gradient.addColorStop(1, "rgba(0,0,0,0)");
        context.fillStyle = gradient;
        context.beginPath(); context.arc(group.cx + group.dx, group.cy + group.dy, group.sr + 14, 0, Math.PI * 2); context.fill();
      }
      if (ties.length) {
        context.strokeStyle = colors.halo;
        context.lineCap = "round";
        for (const tie of ties) {
          const ga = groups[tie.a], gb = groups[tie.b];
          const ax = ga.cx + ga.dx, ay = ga.cy + ga.dy, bx = gb.cx + gb.dx, by = gb.cy + gb.dy;
          context.globalAlpha = hovered ? 0.25 : 0.55;
          context.lineWidth = 1.5 + 6 * Math.sqrt(tie.weight / heaviest);
          context.beginPath();
          context.moveTo(ax, ay);
          context.quadraticCurveTo((ax + bx) / 2 - (by - ay) * 0.12, (ay + by) / 2 + (bx - ax) * 0.12, bx, by);
          context.stroke();
        }
        context.globalAlpha = 1;
      }
      context.lineWidth = mode === "focus" ? 1.2 : 1;
      context.strokeStyle = colors.edge;
      context.globalAlpha = (hovered ? 0.45 : 1) * appear;
      context.beginPath();
      for (const edge of edges) curve(edge.a, edge.b);
      context.stroke();
      context.globalAlpha = 1;
      if (hovered) {
        context.strokeStyle = colors.hot; context.lineWidth = 1.4;
        context.beginPath();
        for (const edge of edges) if (edge.a === hovered || edge.b === hovered) curve(edge.a, edge.b);
        context.stroke();
      }
      const pulse = reduced ? 1 : 0.8 + 0.2 * Math.sin(time * 0.8);
      for (const node of nodes) {
        const dim = hovered && node !== hovered && !hot.has(node.id);
        const tint = tintOf(node);
        const grow = Math.min(1, appear * 1.6);
        context.globalAlpha = (dim ? 0.28 : 1) * grow;
        if (node.radius > 4.5 || node.id === options.focalId) {
          const glow = context.createRadialGradient(node.x, node.y, 0, node.x, node.y, node.radius * 3.4);
          glow.addColorStop(0, tint); glow.addColorStop(1, "rgba(0,0,0,0)");
          context.globalAlpha = (dim ? 0.08 : 0.22) * pulse * grow;
          context.fillStyle = glow;
          context.beginPath(); context.arc(node.x, node.y, node.radius * 3.4, 0, Math.PI * 2); context.fill();
          context.globalAlpha = (dim ? 0.28 : 1) * grow;
        }
        context.fillStyle = tint;
        context.beginPath(); context.arc(node.x, node.y, node.radius * (0.4 + 0.6 * grow), 0, Math.PI * 2); context.fill();
        if ((options.markIds || []).includes(node.id)) {
          // Uncommitted work: a warm ring around the entity.
          context.strokeStyle = colors.work; context.lineWidth = 2;
          context.beginPath(); context.arc(node.x, node.y, node.radius + 4.5, 0, Math.PI * 2); context.stroke();
        }
        if (node === hovered || node.id === options.focalId) {
          context.strokeStyle = colors.surface; context.lineWidth = 2;
          context.stroke();
          context.strokeStyle = tint; context.lineWidth = 1.5;
          context.beginPath(); context.arc(node.x, node.y, node.radius + 3.5, 0, Math.PI * 2); context.stroke();
        }
      }
      context.globalAlpha = 1;
      if (marks.size) {
        const beat = reduced ? 0 : (Math.sin(time * 2.4) + 1) / 2;
        for (const node of nodes) {
          const tone = marks.get(node.id);
          if (!tone) continue;
          const color = tone === "changing" ? colors.changing : colors.agent;
          const reach = node.radius + 15;
          const glow = context.createRadialGradient(node.x, node.y, node.radius, node.x, node.y, reach);
          glow.addColorStop(0, color); glow.addColorStop(1, "rgba(0,0,0,0)");
          context.globalAlpha = (0.18 + 0.14 * beat) * appear;
          context.fillStyle = glow;
          context.beginPath(); context.arc(node.x, node.y, reach, 0, Math.PI * 2); context.fill();
          context.globalAlpha = appear;
          context.strokeStyle = color; context.lineWidth = tone === "changing" ? 2.2 : 1.6;
          context.beginPath(); context.arc(node.x, node.y, node.radius + 4 + beat * 2.5, 0, Math.PI * 2); context.stroke();
        }
        context.globalAlpha = 1;
      }
      for (const item of labels) {
        const x = item.node ? item.node.x + item.dx : item.group.cx + item.group.dx + item.dx;
        const y = item.node ? item.node.y + item.dy : item.group.cy + item.group.dy + item.dy;
        item.label.style.transform = `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px)`;
        if (appear < 1) item.label.style.opacity = String(Math.max(0, appear * 1.4 - 0.4));
        else if (item.label.style.opacity) item.label.style.opacity = "";
        if (item.node) item.label.dataset.hot = String(item.node === hovered);
      }
    }

    function curve(a, b) {
      context.moveTo(a.x, a.y);
      if (mode === "focus") {
        const dx = b.x - a.x;
        context.bezierCurveTo(a.x + dx * 0.55, a.y, b.x - dx * 0.3, b.y, b.x, b.y);
        return;
      }
      const mx = (a.x + b.x) / 2, my = (a.y + b.y) / 2;
      const dx = b.x - a.x, dy = b.y - a.y;
      const bend = mode === "focus" ? 0.12 : a.group === b.group ? 0.04 : 0.18;
      context.quadraticCurveTo(mx - dy * bend, my + dx * bend, b.x, b.y);
    }

    function loop(now) {
      frameHandle = 0;
      if (disposed || !running) return;
      if (bornAt === null) bornAt = now;
      if (appear < 1) { const t = Math.min(1, (now - bornAt) / 900); appear = 1 - Math.pow(1 - t, 3); }
      if (now - lastFrame >= 32) { lastFrame = now; draw(now / 1000); }
      frameHandle = view.requestAnimationFrame(loop);
    }
    function start() {
      if (disposed) return;
      running = !reduced && visible && !doc.hidden;
      if (running && !frameHandle && view.requestAnimationFrame) frameHandle = view.requestAnimationFrame(loop);
      if (!running) draw(0);
    }
    function stop() {
      running = false;
      if (frameHandle && view.cancelAnimationFrame) view.cancelAnimationFrame(frameHandle);
      frameHandle = 0;
    }

    function setHover(node) {
      if (hovered === node) return;
      hovered = node;
      canvas.dataset.hover = String(Boolean(node));
      if (!running) draw(0);
    }
    function nearest(clientX, clientY) {
      const box = canvas.getBoundingClientRect();
      const x = clientX - box.left, y = clientY - box.top;
      let best = null, distance = 14;
      for (const node of nodes) {
        const d = Math.hypot(node.x - x, node.y - y) - node.radius;
        if (d < distance) { distance = d; best = node; }
      }
      return best;
    }
    function showTip(node, clientX, clientY) {
      if (!tip) return;
      if (!node) { tip.hidden = true; return; }
      const about = options.describe ? options.describe(node) : words(node.kind);
      tip.replaceChildren();
      const name = doc.createElement("strong"); name.textContent = node.name;
      const meta = doc.createElement("span"); meta.textContent = about;
      tip.append(name, meta);
      tip.hidden = false;
      const x = Math.min(clientX + 14, (view.innerWidth || 1200) - tip.offsetWidth - 12);
      const y = Math.min(clientY + 16, (view.innerHeight || 800) - tip.offsetHeight - 12);
      tip.style.transform = `translate(${Math.max(8, x)}px, ${Math.max(8, y)}px)`;
      tip.style.left = "0px"; tip.style.top = "0px";
    }
    canvas.addEventListener("pointermove", (event) => {
      const node = nearest(event.clientX, event.clientY);
      setHover(node);
      showTip(node, event.clientX, event.clientY);
    });
    canvas.addEventListener("pointerleave", () => { setHover(null); showTip(null); });
    canvas.addEventListener("click", (event) => {
      const node = nearest(event.clientX, event.clientY);
      if (node && node.id !== options.focalId) { showTip(null); options.onOpen?.(node.id); }
    });

    const onVisibility = () => { if (doc.hidden) stop(); else start(); };
    doc.addEventListener?.("visibilitychange", onVisibility);
    const onTheme = () => { readColors(); if (!running) draw(0); };
    doc.addEventListener?.("kin-theme", onTheme);
    const scheme = view.matchMedia?.("(prefers-color-scheme: dark)");
    scheme?.addEventListener?.("change", onTheme);
    const resize = view.ResizeObserver ? new view.ResizeObserver(() => { fit(); if (!running) draw(0); }) : null;
    resize?.observe(host);
    const intersection = view.IntersectionObserver ? new view.IntersectionObserver((entries) => {
      visible = entries.some((entry) => entry.isIntersecting);
      if (visible) start(); else stop();
    }) : null;
    intersection?.observe(host);

    readColors();
    fit();
    start();

    return {
      dispose() {
        disposed = true;
        stop();
        resize?.disconnect();
        intersection?.disconnect();
        doc.removeEventListener?.("visibilitychange", onVisibility);
        doc.removeEventListener?.("kin-theme", onTheme);
        scheme?.removeEventListener?.("change", onTheme);
        if (tip) tip.hidden = true;
        canvas.remove(); labelLayer.remove();
      },
      get labelled() { return labels.filter((item) => item.node).map((item) => item.node.id); },
      /** Ring the entities an agent is working on, [[id, "looked" | "changing"], ...], and name them first. */
      agentMark(entries) {
        if (disposed) return;
        const next = new Map((Array.isArray(entries) ? entries : []).filter(([id]) => byId.has(id)));
        const same = next.size === marks.size && [...next].every(([id, tone]) => marks.get(id) === tone);
        if (same) return;
        marks = next;
        buildLabels();
        if (!running) draw(0);
      },
      get agentMarked() { return [...marks.keys()]; },
      /** Name a different set of entities first, without redrawing the map. */
      relabel(ids) {
        if (disposed) return;
        options.labelIds = Array.isArray(ids) ? ids : [];
        buildLabels();
        if (!running) draw(0);
      },
      /** Ring the entities in uncommitted work, and name them first. */
      mark(ids) {
        if (disposed) return;
        options.markIds = Array.isArray(ids) ? ids : [];
        buildLabels();
        if (!running) draw(0);
      },
    };
  }

  const api = { graphData, neighborRows, editorUri, mapFromGraph, mapFromContainers, layoutClusters, layoutFocus, placeLabels, createMap, CONTAINER_KINDS };
  root.KinWorkspaceGraph = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})(globalThis);
