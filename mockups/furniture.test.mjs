// node mockups/furniture.test.mjs: the furniture models, builder and placement helpers, with a minimal THREE stub.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as F from './furniture.js';

let passed = 0, failed = 0;
function test(name, fn) {
  try { fn(); passed++; console.log(`ok   ${name}`); } catch (e) { failed++; console.log(`FAIL ${name}\n     ${e.message}`); }
}

/* ---- a minimal THREE stub: transforms, groups, meshes, and counted geometries and materials */
function stub() {
  const counts = { geometries: 0, materials: 0, disposed: 0 };
  const vec = () => ({ x: 0, y: 0, z: 0, set(x, y, z) { this.x = x; this.y = y; this.z = z; return this; } });
  class Object3D {
    constructor() { this.position = vec(); this.rotation = vec(); this.scale = vec().set(1, 1, 1); this.children = []; this.parent = null; this.userData = {}; }
    add(o) { o.parent = this; this.children.push(o); return this; }
    remove(o) { this.children = this.children.filter(c => c !== o); o.parent = null; return this; }
    clear() { this.children.forEach(c => { c.parent = null; }); this.children = []; return this; }
  }
  class Geometry { constructor(kind, args) { this.kind = kind; this.args = args; counts.geometries++; } dispose() { counts.disposed++; } }
  return {
    counts,
    THREE: {
      Group: class extends Object3D {},
      Mesh: class extends Object3D { constructor(g, m) { super(); this.geometry = g; this.material = m; this.isMesh = true; } },
      BoxGeometry: class extends Geometry { constructor(...a) { super('box', a); } },
      CylinderGeometry: class extends Geometry { constructor(...a) { super('cylinder', a); } },
      SphereGeometry: class extends Geometry { constructor(...a) { super('sphere', a); } },
      MeshStandardMaterial: class { constructor(p) { Object.assign(this, p); counts.materials++; } dispose() { counts.disposed++; } },
    },
  };
}
// World bounds of a built group, computed here independently: each unit-cube corner through
// mesh (scale, rotate XYZ as three.js does, translate), then every parent's transform.
function rotXYZ([x, y, z], r) {
  // three.js Euler 'XYZ' applies Rz first, then Ry, then Rx to a vector.
  let [a, b, c] = [x, y, z];
  [a, b] = [a * Math.cos(r.z) - b * Math.sin(r.z), a * Math.sin(r.z) + b * Math.cos(r.z)];
  [a, c] = [a * Math.cos(r.y) + c * Math.sin(r.y), -a * Math.sin(r.y) + c * Math.cos(r.y)];
  [b, c] = [b * Math.cos(r.x) - c * Math.sin(r.x), b * Math.sin(r.x) + c * Math.cos(r.x)];
  return [a, b, c];
}
function toParent(o, p) {
  const s = [p[0] * o.scale.x, p[1] * o.scale.y, p[2] * o.scale.z], r = rotXYZ(s, o.rotation);
  return [r[0] + o.position.x, r[1] + o.position.y, r[2] + o.position.z];
}
function worldBounds(root) {
  const min = [Infinity, Infinity, Infinity], max = [-Infinity, -Infinity, -Infinity];
  (function walk(o) {
    if (o.isMesh) for (const x of [-0.5, 0.5]) for (const y of [-0.5, 0.5]) for (const z of [-0.5, 0.5]) {
      let p = [x, y, z], n = o;
      while (n) { p = toParent(n, p); n = n.parent; }
      for (let i = 0; i < 3; i++) { min[i] = Math.min(min[i], p[i]); max[i] = Math.max(max[i], p[i]); }
    }
    o.children.forEach(walk);
  })(root);
  return { min, max };
}

const BACKEND = ['treatment_bed', 'storage_cabinet', 'task_lighting', 'hand_sink', 'reception_desk', 'shelving_unit'];
// The page's catalog ids (mockups/workspace.js, const CAT), read from the file so a new item fails here.
const page = readFileSync(new URL('./workspace.js', import.meta.url), 'utf8');
const PAGE = [...page.slice(page.indexOf('const CAT = ['), page.indexOf('const C = ')).matchAll(/\{ id: '([^']+)'/g)].map(m => m[1]);

test('the page catalog was read', () => assert.ok(PAGE.length >= 12 && PAGE.includes('washer') && PAGE.includes('bed'), PAGE.join()));
test('every backend item type and page item has a model', () => {
  for (const id of [...BACKEND, ...PAGE, 'treatment_bed@treatment', 'shelf@storage']) assert.ok(F.modelFor(id), id);
});
test('surface lines and unknown types have no model', () => {
  for (const id of ['flooring_lvp_sqm', 'wall_paint_sqm', 'jacuzzi', '', undefined, 'constructor', '__proto__']) assert.equal(F.modelFor(id), null, String(id));
});
test('every model validates, is approximate and carries the note', () => {
  for (const [id, m] of Object.entries(F.MODELS)) {
    assert.deepEqual(F.validate(m), [], id);
    assert.equal(m.approximate, true); assert.match(m.note, /approximate/i); assert.match(m.note, /not the product/);
  }
});
test('part budget: every model within it, and the validator rejects one part over', () => {
  for (const m of Object.values(F.MODELS)) assert.ok(m.parts.length <= F.PART_BUDGET);
  const p = { shape: 'box', size: [0.1, 0.1, 0.1], at: [0, 0.5, 0], mat: 'wood', role: 'body' };
  const ok = { ...F.MODELS.stool, parts: Array(F.PART_BUDGET).fill(p) }, over = { ...ok, parts: Array(F.PART_BUDGET + 1).fill(p) };
  assert.deepEqual(F.validate(ok), []);
  assert.match(F.validate(over).join(), /over the budget of 40/);
});
test('the validator rejects a part outside the unit box, rotated out, or of zero size', () => {
  const base = F.MODELS.stool, part = q => ({ ...base, parts: [{ shape: 'box', size: [0.2, 0.2, 0.2], at: [0, 0.5, 0], mat: 'wood', role: 'body', ...q }] });
  assert.match(F.validate(part({ at: [0.45, 0.5, 0] })).join(), /leaves the unit box/);
  assert.match(F.validate(part({ at: [0, 0.05, 0] })).join(), /leaves the unit box/); // below the floor
  assert.match(F.validate(part({ size: [1, 0.1, 0.4], rot: [0, 0.3, 0] })).join(), /leaves the unit box/); // fits unrotated only
  assert.deepEqual(F.validate(part({ size: [1, 0.1, 0.1] })), []);
  assert.match(F.validate(part({ size: [0, 0.1, 0.1] })).join(), /size must be positive/);
  assert.match(F.validate(part({ size: [-1, 0.1, 0.1] })).join(), /size must be positive/);
  assert.match(F.validate(part({ shape: 'torus' })).join(), /unknown shape/);
  assert.match(F.validate({ ...part({}), approximate: false }).join(), /approximate must be true/);
});
test('built bounding box equals the requested size, footprint centred, on the floor (three sizes, all models)', () => {
  const { THREE } = stub();
  for (const [id, m] of Object.entries(F.MODELS)) for (const [w, d, h] of [[1.89, 0.76, 0.73], [0.4, 0.4, 1.5], [3.1, 0.07, 2.2]]) {
    const g = F.build(THREE, m, { width: w, depth: d, height: h }), b = worldBounds(g), mm = 1e-3;
    const msg = `${id} ${w}x${d}x${h}: ${JSON.stringify(b)}`;
    assert.ok(Math.abs(b.max[0] - b.min[0] - w) < mm && Math.abs(b.max[1] - b.min[1] - h) < mm && Math.abs(b.max[2] - b.min[2] - d) < mm, msg);
    assert.ok(Math.abs(b.min[1]) < mm && Math.abs(b.min[0] + b.max[0]) < mm && Math.abs(b.min[2] + b.max[2]) < mm, msg);
  }
});
test('the built group is labelled approximate with the note', () => {
  const { THREE } = stub(), g = F.build(THREE, 'bed', { width: 1.9, depth: 0.8, height: 0.7 });
  assert.equal(g.userData.approximate, true); assert.equal(g.userData.placeholder, false); assert.equal(g.userData.note, F.NOTE); assert.equal(g.userData.model, 'treatment_bed');
});
test('thirty identical stools share geometries and materials; dispose frees them', () => {
  const { THREE, counts } = stub(), room = new THREE.Group(), stools = [];
  for (let i = 0; i < 30; i++) { const g = F.build(THREE, 'stool', { width: 0.46, depth: 0.46, height: 0.52 }); room.add(g); stools.push(g); }
  const shapes = new Set(F.MODELS.stool.parts.map(p => p.shape)), mats = new Set(F.MODELS.stool.parts.map(p => `${p.mat}|${p.role}`));
  assert.equal(counts.geometries, shapes.size); // box, cylinder, sphere: 3, not 30 × 14
  assert.ok(counts.materials <= mats.size, `${counts.materials} materials`);
  assert.deepEqual(F.cacheStats(THREE), { geometries: counts.geometries, materials: counts.materials });
  const meshes = new Set(stools.flatMap(s => s.children[0].children.map(c => c.geometry)));
  assert.equal(meshes.size, shapes.size);
  stools.slice(0, 29).forEach(s => F.dispose(THREE, s));
  assert.equal(counts.disposed, 0, 'still used by the last stool');
  assert.equal(room.children.length, 1);
  F.dispose(THREE, stools[29]);
  assert.equal(counts.disposed, counts.geometries + counts.materials);
  assert.deepEqual(F.cacheStats(THREE), { geometries: 0, materials: 0 });
});
test('selection and proposed states recolour through the cache, without new geometry', () => {
  const { THREE, counts } = stub(), g = F.build(THREE, 'desk', { width: 1.5, depth: 0.67, height: 1.07 }), geo = counts.geometries;
  F.setState(THREE, g, 'selected');
  const body = g.children[0].children.find(c => c.userData.role === 'body');
  assert.equal(body.material.color, F.DEFAULT_PALETTE.selected);
  F.setState(THREE, g, 'proposed');
  assert.equal(body.material.transparent, true); assert.equal(body.material.opacity, 0.35);
  assert.equal(counts.geometries, geo);
  const custom = F.build(THREE, 'desk', { width: 1, depth: 1, height: 1, palette: { body: 0x123456 } });
  assert.equal(custom.children[0].children.find(c => c.userData.role === 'body').material.color, 0x123456);
});
test('an unknown type gives a placeholder box of the requested size, never another object', () => {
  const { THREE } = stub();
  for (const id of ['jacuzzi', 'flooring_lvp_sqm']) {
    const g = F.build(THREE, id, { width: 2, depth: 1, height: 0.5 }), b = worldBounds(g);
    assert.equal(g.userData.placeholder, true); assert.equal(g.userData.model, 'placeholder'); assert.equal(g.userData.note, F.PLACEHOLDER_NOTE);
    assert.equal(g.children[0].children.length, 1);
    assert.deepEqual(b.max.map((v, i) => +(v - b.min[i]).toFixed(9)), [2, 0.5, 1]);
  }
  assert.throws(() => F.build(THREE, 'stool', { width: 0, depth: 1, height: 1 }), RangeError);
});
test('a rounded box uses RoundedBoxGeometry when the caller passes it, a plain box otherwise', () => {
  const s = stub(); let made = 0;
  class RoundedBoxGeometry { constructor(...a) { this.args = a; made++; } dispose() {} }
  const g = F.build(s.THREE, 'bench', { width: 1.7, depth: 0.55, height: 0.46, RoundedBoxGeometry });
  assert.ok(made >= 1);
  assert.ok(g.children[0].children.some(c => c.geometry instanceof RoundedBoxGeometry));
  const plain = F.build(stub().THREE, 'bench', { width: 1.7, depth: 0.55, height: 0.46 });
  assert.ok(plain.children[0].children.every(c => ['box', 'cylinder', 'sphere'].includes(c.geometry.kind)));
});
test('nominal sizes exist for every backend type and say they are not measured', () => {
  for (const id of BACKEND) { const s = F.nominalSize(id); assert.ok(s && s.nominal && s.width > 0, id); assert.match(s.note, /no measurement/); }
  assert.equal(F.nominalSize('bed'), null);
});
test('rootOf finds the built group from a part hit by a raycast', () => {
  const { THREE } = stub(), g = F.build(THREE, 'lamp', { width: 0.4, depth: 0.4, height: 1.5 });
  assert.equal(F.rootOf(g.children[0].children[3]), g); assert.equal(F.rootOf(new THREE.Group()), null);
});

/* ---- placement */
const close = (a, b) => Math.abs(a - b) < 1e-9;
test('footprint: centre position, rotation as rotation.y', () => {
  const fp = F.footprint({ x: 1, z: 2, width: 2, depth: 1 });
  assert.deepEqual(fp, [[0, 1.5], [2, 1.5], [2, 2.5], [0, 2.5]]);
  const r = F.footprint({ x: 0, z: 0, width: 2, depth: 1, rotation: Math.PI / 2 });
  // +x of the item turns toward -z: the width now runs along z.
  assert.ok(r.every(([x, z]) => Math.abs(x) <= 0.5 + 1e-9 && Math.abs(z) <= 1 + 1e-9));
  assert.ok(close(r[1][0], -0.5) && close(r[1][1], -1)); // local (+w/2, -d/2) -> (-0.5, -1)
});
test('snap: 0.1 m and 15 degrees by default, rotation kept in [0, 2pi)', () => {
  assert.deepEqual(F.snap({ x: 1.234, z: -0.06 }, 0.3), { x: 1.2, z: -0.1, rotation: Number((Math.PI / 12).toFixed(12)) });
  assert.equal(F.snap({ x: 0.05, z: 0.149 }, 0).x, 0.1); assert.equal(F.snap({ x: 0.05, z: 0.149 }, 0).z, 0.1);
  assert.equal(F.snap({ x: 0, z: 0 }, -0.1).rotation, 0);
  assert.equal(F.snap({ x: 0, z: 0 }, 2 * Math.PI - 0.05).rotation, 0);
  assert.ok(close(F.snap({ x: 0, z: 0 }, -Math.PI / 2).rotation, 1.5 * Math.PI));
  assert.deepEqual(F.snap({ x: 0.26, z: 0.74 }, 1, { step: 0.25, angle: 90 }), { x: 0.25, z: 0.75, rotation: Number((Math.PI / 2).toFixed(12)) });
});
const RECT = [[0, 0], [4, 0], [4, 3], [0, 3]];
const L = [[0, 0], [4, 0], [4, 2], [2, 2], [2, 4], [0, 4]]; // an L: the top-right 2 x 2 is missing
const at = (x, z, width, depth, rotation = 0) => F.footprint({ x, z, width, depth, rotation });
test('fits: rectangle room, inside, touching a wall, crossing a wall', () => {
  assert.equal(F.fits(at(2, 1.5, 1, 1), RECT), true);
  assert.equal(F.fits(at(0.5, 0.5, 1, 1), RECT), true); // touches two walls
  assert.equal(F.fits(at(0.45, 1.5, 1, 1), RECT), false);
  assert.equal(F.fits(at(10, 10, 1, 1), RECT), false);
});
test('fits: L-shaped room, including the inner corner', () => {
  assert.equal(F.fits(at(1, 3, 1.8, 1.8), L), true); // in the left leg
  assert.equal(F.fits(at(3, 1, 1.8, 1.8), L), true); // in the bottom leg
  assert.equal(F.fits(at(2.5, 2.5, 0.8, 0.8), L), false); // in the missing corner
  assert.equal(F.fits(at(2, 2, 1, 1), L), false); // corners straddle, the inner corner pokes in
  assert.equal(F.fits(at(2.2, 2, 3.2, 0.6), L), false); // bridges past the inner corner along x
  assert.equal(F.fits(at(1.5, 1.5, 1, 1), L), true); // touches the inner corner at its own corner
});
test('fits: a rotated item at the boundary', () => {
  const h = Math.SQRT2 / 2; // a 1 x 1 square turned 45 degrees reaches h from its centre
  assert.equal(F.fits(at(h, 1.5, 1, 1, Math.PI / 4), RECT), true); // its corner touches the wall at x = 0
  assert.equal(F.fits(at(h - 0.01, 1.5, 1, 1, Math.PI / 4), RECT), false);
  assert.equal(F.fits(at(0.5, 1.5, 1, 1, Math.PI / 4), RECT), false); // unrotated it would fit
  assert.equal(F.fits(at(0.5, 1.5, 1, 1, Math.PI / 2), RECT), true); // a quarter turn of a square still fits
});
test('overlaps: touching, overlapping, rotated', () => {
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(1, 0, 1, 1)), false); // edges touch
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(1, 1, 1, 1)), false); // corners touch
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(0.9, 0, 1, 1)), true);
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(0, 0, 0.2, 0.2)), true); // one inside the other
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(1.2, 0, 1, 1, Math.PI / 4)), true); // the turned corner reaches 0.49
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(1.22, 0, 1, 1, Math.PI / 4)), false); // its corner stops at 0.513
  assert.equal(F.overlaps(at(0, 0, 1, 1), at(0.5 + Math.SQRT2 / 2, 0, 1, 1, Math.PI / 4)), false); // corner touches the edge
  assert.equal(F.overlaps(at(0, 0, 2, 0.2, 0.3), at(0, 0.6, 2, 0.2, 0.3)), false); // parallel, turned, apart
});
test('issues: plain wording', () => {
  const room = { polygon: RECT }, bed = { id: 1, name: 'Facial bed', x: 2, z: 1.5, width: 1.9, depth: 0.8, rotation: 0 };
  const stool = { id: 2, name: 'Stool', x: 2.5, z: 1.6, width: 0.5, depth: 0.5 }, lamp = { id: 3, name: 'Lamp', x: 3.8, z: 2.9, width: 0.4, depth: 0.4 };
  assert.deepEqual(F.issues(bed, [bed, stool, lamp], room), ['overlaps Stool']);
  assert.deepEqual(F.issues(lamp, [bed, stool], room), ['outside the room']);
  assert.deepEqual(F.issues({ ...stool, x: 3.9 }, [bed, { name: '' , x: 3.9, z: 1.6, width: 0.2, depth: 0.2 }], room), ['outside the room', 'overlaps another item']);
  assert.deepEqual(F.issues({ ...bed, x: 1 }, [stool], room), []);
});

test('degenerate placement input is refused, never "fits" (review)', () => {
  assert.throws(() => F.footprint({ x: 1, z: 1, width: 0, depth: 0 }), RangeError);
  assert.throws(() => F.footprint({ x: NaN, z: 1, width: 1, depth: 1 }), RangeError);
  assert.throws(() => F.overlaps([[NaN, 0], [1, 0], [1, 1], [0, 1]], F.footprint({ x: 0, z: 0, width: 1, depth: 1 })), RangeError);
  assert.deepEqual(F.issues({ name: 'X', x: 1, z: 1, width: 0, depth: 0 }, [], { polygon: RECT }), ['size or position is not a valid number']);
  assert.deepEqual(F.issues({ name: 'X', x: 1, z: 1, width: 0.5, depth: 0.5 }, [{ name: 'Y', x: NaN, z: 1, width: 1, depth: 1 }], { polygon: RECT }), ['cannot check against Y: invalid size or position']);
});
test('a model id builds that model, not a placeholder (review)', () => {
  for (const id of Object.keys(F.MODELS)) assert.equal(F.modelFor(id)?.id, id);
  assert.equal(F.modelFor('sofa'), null);
});

/* ---- the file itself */
const src = readFileSync(new URL('./furniture.js', import.meta.url), 'utf8');
test('no brand, product name or money amount in the file', () => {
  const brands = page.slice(page.indexOf('const CAT = ['), page.indexOf('const C = ')).matchAll(/by: '([^']+)'/g);
  for (const [, b] of brands) assert.ok(!src.includes(b), `names ${b}`);
  assert.doesNotMatch(src, /\$\s?\d|\b\d+\s?(usd|dollars?|cents)\b|\bprice\b/i);
  for (const [, n] of page.slice(page.indexOf('const CAT = ['), page.indexOf('const C = ')).matchAll(/name: '([^']+)'/g)) assert.ok(!src.includes(n), `names ${n}`);
});
test('the file imports nothing', () => {
  assert.doesNotMatch(src, /^\s*import\b|\bimport\s*\(|\brequire\s*\(/m);
});

console.log(`\n${passed} passed, ${failed} failed`);
process.exit(failed ? 1 : 0);
