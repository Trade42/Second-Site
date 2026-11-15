// Second Site workspace prototype. One page, three stages, one conversation.
// Design mock: the conversation is scripted and every figure is demo data.

const $ = (s, r = document) => r.querySelector(s);
const $$ = (s, r = document) => [...r.querySelectorAll(s)];
const money = n => '$' + Math.round(n).toLocaleString('en-US');
const esc = s => String(s).replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const cap = s => s.charAt(0).toUpperCase() + s.slice(1);
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const asset = (src, f) => `assets/${src}/${f}`;
const ICON = {
  x: '<svg viewBox="0 0 16 16" width="13" height="13" aria-hidden="true"><path d="m4 4 8 8M12 4l-8 8" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>',
};

/* ------------------------------------------------------------------ data */

// Three real listings (photos, scans, aerials). The rest reuse their photos with a
// hue shift as stand-ins until more listings are pulled.
const POOL = [
  { id: 'mateo-109', src: 'mateo-109', photo: 'costar-01.jpg', address: '915 Mateo St', short: '915 Mateo', suite: 'Suite 109', hood: 'Arts District', zone: 'downtown', rent: 2211, sqft: 1128, use: 'Office / retail', lease: 'Modified gross', broker: 'Cannon, Binford Lofts', lat: 34.03186, lon: -118.23261 },
  { id: 'sixth-110b', src: 'sixth-110b', photo: 'costar-01.jpg', address: '630 W 6th St', short: '630 W 6th', suite: 'Suite 110B', hood: 'Downtown', zone: 'downtown', rent: 4048, sqft: 1310, use: 'Retail', lease: 'Triple net', broker: 'NAI Capital', lat: 34.04907, lon: -118.2563 },
  { id: 'beverly-720', src: 'beverly-720', photo: 'costar-01.jpg', address: '1125 S Beverly Dr', short: '1125 Beverly', suite: 'Suite 720', hood: 'Pico-Robertson', zone: 'westside', rent: 5215, sqft: 1490, use: 'Medical office', lease: 'Full service', broker: 'Jedamist', lat: 34.05685, lon: -118.3968 },
  { id: 'broadway', src: 'mateo-109', photo: 'costar-14.jpg', hue: 0, address: '1933 S Broadway', short: '1933 Broadway', suite: 'Unit 4', hood: 'South Park', zone: 'downtown', rent: 8200, sqft: 3400, use: 'Industrial', lease: 'Triple net', broker: 'Lee & Associates', lat: 34.0275, lon: -118.2685 },
  { id: 'lankershim', src: 'sixth-110b', photo: 'costar-05.jpg', hue: 28, address: '5250 Lankershim Blvd', short: '5250 Lankershim', suite: 'Suite 120', hood: 'North Hollywood', zone: 'valley', rent: 3300, sqft: 1250, use: 'Retail', lease: 'Triple net', broker: 'Illi Commercial', lat: 34.166, lon: -118.3745 },
  { id: 'sunset', src: 'mateo-109', photo: 'costar-03.jpg', hue: -35, address: '4311 W Sunset Blvd', short: '4311 Sunset', suite: 'Unit 2', hood: 'Silver Lake', zone: 'eastside', rent: 3750, sqft: 980, use: 'Retail', lease: 'Modified gross', broker: 'Figure 8 Realty', lat: 34.0935, lon: -118.2835 },
  { id: 'sanvicente', src: 'beverly-720', photo: 'costar-09.jpg', hue: 150, address: '11677 San Vicente Blvd', short: '11677 San Vicente', suite: 'Suite 310', hood: 'Brentwood', zone: 'westside', rent: 7450, sqft: 1380, use: 'Medical office', lease: 'Full service', broker: 'Madison Partners', lat: 34.0535, lon: -118.465 },
  { id: 'third', src: 'sixth-110b', photo: 'costar-04.jpg', hue: -20, address: '8560 W 3rd St', short: '8560 W 3rd', suite: 'Suite 204', hood: 'Beverly Grove', zone: 'westside', rent: 5400, sqft: 1220, use: 'Office / retail', lease: 'Full service', broker: 'Kennedy Wilson', lat: 34.0735, lon: -118.3785 },
  { id: 'olive', src: 'sixth-110b', photo: 'costar-06.jpg', hue: 18, address: '818 S Olive St', short: '818 Olive', suite: 'Suite B', hood: 'South Park', zone: 'downtown', rent: 3150, sqft: 640, use: 'Retail', lease: 'Triple net', broker: 'Avison Young', lat: 34.044, lon: -118.259 },
];
const P = Object.fromEntries(POOL.map(p => [p.id, p]));
const FIRST = ['broadway', 'lankershim', 'sunset', 'sanvicente'];
const FILL = ['olive', 'sixth-110b', 'third', 'mateo-109', 'beverly-720', 'sanvicente', 'sunset', 'lankershim', 'broadway'];
const ZONE = { downtown: 'Downtown', westside: 'Westside', eastside: 'Eastside', valley: 'The Valley' };
const FRAMES = [{ g: 1.12, h: .94, y: 0, t: 17, d: 0 }, { g: .94, h: .94, y: .04, t: 21, d: -6 }, { g: 1.04, h: .94, y: 0, t: 19, d: -11 }, { g: .9, h: .94, y: .04, t: 23, d: -3 }];

// What the listing does not show. Plans are 100 px per metre; `mesh` is the model
// coordinate of the plan's top-left ink corner, so plan positions map into the 3D model.
// ponytail: mesh alignment is a linear fit per property; nudge `mesh` if blocks sit off.
const D = {
  'mateo-109': {
    extras: null, build: 142000, other: 29400, weeks: 21,
    photos: ['matterport-01.jpg', 'matterport-02.jpg', 'matterport-04.jpg', 'matterport-05.jpg', 'costar-08.jpg', 'costar-01.jpg'],
    plan: { w: 1607, h: 680, ink: [39, 40], mesh: [-13.7, 1.01], box: [-13.7, 1.5, -4.97, 1.01] },
    rooms: [
      { id: 't1', type: 'treatment', name: 'Treatment 1', r: [.04, .09, .25, .53], proposed: 1 },
      { id: 't2', type: 'treatment', name: 'Treatment 2', r: [.26, .09, .47, .53], proposed: 1 },
      { id: 't3', type: 'treatment', name: 'Treatment 3', r: [.48, .09, .69, .53], proposed: 1 },
      { id: 'rc', type: 'reception', name: 'Reception', r: [.7, .09, .965, .6], proposed: 1 },
      { id: 'st', type: 'storage', name: 'Storage', r: [.04, .62, .2, .915], proposed: 1 },
    ],
    issues: [
      { id: 'plumbing', sev: 'high', title: 'Water and drains reach one corner only', impact: '~$46,000 · 6 wks', sum: 'Each treatment room needs a sink. The only plumbing in the scan is the restroom at the east end.', why: 'Reaching three rooms means cutting a trench through the concrete slab, then patching and sealing it before any walls go up.', kind: 'observed', ev: 'Matterport scan, July 2026. One restroom, no other fixtures or floor drains visible.', next: 'Ask for plumbing as-builts and whether the landlord allows slab trenching.', ask: 'Could you send the plumbing as-builts for Suite 109, and confirm whether slab trenching is permitted for three new sinks?', show: { view: 'photos', photo: 1, note: 'The only plumbing in the scan' } },
      { id: 'layout', sev: 'high', title: 'No rooms exist yet', impact: '~$58,000 · 7 wks', sum: 'The suite is one open room. Three treatment rooms, reception and storage all have to be built.', why: 'New partitions need permits, sound insulation, doors, lighting and air supply to each room. This is most of the opening cost.', kind: 'observed', ev: 'Measured floor plan from the scan. One open space and a restroom.', next: 'Test a three-room layout to confirm the rooms fit with a clear path to the exit.', show: { view: 'floorplan', note: 'One possible layout, not built' } },
      { id: 'power', sev: 'unk', title: 'Electrical capacity is not documented', impact: 'Unknown', sum: 'Steamers, towel warmers and a water heater for three rooms may need more power than a creative office has.', why: 'A service upgrade can add months, because it depends on the utility as well as the landlord.', kind: 'unknown', ev: 'No panel is visible in the scan and the listing gives no service size.', next: 'Ask for a photo of the panel and its schedule.', ask: 'Could you share a photo of the electrical panel for Suite 109 and its amperage?' },
      { id: 'lease', sev: 'unk', title: 'Charges beyond rent are not stated', impact: 'Unknown', sum: 'A modified gross lease passes some building costs to you. The listing does not say which.', why: 'Utilities, janitorial and a share of common-area costs are often billed on top of the asking rent.', kind: 'documented', ev: 'CoStar listing, October 15, 2026. Lease type: modified gross, $1.96 per SF per month.', next: 'Ask for last year’s pass-through charges for this suite.', ask: 'Which expenses are passed through to the tenant at Suite 109, and what did they total last year?' },
    ],
  },
  'sixth-110b': {
    extras: 1290, build: 88000, other: 33600, weeks: 15,
    photos: ['matterport-04.jpg', 'matterport-05.jpg', 'matterport-02.jpg', 'matterport-01.jpg', 'costar-04.jpg', 'costar-01.jpg'],
    plan: { w: 1786, h: 1033, ink: [89, 48], mesh: [-13.79, 1.45], box: [-13.79, 2.18, -6.94, 1.45] },
    rooms: [
      { id: 't1', type: 'treatment', name: 'Treatment 1', r: [.175, .11, .415, .42] },
      { id: 't2', type: 'treatment', name: 'Treatment 2', r: [.13, .45, .415, .625] },
      { id: 't3', type: 'treatment', name: 'Treatment 3', r: [.7, .11, .86, .46], proposed: 1 },
      { id: 'rc', type: 'reception', name: 'Reception', r: [.445, .14, .69, .85], proposed: 1 },
      { id: 'st', type: 'storage', name: 'Storage', r: [.24, .66, .342, .865] },
    ],
    issues: [
      { id: 'equipment', sev: 'high', title: 'A restaurant has to come out first', impact: '~$24,000 · 3 wks', sum: 'The space still has a service counter, a kitchen line and an exhaust hood from the last tenant.', why: 'Removing a hood means capping the duct and patching the roof or wall where it exits. None of it is useful to a studio.', kind: 'observed', ev: 'Matterport scan, December 2024. Counter, cook line and hood are in place.', next: 'Ask whether the landlord will deliver the space cleared.', ask: 'Will Suite 110B be delivered with the kitchen equipment and hood removed, or is that the tenant’s cost?', show: { view: 'photos', photo: 0, note: 'Left by the last tenant' } },
      { id: 'lease', sev: 'high', title: 'Triple net adds about $1,290 a month', impact: '~$1,290 / mo', sum: 'On top of rent you pay a share of the building’s taxes, insurance and maintenance.', why: 'These charges change every year and are not capped unless the lease says so.', kind: 'estimate', ev: 'Lease type is from the CoStar listing, October 2, 2026. The amount is estimated from similar downtown retail.', next: 'Ask for the current NNN rate per square foot.', ask: 'What are the current NNN charges per square foot for Suite 110B?' },
      { id: 'layout', sev: 'mid', title: 'Rooms need building in the dining area', impact: '~$43,000 · 5 wks', sum: 'Two back rooms could work as they are. The third room and reception have to be built.', why: 'New walls here cross the old kitchen floor, which needs levelling first.', kind: 'observed', ev: 'Measured floor plan from the scan.', next: 'Test a layout that keeps the two existing rooms.', show: { view: 'floorplan', note: 'Dashed rooms would be built' } },
      { id: 'plumbing', sev: 'mid', title: 'Drains follow the kitchen, not the rooms', impact: '~$21,000 · 3 wks', sum: 'There is plenty of plumbing, but it runs along the cook line on the far side from the back rooms.', why: 'Extending supply and drain lines to three sinks is simpler than starting from nothing, but still needs floor work.', kind: 'observed', ev: 'Matterport scan, December 2024. Sinks and floor drains along the kitchen wall.', next: 'Ask for plumbing as-builts.', ask: 'Could you send plumbing as-builts for Suite 110B?' },
      { id: 'power', sev: 'low', title: 'Power is probably ample', impact: 'None expected', sum: 'A commercial kitchen draws far more than a facial studio will.', why: 'Worth one question to rule out a surprise.', kind: 'unknown', ev: 'Inferred from the previous use. The panel is not visible in the scan.', next: 'Confirm the panel size with the broker.', ask: 'What is the electrical service size for Suite 110B?' },
    ],
  },
  'beverly-720': {
    extras: 420, build: 31000, other: 37900, weeks: 7,
    photos: ['costar-19.jpg', 'costar-22.jpg', 'matterport-02.jpg', 'matterport-03.jpg', 'matterport-04.jpg', 'matterport-05.jpg', 'costar-23.jpg'],
    plan: { w: 1638, h: 1034, ink: [38, 37], mesh: [-1.13, 7.74], box: [-1.13, 14.36, -1.76, 7.74] },
    rooms: [
      { id: 't1', type: 'treatment', name: 'Treatment 1', dim: '12½ × 11 ft', r: [.375, .28, .56, .55] },
      { id: 't2', type: 'treatment', name: 'Treatment 2', dim: '14 × 10 ft', r: [.708, .36, .96, .64] },
      { id: 't3', type: 'treatment', name: 'Treatment 3', dim: '14 × 10 ft', r: [.708, .672, .957, .95] },
      { id: 'rc', type: 'reception', name: 'Reception', dim: '36 × 13 ft', r: [.035, .62, .68, .95] },
      { id: 'st', type: 'storage', name: 'Storage', dim: '9 × 7 ft', r: [.44, .05, .595, .245] },
    ],
    issues: [
      { id: 'size', sev: 'mid', title: 'You would pay for 1,490 SF and use about 1,080', impact: '~$1,440 / mo', sum: 'The listed size includes a share of lobbies and corridors. The rooms inside the suite measure about 1,080 SF.', why: 'About a quarter of the rent is for space you cannot furnish. It also changes how this compares per usable square foot.', kind: 'documented', ev: 'Listing size from CoStar, September 10, 2026. Room sizes measured by the Matterport scan.', next: 'Ask how the rentable area was calculated.', ask: 'How is the 1,490 SF for Suite 720 calculated, and what is the usable area?', show: { view: 'floorplan', note: 'About 1,080 SF measured' } },
      { id: 'lease', sev: 'mid', title: 'Evening and weekend air is billed by the hour', impact: '~$420 / mo', sum: 'Full-service rent in a medical tower usually covers weekday business hours only.', why: 'A studio earns most on evenings and Saturdays, when after-hours air conditioning is charged separately.', kind: 'estimate', ev: 'Not in the listing. Estimated at $35 an hour for 12 hours a month, typical for comparable towers.', next: 'Ask for building hours and the after-hours rate.', ask: 'What are the building HVAC hours at 1125 S Beverly Dr, and the after-hours rate?' },
      { id: 'layout', sev: 'low', title: 'The fourth room is too small to treat in', impact: '3 rooms, not 4', sum: 'Three rooms are 143 to 154 SF, right for a facial bed with space to work. The fourth is 9 by 7 ft.', why: 'It suits storage and laundry. If a fourth treatment room matters later, this suite cannot grow into it.', kind: 'documented', ev: 'Measured floor plan from the scan.', next: 'Try a layout to confirm bed clearances.', show: { view: 'floorplan', note: 'The fourth room is 9 × 7 ft' } },
      { id: 'equipment', sev: 'low', title: 'Sink cabinets exist, condition untested', impact: '~$3,500', sum: 'Three rooms already have a sink cabinet, which is why this space is cheap to open.', why: 'If the valves or drains have failed since the suite was vacated, replacing them is still a small job.', kind: 'observed', ev: 'Matterport scan, July 2024. Sink cabinets in three rooms.', next: 'Run the taps at the walkthrough.', show: { view: 'photos', photo: 2, note: 'Existing sink cabinet' } },
    ],
  },
};
const KIND = { documented: 'Documented', observed: 'Observed', estimate: 'Estimate', unknown: 'Unknown' };
const VIEW = { dollhouse: 'Dollhouse', floorplan: 'Floor plan', aerial: 'Aerial', map: 'Map', photos: 'Photos' };

// Furnishing catalog. w, d, h in feet. `at` are positions within the room, 0 to 1.
const CAT = [
  { id: 'desk', room: 'reception', name: 'Reception desk, oak, 60 in', kw: 'desk', w: 5, d: 2.2, h: 3.5, price: 1850, by: 'Blu Dot', lead: '2 wks', at: [[.28, .35]] },
  { id: 'desk-s', room: 'reception', name: 'Reception desk, oak, 42 in', kw: 'compact', w: 3.5, d: 2, h: 3.5, price: 1240, by: 'Blu Dot', lead: '2 wks', at: [[.28, .35]] },
  { id: 'bench', room: 'reception', name: 'Waiting bench, bouclé', kw: 'bench|seat', w: 5.5, d: 1.8, h: 1.5, price: 920, by: 'Article', lead: '1 wk', at: [[.8, .9], [.2, .9]] },
  { id: 'retail', room: 'reception', name: 'Retail shelving, wall, 48 in', kw: 'retail|shelving', w: 4, d: 1.2, h: 6, price: 640, by: 'Rejuvenation', lead: '3 wks', at: [[.72, .08], [.5, .08]] },
  { id: 'bed', room: 'treatment', name: 'Facial bed, electric lift', kw: 'bed|table', w: 6.2, d: 2.5, h: 2.4, price: 2450, by: 'Earthlite', lead: '2 wks', at: [[.5, .56]] },
  { id: 'stool', room: 'treatment', name: 'Esthetician stool', kw: 'stool', w: 1.5, d: 1.5, h: 1.7, price: 240, by: 'Earthlite', lead: '1 wk', at: [[.88, .56]] },
  { id: 'cart', room: 'treatment', name: 'Rolling trolley, 3 tier', kw: 'trolley|cart', w: 1.7, d: 1.3, h: 2.8, price: 310, by: 'Dir Salon', lead: '1 wk', at: [[.84, .2]] },
  { id: 'steamer', room: 'treatment', name: 'Facial steamer, ozone', kw: 'steamer', w: 1.5, d: 1.5, h: 4, price: 480, by: 'Dir Salon', lead: '1 wk', at: [[.2, .16]] },
  { id: 'lamp', room: 'treatment', name: 'LED magnifying lamp', kw: 'lamp|light', w: 1.3, d: 1.3, h: 5, price: 190, by: 'Daylight', lead: '4 days', at: [[.42, .14]] },
  { id: 'warmer', room: 'treatment', name: 'Towel warmer cabinet', kw: 'warmer|towel', w: 1.5, d: 1.2, h: 1.6, price: 260, by: 'Dir Salon', lead: '1 wk', at: [[.85, .9]] },
  { id: 'shelf', room: 'storage', name: 'Steel shelving, 48 in', kw: 'steel|rack', w: 4, d: 1.5, h: 6, price: 210, by: 'Uline', lead: '3 days', at: [[.3, .14], [.3, .86]] },
  { id: 'washer', room: 'storage', name: 'Stacked washer and dryer', kw: 'washer|dryer|laundry', w: 2.3, d: 2.6, h: 6.3, price: 1890, by: 'LG', lead: '1 wk', at: [[.82, .5]] },
];
const C = Object.fromEntries(CAT.map(c => [c.id, c]));
const FURNISH_COMMAND = /^(remove|add|replace|swap|move|rotate|resize|delete|undo)\b/i;
const ROOMTYPE = { reception: 'Reception', treatment: 'Treatment rooms', storage: 'Storage' };
const BATCH = { reception: ['desk', 'bench', 'retail'], treatment: ['bed', 'stool', 'cart', 'steamer'], storage: ['shelf', 'shelf', 'washer'] };

/* ----------------------------------------------------------------- state */

const fresh = () => ({ stage: 'discover', step: 0, req: {}, slots: [...FIRST], saved: [], dismissed: [], open: null, offered: false, msgs: [], replies: [], finalist: null, issue: null, view: 'dollhouse', photo: 0, dir: 'oblique-n', seenCompare: false, chosen: null, committed: false, placed: [], hist: [], tab: null, room: null, sel: null, seenFurnish: null });
const KEY = 'second-site-workspace';
let S = fresh();
try { S = { ...S, ...JSON.parse(localStorage.getItem(KEY) || '{}') }; } catch { /* storage unavailable: run in memory */ }
if (FURNISH_COMMAND.test(S.req.business || '')) S.req.business = null;
const save = () => { try { localStorage.setItem(KEY, JSON.stringify(S)); } catch { /* fine */ } };
let fast = false, busy = false;

/* ---------------------------------------------------------- conversation */

const log = $('#log');
const scrollLog = () => { log.scrollTop = 1e9; };
function bubble(m) { const el = document.createElement('div'); el.className = 'msg ' + m.who; el.innerHTML = m.html; return el; }
function say(who, html) { S.msgs.push({ who, html }); log.append(bubble(S.msgs.at(-1))); scrollLog(); save(); }
const a = html => say('a', html), note = html => say('n', html);
async function think(ms = 700) {
  if (fast) return;
  const t = document.createElement('div'); t.className = 'typing'; t.innerHTML = '<i></i><i></i><i></i>';
  log.append(t); scrollLog(); await new Promise(r => setTimeout(r, ms)); t.remove();
}
function replies(list) { S.replies = list; $('#replies').innerHTML = list.map(t => `<button type="button">${esc(t)}</button>`).join(''); scrollLog(); save(); }
async function send(text) {
  text = text.trim(); if (!text || busy) return;
  busy = true; say('u', esc(text)); replies([]);
  try { await TURN[S.stage](text); } finally { busy = false; save(); }
}

/* ------------------------------------------------------------ 1 discover */

const WORDNUM = { one: 1, two: 2, three: 3, four: 4, five: 5, six: 6 };
const STEPS = [
  { key: 'business', label: 'Business', replies: ['A facial and skincare studio', 'A pilates studio', 'A café'],
    ask: 'What kind of business is this location for?',
    set: t => { if (FURNISH_COMMAND.test(t.trim())) return false; S.req.business = /facial|skin/i.test(t) ? 'Facial studio' : cap(t.replace(/^(an?|the)\s+/i, '').replace(/[.!]+$/, '')).slice(0, 28); },
    show: r => r.business || 'Not set', note: n => `Industrial space set aside · ${n} fit` },
  { key: 'zones', label: 'Area', replies: ['Downtown or the Westside', 'Westside only', 'Anywhere in LA'],
    ask: 'Where in Los Angeles do you want to be?',
    set: t => { const z = []; if (/downtown|dtla|arts/i.test(t)) z.push('downtown'); if (/west|beverly|brentwood|santa monica/i.test(t)) z.push('westside'); if (/east|silver|echo/i.test(t)) z.push('eastside'); if (/valley|noho|north holly/i.test(t)) z.push('valley'); S.req.zones = z.length ? z : null; },
    show: r => r.zones ? r.zones.map(z => ZONE[z]).join(' or ') : 'Anywhere in LA', note: n => `${STEPS[1].show(S.req)} · ${n} fit` },
  { key: 'rent', label: 'Rent', replies: ['Up to $5,500', 'Up to $7,500', 'Under $3,500'],
    ask: 'What monthly rent can the studio carry?',
    set: t => { const m = t.replace(/,/g, '').match(/(\d+(?:\.\d+)?)\s*(k)?/i); S.req.rent = m ? Math.round(m[1] * (m[2] || m[1] < 100 ? 1000 : 1)) : null; },
    show: r => r.rent ? `Up to ${money(r.rent)} / month` : 'No limit', note: n => `${S.req.rent ? 'Up to ' + money(S.req.rent) : 'Any rent'} · ${n} fit` },
  { key: 'rooms', label: 'Space', replies: ['Three, plus reception', 'Two', 'Four'],
    ask: 'How many treatment rooms do you need?',
    set: t => { const m = t.match(/\d+/) || t.toLowerCase().match(/one|two|three|four|five|six/); S.req.rooms = m ? (WORDNUM[m[0]] || +m[0]) : 3; },
    show: r => `${r.rooms} treatment ${r.rooms === 1 ? 'room' : 'rooms'} + reception`, note: n => `${minSqft().toLocaleString()} SF or more · ${n} fit` },
  { key: 'opening', label: 'Opening', replies: ['By March', 'This summer', 'No fixed date'],
    ask: 'When would you like to open?',
    set: t => { S.req.opening = cap(t.replace(/[.!]+$/, '')).slice(0, 28); },
    show: r => r.opening, note: () => '' },
];
const minSqft = () => 250 * S.req.rooms + 250;
const matches = p => !S.dismissed.includes(p.id) && (!S.req.business || p.use !== 'Industrial') && (!S.req.zones || S.req.zones.includes(p.zone)) && (!S.req.rent || p.rent <= S.req.rent) && (!S.req.rooms || p.sqft >= minSqft());
const fits = () => POOL.filter(matches);

// Frames keep their property while it still fits, so the strip changes only where it must.
function refill() {
  S.slots = S.slots.map(id => (id && matches(P[id]) ? id : null));
  S.slots.forEach((id, i) => { if (!id) S.slots[i] = FILL.find(f => matches(P[f]) && !S.slots.includes(f)) || null; });
  if (S.open && !matches(P[S.open])) S.open = null;
}
async function research(noteFor) {
  $$('.frame').forEach(f => f.classList.add('loading'));
  const before = S.slots.join();
  await think(800);
  refill(); renderDiscover();
  const text = noteFor(fits().length);
  if (text && S.slots.join() !== before) note(text); // say something only when the photos changed
}
function ask() {
  if (S.step < STEPS.length) { a(STEPS[S.step].ask); replies(STEPS[S.step].replies); }
  else { a(fits().length ? 'Save the ones worth a closer look.' : 'Nothing fits yet. Try a higher rent or a wider area.'); offer(); }
}
const offer = () => replies(S.step >= STEPS.length && S.saved.length >= 2 ? ['Look into them'] : []);
async function discoverTurn(text) {
  if (S.step < STEPS.length) {
    const st = STEPS[S.step];
    if (st.set(text) === false) return ask();
    S.step++; await research(st.note); return ask();
  }
  if (/look into|compare|closer|ready|go ahead/i.test(text)) return toCompare();
  const i = /\$|rent|budget|month/i.test(text) ? 2 : /room/i.test(text) ? 3 : /downtown|west|east|valley|anywhere/i.test(text) ? 1 : /open/i.test(text) ? 4 : -1;
  if (i > 0) { STEPS[i].set(text); await research(STEPS[i].note); } else { await think(); a('Noted.'); }
  offer();
}
function toggleSave(id) {
  S.saved = S.saved.includes(id) ? S.saved.filter(x => x !== id) : [...S.saved, id];
  if (S.step >= STEPS.length && S.saved.length === 2 && !S.offered) { S.offered = true; a('Want me to look into what each would really take to open?'); }
  if (S.stage === 'discover') { if (S.step >= STEPS.length) offer(); renderDiscover(); }
  save();
}
function dismiss(id) {
  S.dismissed.push(id); S.saved = S.saved.filter(x => x !== id);
  refill(); if (S.step >= STEPS.length) offer(); renderDiscover(); save();
}
function selectSpace(id) { S.open = S.open === id ? null : id; renderDiscover(); save(); }
function toCompare() {
  if (S.saved.length < 2) return a('Save at least two first.');
  if (!S.saved.includes(S.finalist)) S.finalist = S.saved[0];
  go('compare');
}

function frameHTML(p) {
  return `<img src="${asset(p.src, p.photo)}" alt="${esc(p.address)}" style="${p.hue ? `filter:hue-rotate(${p.hue}deg)` : ''}">
    <button type="button" class="save-card" data-act="save" data-property="${p.id}" aria-label="Save ${esc(p.address)}"></button>
    <div class="acts"><button type="button" data-act="dismiss" aria-label="Dismiss ${esc(p.address)}">${ICON.x}</button></div>
    <div class="cap"><div><b>${esc(p.address)}</b><small>${esc(p.suite)} · ${p.sqft.toLocaleString()} SF</small></div><div class="rent"><b>${money(p.rent)}</b><small>a month</small></div></div>`;
}
function renderStrip() {
  const strip = $('#strip');
  if (!strip.children.length) strip.innerHTML = FRAMES.map(f => `<div class="frame" style="--g:${f.g};--h:calc(var(--strip-h) * ${f.h});--y:calc(var(--strip-h) * ${f.y});--t:${f.t}s;--d:${f.d}s"></div>`).join('');
  $$('.frame', strip).forEach((el, i) => {
    const id = S.slots[i] || '';
    if (id) el.dataset.property = id; else delete el.dataset.property;
    if (el.dataset.id !== id) {
      el.dataset.id = id;
      if (id) { el.innerHTML = frameHTML(P[id]); } else { el.innerHTML = 'No more spaces fit'; }
    }
    el.className = 'frame' + (id ? '' : ' none') + (S.saved.includes(id) ? ' saved' : '') + (S.open === id && id ? ' open' : '');
    $('[data-act=save]', el)?.setAttribute('aria-pressed', S.saved.includes(id));
  });
}
function renderBrief() {
  const rows = STEPS.filter(s => s.key in S.req), el = $('#brief');
  el.hidden = !rows.length;
  el.innerHTML = rows.length ? `<dl>${rows.map(s => `<dt>${s.label}</dt><dd><button type="button" data-edit="${s.key}" aria-label="Change ${s.label.toLowerCase()}">${esc(s.show(S.req))}</button></dd>`).join('')}</dl>` : '';
}
let map, pins = {}, fitKey = '';
function initMap() {
  if (!window.maplibregl) return; // offline: the static LA map behind stays visible
  try { map = new maplibregl.Map({ container: 'map', style: 'https://tiles.openfreemap.org/styles/positron', center: [-118.33, 34.07], zoom: 9.4, attributionControl: { compact: true } }); } catch { return; }
  POOL.forEach(p => {
    const wrap = document.createElement('div'), el = document.createElement('button');
    el.type = 'button'; el.setAttribute('aria-label', p.address);
    el.dataset.property = p.id;
    el.onclick = () => selectSpace(p.id);
    wrap.append(el); pins[p.id] = el;
    new maplibregl.Marker({ element: wrap }).setLngLat([p.lon, p.lat]).addTo(map);
  });
}
function renderMap() {
  if (!map) return;
  const vis = fits();
  POOL.forEach(p => { pins[p.id].className = 'pin location' + (vis.includes(p) ? '' : ' off') + (S.open === p.id ? ' on' : ''); });
  const key = vis.map(p => p.id).join();
  if (key === fitKey || !vis.length) return; // only move the map when the search itself changes
  fitKey = key;
  const b = new maplibregl.LngLatBounds(); vis.forEach(p => b.extend([p.lon, p.lat]));
  const wide = innerWidth > 860;
  map.fitBounds(b, { padding: { top: 60, bottom: 60, left: wide ? 300 : 40, right: wide ? 70 : 40 }, maxZoom: 13.2, duration: fast ? 0 : 1600 });
}
function renderDiscover() { renderStrip(); renderBrief(); renderMap(); renderNav(); }

function editBrief(key) {
  const st = STEPS.find(s => s.key === key), btn = $(`[data-edit="${key}"]`), input = document.createElement('input');
  input.value = st.show(S.req); input.setAttribute('aria-label', st.label);
  btn.replaceWith(input); input.focus(); input.select();
  let done = false;
  const commit = async ok => {
    if (done) return; done = true;
    if (ok && input.value.trim() && input.value !== st.show(S.req)) { st.set(input.value); await research(st.note); if (S.step >= STEPS.length) offer(); } else renderBrief();
  };
  input.onkeydown = e => { if (e.key === 'Enter') commit(true); if (e.key === 'Escape') commit(false); };
  input.onblur = () => commit(true);
}

/* ------------------------------------------------------------- 2 compare */

const finalists = () => S.saved.slice(0, 3);
const openIssues = d => d.issues.filter(i => i.sev !== 'low');
function costs(p) {
  const d = D[p.src], unk = d.extras == null, monthly = p.rent + (d.extras || 0), total = d.build + d.other;
  return { d, unk, monthly, total, three: total + 36 * monthly };
}
function renderFinalists() {
  $('#finalists').innerHTML = finalists().map(id => { const p = P[id];
    return `<button type="button" class="fin" data-finalist="${id}" aria-pressed="${id === S.finalist}"><img src="${asset(p.src, p.photo)}" alt="" style="${p.hue ? `filter:hue-rotate(${p.hue}deg)` : ''}"><span class="toopen">~${money(costs(p).total)} to open</span><div class="cap"><div><b>${esc(p.address)}</b><small>${esc(p.suite)} · ${p.sqft.toLocaleString()} SF</small></div><div class="rent"><b>${money(p.rent)}</b><small>a month</small></div></div></button>`; }).join('');
}
function renderFindings() {
  const ids = finalists(), d = D[P[S.finalist].src];
  const row = (label, fn, cls = '') => `<tr class="${cls}"><td>${label}</td>${ids.map(id => { const v = fn(P[id], costs(P[id])); return `<td class="${id === S.finalist ? 'sel' : ''} ${v === 'Not stated' ? 'unk' : ''}">${v}</td>`; }).join('')}</tr>`;
  const scroll = $('#findings').scrollTop;
  $('#findings').innerHTML = `
    <h2>Before you sign</h2>
    <div class="issues">${d.issues.map(i => `<details class="issue" name="issue" data-issue="${i.id}" ${S.issue === i.id ? 'open' : ''}>
        <summary><span class="t">${i.title}</span><span class="imp ${i.impact === 'Unknown' ? 'unk' : ''}">${i.impact}</span></summary>
        <div class="more"><p>${i.sum}</p><p class="ev">${KIND[i.kind]} · ${i.ev.split('. ')[0].replace(/\.$/, '')}</p>${i.ask ? `<button type="button" class="btn sm" data-act="ask" data-issue="${i.id}">Ask the broker</button>` : ''}</div>
      </details>`).join('')}</div>
    <h2>What it takes to open</h2>
    <div class="table-scroll" role="region" aria-label="Opening cost comparison" tabindex="0"><table class="costs">
      <thead><tr><th></th>${ids.map(id => `<th class="${id === S.finalist ? 'sel' : ''}"><button type="button" data-finalist="${id}">${esc(P[id].short)}</button></th>`).join('')}</tr></thead>
      <tbody>
        ${row('Rent', p => money(p.rent))}
        ${row('Lease extras', (p, c) => c.unk ? 'Not stated' : c.d.extras ? '~' + money(c.d.extras) : 'Included')}
        ${row('Renovation', (p, c) => '~' + money(c.d.build))}
        ${row('Total to open', (p, c) => '~' + money(c.total), 'total')}
        ${row('Time to open', (p, c) => `~${c.d.weeks} wks`)}
      </tbody>
    </table></div>`;
  $('#findings').scrollTop = scroll;
}
function selectFinalist(id) { if (id === S.finalist) return; S.finalist = id; S.issue = null; S.photo = 0; renderCompare(); save(); }
function showIssue(id, openIt = true) {
  const i = D[P[S.finalist].src].issues.find(x => x.id === id); if (!i) return null;
  S.issue = id;
  if (i.show) { S.view = i.show.view; if (i.show.photo != null) S.photo = i.show.photo; }
  if (openIt) renderFindings();
  renderViewer(); save();
  return i;
}
function renderCompare() { renderFinalists(); renderFindings(); renderViewer(); renderNav(); }
function draft(issue) {
  a(`<div class="draft"><span class="label">To ${esc(P[S.finalist].broker)}</span><p>${issue.ask}</p><button type="button" class="btn sm solid" data-act="send-draft">Approve and send</button></div>`);
}
function explore(commit) {
  if (S.chosen !== S.finalist) { S.placed = []; S.hist = []; S.room = S.sel = null; }
  S.chosen = S.finalist; S.committed = !!commit; go('furnish');
}
async function compareTurn(text) {
  const named = finalists().find(id => new RegExp(P[id].short.split(' ').pop().replace('6th', '6th|sixth'), 'i').test(text));
  if (named) selectFinalist(named);
  const p = P[S.finalist], d = D[p.src];
  await think();
  if (/anyway/i.test(text)) { a(`Done. ${p.address} is your space.`); return explore(true); }
  if (/layout|try/i.test(text)) return explore(false);
  if (/choose|go with|pick|sign/i.test(text)) {
    const open = openIssues(d);
    if (!open.length) { a(`Done. ${p.address} is your space.`); return explore(true); }
    a(`${open.length} ${open.length === 1 ? 'question is' : 'questions are'} still open here. Choose it anyway?`);
    return replies(['Choose it anyway', 'Try a layout first']);
  }
  const topic = /plumb|sink|water|drain/i.test(text) ? 'plumbing' : /power|electr|panel/i.test(text) ? 'power' : /lease|nnn|charge|air|hvac|net/i.test(text) ? 'lease' : /room|fit|small|size|sf\b/i.test(text) ? 'layout' : /kitchen|equipment|restaurant|hood/i.test(text) ? 'equipment' : null;
  if (/ask|broker|draft|question/i.test(text)) {
    const i = d.issues.find(x => x.ask && (!topic || x.id === topic)) || d.issues.find(x => x.ask);
    if (i) return draft(i);
  }
  const i = (topic && showIssue(topic)) || showIssue(d.issues[0].id);
  a(i.sum);
  replies(i.ask ? ['Ask the broker', 'Try a layout'] : ['Try a layout']);
}

/* ------------------------------------------------------------- 3 furnish */

const roomsOf = () => D[P[S.chosen].src].rooms;
const keyOf = it => `${it.c}|${it.r}`;
const sums = () => {
  const placed = S.placed.filter(i => i.st === 'placed'), items = placed.reduce((n, i) => n + C[i.c].price, 0);
  const proposed = S.placed.filter(i => i.st === 'proposed').reduce((n, i) => n + C[i.c].price, 0);
  const delivery = placed.length * 40, tax = items * .095, install = items * .06;
  return { items, proposed, delivery, tax, install, total: items + delivery + tax + install, count: placed.length };
};
function mutate(fn) { S.hist.push(JSON.stringify(S.placed)); if (S.hist.length > 30) S.hist.shift(); fn(); renderFurnish(); save(); }
function undo() { if (!S.hist.length) return false; S.placed = JSON.parse(S.hist.pop()); renderFurnish(); save(); return true; }
function place(c, r, st = 'placed') {
  const cat = C[c], rooms = roomsOf().filter(x => x.type === cat.room);
  const room = rooms.find(x => x.id === (r || S.room)) || rooms.slice().sort((x, y) => S.placed.filter(i => i.c === c && i.r === x.id).length - S.placed.filter(i => i.c === c && i.r === y.id).length)[0];
  if (room) S.placed.push({ c, r: room.id, st });
  return room;
}
function furnishHead() {
  const p = P[S.chosen], s = sums();
  const part = (n, label, why) => `<span data-tip="${why}" tabindex="0">${money(n)} ${label}</span>`;
  $('#furnish-head').innerHTML = `<h2>${esc(p.address)}, ${esc(p.suite)}</h2>` +
    (s.count ? `<div class="cost"><b>${money(s.total)}</b><p>${part(s.items, 'items', `${s.count} pieces placed`)} + ${part(s.delivery, 'delivery', '$40 a piece, carried in')} + ${part(s.tax, 'tax', 'Los Angeles sales tax, 9.5%')} + ${part(s.install, 'install', 'Assembly and placement, 6% of items')}</p></div>` : '') +
    (S.committed ? '' : '<button type="button" class="btn solid" data-act="commit">Choose this space</button>');
}
// A stand-in model: a box with the piece's real proportions, until real 3D models exist.
function model(c, size) {
  const k = size / Math.max(c.w, c.d, c.h, 3.2);
  return `<span class="model"><span class="box" style="--w:${(c.w * k).toFixed(1)}px;--d:${(c.d * k).toFixed(1)}px;--h:${(c.h * k).toFixed(1)}px"><i class="f"></i><i class="b"></i><i class="l"></i><i class="r"></i><i class="t"></i></span></span>`;
}
function renderPane() {
  const rooms = roomsOf(), room = rooms.find(r => r.id === S.room), s = sums(), pane = $('#pane');
  $$('.tabs button').forEach(b => b.setAttribute('aria-selected', b.dataset.tab === S.tab));
  $('#parts-count').textContent = S.placed.length || '';
  $('#furnish .side').dataset.open = S.tab || '';
  const chat = $('.chat-slot[data-slot=furnish]'), market = $('#furnish .market');
  chat.inert = !!S.tab; chat.setAttribute('aria-hidden', !!S.tab);
  market.inert = !S.tab; market.setAttribute('aria-hidden', !S.tab);
  if (!S.tab) return; // Keep the outgoing contents intact while the panel fades away.
  const chip = room && S.tab ? `<div class="filter">${room.name}<button type="button" data-act="clear-room" aria-label="Show all rooms">${ICON.x}</button></div>` : '';
  if (S.tab !== 'parts') {
    pane.className = 'pane grid';
    pane.innerHTML = chip + `<div class="tiles">${CAT.filter(c => !room || room.type === c.room).map(c => {
      const n = S.placed.filter(i => i.c === c.id && (!room || i.r === room.id)).length;
      return `<button type="button" class="tile" data-place="${c.id}" data-tip="${esc(c.name)}" aria-label="Place ${c.name}, ${money(c.price)}">${model(c, 46)}<span class="price">${money(c.price)}</span>${n ? `<i class="n">${n}</i>` : ''}</button>`;
    }).join('')}</div>`;
    return;
  }
  pane.className = 'pane';
  const groups = {};
  S.placed.filter(i => !room || i.r === room.id).forEach(i => { const k = keyOf(i) + '|' + i.st; (groups[k] ||= { ...i, n: 0 }).n++; });
  const list = Object.values(groups);
  if (!list.length) { pane.innerHTML = chip + '<p class="blank">Nothing placed yet</p>'; return; }
  const proposed = S.placed.filter(i => i.st === 'proposed').length;
  pane.innerHTML = chip + `
    <table class="parts"><thead><tr><th>Item</th><th>Qty</th><th>Each</th><th>Total</th><th></th></tr></thead><tbody>
    ${list.map(g => { const c = C[g.c], rm = rooms.find(r => r.id === g.r);
      return `<tr data-key="${keyOf(g)}" class="${S.sel === keyOf(g) ? 'on' : ''}"><td>${c.name}<small>${rm.name}${g.st === 'proposed' ? ' · <span class="proposed">Proposed</span>' : ''}</small></td><td>${g.n}</td><td>${money(c.price)}</td><td>${money(c.price * g.n)}</td><td><button type="button" class="rm" data-remove="${keyOf(g)}|${g.st}" aria-label="Remove ${c.name}">${ICON.x}</button></td></tr>`; }).join('')}
    </tbody>${room ? '' : `<tfoot>
      <tr><td colspan="3">Items</td><td>${money(s.items)}</td><td></td></tr>
      <tr><td colspan="3">Delivery</td><td>${money(s.delivery)}</td><td></td></tr>
      <tr><td colspan="3">Tax</td><td>${money(s.tax)}</td><td></td></tr>
      <tr><td colspan="3">Install</td><td>${money(s.install)}</td><td></td></tr>
      <tr class="total"><td colspan="3">Total</td><td>${money(s.total)}</td><td></td></tr>
    </tfoot>`}</table>` + (proposed ? `<button type="button" class="btn sm" data-act="accept">Add the ${proposed} proposed</button>` : '');
  $('#pane tr.on')?.scrollIntoView({ block: 'nearest' });
}
function renderFurnish() { furnishHead(); renderPane(); renderViewer(); renderNav(); }
const furnishReplies = () => !S.placed.length ? ['Warm and minimal, three treatment rooms, under $25k'] : [
  S.placed.some(i => i.st === 'proposed') && 'Accept the proposals', S.placed.some(i => i.c === 'desk') && 'Use a smaller reception desk',
  S.placed.some(i => i.c === 'retail') && 'Remove the retail shelving'].filter(Boolean).slice(0, 2);
const findCat = text => CAT.find(c => new RegExp(c.kw, 'i').test(text));
async function furnishTurn(text) {
  await think(900);
  const cat = findCat(text), short = c => c.name.split(',')[0].toLowerCase();
  if (/undo/i.test(text)) a(undo() ? 'Undone.' : 'Nothing to undo.');
  else if (/accept|proposal|add them/i.test(text) && S.placed.some(i => i.st === 'proposed')) { mutate(() => S.placed.forEach(i => { i.st = 'placed'; })); a(`Added. ${money(sums().total)} in all.`); }
  else if (/swap|replace|smaller/i.test(text) && S.placed.some(i => i.c === 'desk')) { mutate(() => S.placed.forEach(i => { if (i.c === 'desk') i.c = 'desk-s'; })); a('Swapped for the 42 in desk, saving $610.'); }
  else if (/remove|take out|drop|delete|without/i.test(text) && cat && S.placed.some(i => i.c === cat.id)) { mutate(() => { S.placed = S.placed.filter(i => i.c !== cat.id || (S.room && i.r !== S.room)); }); a(`Removed the ${short(cat)}.`); }
  else if (/add|place|put|need|another/i.test(text) && cat) { let room; mutate(() => { room = place(cat.id); }); S.sel = `${cat.id}|${room.id}`; renderFurnish(); a(`Added a ${short(cat)} to ${room.name}.`); }
  else if (!S.placed.length || /room|warm|minimal|style|start over|furnish/i.test(text)) {
    mutate(() => { S.placed = []; roomsOf().forEach(r => { BATCH[r.type].forEach(c => place(c, r.id)); if (r.type === 'treatment') ['lamp', 'warmer'].forEach(c => place(c, r.id, 'proposed')); }); });
    a(`Placed ${sums().count} pieces. I would also add a lamp and a towel warmer to each treatment room.`);
  } else a('I could not find that in the catalog.');
  replies(furnishReplies());
}

/* ---------------------------------------------------------------- viewer */

// Footprint of a piece in plan-image pixels (100 px per metre).
function pieceRect(d, it, idx) {
  const c = C[it.c], room = d.rooms.find(r => r.id === it.r), PX = 30.48;
  const x0 = room.r[0] * d.plan.w, y0 = room.r[1] * d.plan.h, rw = room.r[2] * d.plan.w - x0, rh = room.r[3] * d.plan.h - y0;
  let w = c.w * PX, h = c.d * PX; if ((rh > rw) !== (h > w)) [w, h] = [h, w]; // long side follows the room
  const [u, v] = c.at[idx % c.at.length];
  return { w, h, cx: clamp(x0 + u * rw, x0 + w / 2 + 8, x0 + rw - w / 2 - 8), cy: clamp(y0 + v * rh, y0 + h / 2 + 46, y0 + rh - h / 2 - 8) };
}
function pieces(d) {
  const seen = {};
  return S.placed.map(it => { const k = keyOf(it), idx = seen[k] = (seen[k] ?? -1) + 1; return { it, key: k, ...pieceRect(d, it, idx) }; });
}
function paintPlan(p, d) {
  const el = $('#v-floorplan .plan'), img = $('img', el), src = asset(p.src, 'matterport-floorplan-08.png');
  if (!img.src.endsWith(src)) img.src = src;
  el.style.setProperty('--ar', d.plan.w / d.plan.h);
  const furnish = S.stage === 'furnish', pct = (v, t) => (v / t * 100).toFixed(3) + '%';
  const rooms = d.rooms.filter(r => furnish || S.issue === 'layout' || !r.proposed);
  $('.overlay', el).innerHTML = rooms.map(r => {
    const st = `left:${r.r[0] * 100}%;top:${r.r[1] * 100}%;width:${(r.r[2] - r.r[0]) * 100}%;height:${(r.r[3] - r.r[1]) * 100}%`;
    return furnish ? `<button type="button" class="room ${r.proposed ? 'proposed' : ''} ${S.room === r.id ? 'on' : ''}" data-room="${r.id}" aria-label="${r.name}" style="${st}"></button>` : `<div class="room ${r.proposed ? 'proposed' : ''}" style="${st}"></div>`;
  }).join('') + rooms.map(r => `<span class="rlabel" style="left:${r.r[0] * 100}%;top:${r.r[1] * 100}%">${r.name}${r.dim && !furnish ? `<i>${r.dim}</i>` : ''}</span>`).join('') + (furnish ? pieces(d).map(q => `<button type="button" class="piece ${q.it.st === 'proposed' ? 'proposed' : ''} ${S.sel === q.key ? 'on' : ''}" data-key="${q.key}" data-tip="${esc(C[q.it.c].name.split(',')[0])}" aria-label="${esc(C[q.it.c].name)}" style="left:${pct(q.cx - q.w / 2, d.plan.w)};top:${pct(q.cy - q.h / 2, d.plan.h)};width:${pct(q.w, d.plan.w)};height:${pct(q.h, d.plan.h)}"></button>`).join('') : '');
}

let T = null, want3D = '';
// Above the scan's middle, keep coherent walls and discard ceiling faces and rough fragments.
function cleanScanTop(geometry, upperStart) {
  const p = geometry.attributes.position, normals = geometry.attributes.normal, index = geometry.index, kept = [];
  const a = new T.THREE.Vector3(), b = new T.THREE.Vector3(), c = new T.THREE.Vector3(), face = new T.THREE.Vector3(), edge = new T.THREE.Vector3(), normal = new T.THREE.Vector3();
  for (let i = 0; i < index.count; i += 3) {
    const ids = [index.getX(i), index.getX(i + 1), index.getX(i + 2)];
    a.fromBufferAttribute(p, ids[0]); b.fromBufferAttribute(p, ids[1]); c.fromBufferAttribute(p, ids[2]);
    if ((a.z + b.z + c.z) / 3 > upperStart) { // source scans are Z-up
      face.subVectors(b, a).cross(edge.subVectors(c, a)).normalize();
      const rough = ids.filter(id => face.dot(normal.fromBufferAttribute(normals, id)) < .65).length >= 2;
      if (Math.abs(face.z) > .35 || rough) continue;
    }
    kept.push(...ids);
  }
  geometry.setIndex(kept);
  geometry.computeVertexNormals();
}
async function init3D() {
  const THREE = await import('three');
  const [{ OrbitControls }, { OBJLoader }, { mergeVertices }] = await Promise.all([import('three/addons/controls/OrbitControls.js'), import('three/addons/loaders/OBJLoader.js'), import('three/addons/utils/BufferGeometryUtils.js')]);
  const box = $('#v-dollhouse'), canvas = $('canvas', box);
  const renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true }); renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  const scene = new THREE.Scene();
  scene.add(new THREE.HemisphereLight(0xffffff, 0xcfc8b8, 1.5));
  const sun = new THREE.DirectionalLight(0xfff4e0, 1.4); sun.position.set(6, 12, 4); scene.add(sun);
  const camera = new THREE.OrthographicCamera(-10, 10, 10, -10, .1, 400), controls = new OrbitControls(camera, canvas);
  Object.assign(controls, { enableDamping: true, maxPolarAngle: Math.PI * .47, minZoom: .5, maxZoom: 4 });
  const blocks = new THREE.Group(); scene.add(blocks);
  const mat = c => new THREE.MeshStandardMaterial({ color: c, roughness: .95 });
  T = { THREE, renderer, scene, camera, controls, blocks, mergeVertices, loader: new OBJLoader(), cache: {}, cur: null, shell: new THREE.MeshStandardMaterial({ vertexColors: true, roughness: .95 }) /* inward scan normals leave an open dollhouse */, mats: { placed: mat(0x6E7E72), on: mat(0xA5823A), proposed: new THREE.MeshStandardMaterial({ color: 0x6E7E72, transparent: true, opacity: .35 }) } };
  const size = () => {
    const w = box.clientWidth, h = box.clientHeight; if (!w || !h) return;
    renderer.setSize(w, h, false);
    const half = Math.max(T.frameHeight || 20, (T.frameWidth || 20) / (w / h)) * .56;
    Object.assign(camera, { left: -half * w / h, right: half * w / h, top: half, bottom: -half }); camera.updateProjectionMatrix();
  };
  new ResizeObserver(size).observe(box); T.size = size;
  // Click a block to select it (ignore drags).
  let down = null; const ray = new THREE.Raycaster();
  canvas.addEventListener('pointerdown', e => { down = [e.clientX, e.clientY]; });
  canvas.addEventListener('pointerup', e => {
    if (!down || Math.hypot(e.clientX - down[0], e.clientY - down[1]) > 4 || S.stage !== 'furnish') return;
    const r = canvas.getBoundingClientRect(); ray.setFromCamera(new THREE.Vector2((e.clientX - r.left) / r.width * 2 - 1, -(e.clientY - r.top) / r.height * 2 + 1), camera);
    const hit = ray.intersectObjects(blocks.children)[0]; if (hit) selectPiece(hit.object.userData.key);
  });
  (function loop() { requestAnimationFrame(loop); if (box.hidden || !T.cur) return; controls.update(); renderer.render(scene, camera); })();
}
function frame3D() {
  const { camera, controls, THREE } = T, bounds = new THREE.Box3().setFromObject(T.cur);
  controls.enableDamping = false; controls.update(); // consume orbit momentum before resetting the pose
  bounds.getCenter(controls.target);
  camera.position.copy(controls.target).add(new THREE.Vector3(-1, 1, 1).multiplyScalar(30));
  camera.zoom = 1; camera.lookAt(controls.target); controls.update(); controls.enableDamping = true; camera.updateMatrixWorld();
  const view = bounds.clone().applyMatrix4(camera.matrixWorldInverse).getSize(new THREE.Vector3());
  T.frameWidth = view.x; T.frameHeight = view.y; T.size();
}
async function paint3D(p, d) {
  const box = $('#v-dollhouse'), poster = $('.poster', box), state = $('.vstate', box), src = p.src;
  want3D = src;
  if (!poster.src.endsWith(`dollhouses/${src}.png`)) poster.src = `assets/dollhouses/${src}.png`;
  if (!T?.cache[src]) { box.dataset.state = 'loading'; state.textContent = 'Loading model'; }
  try {
    if (!T) await init3D();
    let obj = T.cache[src];
    if (!obj) {
      obj = await T.loader.loadAsync(`assets/meshes/${src}.obj`);
      const bounds = new T.THREE.Box3().setFromObject(obj), upperStart = bounds.min.z + (bounds.max.z - bounds.min.z) * .55;
      // These OBJ scans have no textures: retain the study's floor/glass colors, softened toward gray.
      const gray = new T.THREE.Color('#C8C7C2'), floor = new T.THREE.Color('#D6B089').lerp(gray, .4), glass = new T.THREE.Color('#9CC2E6').lerp(gray, .4), mirror = new T.THREE.Color('#CDD3DA').lerp(gray, .4);
      obj.traverse(m => { if (m.isMesh) {
        m.geometry.deleteAttribute('normal'); m.geometry = T.mergeVertices(m.geometry); m.geometry.computeVertexNormals(); cleanScanTop(m.geometry, upperStart);
        const normals = m.geometry.attributes.normal, colors = new Float32Array(normals.count * 3), special = /^window/.test(m.name) ? glass : /^mirror/.test(m.name) ? mirror : null;
        for (let i = 0; i < normals.count; i++) (special || (normals.getZ(i) > .7 ? floor : gray)).toArray(colors, i * 3);
        m.geometry.setAttribute('color', new T.THREE.BufferAttribute(colors, 3)); m.material = T.shell;
      } });
      obj.rotation.x = -Math.PI / 2; // scans are Z-up
      T.cache[src] = obj;
    }
    if (want3D !== src) return; // a newer selection won
    if (T.cur !== obj) { if (T.cur) T.scene.remove(T.cur); T.scene.add(obj); T.cur = obj; frame3D(); }
    T.blocks.clear();
    if (S.stage === 'furnish') pieces(d).forEach(q => {
      const hgt = C[q.it.c].h * .3048, mesh = new T.THREE.Mesh(new T.THREE.BoxGeometry(q.w / 100, hgt, q.h / 100), S.sel === q.key ? T.mats.on : T.mats[q.it.st]);
      mesh.position.set(d.plan.mesh[0] + (q.cx - d.plan.ink[0]) / 100, hgt / 2 + .02, -(d.plan.mesh[1] - (q.cy - d.plan.ink[1]) / 100));
      mesh.userData.key = q.key; T.blocks.add(mesh);
    });
    box.dataset.state = 'ready';
    state.textContent = '';
  } catch { box.dataset.state = 'error'; state.textContent = '3D model unavailable'; }
}

let vmap, vpins = [];
function paintMapView(p) {
  if (!window.maplibregl) return;
  if (!vmap) vmap = new maplibregl.Map({ container: 'vmap', style: 'https://tiles.openfreemap.org/styles/positron', center: [p.lon, p.lat], zoom: 14, attributionControl: { compact: true } });
  vpins.forEach(m => m.remove());
  vpins = finalists().map(id => { const q = P[id], wrap = document.createElement('div'), el = document.createElement('button');
    el.type = 'button'; el.className = 'pin' + (id === p.id ? ' on' : ''); el.textContent = q.short; el.onclick = () => selectFinalist(id); wrap.append(el);
    return new maplibregl.Marker({ element: wrap }).setLngLat([q.lon, q.lat]).addTo(vmap); });
  vmap.resize(); vmap.flyTo({ center: [p.lon, p.lat], zoom: 14.5, duration: fast ? 0 : 1200 });
}
function renderViewer() {
  const furnish = S.stage === 'furnish', p = P[furnish ? S.chosen : S.finalist]; if (!p || S.stage === 'discover') return;
  const d = D[p.src], views = furnish ? ['floorplan', 'dollhouse', 'photos'] : ['dollhouse', 'floorplan', 'aerial', 'map', 'photos'];
  if (!views.includes(S.view)) S.view = views[0];
  $('#views').innerHTML = views.map(v => `<button type="button" data-view="${v}" aria-pressed="${v === S.view}">${VIEW[v]}</button>`).join('');
  $$('#viewer .layer').forEach(l => { l.hidden = l.id !== 'v-' + S.view; });
  const hue = p.hue ? `hue-rotate(${p.hue}deg)` : '';
  if (S.view === 'dollhouse') paint3D(p, d);
  if (S.view === 'floorplan') paintPlan(p, d);
  if (S.view === 'aerial') { const img = $('#v-aerial img'); img.src = asset(p.src, `vexcel-${S.dir}.jpg`); img.style.filter = hue; $$('.dirs button').forEach(b => b.setAttribute('aria-pressed', b.dataset.dir === S.dir)); }
  if (S.view === 'map') paintMapView(p);
  if (S.view === 'photos') {
    S.photo = clamp(S.photo, 0, d.photos.length - 1);
    const img = $('#v-photos > img'); img.src = asset(p.src, d.photos[S.photo]); img.style.filter = hue; img.alt = `${p.address}, photo ${S.photo + 1}`;
    $('.thumbs').innerHTML = d.photos.map((f, i) => `<button type="button" data-photo="${i}" aria-pressed="${i === S.photo}" aria-label="Photo ${i + 1}"><img src="${asset(p.src, f)}" alt="" style="filter:${hue || 'none'}"></button>`).join('');
  }
  const issue = !furnish && d.issues.find(i => i.id === S.issue), ev = $('#evidence');
  ev.hidden = !(issue?.show && issue.show.view === S.view && (issue.show.photo == null || issue.show.photo === S.photo));
  if (!ev.hidden) ev.textContent = issue.show.note;
  $('#vextra').innerHTML = furnish ? '' : '<button type="button" class="btn solid" data-act="explore">Try a layout</button>';
}
function selectPiece(key) { S.sel = S.sel === key ? null : key; if (S.sel) S.tab = 'parts'; renderFurnish(); save(); }

/* ----------------------------------------------------------------- shell */

const TURN = { discover: discoverTurn, compare: compareTurn, furnish: furnishTurn };
const HINT = { discover: 'Describe what you need', compare: 'Ask about these spaces', furnish: 'Describe the space you want' };
function renderNav() {
  const can = { discover: true, compare: S.saved.length >= 2, furnish: !!S.chosen };
  $$('.stages button').forEach(b => { b.disabled = !can[b.dataset.go]; b.toggleAttribute('aria-current', b.dataset.go === S.stage); });
}
function go(stage) {
  if (stage === 'compare' && S.saved.length < 2) return;
  if (stage === 'compare' && !S.saved.includes(S.finalist)) S.finalist = S.saved[0];
  if (stage === 'furnish' && !S.chosen) return;
  S.stage = stage; document.body.dataset.stage = stage;
  $$('.stage').forEach(s => { s.hidden = s.id !== stage; });
  $(`.chat-slot[data-slot=${stage}]`).append($('#chat'));
  if (stage !== 'discover') $(`.viewer-slot[data-slot=${stage}]`).append($('#viewer'));
  $('#say').placeholder = HINT[stage];
  if (stage === 'discover') { renderDiscover(); map?.resize(); if (S.step >= STEPS.length) offer(); }
  if (stage === 'compare') {
    S.view = 'dollhouse'; renderCompare();
    if (!S.seenCompare) {
      S.seenCompare = true;
      const cs = finalists().map(id => ({ p: P[id], ...costs(P[id]) })), lo = cs.reduce((x, y) => (y.total < x.total ? y : x)), hi = cs.reduce((x, y) => (y.total > x.total ? y : x));
      a(`${hi.p.short} needs about ${money(hi.d.build)} of work before it can open. ${lo.p.short} needs about ${money(lo.d.build)}.`);
    }
    replies(['What is the biggest risk here?', 'Ask the broker']);
  }
  if (stage === 'furnish') {
    S.tab = null; S.view = 'floorplan'; renderFurnish();
    if (S.seenFurnish !== S.chosen) { S.seenFurnish = S.chosen; a(`Tell me how ${P[S.chosen].suite} should feel, and I will furnish it.`); }
    replies(furnishReplies());
  }
  scrollLog(); save();
}

document.addEventListener('click', e => {
  const t = e.target.closest('button, .frame, tr[data-key]'); if (!t) return;
  const act = t.dataset.act, frame = t.closest('.frame');
  if (t.parentElement?.id === 'replies') return send(t.textContent);
  if (t.dataset.go) return go(t.dataset.go);
  if (t.dataset.edit) return editBrief(t.dataset.edit);
  if (t.dataset.open) return selectSpace(t.dataset.open);
  if (act === 'save' || act === 'dismiss') {
    const id = frame?.dataset.id;
    if (id) (act === 'save' ? toggleSave : dismiss)(id);
    return;
  }
  if (t.dataset.finalist) return selectFinalist(t.dataset.finalist);
  if (t.dataset.view) { S.view = t.dataset.view; renderViewer(); return save(); }
  if (act?.startsWith('dollhouse-') && T?.cur) {
    if (act === 'dollhouse-reset') frame3D();
    else { T.camera.zoom = clamp(T.camera.zoom * (act === 'dollhouse-in' ? 1.25 : .8), T.controls.minZoom, T.controls.maxZoom); T.camera.updateProjectionMatrix(); }
    return;
  }
  if (t.dataset.dir) { S.dir = t.dataset.dir; return renderViewer(); }
  if (t.dataset.photo) { S.photo = +t.dataset.photo; return renderViewer(); }
  if (act === 'ask') return draft(D[P[S.finalist].src].issues.find(i => i.id === t.dataset.issue));
  if (act === 'send-draft') { const msg = t.closest('.msg'), i = $$('.msg', log).indexOf(msg); t.outerHTML = '<span class="sent">Sent</span>'; if (S.msgs[i]) S.msgs[i].html = msg.innerHTML; return save(); }
  if (act === 'explore') return explore(false);
  if (act === 'commit') { S.committed = true; a(`Done. ${P[S.chosen].address} is your space.`); return renderFurnish(); }
  if (act === 'show-chat') { S.tab = null; renderPane(); scrollLog(); $('#say').focus({ preventScroll: true }); return save(); }
  if (t.dataset.tab) { S.tab = S.tab === t.dataset.tab ? null : t.dataset.tab; renderPane(); scrollLog(); return save(); }
  if (t.dataset.place) return mutate(() => { const room = place(t.dataset.place); if (room) S.sel = `${t.dataset.place}|${room.id}`; });
  if (t.dataset.remove) { const [c, r, st] = t.dataset.remove.split('|'); return mutate(() => { S.placed = S.placed.filter(i => !(i.c === c && i.r === r && i.st === st)); }); }
  if (act === 'accept') return mutate(() => S.placed.forEach(i => { i.st = 'placed'; }));
  if (act === 'clear-room') { S.room = null; return renderFurnish(); }
  if (t.dataset.room) { S.room = S.room === t.dataset.room ? null : t.dataset.room; S.sel = null; return renderFurnish(); }
  if (t.dataset.key) return selectPiece(t.dataset.key);
});
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && S.stage === 'discover' && S.open) selectSpace(null);
});
// Opening an issue points the viewer at its evidence.
$('#findings').addEventListener('toggle', e => { const el = e.target; if (el.open) showIssue(el.dataset.issue, false); else if (S.issue === el.dataset.issue) { S.issue = null; renderViewer(); } }, true);
$('#composer').addEventListener('submit', e => { e.preventDefault(); const v = $('#say').value; $('#say').value = ''; send(v); });
// Property summaries share the item tooltip's cursor motion and dismissal behavior.
const SPACE_NOTES = {
  'mateo-109': {
    description: 'An open Arts District shell gives you flexibility to shape the studio. Treatment rooms and plumbing would need building before opening.',
    pros: ['Flexible open layout', 'Lowest asking rent of the three scanned spaces'],
    cons: ['New rooms and sink runs needed', 'Electrical capacity unknown'],
  },
  'sixth-110b': {
    description: 'Two back rooms in this former restaurant could suit treatments. The kitchen needs removing and the dining area needs new partitions.',
    pros: ['Two rooms could be reused', 'Existing kitchen plumbing'],
    cons: ['Kitchen strip-out needed', 'NNN charges add to rent'],
  },
  'beverly-720': {
    description: 'Three rooms and existing sink cabinets could make this a lighter studio conversion. After-hours air and less usable space are the main trade-offs.',
    pros: ['Three potential treatment rooms', 'Sink cabinets already present'],
    cons: ['After-hours air costs extra', 'Usable area below listed area'],
  },
};
function propertyTip(p) {
  const notes = S.req.business === 'Facial studio' && SPACE_NOTES[p.id] || {
    description: `This ${p.use.toLowerCase()} space offers ${p.sqft.toLocaleString()} SF in ${p.hood}. The ${p.lease.toLowerCase()} lease needs a review of running costs and permitted use.`,
    pros: [S.req.rent && p.rent <= S.req.rent ? 'Asking rent within your limit' : `${p.sqft.toLocaleString()} SF listed`, `${p.hood} location`],
    cons: [p.lease === 'Triple net' ? 'NNN costs on top of rent' : 'Lease extras need confirming', 'Layout and utilities not assessed'],
  };
  const list = (label, items) => `<div><b>${label}</b><ul>${items.map(t => `<li>${esc(t)}</li>`).join('')}</ul></div>`;
  return `${p.id !== p.src ? '<span class="space-origin">Demo property</span>' : ''}<p class="space-description">${esc(notes.description)}</p><div class="space-tradeoffs">${list('Pros', notes.pros)}${list('Cons', notes.cons)}</div>`;
}
const tipElement = target => target?.closest?.('[data-tip], [data-property]');
const tip = $('#tip');
const tipReducedMotion = matchMedia('(prefers-reduced-motion: reduce)');
let tipTarget, tipFromPointer = false, tipFollowsPointer = false, tipFrame = 0, tipTime = 0;
let tipX = 0, tipY = 0, tipToX = 0, tipToY = 0, tipWidth = 0, tipHeight = 0;
function hideTip() {
  tip.hidden = true; tipTarget?.removeAttribute('aria-describedby'); tipTarget = null;
  cancelAnimationFrame(tipFrame); tipFrame = 0;
}
function drawTip(time) {
  tipFrame = 0;
  if (!tipTarget?.isConnected) return hideTip();
  // Time-based lerp keeps the same short trailing feel at any refresh rate.
  const mix = tipReducedMotion.matches ? 1 : 1 - Math.exp(-(time - tipTime) / 65);
  tipTime = time;
  tipX += (tipToX - tipX) * mix; tipY += (tipToY - tipY) * mix;
  const moving = Math.abs(tipToX - tipX) + Math.abs(tipToY - tipY) > .2;
  if (!moving) { tipX = tipToX; tipY = tipToY; }
  tip.style.transform = `translate3d(${tipX}px, ${tipY}px, 0)`;
  if (moving) tipFrame = requestAnimationFrame(drawTip);
}
function positionTip(pointer, snap = false) {
  const right = Math.max(12, innerWidth - tipWidth - 12), bottom = Math.max(12, innerHeight - tipHeight - 12);
  if (pointer) {
    tipToX = pointer.clientX + 18;
    tipToY = pointer.clientY + 18;
    if (tipToX > right) tipToX = pointer.clientX - tipWidth - 18;
    if (tipToY > bottom) tipToY = pointer.clientY - tipHeight - 18;
  } else {
    const r = tipTarget.getBoundingClientRect();
    tipToX = r.left + r.width / 2 - tipWidth / 2;
    tipToY = r.top - tipHeight - 10;
    if (tipToY < 12) tipToY = r.bottom + 10;
  }
  tipToX = clamp(tipToX, 12, right); tipToY = clamp(tipToY, 12, bottom);
  // A new card appears beside its item; only subsequent cursor travel trails.
  if (snap) { tipX = tipToX; tipY = tipToY; }
  tipX = clamp(tipX, 12, right); tipY = clamp(tipY, 12, bottom);
  if (!tipFrame) { tipTime = performance.now(); drawTip(tipTime); }
}
function showTip(e) {
  const t = tipElement(e.target); if (!t || e.pointerType === 'touch') return;
  if (e.type === 'pointerover' && tipElement(e.relatedTarget) === t) return;
  const wasHidden = tip.hidden;
  tipTarget?.removeAttribute('aria-describedby'); tipTarget = t; t.setAttribute('aria-describedby', 'tip');
  if (e.type === 'pointerover') tipFromPointer = true;
  else if (e.type === 'focusin') tipFromPointer = !t.matches(':focus-visible');
  const item = C[t.dataset.place || t.dataset.key?.split('|')[0]];
  const property = P[t.dataset.property];
  tip.classList.toggle('item-tip', !!item);
  tip.classList.toggle('space-tip', !!property);
  tip.innerHTML = property ? propertyTip(property) : item ? `<strong>${esc(item.name)}</strong><span class="tip-meta">${esc(item.by)} · ${esc(item.lead)} lead time</span><dl class="tip-facts"><div><dt>Footprint</dt><dd>${item.w} × ${item.d} ft</dd></div><div><dt>Each</dt><dd class="tip-price">${money(item.price)}</dd></div></dl>` : esc(t.dataset.tip);
  tip.hidden = false;
  tipWidth = tip.offsetWidth; tipHeight = tip.offsetHeight;
  tipFollowsPointer = !!(item || property) && e.type === 'pointerover' && !tipReducedMotion.matches;
  positionTip(tipFollowsPointer ? e : null, wasHidden || !tipFollowsPointer);
}
['pointerover', 'focusin'].forEach(ev => document.addEventListener(ev, showTip));
document.addEventListener('pointermove', e => {
  if (tipTarget && tipFollowsPointer && tipElement(e.target) === tipTarget) positionTip(e);
});
document.addEventListener('pointerout', e => {
  if (!tipTarget?.contains(e.target) || tipTarget.contains(e.relatedTarget)) return;
  if (!tipFromPointer && tipTarget === document.activeElement) {
    tipFollowsPointer = false; positionTip(null, true);
  } else hideTip();
});
['click', 'focusout'].forEach(ev => document.addEventListener(ev, hideTip, true));
document.addEventListener('scroll', e => {
  if (!tipTarget || (e.target !== document && !e.target.contains?.(tipTarget))) return;
  if (tipTarget === document.activeElement) showTip({ target: tipTarget }); else hideTip();
}, true);
document.addEventListener('keydown', e => { if (e.key === 'Escape') hideTip(); });
window.addEventListener('resize', hideTip);
window.addEventListener('blur', hideTip);
tipReducedMotion.addEventListener('change', () => { if (tipTarget) showTip({ target: tipTarget }); });

// Demo shortcuts: workspace.html#searched, #compare or #furnish replay the script to that point.
async function seed(to) {
  fast = true;
  for (const t of ['A facial and skincare studio', 'Downtown or the Westside', 'Up to $5,500', 'Three, plus reception', 'By March']) await send(t);
  if (to !== 'searched') { ['mateo-109', 'sixth-110b', 'beverly-720'].forEach(toggleSave); await send('Look into them'); }
  if (to === 'furnish') { S.finalist = 'beverly-720'; explore(false); await send('Warm and minimal, three treatment rooms, under $25k'); }
  fast = false; fitKey = ''; renderMap();
  history.replaceState(null, '', location.pathname); // a reload keeps what you did next
}

const start = location.hash.slice(1); // #reset clears saved state
if (['searched', 'compare', 'furnish', 'reset'].includes(start)) S = fresh();
if (start === 'reset') history.replaceState(null, '', location.pathname);
S.msgs.forEach(m => log.append(bubble(m)));
replies(S.replies);
initMap();
go(S.stage);
if (!S.msgs.length) ask();
if (['searched', 'compare', 'furnish'].includes(start)) seed(start);
