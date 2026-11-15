// Tests for nessie.js. No packages: `node mockups/nessie.test.mjs`.
// A stub request function plays the backend's Nessie routes (the shapes of
// apps/backend/crates/server/src/services/nessie.rs) and a small DOM stand-in holds the card. No network.
import assert from 'node:assert/strict';
import * as N from './nessie.js';

let pass = 0, fail = 0;
async function t(name, fn) {
  try { await fn(); pass++; console.log('ok   ', name); } catch (e) { fail++; console.log('FAIL ', name, '\n     ', String(e.stack || e.message).split('\n').slice(0, 8).join('\n      ')); }
}

/* ------------------------------------------------------------ DOM stand-in */
const made = []; // every tag name the module creates
class El {
  constructor(tag) { this.tagName = tag.toUpperCase(); this.children = []; this.parentNode = null; this.dataset = {}; this.attrs = {}; this.listeners = {}; this.className = ''; this.id = ''; this.disabled = false; this._text = ''; }
  get textContent() { return this.children.length ? this.children.map(c => c.textContent).join('') : this._text; }
  set textContent(v) { for (const c of this.children) c.parentNode = null; this.children = []; this._text = String(v); }
  set innerHTML(_) { throw new Error('innerHTML must never be written'); }
  append(...kids) { for (const k of kids) { assert.ok(k instanceof El, 'only elements are appended'); k.remove(); k.parentNode = this; this.children.push(k); } }
  replaceChildren(...kids) { for (const c of this.children) c.parentNode = null; this.children = []; this._text = ''; this.append(...kids); this.onDraw?.(); }
  remove() { const p = this.parentNode; if (p) { p.children = p.children.filter(c => c !== this); this.parentNode = null; } }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  addEventListener(type, fn) { (this.listeners[type] ||= []).push(fn); }
  contains(n) { for (let x = n; x; x = x.parentNode) if (x === this) return true; return false; }
  all() { return this.children.flatMap(c => [c, ...c.all()]); }
  querySelectorAll(sel) { assert.equal(sel, '[data-nessie]'); return this.all().filter(n => n.dataset.nessie); }
  focus() {}
  click() { for (let x = this; x; x = x.parentNode) for (const fn of x.listeners.click || []) fn({ target: this }); }
}
const newDoc = () => { const head = new El('head'); return { head, activeElement: null, createElement: tag => { made.push(tag); return new El(tag); }, getElementById: id => head.all().find(n => n.id === id) || null }; };
const esc = s => String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])); // backend.js html()
const serial = n => (n.children.length ? `<${n.tagName.toLowerCase()}>${n.children.map(serial).join('')}</${n.tagName.toLowerCase()}>` : `<${n.tagName.toLowerCase()}>${esc(n.textContent)}</${n.tagName.toLowerCase()}>`);
const tick = () => new Promise(r => setTimeout(r, 0));

/* ------------------------------------------------------------ stub backend */
const PID = '11111111-1111-4111-8111-111111111111', PROP = '22222222-2222-4222-8222-222222222222', APPR = n => `aaaaaaaa-aaaa-4aaa-8aaa-${String(n).padStart(12, '0')}`;
const SIM = 'CAPITAL_ONE_NESSIE_SANDBOX', DISC = 'Simulation in the Capital One Nessie sandbox: mock money only. This is not a real bank balance, payment, paid deposit or signed commitment.';
const problem = (status, error_code, message, details = {}) => Object.assign(new Error(`answered ${status} ${error_code}`), { status, data: { error_code, message, details, request_id: null, retryable: false } });
const BILL = 'movein:recurring_occupancy_bill', DEP = 'movein:security_deposit', FIRST = 'movein:first_month_occupancy';
const PLAN = () => ({
  simulation: SIM, disclaimer: DISC, currency: 'USD',
  payments: [
    { plan_item_id: DEP, sequence: 1, label: 'Security deposit', operation: 'purchase', moves_money: true, amount_expected_minor: 450000, depends_on: null, basis: 'cost scenario line items (expected planning estimate; low and high shown for context)', notes: ['The landlord is a stand-in sandbox merchant.'] },
    { plan_item_id: BILL, sequence: 2, label: 'Monthly rent bill', operation: 'create_recurring_bill', moves_money: false, amount_expected_minor: 380050, depends_on: null, basis: 'Sum of the scenario’s monthly lines.', notes: [] },
    { plan_item_id: FIRST, sequence: 3, label: 'First month’s rent', operation: 'pay_bill', moves_money: true, amount_expected_minor: 380050, depends_on: BILL, basis: 'The first payment of the bill.', notes: [] },
  ],
  reserves: [{ reserve_id: 'r1', label: 'Working capital', amount_expected_minor: 100000, note: 'Kept in the account.' }],
  not_simulated: [{ line_item_id: 'x', category: 'PERMIT_FEES', expected_minor: 25000, reason: 'A city fee; it is not simulated.' }],
  move_in_cash_expected_minor: 550000, move_in_cash_high_minor: 700000,
});
const HEADLINE = 'The sandbox account holds $20,000.00 of mock money, $1,000.00 short of the plan.';
function stub(over = {}) {
  const s = { calls: [], sandbox: false, balance: 2000000, plan: PLAN(), approvals: [], attempts: {}, fail: {}, n: 0, hold: null, outcome: 'SIMULATED_PENDING', provider: 'pending', ...over };
  const view = created => ({ simulation: SIM, disclaimer: DISC, project_id: PID, sandbox_status: 'READY', created, opening_balance_minor: 2000000, account: { simulation: SIM, disclaimer: DISC, record: { id: 'acc', balance_cents: s.balance, available_cents: s.balance } }, notes: [] });
  const record = (item, a) => (item === BILL
    ? { simulation: SIM, disclaimer: DISC, approval_id: a.approval_id, plan_item_id: item, operation: 'create_recurring_bill', moves_money: false, amount_minor: a.action_payload.amount_minor, outcome: 'SIMULATED_BILL_RECORDED', provider_status: 'recurring', nessie_transaction_id: null, nessie_bill_id: 'bill1', note: 'A recurring bill was recorded in the sandbox. No money moved; nothing was committed.' }
    : { simulation: SIM, disclaimer: DISC, approval_id: a.approval_id, plan_item_id: item, operation: 'purchase', moves_money: true, amount_minor: a.action_payload.amount_minor, outcome: s.outcome, provider_status: s.provider, nessie_transaction_id: 'txn1', nessie_bill_id: null, note: 'The sandbox reports this simulated purchase as pending. Mock money only.' });
  s.req = async (method, path, body) => {
    s.calls.push(`${method} ${path.replace(`/api/v1/projects/${PID}`, '').replace(`/properties/${PROP}`, '') || path}`);
    const key = Object.keys(s.fail).find(k => `${method} ${path}`.includes(k));
    if (s.hold) await s.hold;
    if (key) { const f = s.fail[key]; if (f.once) delete s.fail[key]; throw f.error; }
    const m = path.match(/\/nessie\/payments\/([^/]+)\/(approval|execute|refresh)$/), item = m && decodeURIComponent(m[1]);
    const required = () => problem(409, 'NESSIE_SANDBOX_REQUIRED', 'no Nessie sandbox is set up for this project; POST /nessie/sandbox first');
    if (path.endsWith('/nessie/plan')) { if (!s.plan) throw problem(404, 'COST_SCENARIO_REQUIRED', 'this property has no live cost scenario yet; analyze it first'); return { simulation: SIM, disclaimer: DISC, inputs: {}, plan: s.plan }; }
    if (path.endsWith('/nessie/affordability')) { if (!s.sandbox) throw required(); return { simulation: SIM, disclaimer: DISC, report: { verdict: 'SANDBOX_BALANCE_SHORT', headline: HEADLINE, caveats: [] } }; }
    if (path.endsWith('/nessie/sandbox')) {
      if (method === 'GET') { if (!s.sandbox) throw problem(404, 'NESSIE_SANDBOX_NOT_SET_UP', 'no Nessie sandbox is set up for this project'); return view(false); }
      s.sandbox = true; return view(true);
    }
    if (path.endsWith('/approvals')) return s.approvals.map(a => ({ ...a }));
    if (path.includes('/decision')) { const a = s.approvals.find(x => path.includes(x.approval_id)); assert.equal(body.action_payload_hash, a.action_payload_hash); assert.equal(a.approval_status, 'PENDING'); a.approval_status = body.decision; return { ...a }; }
    if (m && m[2] === 'approval') {
      if (!s.sandbox) throw required();
      if (s.attempts[item]) throw problem(409, 'NESSIE_ALREADY_ATTEMPTED', `plan item ${item} already has a simulated payment attempt`, { attempt_status: s.attempts[item].status });
      if (item === FIRST && s.attempts[BILL]?.status !== 'SENT') throw problem(409, 'NESSIE_BILL_REQUIRED', `${FIRST} pays the recurring bill of ${BILL}: execute that item first`);
      let a = s.approvals.find(x => x.action_payload.plan_item_id === item && ['PENDING', 'APPROVED'].includes(x.approval_status)); const created = !a;
      if (!a) { const p = s.plan.payments.find(x => x.plan_item_id === item); a = { approval_id: APPR(++s.n), project_id: PID, action_payload: { plan_item_id: item, property_id: PROP, amount_minor: p.amount_expected_minor, simulation: SIM, disclaimer: DISC }, action_payload_hash: 'hash' + s.n, approval_status: 'PENDING', summary: `Simulation only: ${p.label} in the sandbox (mock money).`, expires_at: '2999-01-01T00:00:00Z', created_at: `2026-11-15T09:00:${String(s.n).padStart(2, '0')}Z` }; s.approvals.push(a); }
      return { simulation: SIM, disclaimer: DISC, created, property_id: PROP, plan_item_id: item, attempt_id: 'att1', approval: { ...a }, next_step: 'Decide it.' };
    }
    if (m && m[2] === 'execute') {
      const a = s.approvals.find(x => x.approval_id === body.approval_id);
      if (!a) throw problem(404, 'NOT_FOUND', 'approval not found');
      if (s.attempts[item]) throw problem(409, 'NESSIE_ALREADY_ATTEMPTED', `plan item ${item} already has a simulated payment attempt (SENT); only one per plan item is made`, { attempt_status: 'SENT', simulation: SIM, disclaimer: DISC });
      if (a.approval_status !== 'APPROVED') throw problem(409, 'APPROVAL_REQUIRED', `approval is ${a.approval_status}`);
      a.approval_status = 'EXECUTED'; if (item !== BILL) s.balance -= a.action_payload.amount_minor; s.attempts[item] = { status: 'SENT', record: record(item, a) };
      return { simulation: SIM, disclaimer: DISC, attempt_status: 'SENT', approval_id: a.approval_id, approval_status: 'EXECUTED', record: s.attempts[item].record };
    }
    if (m && m[2] === 'refresh') {
      const at = s.attempts[item];
      if (!at) throw problem(404, 'NESSIE_NO_ATTEMPT', `plan item ${item} has no recorded simulated payment`);
      if (!at.record?.nessie_transaction_id) throw problem(409, 'NESSIE_NOTHING_TO_REFRESH', `plan item ${item} has no transaction to refresh`, { attempt_status: at.status });
      at.record = { ...at.record, outcome: s.outcome, provider_status: s.provider };
      return { simulation: SIM, disclaimer: DISC, attempt_status: at.status, record: at.record, transaction: {} };
    }
    throw problem(404, 'NOT_FOUND', 'no such route');
  };
  return s;
}
// A card on a fresh document. Every draw is checked: the simulation sentence is there, exactly once.
function open(s, { project = PID, property = PROP } = {}) {
  const doc = newDoc(), box = new El('div'), draws = [];
  const h = N.nessieCard(project, property, s.req, box, { document: doc });
  const body = h.card.children[1];
  body.onDraw = () => { const text = h.card.textContent; draws.push(text); const sims = h.card.all().filter(n => n.className === 'nessie-sim');
    assert.equal(sims.length, 1, 'one simulation sentence per draw'); assert.match(sims[0].textContent, /simulation/); assert.match(sims[0].textContent, /mock money/); assert.match(sims[0].textContent, /Nothing is paid\.$/); };
  body.onDraw(); // the draw made before the hook was set
  const buttons = () => h.card.all().filter(n => n.tagName === 'BUTTON');
  const btn = label => buttons().find(b => b.textContent === label);
  const press = async label => { const b = btn(label); assert.ok(b, `button "${label}" is shown`); assert.equal(b.disabled, false, `button "${label}" is enabled`); b.click(); await tick(); await tick(); await h.settled(); };
  h.settled = async () => { for (let i = 0; i < 50 && buttons().some(b => b.disabled && b.textContent !== N.TEXT.run); i++) await tick(); };
  return { h, doc, box, draws, buttons, btn, press, text: () => h.card.textContent };
}
const usable = c => { const on = c.buttons().filter(b => !b.disabled); assert.ok(on.length > 0, 'the card still has an enabled button'); };

/* ------------------------------------------------------------------ tests */
const count = (s, tail) => s.calls.filter(x => x.endsWith(tail)).length;
const approve = async c => { await c.press(N.TEXT.ask); await c.press(N.TEXT.approve); await c.press(N.TEXT.yes); };
await t('money: integer cents only, formatted, never computed', () => {
  assert.equal(N.money(450000), '$4,500.00'); assert.equal(N.money(380050), '$3,800.50'); assert.equal(N.money(5), '$0.05'); assert.equal(N.money(0), '$0.00'); assert.equal(N.money(-120001), '-$1,200.01'); assert.equal(N.money(18000000), '$180,000.00');
  for (const bad of [1.5, '100', null, undefined, NaN, Infinity, 2 ** 60]) assert.equal(N.money(bad), null);
});
await t('phase: built from the approval list; an EXECUTED approval is "ran"; a run this page saw ends the steps', () => {
  const A = (status, extra = {}) => ({ approval_id: APPR(1), approval_status: status, expires_at: '2999-01-01T00:00:00Z', ...extra }), ph = (...x) => N.phaseOf(...x).phase;
  assert.equal(ph([]), 'ask'); assert.equal(ph([A('PENDING')]), 'decide'); assert.equal(ph([A('APPROVED')]), 'run');
  for (const dead of ['REJECTED', 'EXPIRED', 'CANCELLED', 'SOMETHING_NEW']) assert.equal(ph([A(dead)]), 'ask');
  assert.equal(ph([A('APPROVED', { expires_at: '2020-01-01T00:00:00Z' })]), 'ask', 'an approval past its time is not offered');
  assert.equal(ph([A('PENDING'), A('EXECUTED')]), 'ran', 'an EXECUTED approval anywhere in the list wins');
  for (const st of ['SENT', 'IN_FLIGHT', 'MAY_EXIST', 'A_NEW_WORD']) assert.equal(ph([A('APPROVED')], { status: st }), 'result');
  assert.equal(ph([A('APPROVED')], { status: 'REFUSED' }), 'run');
  const list = [{ approval_id: APPR(2), action_payload: { plan_item_id: 'a', property_id: PROP }, created_at: '2' }, { approval_id: APPR(1), action_payload: { plan_item_id: 'a', property_id: PROP }, created_at: '1' }, { approval_id: APPR(3), action_payload: { plan_item_id: 'b', property_id: PROP } }, { approval_id: APPR(4), action_payload: { plan_item_id: 'a', property_id: PID } }, null, { approval_id: 'x', action_payload: { plan_item_id: 'a', property_id: PROP } }];
  assert.deepEqual(N.approvalsFor(list, PROP, 'a').map(a => a.approval_id), [APPR(2), APPR(1)], 'this item, this property, real ids, newest first');
});
await t('the whole happy path: set up, ask, approve, run, result; the calls in order', async () => {
  N.forgetSent(); const s = stub(), c = open(s); await c.h.ready;
  assert.match(c.text(), /^Move-in payments, simulated/);
  assert.match(c.text(), /This is a simulation in Capital One’s Nessie sandbox with mock money\. Nothing is paid\./);
  assert.ok(c.text().includes(DISC), 'the backend’s own disclaimer is printed');
  assert.match(c.text(), /Sandbox account: not set up for this project\./);
  assert.match(c.text(), /Security deposit\$4,500\.00cost scenario line items \(expected planning estimate; low and high shown for context\)/); // label, amount, the backend's basis
  assert.match(c.text(), /Monthly rent bill\$3,800\.50Sum of the scenario’s monthly lines\./);
  assert.match(c.text(), /Cash to have at move-in, by the plan: \$5,500\.00 expected, up to \$7,000\.00\./);
  assert.match(c.text(), /Kept back, not paid to anyone: Working capital, \$1,000\.00\. Kept in the account\./);
  assert.match(c.text(), /Not simulated: permit fees, \$250\.00\. A city fee; it is not simulated\./);
  assert.equal(c.btn(N.TEXT.ask), undefined, 'no approval step before the sandbox exists');
  await c.press(N.TEXT.setUp);
  assert.match(c.text(), /Sandbox account: set up \(the backend’s word: “ready”\)\. Mock balance \$20,000\.00\./);
  assert.ok(c.text().includes(HEADLINE), 'the affordability headline as the backend words it');
  await c.press(N.TEXT.ask);
  assert.match(c.text(), /Asked\. Simulation only: Security deposit in the sandbox \(mock money\)\./);
  assert.match(c.text(), /The backend’s word: “pending”\. Waiting for your decision\./);
  assert.equal(c.btn(N.TEXT.run), undefined, 'no run while the approval is pending');
  await c.press(N.TEXT.approve); // first press only asks
  assert.match(c.text(), /Approve this simulated payment of \$4,500\.00\? Nothing real is paid\./);
  assert.equal(s.calls.filter(x => x.includes('/decision')).length, 0, 'no decision before the confirm');
  await c.press(N.TEXT.yes);
  assert.match(c.text(), /The backend’s word for the approval: “approved”\./);
  await c.press(N.TEXT.run);
  assert.match(c.text(), /Sent to the sandbox\. The backend’s word for this run: “sent”\./);
  assert.match(c.text(), /What the sandbox reports: “simulated pending” \(its own status: “pending”\)\. Amount: \$4,500\.00\./);
  assert.match(c.text(), /The sandbox reports this simulated purchase as pending\. Mock money only\./);
  assert.doesNotMatch(c.text(), /completed|executed|success/i, 'an accepted, pending run is not shown as completed, and "executed" is never printed');
  assert.doesNotMatch(c.text().replace(DISC, '').replace(/Nothing (real )?is paid|not paid to anyone/g, ''), /paid/i, 'the only "paid" on the card is "nothing is paid" and the backend’s disclaimer');
  assert.ok(c.text().includes(N.TEXT.used));
  assert.match(c.text(), /Mock balance \$15,500\.00\./, 'the balance is read again after the run');
  assert.equal(c.btn(N.TEXT.run), undefined); assert.equal(c.btn(N.TEXT.ask), undefined);
  for (const n of c.h.card.all()) for (const k of ['act', 'go', 'edit', 'open', 'tab', 'view', 'place', 'remove', 'room', 'key']) assert.equal(n.dataset[k], undefined, `no data-${k}: the page’s own click handler acts on it`);
  const item = 'POST /nessie/payments/movein%3Asecurity_deposit';
  assert.deepEqual(s.calls, [
    'GET /nessie/plan', 'GET /nessie/sandbox', 'GET /approvals',
    'POST /nessie/sandbox', 'GET /nessie/affordability',
    `${item}/approval`, 'GET /approvals',
    `POST /api/v1/approvals/${APPR(1)}/decision`, 'GET /approvals',
    `${item}/execute`, 'GET /nessie/sandbox', 'GET /nessie/affordability', 'GET /approvals',
  ]);
  s.outcome = 'SIMULATED_COMPLETED'; s.provider = 'completed';
  await c.press(N.TEXT.again);
  assert.match(c.text(), /What the sandbox reports: “simulated completed” \(its own status: “completed”\)/, 'completed only when the backend says so');
  assert.equal(count(s, '/execute'), 1);
});
await t('the recurring bill: recorded, not paid; no refresh is offered; the line that depends on it opens after it', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  await c.press('First month’s rent$3,800.50');
  assert.match(c.text(), /This line comes after “Monthly rent bill”: run that one first\./); assert.equal(c.btn(N.TEXT.ask), undefined);
  await c.press('Monthly rent bill$3,800.50');
  assert.match(c.text(), /Monthly rent bill: \$3,800\.50 \(no money moves in this step\)/);
  await approve(c); await c.press(N.TEXT.run);
  assert.match(c.text(), /What the sandbox reports: “simulated bill recorded” \(its own status: “recurring”\)\. Amount: \$3,800\.50; no money moves in this step\./);
  assert.match(c.text(), /No money moved; nothing was committed\./);
  assert.equal(c.btn(N.TEXT.again), undefined, 'no refresh on the recurring bill'); assert.equal(c.btn(N.TEXT.look), undefined);
  assert.match(c.text(), /Mock balance \$20,000\.00\./);
  await c.press('First month’s rent$3,800.50'); assert.ok(c.btn(N.TEXT.ask), 'the dependent line opens');
  assert.equal(count(s, '/refresh'), 0); assert.equal(count(s, `${encodeURIComponent(FIRST)}/approval`), 0, 'no approval was asked for to probe');
});
await t('a reload redraws the true state from the backend, not from memory', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  await approve(c);
  N.forgetSent(); const d = open(s); await d.h.ready; // a new page: approved, not run
  assert.ok(d.btn(N.TEXT.run) && !d.btn(N.TEXT.run).disabled); assert.equal(d.btn(N.TEXT.ask), undefined);
  await d.press(N.TEXT.run);
  N.forgetSent(); const before = s.calls.length, e = open(s); await e.h.ready; // a new page: already run
  assert.ok(e.text().includes(N.TEXT.ran)); assert.match(N.TEXT.ran, /already run for this line.*outcome is not shown/);
  assert.doesNotMatch(e.text(), /Sent to the sandbox|simulated pending|completed|executed/i, 'no outcome is invented after a reload');
  assert.deepEqual(e.buttons().filter(b => b.dataset.nessie !== 'select').map(b => b.textContent), [], 'no button for a line that was run');
  assert.deepEqual(s.calls.slice(before), ['GET /nessie/plan', 'GET /nessie/sandbox', 'GET /nessie/affordability', 'GET /approvals'], 'a load only reads');
  assert.equal(count(s, '/execute'), 1);
  await e.press('Monthly rent bill$3,800.50'); // another line has its own state
  assert.ok(e.btn(N.TEXT.ask)); assert.ok(!e.text().includes(N.TEXT.ran));
});
await t('double click: one request, every button disabled while it runs', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  let release; s.hold = new Promise(r => { release = r; });
  const ask = c.btn(N.TEXT.ask); ask.click(); ask.click(); c.btn(N.TEXT.ask)?.click();
  assert.ok(c.buttons().length > 0 && c.buttons().every(b => b.disabled), 'every button is disabled while the request runs');
  for (const b of c.buttons()) b.click();
  release(); s.hold = null; await tick(); await c.h.settled();
  assert.equal(count(s, '/approval'), 1);
  await c.press(N.TEXT.approve); await c.press(N.TEXT.yes);
  s.hold = new Promise(r => { release = r; });
  const run = c.btn(N.TEXT.run); run.click(); run.click(); c.btn(N.TEXT.run).click();
  assert.ok(c.buttons().every(b => b.disabled));
  for (const b of c.buttons()) b.click();
  release(); s.hold = null; await tick(); await c.h.settled();
  assert.equal(count(s, '/execute'), 1, 'execute left once'); assert.equal(count(s, '/decision'), 1);
  assert.equal(c.btn(N.TEXT.run), undefined, 'after it succeeded the run button is gone');
});
await t('failure: no cost scenario yet', async () => {
  N.forgetSent(); const s = stub({ plan: null }), c = open(s); await c.h.ready;
  assert.match(c.text(), /this property has no live cost scenario yet; analyze it first/);
  usable(c); s.plan = PLAN(); await c.press(N.TEXT.reread);
  assert.match(c.text(), /Security deposit\$4,500\.00/); assert.doesNotMatch(c.text(), /no live cost scenario/);
});
await t('failure: the sandbox is not set up when the approval is asked for', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  s.sandbox = false; // gone since the card read it
  await c.press(N.TEXT.ask);
  assert.match(c.text(), /no Nessie sandbox is set up for this project; POST \/nessie\/sandbox first/);
  assert.match(c.text(), /Sandbox account: not set up for this project\./, 'the sandbox state is read again');
  usable(c); await c.press(N.TEXT.setUp); await c.press(N.TEXT.ask); assert.ok(c.btn(N.TEXT.approve));
});
await t('failure: the sandbox set-up stops part way (502)', async () => {
  N.forgetSent(); const s = stub(), c = open(s); await c.h.ready;
  s.fail['POST /api/v1/projects/' + PID + '/nessie/sandbox'] = { once: true, error: problem(502, 'NESSIE_SANDBOX_INCOMPLETE', 'the sandbox set-up stopped part way (the bank did not answer); what was created is recorded and unused. Mock money only.') };
  await c.press(N.TEXT.setUp);
  assert.match(c.text(), /the sandbox set-up stopped part way/); assert.match(c.text(), /Sandbox account: not set up/);
  usable(c); await c.press(N.TEXT.setUp); assert.match(c.text(), /Sandbox account: set up/);
});
await t('failure: the approval is missing or used; execute is not sent again for it', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  await approve(c);
  s.approvals[0].approval_status = 'EXPIRED'; // behind the card's back
  await c.press(N.TEXT.run);
  assert.match(c.text(), /approval is EXPIRED/);
  assert.match(c.text(), /The last approval is “expired”, so it cannot be used\./, 'the state was read again from the backend');
  usable(c); await c.press(N.TEXT.askAgain); await c.press(N.TEXT.approve); await c.press(N.TEXT.yes); await c.press(N.TEXT.run);
  assert.match(c.text(), /Sent to the sandbox\./);
  const sent = s.calls.filter(x => x.endsWith('/execute')); assert.equal(sent.length, 2, 'one per approval');
});
await t('failure: the bank is unreachable during the run (502 may exist): unknown stays unknown, no retry', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  await approve(c);
  s.fail['/execute'] = { once: true, error: problem(502, 'NESSIE_PAYMENT_MAY_EXIST', 'The simulated payment may have been made in the Capital One Nessie sandbox (mock money): the bank did not answer. The approval is spent.', { attempt_status: 'MAY_EXIST' }) };
  s.attempts[DEP] = { status: 'MAY_EXIST', record: null }; s.approvals[0].approval_status = 'EXECUTED'; // what the backend does on this answer
  await c.press(N.TEXT.run);
  assert.match(c.text(), /Not known: the simulated payment may or may not exist in the sandbox\. The backend’s word for this run: “may exist”\./);
  assert.match(c.text(), /The simulated payment may have been made in the Capital One Nessie sandbox \(mock money\): the bank did not answer\./);
  assert.doesNotMatch(c.text(), /Sent to the sandbox|completed/);
  assert.equal(c.btn(N.TEXT.run), undefined); assert.equal(c.btn(N.TEXT.ask), undefined);
  usable(c); await c.press(N.TEXT.look);
  assert.match(c.text(), /may or may not exist/); assert.match(c.text(), /has no transaction to refresh/);
  assert.equal(count(s, '/execute'), 1);
  N.forgetSent(); const d = open(s); await d.h.ready; // after a reload: run, outcome not shown, no button
  assert.ok(d.text().includes(N.TEXT.ran)); assert.equal(d.btn(N.TEXT.run), undefined); assert.equal(d.btn(N.TEXT.look), undefined);
});
await t('failure: the bank refuses the run (422): shown as refused, and the same approval is not sent twice', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  await approve(c);
  s.fail['/execute'] = { once: true, error: problem(422, 'NESSIE_PAYMENT_REFUSED', 'the bank refused the simulated payment (insufficient-funds); nothing was created and the approval is not spent') };
  await c.press(N.TEXT.run);
  assert.match(c.text(), /the bank refused the simulated payment/); assert.match(c.text(), /The sandbox refused the last run, so nothing exists\./);
  const run = c.btn(N.TEXT.run); assert.ok(run && run.disabled, 'the run button stays, disabled');
  assert.ok(c.text().includes(N.TEXT.onceOnly));
  run.click(); await tick();
  assert.equal(count(s, '/execute'), 1);
  usable(c);
});
await t('failure: a 5xx with no body, and a backend that cannot be reached', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  s.fail['/approval'] = { once: true, error: Object.assign(new Error('POST x answered 500'), { status: 500, data: '<html>oops</html>' }) };
  await c.press(N.TEXT.ask);
  assert.match(c.text(), /The backend answered 500 without a message\./); assert.doesNotMatch(c.text(), /oops/);
  usable(c); await approve(c);
  const down = Object.assign(new Error('The backend at http://localhost:8000 cannot be reached (fetch failed).'), { status: 0 });
  s.fail['/execute'] = { error: down }; s.fail['/approvals'] = { error: down }; s.fail['/nessie/sandbox'] = { error: down };
  await c.press(N.TEXT.run);
  assert.match(c.text(), /The backend at http:\/\/localhost:8000 cannot be reached \(fetch failed\)\./);
  assert.match(c.text(), /Sandbox account: its state is not known\./);
  assert.doesNotMatch(c.text(), /Sent to the sandbox|completed/);
  assert.equal(c.btn(N.TEXT.run), undefined, 'no run is offered while the state is unknown');
  s.fail = {}; usable(c); await c.press(N.TEXT.reread);
  const run = c.btn(N.TEXT.run); assert.ok(run && run.disabled, 'approved and not run by the backend, but this page already sent one run');
  assert.match(c.text(), /it is not known whether it reached the sandbox/);
  run.click(); await tick(); assert.equal(count(s, '/execute'), 1);
  s.fail['/approvals'] = { error: down }; await c.h.reload();
  assert.match(c.text(), /Not known: the approvals of this project could not be read, so the step this line is at is not known\./);
  assert.equal(c.btn(N.TEXT.ask), undefined); usable(c);
});
await t('rejecting is a decision too, and a new approval can be asked for', async () => {
  N.forgetSent(); const s = stub({ sandbox: true }), c = open(s); await c.h.ready;
  await c.press(N.TEXT.ask); await c.press(N.TEXT.reject);
  assert.match(c.text(), /Reject this simulated payment\?/); await c.press(N.TEXT.cancel);
  assert.equal(s.approvals[0].approval_status, 'PENDING'); await c.press(N.TEXT.reject); await c.press(N.TEXT.yes);
  assert.equal(s.approvals[0].approval_status, 'REJECTED'); assert.match(c.text(), /The last approval is “rejected”/);
  assert.equal(c.btn(N.TEXT.run), undefined); assert.ok(c.btn(N.TEXT.askAgain));
});
await t('hostile and oversized text from the backend is inert and bounded', async () => {
  N.forgetSent(); made.length = 0;
  const evil = '<img src=x onerror="alert(1)"><script>alert(2)</script>&amp;', long = 'L'.repeat(5000);
  const plan = PLAN(); Object.assign(plan.payments[0], { label: evil, basis: evil + long, notes: [evil, long, long, long, long] });
  plan.payments.push({ plan_item_id: 'bad id with spaces', label: 'never shown', amount_expected_minor: 1 }, { plan_item_id: 'x'.repeat(65), label: 'never shown', amount_expected_minor: 1 }, null, 'text');
  plan.payments[1].amount_expected_minor = '3800.50'; plan.reserves[0].label = evil; plan.not_simulated[0].reason = long; plan.not_simulated[0].category = evil; plan.disclaimer = evil;
  const s = stub({ sandbox: true, plan }), c = open(s); await c.h.ready;
  s.fail['/approval'] = { once: true, error: problem(409, 'X', evil + long) };
  await c.press(N.TEXT.ask);
  const out = serial(c.h.card);
  assert.doesNotMatch(out, /<img|<script|onerror="/, 'no markup from the backend reaches the page');
  assert.match(out, /&lt;img src=x onerror=&quot;alert\(1\)&quot;&gt;/, 'it is shown as text');
  assert.deepEqual([...new Set(made)].sort(), ['b', 'button', 'div', 'h3', 'h4', 'input', 'li', 'ol', 'p', 'section', 'span', 'style', 'ul'], 'only these elements are ever created'); // web-link-13: 'input' is the set-up's one input, made with every card (its <label> only where the set-up is drawn)
  for (const n of c.h.card.all()) if (!n.children.length) assert.ok(n.textContent.length <= 700, `a text node of ${n.textContent.length} characters`);
  assert.doesNotMatch(c.text(), /never shown/, 'a plan item id the backend would refuse is not offered');
  assert.match(c.text(), /Monthly rent billamount not given/, 'an amount that is not integer cents is not an amount');
  assert.ok(c.text().length < 7000, `the card holds ${c.text().length} characters`);
  s.approvals.push({ approval_id: '"><img>', action_payload: { plan_item_id: DEP, property_id: PROP }, approval_status: 'APPROVED', created_at: '2027' });
  await c.press(N.TEXT.reread); assert.equal(c.btn(N.TEXT.run), undefined, 'an approval whose id is not an id is ignored');
  assert.ok(!s.calls.some(x => /<|>|"/.test(x)), 'no hostile text in a request path');
});
await t('another bank or no answer: the sentence stays true; no ids, no requests', async () => {
  assert.match(N.simSentence('FIXTURE_IN_MEMORY_BANK'), /built-in test bank with mock money, not in Capital One’s Nessie sandbox\. Nothing is paid\.$/);
  assert.match(N.simSentence('SOMETHING_ELSE'), /simulation with mock money; the backend has not said in which sandbox\. Nothing is paid\.$/);
  const s = stub(), c = open(s, { project: '', property: 'not-an-id' }); await c.h.ready; await tick();
  assert.equal(s.calls.length, 0); assert.match(c.text(), /No project or listing is selected yet/);
  assert.equal(c.h.reload(), undefined); c.h.close(); assert.equal(c.box.children.length, 0);
});
// Added by the review (b27bbb96).
await t('review: a hostile "simulation" value cannot replace the sentence; a second card for the same line sends nothing and says so', async () => {
  for (const k of ['constructor', '__proto__', 'toString', 'hasOwnProperty', 7, null, {}]) assert.equal(N.simSentence(k), N.TEXT.sim.unknown, String(k));
  N.forgetSent(); const s = stub({ sandbox: true }), base = s.req;
  s.req = async (...a) => { const v = await base(...a); return v && typeof v === 'object' && !Array.isArray(v) ? { ...v, simulation: 'constructor' } : v; };
  const c = open(s); await c.h.ready; // open() checks the sentence on every draw
  assert.ok(c.text().includes(N.TEXT.sim.unknown));
  await approve(c);
  const c2 = open(s); await c2.h.ready; // two cards for the same line, both at step 3
  await c.press(N.TEXT.run);
  assert.match(c.h.card.all().find(n => n.className === 'nessie-result').textContent, /^A simulation with mock money\. Nothing is paid\./, 'the result box says so itself');
  const run2 = c2.btn(N.TEXT.run); assert.ok(run2 && !run2.disabled, 'the second card has not read the state again'); run2.click(); await tick();
  assert.equal(count(s, '/execute'), 1);
  const after = c2.btn(N.TEXT.run); assert.ok(after && after.disabled, 'the second card turns its run button off'); assert.ok(c2.text().includes(N.TEXT.onceOnly));
  assert.doesNotMatch(N.TEXT.onceOnly, /above/, 'the backend’s line is drawn under the steps');
});
/* web-link-13: the set-up asks for a mock opening balance; a refusal's reason is shown beside the button */
const bodiesOf = s => { const got = [], inner = s.req; s.req = (m, p, b) => { if (m === 'POST' && p.endsWith('/nessie/sandbox')) got.push(b); return inner(m, p, b); }; return got; };
const inputOf = c => c.h.card.all().find(n => n.tagName === 'INPUT');
const NO_CAPITAL = () => problem(422, 'VALIDATION_ERROR', 'request validation failed', { errors: [{ type: 'missing', loc: ['body', 'opening_balance_minor'], msg: 'the business profile reports no opening capital; give opening_balance_minor' }] });
await t('web-link-13: the amount typed is integer cents or one plain line; the offer is the plan’s cash rounded up to $1,000', () => {
  assert.deepEqual(N.openingCents(''), { empty: true }); assert.deepEqual(N.openingCents('   '), { empty: true });
  assert.deepEqual(N.openingCents('158,000'), { cents: 15800000 }); assert.deepEqual(N.openingCents('$1,200.5'), { cents: 120050 });
  assert.deepEqual(N.openingCents('0'), { cents: 0 }); assert.deepEqual(N.openingCents('0.07'), { cents: 7 });
  assert.deepEqual(N.openingCents('1000000000'), { cents: N.MAX_OPENING_MINOR }, 'the backend’s maximum is taken');
  for (const bad of ['-5', 'abc', '1e5', '12.345', '1000000000.01', '1000000001', '99999999999', '1.2.3', '.', '５０', 'NaN', '0x10']) assert.deepEqual(N.openingCents(bad), { error: N.TEXT.openingBad }, bad);
  assert.equal(N.openingOffer({ move_in_cash_expected_minor: 15796475 }), '158,000'); assert.equal(N.openingOffer({ move_in_cash_expected_minor: 15800000 }), '158,000');
  assert.equal(N.openingOffer({ move_in_cash_expected_minor: 1 }), '1,000'); assert.equal(N.openingOffer({ move_in_cash_expected_minor: 0 }), '0');
  for (const none of [null, {}, { move_in_cash_expected_minor: '5' }, { move_in_cash_expected_minor: -1 }, { move_in_cash_expected_minor: 1.5 }, { move_in_cash_expected_minor: 2 ** 53 - 1 }]) assert.equal(N.openingOffer(none), '', JSON.stringify(none));
  assert.deepEqual(N.openingCents(N.openingOffer({ move_in_cash_expected_minor: 15796475 })), { cents: 15800000 }, 'the offer reads back as itself');
});
await t('web-link-13: a refusal’s first reason is in the same line, as bounded text', () => {
  assert.equal(N.say(NO_CAPITAL()), 'request validation failed: the business profile reports no opening capital; give opening_balance_minor');
  assert.equal(N.say(problem(422, 'VALIDATION_ERROR', 'request validation failed', { errors: [{ loc: ['body', 'amount'], msg: 'Input should be greater than or equal to 0' }, { loc: ['body', 'x'], msg: 'second' }] })), 'request validation failed: amount: Input should be greater than or equal to 0');
  assert.equal(N.say(problem(409, 'X', 'plain message.', { attempt_status: 'SENT' })), 'plain message.', 'details without reasons change nothing');
  for (const odd of [{ errors: 'x' }, { errors: [null, 7, { msg: 5 }, { msg: '  ' }] }, { errors: {} }]) assert.equal(N.say(problem(422, 'V', 'request validation failed', odd)), 'request validation failed');
  const long = N.say(problem(422, 'V', 'm'.repeat(400), { errors: [{ loc: ['body', '<img src=x onerror=1>'], msg: '<b>' + 'r'.repeat(5000) }] }));
  assert.ok(long.length <= 300, 'bounded'); assert.equal(N.reason(problem(500, 'V', 'm')), '');
});
await t('web-link-13: set-up sends the mock opening balance shown, in cents; the card says what the money is', async () => {
  N.forgetSent(); const s = stub(), bodies = bodiesOf(s), c = open(s); await c.h.ready;
  const inp = inputOf(c); assert.ok(inp, 'one input'); assert.equal(c.h.card.all().filter(n => n.tagName === 'INPUT').length, 1);
  assert.equal(inp.value, N.openingOffer(s.plan)); assert.ok(inp.value, 'the stub’s plan has an expected cash figure');
  assert.ok(c.text().includes(N.TEXT.openingLabel)); assert.ok(c.text().includes(N.TEXT.openingNote)); assert.ok(c.text().includes(N.TEXT.openingOffered));
  assert.match(N.TEXT.openingNote, /mock money chosen for the simulation, not your funds and not a bank balance/);
  await c.press(N.TEXT.setUp);
  assert.deepEqual(bodies, [{ opening_balance_minor: N.openingCents(N.openingOffer(s.plan)).cents }]); assert.ok(Number.isSafeInteger(bodies[0].opening_balance_minor));
  assert.match(c.text(), /Sandbox account: set up/); assert.equal(inputOf(c), undefined, 'no input once it is set up');
  // typed by the person: sent as typed, and a reload of the card does not overwrite it
  const s2 = stub(), b2 = bodiesOf(s2), c2 = open(s2); await c2.h.ready;
  const i2 = inputOf(c2); i2.value = '2,500.25'; for (const fn of i2.listeners.input || []) fn({});
  await c2.h.reload(); assert.equal(inputOf(c2).value, '2,500.25'); assert.ok(!c2.text().includes(N.TEXT.openingOffered));
  await c2.press(N.TEXT.setUp); assert.deepEqual(b2, [{ opening_balance_minor: 250025 }]);
});
await t('web-link-13: an empty input tries the bodiless set-up once; the refusal and its reason stand beside the button; then an amount is asked for', async () => {
  N.forgetSent(); const s = stub(), bodies = bodiesOf(s), c = open(s); await c.h.ready;
  s.fail['POST /api/v1/projects/' + PID + '/nessie/sandbox'] = { once: true, error: NO_CAPITAL() };
  const inp = inputOf(c); inp.value = ''; for (const fn of inp.listeners.input || []) fn({});
  await c.press(N.TEXT.setUp);
  assert.deepEqual(bodies, [undefined], 'no body when the input is empty');
  const all = c.h.card.all(), line = all.find(n => /nessie-setup-line/.test(n.className));
  assert.ok(line, 'the set-up has its own line'); assert.equal(line.getAttribute('role'), 'status');
  assert.equal(line.textContent, `request validation failed: the business profile reports no opening capital; give opening_balance_minor. ${N.TEXT.openingAsk}`);
  const at = n => all.indexOf(n), setUp = c.btn(N.TEXT.setUp), plan = all.find(n => n.className === 'nessie-plan');
  assert.ok(at(setUp) < at(line) && at(line) < at(plan), 'the line comes right after the button, before the plan’s lines');
  assert.equal(all.filter(n => /nessie-error/.test(n.className)).length, 1, 'said once, not again at the card’s bottom');
  assert.match(c.text(), /Sandbox account: not set up/); usable(c);
  // empty again: the backend has said it needs the amount, so nothing is sent
  const before = s.calls.length; c.btn(N.TEXT.setUp).click(); await tick();
  assert.equal(s.calls.length, before, 'no request'); assert.equal(c.h.card.all().find(n => /nessie-setup-line/.test(n.className)).textContent, N.TEXT.openingNeeded);
  inputOf(c).value = '158000'; await c.press(N.TEXT.setUp);
  assert.deepEqual(bodies, [undefined, { opening_balance_minor: 15800000 }]); assert.match(c.text(), /Sandbox account: set up/);
  assert.equal(c.h.card.all().find(n => /nessie-setup-line/.test(n.className)), undefined, 'the line goes when the set-up worked');
});
await t('web-link-13: an amount that is negative, not a number or above the maximum is one plain line and no request', async () => {
  N.forgetSent(); const s = stub(), bodies = bodiesOf(s), c = open(s); await c.h.ready;
  for (const bad of ['-100', 'ten thousand', '1000000000.01', '<img src=x>']) {
    inputOf(c).value = bad; const before = s.calls.length; c.btn(N.TEXT.setUp).click(); await tick();
    assert.equal(s.calls.length, before, bad); const lines = c.h.card.all().filter(n => /nessie-error/.test(n.className));
    assert.equal(lines.length, 1); assert.equal(lines[0].textContent, N.TEXT.openingBad); usable(c);
  }
  assert.deepEqual(bodies, []); assert.match(N.TEXT.openingBad, /Nothing was sent/);
  // a plan without the figure: the input starts empty and no offer is claimed
  const s3 = stub(); delete s3.plan.move_in_cash_expected_minor; const c3 = open(s3); await c3.h.ready;
  assert.equal(inputOf(c3).value, ''); assert.ok(!c3.text().includes(N.TEXT.openingOffered)); assert.ok(c3.text().includes(N.TEXT.openingNote));
});
await t('every draw of every test above carried the simulation sentence', () => { assert.ok(pass >= 15, 'the per-draw check in open() ran in the tests above'); });

await t('review 8d495183: Enter in the input sets up once; 0 and 1 are sent as cents; the backend’s out-of-range refusal is shown beside the button', async () => {
  N.forgetSent(); const s = stub(), bodies = bodiesOf(s), c = open(s); await c.h.ready;
  const inp = inputOf(c); inp.value = '1'; for (const fn of inp.listeners.input || []) fn({});
  for (let i = 0; i < 5; i++) for (const fn of inp.listeners.keydown || []) fn({ key: 'Enter', preventDefault() {} });
  c.btn(N.TEXT.setUp)?.click(); for (let i = 0; i < 20; i++) await tick();
  assert.deepEqual(bodies, [{ opening_balance_minor: 100 }], 'one request for five Enters and a click');
  const s0 = stub(), b0 = bodiesOf(s0), c0 = open(s0); await c0.h.ready; inputOf(c0).value = '0'; await c0.press(N.TEXT.setUp); assert.deepEqual(b0, [{ opening_balance_minor: 0 }]);
  const s2 = stub(), c2 = open(s2); await c2.h.ready;
  s2.fail['POST /api/v1/projects/' + PID + '/nessie/sandbox'] = { once: true, error: problem(422, 'VALIDATION_ERROR', 'request validation failed', { errors: [{ type: 'less_than_equal', loc: ['body', 'opening_balance_minor'], msg: 'the opening balance must be 0 to 100000000000 cents of mock money, got 100000000001' }] }) };
  inputOf(c2).value = '5'; await c2.press(N.TEXT.setUp);
  assert.equal(c2.h.card.all().find(n => /nessie-setup-line/.test(n.className)).textContent, 'request validation failed: opening_balance_minor: the opening balance must be 0 to 100000000000 cents of mock money, got 100000000001');
});

console.log(`\n${pass} passed, ${fail} failed`);
process.exit(fail ? 1 : 0);
