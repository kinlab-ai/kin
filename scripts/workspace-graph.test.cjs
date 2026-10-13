// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const graphPath = path.resolve(__dirname, '../crates/kin-cli/assets/workspace_ui/graph.js');
const { editorUri, joinColumns, joinEdges } = require(graphPath);

const first = '11111111-1111-4111-8111-111111111111';
const second = '22222222-2222-4222-8222-222222222222';
const third = '33333333-3333-4333-8333-333333333333';
const intentPrefix = 'vscode://firelock.kin-editor/open-entity?';
const encodeQuery = value => encodeURIComponent(value).replace(/[!'()*]/g,
  character => `%${character.charCodeAt(0).toString(16).toUpperCase()}`);
const transportIntent = raw => intentPrefix + encodeQuery(raw.slice(intentPrefix.length));
const intent = `${intentPrefix}v=1&repository=team%2Frepo&workspace=${third}&root=%2Frepo&entity=${first}&scope=workspace`;

test('editor anchor accepts only one current-entity intent for the selected UUID', () => {
  assert.equal(editorUri(intent, first), transportIntent(intent));
  assert.equal(editorUri(transportIntent(intent), first), transportIntent(intent));
  const reordered = intentPrefix + intent.slice(intentPrefix.length).split('&').reverse().join('&');
  assert.equal(editorUri(reordered, first), transportIntent(reordered));
  assert.equal(editorUri(transportIntent(reordered), first), transportIntent(reordered));
  for (const raw of [
    'javascript:alert(1)', 'https://example.com', intent.replace('/open-entity', '/run'),
    intent + '&command=run', intent + '&entity=' + second, intent + '#fragment',
    intent.replace('scope=workspace', 'scope=change'), intent.replace('v=1', 'v=2'),
    intent.replace('firelock.kin-editor', 'another.extension'),
  ]) assert.equal(editorUri(raw, first), null, raw);
  assert.equal(editorUri(intent, second), null);
});

test('editor transport retains punctuation, literal escapes and Unicode through the platform decode', () => {
  for (const name of ['R&D', '100% coverage', 'repo%20name', 'repo%26name', 'hash#name',
    'equals=name', 'question?name', 'plus+name', '雪 repo', "punctuation!'()*"]) {
    const repository = 'org/' + name;
    const root = '/work/' + name;
    const query = `v=1&repository=${encodeQuery(repository)}&workspace=${third}&root=${encodeQuery(root)}&entity=${first}&scope=workspace`;
    const legacy = intentPrefix + query;
    const transport = intentPrefix + encodeQuery(query);
    for (const raw of [legacy, transport]) {
      const href = editorUri(raw, first);
      assert.equal(href, transport);
      // Model the platform's whole-query decode, then the handler's value decode.
      const inner = decodeURIComponent(href.slice(intentPrefix.length));
      const fields = new Map(inner.split('&').map(pair => {
        const equal = pair.indexOf('=');
        return [decodeURIComponent(pair.slice(0, equal)), decodeURIComponent(pair.slice(equal + 1))];
      }));
      assert.equal(fields.size, 6);
      assert.equal(fields.get('repository'), repository);
      assert.equal(fields.get('root'), root);
    }
  }
  const plus = intent.replace('team%2Frepo', 'team+repo');
  assert.equal(editorUri(plus, first), transportIntent(plus));
});

// Breaks exactly one place in a valid selector, the first occurrence of
// `from`, so each case below is wrong in one way only.
const corruptFirst = (text, from, to) => {
  const at = text.indexOf(from);
  assert.notEqual(at, -1, `${from} is not in ${text}`);
  return text.slice(0, at) + to + text.slice(at + from.length);
};

test('editor transport refuses malformed, ambiguous and historical selectors at either layer', () => {
  const query = intent.slice(intentPrefix.length);
  for (const inner of [
    query + '&entity=' + first, query + '&%72epository=other', query + '&at=old',
    query + '&command=run', query + '&unknown=value', query + '&broken',
    query.replace('scope=workspace', 'scope=change'), query.replace('v=1', 'v=2'),
    corruptFirst(query, 'team%2Frepo', ''), corruptFirst(query, 'team%2Frepo', '%ZZ'),
    corruptFirst(query, 'team%2Frepo', '%C3%28'), corruptFirst(query, 'team%2Frepo', '%00'),
    corruptFirst(query, 'team%2Frepo', '%0A'), corruptFirst(query, 'team%2Frepo', '%7F'),
    query.replace(third, 'not-a-workspace'),
  ]) {
    for (const raw of [intentPrefix + inner, intentPrefix + encodeQuery(inner)]) {
      assert.equal(editorUri(raw, first), null, raw);
    }
  }
  const transport = transportIntent(intent);
  for (const raw of [
    transport + '#', transport + '#fragment', transport + '&command=run',
    corruptFirst(transport, '%26repository', '&repository'), corruptFirst(transport, '%3D', '%3d'),
    corruptFirst(transport, '%3D', '%ZZ'), transportIntent(transport), intent + '#',
    corruptFirst(intent, 'team%2Frepo', 'team\nrepo'),
  ]) assert.equal(editorUri(raw, first), null, raw);
});


// ── Lines ──────────────────────────────────────────────────────────────────

/** A box with a place on the page, for a picture that measures the boxes it is given. */
class Box {
  constructor(tag, place = null) { this.tagName = tag; this.place = place; this.children = []; this.attributes = {}; this.isConnected = true; }
  setAttribute(key, value) { this.attributes[key] = String(value); }
  getAttribute(key) { return this.attributes[key] ?? null; }
  prepend(...children) { this.children.unshift(...children); }
  replaceChildren(...children) { this.children = children; }
  getBoundingClientRect() { const { left, top, width, height } = this.place; return { left, top, width, height, right: left + width, bottom: top + height }; }
}
const SVG = 'http://www.w3.org/2000/svg';

/**
 * graph.js in a page of its own: a document that makes boxes, an observer the
 * test fires, and frames that wait until the test runs them. A page may lack
 * either, as an older browser does.
 */
function page({ frames = true, observer = true, timers = true } = {}) {
  const observers = [];
  const waiting = [];
  const later = new Map();
  const context = { document: { createElementNS: (namespace, tag) => Object.assign(new Box(tag), { namespace }) } };
  if (observer) context.ResizeObserver = class { constructor(redraw) { this.redraw = redraw; this.watched = []; observers.push(this); } observe(node) { this.watched.push(node); } disconnect() { this.disconnected = true; } };
  if (frames) context.requestAnimationFrame = (run) => { waiting.push(run); };
  if (timers) { let next = 0; context.setTimeout = (run, delay) => { later.set(++next, { run, delay }); return next; }; context.clearTimeout = (id) => later.delete(id); }
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(fs.readFileSync(graphPath, 'utf8'), context, { filename: 'graph.js' });
  return { api: context.KinWorkspaceGraph, observers, later, frame() { for (const run of waiting.splice(0)) run(); },
    /** Let every timer the picture set run, as if its time had come. */
    wait() { for (const [id, { run }] of [...later]) { later.delete(id); run(); } } };
}

/** Three columns side by side inside a picture that starts 100 across and 50 down the page. */
function picture() {
  const host = new Box('div', { left: 100, top: 50, width: 800, height: 200 });
  const focus = new Box('div', { left: 450, top: 130, width: 100, height: 40 });
  const left = [new Box('a', { left: 100, top: 60, width: 200, height: 20 }), new Box('a', { left: 100, top: 100, width: 200, height: 20 })];
  const right = [new Box('a', { left: 700, top: 60, width: 200, height: 20 })];
  return { host, focus, left, right, lines: () => host.children[0].children.map((line) => line.getAttribute('d')) };
}

test('the workspace picture is offered to the page and to a script that requires it', () => {
  assert.equal(typeof editorUri, 'function');
  assert.equal(typeof joinColumns, 'function');
  assert.equal(typeof joinEdges, 'function');
  const { api } = page();
  assert.equal(typeof api.editorUri, 'function');
  assert.equal(typeof api.joinColumns, 'function');
  assert.equal(typeof api.joinEdges, 'function');
});

test('one line joins each box on the left to the entity, and the entity to each box on the right', () => {
  const { api, frame } = page();
  const { host, focus, left, right, lines } = picture();
  api.joinColumns(host, { left, focus, right });
  const svg = host.children[0];
  assert.equal(svg.tagName, 'svg', 'the lines go in before the boxes, so they sit behind them');
  assert.equal(svg.namespace, SVG);
  assert.equal(svg.getAttribute('aria-hidden'), 'true', 'the boxes say what is connected; the lines are for the eye');
  assert.deepEqual(lines(), [], 'nothing is measured until the boxes are laid out');
  frame();
  // Every point is inside the picture: the page's own offsets are taken out.
  assert.deepEqual(lines(), [
    'M200.0 20.0C275.0 20.0 275.0 100.0 350.0 100.0',
    'M200.0 60.0C275.0 60.0 275.0 100.0 350.0 100.0',
    'M450.0 100.0C525.0 100.0 525.0 20.0 600.0 20.0']);
  assert.equal(svg.getAttribute('viewBox'), '0 0 800.0 200.0');
  assert.ok(svg.children.every((line) => line.tagName === 'path' && line.namespace === SVG));
  // A side with nothing on it draws nothing for that side.
  const alone = picture();
  api.joinColumns(alone.host, { left: [], focus: alone.focus, right: alone.right });
  frame();
  assert.deepEqual(alone.lines(), ['M450.0 100.0C525.0 100.0 525.0 20.0 600.0 20.0']);
});

test('the lines are measured again when the picture changes size', () => {
  const { api, observers, frame } = page();
  const { host, focus, left, right, lines } = picture();
  api.joinColumns(host, { left, focus, right });
  frame();
  assert.equal(observers.length, 1);
  assert.deepEqual(observers[0].watched, [host]);
  host.place = { left: 100, top: 50, width: 1000, height: 200 };
  focus.place = { left: 550, top: 130, width: 100, height: 40 };
  right[0].place = { left: 900, top: 100, width: 200, height: 20 };
  observers[0].redraw();
  assert.deepEqual(lines(), [
    'M200.0 20.0C325.0 20.0 325.0 100.0 450.0 100.0',
    'M200.0 60.0C325.0 60.0 325.0 100.0 450.0 100.0',
    'M550.0 100.0C675.0 100.0 675.0 60.0 800.0 60.0']);
  assert.equal(host.children[0].getAttribute('viewBox'), '0 0 1000.0 200.0');
  assert.equal(host.children.length, 1, 'the same drawing is used again');
});

test('columns stacked one above another get no lines', () => {
  const { api, observers, frame } = page();
  const { host, focus, left, right, lines } = picture();
  api.joinColumns(host, { left, focus, right });
  frame();
  assert.equal(lines().length, 3);
  // On a narrow window the columns stack: a box on the left now reaches past the entity's left edge.
  host.place = { left: 0, top: 0, width: 300, height: 400 };
  focus.place = { left: 0, top: 120, width: 300, height: 40 };
  for (const [at, box] of [...left, ...right].entries()) box.place = { left: 0, top: at * 40, width: 300, height: 20 };
  observers[0].redraw();
  assert.deepEqual(lines(), [], 'the lines drawn for the wide layout are taken away');
  // The same holds when only the right column has dropped under the entity.
  left.forEach((box, at) => { box.place = { left: 0, top: at * 40, width: 100, height: 20 }; });
  focus.place = { left: 150, top: 0, width: 100, height: 40 };
  right[0].place = { left: 150, top: 80, width: 100, height: 20 };
  observers[0].redraw();
  assert.deepEqual(lines(), []);
  // Side by side again, they come back.
  right[0].place = { left: 280, top: 0, width: 20, height: 20 };
  observers[0].redraw();
  assert.equal(lines().length, 3);
});

test('nothing is drawn for a picture with no width or an entity that has left the page', () => {
  const { api, observers, frame } = page();
  const hidden = picture();
  hidden.host.place = { left: 0, top: 0, width: 0, height: 0 };
  api.joinColumns(hidden.host, hidden);
  frame();
  assert.deepEqual(hidden.lines(), []);
  assert.equal(hidden.host.children[0].getAttribute('viewBox'), null);
  const gone = picture();
  api.joinColumns(gone.host, gone);
  frame();
  assert.equal(gone.lines().length, 3);
  gone.focus.isConnected = false;
  gone.focus.place = null;
  assert.doesNotThrow(() => observers[1].redraw(), 'a late measurement of a page that moved on is ignored');
});

test('what joining returns stops the measuring', () => {
  const { api, observers, frame } = page();
  const { host, focus, left, right } = picture();
  const stop = api.joinColumns(host, { left, focus, right });
  frame();
  assert.notEqual(observers[0].disconnected, true);
  stop();
  assert.equal(observers[0].disconnected, true);
  // A browser with neither an observer nor frames still draws, once, and stopping is safe.
  const plain = page({ frames: false, observer: false });
  const still = picture();
  const stopPlain = plain.api.joinColumns(still.host, still);
  assert.equal(still.lines().length, 3, 'drawn at once');
  assert.doesNotThrow(stopPlain);
});

/** Two columns of a piece of work inside a picture that starts 100 across and 50 down: what changed on the left, what relies on it on the right. */
function work() {
  const host = new Box('div', { left: 100, top: 50, width: 800, height: 200 });
  const changed = [new Box('a', { left: 100, top: 60, width: 200, height: 20 }), new Box('a', { left: 100, top: 100, width: 200, height: 20 })];
  const reliers = [new Box('a', { left: 700, top: 60, width: 200, height: 20 }), new Box('a', { left: 700, top: 140, width: 200, height: 20 })];
  return { host, changed, reliers, lines: () => host.children[0].children.map((line) => [line.getAttribute('d'), line.getAttribute('data-tone')]) };
}

test('each pair in a piece of work is joined by one curve, in the tone the pair carries', () => {
  const { api, frame } = page();
  const { host, changed, reliers, lines } = work();
  api.joinEdges(host, [{ from: changed[0], to: reliers[0], tone: 'bad' }, { from: changed[0], to: reliers[1], tone: 'bad' }, { from: changed[1], to: reliers[0], tone: 'quiet' }, { from: changed[1], to: reliers[1] }]);
  const svg = host.children[0];
  assert.equal(svg.tagName, 'svg', 'the lines go in before the boxes, so they sit behind them');
  assert.equal(svg.namespace, SVG);
  assert.equal(svg.getAttribute('aria-hidden'), 'true');
  assert.deepEqual(lines(), [], 'nothing is measured until the boxes are laid out');
  frame();
  // From a box's right edge at its middle to the other's left edge at its middle, inside the picture.
  assert.deepEqual(lines(), [
    ['M200.0 20.0C400.0 20.0 400.0 20.0 600.0 20.0', 'bad'], ['M200.0 20.0C400.0 20.0 400.0 100.0 600.0 100.0', 'bad'],
    ['M200.0 60.0C400.0 60.0 400.0 20.0 600.0 20.0', 'quiet'], ['M200.0 60.0C400.0 60.0 400.0 100.0 600.0 100.0', null]], 'a pair with no tone is drawn plain');
  assert.equal(svg.getAttribute('viewBox'), '0 0 800.0 200.0');
  assert.ok(svg.children.every((line) => line.namespace === SVG && line.getAttribute('pathLength') === '1'), 'each line is measured as one length, so it can draw itself in');
});

test('a pair whose box has left the page is not joined, and stacked columns are joined by nothing', () => {
  const { api, observers, frame } = page();
  const { host, changed, reliers, lines } = work();
  api.joinEdges(host, [{ from: changed[0], to: reliers[0], tone: 'bad' }, { from: changed[1], to: reliers[1], tone: 'warn' }]);
  frame();
  assert.equal(lines().length, 2);
  reliers[1].isConnected = false;
  observers[0].redraw();
  assert.deepEqual(lines().map(([, tone]) => tone), ['bad']);
  // On a narrow window the right column drops under the left: a box on the left now reaches past one on the right.
  reliers[0].place = { left: 100, top: 160, width: 200, height: 20 };
  observers[0].redraw();
  assert.deepEqual(lines(), []);
  reliers[0].place = { left: 700, top: 60, width: 200, height: 20 };
  observers[0].redraw();
  assert.equal(lines().length, 1, 'side by side again, the line comes back');
  // A picture with no width yet is not measured.
  const hidden = work();
  hidden.host.place = { left: 0, top: 0, width: 0, height: 0 };
  api.joinEdges(hidden.host, [{ from: hidden.changed[0], to: hidden.reliers[0] }]);
  frame();
  assert.deepEqual(hidden.lines(), []);
  assert.equal(hidden.host.children[0].getAttribute('viewBox'), null);
});

test('the lines of a piece of work draw themselves in once, follow the picture as it changes size, and stop when asked', () => {
  const { api, observers, later, frame, wait } = page();
  const { host, changed, reliers, lines } = work();
  const stop = api.joinEdges(host, [{ from: changed[0], to: reliers[0], tone: 'bad' }]);
  frame();
  assert.deepEqual(observers[0].watched, [host]);
  assert.equal(host.children[0].getAttribute('data-drawn'), null);
  assert.deepEqual([...later.values()].map(({ delay }) => delay), [900]);
  wait();
  assert.equal(host.children[0].getAttribute('data-drawn'), '', 'once drawn in, the lines are marked so, and are not drawn in again');
  host.place = { left: 100, top: 50, width: 1000, height: 200 };
  reliers[0].place = { left: 900, top: 60, width: 200, height: 20 };
  observers[0].redraw();
  assert.deepEqual(lines(), [['M200.0 20.0C500.0 20.0 500.0 20.0 800.0 20.0', 'bad']]);
  assert.equal(host.children.length, 1, 'the same drawing is used again');
  stop();
  assert.equal(observers[0].disconnected, true);
  // Stopping before the lines have drawn themselves in cancels the mark.
  const early = work();
  const stopEarly = api.joinEdges(early.host, [{ from: early.changed[0], to: early.reliers[0] }]);
  stopEarly();
  assert.equal(later.size, 0);
  // A browser with no observer, frames or timers still draws, once, and stopping is safe.
  const plain = page({ frames: false, observer: false, timers: false });
  const still = work();
  const stopPlain = plain.api.joinEdges(still.host, [{ from: still.changed[0], to: still.reliers[0], tone: 'warn' }]);
  assert.deepEqual(still.lines(), [['M200.0 20.0C400.0 20.0 400.0 20.0 600.0 20.0', 'warn']], 'drawn at once');
  assert.doesNotThrow(stopPlain);
});
