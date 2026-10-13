// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// The workspace's one picture: an entity between what relies on it and what
// it relies on, drawn flat. Each box is an entity a graph read returned and
// each line a relationship that read named; nothing here invents either.
(function (root) {
  "use strict";

  const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
  const SVG = "http://www.w3.org/2000/svg";

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

  /**
   * Join the boxes of a three-column picture with lines: every box in the
   * left column to the centre, and the centre to every box on the right. The
   * lines are measured from the laid-out boxes, so they are drawn again when
   * the picture changes size. Returns what stops the redrawing.
   */
  function joinColumns(host, { left, focus, right }) {
    const svg = document.createElementNS(SVG, "svg");
    svg.setAttribute("class", "conn-lines");
    svg.setAttribute("aria-hidden", "true");
    host.prepend(svg);
    const draw = () => {
      const frame = host.getBoundingClientRect();
      if (!frame.width || !focus.isConnected) return;
      const centre = focus.getBoundingClientRect();
      const paths = [];
      const curve = (x1, y1, x2, y2) => {
        const bend = (x2 - x1) / 2;
        return `M${x1.toFixed(1)} ${y1.toFixed(1)}C${(x1 + bend).toFixed(1)} ${y1.toFixed(1)} ${(x2 - bend).toFixed(1)} ${y2.toFixed(1)} ${x2.toFixed(1)} ${y2.toFixed(1)}`;
      };
      const middle = centre.top + centre.height / 2 - frame.top;
      for (const node of left) {
        const box = node.getBoundingClientRect();
        // Stacked columns on a narrow window have no room for lines.
        if (box.right > centre.left) return svg.replaceChildren();
        paths.push(curve(box.right - frame.left, box.top + box.height / 2 - frame.top, centre.left - frame.left, middle));
      }
      for (const node of right) {
        const box = node.getBoundingClientRect();
        if (box.left < centre.right) return svg.replaceChildren();
        paths.push(curve(centre.right - frame.left, middle, box.left - frame.left, box.top + box.height / 2 - frame.top));
      }
      svg.setAttribute("viewBox", `0 0 ${frame.width.toFixed(1)} ${frame.height.toFixed(1)}`);
      svg.replaceChildren(...paths.map((d) => {
        const path = document.createElementNS(SVG, "path");
        path.setAttribute("d", d);
        return path;
      }));
    };
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(draw) : null;
    observer?.observe(host);
    if (root.requestAnimationFrame) root.requestAnimationFrame(draw); else draw();
    return () => observer?.disconnect();
  }

  /**
   * Join pairs of boxes anywhere inside a picture with one curve each, in the
   * tone the pair carries. Measured from the laid-out boxes like the columns
   * above, and drawn again when the picture changes size.
   */
  function joinEdges(host, edges) {
    const svg = document.createElementNS(SVG, "svg");
    svg.setAttribute("class", "impact-lines");
    svg.setAttribute("aria-hidden", "true");
    host.prepend(svg);
    const draw = () => {
      const frame = host.getBoundingClientRect();
      if (!frame.width) return;
      const paths = [];
      for (const edge of edges) {
        if (!edge.from?.isConnected || !edge.to?.isConnected) continue;
        const a = edge.from.getBoundingClientRect();
        const b = edge.to.getBoundingClientRect();
        // Stacked columns on a narrow window have no room for lines.
        if (a.right > b.left) return svg.replaceChildren();
        const x1 = a.right - frame.left, y1 = a.top + a.height / 2 - frame.top;
        const x2 = b.left - frame.left, y2 = b.top + b.height / 2 - frame.top;
        const bend = (x2 - x1) / 2;
        const path = document.createElementNS(SVG, "path");
        path.setAttribute("d", `M${x1.toFixed(1)} ${y1.toFixed(1)}C${(x1 + bend).toFixed(1)} ${y1.toFixed(1)} ${(x2 - bend).toFixed(1)} ${y2.toFixed(1)} ${x2.toFixed(1)} ${y2.toFixed(1)}`);
        path.setAttribute("pathLength", "1");
        if (edge.tone) path.setAttribute("data-tone", edge.tone);
        paths.push(path);
      }
      svg.setAttribute("viewBox", `0 0 ${frame.width.toFixed(1)} ${frame.height.toFixed(1)}`);
      svg.replaceChildren(...paths);
    };
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(draw) : null;
    observer?.observe(host);
    if (root.requestAnimationFrame) root.requestAnimationFrame(draw); else draw();
    // The lines draw themselves in once, not again each time the window moves.
    const settled = root.setTimeout ? root.setTimeout(() => svg.setAttribute("data-drawn", ""), 900) : null;
    return () => { observer?.disconnect(); if (settled) root.clearTimeout(settled); };
  }

  const api = { editorUri, joinColumns, joinEdges };
  root.KinWorkspaceGraph = api;
  if (typeof module !== "undefined" && module.exports) module.exports = api;
})(globalThis);
