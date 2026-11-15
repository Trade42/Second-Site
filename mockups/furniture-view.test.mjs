// node mockups/furniture-view.test.mjs: the furnish view's models, selection, moving, turning and fit
// checks, with a minimal THREE stub. With THREE_MODULE=<path to three.module.js r161> (and
// ROUNDED_BOX=<path to examples/jsm/geometries/RoundedBoxGeometry.js>) the same tests run again against
// the real library, plus the pointer tests (real Raycaster) and Box3 bounds.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import * as F from './furniture.js';
import * as V from './furniture-view.js';

let passed = 0, failed = 0;
async function test(name, fn) {
  try { await fn(); passed++; console.log(`ok   ${name}`); } catch (e) { failed++; console.log(`FAIL ${name}\n     ${e.stack.split('\n').slice(0, 9).join('\n     ')}`); }
}
const near = (a, b, msg, tol = 1e-3) => assert.ok(Math.abs(a - b) <= tol, `${msg}: ${a} vs ${b}`);

/* ---- the page's own code, read from workspace.js so a change there fails here */
const src = readFileSync(new URL('./workspace.js', import.meta.url), 'utf8');
const CAT = new Function(`return ${src.slice(src.indexOf('const CAT = [') + 'const CAT = '.length, src.indexOf('const C = ')).trim().replace(/;$/, '')}`)();
const pageFns = src.slice(src.indexOf('function pieceRect('), src.indexOf('function paintPlan('));
// pieces() with the module's planRect, as the page calls it (FV?.planRect).
const makePieces = (S, C, FV) => new Function('S', 'C', 'FV', 'clamp', 'keyOf', `${pageFns}; return pieces;`)(S, C, FV, (v, a, b) => Math.max(a, Math.min(b, v)), it => `${it.c}|${it.r}`);
const BOX_LINE = 'mesh.position.set(d.plan.mesh[0] + (q.cx - d.plan.ink[0]) / 100, hgt / 2 + .02, -(d.plan.mesh[1] - (q.cy - d.plan.ink[1]) / 100));';
const boxOf = (d, q, C) => { const h = C[q.it.c].h * .3048, x = d.plan.mesh[0] + (q.cx - d.plan.ink[0]) / 100, z = -(d.plan.mesh[1] - (q.cy - d.plan.ink[1]) / 100); return { min: [x - q.w / 200, .02, z - q.h / 200], max: [x + q.w / 200, .02 + h, z + q.h / 200] }; };

/* ---- a minimal THREE stub (as in furniture.test.mjs) */
function stub() {
  const vec = () => ({ x: 0, y: 0, z: 0, set(x, y, z) { this.x = x; this.y = y; this.z = z; return this; } });
  class Object3D {
    constructor() { this.position = vec(); this.rotation = vec(); this.scale = vec().set(1, 1, 1); this.children = []; this.parent = null; this.userData = {}; }
    add(o) { o.parent?.remove(o); o.parent = this; this.children.push(o); return this; }
    remove(o) { this.children = this.children.filter(c => c !== o); o.parent = null; return this; }
    clear() { this.children.forEach(c => { c.parent = null; }); this.children = []; return this; }
  }
  class Geometry { dispose() {} }
  return {
    Group: class extends Object3D {},
    Mesh: class extends Object3D { constructor(g, m) { super(); this.geometry = g; this.material = m; this.isMesh = true; } },
    BoxGeometry: class extends Geometry {}, CylinderGeometry: class extends Geometry {}, SphereGeometry: class extends Geometry {},
    MeshStandardMaterial: class { constructor(p) { Object.assign(this, p); } dispose() {} },
  };
}

/* ---- a minimal DOM: the dollhouse layer, its canvas and tool group, the parts list */
function makeDoc() {
  const doc = { listeners: {}, addEventListener(t, f) { (this.listeners[t] ||= []).push(f); }, createElement: tag => el(tag) };
  function el(tag, props = {}) {
    const L = {}, C = {};
    const e = Object.assign({ tagName: tag, children: [], dataset: {}, hidden: false, className: '', textContent: '', ownerDocument: doc, parentElement: null, isConnected: true, attrs: {},
      addEventListener(t, f, cap) { ((cap ? C : L)[t] ||= []).push(f); },
      fire(t, ev, cap) { for (const f of (cap ? C : L)[t] || []) f(ev); },
      append(...k) { for (const x of k) { x.parentElement = e; e.children.push(x); } },
      prepend(...k) { for (const x of k) x.parentElement = e; e.children.unshift(...k); },
      setAttribute(k, v) { e.attrs[k] = v; },
      all() { return e.children.flatMap(c => [c, ...c.all()]); },
      querySelector(s) { return e.querySelectorAll(s)[0] || null; },
      querySelectorAll(s) { return e.all().filter(s === 'tr[data-key]' ? c => c.tagName === 'tr' && c.dataset.key : s.startsWith('.') ? c => c.className.split(' ').includes(s.slice(1)) : c => c.tagName === s); },
      closest: () => null, getBoundingClientRect: () => ({ left: 0, top: 0, width: 200, height: 200 }), setPointerCapture() {}, releasePointerCapture() {} }, props);
    return e;
  }
  const host = el('div'), canvas = el('canvas'), tools = el('div', { className: 'dollhouse-tools' });
  tools.append(el('button', { textContent: '-' }), el('button', { textContent: '+' }), el('button', { textContent: 'Reset view' }));
  host.append(canvas, tools);
  const dispatch = (target, type, ev) => { ev = { button: 0, pointerId: 1, ...ev, target, stopPropagation() { ev.stopped = true; } }; host.fire(type, ev, true); if (!ev.stopped) target.fire(type, ev); return ev; };
  const key = ev => { for (const f of doc.listeners.keydown || []) f({ target: el('body'), ...ev, preventDefault() {} }); };
  return { doc, el, host, canvas, tools, dispatch, key };
}

/* ---- a plan with two rooms (100 px per metre) and the page's demo catalog */
const d = {
  plan: { w: 1000, h: 600, ink: [20, 30], mesh: [-5, 3] },
  rooms: [
    { id: 'rc', type: 'reception', name: 'Reception', r: [.05, .1, .5, .9] }, // 4.5 m wide, 4.8 m tall: long items turn
    { id: 't1', type: 'treatment', name: 'Treatment 1', r: [.55, .1, .95, .5] }, // 4 m by 2.4 m
  ],
};
function setup(THREE, extra = {}) {
  V._reset();
  const C = Object.fromEntries(CAT.map(c => [c.id, { ...c }]));
  C['treatment_bed@treatment'] = { id: 'treatment_bed@treatment', room: 'treatment', name: 'Treatment bed, planning price', w: 0, d: 0, h: 0, at: [[0.5, 0.5]] };
  C['flooring_lvp_sqm@x1'] = { id: 'flooring_lvp_sqm@x1', room: 'treatment', name: 'Flooring lvp sqm, 12 m2', w: 0, d: 0, h: 0, at: [[0.5, 0.5]] };
  const S = { stage: 'furnish', sel: null, placed: [
    { c: 'desk', r: 'rc', st: 'placed' }, { c: 'bench', r: 'rc', st: 'placed' }, { c: 'bench', r: 'rc', st: 'placed' },
    { c: 'bed', r: 't1', st: 'placed' }, { c: 'stool', r: 't1', st: 'placed' }, { c: 'lamp', r: 't1', st: 'proposed' },
    ...(extra.nominal ? [{ c: 'treatment_bed@treatment', r: 't1', st: 'placed' }, { c: 'flooring_lvp_sqm@x1', r: 't1', st: 'placed' }] : []),
  ] };
  const dom = makeDoc(), saves = [];
  const blocks = new THREE.Group(), scene = THREE.Scene ? new THREE.Scene() : new THREE.Group(); scene.add(blocks);
  let camera = { isCamera: true };
  if (THREE.OrthographicCamera) { camera = new THREE.OrthographicCamera(-10, 10, 10, -10, .1, 400); camera.position.set(0, 50, 0); camera.rotation.x = -Math.PI / 2; camera.updateProjectionMatrix(); camera.updateMatrixWorld(); }
  const controls = { enabled: true, log: [] };
  const T = { THREE, scene, blocks, camera, controls, renderer: { domElement: dom.canvas } };
  const ctx = { S, C, T, dom, saves, pieces: makePieces(S, C, V) };
  ctx.paint = () => {
    const list = ctx.pieces(d), fv = V.paint(T, d, list);
    if (!fv) { T.blocks.clear(); list.forEach(q => { const m = new THREE.Group(); m.userData.key = q.key; T.blocks.add(m); }); } // the page's boxes
    return fv;
  };
  ctx.selectPiece = key => { S.sel = S.sel === key ? null : key; ctx.paint(); };
  V.bind({ S: () => S, C, save: () => saves.push(JSON.stringify(S.placed)), selectPiece: ctx.selectPiece, renderViewer: ctx.paint, load: extra.load });
  return ctx;
}
const groups = T => T.blocks.children.filter(o => o.userData.furniture);
const groupOf = (T, id) => groups(T).find(g => g.userData.pieceId === id);
const entryOf = id => V._view().entries.find(e => e.id === id);

async function suite(THREE, label, real) {
  const load = real && process.env.ROUNDED_BOX ? () => import(pathToFileURL(process.env.ROUNDED_BOX)) : undefined;
  const tick = () => new Promise(r => setTimeout(r, 0));
  const fresh = async (extra = {}) => { const ctx = setup(THREE, { load, ...extra }); await tick(); return ctx; };

  await test(`${label}: each model sits where the page's box sits (centre and footprint, 1 mm), two rooms`, async () => {
    const ctx = await fresh();
    assert.ok(src.includes(BOX_LINE), 'the page still places boxes with the formula this test copies');
    assert.equal(ctx.paint(), true);
    const list = ctx.pieces(d);
    assert.equal(groups(ctx.T).length, list.length);
    const seen = {};
    for (const q of list) {
      const n = (seen[q.key] = (seen[q.key] ?? -1) + 1), g = groupOf(ctx.T, `${q.key}#${n}`), e = entryOf(`${q.key}#${n}`), box = boxOf(d, q, ctx.C);
      near(g.position.x, (box.min[0] + box.max[0]) / 2, `${q.key} centre x`); near(g.position.z, (box.min[2] + box.max[2]) / 2, `${q.key} centre z`);
      const fp = F.footprint({ x: e.x, z: e.z, width: e.size.width, depth: e.size.depth, rotation: e.rotation });
      near(Math.min(...fp.map(p => p[0])), box.min[0], `${q.key} min x`); near(Math.max(...fp.map(p => p[0])), box.max[0], `${q.key} max x`);
      near(Math.min(...fp.map(p => p[1])), box.min[2], `${q.key} min z`); near(Math.max(...fp.map(p => p[1])), box.max[2], `${q.key} max z`);
      if (real) {
        const b = new THREE.Box3().setFromObject(g);
        for (const i of [0, 1, 2]) { near(b.min.getComponent(i), box.min[i], `${q.key} Box3 min ${i}`); near(b.max.getComponent(i), box.max[i], `${q.key} Box3 max ${i}`); }
      }
    }
    assert.ok(list.some(q => entryOf(`${q.key}#0`).rotation > 0), 'at least one piece is turned to follow its room (bench in the tall reception)');
    assert.equal(groupOf(ctx.T, 'lamp|t1#0').userData.state, 'proposed', 'a proposed piece is drawn proposed');
  });

  await test(`${label}: select, move, turn and snap; a stored move survives a repaint and a reload`, async () => {
    const ctx = await fresh(); ctx.paint();
    ctx.selectPiece('bed|t1');
    assert.equal(groupOf(ctx.T, 'bed|t1#0').userData.state, 'selected');
    const e = entryOf('bed|t1#0'), x0 = e.x, z0 = e.z, r0 = e.rotation;
    assert.deepEqual(V.moveTo('bed|t1#0', x0, z0, 0.26, -0.04), [], 'a small move inside the room fits');
    near(e.x, x0 + 0.3, 'x snapped to 0.1 m steps', 1e-9); near(e.z, z0, 'z snapped', 1e-9);
    assert.equal(ctx.saves.length, 0, 'nothing stored while dragging');
    assert.deepEqual(V.drop('bed|t1#0'), []);
    assert.equal(ctx.saves.length, 1, 'stored on release');
    assert.deepEqual(ctx.S.placed[3].pos, { x: e.x, z: e.z, rotation: r0 });
    const turns = [1, 2, 3, 4, 5, 6].map(() => V.turn(null, -1)); // R six times: 90° clockwise
    assert.deepEqual(turns[0], ['overlaps Stool', 'overlaps Task lamp'], 'turned 15°, its corners reach the stool and the lamp: said, kept, not stored');
    assert.deepEqual(turns[5], [], 'a quarter turn fits');
    assert.equal(ctx.saves.length, 2, 'only the turn that fits is stored');
    near(entryOf('bed|t1#0').rotation, ((r0 - Math.PI / 2) % (2 * Math.PI) + 2 * Math.PI) % (2 * Math.PI), 'turned 6 × 15° clockwise', 1e-9);
    const stored = ctx.S.placed[3].pos; near(stored.rotation, entryOf('bed|t1#0').rotation, 'stored turned', 1e-12);
    for (let i = 0; i < 3; i++) ctx.paint();
    const g = groupOf(ctx.T, 'bed|t1#0');
    near(g.position.x, stored.x, 'repaint x', 1e-9); near(g.position.z, stored.z, 'repaint z', 1e-9); near(g.rotation.y, stored.rotation, 'repaint rotation', 1e-9);
    // a reload: the page's stored state read back into a fresh page
    const placed = JSON.parse(ctx.saves.at(-1)), again = await fresh(); again.S.placed = placed; again.paint();
    const g2 = groupOf(again.T, 'bed|t1#0');
    near(g2.position.x, stored.x, 'reload x', 1e-9); near(g2.position.z, stored.z, 'reload z', 1e-9); near(g2.rotation.y, stored.rotation, 'reload rotation', 1e-9);
    const q = again.pieces(d).find(p => p.key === 'bed|t1'), pl = V.toPlan(d, stored.x, stored.z);
    near(q.cx, pl.px, 'the floor plan draws the moved piece there too'); near(q.cy, pl.py, 'floor plan y');
    const other = again.pieces(d).find(p => p.key === 'stool|t1'); assert.equal(other.cx, makePieces(again.S, again.C, null)(d).find(p => p.key === 'stool|t1').cx, 'an untouched piece keeps the page position');
  });

  await test(`${label}: a drop that does not fit stays where it was put, proposed, with its reason, not stored`, async () => {
    const ctx = await fresh(); ctx.paint(); ctx.selectPiece('stool|t1');
    const st = entryOf('stool|t1#0'), bed = entryOf('bed|t1#0'), sx = st.x, sz = st.z;
    V.moveTo('stool|t1#0', sx, sz, bed.x - sx, bed.z - sz);
    assert.equal(groupOf(ctx.T, 'stool|t1#0').userData.state, 'proposed', 'shown proposed while dragging');
    const reasons = V.drop('stool|t1#0');
    assert.deepEqual(reasons, ['overlaps Treatment bed']);
    assert.equal(ctx.saves.length, 0); assert.equal(ctx.S.placed[4].pos, undefined, 'not stored');
    const x = entryOf('stool|t1#0').x; ctx.paint();
    near(groupOf(ctx.T, 'stool|t1#0').position.x, x, 'not snapped back after a repaint', 1e-9);
    assert.equal(groupOf(ctx.T, 'stool|t1#0').userData.state, 'proposed');
    V.moveTo('stool|t1#0', sx, sz, 0, -20);
    assert.deepEqual(V.drop('stool|t1#0'), ['outside the room']);
    // the parts list says it in plain words
    const pane = ctx.dom.el('div'), row = ctx.dom.el('tr'), td = ctx.dom.el('td'); row.dataset.key = 'stool|t1'; row.append(td); pane.append(row);
    V.annotate(pane);
    const [note, fit] = td.children;
    assert.equal(note.textContent, F.NOTE);
    assert.match(fit.textContent, /^Not saved: outside the room\. Move or turn it until it fits\.$/);
    assert.ok(fit.className.includes('proposed'));
    V.moveTo('stool|t1#0', sx, sz, 0, 0); assert.deepEqual(V.drop('stool|t1#0'), []);
    assert.equal(fit.textContent, 'Fits this sketch: inside the room and clear of the other pieces. Position saved.');
    assert.equal(groupOf(ctx.T, 'stool|t1#0').userData.state, 'selected');
    for (const t of [note, fit]) assert.doesNotMatch(t.textContent, /approv|complian|to code|\$/i);
  });

  await test(`${label}: 50 repaints leave the caches, groups and scene as after one`, async () => {
    const ctx = await fresh(); ctx.paint();
    const one = { ...F.cacheStats(THREE), groups: V._view().groups.size, children: ctx.T.blocks.children.length };
    for (let i = 0; i < 50; i++) { if (i % 7 === 0) ctx.selectPiece(i % 14 ? 'bed|t1' : 'desk|rc'); else ctx.paint(); }
    ctx.S.sel = null; ctx.paint();
    assert.deepEqual({ ...F.cacheStats(THREE), groups: V._view().groups.size, children: ctx.T.blocks.children.length }, one);
    ctx.S.stage = 'compare'; V.paint(ctx.T, d, []);
    assert.deepEqual([V._view().groups.size, ctx.T.blocks.children.length], [0, 0], 'leaving the furnish stage frees every group');
  });

  await test(`${label}: an item with no size gets the nominal size and its note; a surface draws nothing`, async () => {
    const ctx = await fresh({ nominal: true }); ctx.paint();
    const e = entryOf('treatment_bed@treatment|t1#0'), n = F.nominalSize('treatment_bed');
    assert.deepEqual([e.size.width, e.size.depth, e.size.height], [n.width, n.depth, n.height]);
    assert.equal(groupOf(ctx.T, 'flooring_lvp_sqm@x1|t1#0'), undefined);
    ctx.selectPiece('treatment_bed@treatment|t1');
    const pane = ctx.dom.el('div'), row = ctx.dom.el('tr'), td = ctx.dom.el('td'); row.dataset.key = 'treatment_bed@treatment|t1'; row.append(td); pane.append(row);
    V.annotate(pane);
    assert.equal(td.children[0].textContent, `${F.NOTE} ${F.NOMINAL_NOTE}`);
    assert.match(td.children[1].textContent, /^Drag it to move/);
  });

  await test(`${label}: a throwing builder falls back to the page's boxes with one console line`, async () => {
    const lines = [], warn = console.warn; console.warn = m => lines.push(m);
    try {
      let n = 0;
      const Bad = { ...THREE, Mesh: class extends THREE.Mesh { constructor(...a) { super(...a); if (++n > 30) throw new Error('builder broke'); } } };
      const ctx = setup(Bad, { load }); await tick();
      assert.equal(ctx.paint(), false);
      assert.equal(groups(ctx.T).length, 0, 'no model left in the scene');
      assert.equal(ctx.T.blocks.children.length, ctx.pieces(d).length, 'the boxes are drawn');
      assert.equal(ctx.paint(), false); assert.equal(V.planRect(d, { c: 'bed', pos: { x: 0, z: 0, rotation: 0 } }).cx, V.toPlan(d, 0, 0).px, 'stored moves still show on the plan');
      assert.equal(lines.length, 1, lines.join('\n')); assert.match(lines[0], /^furniture view: paint failed \(builder broke\); the page draws plain boxes$/);
    } finally { console.warn = warn; }
  });

  await test(`${label}: the room polygon is the drawn rectangle, in the boxes' frame`, () => {
    const poly = V.roomPolygon(d, d.rooms[1]);
    near(poly[0][0], -5 + (550 - 20) / 100, 'x0'); near(poly[0][1], (60 - 30) / 100 - 3, 'z0'); near(poly[2][0], -5 + (950 - 20) / 100, 'x1'); near(poly[2][1], (300 - 30) / 100 - 3, 'z1');
  });

  if (!real) return;
  await test(`${label}: pointer: a click selects, a short wobble only selects, a drag on the selected piece moves it, R turns it`, async () => {
    const ctx = await fresh(); ctx.paint();
    const { dom, T } = ctx, toPx = (x, z) => ({ clientX: (x / 10 + 1) * 100, clientY: (z / 10 + 1) * 100 });
    // the page's own listeners on the canvas: orbit controls on pointerdown, the selection raycast on pointerup
    let down = null; const orbit = [];
    dom.canvas.addEventListener('pointerdown', e => { orbit.push(T.controls.enabled); down = [e.clientX, e.clientY]; });
    dom.canvas.addEventListener('pointerup', e => {
      if (!down || Math.hypot(e.clientX - down[0], e.clientY - down[1]) > 4) return;
      const ray = new THREE.Raycaster(); ray.setFromCamera(new THREE.Vector2(e.clientX / 100 - 1, -e.clientY / 100 + 1), T.camera); T.scene.updateMatrixWorld();
      const hit = ray.intersectObjects(T.blocks.children, true)[0]; if (hit) ctx.selectPiece(V.keyOf(hit.object) ?? hit.object.userData.key);
    });
    const bed = entryOf('bed|t1#0'), at = toPx(bed.x, bed.z), x0 = bed.x;
    dom.dispatch(dom.canvas, 'pointerdown', at); dom.dispatch(dom.canvas, 'pointerup', at);
    assert.equal(ctx.S.sel, 'bed|t1', 'a click on the model selects it'); assert.deepEqual(orbit, [true], 'orbit untouched for an unselected piece');
    assert.equal(dom.tools.children.length, 5); assert.equal(dom.tools.children.at(-1).textContent, 'Reset view'); assert.equal(dom.tools.children[0].hidden, false, 'turn buttons shown');
    dom.dispatch(dom.canvas, 'pointerdown', at); dom.dispatch(dom.canvas, 'pointermove', { clientX: at.clientX + 2, clientY: at.clientY }); dom.dispatch(dom.canvas, 'pointerup', { clientX: at.clientX + 2, clientY: at.clientY });
    assert.equal(entryOf('bed|t1#0').x, x0, 'a 2 px wobble does not move'); assert.equal(ctx.S.sel, null, 'it clicks, as on a box today (toggles the selection)');
    assert.equal(T.controls.enabled, true);
    ctx.selectPiece('bed|t1');
    dom.dispatch(dom.canvas, 'pointerdown', at);
    assert.equal(orbit.at(-1), false, 'orbit controls are off before they see a press on the selected piece');
    const far = toPx(bed.x + 1.5, bed.z), to = toPx(bed.x + 0.33, bed.z + 0.04); // 10 px per metre here: past the 4 px click limit, then back
    dom.dispatch(dom.canvas, 'pointermove', far);
    assert.equal(groupOf(T, 'bed|t1#0').userData.state, 'proposed', 'over the stool while dragging: proposed');
    dom.dispatch(dom.canvas, 'pointermove', to); dom.dispatch(dom.canvas, 'pointerup', to);
    near(entryOf('bed|t1#0').x, x0 + 0.3, 'dragged 0.33 m, moved 0.3 m', 1e-6); near(groupOf(T, 'bed|t1#0').position.y, 0.02, 'stays on the floor', 1e-12);
    assert.equal(ctx.S.sel, 'bed|t1', 'a drag that ends near its start does not change the selection'); assert.equal(T.controls.enabled, true); assert.equal(ctx.S.placed[3].pos.x, entryOf('bed|t1#0').x);
    const r = entryOf('bed|t1#0').rotation; dom.key({ key: 'R', shiftKey: true });
    near(entryOf('bed|t1#0').rotation, (r + Math.PI / 12) % (2 * Math.PI), 'Shift+R turns 15° anticlockwise', 1e-9);
    dom.tools.children[1].fire('click', {}); near(entryOf('bed|t1#0').rotation, r, 'the right-turn button turns back', 1e-9);
    dom.key({ key: 'r', target: { closest: s => (s.includes('input') ? {} : null) } }); near(entryOf('bed|t1#0').rotation, r, 'R while typing does nothing', 1e-9);
  });

  await test(`${label}: pointer: a missed release never leaves the piece following a hover or the orbit off`, async () => {
    const ctx = await fresh(); ctx.paint(); ctx.selectPiece('stool|t1');
    const { dom, T } = ctx, toPx = (x, z) => ({ clientX: (x / 10 + 1) * 100, clientY: (z / 10 + 1) * 100 }), st = entryOf('stool|t1#0'), x0 = st.x;
    dom.dispatch(dom.canvas, 'pointerdown', toPx(st.x, st.z)); assert.equal(T.controls.enabled, false);
    dom.dispatch(dom.canvas, 'pointermove', { ...toPx(x0 - 0.6, st.z), buttons: 0 }); // no pointerup came
    assert.equal(entryOf('stool|t1#0').x, x0, 'a hover with no button does not move it'); assert.equal(T.controls.enabled, true, 'orbit back on');
    dom.dispatch(dom.canvas, 'pointerdown', toPx(st.x, st.z)); dom.canvas.fire('lostpointercapture', { pointerId: 1 });
    assert.equal(T.controls.enabled, true, 'lost capture ends the drag');
    dom.dispatch(dom.canvas, 'pointerdown', toPx(st.x, st.z)); dom.dispatch(dom.canvas, 'pointerdown', toPx(9, 9));
    assert.equal(T.controls.enabled, true, 'a second press ends a stale drag');
  });
}

const STUB = stub();
await suite(STUB, 'stub', false);
/* ---- browser sweep 1: a live layout's pieces (no size from the backend), and the furnish view on a sketch */
function sketchPage(THREE) {
  V._reset();
  const item = (id, name) => [id, { id, room: id.split('@')[1], name, w: 0, d: 0, h: 0, at: [[0.5, 0.5]], live: {} }];
  const C = Object.fromEntries([item('treatment_bed@treatment', 'Treatment bed'), item('storage_cabinet@treatment', 'Storage cabinet'), item('task_lighting@treatment', 'Task lighting'), item('hand_sink@treatment', 'Hand sink'),
    item('reception_desk@reception', 'Reception desk'), item('shelving_unit@storage', 'Shelving unit'), item('flooring_lvp_sqm@b1', 'Flooring, vinyl plank, 12 m²')]);
  // 12 m by 8 m: two treatment rooms (one wide, one tall), a reception and a storage room.
  const dd = { plan: { w: 1200, h: 800, ink: [0, 0], mesh: [0, 0] }, sketch: { label: 'Approximate sketch' }, rooms: [
    { id: 't1', type: 'treatment', name: 'Treatment 1', r: [0, 0, .3, .4] }, { id: 't2', type: 'treatment', name: 'Treatment 2', r: [0, .4, .25, 1] },
    { id: 'rc', type: 'reception', name: 'Reception 1', r: [.3, 0, 1, .6] }, { id: 'st', type: 'storage', name: 'Storage 1', r: [.3, .6, .55, 1] }] };
  const S = { stage: 'furnish', sel: null, placed: [
    ...['t1', 't2'].flatMap(r => ['treatment_bed@treatment', 'storage_cabinet@treatment', 'task_lighting@treatment', 'hand_sink@treatment', 'flooring_lvp_sqm@b1'].map(c => ({ c, r, st: 'placed' }))),
    { c: 'reception_desk@reception', r: 'rc', st: 'placed' }, { c: 'flooring_lvp_sqm@b1', r: 'rc', st: 'placed' }, { c: 'shelving_unit@storage', r: 'st', st: 'placed' }, { c: 'shelving_unit@storage', r: 'st', st: 'placed' }] };
  const dom = makeDoc(), blocks = new THREE.Group(), scene = THREE.Scene ? new THREE.Scene() : new THREE.Group(); scene.add(blocks);
  const T = { THREE, scene, blocks, camera: { isCamera: true }, controls: { enabled: true }, renderer: { domElement: dom.canvas } };
  const pieces = makePieces(S, C, V);
  V.bind({ S: () => S, C, save() {}, selectPiece() {}, renderViewer() {} });
  return { C, dd, S, T, pieces };
}
const outlinesOf = T => T.scene.children.filter(o => o.userData.sketchOutlines);
await test('sweep 2: pieces with no size are drawn on the plan at nominal size, spread inside their room, none on another', () => {
  const { C, dd, S, pieces } = sketchPage(STUB), list = pieces(dd);
  const solid = list.filter(q => !q.it.c.startsWith('flooring')), flat = list.filter(q => q.it.c.startsWith('flooring'));
  assert.equal(solid.length, 11); assert.ok(flat.every(q => q.w === 0 && q.h === 0), 'a surface draws nothing, as before');
  for (const q of solid) {
    const n = F.nominalSize(q.it.c), room = dd.rooms.find(r => r.id === q.it.r);
    near(Math.max(q.w, q.h), Math.max(n.width, n.depth) * 100, `${q.key} long side`); near(Math.min(q.w, q.h), Math.min(n.width, n.depth) * 100, `${q.key} short side`);
    assert.ok(q.cx - q.w / 2 >= room.r[0] * 1200 && q.cx + q.w / 2 <= room.r[2] * 1200 && q.cy - q.h / 2 >= room.r[1] * 800 && q.cy + q.h / 2 <= room.r[3] * 800, `${q.key} in ${room.id}`);
  }
  for (const a of solid) for (const b of solid) if (a !== b) assert.ok(Math.abs(a.cx - b.cx) >= (a.w + b.w) / 2 + 10 || Math.abs(a.cy - b.cy) >= (a.h + b.h) / 2 + 10, `${a.key} and ${b.key} are apart`);
  assert.equal(new Set(solid.map(q => `${q.cx}|${q.cy}`)).size, solid.length, 'no pile');
  // The page's own sized items keep the page's rule: nothing from here.
  assert.deepEqual(V.planRect(dd, { c: 'bed', r: 't1' }), {});
  // A moved piece keeps its place and its turned footprint.
  const bed = S.placed[0]; bed.pos = { x: 1.5, z: 2, rotation: Math.PI / 2 };
  const moved = pieces(dd)[0]; near(moved.cx, 150, 'cx'); near(moved.cy, 200, 'cy'); near(moved.w, 76, 'turned width'); near(moved.h, 189, 'turned depth');
});
await test('sweep (added 9): on a sketch the scene has the rooms’ outlines on the floor’s rectangle and every model stands inside its room, clear of the others; none on a measured layout', () => {
  const { dd, S, T, pieces } = sketchPage(STUB);
  assert.equal(V.paint(T, dd, pieces(dd)), true);
  const es = V._view().entries; assert.equal(es.length, 11); assert.equal(groups(T).length, 11);
  for (const e of es) assert.deepEqual(V.check(e), [], `${e.id} fits where it starts`);
  assert.equal(new Set(es.map(e => `${e.x}|${e.z}`)).size, 11); assert.ok(es.every(e => e.x > 0 && e.z > 0), 'nothing at the origin');
  const [o] = outlinesOf(T); assert.ok(o && outlinesOf(T).length === 1); assert.equal(o.children.length, 16, 'four sides for each of four rooms');
  const xs = o.children.flatMap(m => [m.position.x - m.userData.size[0] / 2, m.position.x + m.userData.size[0] / 2]), zs = o.children.flatMap(m => [m.position.z - m.userData.size[2] / 2, m.position.z + m.userData.size[2] / 2]);
  near(Math.min(...xs), 0, 'floor min x', 0.03); near(Math.max(...xs), 12, 'floor max x', 0.03); near(Math.min(...zs), 0, 'floor min z', 0.03); near(Math.max(...zs), 8, 'floor max z', 0.03);
  assert.ok(!T.blocks.children.includes(o) && o.children.every(m => m.position.y < 0.02), 'flat, and never hit by a click on a piece');
  for (let i = 0; i < 20; i++) V.paint(T, dd, pieces(dd));
  assert.equal(outlinesOf(T).length, 1, 'one group after repaints');
  S.stage = 'compare'; V.paint(T, dd, []); assert.equal(outlinesOf(T).length, 0, 'the furnish stage only');
  S.stage = 'furnish'; V.paint(T, dd, pieces(dd)); assert.equal(outlinesOf(T).length, 1);
  delete dd.sketch; V.paint(T, dd, pieces(dd)); assert.equal(outlinesOf(T).length, 0, 'a measured layout has no drawn outlines');
});
await test('review: a small move of one piece leaves every other piece where it was, and nothing reads overlaps', () => {
  const { dd, S, T, pieces } = sketchPage(STUB);
  V.paint(T, dd, pieces(dd));
  const before = V._view().entries.map(e => [e.id, e.x, e.z]), e = V._view().entries[0];
  assert.deepEqual(V.moveTo(e.id, e.x, e.z, 0, -0.1), [], 'the move itself is clean'); assert.deepEqual(V.drop(e.id), []);
  assert.ok(S.placed[0].pos, 'stored');
  V.paint(T, dd, pieces(dd));
  const after = V._view().entries;
  for (const [id, x, z] of before.slice(1)) { const a = after.find(o => o.id === id); near(a.x, x, id + ' x'); near(a.z, z, id + ' z'); }
  for (const a of after) assert.deepEqual(V.check(a), [], a.id + ' still fits');
  // removing the moved piece's neighbour does not move the moved piece; the list still has no pile
  assert.equal(new Set(after.map(a => a.x + '|' + a.z)).size, after.length);
});
await test('workspace.js: without ?live=1 or ?models=1 none of our code runs (one guard)', () => {
  const guard = src.match(/^const FV = .*$/m)?.[0];
  assert.ok(guard && guard.includes("LIVE || new URLSearchParams(location.search).get('models') === '1' ? await import('./furniture-view.js')") && guard.includes(': null;'), 'the guard');
  const uses = [...src.matchAll(/\bFV\b(.{0,2})/g)].slice(1);
  assert.ok(uses.length >= 5 && uses.every(m => m[1] === '?.' || m[0] === 'FV)['), `every use is FV?. or !!FV: ${uses.map(m => m[0]).join(' ')}`);
  assert.ok(src.includes('if (!fv) T.blocks.clear();') && src.includes('if (!fv && S.stage'), 'the boxes are drawn when the module did not paint');
});
await test('furniture-view.js imports only ./furniture.js', () => {
  const own = readFileSync(new URL('./furniture-view.js', import.meta.url), 'utf8');
  assert.deepEqual([...own.matchAll(/^import .* from '([^']+)'/gm)].map(m => m[1]), ['./furniture.js']);
  assert.doesNotMatch(own, /import\(/);
});
if (process.env.THREE_MODULE) {
  const THREE = await import(pathToFileURL(process.env.THREE_MODULE));
  console.log(`-- real three.js r${THREE.REVISION}${process.env.ROUNDED_BOX ? ' with RoundedBoxGeometry' : ''}`);
  await suite(THREE, 'three', true);
}
console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
