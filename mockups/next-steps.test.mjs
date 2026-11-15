// Tests for next-steps.js. No packages: `node mockups/next-steps.test.mjs`.
// The pure functions (what is shown, what is sent, where a request stands) and the file's own rules.
// The cards on screen are seen in a browser, not here (reports/web-link-11.md).
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import * as N from './next-steps.js';

let pass = 0, fail = 0;
async function t(name, fn) {
  try { await fn(); pass++; console.log('ok   ', name); } catch (e) { fail++; console.log('FAIL ', name, '\n     ', String(e.message).split('\n').slice(0, 6).join('\n      ')); }
}
const PID = '00000000-0000-4000-8000-00000000a001', HOSTILE = '<img src=x onerror="window.__pwn=1">';
const rec = (c = { name: 'Dana Broker', phone: '+15555550100', company: 'Co', role: 'broker' }) => ({ property_id: PID, commercial_listing: { broker_contact: c } });
const contact = N.contactOf(rec());

await t('clip and words bound every string and keep the backend’s own words', () => {
  assert.equal(N.clip('a\n\n b   c'), 'a b c');
  assert.equal(N.clip('x'.repeat(1000)).length, 300); assert.ok(N.clip('x'.repeat(1000)).endsWith('…'));
  assert.equal(N.words('AWAITING_APPROVAL'), 'awaiting approval'); assert.equal(N.words(null), ''); assert.equal(N.words({}), '');
});
await t('the contact: a name and a phone, or nobody to ask', () => {
  assert.deepEqual(contact, { name: 'Dana Broker', phone: '+15555550100', company: 'Co', role: 'broker', ok: true });
  for (const c of [null, {}, { name: 'A' }, { phone: '+15555550100' }, { name: 'A', phone: 'call me' }, { name: 7, phone: '+15555550100' }]) assert.equal(N.contactOf(rec(c)).ok, false, JSON.stringify(c));
  assert.equal(N.contactOf(undefined).ok, false);
  assert.equal(N.contactOf(rec({ name: 'N'.repeat(500), phone: '+15555550100' })).name.length, 120);
});
const D = {
  rooms: [{ id: 'r1' }, { id: 'r2' }], sketch: null, geometry: { source: 'MATTERPORT_MEASURED', data_origin: 'LIVE', scale_verified: true },
  bom: { estimated_total: 13776.5, placed: [] }, scenario: { low: 123180, expected: 208184, high: 334523 }, extras_unknown: true,
  issues: [
    { id: 'layout', status: 'MEETS', title: 'Floor area: meets', next: 'never shown' },
    { id: 'power', status: 'UNKNOWN', title: 'Electrical service: unknown', next: 'What amperage is allocated?' },
    { id: 'plumbing', status: 'MAY_REQUIRE_WORK', title: 'Sinks &amp; drains: may require work', next: null },
    { id: 'hvac', status: 'CONFLICTING', title: `Air ${HOSTILE.replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;')}: conflicting`, next: 'Q'.repeat(999) },
  ],
};
const P = { address: '100 S Example Ave', rent: 4126, rentKnown: true, sqft: 1300, sqftKnown: true, live: { property_id: PID, source_label: 'CoStar listing (data-kit snapshot)' } };
const lib = { billHead: () => ({ amount: 13776.5, line: 'The backend’s bill of materials total for this layout: 20 items at planning prices, not quotes.' }), geometryWords: g => `${g.geometry_source.toLowerCase().replace(/_/g, ' ')} (live provider data)`, unitName: p => `${p.address}, Suite 210` };
await t('open issues: nothing the backend calls met, each status in its words, the first question', () => {
  const o = N.openIssues(D);
  assert.deepEqual(o.map(i => i.status), ['unknown', 'may require work', 'conflicting']);
  assert.equal(o[0].question, 'What amperage is allocated?'); assert.equal(o[1].question, null); assert.equal(o[1].text, 'Sinks & drains: may require work');
  assert.equal(o[2].question.length, 240); assert.ok(o[2].text.includes('<img'), 'shown as text, by textContent');
  assert.ok(!JSON.stringify(o).match(/\bmeets\b|\bmet\b/i));
  assert.equal(N.openIssues({ issues: Array.from({ length: 40 }, (_, i) => ({ id: i, status: 'UNKNOWN', title: 't' })) }).length, 12);
  assert.deepEqual(N.openIssues(null), []);
});
await t('what you chose: the listing, the floor’s label, the bill total, the cost to open with low and high', () => {
  const f = N.chosenFacts(P, D, {}, {}, lib);
  assert.equal(f.name, '100 S Example Ave, Suite 210'); assert.equal(f.origin, 'CoStar listing (data-kit snapshot)');
  assert.equal(f.rent, '$4,126 a month, asking'); assert.equal(f.area, '1,300 SF');
  assert.match(f.layout, /^A proposed layout of 2 rooms, not an approved plan\.$/);
  assert.equal(f.sketch, false); assert.equal(f.floor, 'Floor geometry: matterport measured (live provider data).');
  assert.match(f.bill, /^\$13,777\. The backend’s bill of materials total/);
  assert.match(f.open, /^About \$208,184 expected \(low \$123,180, high \$334,523\): the backend’s cost scenario, a planning estimate, not a quote\. Lease extras \(NNN\/CAM\) are not stated and are not in it\.$/);
  const label = 'Approximate sketch on the listed area (1,300 SF as the listing states it). Not a measured plan.';
  const s = N.chosenFacts({ ...P, rentKnown: false, sqftKnown: false }, { ...D, sketch: { label }, scenario: null, bom: null }, {}, {}, lib);
  assert.equal(s.sketch, true); assert.equal(s.floor, label, 'the furnish stage’s label, as it is');
  assert.equal(s.rent, 'Rent not stated'); assert.equal(s.area, 'Area not stated');
  assert.equal(s.open, 'The backend gave no cost scenario for this listing.'); assert.equal(s.bill, 'The backend gave no bill of materials.');
  assert.equal(N.chosenFacts({ address: 'demo' }, D, {}, {}, lib), null, 'never for the page’s own demo listing');
});
await t('the outreach request: questions only carries no `purposes` (a backend before c50c34b takes it)', () => {
  const { body } = N.outreachBody(PID, contact, { questions: ['Q1', '  '], free: 'Is parking included?' });
  assert.deepEqual(body, { property_id: PID, channel: 'VOICE', recipient: { role: 'broker', name: 'Dana Broker', phone: '+15555550100' }, purpose: 'Questions about the listing', questions: ['Q1', 'Is parking included?'] });
  assert.deepEqual(Object.keys(body.recipient), ['role', 'name', 'phone'], 'the order the backend’s approval key reads');
  assert.equal(N.outreachBody(PID, contact, { questions: Array.from({ length: 50 }, (_, i) => 'q' + i) }).body.questions.length, 20);
});
await t('the outreach request: a visit and rent limits in the backend’s shape; the maximum only in its own field', () => {
  const { body } = N.outreachBody(PID, contact, { questions: ['Q1'], visit: [{ start: '2026-11-18T15:00', end: '2026-11-18T17:00' }, { start: '', end: '' }], target: '4,000', max: '$4517', counters: 1 }, -480);
  assert.deepEqual(body.purposes, { tour: { windows: [{ start: '2026-11-18T15:00:00-08:00', end: '2026-11-18T17:00:00-08:00' }], duration_minutes: 30 }, negotiation: { target_monthly_rent_cents: 400000, max_monthly_rent_cents: 451700, max_counters: 1 } });
  assert.equal(body.purpose, 'Questions about the listing, a visit, the rent');
  const { purposes, ...rest } = body; assert.ok(!/4517/.test(JSON.stringify(rest)), 'the purpose is texted to the owner: no limit in it');
  assert.equal(N.withOffset('2026-11-18T15:00', 330), '2026-11-18T15:00:00+05:30'); assert.equal(N.withOffset('tomorrow'), null);
});
await t('the outreach request: what is wrong is one line and nothing is sent', () => {
  const e = f => N.outreachBody(PID, contact, f).error;
  assert.match(e({}), /Tick at least one question/);
  assert.match(e({ questions: ['q'], target: '4000' }), /both a target and a maximum/);
  assert.match(e({ questions: ['q'], target: '5000', max: '4000' }), /cannot be above the maximum/);
  assert.match(e({ questions: ['q'], target: '4000', max: '4500', counters: 9 }), /0 to 5/);
  assert.match(e({ questions: ['q'], visit: [{ start: '2026-11-18T15:00', end: '' }] }), /both a start and an end/);
  assert.match(e({ questions: ['q'], visit: [{ start: '2026-11-18T15:00', end: '2026-11-18T14:00' }] }), /end after it starts/);
  assert.equal(N.outreachBody(PID, N.contactOf(rec(null)), { questions: ['q'] }).error, N.TEXT.noContact);
  for (const x of ['4000', '5000', '4500']) assert.ok(![e({ questions: ['q'], target: '5000', max: '4000' }), e({ questions: ['q'], target: '4000', max: '4500', counters: 9 })].some(l => l.includes(x)), 'no figure in an error line');
});
const comm = (o = {}) => ({ communication_id: 'c1', property_id: PID, channel: 'VOICE', approval_id: 'a1', created_at: '2026-11-15T09:00:00Z', ...o });
const appr = (approval_status, o = {}) => ({ approval_id: 'a1', approval_status, action_payload_hash: 'h', summary: 's', expires_at: '2026-11-16T09:00:00Z', ...o });
const NOW = Date.parse('2026-11-15T10:00:00Z');
await t('where the request stands comes from the backend’s lists', () => {
  const ph = (lists) => N.stateOf(lists, PID, NOW).phase;
  assert.equal(ph(null), 'none'); assert.equal(ph({ comms: 'x', approvals: {}, calls: 3 }), 'none');
  assert.equal(ph({ comms: [comm({ property_id: 'other' })], approvals: [appr('PENDING')] }), 'none', 'another listing’s request');
  assert.equal(ph({ comms: [comm({ channel: 'PHOTON_IMESSAGE' })], approvals: [appr('PENDING')] }), 'none', 'an owner text is not a call request');
  assert.equal(ph({ comms: [comm()], approvals: [appr('PENDING')] }), 'pending');
  assert.equal(ph({ comms: [comm()], approvals: [appr('APPROVED')] }), 'approved');
  assert.equal(ph({ comms: [comm()], approvals: [appr('APPROVED', { expires_at: '2026-11-15T09:59:00Z' })] }), 'expired');
  assert.equal(ph({ comms: [comm()], approvals: [appr('REJECTED')] }), 'refused');
  assert.equal(ph({ comms: [comm()], approvals: [appr('EXPIRED')] }), 'expired');
  assert.equal(ph({ comms: [comm()], approvals: [appr('EXECUTED')] }), 'used');
  assert.equal(ph({ comms: [comm()], approvals: [appr('CANCELLED')] }), 'other');
  assert.equal(ph({ comms: [comm()], approvals: [] }), 'unknown');
  for (const k of ['refused', 'expired', 'used', 'other']) assert.match(N.PHASE_LINE[k], /a request with a visit or rent limits \(or different ones\) is a new request\.$/);
  assert.doesNotMatch(Object.values(N.PHASE_LINE).join(' '), /request approval again/i, 'no promise the backend does not keep');
  assert.match(N.sameApproval('REJECTED'), /^The backend answered with the earlier approval for this listing and contact \(rejected\), not a new one\./);
  // a repeated request: a second communication on the same, used approval still shows that approval's call
  const again = N.stateOf({ comms: [comm(), comm({ communication_id: 'c9', created_at: '2026-11-15T09:50:00Z' })], approvals: [appr('EXECUTED')], calls: [{ call_id: 'k1', property_id: PID, communication_id: 'c1', approval_id: 'a1', state: 'COMPLETED' }] }, PID, NOW);
  assert.equal(again.phase, 'call'); assert.equal(again.call.call_id, 'k1');
  const s = N.stateOf({ comms: [comm(), comm({ communication_id: 'c2', approval_id: 'a2', created_at: '2026-11-15T09:30:00Z' })], approvals: [appr('EXECUTED'), appr('EXECUTED', { approval_id: 'a2' })], calls: [{ call_id: 'k1', property_id: PID, communication_id: 'c1' }, { call_id: 'k2', property_id: PID, communication_id: 'c2', state: 'COMPLETED' }], handoffs: [{ call_id: 'k1' }, { call_id: 'k2', reason: 'FINANCIAL_DETAILS' }] }, PID, NOW);
  assert.equal(s.phase, 'call'); assert.equal(s.call.call_id, 'k2', 'the newest request and its own call'); assert.equal(s.handoffs.length, 1);
  assert.equal(N.callDone(s.call), true); assert.equal(N.callDone({ state: 'DIALING' }), false);
});
await t('simulated or live: /ready’s Telnyx line when it can be read, else unknown', () => {
  assert.equal(N.callsMode({ providers: [{ provider: 'telnyx', mode: 'fixture' }] }), 'simulated');
  assert.equal(N.callsMode({ providers: [{ provider: 'photon', mode: 'fixture' }, { provider: 'telnyx', mode: 'LIVE' }] }), 'live');
  for (const r of [null, {}, { providers: 'x' }, { providers: [{ provider: 'telnyx' }] }, { providers: [{ provider: 'telnyx', mode: 'other' }] }]) assert.equal(N.callsMode(r), 'unknown');
});
await t('a failed request is one plain, bounded line', () => {
  assert.match(N.say({ status: 0 }, 'Your decision'), /^Your decision got no answer from the backend \(the connection failed\), so this page does not know whether it arrived\. Nothing was repeated; what is shown is read again from the backend\.$/);
  assert.equal(N.say({ status: 409, data: { error_code: 'SIMULATOR_DISABLED', message: 'x' } }), N.TEXT.liveRefused);
  assert.match(N.TEXT.liveRefused, /never rings a phone: no call was started and the approval was not used/);
  assert.equal(N.say({ status: 409, data: { error_code: 'APPROVAL_REQUIRED', message: 'approval is EXECUTED' } }, 'The simulated call'), 'The simulated call was not accepted: the backend answered 409 APPROVAL_REQUIRED (approval is EXECUTED). Nothing was repeated.');
  assert.equal(N.say({ status: 422, data: { detail: [{ type: 'extra_forbidden', loc: ['body', 'purposes'], msg: 'Extra inputs are not permitted' }] } }), N.TEXT.oldBackend);
  assert.match(N.say({ status: 422, data: { detail: [{ type: 'value_error', loc: ['body', 'purposes'], msg: 'purposes could not be read' }] } }), /422 \(purposes could not be read\)/);
  const long = N.say({ status: 500, data: { message: HOSTILE + 'x'.repeat(5000), error_code: 'E'.repeat(500) } });
  assert.ok(long.length < 420, String(long.length));
  assert.match(N.say({ status: 502, data: 'Bad gateway' }), /answered 502\. Nothing was repeated\.$/);
});
await t('a call’s parts: every heading carries what the backend says of the call; all bounded', () => {
  const call = { simulated: true, state: 'COMPLETED', transcript: Array.from({ length: 90 }, (_, i) => ({ speaker: 'assistant', text: 'line ' + i + ' ' + 'x'.repeat(900) })), findings: [{ question: 'Amps?', answer: '200 A', classification: 'OWNER_REPORTED' }, { question: 'Water?', answer: null }], deferred_questions: ['Deposit by Friday?'], outcome_summary: 'S'.repeat(5000) };
  const c = N.callParts(call, [{ reason: 'FINANCIAL_DETAILS', question: 'Deposit by Friday?', status: 'ACCEPTED' }]);
  assert.equal(c.head, 'Call, simulated: completed'); assert.equal(c.tag, 'simulated');
  assert.equal(c.transcript.length, 60); assert.equal(c.more, 30); assert.ok(c.transcript.every(l => l.length < 420));
  assert.deepEqual(c.findings, ['Amps?: 200 A (owner reported)', 'Water?: no answer (class not given)']);
  assert.deepEqual(c.holds, ['financial details: Deposit by Friday? (accepted)']); assert.equal(c.summary.length, 1500);
  const real = N.callParts({ simulated: false, state: 'ACTIVE', failure_reason: 'f' });
  assert.equal(real.sim, false); assert.equal(real.head, 'Call, not labelled simulated by the backend: active (f)');
  assert.equal(N.callParts({}).head, 'Call, not labelled simulated by the backend: state not given');
});
await t('without a page there is nothing to mount; the last sentence promises nothing', () => {
  assert.equal(N.mount({ page: {}, backend: () => null }), null); assert.equal(N.mount(null), null);
  assert.match(N.TEXT.end, /^Nothing here has been signed, nothing has been sent to the listing’s contact, and no money has moved: a call started here is a simulation and the payments are mock money in a sandbox\.$/);
  assert.match(N.TEXT.live, /never rings a phone, so it offers no start button and asks for no new approval here\.$/);
  assert.match(N.TEXT.sim, /^No phone will ring\. This is a simulation of the call/);
});
await t('the file’s own rules: no HTML from strings, a call is only ever started as a simulation', () => {
  const src = readFileSync(new URL('./next-steps.js', import.meta.url), 'utf8').split('\n').filter(l => !l.trim().startsWith('//')).join('\n');
  assert.doesNotMatch(src, /innerHTML|outerHTML|insertAdjacentHTML|document\.write/);
  const starts = src.match(/req\('POST', `\$\{P\(\)\}\/calls`, [^)]*\)/g) || [];
  assert.equal(starts.length, 1); assert.match(starts[0], /simulate: true/);
  const sims = src.match(/if \(call\?\.simulated !== true\) \{ st\.line = TEXT\.notLabelled; return; \}/g) || [];
  assert.equal(sims.length, 1, 'the simulate route is asked only for a call labelled simulated');
  assert.doesNotMatch(src, /setItem\([^)]*(max|form)/i);
  assert.doesNotMatch(src, /8010|8011/);
});
/* ---- the card itself, mounted on a minimal DOM against a stub (reviewer 403f74b4): what it offers and what it sends */
function world(opt = {}) {
  const capture = [];
  class El {
    constructor(tag) { this.tagName = String(tag).toUpperCase(); this.children = []; this.attrs = {}; this.L = {}; this.style = {}; this.className = ''; this.hidden = false; this._t = ''; this.parentElement = null; this.value = ''; this.disabled = false; const c = new Set(); this.classList = { add: x => c.add(x), remove: x => c.delete(x) }; }
    set textContent(v) { this.children = []; this._t = String(v); } get textContent() { return this._t + this.children.map(c => c.textContent).join(' '); }
    append(...n) { for (const x of n) { if (x == null) continue; if (x.parentElement) x.parentElement.children = x.parentElement.children.filter(c => c !== x); x.parentElement = this; this.children.push(x); } }
    replaceChildren(...n) { for (const c of this.children) c.parentElement = null; this.children = []; this._t = ''; this.append(...n); }
    setAttribute(k, v) { this.attrs[k] = String(v); } addEventListener(k, fn) { (this.L[k] ??= []).push(fn); }
    connected() { let n = this; while (n.parentElement) n = n.parentElement; return n === doc.body; }
    click() { if (this.disabled || !this.connected()) return false; const e = { target: this, preventDefault() {} }; for (const fn of [...capture, ...(this.L.click || [])]) fn(e); return true; }
    contains(n) { for (let x = n; x; x = x.parentElement) if (x === this) return true; return false; } closest() { return null; } focus() {} querySelector() { return null; }
    all(fn, out = []) { if (fn(this)) out.push(this); for (const c of this.children) c.all(fn, out); return out; }
  }
  const doc = { head: new El('head'), body: new El('body'), activeElement: null, createElement: x => new El(x), querySelector: () => null, getElementById: id => doc.body.all(e => e.id === id)[0] || null, addEventListener: (k, fn, cap) => { if (k === 'click' && cap) capture.push(fn); } };
  const main = new El('main'), furnish = new El('section'); furnish.id = 'furnish'; main.append(furnish); doc.body.append(main);
  globalThis.document = doc; globalThis.localStorage = undefined;
  const P = '/api/v1/projects/proj', log = [], db = { comms: [], approvals: [], calls: [], handoffs: [], ...(opt.db || {}) }, sleep = ms => new Promise(r => setTimeout(r, ms));
  const err = (status, data) => Object.assign(new Error('x'), { status, data });
  const req = async (method, path, body) => {
    log.push(method + ' ' + path.replace(P, 'P')); if (opt.slow) await sleep(opt.slow);
    if (path === '/ready') { const r = opt.ready(); if (r instanceof Error) throw r; return r; }
    if (method === 'GET') { const k = { '/communications': 'comms', '/approvals': 'approvals', '/calls': 'calls', '/handoffs': 'handoffs' }[path.replace(P, '')]; if (!k) throw err(404, null); return structuredClone(db[k]); }
    if (/decision$/.test(path)) { const a = db.approvals[0]; if (a.approval_status !== 'PENDING') throw err(409, { error_code: 'APPROVAL_NOT_PENDING' }); a.approval_status = body.decision; return a; }
    if (path === P + '/calls') { db.calls = [{ call_id: 'k1', property_id: PID, communication_id: 'c1', approval_id: 'ap1', state: 'COMPLETED', simulated: true, transcript: [] }]; return db.calls[0]; }
    throw err(404, null);
  };
  const be = { project: 'proj', signIn: async () => {}, p: r => P + r, req }, S = { chosen: 'k', stage: 'furnish' };
  const page = { S: () => S, P: { k: { src: 's', address: '1 Main St', live: { property_id: PID } } }, D: { s: { rooms: [{}], issues: [] } }, C: {} };
  N.unmount(); const h = N.mount({ page, backend: () => be, records: () => [{ rec: rec() }], lib: {} });
  const B = label => doc.body.all(e => e.tagName === 'BUTTON' && e.connected() && e.textContent === label)[0];
  return { h, log, db, B, sleep, top: () => doc.body.all(e => e.className === 'ns-sim')[0]?.textContent, text: () => doc.body.textContent, sent: re => log.filter(x => re.test(x)).length, settle: () => sleep((opt.slow || 0) * 8 + 40), open: async function () { B(N.TEXT.go).click(); await this.settle(); } };
}
const FIXTURE = { providers: [{ provider: 'telnyx', mode: 'fixture' }] }, LIVE_ = { providers: [{ provider: 'telnyx', mode: 'live' }] };
const seeded = (status, call) => ({ comms: [{ communication_id: 'c1', property_id: PID, channel: 'VOICE', approval_id: 'ap1', created_at: '1' }], approvals: [{ approval_id: 'ap1', approval_status: status, action_payload_hash: 'H', summary: 'VOICE to Dana', expires_at: '2099-01-01T00:00:00Z' }], calls: call ? [call] : [] });
await t('the card: an approval and a start are offered only when /ready says the Telnyx mode is fixture', async () => {
  for (const ready of [() => Object.assign(new Error('x'), { status: 403 }), () => Object.assign(new Error('x'), { status: 0 }), () => ({ providers: [{ provider: 'telnyx' }] }), () => '<html>', () => null, () => LIVE_]) {
    for (const status of ['PENDING', 'APPROVED']) {
      const w = world({ ready, db: seeded(status) }); await w.open();
      assert.equal(w.B('Start the simulated call'), undefined, 'no start button'); assert.equal(w.B('Approve')?.disabled ?? true, true); assert.equal(w.B('Request approval')?.disabled ?? true, true);
      assert.ok([N.TEXT.unknownMode, N.TEXT.live].includes(w.top()), w.top()); assert.equal(w.sent(/^POST/), 0);
    }
  }
  const w = world({ ready: () => FIXTURE, db: seeded('APPROVED') }); await w.open();
  assert.equal(w.top(), N.TEXT.sim); assert.equal(w.B('Start the simulated call').disabled, false);
});
await t('the card: the backend goes live between the read and the click, and nothing is sent', async () => {
  for (const [status, label] of [['APPROVED', 'Start the simulated call'], ['PENDING', 'Approve']]) {
    let live = false; const w = world({ ready: () => (live ? LIVE_ : FIXTURE), db: seeded(status) }); await w.open();
    live = true; assert.equal(w.B(label).click(), true); await w.settle();
    assert.equal(w.sent(/^POST/), 0, label); assert.equal(w.top(), N.TEXT.live); assert.equal(w.B('Start the simulated call'), undefined);
  }
});
await t('the card: a call the backend does not label simulated is said to be started and is never drawn as a simulation', async () => {
  for (const simulated of [undefined, false, 'true']) {
    const w = world({ ready: () => FIXTURE, db: seeded('EXECUTED', { call_id: 'k1', property_id: PID, communication_id: 'c1', state: 'COMPLETED', simulated, transcript: [{ speaker: 'broker', text: 'A REAL VOICE' }], outcome_summary: 'REAL SUMMARY' }) }); await w.open();
    assert.equal(w.top(), N.TEXT.started); assert.match(N.TEXT.started, /started; this page cannot show a live call/);
    assert.doesNotMatch(w.text(), /A REAL VOICE|REAL SUMMARY|Simulated call run|No phone will ring|Transcript/); assert.equal(w.sent(/^POST/), 0);
  }
});
await t('the card: one click, one decision, even when the stage is opened again while the answer and the re-read are slow', async () => {
  const w = world({ ready: () => FIXTURE, db: seeded('PENDING'), slow: 25 }); await w.open();
  w.B('Approve').click(); w.B('Approve')?.click();
  for (let i = 0; i < 40; i++) { await w.sleep(10); w.h.open(); w.B('Approve')?.click(); }
  await w.settle(); assert.equal(w.sent(/decision$/), 1); N.unmount();
});
console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
