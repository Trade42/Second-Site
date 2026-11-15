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
});
const [alh, hw] = F.properties;
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
    const req = { business: 'Facial studio', zones: null, rent: 7500, rooms: 3 };
    results = await be.search(req);
    assert.ok(results.length >= 2, `${results.length} listings`);
    const pool = results.map(r => B.poolEntry(r.rec));
    for (const p of pool) assert.equal(p.live.data_origin, 'FIXTURE_SYNTHETIC');
    console.log('      listings:', pool.map(p => `${p.address} $${p.rent}/mo ${p.sqft} SF zone=${p.zone} [${p.suite}]`).join('; '));
    await be.search({ ...req, rent: 5000 }); // a changed answer: PUT business-profile, then search again
    const narrower = await be.search({ ...req, rent: 5000 });
    assert.ok(narrower.every(r => r.rec.commercial_listing.asking_rent_monthly <= 500000), 'rent limit honoured');
    console.log('      with rent up to $5,000:', narrower.map(r => r.rec.property_identity.street_address).join(', ') || 'none');
  });
  await t('stage 2 compare: findings and costs for each listing', async () => {
    for (const { rec, assessments, scenarios } of results) {
      const d = B.detailEntry(rec, assessments, scenarios);
      assert.ok(d.issues.length > 0); assert.ok(d.build > 0);
      console.log(`      ${rec.property_identity.street_address}: ${d.issues.map(i => `${i.id}=${i.status}/${i.kind}`).join(' ')}; renovation $${d.build}, FF&E $${d.other}, NNN ${d.extras == null ? 'not stated' : '$' + d.extras}`);
    }
  });
  await t('stage 3 furnish: layout generation and bill of materials', async () => {
    const hw = results.find(r => (r.rec.interior_assets || []).some(a => a.has_measured_geometry));
    assert.ok(hw, 'a listing with geometry');
    const out = await be.layout(hw.rec.property_id);
    assert.equal(out.layout.approval_status, 'PROPOSED');
    const { rooms } = B.roomsFrom(out.layout, out.geometry), cat = B.catalogFrom(out.bom, rooms);
    assert.equal(out.geometry.data_origin, 'FIXTURE_SYNTHETIC');
    console.log(`      ${rooms.length} rooms (${rooms.map(r => r.name + ' ' + r.dim).join(', ')}); ${cat.placed.length} parts, total $${cat.estimated_total} (${out.bom.status})`);
    const noGeo = results.find(r => r !== hw);
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
