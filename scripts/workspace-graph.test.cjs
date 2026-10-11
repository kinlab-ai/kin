// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC
const assert = require('node:assert/strict');
const test = require('node:test');
const { graphData, neighborRows, editorUri, mapFromGraph, mapFromContainers, layoutClusters, layoutFocus, placeLabels, createMap } = require('../crates/kin-cli/assets/workspace_ui/graph.js');

const first = '11111111-1111-4111-8111-111111111111';
const second = '22222222-2222-4222-8222-222222222222';
const third = '33333333-3333-4333-8333-333333333333';
const row = (id, name = id) => ({ id, name, kind: 'Function' });
function graph() {
  return {
    schema: 'kin.workspace.graph.v1', scope: 'workspace', repository_id: 'team/repo', workspace_id: third,
    graph: { root_hash: 'a'.repeat(64), seq: 3, nodes: [row(first, 'First'), row(second, 'Second')], links: [{ source: first, target: second, kind: 'Calls' }], entity_count: 12, relation_count: 20, unresolved_links: 4, filtered_links: 19, sampled: true },
    inventory: { repo_id: 'team/repo', total: 12, entities: [row(first, 'First'), row(second, 'Second')] },
    offset: 0, page_size: 50, read_at: 1, reconciliation_status: 'idle',
  };
}
const intentPrefix = 'vscode://firelock.kin-editor/open-entity?';
const encodeQuery = value => encodeURIComponent(value).replace(/[!'()*]/g,
  character => `%${character.charCodeAt(0).toString(16).toUpperCase()}`);
const transportIntent = raw => intentPrefix + encodeQuery(raw.slice(intentPrefix.length));
const intent = `${intentPrefix}v=1&repository=team%2Frepo&workspace=${third}&root=%2Frepo&entity=${first}&scope=workspace`;

test('graph keeps sample, withheld links and full inventory counts distinct', () => {
  const value = graphData(graph());
  assert.equal(value.graph.nodes.length, 2);
  assert.equal(value.graph.entity_count, 12);
  assert.equal(value.graph.unresolved_links, 4);
  assert.equal(value.inventory.total, 12);
  assert.equal(value.graph.sampled, true);
});

test('graph refuses mismatched repository, incomplete counts and absent endpoints', () => {
  const wrongRepo = graph(); wrongRepo.inventory.repo_id = 'another/repo';
  assert.throws(() => graphData(wrongRepo), /left out/);
  const badScope = graph(); badScope.scope = 'at';
  assert.throws(() => graphData(badScope), /form this page/);
  const unknown = graph(); unknown.graph.links[0].target = third;
  assert.throws(() => graphData(unknown), /both of its entities/);
  const noCounts = graph(); delete noCounts.graph.unresolved_links;
  assert.throws(() => graphData(noCounts), /left out its counts/);
  const duplicate = graph(); duplicate.graph.nodes.push(row(first));
  assert.throws(() => graphData(duplicate), /identities/);
});

test('neighborhood keeps only direct edges and discloses external and candidate endpoints', () => {
  const rows = neighborRows({
    focal_id: first, truncated: true, entities: [row(first), row(second, 'Callee'), row(third)],
    relations: [
      { src: { Entity: first }, dst: { Entity: second }, kind: 'Calls', resolution: 'name_only' },
      { src: { Entity: second }, dst: { Entity: third }, kind: 'Calls' },
      { src: { External: 'outside' }, dst: { Entity: first }, kind: 'Imports' },
    ],
  }, first);
  assert.equal(rows.length, 2);
  assert.equal(rows[0].name, 'Callee');
  assert.equal(rows[0].resolution, 'name_only');
  assert.equal(rows[0].direction, 'outgoing');
  assert.equal(rows[1].name, 'External');
  assert.equal(rows[1].id, undefined);
  assert.equal(rows[1].direction, 'incoming');
  assert.throws(() => neighborRows({ focal_id: second, entities: [], relations: [], truncated: false }, first), /different entity/);
});

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

test('editor transport refuses malformed, ambiguous and historical selectors at either layer', () => {
  const query = intent.slice(intentPrefix.length);
  for (const inner of [
    query + '&entity=' + first, query + '&%72epository=other', query + '&at=old',
    query + '&command=run', query + '&unknown=value', query + '&broken',
    query.replace('scope=workspace', 'scope=change'), query.replace('v=1', 'v=2'),
    query.replace('team%2Frepo', ''), query.replace('team%2Frepo', '%ZZ'),
    query.replace('team%2Frepo', '%C3%28'), query.replace('team%2Frepo', '%00'),
    query.replace('team%2Frepo', '%0A'), query.replace('team%2Frepo', '%7F'),
    query.replace(third, 'not-a-workspace'),
  ]) {
    for (const raw of [intentPrefix + inner, intentPrefix + encodeQuery(inner)]) {
      assert.equal(editorUri(raw, first), null, raw);
    }
  }
  const transport = transportIntent(intent);
  for (const raw of [
    transport + '#', transport + '#fragment', transport + '&command=run',
    transport.replace('%26repository', '&repository'), transport.replace('%3D', '%3d'),
    transport.replace('%3D', '%ZZ'), transportIntent(transport), intent + '#',
    intent.replace('team%2Frepo', 'team\nrepo'),
  ]) assert.equal(editorUri(raw, first), null, raw);
});


// ── Map model ──────────────────────────────────────────────────────────────

const uuid = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const node = (n, name, kind, degree = 1) => ({ id: uuid(n), name, kind, degree, file: 'never shown' });

test('the graph sample clusters entities by the module that contains them and never draws a module as a dot', () => {
  const graph = {
    nodes: [node(1, 'signer', 'Module'), node(2, 'Signer', 'Class', 30), node(3, 'Signer.sign', 'Method', 9), node(4, 'want_bytes', 'Function', 20),
      node(5, 'encoding', 'Module'), node(6, 'loose', 'Function', 2)],
    links: [
      { source: uuid(1), target: uuid(2), kind: 'Contains' }, { source: uuid(3), target: uuid(2), kind: 'Contains' },
      { source: uuid(4), target: uuid(5), kind: 'Contains' }, { source: uuid(3), target: uuid(4), kind: 'Calls' },
      { source: uuid(4), target: uuid(3), kind: 'Calls' }, { source: uuid(2), target: uuid(5), kind: 'Imports' },
    ],
  };
  const model = mapFromGraph(graph);
  assert.deepEqual(model.nodes.map((row) => row.name), ['Signer', 'want_bytes', 'Signer.sign', 'loose']);
  const group = (name) => model.nodes.find((row) => row.name === name).group;
  assert.equal(group('Signer'), uuid(1));
  assert.equal(group('Signer.sign'), uuid(1), 'a method reaches its module through its class');
  assert.equal(group('want_bytes'), uuid(5));
  assert.equal(group('loose'), '');
  assert.deepEqual(model.groups.find((row) => row.id === uuid(1)), { id: uuid(1), name: 'signer', size: 2 });
  assert.equal(model.edges.length, 1, 'containment is closeness, and a pair is drawn once');
  assert.ok(!JSON.stringify(model).includes('never shown'), 'no location reaches the drawable model');
});

test('the server map turns top-level containers into named clusters and their edges into ties', () => {
  const map = {
    schema: 'kin.workspace.map.v1',
    nodes: [
      { id: uuid(1), name: 'signer', kind: 'module', role: 'container', size: 30, degree: 11, parent: null, group: uuid(1) },
      { id: uuid(2), name: 'Signer', kind: 'class', role: 'entity', degree: 34, parent: uuid(1), group: uuid(1) },
      { id: uuid(3), name: 'exc', kind: 'module', role: 'container', size: 12, degree: 6, parent: null, group: uuid(3) },
      { id: uuid(4), name: 'want_bytes', kind: 'function', role: 'entity', degree: 20, parent: null, group: uuid(5) },
      { id: uuid(5), name: 'encoding', kind: 'module', role: 'container', size: 6, degree: 3, parent: null, group: uuid(5) },
    ],
    edges: [
      { source: uuid(1), target: uuid(5), level: 'container', weight: 14, kinds: { calls: 14 } },
      { source: uuid(2), target: uuid(4), level: 'entity', weight: 1, kinds: { calls: 1 } },
      { source: uuid(2), target: uuid(9), level: 'entity', weight: 1, kinds: { calls: 1 } },
    ],
  };
  const model = mapFromContainers(map);
  assert.deepEqual(model.nodes.filter((row) => !row.container).map((row) => row.name), ['Signer', 'want_bytes']);
  const lone = model.nodes.find((row) => row.container);
  assert.equal(lone.name, 'exc', 'a container with no entity shown still has a place');
  assert.deepEqual(model.groups.map((row) => [row.name, row.size]).sort(), [['encoding', 1], ['exc', 1], ['signer', 1]]);
  assert.deepEqual(model.ties, [{ source: uuid(1), target: uuid(5), weight: 14 }]);
  assert.equal(model.edges.length, 1, 'an edge to an entity that is not drawn is dropped');
});

function overlaps(groups) {
  for (let i = 0; i < groups.length; i++) for (let j = i + 1; j < groups.length; j++) {
    const a = groups[i], b = groups[j];
    if (Math.hypot(a.x - b.x, a.y - b.y) < a.r + b.r - 1e-6) return [a.id, b.id];
  }
  return null;
}

test('clusters fill a wide stage without overlapping and lay out the same way every time', () => {
  const nodes = [], edges = [];
  const sizes = [26, 20, 20, 16, 14, 11, 10, 6, 6, 5, 3, 3];
  let n = 1;
  sizes.forEach((size, g) => { for (let i = 0; i < size; i++) nodes.push({ id: uuid(n++), name: `e${n}`, kind: 'method', weight: size - i, group: `g${g}` }); });
  for (let i = 1; i < nodes.length; i += 3) edges.push({ source: nodes[i].id, target: nodes[(i * 7) % nodes.length].id });
  const model = { nodes, edges };
  const one = layoutClusters(model, 2.5);
  const two = layoutClusters(model, 2.5);
  assert.deepEqual([...one.positions], [...two.positions]);
  assert.equal(overlaps(one.groups), null);
  let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
  for (const g of one.groups) { minX = Math.min(minX, g.x - g.r); maxX = Math.max(maxX, g.x + g.r); minY = Math.min(minY, g.y - g.r); maxY = Math.max(maxY, g.y + g.r); }
  assert.ok((maxX - minX) / (maxY - minY) >= 2.5 * 0.85, `aspect ${(maxX - minX) / (maxY - minY)}`);
  for (const g of one.groups) {
    for (const member of nodes.filter((row) => row.group === g.id)) {
      const p = one.positions.get(member.id);
      assert.ok(Math.hypot(p.x - g.x, p.y - g.y) <= g.r + 1e-6, 'every entity sits inside its own cluster');
    }
  }
});

test('the focus layout puts what relies on an entity left, what it relies on right, and bounds each side', () => {
  const focal = uuid(1);
  const nodes = [{ id: focal, name: 'Focal', role: 'focal', weight: 0 }];
  for (let i = 0; i < 15; i++) nodes.push({ id: uuid(100 + i), name: `caller${i}`, role: i < 10 ? 'test' : 'in', weight: 0 });
  nodes.push({ id: uuid(200), name: 'callee', role: 'out', weight: 0 });
  const { positions } = layoutFocus({ nodes }, focal, 12);
  assert.deepEqual(positions.get(focal), { x: 0, y: 0 });
  const left = [...positions].filter(([id, p]) => id !== focal && p.x < 0);
  assert.equal(left.length, 12);
  assert.ok(positions.get(uuid(200)).x > 0);
  assert.ok(!positions.has(uuid(113)) && positions.has(uuid(111)), 'the caller decides which twelve are shown');
  for (let i = 0; i < 10; i++) assert.ok(positions.get(uuid(110)).y < positions.get(uuid(100 + i)).y, 'entities other than tests sit above tests');
  const ys = left.map(([, p]) => p.y).sort((a, b) => a - b);
  for (let i = 1; i < ys.length; i++) assert.ok(ys[i] - ys[i - 1] >= 29, 'rows keep room for their names');
});

test('labels are kept in priority order, inside the bounds and clear of each other', () => {
  const bounds = { left: 0, top: 0, right: 300, bottom: 200 };
  const box = (id, left, top, extra = {}) => ({ id, left, top, right: left + 80, bottom: top + 20, ...extra });
  const shown = placeLabels([box('a', 10, 10), box('b', 50, 15), box('c', 10, 60), box('d', 260, 60), box('e', 50, 12, { force: true })], bounds);
  assert.deepEqual([...shown], ['a', 'c', 'e']);
});

class Fake {
  constructor(tag, doc) { this.tagName = tag.toUpperCase(); this.ownerDocument = doc; this.children = []; this.attributes = {}; this.dataset = {}; this.events = {}; this.style = {}; this.text = ''; this.parentNode = null; }
  set className(value) { this.attributes.class = value; }
  get className() { return this.attributes.class || ''; }
  set textContent(value) { this.text = String(value); this.children = []; }
  get textContent() { return this.text + this.children.map((child) => child.textContent).join(''); }
  setAttribute(key, value) { this.attributes[key] = String(value); }
  getAttribute(key) { return this.attributes[key] ?? null; }
  append(...children) { for (const child of children) { child.parentNode = this; this.children.push(child); } }
  replaceChildren(...children) { this.children = []; this.append(...children); }
  remove() { if (this.parentNode) this.parentNode.children = this.parentNode.children.filter((child) => child !== this); this.parentNode = null; }
  addEventListener(type, handler) { (this.events[type] ||= []).push(handler); }
  removeEventListener() {}
  emit(type, values = {}) { for (const handler of this.events[type] || []) handler({ type, ...values }); }
  getBoundingClientRect() { return { left: 0, top: 0, width: 1000, height: 420 }; }
}

test('the map renderer names entities with real buttons, opens them, and cleans up after itself', () => {
  const doc = { createElement: (tag) => new Fake(tag, doc), addEventListener() {}, removeEventListener() {} };
  const host = new Fake('div', doc);
  doc.defaultView = { devicePixelRatio: 1, innerWidth: 1200, innerHeight: 800 };
  const nodes = [];
  for (let i = 1; i <= 30; i++) nodes.push({ id: uuid(i), name: `entity_${i}`, kind: 'function', weight: 31 - i, group: i % 3 ? 'a' : 'b' });
  const opened = [];
  const controller = createMap(host, { nodes, edges: [{ source: uuid(1), target: uuid(2) }], groups: [{ id: 'a', name: 'alpha' }, { id: 'b', name: 'beta' }] },
    { labelCount: 4, labelIds: [uuid(30)], onOpen: (id) => opened.push(id) });
  const layer = host.children.find((child) => child.className === 'map-labels');
  const buttons = layer.children.filter((child) => child.tagName === 'BUTTON');
  assert.ok(buttons.length > 0 && buttons.length <= 4);
  assert.equal(buttons[0].textContent, 'entity_30', 'the most relied-on list decides which names are shown first');
  assert.match(buttons[0].getAttribute('aria-label'), /entity_30, function\. Open\./);
  buttons[0].emit('click');
  assert.deepEqual(opened, [uuid(30)]);
  assert.equal(host.children.find((child) => child.tagName === 'CANVAS').getAttribute('role'), 'img');
  controller.dispose();
  assert.equal(host.children.length, 0);
});
