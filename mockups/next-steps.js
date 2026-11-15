// "Next steps": the page's last stage in live mode (?live=1), reached from Furnish. Three cards:
//   1. What you chose        the listing, its layout, the furnishing bill, the cost to open, what is open
//   2. Ask the listing's contact   an approved, SIMULATED call (outreach request -> the owner's own
//                                  decision -> the call -> the backend's simulate route)
//   3. Move-in payments, simulated a mount point for nessie.js (another file; nothing shown if absent)
// Loaded by backend.js bind() only; never without ?live=1. Contract with backend.js, mount(ctx):
//   ctx.page      workspace.js's tables and functions as given to bind() (P, D, C, S())
//   ctx.backend() the link's Backend (req(method, path, body), signIn(), project)
//   ctx.records() the last search's records as the backend answered them ({ rec })
//   ctx.lib       { billHead, geometryWords, unitName }
// Contract with nessie.js (card 3): nessieCard(projectId, propertyId, req, container), where req is
// Backend.req. If the file or the function is missing, or it throws, the mount point stays empty.
//
// Rules this file keeps:
// - No string from the backend is written as HTML: every node is createElement + textContent, every
//   string is cut to a bounded length first (clip).
// - A request is sent only by a person's click, once; a failure is one plain line and nothing is retried.
// - This page never rings a phone: the start request always carries `simulate: true`, which a backend
//   with the live voice provider refuses (409 SIMULATOR_DISABLED) before the approval is spent and before
//   any carrier request (routes/communications.rs start_call). The simulate route is called only for a
//   call whose own answer says `simulated: true`.
// - The owner's maximum rent lives in its own input only: never in a sentence, a summary or storage.
// - What is shown after a reload is read from the backend (communications, approvals, calls, handoffs).

const MAX = { line: 300, name: 120, question: 240, summary: 1500, turn: 400, turns: 60, findings: 30, issues: 12, questions: 20, list: 400 };
export const OPEN_KEY = 'second-site-workspace-live-next';

export const clip = (s, n = MAX.line) => { const t = String(s ?? '').replace(/\s+/g, ' ').trim(); return t.length > n ? t.slice(0, n - 1).trimEnd() + '…' : t; };
// A backend status token in its own words: AWAITING_APPROVAL -> "awaiting approval".
export const words = s => clip(typeof s === 'string' ? s : '', 48).toLowerCase().replace(/_/g, ' ');
const unhtml = s => String(s ?? '').replace(/&(amp|lt|gt|quot|#39);/g, (_, k) => ({ amp: '&', lt: '<', gt: '>', quot: '"', '#39': "'" }[k]));
const usd = n => '$' + Math.round(n).toLocaleString('en-US');

export const TEXT = {
  go: 'Next steps: review, ask, payments →',
  title: 'Next steps',
  back: '← Back to Furnish',
  chose: 'What you chose',
  ask: 'Ask the listing’s contact',
  sim: 'No phone will ring. This is a simulation of the call: the backend plays the contact’s side from a script, and nothing it “says” came from a real person.',
  script: 'The simulated call asks your ticked questions first (the first 20). The simulated contact has scripted answers only for the standard questions; any other stays open. The visit and the rent limits are kept with the approval, but the scripted call does not use them.',
  live: 'The backend is in live calling mode, where starting a call would dial the contact’s number. This page never rings a phone, so it offers no start button and asks for no new approval here.',
  liveRefused: 'The backend refused a simulated call because it is in live calling mode. This page never rings a phone: no call was started and the approval was not used.',
  unknownMode: 'This page could not read from the backend that its calls are simulated, so it asks for no approval and offers no start button here: this page never rings a phone.',
  started: 'The backend lists a call for this request and does not label it simulated: started; this page cannot show a live call, so it shows no transcript and starts nothing more.',
  notLabelled: 'The backend did not label this call as simulated, so this page stops here and does not run it.',
  noContact: 'This listing names no contact with a phone number, so there is nobody to ask from this page.',
  ownerText: 'Requesting approval also sends you a text asking you to review it, if your phone is linked. Nothing goes to the contact.',
  counters: 'With more than one counter-offer, the maximum can be heard by the other side: each counter moves toward it.',
  end: 'Nothing here has been signed, nothing has been sent to the listing’s contact, and no money has moved: a call started here is a simulation and the payments are mock money in a sandbox.',
  notConfirmed: 'Not yet confirmed',
  noneOpen: 'The backend lists no open issue for this listing. That is not a statement that everything is met.',
  oldBackend: 'This backend does not take a visit or rent limits yet (it needs a newer build). Clear them to ask the questions only.',
};

/* --------------------------------------------------------------- pure */

export function contactOf(rec) {
  const c = rec?.commercial_listing?.broker_contact, s = k => (typeof c?.[k] === 'string' ? clip(c[k], MAX.name) : '');
  const phone = s('phone'), name = s('name');
  return { name, phone, company: s('company'), role: s('role') || 'broker', ok: !!name && /^\+?[0-9][0-9 ().-]{5,22}$/.test(phone) };
}
// The issues the backend does not call met, each with its status in the backend's words and its first
// verification question. Nothing is restated as met.
export function openIssues(d) {
  return (d?.issues || []).filter(i => i && i.status !== 'MEETS').slice(0, MAX.issues).map(i => ({
    id: String(i.id), text: clip(unhtml(i.title), MAX.name), status: words(i.status) || 'status not given',
    question: typeof i.next === 'string' && i.next.trim() ? clip(i.next, MAX.question) : null,
  }));
}
// Card 1, as lines of plain text. `lib` is backend.js's own wording, so the figures and labels are the
// ones the compare and furnish stages show.
export function chosenFacts(p, d, S, C, lib = {}) {
  if (!p?.live || !d) return null;
  const sc = d.scenario, rooms = (d.rooms || []).length, bom = d.bom;
  const bill = bom && lib.billHead ? lib.billHead(S, C, bom) : null;
  const g = d.geometry || {};
  const floor = d.sketch?.label ? clip(d.sketch.label, 600)
    : lib.geometryWords ? `Floor geometry: ${clip(lib.geometryWords({ geometry_source: g.source, data_origin: g.data_origin, scale_verified: g.scale_verified }), 200)}.` : 'Floor geometry: not stated.';
  const n = v => Number.isFinite(v);
  return {
    name: clip(lib.unitName?.(p) ?? p.address, 200),
    origin: clip(p.live.source_label || 'Source not stated', MAX.name),
    rent: p.rentKnown ? `${usd(p.rent)} a month, asking` : 'Rent not stated',
    area: p.sqftKnown ? `${p.sqft.toLocaleString('en-US')} SF` : 'Area not stated',
    layout: rooms ? `A proposed layout of ${rooms} ${rooms === 1 ? 'room' : 'rooms'}, not an approved plan.` : 'No layout.',
    sketch: !!d.sketch, floor,
    bill: bill ? `${usd(bill.amount)}. ${clip(bill.line, 500)}` : 'The backend gave no bill of materials.',
    open: sc && n(sc.expected) ? `About ${usd(sc.expected)} expected${n(sc.low) && n(sc.high) ? ` (low ${usd(sc.low)}, high ${usd(sc.high)})` : ''}: the backend’s cost scenario, a planning estimate, not a quote.${d.extras_unknown ? ' Lease extras (NNN/CAM) are not stated and are not in it.' : ''}` : 'The backend gave no cost scenario for this listing.',
    issues: openIssues(d),
  };
}
// "2026-11-18T15:00" (a datetime-local value, this browser's time) -> ISO 8601 with its UTC offset.
export function withOffset(local, offsetMin = null) {
  if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2})?$/.test(String(local))) return null;
  const d = new Date(local); if (Number.isNaN(d.getTime())) return null;
  const off = offsetMin ?? -d.getTimezoneOffset(), a = Math.abs(off), pad = v => String(v).padStart(2, '0');
  return `${local.length === 16 ? local + ':00' : local}${off < 0 ? '-' : '+'}${pad(Math.floor(a / 60))}:${pad(a % 60)}`;
}
const dollars = v => { const t = String(v ?? '').replace(/[$,\s]/g, ''); return /^\d{1,7}$/.test(t) && Number(t) > 0 ? Number(t) * 100 : null; };
// The outreach request, or one plain line saying what is wrong. `form`: { questions[], free, visit:
// [{start, end}], target, max, counters }.
export function outreachBody(propertyId, contact, form = {}, offsetMin = null) {
  if (!contact?.ok) return { error: TEXT.noContact };
  const questions = [...(form.questions || []), form.free].map(q => clip(q, MAX.question)).filter(Boolean).slice(0, MAX.questions);
  const windows = [];
  for (const w of form.visit || []) {
    if (!w?.start && !w?.end) continue;
    const start = withOffset(w.start, offsetMin), end = withOffset(w.end, offsetMin);
    if (!start || !end) return { error: 'A visit window needs both a start and an end.' };
    if (new Date(end) <= new Date(start)) return { error: 'A visit window must end after it starts.' };
    windows.push({ start, end });
  }
  const anyRent = String(form.target ?? '').trim() || String(form.max ?? '').trim();
  let negotiation = null;
  if (anyRent) {
    const target = dollars(form.target), max = dollars(form.max), counters = Number(form.counters ?? 1);
    if (target == null || max == null) return { error: 'Rent limits need both a target and a maximum, in whole dollars a month.' };
    if (target > max) return { error: 'The target rent cannot be above the maximum.' };
    if (!Number.isInteger(counters) || counters < 0 || counters > 5) return { error: 'Counter-offers: a whole number from 0 to 5.' };
    negotiation = { target_monthly_rent_cents: target, max_monthly_rent_cents: max, max_counters: counters };
  }
  if (!questions.length && !windows.length && !negotiation) return { error: 'Tick at least one question, or add a visit or rent limits.' };
  const body = {
    property_id: propertyId, channel: 'VOICE',
    recipient: { role: contact.role, name: contact.name, phone: contact.phone },
    purpose: ['Questions about the listing', windows.length ? 'a visit' : null, negotiation ? 'the rent' : null].filter(Boolean).join(', '),
    questions,
  };
  // `purposes` exists only on a backend from commit c50c34b: it is sent only when the person filled it in.
  if (windows.length || negotiation) body.purposes = { ...(windows.length ? { tour: { windows, duration_minutes: 30 } } : {}), ...(negotiation ? { negotiation } : {}) };
  return { body };
}
// Where the request for this listing stands, from the backend's four lists.
export function stateOf(lists, propertyId, now = Date.now()) {
  const arr = v => (Array.isArray(v) ? v : []);
  const mine = arr(lists?.comms).filter(c => c && c.property_id === propertyId && c.channel === 'VOICE' && c.approval_id)
    .sort((a, b) => String(a.created_at).localeCompare(String(b.created_at)));
  const calls = arr(lists?.calls).filter(k => k && k.property_id === propertyId).sort((a, b) => String(a.started_at ?? '').localeCompare(String(b.started_at ?? '')));
  // the newest request that has a call, else the newest request
  const comm = mine.at(-1) || null;
  if (!comm) return { phase: 'none', comm: null, approval: null, call: null, handoffs: [] };
  const approval = arr(lists?.approvals).find(a => a?.approval_id === comm.approval_id) || null;
  // A repeated request is a new communication on the SAME approval (the backend keeps one approval per
  // request, services/approvals.rs): the call made on that approval is this request's call.
  const call = calls.filter(k => k.communication_id === comm.communication_id || (k.approval_id && k.approval_id === comm.approval_id)).at(-1) || null;
  const handoffs = call ? arr(lists?.handoffs).filter(h => h?.call_id === call.call_id) : [];
  const status = approval?.approval_status, late = approval?.expires_at && Date.parse(approval.expires_at) <= now;
  const phase = call ? 'call' : !approval ? 'unknown' : status === 'PENDING' ? (late ? 'expired' : 'pending') : status === 'APPROVED' ? (late ? 'expired' : 'approved')
    : status === 'REJECTED' ? 'refused' : status === 'EXPIRED' ? 'expired' : status === 'EXECUTED' ? 'used' : 'other';
  return { phase, comm, approval, call, handoffs };
}
// The backend keeps one approval per request: for questions alone that is one per listing and contact,
// whatever its status; a request with a visit or rent limits is keyed by them too (routes/communications.rs
// outreach, the approval's idempotency key). So "ask again" is promised only as the backend allows it.
const AGAIN = 'For questions alone the backend keeps this one approval for this listing and contact; a request with a visit or rent limits (or different ones) is a new request.';
export const PHASE_LINE = {
  refused: `You refused this request. Nothing was started. ${AGAIN}`,
  expired: `The approval ran out before it was used. Nothing was started. ${AGAIN}`,
  used: `The backend says this approval was already used, and it lists no call for it. ${AGAIN}`,
  other: `The backend gives this approval a status this page does not act on. ${AGAIN}`,
  unknown: 'The backend lists the request but not its approval.',
};
export const sameApproval = status => `The backend answered with the earlier approval for this listing and contact (${words(status) || 'status not given'}), not a new one.`;
const TERMINAL = new Set(['COMPLETED', 'FAILED', 'DECLINED', 'CANCELLED']);
export const callDone = call => TERMINAL.has(call?.state);
// 'live' | 'simulated' | 'unknown', from /ready's provider list (the Telnyx line's mode), when readable.
export function callsMode(ready) {
  const t = (Array.isArray(ready?.providers) ? ready.providers : []).find(x => x?.provider === 'telnyx');
  const m = typeof t?.mode === 'string' ? t.mode.toLowerCase() : null;
  return m === 'live' ? 'live' : m === 'fixture' ? 'simulated' : 'unknown';
}
// One plain line for a failed request.
export function say(e, what = 'The request') {
  const st = e?.status, d = e?.data;
  if (!st) return `${what} got no answer from the backend (the connection failed), so this page does not know whether it arrived. Nothing was repeated; what is shown is read again from the backend.`;
  const code = typeof d?.error_code === 'string' ? d.error_code : typeof d?.code === 'string' ? d.code : null;
  if (code === 'SIMULATOR_DISABLED') return TEXT.liveRefused;
  const msg = typeof d?.message === 'string' ? d.message : typeof d?.detail === 'string' ? d.detail : Array.isArray(d?.detail) ? d.detail.map(x => x?.msg).filter(Boolean).join('; ') : '';
  if (st === 422 && Array.isArray(d?.detail) && d.detail.some(x => Array.isArray(x?.loc) && x.loc.includes('purposes') && x.type !== 'value_error')) return TEXT.oldBackend;
  return `${what} was not accepted: the backend answered ${st}${code ? ' ' + clip(code, 48) : ''}${msg ? ` (${clip(msg, 220)})` : ''}. Nothing was repeated.`;
}
// A call's parts as plain lines, each labelled by what the backend says of the call.
export function callParts(call, handoffs = []) {
  const sim = call?.simulated === true, tag = sim ? 'simulated' : 'not labelled simulated by the backend';
  const arr = v => (Array.isArray(v) ? v : []);
  return {
    sim, tag,
    head: `Call, ${tag}: ${words(call?.state) || 'state not given'}${call?.failure_reason ? ` (${clip(call.failure_reason, 160)})` : ''}`,
    transcript: arr(call?.transcript).slice(0, MAX.turns).map(t => `${clip(t?.speaker, 24) || 'speaker not given'}: ${clip(t?.text, MAX.turn)}`),
    more: Math.max(0, arr(call?.transcript).length - MAX.turns),
    findings: arr(call?.findings).slice(0, MAX.findings).map(f => `${clip(f?.question, MAX.question)}: ${f?.answer == null ? 'no answer' : clip(f.answer, MAX.question)} (${words(f?.classification) || 'class not given'})`),
    deferred: arr(call?.deferred_questions).slice(0, MAX.findings).map(q => clip(q, MAX.question)),
    holds: arr(handoffs).slice(0, 6).map(h => `${words(h?.reason) || 'reason not given'}: ${clip(h?.question, MAX.question)} (${words(h?.status) || 'status not given'})`),
    summary: typeof call?.outcome_summary === 'string' && call.outcome_summary.trim() ? String(call.outcome_summary).trim().slice(0, MAX.summary) : null,
  };
}

/* ---------------------------------------------------------------- page */

export const STYLE = `#next-steps { height: 100%; min-height: 0; overflow: auto; padding: 0 var(--pad, 16px) var(--pad, 16px); }
#next-steps[hidden], #ns-go[hidden] { display: none; }
#next-steps .ns-col { max-width: 820px; margin: 0 auto; display: flex; flex-direction: column; gap: var(--space-4, 16px); min-width: 0; }
#next-steps .ns-top { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
#next-steps h2 { margin: 0; }
.ns-card { display: flex; flex-direction: column; gap: var(--space-2, 8px); min-width: 0; padding: var(--space-3, 12px) var(--space-4, 16px) var(--space-4, 16px); border-radius: 12px; background: var(--tint, #E7E2D6); font-size: 14px; line-height: 1.45; color: var(--ink2, #54584F); overflow-wrap: anywhere; }
.ns-card h3 { margin: 0; font-size: 16px; font-weight: 600; color: var(--ink, #1c1c1a); }
.ns-card h4 { margin: 6px 0 0; font-size: 13px; font-weight: 600; color: var(--ink, #1c1c1a); }
.ns-card p, .ns-card ul, .ns-card ol, .ns-card dl { margin: 0; }
.ns-card ul, .ns-card ol { padding-left: 18px; }
.ns-card dl { display: grid; grid-template-columns: minmax(90px, 150px) minmax(0, 1fr); gap: 4px 12px; }
.ns-card dt { color: var(--ink3, #7a7d74); }
.ns-card dd { margin: 0; min-width: 0; }
.ns-card label { display: flex; gap: 8px; align-items: flex-start; min-width: 0; }
.ns-card input[type=text], .ns-card input[type=datetime-local], .ns-card input[type=number], .ns-card select { min-width: 0; max-width: 100%; padding: 6px 8px; border: 0; border-radius: 8px; background: var(--surface, #F8F6F0); font: inherit; color: var(--ink, #1c1c1a); }
.ns-card .ns-row { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; min-width: 0; }
.ns-card .ns-grow { flex: 1 1 220px; }
.ns-card .ns-note { font-size: 13px; color: var(--ink3, #7a7d74); }
.ns-card .ns-sim { padding: 8px 10px; border-radius: 8px; background: var(--surface, #F8F6F0); color: var(--ink, #1c1c1a); }
.ns-card .ns-line { color: var(--ink, #1c1c1a); font-weight: 500; }
.ns-card .ns-pre { white-space: pre-wrap; }
.ns-card button.ns-btn { padding: 8px 15px; border-radius: 9px; background: var(--surface, #F8F6F0); font-size: 14px; font-weight: 500; }
.ns-card button.ns-btn.solid { background: var(--accent, #2f5d50); color: #fff; }
.ns-card button.ns-btn:disabled { opacity: .45; cursor: default; }
.ns-end { margin: 0; font-size: 14px; color: var(--ink2, #54584F); }
#ns-go { display: flex; justify-content: flex-end; }
body.ns-open #furnish { display: none; }
.stages .ns-nav { display: inline-flex; align-items: center; min-height: 44px; padding-inline: var(--space-1, 4px); color: var(--ink3, #7a7d74); cursor: pointer; }
.stages .ns-nav[hidden] { display: none; }
.stages .ns-nav:hover, body.ns-open .stages .ns-nav { color: var(--accent, #2f5d50); }
body.ns-open .stages .ns-nav { font-weight: 500; }
body.ns-open .stages button[aria-current] { color: var(--ink3, #7a7d74); font-weight: 400; }
@media (max-width: 480px) { .stages .ns-nav { display: none; } }
@media (max-width: 860px) { .ns-card dl { grid-template-columns: minmax(0, 1fr); } .ns-card dt { margin-top: 4px; } }`;

const kv = {
  get: () => { try { return globalThis.localStorage?.getItem(OPEN_KEY) ?? null; } catch { return null; } },
  set: v => { try { if (v) globalThis.localStorage?.setItem(OPEN_KEY, v); else globalThis.localStorage?.removeItem(OPEN_KEY); } catch { /* storage unavailable */ } },
};

// The card's request function (card 3): no request of the card leaves before sign-in has finished. After a
// reload the stage opens while sign-in is still under way, and a request sent then carries no token (401).
export const signedReq = be => async (...a) => { await be.signIn(); return be.req(...a); };

let mounted = null;
export function mount(ctx) {
  const doc = globalThis.document, furnish = doc?.getElementById?.('furnish'), main = furnish?.parentElement;
  if (!doc?.createElement || !furnish || !main || mounted || !ctx?.page || typeof ctx.backend !== 'function') return null;
  const el = (tag, cls, text) => { const n = doc.createElement(tag); if (cls) n.className = cls; if (text != null) n.textContent = text; return n; };
  const { page, lib = {} } = ctx;
  const chosen = () => { const S = page.S(), p = S && page.P[S.chosen], d = p && page.D[p.src]; return p?.live && d?.rooms?.length ? { S, p, d, id: p.live.property_id } : null; };

  if (!doc.getElementById('ns-style')) { const s = el('style'); s.id = 'ns-style'; s.textContent = STYLE; (doc.head || doc.documentElement).append(s); }
  const goBar = el('div'); goBar.id = 'ns-go';
  const goBtn = el('button', 'btn solid', TEXT.go); goBtn.type = 'button'; goBar.append(goBtn);
  (furnish.querySelector('.canvas') || furnish).append(goBar);
  // The same stage from the top bar. Not a <button>: the page disables every bar button that is not its own.
  const navBtn = el('span', 'ns-nav', TEXT.title); navBtn.setAttribute('role', 'button'); navBtn.tabIndex = 0; navBtn.hidden = true;
  doc.querySelector('.stages')?.append(navBtn);

  const stage = el('section'); stage.id = 'next-steps'; stage.hidden = true; stage.setAttribute('aria-label', TEXT.title);
  const col = el('div', 'ns-col'); stage.append(col); main.append(stage);

  const st = { open: false, id: null, busy: null, reading: 0, seq: 0, liveSeen: false, line: null, lists: null, readLine: null, mode: 'unknown', result: null, ownerAction: 'accept', form: null, nessie: null };

  function close() {
    if (!st.open) return;
    st.open = false; stage.hidden = true; doc.body.classList.remove('ns-open'); kv.set(null);
  }
  function open() {
    const c = chosen(); if (!c) return;
    if (page.S().stage !== 'furnish') { doc.querySelector('.stages [data-go=furnish]')?.click(); if (page.S().stage !== 'furnish') return; } // through the page's own Furnish
    if (st.id !== c.id) { st.id = c.id; st.lists = null; st.line = null; st.result = null; st.form = null; st.nessie = null; try { st.payments?.close?.(); } catch { /* another file's card */ } st.payments = null; }
    st.open = true; stage.hidden = false; doc.body.classList.add('ns-open'); kv.set(c.id);
    draw(); read();
  }
  goBtn.addEventListener('click', open); navBtn.addEventListener('click', open);
  navBtn.addEventListener('keydown', e => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); open(); } });
  // Any of the page's own stage buttons leaves this stage first; the page then shows its own.
  doc.addEventListener('click', e => { if (st.open && e.target?.closest?.('[data-go]')) close(); }, true);

  /* ---------- reads: the true state, from the backend */
  async function read() {
    const be = ctx.backend(), id = st.id, n = ++st.seq;
    if (!be?.project) { st.readLine = 'There is no project on the backend yet.'; return draw(); }
    st.reading++; draw(); // every button is off until the true state and the calling mode are read
    try {
      await be.signIn();
      const [comms, approvals, calls, handoffs] = await Promise.all(['/communications', '/approvals', '/calls', '/handoffs'].map(r => be.req('GET', be.p(r))));
      if (st.id === id && n === st.seq) { st.lists = { comms, approvals, calls, handoffs }; st.readLine = null; }
    } catch (e) { if (st.id === id && n === st.seq) st.readLine = say(e, 'Reading where this request stands'); }
    // /ready is the backend's own account of its providers, asked on every read; unreadable is "unknown".
    const m = await mode(); st.reading--;
    if (st.id === id) { st.mode = m; draw(); }
  }
  const mode = async () => { if (st.liveSeen) return 'live'; try { return callsMode(await ctx.backend().req('GET', '/ready')); } catch { return 'unknown'; } };
  const simulatedNow = async () => (st.mode = await mode()) === 'simulated'; // asked again at the click
  async function act(name, fn) {
    if (st.busy) return;
    st.busy = name; st.line = null; draw();
    try { await fn(); } catch (e) { st.line = say(e, { request: 'The approval request', approve: 'Your decision', refuse: 'Your decision', start: 'The simulated call', run: 'The simulated conversation' }[name] || 'The request'); if (e?.data?.error_code === 'SIMULATOR_DISABLED' || e?.data?.code === 'SIMULATOR_DISABLED') st.liveSeen = true; }
    st.busy = null;
    await read();
  }
  const P = () => ctx.backend().p('');
  const request = () => {
    if (st.busy) return;
    const rec = (ctx.records?.() || []).find(r => r?.rec?.property_id === st.id)?.rec;
    const out = outreachBody(st.id, contactOf(rec), readForm());
    if (out.error) { st.line = out.error; return draw(); } // nothing is sent
    return act('request', async () => {
      const got = await ctx.backend().req('POST', `${P()}/outreach`, out.body);
      if (got?.approval?.approval_status !== 'PENDING') st.line = sameApproval(got?.approval?.approval_status);
    });
  };
  const decide = decision => act(decision === 'APPROVED' ? 'approve' : 'refuse', async () => {
    if (decision === 'APPROVED' && !(await simulatedNow())) return; // nothing is sent; the card says why
    const a = stateOf(st.lists, st.id).approval;
    await ctx.backend().req('POST', `/api/v1/approvals/${a.approval_id}/decision`, { decision, action_payload_hash: a.action_payload_hash });
  });
  const runSim = async call => {
    if (call?.simulated !== true) { st.line = TEXT.notLabelled; return; }
    st.result = await ctx.backend().req('POST', `${P()}/calls/${call.call_id}/simulate`, { owner_action: st.ownerAction });
  };
  const start = () => act('start', async () => {
    const s = stateOf(st.lists, st.id);
    if (!(await simulatedNow())) return; // nothing is sent; the card says why
    const call = await ctx.backend().req('POST', `${P()}/calls`, { communication_id: s.comm.communication_id, simulate: true });
    st.busy = 'run'; await runSim(call);
  });
  const run = () => act('run', async () => runSim(stateOf(st.lists, st.id).call));

  /* ---------- the form (kept across redraws; the maximum is only ever in its own input) */
  function buildForm(facts, contact) {
    const f = { root: el('div'), checks: [], free: null, visit: [], target: null, max: null, counters: null };
    f.root.style.cssText = 'display:flex;flex-direction:column;gap:8px;min-width:0';
    f.root.append(el('h4', null, 'Questions to ask'));
    for (const i of facts.issues.filter(x => x.question)) {
      const l = el('label'), c = el('input'); c.type = 'checkbox'; c.checked = true; c.value = i.question;
      l.append(c, el('span', null, `${i.question} (${i.text})`)); f.checks.push(c); f.root.append(l);
    }
    if (!f.checks.length) f.root.append(el('p', 'ns-note', 'The backend gives no verification question for this listing; write your own below.'));
    f.free = el('input', 'ns-grow'); f.free.type = 'text'; f.free.maxLength = MAX.question; f.free.placeholder = 'One more question, in your words'; f.free.setAttribute('aria-label', 'One more question');
    const fr = el('div', 'ns-row'); fr.append(f.free); f.root.append(fr);
    f.root.append(el('h4', null, 'A visit (optional)'));
    for (const n of [1, 2]) {
      const r = el('div', 'ns-row'), a = el('input'), b = el('input'); a.type = b.type = 'datetime-local';
      a.setAttribute('aria-label', `Visit window ${n}, from`); b.setAttribute('aria-label', `Visit window ${n}, until`);
      r.append(el('span', null, `Window ${n}`), a, el('span', null, 'to'), b); f.visit.push({ a, b }); f.root.append(r);
    }
    f.root.append(el('h4', null, 'Rent limits (optional)'));
    const r = el('div', 'ns-row');
    f.target = el('input'); f.target.type = 'text'; f.target.inputMode = 'numeric'; f.target.size = 16; f.target.placeholder = 'Target $ a month'; f.target.setAttribute('aria-label', 'Target rent, dollars a month');
    f.max = el('input'); f.max.type = 'text'; f.max.inputMode = 'numeric'; f.max.size = 16; f.max.placeholder = 'Maximum $ a month'; f.max.autocomplete = 'off'; f.max.setAttribute('aria-label', 'Maximum rent, dollars a month');
    f.counters = el('input'); f.counters.type = 'number'; f.counters.min = '0'; f.counters.max = '5'; f.counters.value = '1'; f.counters.style.width = '64px'; f.counters.setAttribute('aria-label', 'Counter-offers');
    r.append(el('span', null, 'Target $'), f.target, el('span', null, 'Maximum $'), f.max, el('span', null, 'Counter-offers'), f.counters); f.root.append(r, el('p', 'ns-note', TEXT.counters));
    void contact;
    return f;
  }
  function readForm() {
    const f = st.form; if (!f) return {};
    return { questions: f.checks.filter(c => c.checked).map(c => c.value), free: f.free.value, visit: f.visit.map(v => ({ start: v.a.value, end: v.b.value })), target: f.target.value, max: f.max.value, counters: f.counters.value === '' ? 1 : Number(f.counters.value) };
  }

  /* ---------- drawing */
  const btn = (label, fn, { solid = false, off = false } = {}) => { const b = el('button', 'ns-btn' + (solid ? ' solid' : ''), label); b.type = 'button'; b.disabled = !!st.busy || st.reading > 0 || off; if (fn) b.addEventListener('click', fn); return b; };
  const list = (tag, items) => { const u = el(tag); for (const t of items) u.append(el('li', null, t)); return u; };

  function cardChose(f) {
    const card = el('section', 'ns-card'); card.id = 'ns-chose'; card.append(el('h3', null, TEXT.chose));
    const dl = el('dl'), row = (k, v) => dl.append(el('dt', null, k), el('dd', null, v));
    row('Listing', f.name); row('Data origin', f.origin); row('Rent', f.rent); row('Area', f.area); row('Layout', f.layout);
    row(f.sketch ? 'Floor: approximate sketch' : 'Floor', f.floor); row('Furnishing bill', f.bill); row('Cost to open', f.open);
    card.append(dl, el('h4', null, TEXT.notConfirmed));
    if (!f.issues.length) card.append(el('p', null, TEXT.noneOpen));
    else card.append(list('ul', f.issues.map(i => `${i.text}. ${i.question ? `To verify: ${i.question}` : 'The backend gives no verification question.'}`)));
    return card;
  }
  function cardAsk(f) {
    const card = el('section', 'ns-card'); card.id = 'ns-ask'; card.append(el('h3', null, TEXT.ask));
    const rec = (ctx.records?.() || []).find(r => r?.rec?.property_id === st.id)?.rec, contact = contactOf(rec);
    const s = stateOf(st.lists, st.id), live = st.mode === 'live', sim = st.mode === 'simulated';
    const real = !!s.call && s.call.simulated !== true; // a call the backend does not label simulated
    card.append(el('p', 'ns-sim', real ? TEXT.started : live ? TEXT.live : sim ? TEXT.sim : TEXT.unknownMode));
    if (sim && !real) card.append(el('p', 'ns-note', TEXT.script));
    if (!sim && !live && !real && !st.readLine) { const r = el('div', 'ns-row'); r.append(btn('Read again', () => { if (!st.busy) read(); })); card.append(r); }
    if (!contact.ok) { card.append(el('p', 'ns-line', TEXT.noContact)); return card; }
    card.append(el('p', null, `Contact on the listing: ${contact.name}${contact.company ? `, ${contact.company}` : ''}, ${contact.phone} (as the listing gives it; not dialled from this page).`));
    if (st.readLine) { card.append(el('p', 'ns-line', st.readLine)); const r = el('div', 'ns-row'); r.append(btn('Read again', () => { if (!st.busy) read(); })); card.append(r); }
    const asking = ['none', 'refused', 'expired', 'used', 'other', 'unknown'].includes(s.phase);
    if (!st.form) st.form = buildForm(f, contact);
    if (asking && st.lists && sim) card.append(st.form.root); // only when the backend says calls are simulated is an approval asked for from this page
    if (PHASE_LINE[s.phase]) card.append(el('p', 'ns-line', PHASE_LINE[s.phase]));
    const steps = el('ol'); steps.style.cssText = 'display:flex;flex-direction:column;gap:10px';
    const step = (...nodes) => { const li = el('li'); li.append(...nodes); steps.append(li); return li; };
    // 1
    const r1 = el('div', 'ns-row'); r1.append(btn(st.busy === 'request' ? 'Requesting…' : asking ? 'Request approval' : 'Approval requested', request, { solid: asking && sim, off: !asking || !st.lists || !sim }));
    step(r1, el('p', 'ns-note', TEXT.ownerText));
    // 2
    const r2 = el('div', 'ns-row'), pending = s.phase === 'pending';
    const sum = s.approval && ['pending', 'approved', 'call'].includes(s.phase) ? el('p', null, `The approval covers: ${clip(s.approval.summary, MAX.line)} (${words(s.approval.approval_status)})`) : null;
    r2.append(btn(st.busy === 'approve' ? 'Approving…' : pending || asking ? 'Approve' : 'Approved', () => decide('APPROVED'), { solid: pending && sim, off: !pending || !sim }));
    if (pending) r2.append(btn('Refuse', () => decide('REJECTED'), {}));
    step(...(sum ? [sum, r2] : [r2]));
    // 3
    const r3 = el('div', 'ns-row');
    const unfinished = s.phase === 'call' && !callDone(s.call) && s.call.simulated === true;
    if (!live && !real && (sim || unfinished)) { // offered only when the backend says calls are simulated (or this call is); else the top line says why
      const ready = s.phase === 'approved' && sim;
      const sel = el('select'); sel.setAttribute('aria-label', 'If the call is held for you');
      for (const [v, t] of [['accept', 'If the call is held for you: you join'], ['later', 'If the call is held for you: you answer later'], ['none', 'If the call is held for you: you do not answer']]) { const o = el('option', null, t); o.value = v; o.selected = st.ownerAction === v; sel.append(o); }
      sel.disabled = !!st.busy || !(ready || unfinished); sel.addEventListener('change', () => { st.ownerAction = sel.value; });
      if (unfinished) r3.append(btn(st.busy === 'run' ? 'Simulating…' : 'Run the simulated conversation', run, { solid: true }));
      else r3.append(btn(st.busy === 'start' || st.busy === 'run' ? 'Simulating…' : s.phase === 'call' ? 'Simulated call run' : 'Start the simulated call', start, { solid: ready, off: !ready }));
      r3.append(sel); step(r3);
    }
    card.append(steps);
    if (st.line) card.append(el('p', 'ns-line', st.line));
    if (s.call && !real) { // a call not labelled simulated is never drawn: the top line says it was started
      const c = callParts(s.call, s.handoffs);
      card.append(el('h4', null, c.head));
      if (c.transcript.length) { card.append(el('h4', null, `Transcript (${c.tag})`), list('ul', c.transcript)); if (c.more) card.append(el('p', 'ns-note', `${c.more} more lines are not shown.`)); }
      if (c.findings.length) card.append(el('h4', null, `What was learned (${c.tag}; as the other side stated it, not verified)`), list('ul', c.findings));
      if (c.deferred.length) card.append(el('h4', null, `Left unanswered (${c.tag})`), list('ul', c.deferred));
      if (c.holds.length) card.append(el('h4', null, `Held for you (${c.tag})`), list('ul', c.holds));
      if (c.summary) { card.append(el('h4', null, `Summary (${c.tag}), in the backend’s words`)); card.append(el('p', 'ns-pre', c.summary)); }
    }
    return card;
  }
  function draw() {
    if (!st.open) return;
    const c = chosen();
    if (!c || c.id !== st.id) return close();
    const f = chosenFacts(c.p, c.d, c.S, page.C, lib);
    const keep = doc.activeElement && st.form?.root.contains(doc.activeElement) ? doc.activeElement : null, y = stage.scrollTop;
    const top = el('div', 'ns-top'), back = el('button', 'btn', TEXT.back); back.type = 'button'; back.addEventListener('click', close);
    top.append(el('h2', null, TEXT.title), back);
    if (!st.nessie) {
      st.nessie = el('div'); st.nessie.id = 'ns-payments';
      const box = st.nessie, be = ctx.backend();
      const id = st.id;
      // One card per listing; nothing is shown, and nothing is said, when the module is absent or fails.
      import('./nessie.js').then(m => { if (typeof m.nessieCard === 'function' && be?.project && st.nessie === box && st.id === id) st.payments = m.nessieCard(be.project, id, signedReq(be), box); }).catch(() => { box.textContent = ''; });
    }
    col.replaceChildren(top, cardChose(f), cardAsk(f), st.nessie, el('p', 'ns-end', TEXT.end));
    stage.scrollTop = y; keep?.focus?.();
  }

  // The button is offered only while a live listing with a layout is being furnished.
  const sync = () => { const c = chosen(); goBar.hidden = !c; navBtn.hidden = !c; navBtn.setAttribute('aria-current', st.open ? 'page' : 'false'); if (st.open && (!c || c.id !== st.id || page.S().stage !== 'furnish')) close(); };
  sync(); const timer = setInterval(sync, 700);
  // After a reload: back where the person was, for the same listing only.
  const c0 = chosen(); if (c0 && kv.get() === c0.id && page.S().stage === 'furnish') open();
  mounted = { open, close, state: st, stop: () => clearInterval(timer) };
  return mounted;
}
export function unmount() { mounted?.stop(); mounted = null; } // tests only
