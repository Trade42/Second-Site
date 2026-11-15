// Second Site furniture view: the workspace page's 3D furnish view draws our furniture models
// (furniture.js) in place of plain boxes, and a person can select, move and turn a piece and see
// whether it fits this sketch. Loaded by workspace.js only with ?live=1 or ?models=1.
// Every entry point catches its own errors: paint() then returns false and the page draws its old
// boxes, and one console line says why. Imports only ./furniture.js; the page passes three.js in.
//
// Frames. The floor plan image is 100 px per metre; the page maps a plan pixel (px, py) to the scene
// floor as x = mesh[0] + (px - ink[0]) / 100, z = -(mesh[1] - (py - ink[1]) / 100) (paint3D's boxes).
// A room's rectangle r = [x0, y0, x1, y1] is in fractions of the plan image (plan.w, plan.h pixels):
// its corners are (r[0] * w, r[1] * h) ... (r[2] * w, r[3] * h) in pixels, then the same mapping.
// A piece's position is its footprint centre on the floor in scene metres; rotation is about +Y.
// A pass means the piece fits this sketch: inside its room's drawn rectangle and clear of the other
// pieces. It is not an approved plan and says nothing about code or clearances.
import * as F from './furniture.js';

const FT = 0.3048, PX = 100, BASE_Y = 0.02, DRAG_PX = 4, TURN = (15 * Math.PI) / 180;
const FIT_TEXT = 'Fits this sketch: inside the room and clear of the other pieces. Position saved.';
const HINT_TEXT = 'Drag it to move (0.1 m steps); R, Shift+R or the turn buttons turn it 15°.';

let page = null, broken = false, view = null, drag = null, addons = {}, addonsWanted = false;
const warned = new Set();
const pending = new WeakMap(); // page item -> { x, z, rotation, reasons }: a move that does not fit, not stored
const results = new WeakMap(); // page item -> the last check: { ok, reasons }
function warn(where, e) {
  const line = `furniture view: ${where} failed (${e?.message || e}); the page draws plain boxes`;
  if (!warned.has(line)) { warned.add(line); console.warn(line); }
}

/* ------------------------------------------------------------------ pure placement (metres) */

export const toScene = (d, px, py) => ({ x: d.plan.mesh[0] + (px - d.plan.ink[0]) / PX, z: -(d.plan.mesh[1] - (py - d.plan.ink[1]) / PX) });
export const toPlan = (d, x, z) => ({ px: (x - d.plan.mesh[0]) * PX + d.plan.ink[0], py: (z + d.plan.mesh[1]) * PX + d.plan.ink[1] });
// The room's drawn rectangle as a polygon on the scene floor.
export function roomPolygon(d, room) {
  const [x0, y0, x1, y1] = [room.r[0] * d.plan.w, room.r[1] * d.plan.h, room.r[2] * d.plan.w, room.r[3] * d.plan.h];
  return [[x0, y0], [x1, y0], [x1, y1], [x0, y1]].map(([px, py]) => { const p = toScene(d, px, py); return [p.x, p.z]; });
}
// Metres from the catalog's feet; an item without a size gets the nominal size and its note; null: nothing to draw.
export function sizeOf(c) {
  const s = [c?.w, c?.d, c?.h].map(v => v * FT);
  if (s.every(v => Number.isFinite(v) && v > 0)) return { width: s[0], depth: s[1], height: s[2], nominal: false };
  return c ? F.nominalSize(c.id) : null;
}
// The page turns a piece so its long side follows the room (pieceRect): the model then turns by 90°.
export function baseRotation(d, room, size) {
  const rw = (room.r[2] - room.r[0]) * d.plan.w, rh = (room.r[3] - room.r[1]) * d.plan.h;
  return (rh > rw) !== (size.depth > size.width) ? Math.PI / 2 : 0;
}
const validPos = p => !!p && [p.x, p.z, p.rotation].every(Number.isFinite);
const isSurface = c => F.ITEM_MODELS[String(c?.id ?? '').split('@')[0]] === null;

// The floor plan's rectangle for a moved piece (its footprint's bounding box), for pieces() in workspace.js.
export function planRect(d, it) {
  try {
    if (!page || !validPos(it?.pos)) return {};
    const c = page.C[it.c], { px, py } = toPlan(d, it.pos.x, it.pos.z), out = { cx: px, cy: py };
    if (c && [c.w, c.d].every(v => Number.isFinite(v) && v > 0)) {
      const co = Math.abs(Math.cos(it.pos.rotation)), si = Math.abs(Math.sin(it.pos.rotation)), w = c.w * FT * PX, h = c.d * FT * PX;
      Object.assign(out, { w: co * w + si * h, h: si * w + co * h });
    }
    return out;
  } catch (e) { warn('planRect', e); return {}; }
}

/* ------------------------------------------------------------------------------- page link */

// page: { S: () => S, C, save, selectPiece, renderViewer, load: the page's dynamic import of a module specifier }.
export function bind(p) {
  try {
    page = p;
    if (!addonsWanted && typeof p.load === 'function') {
      addonsWanted = true;
      Promise.resolve(p.load('three/addons/geometries/RoundedBoxGeometry.js'))
        .then(m => { addons.RoundedBoxGeometry = m.RoundedBoxGeometry; if (view) page.renderViewer?.(); }) // redraw with rounded parts
        .catch(e => { warn('loading RoundedBoxGeometry', e); fail(); if (view) page.renderViewer?.(); }); // redraw as boxes
    }
  } catch (e) { warn('bind', e); broken = true; }
}
function fail() {
  broken = true;
  try { if (view) { for (const g of view.groups.values()) F.dispose(view.T.THREE, g); view.groups.clear(); view.entries = []; showTools(); } } catch { /* already falling back */ }
}

// From a raycast hit to the page's piece key (selection), or undefined.
export function keyOf(object) {
  try { return F.rootOf(object)?.userData.key; } catch (e) { warn('keyOf', e); return undefined; }
}

/* ---------------------------------------------------------------------------------- paint */

// Draw `list` (the page's pieces(d)) as models in T.blocks. True when drawn; false: the page draws boxes.
export function paint(T, d, list) {
  if (broken || !page) return false;
  try { attach(T); paintModels(T, d, list); return true; } catch (e) { warn('paint', e); fail(); return false; }
}
function paintModels(T, d, list) {
  const S = page.S(), seen = {}, entries = [], keep = new Set();
  for (const o of [...T.blocks.children]) if (!o.userData?.furniture) T.blocks.remove(o); // boxes from an earlier fallback
  for (const q of list) {
    const c = page.C[q.it.c], n = (seen[q.key] = (seen[q.key] ?? -1) + 1), id = `${q.key}#${n}`;
    const room = d.rooms.find(r => r.id === q.it.r), size = sizeOf(c);
    if (!c || !room || !size || isSurface(c)) continue; // nothing drawn, as the zero-size box drew nothing
    const home = toScene(d, q.cx, q.cy), at = pending.get(q.it) || (validPos(q.it.pos) ? q.it.pos : { ...home, rotation: baseRotation(d, room, size) });
    const label = F.modelFor(c.id)?.label || String(c.name || 'another item').split(',')[0];
    entries.push({ id, key: q.key, it: q.it, c, room, size, label, x: at.x, z: at.z, rotation: at.rotation });
  }
  view.d = d; view.entries = entries;
  for (const e of entries) {
    const sig = `${e.c.id}|${e.size.width}|${e.size.depth}|${e.size.height}|${addons.RoundedBoxGeometry ? 1 : 0}`, state = stateOf(e, S);
    let g = view.groups.get(e.id);
    if (g && g.userData.sig !== sig) { F.dispose(T.THREE, g); g = null; }
    if (!g) {
      g = F.build(T.THREE, e.c.id, { width: e.size.width, depth: e.size.depth, height: e.size.height, state, RoundedBoxGeometry: addons.RoundedBoxGeometry });
      g.userData.sig = sig; view.groups.set(e.id, g);
    } else if (g.userData.state !== state) F.setState(T.THREE, g, state);
    Object.assign(g.userData, { key: e.key, pieceId: e.id });
    g.position.set(e.x, BASE_Y, e.z); g.rotation.y = e.rotation;
    if (g.parent !== T.blocks) T.blocks.add(g);
    keep.add(e.id);
  }
  for (const [id, g] of view.groups) if (!keep.has(id)) { F.dispose(T.THREE, g); view.groups.delete(id); }
  showTools();
}
function stateOf(e, S) { return pending.has(e.it) ? 'proposed' : S.sel === e.key ? 'selected' : e.it.st === 'proposed' ? 'proposed' : 'placed'; }

/* --------------------------------------------------------------- moving, turning and checking */

const entry = id => view?.entries.find(e => e.id === id) || null;
// The piece the person is working on: the one last pressed, if it is selected, else the first selected one.
function active() {
  const S = page.S(), a = entry(view?.active);
  return a && a.key === S.sel ? a : view?.entries.find(e => e.key === S.sel) || null;
}
export function check(e) {
  const others = view.entries.filter(o => o !== e).map(o => ({ id: o.id, name: o.label, x: o.x, z: o.z, width: o.size.width, depth: o.size.depth, rotation: o.rotation }));
  return F.issues({ id: e.id, name: e.label, x: e.x, z: e.z, width: e.size.width, depth: e.size.depth, rotation: e.rotation }, others, { polygon: roomPolygon(view.d, e.room) });
}
function place(e, x, z, rotation) {
  const s = F.snap({ x: 0, z: 0 }, rotation);
  e.x = x; e.z = z; e.rotation = s.rotation;
  const g = view.groups.get(e.id); if (g) { g.position.set(e.x, BASE_Y, e.z); g.rotation.y = e.rotation; }
  const reasons = check(e), state = reasons.length ? 'proposed' : stateOf({ ...e, it: { st: e.it.st } }, page.S());
  if (g && g.userData.state !== state) F.setState(view.T.THREE, g, state);
  return reasons;
}
// Move by (dx, dz) metres from (x0, z0), snapped to 0.1 m steps; checked, not stored (dragging).
export function moveTo(id, x0, z0, dx, dz) {
  try {
    const e = entry(id); if (!e) return null;
    const s = F.snap({ x: dx, z: dz }, 0), reasons = place(e, x0 + s.x, z0 + s.z, e.rotation);
    status(e, reasons.length ? { ok: false, reasons, live: true } : { ok: true, live: true });
    return reasons;
  } catch (err) { warn('moving a piece', err); return null; }
}
// Release: store the position if it fits, else keep it where it was put, proposed, with the reasons.
export function drop(id) {
  try {
    const e = entry(id); if (!e) return null;
    const reasons = place(e, e.x, e.z, e.rotation);
    if (reasons.length) { pending.set(e.it, { x: e.x, z: e.z, rotation: e.rotation, reasons }); results.set(e.it, { ok: false, reasons }); }
    else { pending.delete(e.it); e.it.pos = { x: e.x, z: e.z, rotation: e.rotation }; results.set(e.it, { ok: true }); page.save(); }
    status(e, results.get(e.it));
    return reasons;
  } catch (err) { warn('placing a piece', err); return null; }
}
// Turn 15° about the vertical axis (+1 anticlockwise on the plan, -1 clockwise), then check and store as a drop.
export function turn(id, dir) {
  try {
    const e = id ? entry(id) : active(); if (!e) return null;
    place(e, e.x, e.z, e.rotation + Math.sign(dir || 1) * TURN);
    return drop(e.id);
  } catch (err) { warn('turning a piece', err); return null; }
}

/* ----------------------------------------------------------------------------- the details */

function fitText(r) { return !r ? HINT_TEXT : r.ok ? (r.live ? 'Fits this sketch here.' : FIT_TEXT) : `Not saved: ${r.reasons.join('; ')}. Move or turn it until it fits.`; }
function status(e, r) {
  const el = view?.fitEl; if (!el?.isConnected || el.dataset.piece !== e.id) return;
  el.textContent = fitText(r); el.className = r && !r.ok ? 'fv-fit proposed' : 'fv-fit';
}
// After the page draws its parts list: the selected piece's row says what the model is and whether it fits.
export function annotate(pane) {
  try {
    if (broken || !view || !pane) return;
    const e = active(); if (!e) return;
    const row = [...pane.querySelectorAll('tr[data-key]')].find(r => r.dataset.key === e.key), cell = row?.querySelector('td'); if (!cell) return;
    const g = view.groups.get(e.id), doc = pane.ownerDocument;
    const note = doc.createElement('small'); note.className = 'fv-note';
    note.textContent = [g?.userData.note || F.NOTE, e.size.nominal ? e.size.note : ''].filter(Boolean).join(' ');
    const fit = doc.createElement('small'); fit.dataset.piece = e.id;
    cell.append(note, fit); view.fitEl = fit;
    status(e, pending.has(e.it) ? { ok: false, reasons: pending.get(e.it).reasons } : results.get(e.it));
  } catch (err) { warn('the parts list note', err); }
}

/* ------------------------------------------------------------------- pointer, keys and buttons */

function attach(T) {
  if (view?.T === T) return;
  if (view) for (const g of view.groups.values()) F.dispose(view.T.THREE, g);
  view = { T, groups: new Map(), entries: [], d: null, active: null, fitEl: null, tools: [] };
  const canvas = T.renderer.domElement, host = canvas.parentElement || canvas, doc = canvas.ownerDocument;
  // Capture on the parent, so the orbit controls (listening on the canvas) are off before they see a press on a piece.
  host.addEventListener('pointerdown', ev => guard('pointerdown', () => down(ev)), true);
  canvas.addEventListener('pointermove', ev => guard('pointermove', () => move(ev)));
  // The release of a real drag stops here, so the page's click test (start and end within 4 px) never sees it.
  for (const t of ['pointerup', 'pointercancel']) host.addEventListener(t, ev => guard(t, () => up(ev)), true);
  canvas.addEventListener('lostpointercapture', ev => guard('lostpointercapture', () => up(ev))); // a release the page never saw ends the drag
  doc?.addEventListener('keydown', ev => guard('keydown', () => key(ev)));
  const tools = host.querySelector?.('.dollhouse-tools');
  if (tools && doc) view.tools = [[1, '⟲', 'Turn the selected piece left 15°'], [-1, '⟳', 'Turn the selected piece right 15°']].map(([dir, sym, label]) => {
    const b = doc.createElement('button'); b.type = 'button'; b.className = 'fv-turn'; b.textContent = sym; b.title = label; b.setAttribute('aria-label', label); b.hidden = true;
    b.addEventListener('click', () => guard('turn button', () => turn(null, dir))); return b;
  });
  if (view.tools.length) tools.prepend(...view.tools); // before zoom and Reset view, which stays the last button
}
function guard(where, fn) { if (broken || !view) return; try { fn(); } catch (e) { warn(where, e); if (drag) { view.T.controls.enabled = true; drag = null; } } }
function showTools() { const S = page?.S(), on = !broken && S?.stage === 'furnish' && !!active(); for (const b of view?.tools || []) b.hidden = !on; }
function ray(ev) {
  const { T } = view, r = T.renderer.domElement.getBoundingClientRect(), rc = new T.THREE.Raycaster();
  rc.setFromCamera(new T.THREE.Vector2(((ev.clientX - r.left) / r.width) * 2 - 1, -((ev.clientY - r.top) / r.height) * 2 + 1), T.camera);
  return rc;
}
function floorAt(ev) {
  const { origin: o, direction: v } = ray(ev).ray; if (Math.abs(v.y) < 1e-6) return null;
  const t = (BASE_Y - o.y) / v.y; return { x: o.x + t * v.x, z: o.z + t * v.z };
}
function down(ev) {
  const { T } = view, canvas = T.renderer.domElement;
  if (drag) up({ pointerId: drag.pointer }); // a drag whose release was missed ends here, orbit back on
  if (ev.target !== canvas || ev.button > 0 || page.S().stage !== 'furnish') return;
  T.blocks.updateMatrixWorld?.(true);
  const hit = ray(ev).intersectObjects(T.blocks.children, true)[0], g = hit && F.rootOf(hit.object), e = g && entry(g.userData.pieceId);
  if (!e) return;
  view.active = e.id;
  if (e.key !== page.S().sel) return; // not selected yet: the page's own click handler selects it
  const p = floorAt(ev); if (!p) return;
  drag = { id: e.id, pointer: ev.pointerId, sx: ev.clientX, sy: ev.clientY, x0: e.x, z0: e.z, p0: p, moved: false };
  T.controls.enabled = false;
  try { canvas.setPointerCapture?.(ev.pointerId); } catch { /* fine */ }
}
function move(ev) {
  if (!drag || ev.pointerId !== drag.pointer) return;
  if (ev.buttons === 0) return up(ev); // no button held: the release was missed, never follow a hover
  if (!drag.moved && Math.hypot(ev.clientX - drag.sx, ev.clientY - drag.sy) <= DRAG_PX) return; // a click only selects
  const p = floorAt(ev); if (!p) return;
  drag.moved = true; moveTo(drag.id, drag.x0, drag.z0, p.x - drag.p0.x, p.z - drag.p0.z);
}
function up(ev) {
  if (!drag || ev.pointerId !== drag.pointer) return;
  const d = drag; drag = null; view.T.controls.enabled = true;
  if (d.moved) ev.stopPropagation?.();
  try { view.T.renderer.domElement.releasePointerCapture?.(ev.pointerId); } catch { /* fine */ }
  if (d.moved) drop(d.id); // never snapped back: a piece that does not fit stays where it was put, proposed
}
function key(ev) {
  if (ev.key !== 'r' && ev.key !== 'R') return;
  const t = ev.target, canvas = view.T.renderer.domElement;
  if (ev.ctrlKey || ev.metaKey || ev.altKey || t?.closest?.('input, textarea, select, [contenteditable]') || canvas.closest?.('[hidden]') || page.S().stage !== 'furnish') return;
  if (!active()) return;
  ev.preventDefault?.(); turn(null, ev.shiftKey ? 1 : -1); // R clockwise on the plan, Shift+R anticlockwise
}

// Tests only: internal state, and a fresh module state.
export const _view = () => view;
export function _reset() { page = null; broken = false; view = null; drag = null; addons = {}; addonsWanted = false; warned.clear(); }
export const _broken = () => broken;
