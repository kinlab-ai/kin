// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

// Line diffs for the review page: how two recorded texts of one entity differ,
// drawn unified or side by side. Text is set with textContent only. Colour
// comes from the highlighter's tokens, and a view draws correctly without it.

"use strict";

(() => {
  const root = (globalThis.kinReview = globalThis.kinReview || {});

  const CONTEXT_LINES = 3;
  const FOLD_MIN = 8;
  const DIFF_CELLS_MAX = 4_000_000;

  function el(tag, className, ...children) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    for (const child of children.flat()) {
      if (child === null || child === undefined || child === false) continue;
      node.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return node;
  }

  function lines(text) {
    const out = text.split("\n");
    if (out[out.length - 1] === "") out.pop();
    return out;
  }

  /** The highlighter's tokens for each line of `text`, or null when there is none to ask. */
  function tokensFor(text, language) {
    if (typeof root.highlightLines !== "function") return null;
    try {
      return root.highlightLines(text, language);
    } catch (_) {
      // Colour is a convenience. A line the highlighter cannot read is shown plain.
      return null;
    }
  }

  /**
   * Rows of a line diff between two texts. Each row carries the index of its
   * line on each side, so a view can number it and find its tokens.
   */
  function diffRows(before, after) {
    const a = lines(before);
    const b = lines(after);

    let head = 0;
    while (head < a.length && head < b.length && a[head] === b[head]) head += 1;
    let tailA = a.length;
    let tailB = b.length;
    while (tailA > head && tailB > head && a[tailA - 1] === b[tailB - 1]) {
      tailA -= 1;
      tailB -= 1;
    }

    const rows = [];
    const equal = (i, j) => rows.push({ type: "eq", old: i, neu: j, text: b[j] });
    const removed = (i) => rows.push({ type: "del", old: i, neu: null, text: a[i] });
    const added = (j) => rows.push({ type: "add", old: null, neu: j, text: b[j] });

    for (let i = 0; i < head; i += 1) equal(i, i);

    const n = tailA - head;
    const m = tailB - head;
    if (n * m > DIFF_CELLS_MAX) {
      // Too large to align line by line. Every line is still shown.
      for (let i = head; i < tailA; i += 1) removed(i);
      for (let j = head; j < tailB; j += 1) added(j);
    } else {
      const width = m + 1;
      const table = new Uint32Array((n + 1) * width);
      for (let i = n - 1; i >= 0; i -= 1) {
        for (let j = m - 1; j >= 0; j -= 1) {
          table[i * width + j] =
            a[head + i] === b[head + j]
              ? table[(i + 1) * width + j + 1] + 1
              : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
        }
      }
      let i = 0;
      let j = 0;
      while (i < n && j < m) {
        if (a[head + i] === b[head + j]) {
          equal(head + i, head + j);
          i += 1;
          j += 1;
        } else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) {
          removed(head + i);
          i += 1;
        } else {
          added(head + j);
          j += 1;
        }
      }
      for (; i < n; i += 1) removed(head + i);
      for (; j < m; j += 1) added(head + j);
    }

    for (let i = tailA, j = tailB; i < a.length; i += 1, j += 1) equal(i, j);
    markWithinLines(rows);
    return rows;
  }

  /** Where a removed line and the added line that replaced it actually differ. */
  function markWithinLines(rows) {
    let index = 0;
    while (index < rows.length) {
      if (rows[index].type !== "del") {
        index += 1;
        continue;
      }
      let dels = index;
      while (dels < rows.length && rows[dels].type === "del") dels += 1;
      let adds = dels;
      while (adds < rows.length && rows[adds].type === "add") adds += 1;
      if (dels - index === adds - dels) {
        for (let k = 0; k < dels - index; k += 1) {
          const old = rows[index + k];
          const neu = rows[dels + k];
          let prefix = 0;
          const limit = Math.min(old.text.length, neu.text.length);
          while (prefix < limit && old.text[prefix] === neu.text[prefix]) prefix += 1;
          let suffix = 0;
          while (
            suffix < limit - prefix &&
            old.text[old.text.length - 1 - suffix] === neu.text[neu.text.length - 1 - suffix]
          ) {
            suffix += 1;
          }
          // Two lines that share less than half their text are different
          // lines, not one edit, and marking them would only add noise.
          if ((prefix + suffix) * 2 >= Math.max(old.text.length, neu.text.length)) {
            old.mark = [prefix, old.text.length - suffix];
            neu.mark = [prefix, neu.text.length - suffix];
          }
        }
      }
      index = adds;
    }
  }

  /**
   * One line of code as nodes: its tokens coloured, and the part an edit
   * changed wrapped in a mark. Tokens and the mark are two partitions of the
   * same line, so each token is cut where the mark begins and ends.
   */
  function codeCell(text, tokens, mark) {
    const cell = el("span", "code");
    const parts = tokens && tokens.length ? tokens : [["", text]];
    const [from, to] = mark && mark[1] > mark[0] ? mark : [-1, -1];
    let held = null;
    let at = 0;
    const put = (type, piece, marked) => {
      if (!piece) return;
      const node = type ? el("span", `t-${type}`, piece) : document.createTextNode(piece);
      if (marked) {
        if (!held) {
          held = el("mark");
          cell.append(held);
        }
        held.append(node);
      } else {
        held = null;
        cell.append(node);
      }
    };
    for (const [type, piece] of parts) {
      const end = at + piece.length;
      if (from < 0 || end <= from || at >= to) {
        put(type, piece, false);
      } else {
        put(type, piece.slice(0, Math.max(0, from - at)), false);
        put(type, piece.slice(Math.max(0, from - at), Math.min(piece.length, to - at)), true);
        put(type, piece.slice(Math.min(piece.length, to - at)), false);
      }
      at = end;
    }
    return cell;
  }

  function number(value) {
    const cell = el("span", "no", value === null ? "" : value);
    cell.setAttribute("aria-hidden", "true");
    return cell;
  }

  /** What a view needs to draw rows: where each side starts and its tokens. */
  function sides(before, after, language) {
    return {
      oldStart: before.start_line || 1,
      newStart: after.start_line || 1,
      oldTokens: tokensFor(before.text, language),
      newTokens: tokensFor(after.text, language),
    };
  }

  function unifiedRow(row, side) {
    const sign = row.type === "add" ? "+" : row.type === "del" ? "-" : " ";
    const tokens =
      row.type === "del" ? side.oldTokens && side.oldTokens[row.old] : side.newTokens && side.newTokens[row.neu];
    const signCell = el("span", "sign", sign);
    signCell.setAttribute("aria-hidden", "true");
    return el(
      "div",
      `row row-${row.type}`,
      number(row.old === null ? null : side.oldStart + row.old),
      number(row.neu === null ? null : side.newStart + row.neu),
      signCell,
      codeCell(row.text, tokens, row.mark),
    );
  }

  /** Which rows stay in view: every change and a few lines around it. */
  function kept(rows) {
    const keep = new Array(rows.length).fill(false);
    rows.forEach((row, index) => {
      if (row.type === "eq") return;
      const from = Math.max(0, index - CONTEXT_LINES);
      const to = Math.min(rows.length - 1, index + CONTEXT_LINES);
      for (let k = from; k <= to; k += 1) keep[k] = true;
    });
    return keep;
  }

  /** Draw `rows` with long unchanged runs folded behind a line that opens them. */
  function folded(view, rows, draw) {
    const keep = kept(rows);
    let index = 0;
    while (index < rows.length) {
      if (keep[index]) {
        view.append(...draw([rows[index]]));
        index += 1;
        continue;
      }
      let end = index;
      while (end < rows.length && !keep[end]) end += 1;
      const hidden = rows.slice(index, end);
      if (hidden.length < FOLD_MIN) {
        view.append(...draw(hidden));
      } else {
        const fold = el("button", "fold", `${hidden.length} unchanged lines`);
        fold.type = "button";
        fold.addEventListener("click", () => fold.replaceWith(...draw(hidden)));
        view.append(fold);
      }
      index = end;
    }
    return view;
  }

  function unifiedView(rows, side) {
    const view = el("div", "codeview diff");
    view.setAttribute("role", "group");
    view.setAttribute("tabindex", "0");
    view.setAttribute("aria-label", "Changed lines");
    return folded(view, rows, (some) => some.map((row) => unifiedRow(row, side)));
  }

  /**
   * Rows paired for a side-by-side view: an unchanged line on both sides, and
   * each run of removed lines beside the run of added lines that replaced it.
   */
  function pairs(rows) {
    const out = [];
    let index = 0;
    while (index < rows.length) {
      const row = rows[index];
      if (row.type === "eq") {
        out.push({ type: "eq", left: row, right: row });
        index += 1;
        continue;
      }
      const dels = [];
      const adds = [];
      while (index < rows.length && rows[index].type === "del") dels.push(rows[index++]);
      while (index < rows.length && rows[index].type === "add") adds.push(rows[index++]);
      for (let k = 0; k < Math.max(dels.length, adds.length); k += 1) {
        out.push({ type: "change", left: dels[k] || null, right: adds[k] || null });
      }
    }
    return out;
  }

  function splitRow(pair, side) {
    const half = (row, which) => {
      if (!row) return [number(null), el("span", "code code-empty")];
      const isOld = which === "old";
      const index = isOld ? row.old : row.neu;
      const tokens = isOld ? side.oldTokens && side.oldTokens[index] : side.newTokens && side.newTokens[index];
      return [number((isOld ? side.oldStart : side.newStart) + index), codeCell(row.text, tokens, row.type === "eq" ? null : row.mark)];
    };
    const line = el("div", `row row-split row-${pair.type}`);
    const left = el("div", `half ${pair.left && pair.type !== "eq" ? "half-del" : ""}`, half(pair.left, "old"));
    const right = el("div", `half ${pair.right && pair.type !== "eq" ? "half-add" : ""}`, half(pair.right, "new"));
    line.append(left, right);
    return line;
  }

  function splitView(rows, side) {
    const view = el("div", "codeview diff split");
    view.setAttribute("role", "group");
    view.setAttribute("tabindex", "0");
    view.setAttribute("aria-label", "Changed lines, side by side");
    const paired = pairs(rows);
    // Folding works on what a row is, so a pair counts as unchanged when both sides are.
    return folded(
      view,
      paired.map((pair) => ({ ...pair, type: pair.type === "eq" ? "eq" : "add", pair })),
      (some) => some.map((item) => splitRow(item.pair, side)),
    );
  }

  /** One recorded text with its line numbers. `isHit` marks the lines worth a second look. */
  function plainView(body, language, tone, isHit) {
    const text = lines(body.text);
    const tokens = tokensFor(body.text, language);
    const view = el("div", `codeview plain ${tone || ""}`);
    view.setAttribute("role", "group");
    view.setAttribute("aria-label", "Entity source");
    view.setAttribute("tabindex", "0");
    const start = body.start_line || 1;
    text.forEach((line, index) => {
      view.append(
        el(
          "div",
          isHit && isHit(line) ? "row row-hit" : "row",
          number(start + index),
          codeCell(line, tokens && tokens[index], null),
        ),
      );
    });
    return view;
  }

  root.diff = { rows: diffRows, sides, unifiedView, splitView, plainView };
})();
