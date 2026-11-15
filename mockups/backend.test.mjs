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
  assert.deepEqual(B.config(''), { live: false, talk: false, api: 'http://localhost:8000' });
  assert.equal(B.config('?live=0').live, false);
  assert.deepEqual(B.config('?live=1&api=http://127.0.0.1:9000/'), { live: true, talk: false, api: 'http://127.0.0.1:9000' });
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

// web-link-3: saved copies of a suite's own scan. The places records here are SYNTHETIC (invented slugs,
// the synthetic records' invented scan ids); the real assets/places.json is never copied into a test.
const SYN_PLACES = () => [
  { slug: 'syn-suite-a', matterport_model_id: 'SYNSCAN0001', images: [
    { file: 'syn-suite-a/costar-01.jpg', source: 'costar', kind: 'photo' },
    { file: 'syn-suite-a/matterport-01.jpg', source: 'matterport', kind: 'photo' },
    { file: 'syn-suite-a/matterport-floorplan-08.png', source: 'matterport', kind: 'floorplan' },
    { file: 'syn-suite-a/vexcel-ortho.jpg', source: 'vexcel', kind: 'ortho' },
    { file: 'syn-suite-a/vexcel-oblique-n.jpg', source: 'vexcel', kind: 'oblique-n' },
    { file: 'syn-suite-a/../x.jpg', source: 'costar', kind: 'photo' }, { file: 'syn-suite-a/costar-"x.jpg', source: 'costar', kind: 'photo' },
    { file: 'other/costar-02.jpg', source: 'costar', kind: 'photo' }, { file: 'syn-suite-a/matterport-02.jpg', source: 'costar', kind: 'photo' },
  ] },
  { slug: 'syn-building', matterport_model_id: 'SYNSCAN0002', images: [{ file: 'syn-building/costar-01.jpg', source: 'costar', kind: 'photo' }] },
];
const SCRIPTED = 'SCRIPTED-DEMO-TEXT';
async function livePage(places, extra = {}) {
  const notes = [], real = globalThis.fetch;
  // The page's scripted tables keyed by the slug, as workspace.js has them before bind() (shape only).
  const ctx = { POOL: [{ id: 'syn-suite-a', src: 'syn-suite-a' }], P: { 'syn-suite-a': { id: 'syn-suite-a', rent: 999999 } }, CAT: [{ id: 'bed', name: SCRIPTED, price: 2450 }], C: {}, FIRST: ['syn-suite-a'], FILL: [], pins: {}, a() {}, note: h => notes.push(h), esc: B.html, selectSpace() {},
    D: { 'syn-suite-a': { extras: null, build: 142000, other: 29400, weeks: 21, photos: ['costar-01.jpg'], rooms: [{ id: 't1', name: SCRIPTED }], issues: [{ id: 'plumbing', title: SCRIPTED, impact: '~$46,000 · 6 wks' }] } },
    SPACE_NOTES: { 'syn-suite-a': { description: SCRIPTED, pros: [SCRIPTED], cons: [SCRIPTED] } } };
  const S = { stage: 'discover', step: 5, req: { business: 'Facial studio', rent: null, rooms: 3 }, slots: [], saved: [], open: null, finalist: null, chosen: null, placed: [] };
  globalThis.fetch = strictFetch; routes.length = 0; bodies.length = 0;
  try { B.bind({ ...ctx, S: () => S, map: () => null, ...extra }); B.useLocalPlaces(places); await B.search(S); } finally { globalThis.fetch = real; }
  return { ctx, S, notes };
}
// What workspace.js prints for a listing: card (252-255), finalist (348), hover (746-752), compare cost (343).
const printed = (ctx, p) => {
  const d = ctx.D[p.src], n = ctx.SPACE_NOTES[p.id];
  return [p.id !== p.src ? 'Demo property' : '', p.address, p.short, p.suite, String(p.rent), String(p.sqft), n.description, ...n.pros, ...n.cons, String(d.build), String(d.other), String(d.extras), ...d.issues.flatMap(i => [i.title, i.impact, i.sum]), ...d.rooms.map(r => r.name)].join(' | ');
};
await t('local copies: a listing whose own suite scan id is in places.json gets its local files and only backend facts', async () => {
  const plain = await livePage(null), { ctx, S, notes } = await livePage(SYN_PLACES()); // asset() reads the last bound page
  assert.deepEqual(notes.filter(n => /failed/i.test(n)), [], notes.join(' | '));
  const p = ctx.P['syn-suite-a'], q = plain.ctx.P[k1.property_id];
  assert.ok(p && !ctx.P[k1.property_id], 'keyed by the slug'); assert.equal(p.src, 'syn-suite-a'); assert.equal(p.live.property_id, k1.property_id);
  const text = printed(ctx, p);
  assert.doesNotMatch(text, /Demo property|SCRIPTED|142000|29400|46,000|999999|Demo data|synthetic|fixture/, text);
  for (const k of ['address', 'short', 'suite', 'rent', 'sqft', 'rentKnown', 'sqftKnown', 'use', 'lease', 'broker', 'lat', 'lon', 'zone', 'hood']) assert.deepEqual(p[k], q[k], k);
  const { photos: dp, ...dRest } = ctx.D[p.src], { photos: qp, ...qRest } = plain.ctx.D[q.src];
  assert.deepEqual(dRest, qRest, 'findings, costs and rooms are the backend ones, unchanged');
  assert.deepEqual(dp, ['costar-01.jpg', 'matterport-01.jpg', 'vexcel-ortho.jpg']);
  assert.equal(p.photo, 'costar-01.jpg');
  assert.equal(B.asset(p.src, p.photo), 'assets/syn-suite-a/costar-01.jpg');
  assert.equal(B.asset(p.src, 'matterport-01.jpg'), 'assets/syn-suite-a/matterport-01.jpg');
  assert.equal(B.asset(p.src, 'vexcel-oblique-n.jpg'), 'assets/syn-suite-a/vexcel-oblique-n.jpg');
  assert.equal(B.asset(p.src, 'vexcel-oblique-e.jpg'), q.live.aerial?.url ?? B.asset(q.src, 'vexcel-oblique-e.jpg'), 'a direction with no saved copy: as before');
  assert.match(decodeURIComponent(B.asset(p.src, 'matterport-floorplan-08.png')), /No floor plan image/, 'the scan floor plan image is not used (overlay would not line up)');
  for (const bad of ['../x.jpg', 'costar-"x.jpg', 'costar-02.jpg', 'matterport-02.jpg']) assert.ok(!B.asset(p.src, bad).startsWith('assets/'), bad);
  assert.equal(`assets/dollhouses/${p.src}.png`, 'assets/dollhouses/syn-suite-a.png'); assert.equal(`assets/meshes/${p.src}.obj`, 'assets/meshes/syn-suite-a.obj'); // workspace.js 587, 593
  const n = ctx.SPACE_NOTES[p.id].description;
  assert.match(n, /^CoStar listing \(data-kit snapshot\)\. 3D scan of this suite \(Matterport\): its mesh and dollhouse view and stills are saved copies from this scan, the photos are saved copies of this listing's CoStar photos, and the aerial images are saved Vexcel images of the building \(files on this computer\)\./, n);
  assert.doesNotMatch(n, /\blive\b/i);
  assert.equal(p.live.scan_text, '3D scan of this suite (Matterport)');
  assert.ok(ctx.CAT.every(c => c.name !== SCRIPTED) && !ctx.FIRST.length, 'scripted catalog and order are gone');
  // The layout request goes to the backend's own id.
  const real = globalThis.fetch; globalThis.fetch = strictFetch; bodies.length = 0;
  try { S.finalist = 'syn-suite-a'; assert.equal(await B.furnish(S), true); } finally { globalThis.fetch = real; }
  assert.equal(bodies.find(b => b.route.endsWith('/layouts/generate')).body.property_id, k1.property_id);
  assert.doesNotMatch(printed(ctx, p), /SCRIPTED/);
  // Unmatched: a building-only scan with a places.json id (k2) and the fixture listing are as before.
  for (const id of [k2.property_id, hw.record.property_id]) assert.deepEqual(ctx.P[id], plain.ctx.P[id], id);
  assert.deepEqual(ctx.D[ctx.P[k2.property_id].src], plain.ctx.D[plain.ctx.P[k2.property_id].src]);
  assert.match(decodeURIComponent(B.asset(ctx.P[hw.record.property_id].src, 'costar-01.jpg')), /Demo data from the backend \(fixture\)/);
});
await t('local copies: building-only scan, fixture listing, demo scan, other unit, duplicates and malformed places.json never match', async () => {
  const idx = B.localIndex(SYN_PLACES());
  assert.equal(B.localMatch(k1, idx)?.slug, 'syn-suite-a');
  assert.equal(B.localMatch(k2, idx), null, 'building-only scan, same id');
  const scan = k1.interior_assets[0];
  for (const g of [false, undefined, 'true']) assert.equal(B.localMatch({ ...k1, interior_assets: [{ ...scan, has_measured_geometry: g, matches_listing_unit: null }] }, idx), null, `building scan, geometry ${g}`);
  assert.equal(B.localMatch({ ...k1, interior_assets: [{ ...scan, data_origin: 'FIXTURE_SYNTHETIC' }] }, idx), null, 'demo scan');
  assert.equal(B.localMatch({ ...k1, interior_assets: [{ ...scan, matches_listing_unit: false }] }, idx), null, 'another unit');
  assert.equal(B.localMatch({ ...k1, commercial_listing: { ...k1.commercial_listing, data_origin: 'FIXTURE_SYNTHETIC' } }, idx), null, 'synthetic listing');
  assert.equal(B.localMatch(hw.record, new Map([[hw.record.interior_assets[0].asset_id, { slug: 'x', photos: [], aerial: {}, captions: {} }]])), null, 'fixture listing');
  assert.equal(B.localMatch({ ...k1, property_identity: { ...k1.property_identity, street_address: 'anything' }, interior_assets: [] }, idx), null, 'never by address');
  // review: the matched scan must be the one scansOf() (and so the hover text and live.scan) calls the suite's.
  for (const first of [{ ...scan, asset_id: 'SYNDEMO0001', data_origin: 'FIXTURE_SYNTHETIC' }, { ...scan, asset_id: 'SYNUNIT0001', matches_listing_unit: false }]) {
    assert.equal(B.localMatch({ ...k1, interior_assets: [first, scan] }, idx), null, `another measured scan named first: ${first.asset_id}`);
  }
  for (const bad of [null, undefined, 'x', 42, {}, [null, 1, 'a', { slug: '../x', matterport_model_id: 'SYNSCAN0001' }, { slug: 'ok', matterport_model_id: 'a b' }]]) assert.equal(B.localIndex(bad).size, 0, JSON.stringify(bad));
  assert.equal(B.localIndex([...SYN_PLACES(), { slug: 'dup', matterport_model_id: 'SYNSCAN0001', images: [] }]).has('SYNSCAN0001'), false, 'one id claimed twice: neither');
  assert.equal(B.localIndex([...SYN_PLACES(), { slug: 'syn-suite-a', matterport_model_id: 'SYNSCAN0009', images: [] }]).has('SYNSCAN0001'), false, 'one slug twice: neither');
  const { ctx: none } = await livePage(null);
  // Two listings of one result with the same suite scan: neither gets the slug.
  const keep = k2.interior_assets; k2.interior_assets = [{ ...k1.interior_assets[0] }];
  try {
    const { ctx } = await livePage(SYN_PLACES());
    assert.equal(ctx.P['syn-suite-a'], undefined); assert.ok(ctx.P[k1.property_id] && ctx.P[k2.property_id]);
  } finally { k2.interior_assets = keep; }
  for (const places of [null, [], 'garbage', [{ slug: 'syn-suite-a' }]]) {
    const { ctx } = await livePage(places);
    assert.deepEqual(ctx.POOL, none.POOL, JSON.stringify(places)); assert.deepEqual(ctx.SPACE_NOTES, none.SPACE_NOTES);
  }
  // The browser loader: missing (404), unreadable JSON and a refused request leave live mode as before, no error.
  const realF = globalThis.fetch, hadLoc = 'location' in globalThis, loc = globalThis.location;
  for (const answer of [() => ({ ok: false, status: 404, json: async () => ({}) }), () => ({ ok: true, json: async () => { throw new SyntaxError('Unexpected token <'); } }), () => { throw new TypeError('Failed to fetch'); }]) {
    const notes = [], seen = [];
    const ctx = { POOL: [], P: {}, D: {}, CAT: [], C: {}, FIRST: [], FILL: [], SPACE_NOTES: {}, pins: {}, a() {}, note: h => notes.push(h), esc: B.html, selectSpace() {} };
    const S = { stage: 'discover', step: 5, req: { business: 'Facial studio', rooms: 3 }, slots: [], saved: [], open: null, finalist: null, chosen: null, placed: [] };
    Object.defineProperty(globalThis, 'location', { value: { href: 'http://localhost:8090/workspace.html?live=1', search: '' }, configurable: true, writable: true });
    globalThis.fetch = function (u, o) { if (String(u).endsWith('/assets/places.json')) { seen.push(u); return Promise.resolve().then(answer); } return strictFetch.call(this, u, o); };
    try { B.bind({ ...ctx, S: () => S, map: () => null }); B.useLocalPlaces(undefined); await B.search(S); }
    finally { globalThis.fetch = realF; if (hadLoc) globalThis.location = loc; else delete globalThis.location; }
    assert.deepEqual(seen, ['http://localhost:8090/assets/places.json']);
    assert.deepEqual(notes.filter(n => /failed|error/i.test(n)), [], notes.join(' | '));
    assert.deepEqual(ctx.POOL, none.POOL);
  }
  B.useLocalPlaces(null);
});

// web-link-4: the interior of a live listing (Matterport tour, then the backend's media route). The scan
// ids are the synthetic records' invented SYNSCAN000N; the manifests below are hand-written in the
// contract's shape (no real answer copied).
const media = [];
let MEDIA = {}; // asset id -> manifest JSON, or a status number, or 'network'
function mediaFetch(url, opts) {
  const u = new URL(url), m = u.pathname.match(/^\/api\/v1\/media\/matterport\/([^/]+)\/(.+)$/);
  if (!m) return strictFetch.call(this, url, opts);
  media.push(`${m[1]} ${m[2]}`);
  const a = MEDIA[m[1]];
  if (a === 'network') return Promise.reject(new TypeError('Failed to fetch'));
  if (typeof a === 'object' && m[2] === 'manifest.json') return Promise.resolve({ ok: true, status: 200, json: async () => a });
  const status = typeof a === 'number' ? a : 404;
  return Promise.resolve({ ok: false, status, json: async () => ({ error_code: status === 400 ? 'PROVIDER_UNAVAILABLE' : 'MEDIA_NOT_FOUND', message: 'stub' }) });
}
const manifestOf = (id, { mesh = true, photos = 3, image = true } = {}) => ({ asset_id: id, provider: 'matterport', data_origin: 'LIVE', name: 'Synthetic scan', mesh: { available: mesh, format: 'obj', resolution: '50k', units: 'm', up_axis: 'z' }, photos: { count: photos }, image: { available: image }, floorplan: { available: true, resolution_px_per_m: 100, origin: { x: -1, y: 2 }, width_px: 800, height_px: 600 }, retrieved_at: '2026-11-15T00:00:00Z' });
// A few DOM calls, enough for viewer() and bind()'s two extra elements.
function fakeDocument() {
  const el = tag => ({ tagName: tag.toUpperCase(), id: '', className: '', hidden: false, textContent: '', dataset: {}, style: {}, attrs: {}, children: [], parent: null, alt: '',
    setAttribute(k, v) { this.attrs[k] = v; }, append(...c) { for (const x of c) { x.parent = this; this.children.push(x); } },
    remove() { if (this.parent) this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null; },
    all() { return this.children.flatMap(c => [c, ...c.all()]); },
    querySelector(s) { return this.all().find(c => (s.startsWith('.') ? c.className.split(' ').includes(s.slice(1)) : c.tagName === s.toUpperCase())) || null; } });
  const root = el('body'), viewer = el('div'), stage = el('div'), photos = el('div'), img = el('img');
  viewer.id = 'viewer'; stage.className = 'vstage'; photos.id = 'v-photos'; photos.className = 'layer';
  root.append(viewer); viewer.append(stage); stage.append(photos); photos.append(img);
  return { createElement: el, getElementById: id => root.all().find(c => c.id === id) || null,
    querySelector: s => (s === '#viewer .vstage' ? stage : s === '#v-photos > img' ? img : null), root };
}
const iframes = doc => doc.root.all().filter(c => c.tagName === 'IFRAME');
await t('interior: suite scan and building scan wording, none without a LIVE Matterport scan', () => {
  const s = B.interiorOf(k1), b = B.interiorOf(k2);
  assert.deepEqual([s.asset_id, s.scope], ['SYNSCAN0001', 'suite']);
  assert.equal(s.tour_text, '3D tour of this suite · Matterport tour (live)');
  assert.equal(s.mesh_text, '3D scan of this suite · Matterport scan (live, fetched by the backend)');
  assert.equal(b.scope, 'building');
  assert.equal(b.tour_text, '3D tour of another space in this building: Lobby and common areas · Matterport tour (live)');
  assert.equal(b.photo_text, 'Interior still of another space in this building: Lobby and common areas · Matterport scan (live, fetched by the backend)');
  for (const x of [b.tour_text, b.mesh_text, b.photo_text]) assert.doesNotMatch(x, /this suite/);
  assert.equal(KP[k1.property_id].live.interior.scope, 'suite'); assert.deepEqual(B.views(KP[k1.property_id]), ['tour']);
  // The suite's own scan wins over a building scan listed first; another unit's measured scan is a building scan.
  const bscan = { ...k2.interior_assets[0], asset_id: 'SYNSCAN0009' };
  assert.equal(B.interiorOf({ ...k1, interior_assets: [bscan, k1.interior_assets[0]] }).asset_id, 'SYNSCAN0001');
  assert.equal(B.interiorOf({ ...k1, interior_assets: [{ ...k1.interior_assets[0], matches_listing_unit: false }] }).scope, 'building');
  // Two building scans: the first in the backend's order, named.
  assert.equal(B.interiorOf({ ...k2, interior_assets: [bscan, k2.interior_assets[0]] }).asset_id, 'SYNSCAN0009');
  assert.match(B.interiorOf({ ...k2, interior_assets: [{ ...bscan, label: null }] }).tour_text, /another space in this building: no label given/);
  for (const r of [k3, hw.record, { ...k1, interior_assets: [{ ...k1.interior_assets[0], data_origin: 'FIXTURE_SYNTHETIC' }] }, { ...k1, interior_assets: [{ ...k1.interior_assets[0], provider: 'vexcel' }] }]) assert.equal(B.interiorOf(r), null);
  assert.deepEqual(B.views(KP[k3.property_id]), []); assert.deepEqual(B.views(B.poolEntry(hw.record)), []);
  assert.equal(KP[k3.property_id].live.scan_text, 'No 3D scan');
  assert.equal(B.viewName('tour'), '3D tour');
});
await t('tour URL: strict id check; a bad id gives no tour at all', () => {
  assert.equal(B.tourUrl('SYNSCAN0001'), 'https://my.matterport.com/show/?m=SYNSCAN0001&play=1&qs=1&brand=0&help=0&dh=1');
  for (const bad of ['SYNSCAN001', 'SYNSCAN00001', 'SYNSCAN000"', 'SYN/CAN0001', 'SYNSCAN0001&x=1', '', null, undefined, 12345678901]) assert.equal(B.tourUrl(bad), null, String(bad));
  assert.equal(B.interiorOf({ ...k1, interior_assets: [{ ...k1.interior_assets[0], asset_id: 'SYN"><x>01' }] }), null);
});
await t('media: no request before a viewer opens; one manifest request per scan; 404 and network errors are silent', async () => {
  const { ctx, S, notes } = await livePage(null), real = globalThis.fetch;
  globalThis.fetch = mediaFetch; media.length = 0; B.forgetManifests();
  MEDIA = { SYNSCAN0001: 404, SYNSCAN0002: 'network' };
  try {
    assert.equal(media.length, 0, 'the search itself asks for no media');
    const p1 = ctx.P[k1.property_id], p2 = ctx.P[k2.property_id], p3 = ctx.P[hw.record.property_id]; // the fixture listing: no LIVE scan
    const photos1 = [...ctx.D[p1.src].photos];
    S.stage = 'compare'; S.finalist = p1.id; S.view = 'dollhouse';
    for (let i = 0; i < 5; i++) { B.viewer('dollhouse', p1, ctx.D[p1.src]); B.viewer('photos', p1, ctx.D[p1.src]); await B.dollhouse(p1); }
    for (let i = 0; i < 3; i++) { B.viewer('tour', p2, ctx.D[p2.src]); await B.dollhouse(p2); }
    B.viewer('dollhouse', p3, ctx.D[p3.src]); await B.dollhouse(p3);
    await new Promise(r => setTimeout(r, 0));
    assert.deepEqual(media.sort(), ['SYNSCAN0001 manifest.json', 'SYNSCAN0002 manifest.json']);
    const d1 = await B.dollhouse(p1);
    assert.deepEqual([d1.mesh, d1.poster, d1.blocks], [null, null, false]); assert.match(d1.text, /^No 3D model from the backend for this scan\. The 3D tour view shows the Matterport tour \(live\)\.$/);
    assert.deepEqual(ctx.D[p1.src].photos, photos1, 'a 404 adds no photo');
    assert.equal((await B.dollhouse(p3)).text, 'No 3D scan');
    assert.deepEqual(notes.filter(n => /fail|error|404|unavailable/i.test(n)), [], notes.join(' | '));
    assert.equal(media.length, 2, 'still one request per scan');
  } finally { globalThis.fetch = real; }
});
await t('media: mesh unavailable -> no mesh, stills after the CoStar photo; mesh available -> backend mesh, no blocks', async () => {
  let renders = 0; // renderViewer is passed by workspace.js (its bind call); counted here
  const { ctx, S } = await livePage(null, { renderViewer: () => renders++ }), real = globalThis.fetch;
  globalThis.fetch = mediaFetch; media.length = 0; B.forgetManifests();
  MEDIA = { SYNSCAN0001: manifestOf('SYNSCAN0001', { mesh: false, photos: 2 }), SYNSCAN0002: manifestOf('SYNSCAN0002', { photos: 1 }) };
  try {
    const p1 = ctx.P[k1.property_id], p2 = ctx.P[k2.property_id];
    S.stage = 'compare'; S.finalist = p1.id; S.view = 'photos'; S.photo = 1;
    B.viewer('photos', p1, ctx.D[p1.src]); await new Promise(r => setTimeout(r, 0));
    assert.deepEqual(ctx.D[p1.src].photos, ['listing-photo', 'mp-live-photo-1', 'mp-live-photo-2', 'aerial-vexcel']);
    assert.equal(B.asset(p1.src, 'mp-live-photo-2'), 'http://localhost:8000/api/v1/media/matterport/SYNSCAN0001/photo/2.jpg');
    assert.equal(B.asset(p1.src, p1.photo), k1.commercial_listing.photo_urls[0], 'the card keeps the CoStar photo');
    assert.equal(B.photoCaption(p1, 'mp-live-photo-1'), 'Interior still of this suite · Matterport scan (live, fetched by the backend)');
    assert.equal(B.photoCaption(p1, 'listing-photo'), 'CoStar photo');
    assert.equal(B.photoCaption(p2, 'mp-live-photo-1'), 'Interior still of another space in this building: Lobby and common areas · Matterport scan (live, fetched by the backend)');
    assert.equal(renders, 1, 'the Photos view is painted again once the stills are known');
    const d1 = await B.dollhouse(p1); assert.equal(d1.mesh, null);
    const d2 = await B.dollhouse(p2);
    assert.deepEqual(d2, { key: 'matterport:SYNSCAN0002', mesh: 'http://localhost:8000/api/v1/media/matterport/SYNSCAN0002/mesh.obj', poster: null, caption: '3D scan of another space in this building: Lobby and common areas · Matterport scan (live, fetched by the backend)', blocks: false });
    // The suite's own scan, mesh available: the scan's still as poster; still no furniture blocks.
    B.forgetManifests(); MEDIA.SYNSCAN0001 = manifestOf('SYNSCAN0001');
    const d1b = await B.dollhouse(p1);
    assert.deepEqual([d1b.mesh, d1b.poster, d1b.blocks, d1b.caption], ['http://localhost:8000/api/v1/media/matterport/SYNSCAN0001/mesh.obj', 'http://localhost:8000/api/v1/media/matterport/SYNSCAN0001/image.jpg', false, '3D scan of this suite · Matterport scan (live, fetched by the backend)']);
    // A manifest for another id or another origin is not used.
    B.forgetManifests(); MEDIA.SYNSCAN0001 = { ...manifestOf('SYNSCAN0001'), asset_id: 'SYNSCAN0009' }; assert.equal(await B.manifest('SYNSCAN0001'), null);
    B.forgetManifests(); MEDIA.SYNSCAN0001 = { ...manifestOf('SYNSCAN0001'), data_origin: 'FIXTURE_SYNTHETIC' }; assert.equal(await B.manifest('SYNSCAN0001'), null);
    assert.equal(await B.manifest('../../x'), null);
  } finally { globalThis.fetch = real; }
});
await t('tour iframe: made only when the tour view opens, one at a time, removed on another view or listing', async () => {
  const realDoc = globalThis.document, doc = fakeDocument(); globalThis.document = doc;
  try {
    const { ctx, S } = await livePage(null); // bind() adds the tour layer and the photo caption
    const layer = doc.getElementById('v-tour');
    assert.ok(layer && layer.className === 'layer' && layer.hidden === true, 'a hidden layer the page shows like its own');
    assert.equal(iframes(doc).length, 0, 'no iframe before the tour view opens');
    const p1 = ctx.P[k1.property_id], p2 = ctx.P[k2.property_id], p3 = ctx.P[hw.record.property_id];
    B.viewer('dollhouse', p1, ctx.D[p1.src]); assert.equal(iframes(doc).length, 0);
    B.viewer('tour', p1, ctx.D[p1.src]);
    const [f] = iframes(doc);
    assert.equal(f.src, B.tourUrl('SYNSCAN0001')); assert.equal(f.attrs.allow, 'fullscreen; xr-spatial-tracking'); assert.equal(f.attrs.referrerpolicy, 'no-referrer');
    assert.equal(layer.querySelector('.live-caption').textContent, '3D tour of this suite · Matterport tour (live)');
    B.viewer('tour', p1, ctx.D[p1.src]); assert.equal(iframes(doc)[0], f, 'the same view again keeps the tour');
    B.viewer('tour', p2, ctx.D[p2.src]);
    assert.equal(iframes(doc).length, 1); assert.equal(iframes(doc)[0].src, B.tourUrl('SYNSCAN0002'));
    assert.equal(layer.querySelector('.live-caption').textContent, '3D tour of another space in this building: Lobby and common areas · Matterport tour (live)');
    B.viewer('photos', p2, ctx.D[p2.src]); assert.equal(iframes(doc).length, 0, 'another view removes it');
    assert.equal(layer.querySelector('.live-caption').textContent, '');
    B.viewer('tour', p3, ctx.D[p3.src]); assert.equal(iframes(doc).length, 0, 'no scan, no tour');
    S.photo = 0; B.viewer('photos', p1, ctx.D[p1.src]);
    const cap = doc.getElementById('v-photos').querySelector('.live-caption');
    assert.equal(cap.textContent, 'CoStar photo'); assert.equal(cap.hidden, false);
    B.viewer('dollhouse', p1, ctx.D[p1.src]); assert.equal(cap.hidden, true);
  } finally { if (realDoc === undefined) delete globalThis.document; else globalThis.document = realDoc; }
});
await t('a matched local suite (web-link-3) keeps its saved copies; its tour is offered; no media request', async () => {
  const { ctx } = await livePage(SYN_PLACES()), real = globalThis.fetch;
  globalThis.fetch = mediaFetch; media.length = 0; B.forgetManifests(); MEDIA = {};
  try {
    const p = ctx.P['syn-suite-a'], before = [...ctx.D[p.src].photos];
    B.viewer('photos', p, ctx.D[p.src]); B.viewer('dollhouse', p, ctx.D[p.src]);
    assert.deepEqual(await B.dollhouse(p), { key: 'syn-suite-a', mesh: 'assets/meshes/syn-suite-a.obj', poster: 'assets/dollhouses/syn-suite-a.png', caption: '3D scan of this suite · Matterport (saved copy)', blocks: true });
    await new Promise(r => setTimeout(r, 0));
    assert.deepEqual(media, []); assert.deepEqual(ctx.D[p.src].photos, before);
    assert.equal(B.photoCaption(p, 'matterport-01.jpg'), 'Matterport still (saved copy)');
    assert.deepEqual(B.views(p), ['tour']); assert.equal(p.live.interior.scope, 'suite');
    assert.equal(B.asset(p.src, 'mp-live-photo-1').startsWith('data:'), true, 'no backend still for a saved-copy suite');
  } finally { globalThis.fetch = real; }
});
await t('without ?live=1 none of this runs: every new workspace.js line is behind LIVE', () => {
  assert.equal(B.config('').live, false);
  const src = readFileSync(new URL('./workspace.js', import.meta.url), 'utf8');
  assert.match(src, /^const LIVE = new URLSearchParams\(location\.search\)\.get\('live'\) === '1' \? await import\('\.\/backend\.js'\) : null;/m);
  const uses = src.split('\n').flatMap((l, i) => [...l.matchAll(/LIVE(\?\.|\.| &&| \?)/g)].map(m => [i + 1, m[1], l.trim()]));
  for (const [n, how, l] of uses) assert.ok(how !== '.' || /if \(LIVE\)|LIVE && |LIVE \? /.test(l), `line ${n} uses LIVE unguarded: ${l}`);
  // The paint3D local: null without LIVE, so key = src, the same paths, the blocks and an empty state as before.
  assert.match(src, /const L = LIVE && \(box\.dataset\.state = 'loading', state\.textContent = 'Loading model', poster\.removeAttribute\('src'\), await LIVE\.dollhouse\(p\)\), key = L\?\.key \?\? src;/);
  for (const s of ['L?.mesh ?? `assets/meshes/${src}.obj`', "L?.blocks !== false", "L?.caption || ''", 'if (L && !L.mesh)', 'if (L && !L.poster)']) assert.ok(src.includes(s), s);
});

await t('dollhouse: while a live listing is asked for its model, the previous listing\'s model, poster and caption are not shown', async () => {
  const src = readFileSync(new URL('./workspace.js', import.meta.url), 'utf8'), body = src.slice(src.indexOf('async function paint3D'), src.indexOf('\nlet vmap'));
  const run = (LIVE) => {
    const poster = { src: 'http://x/assets/dollhouses/old.png', removeAttribute(k) { if (k === 'src') this.src = ''; } }, state = { textContent: '3D scan of this suite · Matterport (saved copy)' };
    const box = { dataset: { state: 'ready' } }, $ = (s) => (s === '#v-dollhouse' ? box : s === '.poster' ? poster : state);
    const paint3D = new Function('$', 'LIVE', `let want3D, T = null; const init3D = () => new Promise(() => {}), S = { stage: 'compare' }; ${body}; return paint3D;`)($, LIVE);
    paint3D({ src: 'next' }, {}); return { state: box.dataset.state, text: state.textContent, poster: poster.src };
  };
  let answer; const pending = { dollhouse: () => new Promise(r => { answer = r; }) };
  assert.deepEqual(run(pending), { state: 'loading', text: 'Loading model', poster: '' }, 'live, before the answer');
  answer(null);
  // Without ?live=1 the first synchronous step is the old one: the poster path, then "Loading model".
  assert.deepEqual(run(null), { state: 'loading', text: 'Loading model', poster: 'assets/dollhouses/next.png' });
});

/* ------------------------------------- (a) the waiting sequence (web-link-5) */
// A fake DOM with the page's chat log and composer, and a clock the test moves by hand.
function waitDoc() {
  const el = tag => ({ tagName: tag.toUpperCase(), id: '', className: '', hidden: false, textContent: '', placeholder: '', scrollTop: 0, scrollHeight: 0, attrs: {}, children: [], parent: null,
    setAttribute(k, v) { this.attrs[k] = String(v); }, getAttribute(k) { return k in this.attrs ? this.attrs[k] : null; }, removeAttribute(k) { delete this.attrs[k]; },
    append(...c) { for (const x of c) { x.remove?.(); x.parent = this; this.children.push(x); } this.scrollHeight = this.children.length * 40; },
    remove() { if (this.parent) this.parent.children.splice(this.parent.children.indexOf(this), 1); this.parent = null; },
    all() { return this.children.flatMap(c => [c, ...c.all()]); }, querySelector() { return null; } });
  const root = el('html'), head = el('head'), body = el('body'), log = el('div'), form = el('form'), say = el('input');
  log.id = 'log'; form.id = 'composer'; say.id = 'say'; say.placeholder = 'Describe what you need';
  root.append(head, body); body.append(log, form); form.append(say);
  return { createElement: el, head, documentElement: root, getElementById: id => root.all().find(c => c.id === id) || null, querySelector: () => null, root, log, form, say };
}
let now = 0; const ticks = new Set();
B.useClock({ now: () => now, every: fn => { ticks.add(fn); return fn; }, stop: fn => ticks.delete(fn) });
const advance = ms => { now += ms; for (const f of [...ticks]) f(); };
const boxes = doc => doc.root.all().filter(c => c.className === 'live-wait');
const textOf = n => [n.textContent, ...n.children.map(textOf)].join(' ');
const shownText = b => [b, ...b.all()].filter(n => !n.hidden).map(n => n.textContent).join(' ');
const tick = () => new Promise(r => setImmediate(r));
// strictFetch, except that a route matching `hold` waits for release(), then answers or fails.
function heldFetch(hold) {
  let release; const gate = new Promise(r => { release = r; });
  const f = function (u, o) { const path = new URL(u).pathname; return hold.test(path) ? gate.then(mode => (mode === 'reject' ? Promise.reject(new TypeError('Failed to fetch')) : mode === 503 ? { ok: false, status: 503, text: async () => JSON.stringify({ error_code: 'PYTHON_SERVICE_NOT_CONFIGURED' }) } : strictFetch(u, o))) : strictFetch(u, o); };
  return { f, release: m => release(m) };
}
// bind() makes its client with the global fetch of that moment; the tests switch `current` instead.
let current = strictFetch; const viaCurrent = (u, o) => current(u, o);
async function waitPage(ctxExtra = {}) {
  const doc = waitDoc(); globalThis.document = doc; globalThis.fetch = viaCurrent; current = strictFetch;
  const notes = [], said = [];
  const ctx = { POOL: [], P: {}, D: {}, CAT: [], C: {}, FIRST: [], FILL: [], SPACE_NOTES: {}, pins: {}, a: h => said.push(h), note: h => notes.push(h), esc: B.html, selectSpace() {}, ...ctxExtra };
  const S = { stage: 'discover', step: 5, req: { business: 'Facial studio', rent: null, rooms: 3 }, slots: [], saved: [], open: null, finalist: null, chosen: null, placed: [] };
  B.bind({ ...ctx, S: () => S, map: () => null }); B.useLocalPlaces([]);
  return { doc, ctx, S, notes, said };
}
async function withDoc(fn) {
  const realDoc = globalThis.document, realFetch = globalThis.fetch;
  try { return await fn(); } finally { globalThis.fetch = realFetch; if (realDoc === undefined) delete globalThis.document; else globalThis.document = realDoc; }
}
await t('waiting: each of the three kinds shows the page\'s dots, a verb and what it is doing, and is gone after success', () => withDoc(async () => {
  B.forgetWaits(); now = 0;
  const { doc, S, said } = await waitPage();
  assert.equal(doc.head.children.filter(c => c.tagName === 'STYLE' && c.id === 'live-wait-style').length, 1, 'one <style>, added by bind() in live mode');
  for (const [kind, hold, run] of [
    ['search', /\/properties\/search$/, () => B.search(S)],
    ['layout', /\/layouts\/generate$/, () => { S.finalist = k1.property_id; return B.furnish(S); }],
    ['chat', /\/chat$/, () => { S.stage = 'compare'; return B.turn('compare', 'what about parking', S, 5); }],
  ]) {
    const h = heldFetch(hold); current = h.f;
    const p = run(); await tick(); await tick();
    const [b] = boxes(doc); assert.equal(boxes(doc).length, 1, kind);
    assert.equal(b.parent, doc.log, 'at the end of the conversation log'); assert.equal(doc.log.children.at(-1), b);
    assert.equal(b.attrs.role, 'status'); assert.equal(b.attrs['aria-live'], 'polite');
    const dots = b.children.find(c => c.className === 'typing'); assert.equal(dots.children.length, 3); assert.equal(dots.attrs['aria-hidden'], 'true');
    assert.ok(shownText(b).includes(B.WAIT_TEXT[kind].verb) && shownText(b).includes(B.WAIT_TEXT[kind].what), shownText(b));
    const secs = b.all().find(c => c.className === 'live-wait-secs'); assert.equal(secs.attrs['aria-hidden'], 'true');
    advance(500); assert.equal(secs.textContent, '', 'nothing in the first second'); advance(2700); assert.equal(secs.textContent, ' 3 s');
    h.release('ok'); await p;
    assert.equal(boxes(doc).length, 0, `${kind}: removed after success`); assert.equal(ticks.size, 0, 'its timer is stopped');
  }
  assert.ok(said.at(-1).includes('from the backend'), 'the reply still arrives');
}));
await t('waiting: gone after a rejected call and after a thrown error inside the handler; the failure notes still appear', () => withDoc(async () => {
  B.forgetWaits();
  const { doc, S, notes } = await waitPage();
  await B.search(S); // creates the project
  for (const [hold, mode, run, note] of [
    [/\/chat$/, 503, () => B.turn('compare', 'parking?', S, 5), /no assistant service configured/],
    [/\/chat$/, 'reject', () => B.turn('compare', 'parking?', S, 5), /The assistant is not reachable/],
    [/\/properties\/search$/, 'reject', () => B.search({ ...S, req: { ...S.req, rent: 5000 } }), /^Live search failed: /],
    [/\/layouts\/generate$/, 'reject', () => { S.finalist = k1.property_id; B.useLocalPlaces([]); return B.furnish(S); }, /^No layout for /],
  ]) {
    const h = heldFetch(hold); current = h.f; notes.length = 0;
    const p = run(); await tick(); await tick(); assert.equal(boxes(doc).length, 1);
    h.release(mode); await p;
    assert.equal(boxes(doc).length, 0); assert.equal(ticks.size, 0); assert.match(notes.at(-1) || '', note, notes.join(' | '));
  }
  // A throw inside the handler itself (here the page's note and reply functions).
  const thrower = await waitPage({ a() { throw new Error('page broke'); }, note() { throw new Error('page broke'); } });
  current = strictFetch; await B.search(thrower.S).catch(() => {});
  assert.equal(boxes(thrower.doc).length, 0, 'search: removed although the page threw');
  await assert.rejects(B.turn('compare', 'parking?', thrower.S, 5), /page broke/);
  assert.equal(boxes(thrower.doc).length, 0, 'chat: removed although the page threw'); assert.equal(thrower.doc.form.getAttribute('aria-busy'), null);
}));
/* ------------------------------- (a) a layout that does not fit (web-link-7) */
const NOFIT = 'No layout fits the measured floor: the floor is 99 m2 and its largest rectangular area is 9.0 x 6.0 m = 54 m2; the room program needs 5 room(s), at least 46.8 m2 in all plus circulation, with minimum room widths up to 3.0 m. Solver: No spine layout satisfies the room program; No front layout satisfies the room program. No layout was stored.';
const SUITE = { address: '1 Test St', live: { floor_is_suite: true } }, OTHER = { address: '1 Test St', live: { floor_is_suite: false } };
const cardText = b => textOf(b).replace(/\s+/g, ' ');
function failingFetch(task) { return (u, o) => (new URL(u).pathname.endsWith('/layouts/generate') ? Promise.resolve({ ok: true, status: 202, text: async () => JSON.stringify({ task }) }) : strictFetch(u, o)); }
const results = doc => doc.log.children.filter(c => c.className === 'live-result');
await t('no fit: the infeasible answer gives the card with exactly the message\'s numbers, the reasons, "No layout was stored." and the options', () => withDoc(async () => {
  const { doc, ctx, S, notes } = await waitPage();
  await B.search(S); notes.length = 0; S.finalist = k1.property_id;
  current = failingFetch({ status: 'FAILED', error_code: 'LayoutInfeasibleError', error_message: NOFIT, output_reference: {} });
  assert.equal(await B.furnish(S), false, 'the page stays in Compare');
  assert.equal(boxes(doc).length, 0, 'the wait ended'); assert.deepEqual(notes, [], 'no note in place of the card');
  const [c] = results(doc); assert.ok(c); assert.equal(c.attrs.role, 'status'); assert.equal(doc.log.children.at(-1), c);
  const txt = cardText(c);
  for (const want of ['Measured floor area: 99 m2', 'Largest rectangular area: 9.0 x 6.0 m = 54 m2', 'Rooms needed: 5 room(s), at least 46.8 m2 in all plus circulation', 'Minimum room width: up to 3.0 m', 'Solver:', 'No spine layout satisfies the room program', 'No front layout satisfies the room program', 'No layout was stored.', B.NO_FIT_OPTIONS]) assert.ok(txt.includes(want), want + ' | ' + txt);
  for (const d of txt.match(/\d+(?:\.\d+)?/g)) assert.ok(NOFIT.includes(d), `${d} is not in the message`);
  assert.doesNotMatch(txt, /sq ?ft|SF\b|error|went wrong|unsuitable/i);
  assert.equal(ctx.D[ctx.P[k1.property_id].src].rooms.length, 0, 'no layout put on the page'); assert.equal(S.placed.length, 0);
  // trying again (new rooms) asks the backend again and works as before
  current = strictFetch; assert.equal(await B.furnish(S), true);
}));
await t('no fit: whose floor; the unknown-rectangle wording; an unparsed message shown whole', () => {
  const r = B.layoutResult({ error_code: 'LayoutInfeasibleError', error_message: NOFIT }, SUITE);
  assert.equal(r.heading, 'The rooms do not fit this suite’s measured floor'); assert.deepEqual(r.lines[0], ['Floor', 'the measured floor of this suite (Matterport scan)']);
  const o = B.layoutResult({ error_code: 'LayoutInfeasibleError', error_message: NOFIT }, OTHER);
  assert.doesNotMatch(o.heading + o.lines[0][1], /this suite’s measured|of this suite \(Matterport/); assert.match(o.lines[0][1], /may not be this suite’s own/);
  const u = B.layoutResult({ error_code: 'LayoutInfeasibleError', error_message: NOFIT.replace('its largest rectangular area is 9.0 x 6.0 m = 54 m2', 'the size of its largest rectangular area is unknown') }, SUITE);
  assert.equal(u.kind, 'infeasible'); assert.deepEqual(u.lines[2], ['Largest rectangular area', 'size unknown (the backend could not compute it)']);
  const bad = 'No layout fits: something new <b>x</b>';
  const w = B.layoutResult({ error_code: 'LayoutInfeasibleError', error_message: bad }, SUITE);
  assert.equal(w.kind, 'unparsed'); assert.deepEqual(w.lines[1], ['The backend said', bad]);
  const e0 = B.layoutResult({ error_code: 'LayoutInfeasibleError', error_message: '' }, SUITE);
  assert.equal(e0.kind, 'unparsed'); assert.deepEqual(e0.lines[1], ['The backend said', '(no message: the backend gave no details)']); assert.doesNotMatch(e0.lines.flat().join(' '), /\d/);
  // floorIsSuite on the recorded fixtures agrees with interiorOf: never true without a suite-own scan
  for (const rec of [k1, k2, hw.record]) { const it = B.interiorOf(rec); if (B.floorIsSuite(rec)) assert.equal(it.scope, 'suite'); }
  assert.equal(B.floorIsSuite({ commercial_listing: { data_origin: 'FIXTURE_REAL' }, interior_assets: [{ asset_id: 'BLDG0000001', provider: 'matterport', data_origin: 'LIVE', has_measured_geometry: true, matches_listing_unit: false }, { asset_id: 'SUITE000001', provider: 'matterport', data_origin: 'LIVE', has_measured_geometry: true }] }), false, 'a building scan first: not the suite');
});
await t('no fit: markup in the message is text; another error_code keeps the old note; a completed task with no layout gives the no-reason result', () => withDoc(async () => {
  const { doc, S, notes } = await waitPage();
  await B.search(S); S.finalist = k1.property_id;
  current = failingFetch({ status: 'FAILED', error_code: 'LayoutInfeasibleError', error_message: '<img src=x onerror=alert(1)>' });
  await B.furnish(S);
  const c = results(doc).at(-1); assert.ok(c.all().some(n => n.textContent === '<img src=x onerror=alert(1)>')); assert.ok(!c.all().some(n => n.tagName === 'IMG' || 'innerHTML' in n));
  notes.length = 0; const before = results(doc).length;
  current = failingFetch({ status: 'FAILED', error_code: 'GEOMETRY_UNAVAILABLE', error_message: 'no measured geometry' });
  assert.equal(await B.furnish(S), false); assert.equal(results(doc).length, before);
  assert.match(notes.at(-1), /^No layout for .*Layout generation is FAILED: no measured geometry\.$/);
  current = failingFetch({ status: 'COMPLETED', output_reference: { design_version_ids: [] } });
  assert.equal(await B.furnish(S), false);
  const n = cardText(results(doc).at(-1)); assert.match(n, /The backend returned no layout for this (suite|listing) and gave no reason\./); assert.doesNotMatch(n, /\d/);
}));
await t('no fit: nothing of it exists without ?live=1 (no page file names it)', () => {
  for (const f of ['workspace.js', 'workspace.html', 'workspace.css']) assert.doesNotMatch(readFileSync(new URL('./' + f, import.meta.url), 'utf8'), /live-result|layoutResult|resultCard/, f);
});
await t('waiting: two overlapping waits show one element, and none is left behind in either end order', () => withDoc(async () => {
  const { doc, S } = await waitPage();
  for (const order of [[0, 1], [1, 0]]) {
    const ends = [B.waiting('chat'), B.waiting('search')];
    assert.equal(boxes(doc).length, 1); assert.ok(textOf(boxes(doc)[0]).includes('Searching…'), 'the newest is shown');
    ends[order[0]](); assert.equal(boxes(doc).length, 1, 'the other one is still pending and shown');
    ends[order[1]](); assert.equal(boxes(doc).length, 0); ends[0](); ends[1](); assert.equal(boxes(doc).length, 0, 'end() twice is harmless');
  }
  // Real calls: a chat turn and an inline-edit search at once.
  current = strictFetch; await B.search(S);
  const h = heldFetch(/\/chat$|\/properties\/search$/); current = h.f;
  const p1 = B.turn('compare', 'parking?', S, 5), p2 = B.search({ ...S, req: { ...S.req, rent: 4000 } }); await tick(); await tick();
  assert.equal(boxes(doc).length, 1); h.release('ok'); await Promise.all([p1, p2]);
  assert.equal(boxes(doc).length, 0); assert.equal(ticks.size, 0);
}));
await t('waiting: no estimate on the first wait; later ones show the median of earlier successful waits; failures not counted', () => withDoc(async () => {
  B.forgetWaits(); now = 0; const { doc } = await waitPage();
  const est = () => boxes(doc)[0].all().find(c => c.className === 'live-wait-est')?.textContent ?? null;
  let end = B.waiting('search'); assert.equal(est(), null); advance(4000); end(true);
  end = B.waiting('search'); assert.equal(est(), 'Earlier searches in this session took about 4 s.'); advance(10000); end(true);
  end = B.waiting('search'); assert.equal(est(), 'Earlier searches in this session took about 7 s.', 'median of 4 and 10'); advance(6000); end(true);
  end = B.waiting('search'); assert.equal(est(), 'Earlier searches in this session took about 6 s.', 'median of 4, 6, 10'); advance(300); end(); // a failure
  end = B.waiting('search'); assert.equal(est(), 'Earlier searches in this session took about 6 s.', 'the failed wait is not counted'); end();
  end = B.waiting('chat'); assert.equal(est(), null, 'each kind has its own history'); end();
  assert.equal(B.estimateOf('search'), 6000);
}));
await t('waiting: "longer than usual" only past the threshold (20 s with no estimate; twice the estimate and 5 s more with one)', () => withDoc(async () => {
  B.forgetWaits(); now = 0; const { doc } = await waitPage();
  const late = () => boxes(doc)[0].all().find(c => c.className === 'live-wait-late');
  let end = B.waiting('chat'); assert.equal(late().hidden, true); advance(19900); assert.equal(late().hidden, true); advance(200); assert.equal(late().hidden, false);
  assert.equal(late().textContent, B.LATE_TEXT); end(true); // 20.1 s recorded
  end = B.waiting('chat'); advance(40000); assert.equal(late().hidden, true, 'under twice the 20.1 s estimate'); advance(300); assert.equal(late().hidden, false); end();
  B.forgetWaits(); end = B.waiting('layout'); advance(2000); end(true); // estimate 2 s: late after 7 s
  end = B.waiting('layout'); advance(6900); assert.equal(late().hidden, true); advance(200); assert.equal(late().hidden, false); end();
  assert.equal(B.isLate(12001, 6000), true); assert.equal(B.isLate(11000, 6000), false);
}));
await t('waiting: no text claims progress (no percentage, "almost" or "done"), and none comes from the backend or the demo script', () => withDoc(async () => {
  B.forgetWaits(); now = 0; const { doc } = await waitPage();
  const all = [B.LATE_TEXT, ...Object.values(B.WAIT_TEXT).flatMap(Object.values)];
  for (const kind of Object.keys(B.WAIT_TEXT)) { const e = B.waiting(kind); advance(3000); e(true); const e2 = B.waiting(kind); advance(60000); all.push(textOf(boxes(doc)[0])); e2(); }
  for (const s of all) { assert.doesNotMatch(s, /%|\balmost\b|\bdone\b|\bcomplete|\bnearly\b|\bstep \d/i, s); }
  const src = readFileSync(new URL('./workspace.js', import.meta.url), 'utf8');
  for (const s of Object.values(B.WAIT_TEXT).flatMap(t => [t.what, t.verb])) assert.ok(!src.includes(s), `scripted: ${s}`);
  const mine = readFileSync(new URL('./backend.js', import.meta.url), 'utf8'), block = mine.slice(mine.indexOf('/* ------------------------------------------------- waiting sequence'), mine.indexOf('let page = null'));
  assert.doesNotMatch(block, /innerHTML/, 'the indicator is built with DOM calls and textContent only');
}));
await t('composer: placeholder and aria-busy while a sent message waits, restored exactly afterwards; an inline edit search does not mark it', () => withDoc(async () => {
  const { doc, S } = await waitPage(); current = strictFetch;
  doc.say.placeholder = 'Ask about these spaces'; await B.search(S);
  assert.equal(doc.say.placeholder, 'Ask about these spaces', 'an inline-edit search (no turn) leaves the composer alone');
  let h = heldFetch(/\/chat$/); current = h.f;
  let p = B.turn('compare', 'parking?', S, 5); await tick(); await tick();
  assert.equal(doc.form.getAttribute('aria-busy'), 'true'); assert.equal(doc.say.placeholder, 'Waiting for the assistant…');
  h.release(503); await p;
  assert.equal(doc.form.getAttribute('aria-busy'), null, 'attribute removed, as before'); assert.equal(doc.say.placeholder, 'Ask about these spaces');
  // A search answer sent through the composer: turn() hands it to the page, which calls search() at once.
  h = heldFetch(/\/properties\/search$/); current = h.f; doc.form.setAttribute('aria-busy', 'false');
  assert.equal(B.turn('discover', 'Up to $4,000', { step: 5 }, 5), null); p = B.search({ ...S, req: { ...S.req, rent: 4000 } }); await tick(); await tick();
  assert.equal(doc.say.placeholder, 'Waiting for the search…'); assert.equal(doc.form.getAttribute('aria-busy'), 'true');
  h.release('ok'); await p; assert.equal(doc.form.getAttribute('aria-busy'), 'false', 'a value that was there is put back'); assert.equal(doc.say.placeholder, 'Ask about these spaces');
  // The page changing its hint during the wait (a stage change) is kept.
  const end = B.waiting('chat', { composer: true }); doc.say.placeholder = 'Furnish hint'; end(); assert.equal(doc.say.placeholder, 'Furnish hint');
  // After the microtask the flag is gone: a later search is not marked.
  B.turn('discover', 'Up to $4,000', { step: 5 }, 5); await tick(); h = heldFetch(/\/properties\/search$/); current = h.f;
  p = B.search({ ...S, req: { ...S.req, rent: 3000 } }); await tick(); await tick(); assert.equal(doc.say.placeholder, 'Furnish hint'); h.release('ok'); await p;
}));
await t('waiting: the layout line does not claim the suite\'s own geometry or estimated prices before the answer says where they come from', () => {
  assert.doesNotMatch(B.WAIT_TEXT.layout.what, /suite’s floor geometry|estimated prices/, 'the geometry may be a fixture or a photo estimate, the prices fixture prices');
  assert.match(B.WAIT_TEXT.layout.what, /result says where/);
});
await t('without ?live=1 nothing of the waiting sequence exists: no page file names it, and only bind() and the live calls create it', () => {
  for (const f of ['workspace.js', 'workspace.css', 'workspace.html']) assert.doesNotMatch(readFileSync(new URL('./' + f, import.meta.url), 'utf8'), /live-wait|waiting\(|Waiting for the/, f);
  const mine = readFileSync(new URL('./backend.js', import.meta.url), 'utf8');
  assert.deepEqual([...mine.matchAll(/\b(waitStyle|waiting)\(/g)].length, 7, 'definitions + bind + search + layout + chat + voice');
});

/* ------------------------------------- (a) voice: talk to the assistant (web-link-6) */
// Fakes: WebSocket, navigator.mediaDevices, AudioContext and AudioWorkletNode, and one-shot timers on the
// test clock. Nothing here reaches xAI. The answer below is made up; the claim pattern is codex_chat's.
const CLAIM = "\\bI(?:'ve| have| just)?\\s+(?:called|phoned|rang|dialed|e-?mailed|texted|messaged|contacted|reached out|got in touch|let|told|informed|sent|submitted|put in|placed|booked|reserved|scheduled|bought|purchased|paid|wired|ordered|approved|signed|accepted|locked|confirmed|made an offer)\\b";
const SECRET = 'TESTSECRET-0000-not-a-real-key';
const vresp = (over = {}) => ({ mode: 'assistant', provider: 'xai', websocket_url: 'wss://voice.example.invalid/v1/realtime?model=test', client_secret: SECRET, expires_at: new Date(Date.now() + 120000).toISOString(),
  session_update: { type: 'session.update', session: { instructions: 'test facts', voice: 'eve', turn_detection: { type: 'server_vad' }, tools: [] } },
  disclosure: 'This is the test voice assistant. It is automated, not a person.', disclosure_event: { type: 'conversation.item.create', item: { type: 'force_message', role: 'assistant', interruptible: false, content: [{ type: 'output_text', text: 'disclosure' }] } },
  label: 'Voice assistant: automated (test label).', no_action_notice: '(Note: this assistant cannot act. Nothing was sent.)', claim_pattern: CLAIM, data_notice: 'Test data notice: fixture data.', grounded_record_ids: [], ...over });
const pcm = n => Buffer.from(new Int16Array(Array.from({ length: n }, (_, i) => (i % 2 ? 1000 : -1000))).buffer).toString('base64');
const timers = new Map(); let tid = 0;
const fire = ms => { now += ms; for (const [id, x] of [...timers]) if (x.at <= now && timers.has(id)) { timers.delete(id); x.fn(); } };
// The address the page was opened with, for config() (talk=1 or not); restored after the voice tests.
const hadLocation = 'location' in globalThis, realLocation = globalThis.location;
function setSearch(search) { Object.defineProperty(globalThis, 'location', { value: { href: 'http://localhost:8090/workspace.html' + search, search }, configurable: true, writable: true }); }
async function voiceRig({ mic = 'ok', micGate = null, answer = () => ({ status: 200, body: vresp() }) } = {}) {
  const order = [], sockets = [], tracks = [], ctxs = [], nodes = [], starts = [], stops = [], posts = [];
  class WS { constructor(url, protos) { order.push('socket'); Object.assign(this, { url, protos, sent: [], closed: false }); sockets.push(this); } send(m) { this.sent.push(JSON.parse(m)); } close() { this.closed = true; } emit(o) { this.onmessage?.({ data: JSON.stringify(o) }); } }
  const mediaDevices = { getUserMedia: async c => { order.push('mic'); mediaDevices.asked = c; if (micGate) await micGate; if (mic !== 'ok') throw Object.assign(new Error('no'), { name: mic }); const t = { readyState: 'live', stop() { this.readyState = 'ended'; } }; tracks.push(t); return { getTracks: () => [t] }; } };
  class AC { constructor(o) { Object.assign(this, { opts: o, sampleRate: 48000, currentTime: 0, destination: {}, closed: false, audioWorklet: { addModule: async () => {} } }); ctxs.push(this); }
    resume() { return Promise.resolve(); } close() { this.closed = true; return Promise.resolve(); }
    createBuffer(c, n, r) { const d = new Float32Array(n); return { duration: n / r, rate: r, getChannelData: () => d }; }
    createBufferSource() { return { connect() {}, disconnect() {}, start(t) { starts.push(t); }, stop() { stops.push(1); } }; }
    createMediaStreamSource() { return { connect() {}, disconnect() {} }; } }
  class AWN { constructor(c, name) { this.name = name; this.port = {}; nodes.push(this); } connect() {} disconnect() { this.off = true; } }
  B.useVoiceEnv({ WebSocket: WS, mediaDevices, AudioContext: AC, AudioWorkletNode: AWN });
  B.useClock({ after: (fn, ms) => { const id = ++tid; timers.set(id, { fn, at: now + ms }); return id; }, cancel: id => timers.delete(id) });
  const pg = await waitPage(); const { doc } = pg;
  const chat = doc.createElement('div'); chat.id = 'chat'; doc.root.children[1].append(chat); // body
  const listeners = {}; Object.assign(doc, { visibilityState: 'visible', addEventListener: (k, f) => { listeners[k] = f; }, removeEventListener: (k, f) => { if (listeners[k] === f) delete listeners[k]; } });
  setSearch('?live=1&talk=1'); // the Talk control exists only with ?talk=1 (web-link-8)
  B.bind({ ...pg.ctx, S: () => pg.S, map: () => null }); // again, now that the page has #chat
  current = (u, o) => { const path = new URL(u).pathname; if (path.endsWith('/voice/session')) { order.push('post'); posts.push(JSON.parse(o.body)); const a = answer(); return Promise.resolve({ ok: a.status === 200, status: a.status, text: async () => JSON.stringify(a.body) }); } return strictFetch(u, o); };
  const el = cls => doc.root.all().find(c => c.className === cls);
  const ui = { box: () => doc.getElementById('live-voice'), btn: () => el('live-voice-btn'), note: () => el('live-voice-note'), banner: () => el('live-voice-banner'), consent: () => el('live-voice-consent'), turns: () => el('live-card-log').children.filter(c => /live-voice-turn/.test(c.className)) };
  const click = async () => { const p = ui.btn().onclick(); await tick(); await tick(); await tick(); return p; };
  return { ...pg, mediaDevices, order, sockets, tracks, ctxs, nodes, starts, stops, posts, listeners, ui, click, ws: () => sockets.at(-1) };
}
const settled = r => { // every way of stopping leaves this
  assert.equal(B.voiceActive(), false, 'no session'); assert.ok(r.sockets.every(s => s.closed), 'socket closed'); assert.ok(r.tracks.every(t => t.readyState === 'ended'), 'mic tracks stopped');
  assert.ok(r.ctxs.every(c => c.closed), 'audio context closed'); assert.equal(timers.size, 0, 'no timer'); assert.equal(ticks.size, 0, 'no waiting timer');
  assert.equal(r.ui.btn().textContent, B.VOICE.talk); assert.equal(r.ui.btn().attrs['aria-pressed'], 'false'); assert.equal(r.ui.banner().hidden, true);
};
const notesOf = r => [r.ui.note()].filter(n => !n.hidden).map(n => n.textContent);
const opened = async r => { await r.click(); r.ws().onopen(); };
await t('voice: before a project the button is disabled with a reason; the consent sentence shows before the first session', () => withDoc(async () => {
  const r = await voiceRig(); B.forgetWaits();
  // A fresh Backend has no project until the search: bind() made a new one.
  assert.equal(r.ui.btn().disabled, true); assert.equal(textOf(r.ui.box()).includes(B.VOICE.noProject), true);
  assert.equal(r.ui.consent().textContent, B.VOICE.consent); assert.equal(r.ui.consent().hidden, false);
  assert.match(B.VOICE.consent, /project’s records to xAI/); assert.match(B.VOICE.consent, /microphone’s audio goes to xAI while the conversation is open/);
  await B.search(r.S); assert.equal(r.ui.btn().disabled, false, 'enabled once the search created the project'); assert.equal(r.ui.btn().textContent, 'Talk to the assistant');
  assert.equal(r.doc.head.children.filter(c => c.id === 'live-voice-style').length, 1, 'one scoped <style>');
}));
await t('voice: order is microphone, POST {mode:assistant}, socket with the secret subprotocol, session_update, disclosure_event; never response.create', () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S);
  await r.click();
  assert.deepEqual(r.order, ['mic', 'post', 'socket']); assert.deepEqual(r.posts, [{ mode: 'assistant' }]);
  assert.deepEqual(r.mediaDevices.asked, { audio: { echoCancellation: true, noiseSuppression: true, autoGainControl: true }, video: false }, 'audio only, echo cancellation and noise suppression');
  const ws = r.ws(); assert.equal(ws.url, vresp().websocket_url); assert.deepEqual(ws.protos, ['xai-client-secret.' + SECRET]);
  assert.equal(ws.sent.length, 0, 'nothing before open'); ws.onopen();
  assert.deepEqual(ws.sent, [vresp().session_update, vresp().disclosure_event]);
  assert.equal(B.voiceActive(), true); assert.equal(r.ui.btn().textContent, 'Stop'); assert.equal(r.ui.btn().attrs['aria-pressed'], 'true');
  // label, disclosure and data notice are on screen while it runs; "Connecting…" waits for the first sound
  assert.equal(r.ui.banner().hidden, false); const b = textOf(r.ui.banner());
  for (const s of [vresp().label, vresp().disclosure, vresp().data_notice, 'Voice assistant (automated)']) assert.ok(b.includes(s), s);
  assert.equal(boxes(r.doc).length, 1); assert.ok(textOf(boxes(r.doc)[0]).includes('Connecting…'));
  ws.emit({ type: 'response.output_audio.delta', delta: pcm(2400) }); assert.equal(boxes(r.doc).length, 0, 'waiting ends at the first sound');
  ws.emit({ type: 'response.done', response: { status: 'completed' } }); await tick(); await tick();
  for (const m of ws.sent) assert.notEqual(m.type, 'response.create');
  B.voiceToggle(); settled(r);
}));
await t('voice: no microphone frame before the first response.done, 100 ms frames after', () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S);
  await opened(r); const ws = r.ws();
  assert.equal(r.nodes.length, 0, 'no capture node during the disclosure');
  ws.emit({ type: 'input_audio_buffer.speech_started' }); assert.equal(r.stops.length, 0, 'the disclosure is not interrupted');
  ws.emit({ type: 'response.done' }); await tick(); await tick();
  assert.equal(r.nodes.length, 1); assert.equal(r.nodes[0].name, 'ss-mic-capture');
  r.nodes[0].port.onmessage({ data: new Float32Array(4800).fill(0.5) }); // 100 ms at the context's 48 kHz
  const appends = ws.sent.filter(m => m.type === 'input_audio_buffer.append');
  assert.equal(appends.length, 1); const bytes = Buffer.from(appends[0].audio, 'base64');
  assert.equal(bytes.length, 4800, '2400 samples of PCM16 = 100 ms at 24 kHz'); assert.equal(bytes.readInt16LE(0), Math.round(0.5 * 32767));
  r.nodes[0].port.onmessage({ data: new Float32Array(2400) }); assert.equal(ws.sent.filter(m => m.type === 'input_audio_buffer.append').length, 1, 'half a frame waits');
  B.voiceToggle(); settled(r); assert.equal(r.nodes[0].off, true, 'capture node disconnected');
  r.nodes[0].port.onmessage?.({ data: new Float32Array(4800) }); assert.equal(ws.sent.filter(m => m.type === 'input_audio_buffer.append').length, 1, 'nothing after stop');
}));
await t('voice: deltas are scheduled back to back at 24 kHz and the queue is cleared when the owner starts speaking', () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S); await opened(r); const ws = r.ws();
  assert.deepEqual(r.ctxs[0].opts, { sampleRate: 24000 });
  for (let i = 0; i < 3; i++) ws.emit({ type: 'response.output_audio.delta', delta: pcm(2400) });
  assert.deepEqual(r.starts.map(x => +x.toFixed(3)), [0.03, 0.13, 0.23]);
  ws.emit({ type: 'response.done' }); await tick();
  ws.emit({ type: 'response.output_audio.delta', delta: pcm(2400) }); ws.emit({ type: 'input_audio_buffer.speech_started', item_id: 'u1' });
  assert.equal(r.stops.length, 4, 'every queued buffer stopped');
  ws.emit({ type: 'response.output_audio.delta', delta: pcm(2400) }); assert.equal(+r.starts.at(-1).toFixed(3), 0.03, 'the next answer starts fresh');
  B.voiceToggle(); settled(r);
}));
await t('voice: transcripts are text with the two speaker marks; the claim notice only under a matching assistant turn', () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S); await opened(r); const ws = r.ws();
  ws.emit({ type: 'response.output_audio_transcript.delta', item_id: 'a1', delta: 'I have ' });
  ws.emit({ type: 'response.output_audio_transcript.delta', item_id: 'a1', delta: 'emailed' });
  assert.equal(r.ui.turns().length, 1, 'one pending entry'); assert.equal(textOf(r.ui.turns()[0]).includes('I have emailed'), true);
  ws.emit({ type: 'response.output_audio_transcript.done', item_id: 'a1', transcript: 'I have emailed the broker.' });
  ws.emit({ type: 'conversation.item.input_audio_transcription.completed', item_id: 'u1', transcript: '<img src=x onerror=alert(1)> call him' });
  ws.emit({ type: 'response.output_audio_transcript.done', item_id: 'a2', transcript: "I can't place calls, so nothing was done." });
  const [a1, u1, a2] = r.ui.turns();
  assert.equal(a1.children[0].textContent, 'Voice assistant (automated)'); assert.equal(u1.children[0].textContent, 'You (spoken)'); assert.match(u1.className, /\bme\b/);
  assert.equal(u1.children[1].textContent, '<img src=x onerror=alert(1)> call him', 'as text');
  assert.equal(a1.children.at(-1).textContent, vresp().no_action_notice); assert.equal(a1.children.at(-1).className, 'live-voice-claim');
  assert.equal(a2.children.length, 2, 'no notice under a refusal');
  assert.ok(!readFileSync(new URL('./backend.js', import.meta.url), 'utf8').split('/* --------------------------------------------- voice')[1].includes('innerHTML'), 'no innerHTML in the voice code');
  B.voiceToggle(); settled(r);
  assert.equal(r.ui.turns().length, 3, 'the entries stay in the log after the end (not saved across a reload)');
  assert.equal(r.ui.consent().hidden, true, 'the consent sentence is not repeated after a session has run');
}));
for (const [name, how, note] of [
  ['the Stop button', r => r.ui.btn().onclick(), B.VOICE_END.stop],
  ['a second click (stops, never starts another)', async r => { await r.ui.btn().onclick(); assert.equal(r.sockets.length, 1); }, B.VOICE_END.stop],
  ['the socket closing', r => r.ws().onclose(), B.VOICE_END.closed],
  ['a socket error', r => r.ws().onerror(), B.VOICE_END.error],
  ['the page hidden for 30 s', r => { r.doc.visibilityState = 'hidden'; r.listeners.visibilitychange(); fire(29000); assert.equal(B.voiceActive(), true); fire(1000); }, B.VOICE_END.hidden],
  ['the five-minute cap', r => { fire(5 * 60 * 1000); }, B.VOICE_END.cap],
  ['xAI ending it (max_duration)', r => r.ws().emit({ type: 'error', error: { type: 'max_duration', message: 'too long' } }), B.VOICE_END.provider('too long')],
  ['an error thrown inside a message handler (review)', r => { r.ctxs[0].createBuffer = () => { throw new Error('boom'); }; r.ws().emit({ type: 'response.output_audio.delta', delta: pcm(240) }); }, B.VOICE_END.error],
]) await t(`voice: stopping by ${name} closes the socket, stops the mic, the audio and every timer, resets the button`, () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S); await opened(r); r.ws().emit({ type: 'response.done' }); await tick(); await tick();
  r.ws().emit({ type: 'response.output_audio.delta', delta: pcm(240) });
  await how(r); settled(r); assert.deepEqual(notesOf(r), [note]); assert.equal(r.stops.length >= 1, true, 'queued audio stopped');
}));
await t('voice: Stop pressed while the browser asks for the microphone: no POST, and the stream granted later is stopped at once', () => withDoc(async () => {
  let grant; const r = await voiceRig({ micGate: new Promise(g => { grant = g; }) }); await B.search(r.S);
  const p = r.ui.btn().onclick(); await tick(); assert.equal(B.voiceActive(), true); assert.equal(r.ui.btn().textContent, 'Stop');
  r.ui.btn().onclick(); grant(); await p; await tick();
  assert.equal(r.tracks.length, 1); settled(r); assert.equal(r.posts.length, 0); assert.equal(r.sockets.length, 0); assert.deepEqual(notesOf(r), [B.VOICE_END.stop]);
}));
await t('voice (review): a first response.done that is cancelled or incomplete ends it; the microphone never sends', () => withDoc(async () => {
  for (const status of ['cancelled', 'incomplete']) {
    const r = await voiceRig(); await B.search(r.S); await opened(r); const ws = r.ws();
    ws.emit({ type: 'response.done', response: { status } }); await tick(); await tick();
    assert.equal(r.nodes.length, 0, 'no capture node'); assert.ok(ws.sent.every(m => m.type !== 'input_audio_buffer.append'));
    settled(r); assert.deepEqual(notesOf(r), [B.VOICE_END.undisclosed]);
  }
}));
await t('voice (review): a session started while the previous one is still ending runs alone; the old late stream is stopped', () => withDoc(async () => {
  let grant; const r = await voiceRig({ micGate: new Promise(g => { grant = g; }) }); await B.search(r.S);
  const p1 = r.ui.btn().onclick(); await tick(); r.ui.btn().onclick(); const p2 = r.ui.btn().onclick(); await tick();
  assert.equal(B.voiceActive(), true); grant(); await p1; await p2; await tick(); await tick();
  assert.equal(r.tracks.length, 2); assert.equal(r.tracks[0].readyState, 'ended', 'the first stream is stopped'); assert.equal(r.tracks[1].readyState, 'live');
  assert.equal(r.posts.length, 1, 'one POST'); assert.equal(r.sockets.length, 1, 'one socket'); assert.equal(r.ui.btn().textContent, 'Stop');
  B.voiceToggle(); settled(r);
}));
await t('voice: a hidden page that comes back in time keeps the conversation', () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S); await opened(r);
  r.doc.visibilityState = 'hidden'; r.listeners.visibilitychange(); fire(10000); r.doc.visibilityState = 'visible'; r.listeners.visibilitychange(); fire(60000);
  assert.equal(B.voiceActive(), true); B.voiceToggle(); settled(r); assert.equal(r.listeners.visibilitychange, undefined, 'listener removed');
}));
await t('voice: the secret expiring before the socket opened ends it; an already expired one opens no socket', () => withDoc(async () => {
  let r = await voiceRig({ answer: () => ({ status: 200, body: vresp({ expires_at: new Date(Date.now() + 2000).toISOString() }) }) }); await B.search(r.S);
  await r.click(); assert.equal(r.sockets.length, 1); fire(2500); settled(r); assert.deepEqual(notesOf(r), [B.VOICE_END.expired]);
  r = await voiceRig({ answer: () => ({ status: 200, body: vresp({ expires_at: new Date(Date.now() - 1000).toISOString() }) }) }); await B.search(r.S);
  await r.click(); assert.equal(r.sockets.length, 0); settled(r); assert.deepEqual(notesOf(r), [B.VOICE_END.expired]);
}));
for (const [name, opts, re] of [
  ['microphone refused', { mic: 'NotAllowedError' }, /^The microphone was not allowed, so no voice conversation was started\.$/],
  ['no voice route (404)', { answer: () => ({ status: 404, body: { error_code: 'NOT_FOUND', message: 'Not Found' } }) }, /^Voice is not available on this backend yet \(it has no voice route\)\. Nothing was sent to xAI\.$/],
  ['502, retryable', { answer: () => ({ status: 502, body: { error_code: 'PROVIDER_UNAVAILABLE', message: 'xAI timed out', retryable: true } }) }, /^Voice is not available right now: xAI timed out\. You can try again\.$/],
  ['502, not retryable', { answer: () => ({ status: 502, body: { error_code: 'PROVIDER_UNAVAILABLE', message: 'xAI refused the key', retryable: false } }) }, /Trying again will not help/],
  ['503, not configured', { answer: () => ({ status: 503, body: { error_code: 'VOICE_NOT_CONFIGURED', message: 'Voice is not configured' } }) }, /^Voice is not available: Voice is not configured\. Nothing was sent to xAI\.$/],
  ['an incomplete answer (ws:// url)', { answer: () => ({ status: 200, body: vresp({ websocket_url: 'ws://voice.example.invalid/' }) }) }, /incomplete/],
]) await t(`voice: ${name} gives one note, no socket, no open microphone`, () => withDoc(async () => {
  const r = await voiceRig(opts); await B.search(r.S); await r.click();
  assert.equal(r.sockets.length, 0); settled(r); const n = notesOf(r); assert.equal(n.length, 1); assert.match(n[0], re);
  if (opts.mic) assert.equal(r.posts.length, 0, 'no POST after a refusal');
}));
await t('voice: the secret is in no element, attribute, note or localStorage, before, during or after', () => withDoc(async () => {
  const r = await voiceRig(); await B.search(r.S); await opened(r); r.ws().emit({ type: 'error', error: { type: 'invalid_event', message: 'bad' } });
  const scan = () => { for (const n of r.doc.root.all()) { assert.ok(!String(n.textContent).includes(SECRET)); for (const v of Object.values(n.attrs)) assert.ok(!String(v).includes(SECRET)); assert.ok(!JSON.stringify(n.dataset || {}).includes(SECRET)); } };
  scan(); assert.ok(r.notes.every(n => !n.includes(SECRET)));
  try { for (let i = 0; i < (globalThis.localStorage?.length || 0); i++) assert.ok(!String(localStorage.getItem(localStorage.key(i))).includes(SECRET)); } catch (e) { if (e instanceof assert.AssertionError) throw e; }
  assert.ok(!readFileSync(new URL('./backend.js', import.meta.url), 'utf8').split('/* --------------------------------------------- voice')[1].match(/localStorage|sessionStorage|console\./), 'the voice code stores and logs nothing');
  B.voiceToggle(); scan(); settled(r);
}));
await t('voice: without ?live=1 none of it exists (no page file names it; only bind() makes it, and only where #chat exists)', () => {
  for (const f of ['workspace.js', 'workspace.css', 'workspace.html']) assert.doesNotMatch(readFileSync(new URL('./' + f, import.meta.url), 'utf8'), /live-voice|voice\/session|Talk to the assistant/, f);
  const mine = readFileSync(new URL('./backend.js', import.meta.url), 'utf8');
  assert.equal([...mine.matchAll(/\bvoiceElements\(/g)].length, 2, 'defined once, called once by bind()');
});

/* ------------------------------------------- web-link-8: no voice in the chat; the AI session card */
await t('talk: live mode alone makes no Talk control; ?talk=1 makes it, as a card inside the conversation log', () => withDoc(async () => {
  assert.deepEqual([B.config('?live=1').talk, B.config('?live=1&talk=1').talk, B.config('').talk], [false, true, false]);
  const pg = await waitPage(), { doc } = pg; const chat = doc.createElement('div'); chat.id = 'chat'; doc.root.children[1].append(chat);
  setSearch('?live=1'); B.bind({ ...pg.ctx, S: () => pg.S, map: () => null });
  assert.equal(doc.getElementById('live-voice'), null, 'no Talk control in live mode');
  assert.ok(!doc.root.all().some(n => n.textContent === B.VOICE.talk), 'no "Talk to the assistant" anywhere');
  assert.equal(doc.head.children.filter(c => c.id === 'live-card-style').length, 1, 'the card style is there for a call card');
  setSearch('?live=1&talk=1'); B.bind({ ...pg.ctx, S: () => pg.S, map: () => null });
  const box = doc.getElementById('live-voice'); assert.ok(box, 'present with ?talk=1');
  assert.equal(box.parent, doc.log, 'inside the scrolling conversation, not a fourth row of the chat grid'); assert.equal(doc.log.children.at(-1), box);
  assert.equal(box.className, 'live-card live-voice'); assert.ok(box.all().some(n => n.className === 'live-card-log'), 'turns go to the bounded transcript');
  assert.equal(chat.children.length, 0, 'nothing appended to #chat');
}));
// A fake #findings that renderFindings re-renders the way the page does (innerHTML replaced), and a
// MutationObserver that calls back in a microtask, as browsers do.
function findingsDoc() {
  const doc = waitDoc(), mos = []; doc.body = doc.root.children[1];
  const el0 = doc.createElement;
  doc.createElement = tag => { const e = el0(tag); e.after = function (n) { n.remove?.(); const k = this.parent.children; k.splice(k.indexOf(this) + 1, 0, n); n.parent = this.parent; notify(this.parent, n); };
    const ap = e.append; e.append = function (...c) { ap.apply(this, c); for (const n of c) notify(this, n); }; e.contains = function (n) { return n === this || this.all().includes(n); }; e.focus = function () { doc.activeElement = this; }; return e; };
  const f = doc.createElement('div'); f.id = 'findings'; doc.root.children[1].append(f);
  const notify = (parent, n) => { for (const m of mos) if (m.target === parent && !m.off) { m.q.push({ addedNodes: [n] }); if (!m.sched) { m.sched = true; queueMicrotask(() => { m.sched = false; const r = m.q.splice(0); if (r.length) m.cb(r); }); } } };
  let renders = 0;
  const render = () => { // renderFindings: scrollTop saved, innerHTML replaced, scrollTop set back
    const scroll = f.scrollTop; for (const c of [...f.children]) { c.parent = null; } f.children.length = 0; renders++;
    const h = doc.createElement('h2'); h.textContent = 'Before you sign'; const iss = doc.createElement('div'); iss.className = 'issues';
    const h2 = doc.createElement('h2'); h2.textContent = 'What it takes to open'; const tbl = doc.createElement('div'); tbl.className = 'table-scroll';
    for (const n of [h, iss, h2, tbl]) { n.parent = f; f.children.push(n); } for (const m of mos) if (m.target === f && !m.off) { m.q.push({ addedNodes: f.children }); if (!m.sched) { m.sched = true; queueMicrotask(() => { m.sched = false; const r = m.q.splice(0); if (r.length) m.cb(r); }); } }
    f.scrollTop = scroll;
  };
  class MO { constructor(cb) { this.cb = cb; this.q = []; } observe(t) { this.target = t; mos.push(this); } disconnect() { this.off = true; this.q.length = 0; } takeRecords() { return this.q.splice(0); } }
  return { doc, f, render, MO, mos, renders: () => renders };
}
await t('live call card: absent with no call; under the issue list; the same element after five re-renders, no duplicate, no loop; never positioned; gone when closed', () => withDoc(async () => {
  const { doc, f, render, MO, mos } = findingsDoc(); globalThis.document = doc; const realMO = globalThis.MutationObserver; globalThis.MutationObserver = MO;
  try {
    render(); await tick();
    assert.equal(doc.getElementById('live-call'), null, 'no call: no card, no reserved space');
    assert.deepEqual(f.children.map(c => c.className || c.textContent), ['Before you sign', 'issues', 'What it takes to open', 'table-scroll']);
    const lc = B.openLiveCall('Call to the property'); const card = lc.card;
    assert.equal(B.openLiveCall('again'), lc, 'one call at a time: one card');
    const where = () => f.children.map(c => c.id || c.className || c.textContent);
    assert.deepEqual(where(), ['Before you sign', 'issues', 'live-call', 'What it takes to open', 'table-scroll'], 'directly under the issue list');
    for (let i = 0; i < 12; i++) { const l = doc.createElement('p'); l.textContent = 'line ' + i; lc.log.append(l); }
    const btn = doc.createElement('button'); lc.body.append(btn);
    for (let i = 0; i < 5; i++) {
      f.scrollTop = 40; render(); lc.log.scrollTop = 0; doc.activeElement = doc.root.children[1]; // out of the DOM: scroll reset, focus lost
      await tick();
      assert.equal(doc.getElementById('live-call'), card, `render ${i + 1}: the same element`);
      assert.deepEqual(where(), ['Before you sign', 'issues', 'live-call', 'What it takes to open', 'table-scroll'], `render ${i + 1}: in place`);
      assert.equal(f.children.filter(c => c.id === 'live-call').length, 1, 'one card, no duplicate');
    }
    // no loop: the observer's own insert does not start another (one put per render)
    const before = mos[0].q.length; await tick(); await tick(); assert.equal(mos[0].q.length, before);
    assert.equal(card.style?.position, undefined); assert.equal(card.style?.zIndex, undefined);
    assert.doesNotMatch(B.CARD_STYLE, /position\s*:|z-index|margin[^;]*:[^;]*(^|[\s(])-\d|calc\(-|sticky|fixed|absolute/, 'flow layout only');
    assert.match(B.CARD_STYLE, /\.live-card-log \{[^}]*max-height: min\(40vh, 320px\); overflow-y: auto;/, 'bounded transcript');
    assert.match(B.CARD_STYLE, /var\(--space-/); assert.match(B.CARD_STYLE, /background: var\(--tint\)/);
    B.closeLiveCall(); assert.equal(doc.getElementById('live-call'), null, 'closed: gone');
    render(); await tick(); assert.equal(doc.getElementById('live-call'), null, 'and not put back by a later render');
    assert.equal(mos[0].off, true, 'observer disconnected');
  } finally { B.closeLiveCall(); if (realMO === undefined) delete globalThis.MutationObserver; else globalThis.MutationObserver = realMO; }
}));
await t('live call card keeper: restores the transcript scroll, the findings scroll and the focus after a re-render', () => withDoc(async () => {
  const { doc, f, render, MO } = findingsDoc(); globalThis.document = doc; const realMO = globalThis.MutationObserver; globalThis.MutationObserver = MO;
  const listeners = new Map(); const on = n => { n.addEventListener = (k, fn) => listeners.set(n.id + k + (n.className || ''), fn); };
  try {
    render(); await tick();
    const card = doc.createElement('section'); card.id = 'k'; const log = doc.createElement('div'); log.className = 'lg'; const b = doc.createElement('button'); card.append(log, b);
    on(f); on(log); on(card);
    const k = B.keepCard(f, card, { ...B.afterIssues, scrollers: [log] });
    f.scrollTop = 90; listeners.get('findingsscroll')(); log.scrollTop = 150; listeners.get('scrolllg')(); listeners.get('kfocusin')({ target: b });
    render(); f.scrollTop = 30; log.scrollTop = 0; doc.activeElement = doc.root.children[1]; await tick();
    assert.deepEqual([log.scrollTop, f.scrollTop, doc.activeElement], [150, 90, b]);
    k.stop();
  } finally { if (realMO === undefined) delete globalThis.MutationObserver; else globalThis.MutationObserver = realMO; }
}));
await t('keeper (review): a card only moved inside its container leaves the page\'s scroll alone; focus the person took away is not put back', () => withDoc(async () => {
  const { doc, f, render, MO } = findingsDoc(); globalThis.document = doc; const realMO = globalThis.MutationObserver; globalThis.MutationObserver = MO;
  const listeners = new Map(); const on = n => { n.addEventListener = (k, fn) => listeners.set(n.id + k, fn); };
  try {
    render(); await tick();
    const card = doc.createElement('section'); card.id = 'k'; const b = doc.createElement('button'); card.append(b); on(f); on(card);
    const last = { inPlace: (p, c) => p.children.at(-1) === c, place: (p, c) => { c.remove?.(); p.append(c); } }; // as the ?talk=1 card in #log
    const k = B.keepCard(f, card, last); assert.equal(f.children.at(-1), card);
    f.scrollTop = 10; listeners.get('findingsscroll')();
    const msg = doc.createElement('div'); f.append(msg); f.scrollTop = 500; await tick(); // the page appends a message and scrolls to it
    assert.equal(f.children.at(-1), card, 'moved back to the end'); assert.equal(f.scrollTop, 500, 'the page\'s scroll to its newest message stands');
    k.stop();
    const k2 = B.keepCard(f, card, B.afterIssues);
    card.isConnected = true; listeners.get('kfocusin')({ target: b }); listeners.get('kfocusout')({ relatedTarget: null }); // the person clicked away, card still in the page
    render(); doc.activeElement = doc.root.children[1]; await tick();
    assert.equal(doc.activeElement, doc.root.children[1], 'focus is not pulled back into the card');
    card.isConnected = false; listeners.get('kfocusin')({ target: b }); listeners.get('kfocusout')({ relatedTarget: null }); // lost because the card was taken out
    render(); doc.activeElement = doc.root.children[1]; await tick();
    assert.equal(doc.activeElement, b, 'focus lost to a re-render is put back');
    k2.stop();
  } finally { if (realMO === undefined) delete globalThis.MutationObserver; else globalThis.MutationObserver = realMO; }
}));
await t('without ?live=1 the page is untouched: backend.js is imported only on live=1, and the scripted "Ask the broker" path is the page\'s own', () => {
  const ws = readFileSync(new URL('./workspace.js', import.meta.url), 'utf8');
  assert.match(ws, /^const LIVE = new URLSearchParams\(location\.search\)\.get\('live'\) === '1' \? await import\('\.\/backend\.js'\) : null;/m);
  assert.match(ws, /if \(act === 'ask'\) return draft\(D\[P\[S\.finalist\]\.src\]\.issues\.find\(i => i\.id === t\.dataset\.issue\)\);/);
  const mine = readFileSync(new URL('./backend.js', import.meta.url), 'utf8');
  assert.doesNotMatch(mine, /addEventListener\('click'/, 'backend.js intercepts no click (the call from the page is a later commission)');
  for (const f of ['workspace.js', 'workspace.css', 'workspace.html']) assert.doesNotMatch(readFileSync(new URL('./' + f, import.meta.url), 'utf8'), /live-call|live-card/, f);
});
if (hadLocation) globalThis.location = realLocation; else delete globalThis.location;
B.useVoiceEnv(null);

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
  let results, narrowed;
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
    const narrower = narrowed = await be.search({ ...req, rent: 5000 }); // a changed answer: PUT business-profile, then search again
    assert.ok(narrower.every(r => r.rec.commercial_listing.asking_rent_monthly <= 500000), 'rent limit honoured');
    console.log('      with rent up to $5,000:', narrower.length, 'listings');
  });
  await t('local copies: live listings whose own suite scan id is in assets/places.json (read at run time, not copied)', async () => {
    let places;
    try { places = JSON.parse(readFileSync(new URL('./assets/places.json', import.meta.url))); } catch { console.log('      no readable assets/places.json: nothing to match'); return; }
    const index = B.localIndex(places), urls = [];
    for (const [label, set] of [['no rent limit', results], ['rent up to $5,000', narrowed || []]]) {
    console.log(`      search with ${label}: ${set.length} listings`);
    const pool = B.disambiguate(set.map(r => B.poolEntry(r.rec, be.api)));
    for (const [i, r] of set.entries()) {
      const h = B.localMatch(r.rec, index); if (!h) continue;
      const p = B.withLocal(pool[i], h), page = { [p.id]: p };
      const files = [p.photo, ...p.live.images, ...['ortho', 'oblique-n', 'oblique-e', 'oblique-s', 'oblique-w'].map(d => `vexcel-${d}.jpg`)];
      const got = [...new Set(files.map(f => B.asset(p.src, f, page)).filter(u => u.startsWith('assets/')))];
      got.push(`assets/dollhouses/${p.src}.png`, `assets/meshes/${p.src}.obj`); urls.push(...got);
      const notes = B.spaceNotes(r.rec, be.api, h);
      assert.doesNotMatch([p.suite, notes.description].join(' '), /Demo|synthetic|fixture/i);
      console.log(`      listing ${i + 1} -> ${p.src}: ${got.length} local files (${p.live.images.length} in Photos); scan ${p.live.scan_text}; suite line [${p.suite.replace(/^[^·]*·/, '<unit> ·')}]`);
      const same = set.filter(x => (x.rec.interior_assets || []).some(a => a.asset_id === r.rec.interior_assets.find(b => b.has_measured_geometry)?.asset_id)).length;
      console.log(`        listings in this result carrying that model id at all: ${same} (a building-only copy does not match)`);
    }
    }
    console.log(`      matched ${urls.length ? new Set(urls.map(u => u.split('/')[u.startsWith('assets/dollhouses') || u.startsWith('assets/meshes') ? 2 : 1].replace(/\.(png|obj)$/, ''))).size : 0} of ${index.size} local suites`);
    if (process.env.LOCAL_URLS_OUT) (await import('node:fs')).writeFileSync(process.env.LOCAL_URLS_OUT, [...new Set(urls)].join('\n') + '\n');
  });
  await t('interior: the chosen scan per listing, the tour URL, and the media route as the page asks for it (at most 2 scans)', async () => {
    const pool = results.map(r => B.poolEntry(r.rec, be.api)), its = pool.map(p => p.live.interior);
    const n = k => its.filter(x => x?.scope === k).length;
    console.log(`      of ${pool.length}: suite scan ${n('suite')}, building scan only ${n('building')}, no scan ${its.filter(x => !x).length}`);
    for (const [i, it] of its.entries()) if (it) { assert.ok(B.tourUrl(it.asset_id)); if (it.scope === 'building') assert.match(it.tour_text, /^3D tour of another space in this building: /); else assert.equal(it.tour_text, '3D tour of this suite · Matterport tour (live)'); }
    for (const it of [its.find(x => x?.scope === 'suite'), its.find(x => x?.scope === 'building')].filter(Boolean)) {
      for (const f of ['manifest.json', 'mesh.obj', 'photo/1.jpg']) {
        const r = await fetch(`${be.api}/api/v1/media/matterport/${it.asset_id}/${f}`, { headers: { Origin: ORIGIN } }); // as the page: no token
        const body = await r.text(); let code = null; try { code = JSON.parse(body).error_code ?? null; } catch { /* not JSON */ }
        console.log(`      ${it.scope} scan: GET …/${f} -> ${r.status} ${r.headers.get('content-type') || ''} ${code || ''} allow-origin ${r.headers.get('access-control-allow-origin')}`);
      }
    }
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
  await t('no fit (web-link-7): what the owner reads for each listing with a suite-own live scan (at most 2)', async () => {
    const own = results.map(r => B.poolEntry(r.rec, be.api)).filter(p => p.live.floor_is_suite).slice(0, 2);
    if (!own.length) { console.log('      no listing with a suite-own live scan in this search'); return; }
    for (const p of own) {
      try { await be.layout(p.live.property_id); console.log('      <unit>: a layout was generated'); } catch (e) {
        const r = B.layoutResult(e, p);
        console.log(`      <unit>: ${r ? `${r.kind} | ${r.heading} | ${r.lines.map(l => l.join(': ')).join(' | ')} | ${r.reasons.join('; ')} | ${r.tail.join(' ')}` : 'old note: ' + e.message}`);
      }
    }
  });
  await t('chat through the Rust origin (forwarded to Python)', async () => {
    try { const out = await be.chat('rank the properties'); console.log(`      mode ${out.mode}: ${out.reply.split('\n')[0]}`); assert.ok(out.reply); } catch (e) { console.log('      ' + B.chatFailure(e)); throw e; }
  });
  // Voice: a probe with an invalid mode cannot mint (the route validates the body first): 404 means no route.
  // Only then is the page's own request sent, which also answers 404; with the route present nothing is sent.
  await t('voice: the route on this backend; never a mint from this test', async () => {
    let probe; try { await be.req('POST', be.p('/voice/session'), { mode: 'not-a-mode' }); probe = 200; } catch (e) { probe = e.status; }
    console.log(`      probe with an invalid mode -> ${probe}`);
    if (probe !== 404) { console.log('      the voice route exists here: the assistant request is not sent (it would mint an xAI secret)'); return; }
    try { await be.req('POST', be.p('/voice/session'), { mode: 'assistant' }); assert.fail('answered 2xx after a 404 probe'); }
    catch (e) { if (e instanceof assert.AssertionError) throw e; console.log(`      POST {mode:assistant} -> ${e.status}; the page's note: ${B.voiceFailure(e)}`); assert.equal(e.status, 404); }
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
