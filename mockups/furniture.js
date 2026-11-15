// Second Site furniture models: our own format, builder, furniture set and placement helpers.
// Original work. Plain ES module, imports nothing: the caller passes the three.js namespace in.
//
// ---------------------------------------------------------------- model format (version 1)
// A model is { id, label, version: 1, approximate: true, note, parts: [part, ...], colors? }.
// A part is { shape, size: [sx, sy, sz], at: [x, y, z], rot?: [rx, ry, rz], mat, role, radius? }:
//   shape   'box' | 'rbox' (rounded box) | 'cylinder' (axis along local Y) | 'sphere'.
//           Every shape is drawn from a unit primitive whose bounding box is the unit cube, then
//           scaled by `size`, so a cylinder's size [dx, h, dz] is its two diameters and its height.
//           'rbox' needs the caller to pass RoundedBoxGeometry (from three/addons, one more module
//           fetch from the same CDN); without it a rounded box is drawn as a plain box.
//   size    extent along local X, Y, Z in model units, every value > 0.
//   at      the part's centre. rot: Euler angles in radians, order XYZ (three.js default).
//   mat     material class: wood | metal | fabric | laminate | ceramic | glass | matte.
//   role    colour role: body | accent | top. The colour comes from the caller's palette, so the
//           page's status and selection colours still apply. A model may give default colours.
// Model space is the normalised unit box: X (width) and Z (depth) in [-0.5, 0.5], Y (height) in
// [0, 1], Y up, the floor at Y = 0, the front facing +Z. The builder stretches the model's actual
// extent to the item's width (X), height (Y) and depth (Z), so one model serves any size.
// Validation rejects: a part whose bounding box (rotation included) leaves the unit box, a size
// that is not positive, an unknown shape, material or role, and more than PART_BUDGET parts.
//
// Axes. The page's scene is three.js Y-up: model X = scene X, model Y = scene Y (up), model Z =
// scene Z (the floor plan's downward direction). The Matterport scan is Z-up and the page turns it
// with rotation.x = -PI/2, so scene (X, Y, Z) = scan (x, z, -y): model X = scan x, model Y = scan z
// (up), model Z = scan -y. A model added as a child of the scan object needs rotation.x = +PI/2.
//
// Truth: every model is approximate: a simple shape sized to the item's listed dimensions, not the
// product itself. No brand or product names and no money amounts belong in this file.

export const FORMAT_VERSION = 1;
export const PART_BUDGET = 40;
export const NOTE = 'Approximate shape sized to the item\'s listed dimensions, not the product itself.';
export const PLACEHOLDER_NOTE = 'No model for this item type: a plain box sized to the item.';
export const SHAPES = ['box', 'rbox', 'cylinder', 'sphere'];
export const MATERIALS = {
  wood: { roughness: 0.75, metalness: 0 },
  metal: { roughness: 0.35, metalness: 0.75 },
  fabric: { roughness: 1, metalness: 0 },
  laminate: { roughness: 0.5, metalness: 0 },
  ceramic: { roughness: 0.25, metalness: 0 },
  glass: { roughness: 0.1, metalness: 0, transparent: true, opacity: 0.45 },
  matte: { roughness: 0.95, metalness: 0 },
};
export const ROLES = ['body', 'accent', 'top'];
// Default colours per role: the page's own block colour (0x6E7E72) and neighbours of it.
export const DEFAULT_PALETTE = { body: 0x6e7e72, accent: 0x4a554d, top: 0x9aa79c, selected: 0xa5823a };
const EPS = 1e-9;

/* ------------------------------------------------------------------ authoring helpers */

// A box from its extents (x0..x1, y0..y1, z0..z1).
function bx(x0, x1, y0, y1, z0, z1, mat, role, extra = {}) {
  return { shape: 'box', size: [x1 - x0, y1 - y0, z1 - z0], at: [(x0 + x1) / 2, (y0 + y1) / 2, (z0 + z1) / 2], mat, role, ...extra };
}
// A vertical cylinder from its centre on the floor plan, its diameters and its height range.
function cy(x, z, dx, dz, y0, y1, mat, role, extra = {}) {
  return { shape: 'cylinder', size: [dx, y1 - y0, dz], at: [x, (y0 + y1) / 2, z], mat, role, ...extra };
}
function sp(x, y, z, d, mat, role) {
  return { shape: 'sphere', size: [d, d, d], at: [x, y, z], mat, role };
}
// N spokes radiating from the centre (a five-star base), each `len` long, with a caster at the tip.
function star(n, len, y0, y1, w, mat, role, caster) {
  const out = [];
  for (let i = 0; i < n; i++) {
    const a = Math.PI / 2 + (i * 2 * Math.PI) / n; // first spoke points to the front (+Z)
    const cx = (Math.sin(a) * len) / 2, cz = (Math.cos(a) * len) / 2;
    out.push({ shape: 'box', size: [len, y1 - y0, w], at: [cx, (y0 + y1) / 2, cz], rot: [0, a - Math.PI / 2, 0], mat, role });
    if (caster) out.push(sp(Math.sin(a) * (len - caster / 2), caster / 2, Math.cos(a) * (len - caster / 2), caster, 'matte', 'accent'));
  }
  return out;
}
function model(id, label, parts, colors) {
  return Object.freeze({ id, label, version: FORMAT_VERSION, approximate: true, note: NOTE, parts: Object.freeze(parts.map(p => Object.freeze(p))), ...(colors ? { colors } : {}) });
}

/* ------------------------------------------------------------------------ furniture set */

const legs4 = (x, z, d, y1, mat = 'metal') => [[-x, -z], [x, -z], [-x, z], [x, z]].map(([a, b]) => cy(a, b, d, d * 2.5, 0, y1, mat, 'accent'));

export const MODELS = Object.freeze({
  // Treatment / facial bed: legs, a lift pedestal, a frame, the mattress, a raised backrest and headrest.
  treatment_bed: model('treatment_bed', 'Treatment bed', [
    ...legs4(0.43, 0.38, 0.04, 0.44),
    bx(-0.14, 0.14, 0, 0.44, -0.22, 0.22, 'metal', 'accent'),
    bx(-0.47, 0.47, 0.42, 0.52, -0.5, 0.5, 'laminate', 'body'),
    bx(-0.5, 0.2, 0.52, 0.74, -0.47, 0.47, 'fabric', 'top'),
    { shape: 'rbox', size: [0.32, 0.08, 0.94], at: [0.32, 0.8, 0], rot: [0, 0, 0.5], mat: 'fabric', role: 'top', radius: 0.15 },
    bx(0.4, 0.5, 0.9, 1, -0.3, 0.3, 'fabric', 'top'),
  ]),
  // Reception desk: a raised front with a transaction ledge, a lower work surface, sides, a drawer pedestal.
  reception_desk: model('reception_desk', 'Reception desk', [
    bx(-0.5, 0.5, 0, 0.92, 0.4, 0.5, 'wood', 'body'),
    bx(-0.5, 0.5, 0.92, 1, 0.28, 0.5, 'wood', 'top'),
    bx(-0.46, 0.46, 0.66, 0.72, -0.5, 0.4, 'laminate', 'top'),
    bx(-0.5, -0.46, 0, 0.72, -0.5, 0.4, 'wood', 'body'),
    bx(0.46, 0.5, 0, 0.72, -0.5, 0.4, 'wood', 'body'),
    bx(0.14, 0.44, 0, 0.66, -0.46, 0.36, 'wood', 'accent'),
    bx(0.26, 0.32, 0.54, 0.56, -0.48, -0.46, 'metal', 'accent'),
    bx(0.26, 0.32, 0.34, 0.36, -0.48, -0.46, 'metal', 'accent'),
    bx(0.26, 0.32, 0.14, 0.16, -0.48, -0.46, 'metal', 'accent'),
  ]),
  // Waiting bench: two seat cushions on a base board, four legs and a stretcher.
  bench: model('bench', 'Waiting bench', [
    ...legs4(0.44, 0.36, 0.035, 0.6, 'wood'),
    bx(-0.44, 0.44, 0.16, 0.22, -0.03, 0.03, 'wood', 'accent'),
    bx(-0.5, 0.5, 0.6, 0.68, -0.5, 0.5, 'wood', 'body'),
    { shape: 'rbox', size: [0.49, 0.32, 1], at: [-0.25, 0.84, 0], mat: 'fabric', role: 'top', radius: 0.2 },
    { shape: 'rbox', size: [0.49, 0.32, 1], at: [0.25, 0.84, 0], mat: 'fabric', role: 'top', radius: 0.2 },
  ]),
  // Wall shelving: a back panel, two uprights, a kick plate, four shelves and a top.
  wall_shelving: model('wall_shelving', 'Wall shelving', [
    bx(-0.46, 0.46, 0, 1, -0.5, -0.44, 'wood', 'accent'),
    bx(-0.5, -0.46, 0, 1, -0.5, 0.5, 'wood', 'body'),
    bx(0.46, 0.5, 0, 1, -0.5, 0.5, 'wood', 'body'),
    bx(-0.46, 0.46, 0, 0.06, -0.44, 0.5, 'wood', 'body'),
    ...[0.25, 0.45, 0.65, 0.85].map(y => bx(-0.46, 0.46, y, y + 0.025, -0.44, 0.48, 'wood', 'top')),
    bx(-0.5, 0.5, 0.97, 1, -0.5, 0.5, 'wood', 'top'),
  ]),
  // Steel shelving: four corner posts and five shelves.
  steel_shelving: model('steel_shelving', 'Steel shelving', [
    ...[[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([a, b]) => bx(a < 0 ? -0.5 : 0.48, a < 0 ? -0.48 : 0.5, 0, 1, b < 0 ? -0.5 : 0.45, b < 0 ? -0.45 : 0.5, 'metal', 'accent')),
    ...[0.04, 0.27, 0.5, 0.73, 0.96].map(y => bx(-0.48, 0.48, y, y + 0.03, -0.48, 0.48, 'metal', 'body')),
  ]),
  // Storage cabinet: a carcass on a plinth, two doors with handles.
  storage_cabinet: model('storage_cabinet', 'Storage cabinet', [
    bx(-0.47, 0.47, 0, 0.05, -0.45, 0.4, 'matte', 'accent'),
    bx(-0.5, 0.5, 0.05, 1, -0.5, 0.45, 'laminate', 'body'),
    bx(-0.49, -0.005, 0.07, 0.98, 0.45, 0.48, 'laminate', 'top'),
    bx(0.005, 0.49, 0.07, 0.98, 0.45, 0.48, 'laminate', 'top'),
    cy(-0.06, 0.49, 0.02, 0.02, 0.5, 0.7, 'metal', 'accent'),
    cy(0.06, 0.49, 0.02, 0.02, 0.5, 0.7, 'metal', 'accent'),
  ]),
  // Magnifying task lamp: a weighted round base, a pole, a two-piece arm and a ring head with a lens.
  task_lamp: model('task_lamp', 'Task lamp', [
    cy(0, 0, 1, 1, 0, 0.03, 'metal', 'accent'),
    cy(0, 0, 0.3, 0.3, 0.03, 0.06, 'metal', 'body'),
    cy(0, 0, 0.06, 0.06, 0.06, 0.66, 'metal', 'body'),
    { shape: 'box', size: [0.05, 0.22, 0.05], at: [0, 0.75, 0.06], rot: [0.55, 0, 0], mat: 'metal', role: 'body' },
    { shape: 'box', size: [0.05, 0.05, 0.2], at: [0, 0.84, 0.19], mat: 'metal', role: 'body' },
    { shape: 'cylinder', size: [0.44, 0.06, 0.44], at: [0, 0.79, 0.3], rot: [1.05, 0, 0], mat: 'laminate', role: 'top' },
    { shape: 'cylinder', size: [0.36, 0.065, 0.36], at: [0, 0.79, 0.3], rot: [1.05, 0, 0], mat: 'glass', role: 'top' },
  ]),
  // Treatment stool: a round cushion, a seat pan, a gas lift column and a five-star base on casters.
  stool: model('stool', 'Stool', [
    cy(0, 0, 0.9, 0.9, 0.84, 1, 'fabric', 'top'),
    cy(0, 0, 0.7, 0.7, 0.8, 0.84, 'metal', 'accent'),
    cy(0, 0, 0.12, 0.12, 0.16, 0.8, 'metal', 'body'),
    cy(0, 0, 0.2, 0.2, 0.1, 0.22, 'metal', 'body'),
    ...star(5, 0.5, 0.1, 0.16, 0.08, 'metal', 'body', 0.1),
  ]),
  // Three-tier rolling trolley: four posts, three trays, four casters and a push handle.
  trolley: model('trolley', 'Rolling trolley', [
    ...[[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([a, b]) => cy(a * 0.46, b * 0.44, 0.05, 0.06, 0.08, 0.95, 'metal', 'accent')),
    ...[0.12, 0.46, 0.8].map(y => bx(-0.5, 0.5, y, y + 0.05, -0.5, 0.5, 'laminate', y > 0.7 ? 'top' : 'body')),
    ...[[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([a, b]) => sp(a * 0.46, 0.04, b * 0.44, 0.08, 'matte', 'accent')),
    { shape: 'cylinder', size: [0.05, 0.84, 0.05], at: [0.46, 0.975, 0], rot: [Math.PI / 2, 0, 0], mat: 'metal', role: 'accent' },
  ]),
  // Facial steamer: a five-star rolling base, a pole, a heater with a glass water jar, an arm and a nozzle.
  steamer: model('steamer', 'Facial steamer', [
    ...star(5, 0.5, 0.05, 0.09, 0.07, 'metal', 'body', 0.08),
    cy(0, 0, 0.06, 0.06, 0.09, 0.66, 'metal', 'body'),
    cy(0, 0, 0.42, 0.42, 0.66, 0.78, 'laminate', 'body'),
    cy(0, 0, 0.36, 0.36, 0.78, 0.9, 'glass', 'top'),
    cy(0, 0, 0.3, 0.3, 0.9, 0.93, 'laminate', 'accent'),
    bx(-0.03, 0.03, 0.93, 0.97, 0, 0.42, 'metal', 'accent'),
    { shape: 'cylinder', size: [0.12, 0.16, 0.12], at: [0, 0.94, 0.42], rot: [Math.PI / 2, 0, 0], mat: 'laminate', role: 'top' },
  ]),
  // Heated towel cabinet: a body on feet, a door with a window, a handle and a control strip.
  towel_warmer: model('towel_warmer', 'Towel warmer', [
    ...legs4(0.42, 0.38, 0.06, 0.04, 'matte'),
    bx(-0.5, 0.5, 0.04, 1, -0.5, 0.44, 'laminate', 'body'),
    bx(-0.46, 0.46, 0.08, 0.86, 0.44, 0.47, 'laminate', 'top'),
    bx(-0.34, 0.34, 0.22, 0.76, 0.47, 0.49, 'glass', 'top'),
    bx(-0.4, 0.4, 0.89, 0.96, 0.44, 0.5, 'matte', 'accent'),
    bx(0.38, 0.42, 0.3, 0.64, 0.47, 0.5, 'metal', 'accent'),
  ]),
  // Washer-dryer stack: two cabinets, each with a round door, a porthole and a control strip.
  washer_dryer: model('washer_dryer', 'Washer-dryer stack', [
    bx(-0.5, 0.5, 0, 0.49, -0.5, 0.46, 'laminate', 'body'),
    bx(-0.48, 0.48, 0.49, 0.51, -0.48, 0.44, 'metal', 'accent'),
    bx(-0.5, 0.5, 0.51, 1, -0.5, 0.46, 'laminate', 'body'),
    ...[0.2, 0.71].flatMap(y => [
      { shape: 'cylinder', size: [0.7, 0.03, 0.3], at: [0, y, 0.475], rot: [Math.PI / 2, 0, 0], mat: 'metal', role: 'accent' },
      { shape: 'cylinder', size: [0.52, 0.025, 0.22], at: [0, y, 0.4875], rot: [Math.PI / 2, 0, 0], mat: 'glass', role: 'top' },
      bx(-0.46, 0.46, y + 0.21, y + 0.26, 0.46, 0.49, 'matte', 'accent'),
    ]),
  ], { body: 0xe6e4de }),
  // Hand sink: a vanity cabinet, a countertop with a basin, a backsplash, a faucet and two doors.
  hand_sink: model('hand_sink', 'Hand sink', [
    bx(-0.5, 0.5, 0, 0.78, -0.5, 0.44, 'laminate', 'body'),
    bx(-0.49, -0.005, 0.04, 0.74, 0.44, 0.48, 'laminate', 'body'),
    bx(0.005, 0.49, 0.04, 0.74, 0.44, 0.48, 'laminate', 'body'),
    bx(-0.08, -0.04, 0.6, 0.62, 0.48, 0.5, 'metal', 'accent'),
    bx(0.04, 0.08, 0.6, 0.62, 0.48, 0.5, 'metal', 'accent'),
    bx(-0.5, 0.5, 0.78, 0.84, -0.5, 0.5, 'ceramic', 'top'),
    bx(-0.24, 0.24, 0.84, 0.846, -0.2, 0.26, 'ceramic', 'accent'),
    bx(-0.5, 0.5, 0.84, 1, -0.5, -0.44, 'ceramic', 'top'),
    cy(0, -0.36, 0.05, 0.06, 0.84, 0.96, 'metal', 'accent'),
    bx(-0.02, 0.02, 0.93, 0.96, -0.36, -0.2, 'metal', 'accent'),
  ]),
});

// Item type (backend) or page catalog id -> model id. A live page id `type@room` is looked up by `type`.
// The two surface lines (flooring and wall paint) are areas, not objects: they map to null.
export const ITEM_MODELS = Object.freeze({
  treatment_bed: 'treatment_bed', storage_cabinet: 'storage_cabinet', task_lighting: 'task_lamp',
  hand_sink: 'hand_sink', reception_desk: 'reception_desk', shelving_unit: 'steel_shelving',
  desk: 'reception_desk', 'desk-s': 'reception_desk', bench: 'bench', retail: 'wall_shelving', bed: 'treatment_bed',
  stool: 'stool', cart: 'trolley', steamer: 'steamer', lamp: 'task_lamp', warmer: 'towel_warmer',
  shelf: 'steel_shelving', washer: 'washer_dryer',
  flooring_lvp_sqm: null, wall_paint_sqm: null,
});
// The backend's bill of materials gives no size. These nominal sizes (metres: width, depth, height)
// are typical for the kind of item, for drawing only; the page must say they are not measured.
export const NOMINAL_SIZE = Object.freeze({
  treatment_bed: [1.89, 0.76, 0.73], storage_cabinet: [0.9, 0.45, 1.8], task_lighting: [0.4, 0.4, 1.5],
  hand_sink: [0.6, 0.5, 1.0], reception_desk: [1.52, 0.67, 1.07], shelving_unit: [1.22, 0.46, 1.83],
});
export const NOMINAL_NOTE = 'Nominal size for this kind of item: the bill of materials gives no measurement.';

const baseType = id => String(id ?? '').split('@')[0];
export function modelFor(itemId) {
  const k = baseType(itemId), own = (o, key) => Object.prototype.hasOwnProperty.call(o, key);
  const m = own(ITEM_MODELS, k) ? ITEM_MODELS[k] : own(MODELS, k) ? k : undefined; // an item id, else a model id
  return m ? MODELS[m] : null;
}
export function nominalSize(itemId) {
  const s = NOMINAL_SIZE[baseType(itemId)];
  return s ? { width: s[0], depth: s[1], height: s[2], nominal: true, note: NOMINAL_NOTE } : null;
}

/* --------------------------------------------------------------------------- validation */

// Rotation matrix for Euler XYZ, as three.js composes it (R = Rx * Ry * Rz), row-major 3x3.
function eulerXYZ([x, y, z] = [0, 0, 0]) {
  const a = Math.cos(x), b = Math.sin(x), c = Math.cos(y), d = Math.sin(y), e = Math.cos(z), f = Math.sin(z);
  return [[c * e, -c * f, d], [a * f + b * d * e, a * e - b * d * f, -b * c], [b * f - a * d * e, b * e + a * d * f, a * c]];
}
// A part's axis-aligned bounds in model space: the rotated corners of its unit-cube primitive.
export function partBounds(p) {
  const R = eulerXYZ(p.rot), h = p.size.map(s => s / 2), min = [...p.at], max = [...p.at];
  for (let i = 0; i < 3; i++) {
    const r = Math.abs(R[i][0]) * h[0] + Math.abs(R[i][1]) * h[1] + Math.abs(R[i][2]) * h[2];
    min[i] = p.at[i] - r; max[i] = p.at[i] + r;
  }
  return { min, max };
}
export function modelBounds(m) {
  const min = [Infinity, Infinity, Infinity], max = [-Infinity, -Infinity, -Infinity];
  for (const p of m.parts) { const b = partBounds(p); for (let i = 0; i < 3; i++) { min[i] = Math.min(min[i], b.min[i]); max[i] = Math.max(max[i], b.max[i]); } }
  return { min, max };
}
// Plain reasons a model is invalid; empty when it is valid.
export function validate(m) {
  const out = [], lo = [-0.5, 0, -0.5], hi = [0.5, 1, 0.5], tol = 1e-6, num = v => typeof v === 'number' && Number.isFinite(v);
  if (!m || typeof m !== 'object') return ['not a model'];
  if (m.version !== FORMAT_VERSION) out.push(`version must be ${FORMAT_VERSION}`);
  if (typeof m.id !== 'string' || !m.id) out.push('missing id');
  if (m.approximate !== true) out.push('approximate must be true');
  if (typeof m.note !== 'string' || !m.note) out.push('missing note');
  if (!Array.isArray(m.parts) || !m.parts.length) return [...out, 'no parts'];
  if (m.parts.length > PART_BUDGET) out.push(`${m.parts.length} parts, over the budget of ${PART_BUDGET}`);
  m.parts.forEach((p, i) => {
    const n = `part ${i + 1}`;
    if (!SHAPES.includes(p.shape)) out.push(`${n}: unknown shape ${p.shape}`);
    if (!MATERIALS[p.mat]) out.push(`${n}: unknown material ${p.mat}`);
    if (!ROLES.includes(p.role)) out.push(`${n}: unknown colour role ${p.role}`);
    const vec = v => Array.isArray(v) && v.length === 3 && v.every(num);
    if (!vec(p.size) || !vec(p.at) || (p.rot !== undefined && !vec(p.rot))) { out.push(`${n}: size, at and rot must be three numbers`); return; }
    if (p.size.some(s => s <= 0)) { out.push(`${n}: size must be positive`); return; }
    const b = partBounds(p);
    if (b.min.some((v, k) => v < lo[k] - tol) || b.max.some((v, k) => v > hi[k] + tol)) out.push(`${n}: leaves the unit box`);
  });
  return out;
}

/* ------------------------------------------------------------------------------ builder */

const caches = new WeakMap();
function cacheOf(THREE) {
  let c = caches.get(THREE);
  if (!c) caches.set(THREE, (c = { geometries: new Map(), materials: new Map(), refs: new Map() }));
  return c;
}
function geometry(THREE, c, shape, radius, RoundedBox) {
  const rounded = shape === 'rbox' && RoundedBox, key = rounded ? `rbox:${radius ?? 0.1}` : shape === 'rbox' ? 'box' : shape;
  let g = c.geometries.get(key);
  if (!g) {
    g = key === 'box' ? new THREE.BoxGeometry(1, 1, 1)
      : key === 'cylinder' ? new THREE.CylinderGeometry(0.5, 0.5, 1, 16) // 16 sides keep the bounds at ±0.5
      : key === 'sphere' ? new THREE.SphereGeometry(0.5, 16, 8)
      : new RoundedBox(1, 1, 1, 2, Math.min(0.5, radius ?? 0.1));
    c.geometries.set(key, g);
  }
  return g;
}
function colourOf(role, palette, m, state) {
  if (state === 'selected' && role !== 'accent') return palette.selected ?? DEFAULT_PALETTE.selected;
  return palette[role] ?? m.colors?.[role] ?? DEFAULT_PALETTE[role];
}
function material(THREE, c, mat, colour, state) {
  const key = `${mat}|${colour}|${state}`;
  let x = c.materials.get(key);
  if (!x) {
    const base = MATERIALS[mat], proposed = state === 'proposed';
    x = new THREE.MeshStandardMaterial({ color: colour, roughness: base.roughness, metalness: base.metalness,
      transparent: proposed || !!base.transparent, opacity: (proposed ? 0.35 : 1) * (base.opacity ?? 1), depthWrite: !(proposed || base.transparent) });
    c.materials.set(key, x);
  }
  return x;
}
const hold = (c, o) => c.refs.set(o, (c.refs.get(o) || 0) + 1);

// One THREE.Group: the footprint centred on the origin, sitting on Y = 0, the bounding box exactly
// width × height × depth (metres). `model` is a model or an item id; an unknown id gives a placeholder box.
// options: { width, depth, height, palette, state: 'placed' | 'proposed' | 'selected', RoundedBoxGeometry }.
export function build(THREE, modelOrId, { width, depth, height, palette = {}, state = 'placed', RoundedBoxGeometry } = {}) {
  if (![width, depth, height].every(v => typeof v === 'number' && Number.isFinite(v) && v > 0)) throw new RangeError('width, depth and height must be positive numbers (metres)');
  let m = typeof modelOrId === 'string' ? modelFor(modelOrId) : modelOrId;
  const placeholder = !m || validate(m).length > 0;
  if (placeholder) m = { id: 'placeholder', parts: [bx(-0.5, 0.5, 0, 1, -0.5, 0.5, 'matte', 'body')] };
  const c = cacheOf(THREE), b = modelBounds(m), ext = b.max.map((v, i) => v - b.min[i]);
  const root = new THREE.Group(), inner = new THREE.Group();
  // Stretch the model's actual extent to the requested size, centred on X and Z, floor at Y = 0.
  inner.scale.set(width / ext[0], height / ext[1], depth / ext[2]);
  inner.position.set(-((b.min[0] + b.max[0]) / 2) * width / ext[0], -b.min[1] * height / ext[1], -((b.min[2] + b.max[2]) / 2) * depth / ext[2]);
  for (const p of m.parts) {
    const g = geometry(THREE, c, p.shape, p.radius, RoundedBoxGeometry), x = material(THREE, c, p.mat, colourOf(p.role, palette, m, state), state);
    const mesh = new THREE.Mesh(g, x);
    mesh.scale.set(...p.size); mesh.position.set(...p.at); if (p.rot) mesh.rotation.set(p.rot[0], p.rot[1], p.rot[2]);
    mesh.userData = { furniturePart: true, mat: p.mat, role: p.role };
    hold(c, g); hold(c, x); inner.add(mesh);
  }
  root.add(inner);
  root.userData = { furniture: true, model: m.id, placeholder, approximate: true, note: placeholder ? PLACEHOLDER_NOTE : NOTE, size: { width, depth, height }, state };
  return root;
}
const meshesOf = root => (root?.children?.[0]?.children || []).filter(o => o.userData?.furniturePart);
// Recolour a built group for a new state or palette without rebuilding its geometry.
export function setState(THREE, root, state, palette = {}) {
  const c = cacheOf(THREE), m = MODELS[root.userData.model] || {};
  for (const mesh of meshesOf(root)) {
    const next = material(THREE, c, mesh.userData.mat, colourOf(mesh.userData.role, palette, m, state), state);
    if (next !== mesh.material) { release(c, mesh.material); hold(c, next); mesh.material = next; }
  }
  root.userData.state = state;
  return root;
}
function release(c, o) {
  const n = (c.refs.get(o) || 0) - 1;
  if (n > 0) { c.refs.set(o, n); return; }
  c.refs.delete(o); o.dispose?.();
  for (const map of [c.geometries, c.materials]) for (const [k, v] of map) if (v === o) map.delete(k);
}
// Remove a built group from its parent; shared geometries and materials are disposed when no group uses them.
export function dispose(THREE, root) {
  const c = cacheOf(THREE);
  for (const mesh of meshesOf(root)) { release(c, mesh.geometry); release(c, mesh.material); }
  root.children[0]?.clear?.();
  root.parent?.remove(root);
}
export function cacheStats(THREE) { const c = cacheOf(THREE); return { geometries: c.geometries.size, materials: c.materials.size }; }
// From a raycast hit to the built group it belongs to (the page raycasts recursively).
export function rootOf(object) { let o = object; while (o && !o.userData?.furniture) o = o.parent; return o || null; }

/* ------------------------------------------------------------------ placement (pure, metres) */
// Floor plane coordinates are the page scene's (x, z): x to the right, z down the floor plan image.
// A position is the CENTRE of the item's footprint. `rotation` is radians about the vertical axis,
// the same value as the group's rotation.y (positive turns +x toward -z: anticlockwise on the plan).
// Rooms are polygons [[x, z], ...] in the same frame, convex or not. Distances within 1 mm count as touching.
const TOL = 1e-3;

const finite = v => typeof v === 'number' && Number.isFinite(v);
// Refuses a non-finite position or rotation and a size that is not positive (RangeError).
export function footprint({ x, z, width, depth, rotation = 0 }) {
  if (![x, z, rotation, width, depth].every(finite) || !(width > 0 && depth > 0)) throw new RangeError('footprint needs finite x, z, rotation and positive width and depth (metres)');
  const c = Math.cos(rotation), s = Math.sin(rotation), hw = width / 2, hd = depth / 2;
  return [[-hw, -hd], [hw, -hd], [hw, hd], [-hw, hd]].map(([u, v]) => [x + u * c + v * s, z - u * s + v * c]);
}
export function snap(position, rotation = 0, { step = 0.1, angle = 15 } = {}) {
  const r = v => Number((Math.round(v / step) * step).toFixed(9)), a = (angle * Math.PI) / 180, full = 2 * Math.PI;
  let rot = Math.round(rotation / a) * a; rot = ((rot % full) + full) % full; if (full - rot < EPS) rot = 0;
  return { x: r(position.x), z: r(position.z), rotation: Number(rot.toFixed(12)) };
}
const cross = (o, a, b) => (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
function segDist(p, a, b) {
  const dx = b[0] - a[0], dz = b[1] - a[1], L = dx * dx + dz * dz, t = L ? Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dz) / L)) : 0;
  return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dz);
}
const edges = poly => poly.map((p, i) => [p, poly[(i + 1) % poly.length]]);
// Inside (true), outside (false), with points within TOL of an edge counted as inside.
function inPolygon(p, poly) {
  if (edges(poly).some(([a, b]) => segDist(p, a, b) <= TOL)) return true;
  let inside = false;
  for (const [a, b] of edges(poly)) if ((a[1] > p[1]) !== (b[1] > p[1]) && p[0] < a[0] + ((p[1] - a[1]) * (b[0] - a[0])) / (b[1] - a[1])) inside = !inside;
  return inside;
}
// Strictly inside a convex polygon, more than TOL from its edges.
function deepInConvex(p, poly) {
  const sign = Math.sign(cross(poly[0], poly[1], poly[2]));
  return edges(poly).every(([a, b]) => (sign * cross(a, b, p)) / Math.hypot(b[0] - a[0], b[1] - a[1]) > TOL);
}
// Two segments cross at interior points of both (touching or sharing a line does not count).
function properCross([a, b], [c, d]) {
  const n1 = Math.hypot(b[0] - a[0], b[1] - a[1]), n2 = Math.hypot(d[0] - c[0], d[1] - c[1]);
  const d1 = cross(a, b, c) / n1, d2 = cross(a, b, d) / n1, d3 = cross(c, d, a) / n2, d4 = cross(c, d, b) / n2;
  return ((d1 > TOL && d2 < -TOL) || (d1 < -TOL && d2 > TOL)) && ((d3 > TOL && d4 < -TOL) || (d3 < -TOL && d4 > TOL));
}
// The footprint (corner list) lies inside the room polygon, touching the walls allowed.
export function fits(fp, room) {
  if (!fp.every(p => finite(p[0]) && finite(p[1]))) throw new RangeError('footprint has a non-finite corner');
  if (!fp.every(p => inPolygon(p, room))) return false;
  if (room.some(v => deepInConvex(v, fp))) return false; // a room corner (an L's inner corner) pokes in
  const fe = edges(fp), re = edges(room);
  if (fe.some(e => re.some(r => properCross(e, r)))) return false;
  const mid = [fp.reduce((s, p) => s + p[0], 0) / fp.length, fp.reduce((s, p) => s + p[1], 0) / fp.length];
  return inPolygon(mid, room);
}
// Two convex footprints share area (separating axis test). Touching edges do not overlap.
export function overlaps(a, b) {
  if (![...a, ...b].every(p => finite(p[0]) && finite(p[1]))) throw new RangeError('footprint has a non-finite corner');
  for (const poly of [a, b]) for (const [p, q] of edges(poly)) {
    const nx = q[1] - p[1], nz = p[0] - q[0], len = Math.hypot(nx, nz), pr = s => s.map(v => (v[0] * nx + v[1] * nz) / len);
    const A = pr(a), B = pr(b);
    if (Math.max(...A) <= Math.min(...B) + TOL || Math.max(...B) <= Math.min(...A) + TOL) return false;
  }
  return true;
}
// Plain reasons a placement is a problem. item and others: { id?, name, x, z, width, depth, rotation };
// room: { polygon: [[x, z], ...] }. Empty when it fits this sketch (which is not an approved plan).
export function issues(item, others = [], room) {
  let fp;
  try { fp = footprint(item); } catch { return ['size or position is not a valid number']; }
  const out = [];
  if (room?.polygon && !fits(fp, room.polygon)) out.push('outside the room');
  for (const o of others) {
    if (o === item || (o.id != null && o.id === item.id)) continue;
    let ofp;
    try { ofp = footprint(o); } catch { out.push(`cannot check against ${o.name || 'another item'}: invalid size or position`); continue; }
    if (overlaps(fp, ofp)) out.push(`overlaps ${o.name || 'another item'}`);
  }
  return out;
}
