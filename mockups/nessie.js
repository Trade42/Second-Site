// "Move-in payments, simulated": one in-flow card for live mode (?live=1). It shows the backend's move-in
// payment plan for one listing and lets the owner walk one plan item at a time through the three steps
// the backend requires: ask for approval, decide it, run the simulation. Everything is mock money in a
// sandbox bank; the card says so in every state and never calls a result more than the backend does.
//
// Self-contained: no import. The caller gives the project id, the property id, a request function
// `req(method, path, body)` (Backend.req of backend.js: resolves with the parsed answer, rejects with an
// Error carrying `status` and `data`) and the element to put the card in.
//
// Rules this file keeps:
// - Every amount is integer cents from the backend, only formatted here (money()).
// - No string from the backend is ever written as HTML: every node is made with createElement and filled
//   with textContent, and every string is cut to a bounded length first (clip()).
// - Its buttons carry data-nessie and data-line, never data-act: the page's own click handler acts on
//   data-act ("ask" there drafts a message to the broker).
// - Every button is disabled while a request runs. Execute is sent at most once per approval id for the
//   life of the page (SENT), whatever the answer was; a page reload reads the true state again.
// - The state shown comes from the backend: the plan, the sandbox, its reading of the balance against the
//   plan, and the project's approval list. The backend has no read for a plan item's attempt
//   (reports/nessie-live-1.md, D2), so after a reload a line whose approval is EXECUTED is shown as
//   already run, without an outcome and without a button. The card never asks for an approval to probe.

const MAX = { line: 300, label: 90, basis: 260, word: 48, payments: 12, reserves: 6, skipped: 6, notes: 3 };
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const ITEM = /^[\x21-\x7e]{1,64}$/; // the backend's rule for plan_item_id

export const clip = (s, n = MAX.line) => { const t = String(s ?? '').replace(/\s+/g, ' ').trim(); return t.length > n ? t.slice(0, n - 1).trimEnd() + '…' : t; };
// A backend status token in its own words: SIMULATED_PENDING -> "simulated pending".
export const words = s => clip(typeof s === 'string' ? s : '', MAX.word).toLowerCase().replace(/_/g, ' ');
// Integer cents to "$1,234.50". Anything that is not a whole number of cents is not an amount: null.
export function money(cents) {
  if (!Number.isSafeInteger(cents)) return null;
  const a = Math.abs(cents), d = Math.floor(a / 100);
  return (cents < 0 ? '-$' : '$') + String(d).replace(/\B(?=(\d{3})+(?!\d))/g, ',') + '.' + String(a % 100).padStart(2, '0');
}
const amount = cents => money(cents) ?? 'amount not given';

export const TEXT = {
  title: 'Move-in payments, simulated',
  sim: {
    CAPITAL_ONE_NESSIE_SANDBOX: 'This is a simulation in Capital One’s Nessie sandbox with mock money. Nothing is paid.',
    FIXTURE_IN_MEMORY_BANK: 'This is a simulation in Second Site’s built-in test bank with mock money, not in Capital One’s Nessie sandbox. Nothing is paid.',
    unknown: 'This is a simulation with mock money; the backend has not said in which sandbox. Nothing is paid.',
  },
  noIds: 'No project or listing is selected yet, so there is no plan to show.',
  loading: 'Reading the plan from the backend…',
  noPlan: 'The backend gave no move-in plan for this listing.',
  sandboxChecking: 'Sandbox account: not read yet.',
  sandboxNone: 'Sandbox account: not set up for this project.',
  sandboxUnknown: 'Sandbox account: its state is not known.',
  setUp: 'Set up the sandbox account',
  setUpNote: 'Setting it up creates a mock account with mock money. It pays nobody.',
  pick: 'Choose one line to simulate.',
  ask: 'Ask for approval', askAgain: 'Ask for a new approval',
  approve: 'Approve', reject: 'Reject', yes: 'Yes', cancel: 'Cancel',
  run: 'Run the simulation',
  again: 'Read the result again', look: 'Look for it in the sandbox', reread: 'Read again from the backend',
  used: 'The approval was used for this one run.', // the approval's own word for that is EXECUTED, which says nothing about the payment
  decisionYours: 'Waiting for your decision. Nobody else decides it.',
  unknownState: 'Not known: the approvals of this project could not be read, so the step this line is at is not known.',
  stepsWait: 'The three steps (ask for approval, approve, run the simulation) open when the sandbox account is set up.',
  ran: 'The simulation was already run for this line. The backend does not give its outcome again after a reload, so the outcome is not shown here. It is not run twice.',
  onceOnly: 'A run was already sent with this approval from this page, and a run is never sent twice. Unless the backend’s answer on this card says so, it is not known whether it reached the sandbox. Reload the page to read the state again.',
  noAnswer: status => `The backend answered ${status} without a message.`,
  notReached: 'The backend could not be reached.',
  simShort: 'A simulation with mock money. Nothing is paid.', // repeated beside a result: the card is taller than its pane, so the first sentence can be scrolled out of sight
};
const ATTEMPT = {
  SENT: 'Sent to the sandbox.',
  IN_FLIGHT: 'Being sent to the sandbox now; there is no result yet.',
  MAY_EXIST: 'Not known: the simulated payment may or may not exist in the sandbox.',
  REFUSED: 'The sandbox refused the last run, so nothing exists.',
};
export const STYLE = `.nessie-card { display: flex; flex-direction: column; gap: var(--space-2, 8px); min-width: 0; margin: var(--space-5, 24px) 0 0; padding: var(--space-3, 12px) var(--space-4, 16px) var(--space-4, 16px); border-radius: 12px; background: var(--tint, #E7E2D6); font-size: 14px; line-height: 1.45; color: var(--ink2, #54584F); overflow-wrap: anywhere; }
.nessie-card p, .nessie-card ul, .nessie-card ol, .nessie-card h3, .nessie-card h4 { margin: 0; padding: 0; }
.nessie-card h3 { font-size: 15px; font-weight: 600; color: var(--ink, #22241F); }
.nessie-card h4 { font-size: 14px; font-weight: 600; color: var(--ink, #22241F); }
.nessie-card ul, .nessie-card ol { list-style: none; display: flex; flex-direction: column; gap: var(--space-2, 8px); }
.nessie-sim { color: var(--ink, #22241F); font-weight: 500; }
.nessie-small { font-size: 13px; color: var(--ink3, #5D6157); }
.nessie-line { display: flex; width: 100%; justify-content: space-between; align-items: baseline; gap: var(--space-3, 12px); padding: 6px 10px; border-radius: 8px; background: var(--surface, #F8F6F0); text-align: left; color: var(--ink, #22241F); }
.nessie-line[aria-pressed=true] { background: var(--accent-soft, #DFE7F6); color: var(--accent-ink, #35579B); font-weight: 500; }
.nessie-line span:last-child { flex: none; font-variant-numeric: tabular-nums; }
.nessie-basis { padding: 2px 10px 0; }
.nessie-row { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-2, 8px); }
.nessie-btn { padding: 6px 12px; border-radius: 8px; background: var(--surface, #F8F6F0); color: var(--ink, #22241F); font-size: 13.5px; font-weight: 500; }
.nessie-btn.solid { background: var(--accent, #426AC2); color: #fff; }
.nessie-card button:disabled { opacity: .5; cursor: default; }
.nessie-step { display: flex; flex-direction: column; gap: var(--space-1, 4px); }
.nessie-step b { font-weight: 600; color: var(--ink, #22241F); }
.nessie-result { padding: var(--space-2, 8px) 10px; border-radius: 8px; background: var(--surface, #F8F6F0); display: flex; flex-direction: column; gap: var(--space-1, 4px); }
.nessie-error { color: var(--ink, #22241F); font-weight: 500; }`;

// Approval ids an execute was sent for, for the life of the page (not per card: a card is rebuilt when
// the selected listing changes).
const SENT = new Set();
export function forgetSent() { SENT.clear(); } // tests only

// One plain line for a failed request: the backend's own message when it gave one.
export function say(e) {
  const m = e?.data && typeof e.data === 'object' && typeof e.data.message === 'string' ? e.data.message : '';
  if (m.trim()) return clip(m);
  if (Number.isInteger(e?.status) && e.status > 0) return TEXT.noAnswer(e.status);
  return clip(e?.message || TEXT.notReached);
}
const codeOf = e => (e?.data && typeof e.data === 'object' && typeof e.data.error_code === 'string' ? e.data.error_code : '');
const detailsOf = e => (e?.data && typeof e.data === 'object' && e.data.details && typeof e.data.details === 'object' ? e.data.details : {});

// The approvals of one plan item of one property, out of the project's approval list, newest first.
export function approvalsFor(list, propertyId, itemId) {
  return (Array.isArray(list) ? list : []).filter(a => a && typeof a === 'object' && a.action_payload && typeof a.action_payload === 'object' && a.action_payload.plan_item_id === itemId
    && String(a.action_payload.property_id).toLowerCase() === String(propertyId).toLowerCase() && UUID.test(String(a.approval_id)))
    .sort((a, b) => String(b.created_at).localeCompare(String(a.created_at)));
}
const expired = (a, now) => { const t = Date.parse(a?.expires_at); return Number.isFinite(t) && t < now; };
// Which step a plan item is at, and with which approval. The backend has no read for an item's attempt
// (nessie-live-1, D2): after a reload the only sign of a run is an EXECUTED approval in the list.
//   result: this page holds the backend's answer to a run (anything but a refusal ends the steps)
//   ran:    an approval of this item is EXECUTED; the outcome is not readable after a reload
//   decide / run / ask: by the newest approval that is still open
export function phaseOf(approvals, result = null, now = Date.now()) {
  const list = Array.isArray(approvals) ? approvals : [];
  if (result?.status && result.status !== 'REFUSED') return { phase: 'result', approval: list.find(a => a.approval_id === result.approval_id) || null };
  const done = list.find(a => a.approval_status === 'EXECUTED');
  if (done) return { phase: 'ran', approval: done };
  const a = list[0] || null, s = a && !expired(a, now) ? a.approval_status : null;
  return { phase: s === 'PENDING' ? 'decide' : s === 'APPROVED' ? 'run' : 'ask', approval: a };
}
// Own keys only: a backend value such as "constructor" or "__proto__" must not replace the sentence.
export function simSentence(simulation) { return typeof simulation === 'string' && Object.hasOwn(TEXT.sim, simulation) && typeof TEXT.sim[simulation] === 'string' ? TEXT.sim[simulation] : TEXT.sim.unknown; }

/**
 * Draws the card into `container` and reads its state from the backend.
 * Returns { card, ready, reload(), close() }; `ready` resolves when the first read has been drawn.
 */
export function nessieCard(projectId, propertyId, req, container, opts = {}) {
  const doc = opts.document || globalThis.document, now = opts.now || (() => Date.now());
  const P = `/api/v1/projects/${projectId}`, N = `${P}/properties/${propertyId}/nessie`;
  const call = async (method, path, body) => req(method, path, body); // a request function that throws at once still rejects
  const el = (tag, cls, text) => { const n = doc.createElement(tag); if (cls) n.className = cls; if (text != null) n.textContent = text; return n; };
  const st = {
    closed: false, busy: null, loaded: false, error: null,
    simulation: null, disclaimer: null, plan: null, planError: null,
    sandbox: null, sandboxState: 'checking', sandboxLine: null, headline: null,
    approvals: [], listed: false, listLine: null, selected: null, confirm: null,
    results: {}, // plan item id -> the backend's answer to a run made from this card: { status, record, line, approval_id }
  };

  if (!doc.getElementById?.('nessie-card-style')) {
    const s = el('style'); s.id = 'nessie-card-style'; s.textContent = STYLE; (doc.head || doc.documentElement)?.append?.(s);
  }
  const card = el('section', 'live-card nessie-card'); card.id = 'nessie-card'; card.setAttribute('aria-label', TEXT.title);
  const head = el('h3', null, TEXT.title), body = el('div', 'live-card-body');
  card.append(head, body); container.append(card);

  const button = (label, act, { solid = false, off = false } = {}) => {
    const b = el('button', 'nessie-btn' + (solid ? ' solid' : ''), label); b.type = 'button'; b.dataset.nessie = act;
    b.disabled = !!st.busy || off;
    return b;
  };
  const payments = () => (Array.isArray(st.plan?.payments) ? st.plan.payments.filter(p => p && typeof p === 'object' && ITEM.test(String(p.plan_item_id ?? ''))) : []);
  const item = () => payments().find(p => p.plan_item_id === st.selected) || null;
  const stateOf = id => phaseOf(approvalsFor(st.approvals, propertyId, id), st.results[id] || null, now());
  // The item this one comes after (the bill before its payment), while that one has not been run.
  const waitsFor = it => { const dep = it?.depends_on && payments().find(p => p.plan_item_id === it.depends_on); if (!dep) return null; const ph = stateOf(dep.plan_item_id).phase; return ph === 'ran' || (ph === 'result' && st.results[dep.plan_item_id].status === 'SENT') ? null : dep; };
  const label = it => (st.disclaimer = typeof it?.disclaimer === 'string' && it.disclaimer.trim() ? it.disclaimer : st.disclaimer, it);

  /* ---------------------------------------------------------------- reads */

  function takeSandbox(view) {
    st.sandbox = view && typeof view === 'object' ? view : null; st.sandboxLine = null;
    st.sandboxState = st.sandbox ? 'set' : 'unknown';
    if (st.sandbox?.simulation) st.simulation = st.simulation || st.sandbox.simulation;
    label(st.sandbox);
  }
  async function readSandbox() {
    try { takeSandbox(await call('GET', `${P}/nessie/sandbox`)); } catch (e) {
      st.sandbox = null; const code = codeOf(e);
      st.sandboxState = code === 'NESSIE_SANDBOX_NOT_SET_UP' ? 'none' : code === 'NESSIE_SANDBOX_GONE' ? 'gone' : 'unknown';
      st.sandboxLine = st.sandboxState === 'none' ? null : say(e);
    }
    await readAffordability();
  }
  // The backend's own sentence on whether the mock balance covers the plan (only with a sandbox and a plan).
  async function readAffordability() {
    st.headline = null;
    if (st.sandboxState !== 'set' || !st.plan) return;
    try { const v = await call('GET', `${N}/affordability`); st.headline = typeof v?.report?.headline === 'string' && v.report.headline.trim() ? clip(v.report.headline) : null; }
    catch (e) { st.headline = `The backend gave no reading of the mock balance against the plan: ${say(e)}`; }
  }
  async function readApprovals() {
    try { const list = await call('GET', `${P}/approvals`); st.approvals = Array.isArray(list) ? list : []; st.listed = Array.isArray(list); st.listLine = st.listed ? null : 'The backend’s approval list was not a list.'; }
    catch (e) { st.listed = false; st.listLine = say(e); }
    st.confirm = null;
  }
  async function readAll() {
    try {
      const v = await call('GET', `${N}/plan`);
      if (!v || typeof v !== 'object' || !v.plan || typeof v.plan !== 'object') throw Object.assign(new Error(TEXT.noPlan), { status: 0 });
      st.plan = v.plan; st.planError = null; st.simulation = v.simulation || v.plan.simulation || st.simulation; label(v);
    } catch (e) { st.plan = null; st.planError = say(e); }
    await readSandbox();
    await readApprovals();
    const list = [...payments()].sort((a, b) => (a.sequence ?? 0) - (b.sequence ?? 0));
    if (!list.some(p => p.plan_item_id === st.selected)) st.selected = list.length ? list[0].plan_item_id : null;
    st.loaded = true;
  }

  /* -------------------------------------------------------------- actions */

  // One request at a time: a click while another runs does nothing (and every button is disabled).
  async function act(name, fn) {
    if (st.busy || st.closed) return;
    st.busy = name; st.error = null; draw();
    try { await fn(); } catch (e) { st.error = say(e); } finally { st.busy = null; draw(); }
  }
  const taken = v => ({ status: typeof v?.attempt_status === 'string' ? v.attempt_status : 'UNKNOWN', record: v?.record && typeof v.record === 'object' ? v.record : null });
  const actions = {
    reload: () => act('reload', readAll),
    select: id => { if (st.busy || !payments().some(p => p.plan_item_id === id)) return; st.selected = id; st.confirm = null; st.error = null; draw(); },
    setup: () => act('setup', async () => {
      try { takeSandbox(await call('POST', `${P}/nessie/sandbox`)); await readAffordability(); } catch (e) { st.error = say(e); await readSandbox(); }
    }),
    ask: () => act('ask', async () => {
      const it = item(); if (!it || stateOf(it.plan_item_id).phase !== 'ask' || waitsFor(it)) return;
      try { label(await call('POST', `${N}/payments/${encodeURIComponent(it.plan_item_id)}/approval`)); await readApprovals(); }
      catch (e) { st.error = say(e); await readSandbox(); await readApprovals(); }
    }),
    decide: decision => {
      if (st.busy || (decision !== 'APPROVED' && decision !== 'REJECTED')) return;
      if (st.confirm !== decision) { st.confirm = decision; draw(); return; }
      return act('decide', async () => {
        const it = item(), s = it && stateOf(it.plan_item_id), a = s?.approval; st.confirm = null;
        if (!a || s.phase !== 'decide') return;
        try { await call('POST', `/api/v1/approvals/${a.approval_id}/decision`, { decision, action_payload_hash: String(a.action_payload_hash ?? '') }); }
        catch (e) { st.error = say(e); }
        await readApprovals(); // the decision as the backend holds it
      });
    },
    cancel: () => { if (st.busy) return; st.confirm = null; draw(); },
    run: () => {
      const it = item(), s = it && stateOf(it.plan_item_id), a = s?.approval;
      if (st.busy || !a || s.phase !== 'run' || waitsFor(it)) return;
      if (SENT.has(a.approval_id)) { draw(); return; } // sent by another card on this page: say so (the button goes off), send nothing
      SENT.add(a.approval_id); // before the request leaves: never a second one for this approval
      return act('run', async () => {
        const id = it.plan_item_id;
        try {
          const v = label(await call('POST', `${N}/payments/${encodeURIComponent(id)}/execute`, { approval_id: a.approval_id }));
          st.results[id] = { ...taken(v), approval_id: a.approval_id };
        } catch (e) {
          st.error = say(e);
          const code = codeOf(e), d = detailsOf(e);
          if (code === 'NESSIE_PAYMENT_MAY_EXIST') st.results[id] = { status: 'MAY_EXIST', record: null, approval_id: a.approval_id };
          else if (code === 'NESSIE_PAYMENT_REFUSED') st.results[id] = { status: 'REFUSED', record: null, approval_id: a.approval_id };
          else if (code === 'NESSIE_ALREADY_ATTEMPTED' && typeof d.attempt_status === 'string') st.results[id] = { status: d.attempt_status, record: null, approval_id: a.approval_id };
        }
        const line = st.error; await readSandbox(); await readApprovals(); st.error = line; // the mock balance and the approval as they are now
      });
    },
    // Reads this run's transaction again (the refresh route; no money moves). Only for a run made from this card.
    refresh: () => act('refresh', async () => {
      const it = item(), r = it && st.results[it.plan_item_id]; if (!r) return;
      try { st.results[it.plan_item_id] = { ...taken(label(await call('POST', `${N}/payments/${encodeURIComponent(it.plan_item_id)}/refresh`))), approval_id: r.approval_id }; }
      catch (e) { st.error = say(e); }
    }),
  };
  card.addEventListener('click', e => {
    let t = e.target; while (t && t !== card && !t.dataset?.nessie) t = t.parentNode;
    const a = t?.dataset?.nessie; if (!a || t.disabled) return;
    if (a === 'select') actions.select(t.dataset.line);
    else if (a === 'approve') actions.decide('APPROVED');
    else if (a === 'reject') actions.decide('REJECTED');
    else if (a === 'confirm') actions.decide(st.confirm);
    else if (Object.hasOwn(actions, a)) actions[a]();
  });

  /* ----------------------------------------------------------------- draw */

  function sandboxNodes() {
    const out = [];
    if (st.sandboxState === 'set') {
      const acct = st.sandbox.account?.record, bal = money(acct?.balance_cents), avail = money(acct?.available_cents);
      out.push(el('p', null, `Sandbox account: set up (the backend’s word: “${words(st.sandbox.sandbox_status) || 'not given'}”). ${bal ? `Mock balance ${bal}${avail && avail !== bal ? `, ${avail} available` : ''}.` : 'The backend gave no balance.'}`));
      if (st.headline) out.push(el('p', 'nessie-small', st.headline));
    } else if (st.sandboxState === 'none' || st.sandboxState === 'gone') {
      out.push(el('p', null, st.sandboxState === 'none' ? TEXT.sandboxNone : `Sandbox account: gone. ${st.sandboxLine}`));
      const row = el('div', 'nessie-row'); row.append(button(TEXT.setUp, 'setup', { solid: true }), el('span', 'nessie-small', TEXT.setUpNote)); out.push(row);
    } else if (st.sandboxState === 'unknown') out.push(el('p', null, `${TEXT.sandboxUnknown} ${st.sandboxLine || ''}`.trim()));
    else out.push(el('p', null, TEXT.sandboxChecking));
    return out;
  }
  function planNodes() {
    const out = [], plan = st.plan, list = [...payments()].sort((a, b) => (a.sequence ?? 0) - (b.sequence ?? 0));
    const ul = el('ul', 'nessie-plan');
    for (const p of list.slice(0, MAX.payments)) {
      const li = el('li'), b = el('button', 'nessie-line'); b.type = 'button'; b.dataset.nessie = 'select'; b.dataset.line = p.plan_item_id;
      b.disabled = !!st.busy; b.setAttribute('aria-pressed', String(p.plan_item_id === st.selected));
      b.append(el('span', null, clip(p.label, MAX.label) || 'Line without a name'), el('span', null, amount(p.amount_expected_minor)));
      li.append(b);
      if (p.basis) li.append(el('p', 'nessie-small nessie-basis', clip(p.basis, MAX.basis)));
      ul.append(li);
    }
    out.push(ul);
    if (list.length > MAX.payments) out.push(el('p', 'nessie-small', `${list.length - MAX.payments} more lines are not shown.`));
    if (!list.length) out.push(el('p', null, 'The plan has no line to simulate.'));
    const cash = money(plan.move_in_cash_expected_minor), high = money(plan.move_in_cash_high_minor);
    if (cash) out.push(el('p', null, `Cash to have at move-in, by the plan: ${cash} expected${high && high !== cash ? `, up to ${high}` : ''}.`));
    const reserves = Array.isArray(plan.reserves) ? plan.reserves.filter(r => r && typeof r === 'object') : [];
    for (const r of reserves.slice(0, MAX.reserves)) out.push(el('p', 'nessie-small', `Kept back, not paid to anyone: ${clip(r.label, MAX.label)}, ${amount(r.amount_expected_minor)}.${r.note ? ' ' + clip(r.note, MAX.basis) : ''}`));
    const skipped = Array.isArray(plan.not_simulated) ? plan.not_simulated.filter(r => r && typeof r === 'object') : [];
    for (const r of skipped.slice(0, MAX.skipped)) out.push(el('p', 'nessie-small', `Not simulated: ${words(r.category) || 'a cost line'}, ${amount(r.expected_minor)}.${r.reason ? ' ' + clip(r.reason, MAX.basis) : ''}`));
    if (reserves.length > MAX.reserves || skipped.length > MAX.skipped) out.push(el('p', 'nessie-small', 'More reserve or not-simulated lines exist than are shown.'));
    return out;
  }
  function resultNodes(r) {
    const rec = r.record, box = el('div', 'nessie-result');
    box.append(el('p', 'nessie-small', TEXT.simShort));
    box.append(el('p', null, `${ATTEMPT[r.status] || 'Not known: the backend reports a status this page does not know.'} The backend’s word for this run: “${words(r.status)}”.`));
    if (rec) {
      box.append(el('p', null, `What the sandbox reports: “${words(rec.outcome) || 'unknown'}”${rec.provider_status ? ` (its own status: “${clip(rec.provider_status, MAX.word)}”)` : ''}. Amount: ${amount(rec.amount_minor)}${rec.moves_money === false ? '; no money moves in this step' : ''}.`));
      if (rec.note) box.append(el('p', 'nessie-small', clip(rec.note)));
    }
    return box;
  }
  function stepNodes() {
    const it = item(), out = [];
    if (!it) return [el('p', null, TEXT.pick)];
    out.push(el('h4', null, `${clip(it.label, MAX.label) || 'This line'}: ${amount(it.amount_expected_minor)}${it.moves_money === false ? ' (no money moves in this step)' : ''}`));
    for (const n of (Array.isArray(it.notes) ? it.notes : []).slice(0, MAX.notes)) out.push(el('p', 'nessie-small', clip(n, MAX.basis)));
    if (st.sandboxState !== 'set') { out.push(el('p', 'nessie-small', TEXT.stepsWait)); return out; }
    if (!st.listed) { out.push(el('p', null, `${TEXT.unknownState} ${st.listLine || ''}`.trim())); return out; }
    const { phase, approval: a } = stateOf(it.plan_item_id), r = st.results[it.plan_item_id] || null, status = words(a?.approval_status);
    if (phase === 'ran') { out.push(el('p', null, TEXT.ran)); return out; }
    const dep = phase === 'result' ? null : waitsFor(it);
    if (dep) { out.push(el('p', null, `This line comes after “${clip(dep.label, MAX.label) || 'another line'}”: run that one first.`)); return out; }
    const ol = el('ol', 'nessie-steps');
    const step = (n, title, ...kids) => { const li = el('li', 'nessie-step'), h = el('p'); h.append(el('b', null, `${n}. ${title}`)); li.append(h, ...kids.filter(Boolean)); ol.append(li); };
    const refused = r?.status === 'REFUSED' ? el('p', 'nessie-small', ATTEMPT.REFUSED) : null;
    // 1
    if (phase === 'ask') {
      const row = el('div', 'nessie-row'); row.append(button(a ? TEXT.askAgain : TEXT.ask, 'ask', { solid: true }));
      step(1, 'Ask for approval', a ? el('p', 'nessie-small', expired(a, now()) && (a.approval_status === 'PENDING' || a.approval_status === 'APPROVED') ? 'The last approval has run out of time, so it cannot be used.' : `The last approval is “${status}”, so it cannot be used.`) : null, refused, row);
    } else step(1, 'Ask for approval', el('p', 'nessie-small', `Asked. ${clip(a?.summary) || 'The backend gave no summary.'}`));
    // 2
    if (phase === 'decide') {
      const row = el('div', 'nessie-row');
      if (st.confirm) {
        row.append(el('span', null, st.confirm === 'APPROVED' ? `Approve this simulated payment of ${money(a.action_payload?.amount_minor) ?? amount(it.amount_expected_minor)}? Nothing real is paid.` : 'Reject this simulated payment?'),
          button(TEXT.yes, 'confirm', { solid: true }), button(TEXT.cancel, 'cancel'));
      } else row.append(button(TEXT.approve, 'approve', { solid: true }), button(TEXT.reject, 'reject'));
      step(2, 'Approve', el('p', 'nessie-small', `The backend’s word: “${status}”. ${TEXT.decisionYours}`), row);
    } else step(2, 'Approve', el('p', 'nessie-small', phase === 'ask' ? 'Not asked for yet.' : a?.approval_status === 'EXECUTED' ? TEXT.used : `The backend’s word for the approval: “${status || 'not given'}”.`));
    // 3
    if (phase === 'run') {
      const used = SENT.has(a.approval_id), row = el('div', 'nessie-row'); row.append(button(TEXT.run, 'run', { solid: true, off: used }));
      step(3, 'Run the simulation', refused, used ? el('p', 'nessie-small', TEXT.onceOnly) : null, row);
    } else if (phase === 'result') {
      const row = el('div', 'nessie-row');
      if (r.status === 'MAY_EXIST') row.append(button(TEXT.look, 'refresh'));
      else if (r.status === 'SENT' && r.record?.nessie_transaction_id) row.append(button(TEXT.again, 'refresh')); // never for the recurring bill: it has no transaction
      step(3, 'Run the simulation', resultNodes(r), row.children.length ? row : null);
    } else step(3, 'Run the simulation', el('p', 'nessie-small', 'Opens when the approval is approved.'));
    out.push(ol);
    return out;
  }
  function draw() {
    if (st.closed) return;
    const nodes = [el('p', 'nessie-sim', simSentence(st.simulation))];
    if (st.disclaimer) nodes.push(el('p', 'nessie-small nessie-disclaimer', clip(st.disclaimer)));
    if (!UUID.test(String(projectId)) || !UUID.test(String(propertyId))) nodes.push(el('p', null, TEXT.noIds));
    else if (!st.loaded) nodes.push(el('p', null, TEXT.loading));
    else {
      nodes.push(...sandboxNodes());
      if (st.plan) nodes.push(...planNodes(), ...stepNodes());
      else nodes.push(el('p', null, st.planError || TEXT.noPlan));
      if (st.error) { const p = el('p', 'nessie-error', st.error); p.setAttribute('role', 'status'); nodes.push(p); }
      if (!st.plan || st.error || !st.listed || st.sandboxState === 'unknown') { const row = el('div', 'nessie-row'); row.append(button(TEXT.reread, 'reload')); nodes.push(row); }
    }
    const focused = doc.activeElement, fa = focused && card.contains?.(focused) ? { act: focused.dataset?.nessie, item: focused.dataset?.line } : null;
    body.replaceChildren(...nodes);
    if (fa?.act) [...(body.querySelectorAll?.('[data-nessie]') || [])].find(b => b.dataset.nessie === fa.act && b.dataset.line === fa.item && !b.disabled)?.focus?.({ preventScroll: true });
  }

  draw();
  const valid = UUID.test(String(projectId)) && UUID.test(String(propertyId));
  const ready = valid ? act('load', readAll) : Promise.resolve();
  return {
    card, ready,
    reload: () => (valid ? actions.reload() : undefined),
    close() { st.closed = true; card.remove?.(); },
  };
}
