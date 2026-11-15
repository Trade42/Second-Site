// Tests for backend.js. No packages: `node mockups/backend.test.mjs`.
// (a) the pure mapping functions against recorded answers of the real service (backend.fixtures.json);
// (b) with BACKEND_URL set (for example http://127.0.0.1:8000), the adapter end to end against the
//     running Rust service (and the Python app behind it for the chat), plus the CORS answer for
//     MOCKUP_ORIGIN (default http://localhost:8090). Without BACKEND_URL part (b) prints SKIPPED.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as B from './backend.js';

const F = JSON.parse(readFileSync(new URL('./backend.fixtures.json', import.meta.url)));
let pass = 0, fail = 0;
async function t(name, fn) {
  try { await fn(); pass++; console.log('ok   ', name); } catch (e) { fail++; console.log('FAIL ', name, '\n     ', e.message.split('\n').slice(0, 6).join('\n      ')); }
}

/* ----------------------------------------------------------------- (a) */
await t('config: off without ?live=1, default origin, api override', () => {
  assert.deepEqual(B.config(''), { live: false, api: 'http://localhost:8000' });
  assert.equal(B.config('?live=0').live, false);
  assert.deepEqual(B.config('?live=1&api=http://127.0.0.1:9000/'), { live: true, api: 'http://127.0.0.1:9000' });
});
await t('money and geometry convert exactly, for display only', () => {
  assert.equal(B.toDollars(400000), 4000); assert.equal(B.toDollars(1377650), 13776.5); assert.equal(B.toDollars(null), null); assert.equal(B.toDollars(12.5), null);
  assert.equal(B.toSqft(148.6), 1600); assert.equal(B.toSqft(139.4), 1500); assert.equal(B.toSqft(undefined), null);
});
await t('profile and search body: only what was answered, in minor units', () => {
  const p = B.profileFromAnswers({ business: 'Facial studio', zones: ['downtown'], rent: 5500, rooms: 3 });
  assert.equal(p.business_type, 'facial_studio'); assert.equal(p.monthly_rent_limit, 550000);
  assert.deepEqual(p.space_requirements, { required_rooms: { treatment: 3, reception: 1 } });
  assert.equal('opening_capital' in p, false, 'capital was never asked');
  assert.equal(B.profileFromAnswers({ business: 'A pilates studio' }).business_type, 'a_pilates_studio');
  assert.equal(B.searchBody({ rent: 5500 }).max_monthly_rent, 550000); assert.equal(B.searchBody({}).max_monthly_rent, null);
  // space_requirements is required by the contract: {} until the rooms answer, never an invented count.
  for (const req of [{ business: 'Facial studio' }, { business: 'Facial studio', rooms: 0 }, { business: 'Facial studio', rooms: null, rent: null, zones: null }]) {
    const q = B.profileFromAnswers(req);
    assert.deepEqual(q.space_requirements, {}); assert.equal(B.roomsKnown(req), false); assert.equal(q.monthly_rent_limit, null);
    assert.equal(validProfile(q), null);
  }
  assert.equal(validProfile(B.profileFromAnswers({})), null, 'business_type falls back to a non-empty value');
});
const [alh, hw] = F.properties;
// The fields BusinessExpansionProfile requires (apps/backend/crates/core/src/project.rs), checked like the backend's 422.
function validProfile(q) {
  if (typeof q?.business_type !== 'string' || !q.business_type.length) return 'business_type';
  if (!Number.isInteger(q.current_location_count) || q.current_location_count < 0) return 'current_location_count';
  if (!q.target_market || typeof q.target_market !== 'object') return 'target_market';
  if (!q.space_requirements || typeof q.space_requirements !== 'object') return 'space_requirements';
  for (const [k, v] of Object.entries(q.space_requirements.required_rooms || {})) if (!Number.isInteger(v) || v <= 0) return `required_rooms.${k}`;
  if (q.monthly_rent_limit != null && !(Number.isInteger(q.monthly_rent_limit) && q.monthly_rent_limit >= 0)) return 'monthly_rent_limit';
  return null;
}
const bodies = [];
await t('POOL entry: synthetic listing labelled as demo, never a real listing asset', () => {
  for (const { record } of F.properties) {
    const p = B.poolEntry(record);
    assert.equal(record.commercial_listing.data_origin, 'FIXTURE_SYNTHETIC');
    assert.equal(p.live.fixture, true); assert.match(p.suite, /Demo data/);
    assert.notEqual(p.id, p.src, 'the page labels "Demo property" when id !== src');
    assert.ok(p.src.startsWith('live-') && !['mateo-109', 'sixth-110b', 'beverly-720'].includes(p.src));
    assert.equal(p.rent, record.commercial_listing.asking_rent_monthly / 100);
    assert.match(B.asset(p.src, p.photo), /^data:image\/svg\+xml/); assert.match(decodeURIComponent(B.asset(p.src, 'matterport-floorplan-08.png')), /No floor plan image/);
  }
  assert.equal(B.asset('mateo-109', 'costar-01.jpg'), undefined, 'the page keeps its own assets for its own listings');
  const h = B.poolEntry(hw.record);
  assert.equal(h.address, '6922 W HOLLYWOOD BLVD'); assert.equal(h.short, '6922 Hollywood'); assert.equal(h.sqft, 1500); assert.equal(h.rent, 6000);
  assert.match(h.suite, /^STE 302 · Demo data$|^302 · Demo data$|Demo data/);
  assert.equal(h.zone, null, 'Hollywood is in none of the page\'s four areas');
  assert.equal(B.poolEntry(alh.record).zone, 'eastside');
});
// Hand-written records in the shape of CoStar data-kit answers (invented addresses, ids, scans, URLs).
const K = F.synthetic_kit_shaped.records, [k1, k2, k3, k4, k5] = K, KP = Object.fromEntries(K.map(r => { const p = B.poolEntry(r, 'http://127.0.0.1:8010'); return [p.id, p]; }));
const notDemo = s => assert.doesNotMatch(s, /Demo data|demo|synthetic|fixture|\blive\b/i, s);
const clean = s => { for (const bad of ['NaN', 'undefined', 'null', '$0', '[object', '··', ' ·  ·']) assert.ok(!String(s).includes(bad), `"${bad}" in ${s}`); assert.ok(String(s).trim(), 'empty'); };
await t('labels: a CoStar kit record is a CoStar listing (data-kit snapshot), never demo, synthetic or live', () => {
  const p = KP[k1.property_id];
  assert.equal(p.suite, 'Suite 210 · CoStar listing (data-kit snapshot)'); notDemo(p.suite);
  assert.equal(p.live.source, 'snapshot'); assert.equal(p.live.fixture, false);
  assert.equal(p.src, p.id, 'no "Demo property" label for a real listing (the page shows it when id !== src)');
  assert.equal(B.listingSource(k5.commercial_listing).label, 'CoStar listing (live)');
  assert.equal(B.listingSource({ provider: 'fixture', data_origin: 'FIXTURE_SYNTHETIC' }).label, 'Demo data');
  assert.equal(B.listingSource({}).label, 'Source not stated');
  for (const { record } of F.properties) { const s = B.poolEntry(record); assert.match(s.suite, / · Demo data$/); assert.notEqual(s.src, s.id); }
  const derived = B.evidenceClassOf({ status: 'MAY_REQUIRE_WORK', uncertainty: 'From the listing text only', evidence_ids: [] }, k1);
  assert.equal(derived.text, 'Derived from the CoStar listing (data-kit snapshot), not measured'); notDemo(derived.text);
  assert.equal(B.evidenceClassOf({ status: 'MAY_REQUIRE_WORK', uncertainty: 'listing', evidence_ids: [] }, hw.record).text, 'Derived from the listing (demo data), not measured');
});
await t('search note: each kind counted separately, nothing said for a zero count', () => {
  const syn = F.properties.map(x => B.poolEntry(x.record)), real = [KP[k1.property_id], KP[k2.property_id]];
  assert.equal(B.searchNote(real), 'Of 2 listings from the backend, 2 are real CoStar listings from the data-kit snapshot (dated records, not a live feed). 1 has a 3D scan of the suite itself, so a layout can be generated there.');
  assert.equal(B.searchNote(syn), 'Of 2 listings from the backend, 2 are synthetic demo data (fixture), not CoStar records. 1 has a 3D scan of the suite itself, so a layout can be generated there.');
  assert.equal(B.searchNote([syn[0]]), 'Of 1 listing from the backend, 1 is synthetic demo data (fixture), not CoStar records. None has a 3D scan of the suite itself, so no layout can be generated yet.');
  assert.equal(B.poolEntry(hw.record).live.scan_text, '3D scan of this suite (demo data)');
  assert.equal(B.searchNote([...real.slice(0, 1), ...syn.slice(0, 1), KP[k5.property_id]]), 'Of 3 listings from the backend, 1 is a real CoStar listing from the data-kit snapshot (dated records, not a live feed); 1 from a live listing provider; 1 is synthetic demo data (fixture), not CoStar records. 1 has a 3D scan of the suite itself, so a layout can be generated there.');
  assert.equal(B.searchNote([]), null);
});
await t('images: CoStar photo first, else the Vexcel aerial route, else the neutral image', () => {
  const p1 = KP[k1.property_id], p2 = KP[k2.property_id], p3 = KP[k3.property_id];
  assert.equal(p1.photo, 'listing-photo'); assert.equal(B.asset(p1.src, p1.photo, KP), k1.commercial_listing.photo_urls[0]);
  assert.equal(B.asset(p1.src, 'vexcel-oblique-n.jpg', KP), k1.exterior_assets[0].url, 'the aerial view shows the ortho image');
  assert.equal(p2.photo, 'aerial-vexcel'); assert.equal(B.asset(p2.src, p2.photo, KP), 'http://127.0.0.1:8010/api/v1/media/vexcel/ortho/' + k2.property_id, 'a relative route gets the backend origin');
  assert.equal(p3.photo, 'none'); assert.match(decodeURIComponent(B.asset(p3.src, p3.photo, KP)), /No photo.*CoStar listing \(data-kit snapshot\); no image provided/);
  assert.match(decodeURIComponent(B.asset(p1.src, 'matterport-floorplan-08.png', KP)), /No floor plan image/);
  assert.equal(B.asset('mateo-109', 'costar-01.jpg', KP), undefined, 'the page keeps its own assets');
  const n1 = B.spaceNotes(k1, 'http://127.0.0.1:8010').description, n2 = B.spaceNotes(k2, 'http://127.0.0.1:8010').description;
  assert.equal(B.imagesOf(k2).aerial, null, 'a relative route without the backend origin is not handed to the page');
  assert.equal(B.safeUrl('javascript:alert(1)'), null);
  assert.equal(B.safeUrl('https://cdn.example.invalid/a"b<c>.jpg?x="y"'), 'https://cdn.example.invalid/a%22b%3Cc%3E.jpg?x=%22y%22');
  const odd = B.poolEntry({ ...k1, property_identity: { ...k1.property_identity, street_address: '1 <b>Main</b> St', unit_or_suite: 'Suite <i>9</i>' } });
  assert.doesNotMatch(odd.address + odd.short + odd.suite, /[<>]/, 'workspace.js puts these into chat HTML unescaped');
  assert.match(n1, /^CoStar listing \(data-kit snapshot\)\. 3D scan of this suite\. Image: CoStar photo\. Also: Aerial image, Vexcel, captured 2026-03-14\./);
  assert.match(n2, /Building scans only\. Image: Aerial image, Vexcel, capture date not stated\./);
  notDemo(n1.replace(/CoStar data kit snapshot.*$/, ''));
});
await t('scans: suite scan vs building scans only, the scan id and label kept for the page', () => {
  assert.deepEqual(KP[k1.property_id].live.scan, { asset_id: 'SYNSCAN0001', label: 'Suite 210', data_origin: 'LIVE', area_sqm: 118.4 });
  assert.equal(KP[k1.property_id].live.scan_text, '3D scan of this suite');
  assert.equal(KP[k2.property_id].live.scan, null); assert.equal(KP[k2.property_id].live.scan_text, 'Building scans only');
  assert.equal(KP[k2.property_id].live.scans[0].label, 'Lobby and common areas');
  assert.equal(KP[k3.property_id].live.scan_text, 'No 3D scan');
  assert.match(B.spaceNotes(k2).cons[0], /^Building scans only: no layout without a scan of the suite$/);
  assert.match(B.spaceNotes(k1).pros[0], /^3D scan of this suite/);
});
await t('awkward records: no rent, area, unit or coordinates; long unit; fractions; one building twice', () => {
  const p3 = KP[k3.property_id];
  assert.equal(p3.suite, 'Unit not stated · CoStar listing (data-kit snapshot) · rent not stated · area not stated');
  assert.equal(p3.rentKnown, false); assert.equal(p3.sqftKnown, false); assert.equal(p3.zone, null); assert.equal(p3.lat, undefined);
  const p4 = KP[k4.property_id];
  assert.ok(p4.suite.split(' · ')[0].length <= 32 && p4.suite.includes('…'), p4.suite); assert.equal(p4.live.unit, k4.property_identity.unit_or_suite, 'the full unit is kept');
  const p1 = KP[k1.property_id];
  assert.equal(p1.rent, 4125.5); assert.equal(p1.sqft, 1300); assert.ok(Number.isInteger(p1.sqft));
  const pair = B.disambiguate([B.poolEntry(k1), B.poolEntry(k2)]);
  assert.deepEqual(pair.map(p => p.short), ['100 Example, Suite 210', '100 Example, Suite 305']);
  for (const r of K) { const p = B.poolEntry(r), n = B.spaceNotes(r); for (const s of [p.suite, p.short, p.address, n.description, ...n.pros, ...n.cons]) clean(s); }
  assert.equal(B.poolEntry({ property_id: 'x', property_identity: {}, commercial_listing: {} }).short, 'Address not stated');
});
await t('prices and geometry: words for each origin, never a quote', () => {
  assert.equal(B.priceWords('FIXTURE_SYNTHETIC'), 'fixture price, not a quote');
  assert.equal(B.priceWords('FIXTURE_REAL'), 'catalog snapshot price, not a quote');
  assert.equal(B.priceWords('LIVE'), 'live catalog price, not a quote');
  assert.equal(B.priceWords(undefined), 'estimate, not a quote');
  assert.equal(B.geometryWords({ geometry_source: 'MATTERPORT_SCAN', data_origin: 'LIVE', scale_verified: false }), 'matterport scan (live provider data), scale not verified');
  assert.equal(B.geometryWords({ geometry_source: 'FIXTURE', data_origin: 'FIXTURE_SYNTHETIC', scale_verified: false }), 'fixture (synthetic fixture), scale not verified');
});
await t('findings: UNKNOWN stays unknown, no dollar figure, evidence class kept in words', () => {
  for (const { record, feasibility } of F.properties) {
    const issues = B.issuesFrom(feasibility, record);
    assert.equal(issues.length, feasibility.length);
    assert.equal(new Set(issues.map(i => i.id)).size, issues.length, 'unique ids');
    for (const [k, i] of issues.entries()) {
      const a = feasibility[k];
      if (a.status === 'UNKNOWN') { assert.equal(i.kind, 'unknown'); assert.equal(i.sev, 'unk'); assert.equal(i.impact, 'Unknown'); }
      if (a.status !== 'MEETS') assert.notEqual(i.sev, 'low', 'only MEETS is low');
      assert.doesNotMatch(i.impact, /\$/, 'no dollar impact');
      assert.match(i.ev, /^(Historical|Derived|Unknown|Conflicting|Measured|Visual|Verified|Owner-reported) evidence: /);
      assert.equal(i.ask, undefined, 'no broker draft in live mode');
    }
  }
  const elec = B.issuesFrom(alh.feasibility, alh.record).find(i => i.id === 'power');
  assert.equal(elec.status, 'UNKNOWN'); assert.equal(elec.evidence_class, 'historical'); assert.match(elec.ev, /not current capacity/);
  const area = B.issuesFrom(hw.feasibility, hw.record).find(i => i.id === 'layout');
  assert.equal(area.evidence_class, 'derived');
});
await t('costs: opening cost = renovation + FF&E exactly, NNN as extras, no schedule', () => {
  for (const s of F.cost_scenarios) {
    const c = B.costsFrom(F.cost_scenarios, s.property_id);
    assert.equal(Math.round((c.build + c.other) * 100), s.opening_cost_expected);
    assert.equal(c.extras * 100, s.line_items.find(l => l.category === 'NNN').expected);
    assert.equal(c.weeks, null);
  }
  assert.deepEqual(B.costsFrom([], 'x'), { extras: null, build: null, other: null, weeks: null, scenario: null });
});
await t('layout: rooms from the polygons (metres -> plan fractions), all proposed', () => {
  const { plan, rooms } = B.roomsFrom(F.layout, F.geometry);
  assert.deepEqual(plan, { w: 1200, h: 1160, ink: [0, 0], mesh: [0, 0], box: [0, 0, 0, 0] });
  assert.equal(rooms.length, 5); assert.ok(rooms.every(r => r.proposed === 1));
  const t1 = rooms.find(r => r.id === 'treatment-1');
  assert.deepEqual(t1.r.map(v => +v.toFixed(4)), [0, 0.4138, 0.4167, 0.7586]); assert.equal(t1.name, 'Treatment 1'); assert.equal(t1.dim, '215 SF');
  assert.deepEqual([...new Set(rooms.map(r => r.type))].sort(), ['reception', 'storage', 'treatment']);
});
await t('bill of materials: fixture prices labelled, no size/supplier/lead invented, totals exact', () => {
  const { rooms } = B.roomsFrom(F.layout, F.geometry), out = B.catalogFrom(F.bom, rooms);
  assert.equal(out.placed.length, F.bom.items.reduce((n, i) => n + (i.unit === 'ea' ? Number(i.quantity) : 1), 0));
  assert.ok(out.cat.some(c => /m2, fixture price/.test(c.name)), 'measured items keep quantity and unit');
  const sum = out.placed.reduce((n, i) => n + Math.round(out.cat.find(c => c.id === i.c).price * 100), 0);
  assert.equal(sum, F.bom.estimated_total);
  for (const c of out.cat) { assert.match(c.name, /fixture price, not a quote/); assert.equal(c.w + c.d + c.h, 0); assert.equal(c.by, 'Not available'); assert.equal(c.lead, 'Not available'); }
});
await t('owner texts: only Photon design-change records, owner-reported answer only', () => {
  const rec = { communication_id: 'c1', purpose: 'design change', channel: 'PHOTON_SMS', created_at: 'x', structured_findings: [{ question: 'design change', answer: 'move reception to the window', classification: 'OWNER_REPORTED' }] };
  assert.deepEqual(B.ownerTextNotes([rec, { ...rec, communication_id: 'c2', purpose: 'broker outreach' }]).map(x => [x.id, x.text]), [['c1', 'move reception to the window']]);
});
await t('chat failures: plain words, never a scripted answer', () => {
  assert.match(B.chatFailure({ status: 503, data: { error_code: 'PYTHON_SERVICE_NOT_CONFIGURED' } }), /^The assistant is not reachable.*Nothing was answered/);
  assert.match(B.chatFailure({ status: 502, data: { error_code: 'PYTHON_SERVICE_UNAVAILABLE' } }), /not reachable right now/);
  assert.match(B.chatFailure({ status: 504 }), /not reachable right now/);
  assert.match(B.chatFailure({ status: 0, message: 'The backend at x cannot be reached' }), /not reachable/);
});

await t('chat routing: search questions and page commands stay on the page', () => {
  assert.equal(B.turn('discover', 'A café', { step: 0 }, 5), null);
  assert.equal(B.turn('discover', 'Up to $7,500', { step: 5 }, 5), null);
  assert.equal(B.turn('discover', 'Look into them', { step: 5 }, 5), null);
  assert.equal(B.turn('compare', 'Try a layout', { step: 5 }, 5), null);
  assert.equal(B.turn('compare', 'Choose it anyway', { step: 5 }, 5), null);
});

// A browser's window.fetch throws "Illegal invocation" unless it is called with `this` undefined or the
// window; Node's fetch does not care. This stub behaves like the browser's.
const routes = [];
function strictFetch(url, opts = {}) {
  if (this !== undefined && this !== globalThis) throw new TypeError("Failed to execute 'fetch' on 'Window': Illegal invocation");
  const path = new URL(url).pathname, m = opts.method || 'GET', ids = [k1.property_id, k2.property_id, hw.record.property_id];
  routes.push(`${m} ${path.replace(/[0-9a-f-]{36}/g, ':id')}`);
  const body = opts.body ? JSON.parse(opts.body) : undefined; bodies.push({ route: routes.at(-1), body });
  const bad = path === '/api/v1/projects' && m === 'POST' ? validProfile(body.business_profile) : path.endsWith('/business-profile') ? validProfile(body) : null;
  if (bad) return Promise.resolve({ ok: false, status: 422, text: async () => JSON.stringify({ error_code: 'CONTRACT_VALIDATION_ERROR', details: [{ loc: [bad], msg: 'Field required' }] }) });
  const recs = { [k1.property_id]: k1, [k2.property_id]: k2, [hw.record.property_id]: hw.record };
  const answer = (() => {
    if (path === '/api/v1/dev/token') return { access_token: 'stub-token' };
    if (path === '/api/v1/dev/fixtures/project') return {};
    if (path === '/api/v1/projects' && m === 'POST') return { project: { project_id: '11111111-1111-4111-8111-111111111111' } };
    if (path.endsWith('/business-profile') && m === 'PUT') return body;
    if (path.endsWith('/properties/search')) return { task: { status: 'COMPLETED', output_reference: { property_ids: ids, ranking_id: 'rk' } } };
    if (path.endsWith('/properties')) return Object.values(recs);
    if (path.endsWith('/rankings')) return [{ ...F.ranking, ranking_id: 'rk', ranked_properties: ids.map(property_id => ({ property_id })) }];
    if (path.endsWith('/cost-scenarios')) return F.cost_scenarios;
    if (path.endsWith('/feasibility')) return path.includes(hw.record.property_id) ? hw.feasibility : [];
    if (path.endsWith('/permits')) return [];
    if (path.endsWith('/layouts/generate')) return { task: { status: 'COMPLETED', output_reference: { design_version_ids: [F.layout.design_version_id] } } };
    if (path.endsWith('/layouts')) return [F.layout];
    if (path.includes('/layouts/geometry/')) return { ...F.geometry, data_origin: 'LIVE', geometry_source: 'MATTERPORT_MEASURED' };
    if (path.endsWith('/bom')) return F.bom;
    if (path.endsWith('/communications')) return [];
    if (path.endsWith('/chat')) return F.chat;
    return null;
  })();
  return Promise.resolve({ ok: answer !== null, status: answer === null ? 404 : 200, text: async () => JSON.stringify(answer) });
}
await t('browser fetch: the client calls fetch without itself as `this` (fails on `this.fetch = fetchImpl`)', async () => {
  const be = new B.Backend('http://127.0.0.1:9', strictFetch);
  await be.signIn(); assert.equal(be.token, 'stub-token');
});
await t('page wiring: bind, search, asset, furnish and turn as workspace.js calls them, browser-strict fetch', async () => {
  const notes = [], said = [], real = globalThis.fetch;
  const ctx = { POOL: [{ id: 'x' }], P: { x: 1 }, D: {}, CAT: [], C: {}, FIRST: ['x'], FILL: [], SPACE_NOTES: {}, pins: {}, a: h => said.push(h), note: h => notes.push(h), esc: B.html, selectSpace() {} };
  const S = { stage: 'discover', step: 5, req: { business: 'Facial studio', rent: null, rooms: 3 }, slots: [], saved: ['gone'], open: null, finalist: null, chosen: null, placed: [] };
  globalThis.fetch = strictFetch; // bind() builds its client with the global fetch, as the page does
  try {
    B.bind({ ...ctx, S: () => S, map: () => null }); // workspace.js line 841 (no map in Node: addPin returns early)
    await B.search(S); // workspace.js line 212
    assert.deepEqual(notes.filter(n => /failed/i.test(n)), [], notes.join(' | '));
    assert.equal(ctx.POOL.length, 3); assert.deepEqual(S.saved, []);
    assert.ok(notes.includes(B.html('Of 3 listings from the backend, 2 are real CoStar listings from the data-kit snapshot (dated records, not a live feed); 1 is synthetic demo data (fixture), not CoStar records. 2 have a 3D scan of the suite itself, so a layout can be generated there.')), notes.join(' | '));
    for (const p of ctx.POOL) assert.ok(ctx.D[p.src]?.issues && ctx.SPACE_NOTES[p.id], p.id);
    const p1 = ctx.P[k1.property_id], ph = ctx.P[hw.record.property_id];
    assert.equal(B.asset(p1.src, p1.photo), k1.commercial_listing.photo_urls[0], 'workspace.js line 11 calls asset(src, f) with two arguments');
    assert.deepEqual(ctx.D[p1.src].photos, ['listing-photo', 'aerial-vexcel']);
    assert.match(decodeURIComponent(B.asset(ph.src, ph.photo)), /Demo data from the backend \(fixture\)/);
    assert.equal(B.asset('mateo-109', 'costar-01.jpg'), undefined);
    S.finalist = k1.property_id;
    assert.equal(await B.furnish(S), true); // workspace.js line 388
    assert.equal(ctx.D[p1.src].rooms.length, 5); assert.ok(S.placed.length > 0);
    assert.ok(notes.some(n => n.includes('Floor geometry: matterport measured (live provider data), scale not verified')), notes.at(-1));
    S.stage = 'compare';
    await B.turn('compare', 'what about parking', S, 5); // workspace.js line 146
    assert.ok(said.at(-1)?.includes('from the backend'), said.join(' | '));
    assert.ok(routes.includes('POST /api/v1/projects/:id/chat') && routes.includes('POST /api/v1/projects/:id/layouts/generate'), routes.join(', '));
  } finally { globalThis.fetch = real; }
});

await t('page wiring: no project or search before the rooms answer, one note; then POST, then PUT on a change', async () => {
  const notes = [], real = globalThis.fetch;
  const ctx = { POOL: [], P: {}, D: {}, CAT: [], C: {}, FIRST: [], FILL: [], SPACE_NOTES: {}, pins: {}, a() {}, note: h => notes.push(h), esc: B.html, selectSpace() {} };
  const S = { stage: 'discover', step: 1, req: {}, slots: [], saved: [], open: null, finalist: null, chosen: null, placed: [] };
  globalThis.fetch = strictFetch; routes.length = 0; bodies.length = 0;
  try {
    B.bind({ ...ctx, S: () => S, map: () => null });
    await B.search(S); assert.equal(routes.length, 0, 'no business yet: nothing sent'); assert.equal(notes.length, 0);
    S.req.business = 'Facial studio'; await B.search(S);                 // answer 1
    S.req.zones = ['downtown']; await B.search(S);                       // answer 2
    S.req.rent = null; await B.search(S);                                // answer 3, "No limit"
    assert.equal(routes.length, 0, routes.join(', '));
    assert.deepEqual(notes, ['Live results start once you have said how many rooms you need.'], 'said once');
    S.req.rooms = 3; await B.search(S);                                  // answer 4
    const post = bodies.find(b => b.route === 'POST /api/v1/projects');
    assert.deepEqual(post.body.business_profile.space_requirements, { required_rooms: { treatment: 3, reception: 1 } });
    assert.equal(post.body.business_profile.monthly_rent_limit, null);
    assert.ok(!notes.some(n => /failed|422/i.test(n)), notes.join(' | ')); assert.equal(ctx.POOL.length, 3);
    S.req.opening = 'By March'; routes.length = 0; await B.search(S);    // answer 5: same profile, no PUT
    assert.ok(!routes.some(r => r.includes('business-profile')) && routes.includes('POST /api/v1/projects/:id/properties/search'), routes.join(', '));
    S.req.rooms = 4; S.req.rent = 5500; bodies.length = 0; await B.search(S); // a changed answer after the project exists
    const put = bodies.find(b => b.route === 'PUT /api/v1/projects/:id/business-profile');
    assert.deepEqual(put.body.space_requirements, { required_rooms: { treatment: 4, reception: 1 } }); assert.equal(put.body.monthly_rent_limit, 550000);
    assert.equal(bodies.filter(b => b.route === 'POST /api/v1/projects').length, 0, 'the project is not created twice');
    assert.equal(bodies.find(b => b.route.endsWith('/properties/search')).body.max_monthly_rent, 550000);
    assert.ok(!notes.some(n => /failed|422/i.test(n)), notes.join(' | '));
  } finally { globalThis.fetch = real; }
});

await t('review: two searches at once create one project; short is safe for the page regex; FAILS does not meet', async () => {
  const real = globalThis.fetch;
  const ctx = { POOL: [], P: {}, D: {}, CAT: [], C: {}, FIRST: [], FILL: [], SPACE_NOTES: {}, pins: {}, a() {}, note() {}, esc: B.html, selectSpace() {} };
  const S = { stage: 'discover', step: 5, req: { business: 'Facial studio', rooms: 3 }, slots: [], saved: [], open: null, finalist: null, chosen: null, placed: [] };
  globalThis.fetch = strictFetch; routes.length = 0;
  try {
    B.bind({ ...ctx, S: () => S, map: () => null });
    await Promise.all([B.search(S), B.search({ ...S, req: { ...S.req, rent: 5500 } })]); // chat answer + inline edit
    assert.equal(routes.filter(r => r === 'POST /api/v1/projects').length, 1, routes.join(', '));
  } finally { globalThis.fetch = real; }
  const rec = u => ({ ...k1, property_identity: { ...k1.property_identity, unit_or_suite: u } });
  for (const p of B.disambiguate([B.poolEntry(rec('Suite (Rear) *')), B.poolEntry(rec('[2]+'))])) {
    assert.doesNotThrow(() => new RegExp(p.short.split(' ').pop(), 'i'), p.short); assert.ok(p.short.trim(), 'empty');
  }
  const f = B.issuesFrom([{ requirement_id: 'facial.space.min_area', status: 'FAILS', evidence_ids: [] }], k1)[0];
  assert.equal(f.sev, 'high'); assert.equal(f.impact, 'Does not meet');
});

/* ----------------------------------------------------------------- (b) */
const URL_ = process.env.BACKEND_URL, ORIGIN = process.env.MOCKUP_ORIGIN || 'http://localhost:8090';
if (!URL_) console.log('SKIPPED (b) end to end: BACKEND_URL is not set');
else {
  const be = new B.Backend(URL_.replace(/\/+$/, ''));
  const seen = [];
  const raw = be.fetch; be.fetch = (u, o) => { seen.push(`${o.method} ${u.replace(URL_, '').replace(/[0-9a-f-]{36}/g, ':id')}`); return raw(u, o); };
  await t(`CORS: preflight and a real request from ${ORIGIN} are allowed`, async () => {
    const pre = await fetch(URL_ + '/api/v1/dev/token', { method: 'OPTIONS', headers: { Origin: ORIGIN, 'Access-Control-Request-Method': 'POST', 'Access-Control-Request-Headers': 'authorization,content-type' } });
    console.log('      preflight', pre.status, 'allow-origin', pre.headers.get('access-control-allow-origin'), 'allow-headers', pre.headers.get('access-control-allow-headers'), 'allow-methods', pre.headers.get('access-control-allow-methods'));
    assert.equal(pre.status, 200); assert.equal(pre.headers.get('access-control-allow-origin'), ORIGIN);
    const real = await fetch(URL_ + '/api/v1/dev/fixtures/project', { headers: { Origin: ORIGIN } });
    console.log('      GET', real.status, 'allow-origin', real.headers.get('access-control-allow-origin'));
    assert.equal(real.headers.get('access-control-allow-origin'), ORIGIN);
    const other = await fetch(URL_ + '/health', { headers: { Origin: 'http://evil.example' } });
    assert.equal(other.headers.get('access-control-allow-origin'), null, 'another origin is not allowed');
  });
  let results;
  await t('stage 1 search: dev sign-in, project + profile from the answers, search, ranked listings', async () => {
    const req = { business: 'Facial studio', zones: null, rent: null, rooms: 3 };
    results = await be.search(req);
    assert.ok(results.length >= 2, `${results.length} listings`);
    const pool = B.disambiguate(results.map(r => B.poolEntry(r.rec, be.api)));
    for (const [i, p] of pool.entries()) {
      // The label follows the record's own origin, never a hard-coded one.
      const src = B.listingSource(results[i].rec.commercial_listing);
      assert.ok(p.suite.includes(src.label), p.suite);
      if (src.kind === 'synthetic') { assert.match(p.suite, /Demo data/); assert.notEqual(p.src, p.id); }
      else { assert.doesNotMatch(p.suite, /Demo data|synthetic/i); assert.equal(p.src, p.id); }
      if (src.kind === 'snapshot' && p.live.provider === 'costar') assert.match(p.suite, /CoStar listing \(data-kit snapshot\)/);
      for (const s of [p.suite, p.short, B.spaceNotes(results[i].rec, be.api).description]) for (const bad of ['NaN', 'undefined', '$0']) assert.ok(!s.includes(bad), s);
    }
    const kinds = pool.reduce((m, p) => ({ ...m, [p.live.source]: (m[p.live.source] || 0) + 1 }), {});
    // Real kit addresses are not printed: the source, a masked unit, the image and scan words only.
    const show = p => (p.live.source === 'synthetic' ? `${p.address} [${p.suite}]` : `[${p.suite.replace(/^[^·]*·/, '<unit> ·')}] ${p.photo} / ${p.live.scan_text}`);
    console.log('      kinds:', JSON.stringify(kinds), '; first three:', pool.slice(0, 3).map(show).join('; '));
    console.log('      note:', B.searchNote(pool));
    const narrower = await be.search({ ...req, rent: 5000 }); // a changed answer: PUT business-profile, then search again
    assert.ok(narrower.every(r => r.rec.commercial_listing.asking_rent_monthly <= 500000), 'rent limit honoured');
    console.log('      with rent up to $5,000:', narrower.length, 'listings');
  });
  await t('images: the listing photo link or the Vexcel aerial route (no token) for each listing', async () => {
    const pool = results.map(r => B.poolEntry(r.rec, be.api));
    console.log('      photo', pool.filter(p => p.live.photo_url).length, 'aerial', pool.filter(p => p.live.aerial).length, 'neither', pool.filter(p => p.photo === 'none').length, 'of', pool.length);
    const withAerial = pool.find(p => p.live.aerial);
    if (!withAerial) { console.log('      no listing has a Vexcel aerial image (fixture stack)'); return; }
    const r = await fetch(withAerial.live.aerial.url, { headers: { Origin: ORIGIN } }); // like an <img>: no bearer token
    console.log('      GET aerial ->', r.status, r.headers.get('content-type'), '|', withAerial.live.aerial.caption);
    assert.equal(r.status, 200); assert.match(r.headers.get('content-type') || '', /^image\//);
  });
  await t('stage 2 compare: findings and costs for each listing', async () => {
    for (const { rec, assessments, scenarios } of results) {
      const d = B.detailEntry(rec, assessments, scenarios);
      assert.ok(d.issues.length > 0); assert.ok(d.build > 0);
      const who = B.listingSource(rec.commercial_listing).kind === 'synthetic' ? rec.property_identity.street_address : `listing ${results.indexOf(results.find(r => r.rec === rec)) + 1}`; // no kit addresses in logs
      console.log(`      ${who}: ${d.issues.map(i => `${i.id}=${i.status}/${i.kind}`).join(' ')}; renovation $${d.build}, FF&E $${d.other}, NNN ${d.extras == null ? 'not stated' : '$' + d.extras}`);
    }
  });
  await t('stage 3 furnish: layout generation and bill of materials', async () => {
    const hw = results.find(r => (r.rec.interior_assets || []).some(a => a.has_measured_geometry)); // the first with a measured scan
    assert.ok(hw, 'a listing with a measured scan');
    const out = await be.layout(hw.rec.property_id);
    assert.equal(out.layout.approval_status, 'PROPOSED');
    const { rooms } = B.roomsFrom(out.layout, out.geometry), cat = B.catalogFrom(out.bom, rooms);
    assert.ok(['FIXTURE_SYNTHETIC', 'FIXTURE_REAL', 'LIVE'].includes(out.geometry.data_origin), out.geometry.data_origin);
    const gw = B.geometryWords(out.geometry);
    if (out.geometry.data_origin === 'LIVE') assert.match(gw, /live provider data/); else if (out.geometry.data_origin === 'FIXTURE_SYNTHETIC') assert.match(gw, /synthetic fixture/);
    console.log(`      floor geometry: ${gw}; prices: ${[...new Set(out.bom.items.map(i => B.priceWords(i.price_origin)))].join('; ')}`);
    console.log(`      ${rooms.length} rooms (${rooms.map(r => r.name + ' ' + r.dim).join(', ')}); ${cat.placed.length} parts, total $${cat.estimated_total} (${out.bom.status})`);
    const noGeo = results.find(r => r !== hw && !(r.rec.interior_assets || []).some(a => a.has_measured_geometry));
    try { await be.layout(noGeo.rec.property_id); console.log('      layout for a listing without geometry: generated'); } catch (e) { console.log('      layout for a listing without geometry refused:', e.message); }
  });
  await t('chat through the Rust origin (forwarded to Python)', async () => {
    try { const out = await be.chat('rank the properties'); console.log(`      mode ${out.mode}: ${out.reply.split('\n')[0]}`); assert.ok(out.reply); } catch (e) { console.log('      ' + B.chatFailure(e)); throw e; }
  });
  await t('owner texts: the communications list route answers', async () => {
    const list = await be.communications(); console.log(`      ${list.length} communications, ${B.ownerTextNotes(list).length} owner design-change texts`); assert.ok(Array.isArray(list));
  });
  await t('nothing was sent: only allowed routes were called', () => {
    console.log('      routes:', [...new Set(seen)].join(', '));
    for (const s of seen) assert.doesNotMatch(s, /outreach|\/calls|approvals|nessie|handoffs|simulate|owner\/channel/);
  });
}
console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
