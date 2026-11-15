// Opt-in connection from the three-stage workspace to the Second Site backend (the Rust service, which
// forwards the chat to the Python app). Loaded by workspace.js only when the page URL carries ?live=1;
// without it this file is never imported and the page stays the scripted demo.
//
//   workspace.html?live=1                       backend at http://localhost:8000
//   workspace.html?live=1&api=http://host:port  another backend origin
//
// What it sends: the dev sign-in, the project and its business profile, the property search, layout
// generation and reads. Never outreach, calls, approval decisions or payments.
// Money from the backend is integer minor units (cents) and geometry is metres; both are converted for
// display only, in toDollars() and toSqft(). Pure mapping functions are exported for backend.test.mjs.

const SQFT_PER_SQM = 10.7639104; // exact to the precision shown (1 m = 3.28084 ft)
export const toDollars = cents => (Number.isInteger(cents) ? cents / 100 : null);
export const toSqft = sqm => (typeof sqm === 'number' ? Math.round(sqm * SQFT_PER_SQM) : null);
const cap = s => s.charAt(0).toUpperCase() + s.slice(1);
const words = s => String(s || '').toLowerCase().replace(/_/g, ' ');
// The page inserts finding, room and item text as HTML, so backend text is escaped here.
export const html = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
// Where a record comes from, from the backend's own labels. Only FIXTURE_SYNTHETIC (or provider
// "fixture") is demo data. FIXTURE_REAL is a genuine record from a dated snapshot (the CoStar data kit),
// never "demo", "synthetic" or "live". LIVE is a live provider answer. Anything else stays "not stated".
export function sourceOf(provider, origin) {
  if (origin === 'FIXTURE_SYNTHETIC' || provider === 'fixture') return 'synthetic';
  if (origin === 'FIXTURE_REAL') return 'snapshot';
  if (origin === 'LIVE') return 'live';
  return 'unknown';
}
const PROVIDER = { costar: 'CoStar', matterport: 'Matterport', vexcel: 'Vexcel' };
const providerName = p => PROVIDER[p] || (p ? cap(String(p)) : 'Provider not stated');
export function listingSource(cl = {}) {
  const kind = sourceOf(cl.provider, cl.data_origin);
  const label = kind === 'synthetic' ? 'Demo data'
    : kind === 'snapshot' ? (cl.provider === 'costar' ? 'CoStar listing (data-kit snapshot)' : `${providerName(cl.provider)} record (data snapshot)`)
    : kind === 'live' ? `${providerName(cl.provider)} listing (live)` : 'Source not stated';
  return { kind, label };
}
// workspace.js puts p.address, p.short and p.suite into chat HTML unescaped (its lines 396, 400, 675,
// 681, 711) and escapes them elsewhere; text without < and > is safe both ways.
const plain = s => String(s ?? '').replace(/[<>]/g, '');
// workspace.js (compareTurn) builds `new RegExp(<last word of p.short>)`: a bracket or quantifier in a
// unit ("(Rear)", "*") would throw there, so `short` carries none.
const reSafe = s => s.replace(/[\\^$*+?()[\]{}|]/g, '').replace(/\s+/g, ' ').trim();
// An image link the page puts into src="…" unescaped: http(s) only, serialized by the URL parser
// (which percent-encodes quotes and angle brackets).
export const safeUrl = u => { try { const x = new URL(String(u)); return /^https?:$/.test(x.protocol) ? x.href : null; } catch { return null; } };
const clip = (s, n) => (s.length > n ? s.slice(0, n - 1).trimEnd() + '…' : s);

/* ------------------------------------------------------------ configuration */

export function config(search = globalThis.location?.search || '') {
  const q = new URLSearchParams(search);
  return { live: q.get('live') === '1', api: (q.get('api') || 'http://localhost:8000').replace(/\/+$/, '') };
}

/* ------------------------------------------------- pure mapping: stage 1 */

// The rooms answer is known only as a whole number above zero. The page searches after every answer;
// before this one the link neither creates the project nor searches (no room count is invented).
export const roomsKnown = (req = {}) => Number.isInteger(req.rooms) && req.rooms > 0;
// The search stage's chat answers (the page's S.req) -> the project's business profile.
// Only what the person said is sent. Rooms beyond treatment and reception, capital and reserve were not
// asked, so they are left out (the backend keeps them unknown). The contract requires business_type
// (non-empty), current_location_count, target_market and space_requirements; rent, opening and areas are
// optional and are left out or null when not answered.
export function profileFromAnswers(req = {}) {
  const facial = req.business === 'Facial studio';
  const type = facial ? 'facial_studio' : String(req.business || 'unspecified').toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_|_$/g, '') || 'unspecified';
  const profile = {
    business_type: type,
    business_description: req.business ? `${req.business} (from the workspace chat)` : null,
    current_location_count: 1,
    target_market: { city: 'Los Angeles', state: 'CA' },
    monthly_rent_limit: Number.isFinite(req.rent) && req.rent > 0 ? Math.round(req.rent) * 100 : null,
    source_references: [{ field: '*', source: 'workspace chat answers', confirmation_status: 'CONFIRMED', note: 'Typed or chosen by the person in the search stage' }],
    confirmation_status: 'CONFIRMED',
  };
  profile.space_requirements = roomsKnown(req) ? { required_rooms: { treatment: req.rooms, reception: 1 } } : {}; // required by the contract
  return profile;
}
export function searchBody(req = {}) {
  return { city: 'Los Angeles', state: 'CA', limit: 20, max_monthly_rent: Number.isFinite(req.rent) && req.rent > 0 ? Math.round(req.rent) * 100 : null };
}

// The page's four areas, derived from the backend's coordinates (the backend has no such field).
// Anything outside these boxes has no area and only appears when the person chose "Anywhere".
export function zoneOf(lat, lon) {
  if (typeof lat !== 'number' || typeof lon !== 'number') return null;
  if (lat > 34.135) return 'valley';
  if (lon < -118.37) return 'westside';
  if (lon > -118.225) return 'eastside';
  if (lat < 34.065 && lon > -118.29) return 'downtown';
  return null;
}
const DIRECTIONS = new Set(['N', 'S', 'E', 'W']);
const SUFFIX = new Set(['ST', 'AVE', 'BLVD', 'DR', 'RD', 'WAY', 'PL', 'LN', 'CT', 'PKWY']);
export function shortAddress(street) {
  const t = String(street || '').split(/\s+/).filter(Boolean);
  const keep = t.filter((w, i) => !(i > 0 && (DIRECTIONS.has(w.toUpperCase()) || SUFFIX.has(w.toUpperCase()))));
  return keep.slice(0, 3).map(w => (/^\d/.test(w) ? w : cap(w.toLowerCase()))).join(' ');
}

// The listing's Matterport scans: the suite's own scan (measured geometry) first.
export function scansOf(rec) {
  const all = (rec.interior_assets || []).filter(a => a.provider === 'matterport' || a.asset_type === 'matterport_model');
  const suite = all.find(a => a.has_measured_geometry) || null;
  return {
    suite: suite ? { asset_id: suite.asset_id, label: suite.label ?? null, data_origin: suite.data_origin, area_sqm: suite.area_sqm ?? null } : null,
    all: all.map(a => ({ asset_id: a.asset_id, label: a.label ?? null, data_origin: a.data_origin, matches_listing_unit: a.matches_listing_unit ?? null, has_measured_geometry: !!a.has_measured_geometry })),
    text: suite ? (sourceOf(suite.provider, suite.data_origin) === 'synthetic' ? '3D scan of this suite (demo data)' : '3D scan of this suite') : all.length ? 'Building scans only' : 'No 3D scan',
  };
}
// The listing's images: its own photo (provider CDN link, loaded by the browser only), else the
// Vexcel aerial image the backend serves without a token, else none.
export function imagesOf(rec, api = '') {
  const cl = rec.commercial_listing || {};
  const photo = (cl.photo_urls || []).map(safeUrl).find(Boolean) || null;
  const ax = (rec.exterior_assets || []).find(a => a.provider === 'vexcel' && sourceOf(a.provider, a.data_origin) === 'live');
  const aerial = ax && (api || /^https?:\/\//.test(String(ax.url || ''))) ? {
    url: safeUrl(/^https?:\/\//.test(String(ax.url || '')) ? ax.url : api + (ax.url || `/api/v1/media/vexcel/ortho/${encodeURIComponent(rec.property_id)}`)),
    captured_at: ax.captured_at || null,
    caption: `Aerial image, Vexcel, ${ax.captured_at ? `captured ${ax.captured_at}` : 'capture date not stated'}`,
  } : null;
  const photoCaption = photo ? `${cl.provider === 'costar' ? 'CoStar' : providerName(cl.provider)} photo` : null;
  return { photo, photoCaption, aerial, files: [photo ? 'listing-photo' : null, aerial ? 'aerial-vexcel' : null].filter(Boolean) };
}
// One backend property record -> one POOL entry.
// `src`: a synthetic (demo) listing gets "live-<id>", so the page's own "Demo property" label
// (id !== src) shows for it. A real listing (CoStar kit snapshot or live) gets src = id, so that label
// does not show. Here src is never one of the page's local asset folders; only withLocal() below
// sets it to a slug, for a listing whose own suite scan has that slug's model id. asset() answers for
// every backend listing, so no local photo, scan or plan of another listing is used.
export function poolEntry(rec, api = '') {
  const id = rec.property_id, ident = rec.property_identity || {}, cl = rec.commercial_listing || {};
  const source = listingSource(cl), fixture = source.kind === 'synthetic';
  const unit = plain(ident.unit_or_suite).replace(/\s+/g, ' ').trim();
  const rentKnown = Number.isInteger(cl.asking_rent_monthly), sqft = toSqft(cl.available_area_sqm);
  const img = imagesOf(rec, api), scans = scansOf(rec);
  // The page prints `suite · <sqft> SF` and `money(rent)`; unknown rent and area are said here in words
  // (the page itself prints $0 and 0 SF for them; see the report).
  const suite = [unit ? clip(unit, 32) : 'Unit not stated', source.label, rentKnown ? null : 'rent not stated', sqft == null ? 'area not stated' : null].filter(Boolean).join(' · ');
  return {
    id, src: fixture ? 'live-' + id : id, photo: img.files[0] || 'none',
    address: plain(ident.street_address || ident.normalized_address) || 'Address not stated',
    short: reSafe(plain(shortAddress(ident.street_address))) || 'Address not stated',
    suite,
    hood: ident.city || 'Los Angeles', zone: zoneOf(ident.latitude, ident.longitude),
    rent: toDollars(cl.asking_rent_monthly) ?? 0, rentKnown,
    sqft: sqft ?? 0, sqftKnown: sqft != null,
    use: cl.property_type || 'Use not stated', lease: cl.lease_service_type || 'Lease type not stated',
    broker: cl.broker_contact?.company || cl.brokerage_company || 'Broker not stated',
    lat: ident.latitude, lon: ident.longitude,
    live: {
      property_id: id, fixture, source: source.kind, source_label: source.label, provider: cl.provider, data_origin: cl.data_origin,
      disclaimer: cl.disclaimer || null, listing_id: cl.provider_listing_id, unit: unit || null, source_record_date: cl.source_record_date || null,
      photo_url: img.photo, photo_caption: img.photoCaption, aerial: img.aerial, images: img.files,
      scan: scans.suite, scans: scans.all, scan_text: scans.text, interior: interiorOf(rec),
    },
  };
}
/* ------------------------- the interior of a live listing: Matterport tour and backend media (web-link-4) */

// A Matterport model id as the show page takes it. Nothing else goes into a tour or media URL.
const MP_ID = /^[A-Za-z0-9]{11}$/;
// The scan the viewer shows: the suite's own (measured geometry, not marked as another unit's), else the
// first other scan in the backend's order, which is another space in the same building. LIVE Matterport
// scans of a listing that is not demo data only; anything else gives no interior ("No 3D scan").
export function interiorOf(rec) {
  if (listingSource(rec.commercial_listing || {}).kind === 'synthetic') return null;
  const all = (rec.interior_assets || []).filter(a => a && a.provider === 'matterport' && a.data_origin === 'LIVE' && typeof a.asset_id === 'string' && MP_ID.test(a.asset_id));
  const own = all.find(a => a.has_measured_geometry === true && a.matches_listing_unit !== false), a = own || all[0];
  if (!a) return null;
  const label = clip(plain(a.label).replace(/\s+/g, ' ').trim(), 60);
  const space = own ? 'this suite' : `another space in this building: ${label || 'no label given'}`;
  return {
    asset_id: a.asset_id, label: label || null, scope: own ? 'suite' : 'building',
    tour_text: `3D tour of ${space} · Matterport tour (live)`,
    mesh_text: `3D scan of ${space} · Matterport scan (live, fetched by the backend)`,
    photo_text: `Interior still of ${space} · Matterport scan (live, fetched by the backend)`,
  };
}
export const tourUrl = id => (typeof id === 'string' && MP_ID.test(id) ? `https://my.matterport.com/show/?m=${id}&play=1&qs=1&brand=0&help=0&dh=1` : null);
const mediaUrl = (id, file) => `${backend?.api ?? config().api}/api/v1/media/matterport/${id}/${file}`;
const MEDIA_PHOTO = /^mp-live-photo-(\d{1,2})$/;
// One manifest request per scan for the page's lifetime; a 404, an error answer or a network error is
// cached as null ("no interior from the backend") and is never reported.
const manifests = new Map();
export function manifest(id) {
  if (typeof id !== 'string' || !MP_ID.test(id)) return Promise.resolve(null);
  if (!manifests.has(id)) manifests.set(id, (async () => {
    try {
      const r = await globalThis.fetch(mediaUrl(id, 'manifest.json'));
      if (!r.ok) return null;
      const m = await r.json();
      return m && m.asset_id === id && m.provider === 'matterport' && m.data_origin === 'LIVE' ? m : null;
    } catch { return null; }
  })());
  return manifests.get(id);
}
export function forgetManifests() { manifests.clear(); } // tests only
const photoCount = m => (Number.isInteger(m?.photos?.count) && m.photos.count > 0 ? Math.min(m.photos.count, 12) : 0);
// The extra viewer view a live listing gets, and its button label.
export const views = p => (p?.live?.interior ? ['tour'] : []);
export const viewName = v => (v === 'tour' ? '3D tour' : v);
// What paint3D (workspace.js) loads for a live listing: the saved copy of a matched suite (web-link-3,
// unchanged, blocks as before), else the backend's mesh of the chosen scan (no furniture blocks: the
// layout plan is in the backend geometry's frame, whose fit to the mesh nobody has checked), else no mesh.
export async function dollhouse(p) {
  const L = p?.live;
  if (!L) return null;
  if (L.local) return { key: p.src, mesh: `assets/meshes/${p.src}.obj`, poster: `assets/dollhouses/${p.src}.png`, caption: '3D scan of this suite · Matterport (saved copy)', blocks: true };
  const it = L.interior;
  if (!it) return { key: p.src, mesh: null, poster: null, text: 'No 3D scan', blocks: false };
  const m = await manifest(it.asset_id);
  if (!m?.mesh?.available) return { key: p.src, mesh: null, poster: null, text: 'No 3D model from the backend for this scan. The 3D tour view shows the Matterport tour (live).', blocks: false };
  // The scan's own still as the poster for the suite's scan only (it shows with no caption while loading).
  return { key: 'matterport:' + it.asset_id, mesh: mediaUrl(it.asset_id, 'mesh.obj'), poster: it.scope === 'suite' && m.image?.available ? mediaUrl(it.asset_id, 'image.jpg') : null, caption: it.mesh_text, blocks: false };
}
// The caption of one entry of the Photos view (null: none needed, the neutral image says it).
export function photoCaption(p, f) {
  const L = p?.live;
  if (!L) return null;
  if (L.local?.captions[f]) return L.local.captions[f];
  if (MEDIA_PHOTO.test(f) && L.interior) return L.interior.photo_text;
  if (f === 'listing-photo') return L.photo_caption;
  if (f === 'aerial-vexcel') return L.aerial?.caption ?? null;
  return null;
}
// Once the viewer is opened for a listing with a chosen scan and no saved copy: its manifest, then its
// stills after the listing's own CoStar photo. Nothing is requested before.
function mediaPhotos(p, d) {
  const it = p.live?.interior;
  if (!it || p.live.local || !d || d.mediaFor === it.asset_id) return;
  d.mediaFor = it.asset_id;
  manifest(it.asset_id).then(m => {
    const n = photoCount(m);
    if (!n || page?.D[p.src] !== d) return;
    const names = Array.from({ length: n }, (_, i) => `mp-live-photo-${i + 1}`);
    const own = d.photos.filter(f => f !== 'none'), at = own.indexOf('listing-photo') + 1;
    d.photos = [...own.slice(0, at), ...names, ...own.slice(at)];
    const S = page.S(), shown = page.P[S.stage === 'furnish' ? S.chosen : S.finalist];
    if (shown === p && S.view === 'photos') page.renderViewer?.();
  });
}
// Called by workspace.js at the end of renderViewer(): the tour iframe (created only while the tour view
// of a listing is open, one at a time, removed otherwise) and the captions of the tour and the photos.
export function viewer(view, p, d) {
  if (view === 'tour' || view === 'photos' || view === 'dollhouse') mediaPhotos(p, d);
  const doc = globalThis.document, layer = doc?.getElementById('v-tour');
  if (!layer) return;
  const it = p?.live?.interior, url = view === 'tour' && it ? tourUrl(it.asset_id) : null;
  const holder = layer.querySelector('.live-tour'), old = holder.querySelector('iframe');
  if (old && old.dataset.src !== url) old.remove();
  if (url && !holder.querySelector('iframe')) {
    const f = doc.createElement('iframe');
    f.dataset.src = url; f.src = url; f.title = it.tour_text;
    f.setAttribute('allow', 'fullscreen; xr-spatial-tracking'); f.setAttribute('allowfullscreen', ''); f.setAttribute('referrerpolicy', 'no-referrer');
    f.style.cssText = 'display:block;width:100%;height:100%;border:0';
    holder.append(f);
  }
  layer.querySelector('.live-caption').textContent = url ? it.tour_text : '';
  const cap = doc.getElementById('v-photos')?.querySelector('.live-caption');
  if (cap) {
    const text = view === 'photos' && d ? photoCaption(p, d.photos[clampIndex(page?.S().photo, d.photos.length)]) : null;
    cap.textContent = text || ''; cap.hidden = !text;
    const img = doc.querySelector('#v-photos > img');
    if (text && img) img.alt = `${p.address}: ${text}`;
  }
}
const clampIndex = (i, n) => Math.max(0, Math.min(n - 1, Number.isInteger(i) ? i : 0));
// The viewer's extra elements, made once by bind() in a browser: the tour layer (its caption above the
// tour, not over it) and a caption over the Photos view.
function viewerElements() {
  const doc = globalThis.document, stage = doc?.querySelector('#viewer .vstage');
  if (!stage || doc.getElementById('v-tour')) return;
  const layer = doc.createElement('div'); layer.className = 'layer'; layer.id = 'v-tour'; layer.hidden = true;
  const col = doc.createElement('div'); col.style.cssText = 'position:absolute;inset:0;display:flex;flex-direction:column';
  const cap = doc.createElement('p'); cap.className = 'live-caption'; cap.style.cssText = 'margin:0;padding:8px 12px;font-size:13.5px';
  const holder = doc.createElement('div'); holder.className = 'live-tour'; holder.style.cssText = 'position:relative;flex:1;min-height:0';
  col.append(cap, holder); layer.append(col); stage.append(layer);
  const pc = doc.createElement('p'); pc.className = 'evidence live-caption'; pc.hidden = true;
  doc.getElementById('v-photos')?.append(pc);
}
// Two listings with the same short address (one building, two suites): add the unit so the page's
// chat lines ("<short> needs about …") say which one.
export function disambiguate(pool) {
  const n = {};
  for (const p of pool) n[p.short] = (n[p.short] || 0) + 1;
  for (const p of pool) if (n[p.short] > 1 && reSafe(p.live?.unit || '')) p.short = `${p.short}, ${clip(reSafe(p.live.unit), 20)}`;
  return pool;
}
/* ------------------------------------- local saved copies of a suite's own scan (assets/places.json) */

// The page's asset folder (git-ignored, licensed) holds saved copies for a few suites, one folder per
// slug, indexed by assets/places.json. It is read at run time only; nothing from it is in this file.
// A live listing uses that folder only when the suite's own Matterport scan (measured geometry, the
// backend's own record, not demo data) has exactly the model id of one places.json record.
const SLUG = /^[a-z0-9][a-z0-9-]{0,63}$/, MODEL = /^[A-Za-z0-9]{6,32}$/, FILE = /^[a-z0-9][a-z0-9._-]{0,80}\.(jpe?g|png|webp)$/i;
const LOCAL_CAPTION = { costar: 'CoStar photo (saved copy)', matterport: 'Matterport still (saved copy)', vexcel: 'Vexcel aerial (saved copy)' };
const MAX_PHOTOS = 6; // per source in the Photos view
// places.json -> Map(model id -> { slug, photos, aerial, captions }). Anything malformed is skipped;
// a model id claimed by two records is used by neither.
export function localIndex(places) {
  const out = new Map(), twice = new Set();
  for (const r of Array.isArray(places) ? places : []) {
    if (!r || typeof r !== 'object' || !SLUG.test(String(r.slug)) || !MODEL.test(String(r.matterport_model_id))) continue;
    const slug = r.slug, captions = {}, photos = { costar: [], matterport: [] }, aerial = {};
    for (const im of Array.isArray(r.images) ? r.images : []) {
      const [dir, name, more] = String(im?.file || '').split('/');
      if (dir !== slug || more !== undefined || !FILE.test(name || '') || !LOCAL_CAPTION[im.source] || !name.startsWith(im.source + '-')) continue;
      if (im.kind === 'photo' && im.source !== 'vexcel') { photos[im.source].push(name); captions[name] = LOCAL_CAPTION[im.source]; }
      else if (im.source === 'vexcel' && /^(ortho|oblique-[nesw])$/.test(im.kind) && name === `vexcel-${im.kind}.jpg`) { aerial[im.kind] = name; captions[name] = LOCAL_CAPTION.vexcel; }
    }
    const rec = { slug, photos: [...photos.costar.slice(0, MAX_PHOTOS), ...photos.matterport.slice(0, MAX_PHOTOS)], aerial, captions };
    if (out.has(r.matterport_model_id)) twice.add(r.matterport_model_id); else out.set(r.matterport_model_id, rec);
  }
  for (const m of twice) out.delete(m);
  const slugs = [...out.values()].map(x => x.slug);
  for (const [m, x] of out) if (slugs.filter(s => s === x.slug).length > 1) out.delete(m);
  return out;
}
// The suite's own scan as the backend reports it, or null: a Matterport model with measured geometry,
// not demo data, on a listing that is not demo data, and not marked as another unit's scan.
function ownSuiteScan(rec) {
  if (listingSource(rec.commercial_listing || {}).kind === 'synthetic') return null;
  // Only the scan scansOf() names as the suite's (the one live.scan and the hover text describe).
  const a = (rec.interior_assets || []).filter(x => x.provider === 'matterport' || x.asset_type === 'matterport_model').find(x => x.has_measured_geometry);
  return a && a.provider === 'matterport' && a.has_measured_geometry === true && a.matches_listing_unit !== false
    && sourceOf(a.provider, a.data_origin) !== 'synthetic' && typeof a.asset_id === 'string' ? a : null;
}
export const localMatch = (rec, index) => { const s = ownSuiteScan(rec); return (s && index?.get(s.asset_id)) || null; };
// Give one POOL entry its local files. id and src both become the slug: the page builds the mesh and
// dollhouse paths from src (workspace.js 587, 593) and prints "Demo property" when id !== src. Every
// table the page keys by id or src was emptied by bind(), so the slug finds only backend facts. The
// backend's own id stays in live.property_id (used for the layout request).
export function withLocal(p, local) {
  p.id = p.src = local.slug;
  const first = local.photos[0];
  if (first) p.photo = first;
  p.live.local = local;
  p.live.images = [...local.photos, ...(local.aerial.ortho ? [local.aerial.ortho] : p.live.aerial ? ['aerial-vexcel'] : [])];
  if (!p.live.images.length) p.live.images = ['none'];
  p.live.scan_text = '3D scan of this suite (Matterport)';
  return p;
}
let localLoad = null;
// Tests pass the parsed places.json in; null or anything malformed means no local files; undefined
// makes the next search fetch it again.
export function useLocalPlaces(places) { localLoad = places === undefined ? null : Promise.resolve(localIndex(places)); } // undefined: load again
function localPlaces() {
  if (!localLoad) localLoad = (async () => {
    try {
      if (!globalThis.location?.href) return new Map();
      const r = await globalThis.fetch(new URL('assets/places.json', globalThis.location.href).href, { headers: { accept: 'application/json' } });
      return r.ok ? localIndex(await r.json()) : new Map();
    } catch { return new Map(); } // a clone without the folder: live mode as before
  })();
  return localLoad;
}

// The tooltip text for a live listing (page's SPACE_NOTES shape), from the record only.
export function spaceNotes(rec, api = '', local = null) {
  const cl = rec.commercial_listing || {}, a = rec.infrastructure_assessments || [];
  const count = s => a.filter(x => x.status === s).length;
  const source = listingSource(cl), img = imagesOf(rec, api), scans = scansOf(rec);
  const has = k => Object.values(local?.captions || {}).includes(LOCAL_CAPTION[k]);
  const image = local
    ? `${scans.text} (Matterport): its mesh and dollhouse view${has('matterport') ? ' and stills' : ''} are saved copies from this scan${has('costar') ? `, the photos are saved copies of this listing's CoStar photos` : ''}${has('vexcel') ? ', and the aerial images are saved Vexcel images of the building' : ''} (files on this computer).`
    : [img.photoCaption ? `Image: ${img.photoCaption}.` : null, img.aerial ? `${img.photoCaption ? 'Also: ' : 'Image: '}${img.aerial.caption}.` : null].filter(Boolean).join(' ');
  return {
    description: [`${source.label}.`, local ? null : `${scans.text}.`, image || null, cl.disclaimer, cl.existing_buildout ? `Build-out: ${words(cl.existing_buildout)}.` : null].filter(Boolean).join(' '),
    pros: [scans.suite ? '3D scan of this suite (a layout can be generated)' : null, ...a.filter(x => x.status === 'MEETS').map(x => requirementLabel(x.requirement_id) + ': meets')].filter(Boolean).concat(count('MEETS') ? [] : ['No requirement confirmed as met']).slice(0, 2),
    cons: [scans.suite ? null : `${scans.text}: no layout without a scan of the suite`, count('UNKNOWN') ? `${count('UNKNOWN')} requirement(s) unknown` : null, count('MAY_REQUIRE_WORK') ? `${count('MAY_REQUIRE_WORK')} may need work` : null, ...(rec.missing_information || [])].filter(Boolean).slice(0, 3),
  };
}

/* ------------------------------------------------- pure mapping: stage 2 */

const LABELS = {
  'space.min_area': 'Floor area', 'space.enclosed_rooms': 'Enclosed rooms', 'electrical.tenant_service': 'Electrical service',
  'plumbing.treatment_sinks': 'Sinks in the treatment rooms', 'plumbing.water_service': 'Water service', 'hvac.ventilation': 'Air supply to each room',
};
export const requirementLabel = rid => LABELS[String(rid).split('.').slice(1).join('.')] || cap(words(String(rid).split('.').pop()));
const TOPIC = { plumbing: 'plumbing', electrical: 'power', hvac: 'hvac', space: 'layout' }; // ids the page's chat topics use
const STATUS = {
  MEETS: { sev: 'low', impact: 'Meets, per the evidence' }, MAY_REQUIRE_WORK: { sev: 'mid', impact: 'May need work' },
  DOES_NOT_MEET: { sev: 'high', impact: 'Does not meet' }, FAILS: { sev: 'high', impact: 'Does not meet' }, UNKNOWN: { sev: 'unk', impact: 'Unknown' }, CONFLICTING: { sev: 'unk', impact: 'Conflicting evidence' },
};
// Evidence classes kept apart in words; the page's four kinds only pick the styling.
const CLASS_KIND = { historical: 'documented', verified: 'documented', measured: 'observed', visual: 'observed', 'owner-reported': 'estimate', derived: 'estimate', unknown: 'unknown', conflicting: 'unknown' };
export function evidenceClassOf(assessment, rec) {
  const permits = new Map();
  for (const p of rec.permit_records || []) for (const e of p.source_evidence_ids || []) permits.set(e, p);
  const linked = (assessment.evidence_ids || []).map(e => permits.get(e)).filter(Boolean);
  if (assessment.status === 'CONFLICTING') return { cls: 'conflicting', text: 'Conflicting evidence' };
  if (linked.length) {
    const uniq = [...new Map(linked.map(p => [p.permit_number, p])).values()];
    const first = uniq[0];
    return { cls: 'historical', text: `Historical permit ${uniq.map(p => p.permit_number).join(', ')} (${first.permit_status || 'status not stated'}${first.data_origin === 'FIXTURE_REAL' ? ', stored snapshot of public LA records' : ''}), not current capacity` };
  }
  if (/listing/i.test(assessment.uncertainty || '') || /listing/i.test((assessment.limitations || []).join(' '))) {
    const src = listingSource(rec.commercial_listing || {});
    const from = src.kind === 'synthetic' ? 'the listing (demo data)' : src.kind === 'snapshot' && rec.commercial_listing?.provider === 'costar' ? 'the CoStar listing (data-kit snapshot)' : 'the listing';
    return { cls: 'derived', text: `Derived from ${from}, not measured` };
  }
  return { cls: 'unknown', text: 'No evidence record' };
}
// Feasibility assessments of one property -> the page's `issues`. UNKNOWN is never shown as met and gets
// no dollar figure; the impact column carries the backend's status, not an amount.
export function issuesFrom(assessments, rec) {
  const seen = {};
  return (assessments || []).map(a => {
    const area = String(a.requirement_id).split('.')[1] || 'other';
    const base = TOPIC[area] || area, n = (seen[base] = (seen[base] || 0) + 1);
    const st = STATUS[a.status] || { sev: 'unk', impact: cap(words(a.status)) };
    const ev = evidenceClassOf(a, rec);
    const kind = a.status === 'UNKNOWN' ? 'unknown' : CLASS_KIND[ev.cls];
    const work = (a.proposed_work || []).map(w => (typeof w === 'string' ? w : w.description || '')).filter(Boolean);
    const question = (a.verification_questions || [])[0];
    return {
      id: n === 1 ? base : `${base}-${n}`, sev: st.sev, status: a.status,
      title: html(`${requirementLabel(a.requirement_id)}: ${words(a.status)}`), impact: html(st.impact),
      sum: html([a.uncertainty, work.length ? `Proposed work: ${work.join('; ')}.` : null, question ? `Open question: ${question}` : null].filter(Boolean).join(' ')),
      why: html((a.limitations || []).join(' ')), kind, evidence_class: ev.cls,
      ev: html(`${cap(ev.cls)} evidence: ${ev.text}`.replace(/\. /g, '; ')), next: question || null,
      // No `ask`: the page's "Approve and send" only marks a draft as sent, so live mode offers no draft.
    };
  });
}
// The latest cost scenario of one property -> the page's cost fields (dollars, display only).
// total to open = opening_cost_expected; "Renovation" = its one-time items except furniture (FF&E).
export function costsFrom(scenarios, propertyId) {
  const mine = (scenarios || []).filter(s => s.property_id === propertyId).sort((x, y) => String(x.calculated_at).localeCompare(String(y.calculated_at)));
  const s = mine.at(-1);
  if (!s) return { extras: null, build: null, other: null, weeks: null, scenario: null };
  const ffe = s.line_items.filter(l => l.kind === 'ONE_TIME' && l.category === 'FF&E').reduce((n, l) => n + l.expected, 0);
  const nnn = s.line_items.filter(l => l.kind === 'MONTHLY' && l.category === 'NNN').reduce((n, l) => n + l.expected, 0);
  return {
    extras: s.line_items.some(l => l.category === 'NNN') ? toDollars(nnn) : null,
    build: toDollars(s.opening_cost_expected - ffe), other: toDollars(ffe), weeks: null, // the backend gives no schedule
    scenario: { id: s.scenario_id, low: toDollars(s.opening_cost_low), expected: toDollars(s.opening_cost_expected), high: toDollars(s.opening_cost_high), assumptions: s.assumptions, excluded: s.excluded_costs },
  };
}
export function detailEntry(rec, assessments, scenarios) {
  return { ...costsFrom(scenarios, rec.property_id), photos: ['none'], plan: { w: 1000, h: 600, ink: [0, 0], mesh: [0, 0], box: [0, 0, 0, 0] }, rooms: [], issues: issuesFrom(assessments ?? rec.infrastructure_assessments, rec), live: true };
}

/* ------------------------------------------------- pure mapping: stage 3 */

// The layout solver needs a minimum area and width per room; the page never asks for them. These are
// the demo minimums of the e2e driver (apps/backend/e2e/driver.py, ROOMS), said so in the chat note.
export const ROOM_MINIMUMS = { treatment: { min_area_sqm: 10, min_width_m: 2.8, needs_plumbing: true }, reception: { min_area_sqm: 12, min_width_m: 3 }, storage: { min_area_sqm: 4, min_width_m: 1.5 } };
export function roomRequirements(counts) {
  return Object.entries(counts || { treatment: 3, reception: 1, storage: 1 }).map(([room_type, count]) => ({ room_type, count, ...ROOM_MINIMUMS[room_type] }));
}

// A generated layout and its site geometry (metres) -> the page's plan (100 px per metre) and rooms
// (fractions of the plan). Every room is a proposal: drawn dashed, never as an approved plan.
export function roomsFrom(layout, geometry) {
  const pts = (geometry?.floor_boundary || []).concat(...(layout.room_polygons || []).map(r => r.polygon));
  const xs = pts.map(p => p[0]), ys = pts.map(p => p[1]);
  const x0 = Math.min(...xs), y0 = Math.min(...ys), w = Math.max(...xs) - x0 || 1, h = Math.max(...ys) - y0 || 1;
  const count = {};
  const rooms = (layout.room_polygons || []).map(r => {
    const rx = r.polygon.map(p => p[0]), ry = r.polygon.map(p => p[1]);
    const t = ['treatment', 'reception', 'storage'].includes(r.room_type) ? r.room_type : r.room_type;
    count[t] = (count[t] || 0) + 1;
    return { id: r.room_id, type: t, name: html(`${cap(words(r.room_type))} ${count[t]}`), dim: `${toSqft(r.area_sqm)} SF`, proposed: 1,
      r: [(Math.min(...rx) - x0) / w, (Math.min(...ry) - y0) / h, (Math.max(...rx) - x0) / w, (Math.max(...ry) - y0) / h] };
  });
  return { plan: { w: Math.round(w * 100), h: Math.round(h * 100), ink: [0, 0], mesh: [0, 0], box: [0, 0, 0, 0] }, rooms };
}
// The bill of materials -> CAT entries (one per item type and room type) and the placed list.
// The backend gives no size, supplier or lead time: sizes are 0 (nothing drawn on the plan), the rest is
// marked as not available. Prices are fixture planning prices, never supplier quotes.
export function catalogFrom(bom, rooms) {
  const typeOf = Object.fromEntries(rooms.map(r => [r.id, r.type]));
  const cat = new Map(), placed = [];
  for (const it of bom.items || []) {
    const room = typeOf[it.room_id] || 'storage', q = Number(it.quantity);
    const pieces = it.unit === 'ea' && Number.isInteger(q) && q > 0; // counted items: one placed piece each
    // Measured items (m2 of flooring): one line at the line total, quantity and unit kept in the name.
    const id = pieces ? `${it.item_type}@${room}` : `${it.item_type}@${it.bom_item_id}`;
    const cents = pieces ? it.unit_price_estimate : Math.round(it.unit_price_estimate * q);
    if (!cat.has(id)) cat.set(id, {
      id, room, name: html(`${cap(words(it.item_type))}${pieces ? '' : `, ${it.quantity} ${it.unit}`}, ${priceWords(it.price_origin)}`),
      kw: words(it.item_type), w: 0, d: 0, h: 0, price: toDollars(cents) ?? 0,
      by: 'Not available', lead: 'Not available', at: [[0.5, 0.5]], live: { price_origin: it.price_origin, price_note: it.price_note },
    });
    for (let i = 0; i < (pieces ? q : 1); i++) placed.push({ c: id, r: it.room_id, st: 'placed' });
  }
  return { cat: [...cat.values()], placed, estimated_total: toDollars(bom.estimated_total), status: bom.status };
}
// A BOM price's origin in words. None of them is a supplier quote.
export function priceWords(origin) {
  const k = sourceOf(null, origin);
  return k === 'synthetic' ? 'fixture price, not a quote' : k === 'snapshot' ? 'catalog snapshot price, not a quote' : k === 'live' ? 'live catalog price, not a quote' : 'estimate, not a quote';
}
// A layout's floor geometry origin in words (the chat note after a layout).
export function geometryWords(g = {}) {
  const k = sourceOf(null, g.data_origin);
  const from = k === 'synthetic' ? ' (synthetic fixture)' : k === 'snapshot' ? ' (stored snapshot)' : k === 'live' ? ' (live provider data)' : ' (origin not stated)';
  return `${words(g.geometry_source) || 'source not stated'}${from}${g.scale_verified ? '' : ', scale not verified'}`;
}
// The note after a search: each kind counted separately, nothing said for a zero count.
export function searchNote(pool) {
  const n = k => pool.filter(p => p.live?.source === k).length;
  const parts = [
    n('snapshot') ? `${n('snapshot')} ${n('snapshot') === 1 ? 'is a real CoStar listing' : 'are real CoStar listings'} from the data-kit snapshot (dated records, not a live feed)` : null,
    n('live') ? `${n('live')} from a live listing provider` : null,
    n('synthetic') ? `${n('synthetic')} ${n('synthetic') === 1 ? 'is' : 'are'} synthetic demo data (fixture), not CoStar records` : null,
    n('unknown') ? `${n('unknown')} with no source stated` : null,
  ].filter(Boolean);
  if (!pool.length || !parts.length) return null;
  const scans = pool.filter(p => p.live?.scan).length;
  return `Of ${pool.length} ${pool.length === 1 ? 'listing' : 'listings'} from the backend, ${parts.join('; ')}. ${scans ? `${scans} ${scans === 1 ? 'has' : 'have'} a 3D scan of the suite itself, so a layout can be generated there.` : 'None has a 3D scan of the suite itself, so no layout can be generated yet.'}`;
}
// Owner texts saved by Photon as design-change notes -> read-only chat notes.
export function ownerTextNotes(comms) {
  // Shape written by apps/api/app/domains/communications/inbound.py (_save_design_change).
  return (comms || []).filter(c => String(c.purpose || '').toLowerCase() === 'design change').map(c => ({
    id: c.communication_id, at: c.created_at, channel: c.channel,
    text: (c.structured_findings || []).filter(f => f.question === 'design change' && f.classification === 'OWNER_REPORTED').map(f => String(f.answer || '').trim()).filter(Boolean).join(' '),
  })).filter(x => x.text);
}

/* --------------------------------------------------------------- HTTP */

export class Backend {
  // The wrapper matters in a browser: window.fetch called as a method of this object throws "Illegal invocation".
  constructor(api, fetchImpl = globalThis.fetch) { this.api = api; this.fetch = (...args) => fetchImpl(...args); this.token = null; this.project = null; }
  async req(method, path, body) {
    const headers = { accept: 'application/json' };
    if (body !== undefined) headers['content-type'] = 'application/json';
    if (this.token) headers.authorization = 'Bearer ' + this.token;
    let res;
    try { res = await this.fetch(this.api + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) }); } catch (e) {
      throw Object.assign(new Error(`The backend at ${this.api} cannot be reached (${e.message}).`), { status: 0 });
    }
    const text = await res.text(); let data = null;
    try { data = text ? JSON.parse(text) : null; } catch { data = text; }
    if (!res.ok) throw Object.assign(new Error(`${method} ${path} answered ${res.status}${data?.error_code || data?.code ? ' ' + (data.error_code || data.code) : ''}`), { status: res.status, data });
    return data;
  }
  async signIn() {
    if (this.token) return;
    try { this.token = (await this.req('POST', '/api/v1/dev/token')).access_token; } catch (e) {
      throw Object.assign(new Error(`Sign-in refused: the backend's dev sign-in is not available (${e.message}). Live mode needs a local backend with ALLOW_DEV_TOKENS=true; no token was invented.`), { status: e.status });
    }
    if (!this.token) throw new Error('Sign-in answered without a token.');
  }
  async ensureProject(req) {
    await this.signIn();
    const profile = profileFromAnswers(req), key = JSON.stringify(profile);
    if (!this.project) {
      await this.req('GET', '/api/v1/dev/fixtures/project'); // the dev seed route, as the e2e driver uses it; its contents are not used
      const out = await this.req('POST', '/api/v1/projects', { project_name: `Workspace: ${req.business || 'new location'}`, market: { city: 'Los Angeles', state: 'CA' }, business_profile: profile });
      this.project = out.project.project_id; this.profileKey = key;
    } else if (key !== this.profileKey) {
      await this.req('PUT', this.p('/business-profile'), profile); this.profileKey = key;
    }
    return this.project;
  }
  p(rest = '') { return `/api/v1/projects/${this.project}${rest}`; }
  async search(req) {
    await this.ensureProject(req);
    const task = (await this.req('POST', this.p('/properties/search'), searchBody(req))).task;
    if (task?.status !== 'COMPLETED') throw new Error(`The search task is ${task?.status || 'missing'}${task?.error_message ? ': ' + task.error_message : ''}.`);
    const ids = task.output_reference?.property_ids || [];
    const [props, rankings, scenarios] = await Promise.all([this.req('GET', this.p('/properties')), this.req('GET', this.p('/rankings')), this.req('GET', this.p('/cost-scenarios'))]);
    const ranking = rankings.find(r => r.ranking_id === task.output_reference?.ranking_id) || rankings.at(-1);
    const order = (ranking?.ranked_properties || []).map(x => x.property_id);
    const recs = props.filter(p => ids.includes(p.property_id)).sort((a, b) => (order.indexOf(a.property_id) + 1 || 1e9) - (order.indexOf(b.property_id) + 1 || 1e9));
    const extra = await Promise.all(recs.map(r => Promise.all([this.req('GET', this.p(`/properties/${r.property_id}/feasibility`)), this.req('GET', this.p(`/properties/${r.property_id}/permits`))])));
    return recs.map((rec, i) => ({ rec: { ...rec, permit_records: extra[i][1] }, assessments: extra[i][0], scenarios }));
  }
  async layout(propertyId) {
    const rooms = roomRequirements(this.lastRooms);
    const task = (await this.req('POST', this.p('/layouts/generate'), { property_id: propertyId, room_requirements: rooms })).task;
    if (task?.status !== 'COMPLETED') throw new Error(`Layout generation is ${task?.status || 'missing'}${task?.error_message ? ': ' + task.error_message : ''}.`);
    const id = task.output_reference?.design_version_ids?.[0];
    const layout = (await this.req('GET', this.p('/layouts'))).find(l => l.design_version_id === id);
    if (!layout) throw new Error('The generated layout is not in the layout list.');
    const [geometry, bom] = await Promise.all([this.req('GET', this.p(`/layouts/geometry/${layout.geometry_id}`)), this.req('GET', this.p(`/layouts/${id}/bom`))]);
    return { layout, geometry, bom };
  }
  chat(message) { return this.req('POST', this.p('/chat'), { message }); }
  communications() { return this.req('GET', this.p('/communications')); }
}
// A chat failure in plain words. Never a scripted answer in the assistant's name.
export function chatFailure(e) {
  const code = e?.data?.error_code || e?.data?.code || e?.data?.detail?.error_code || '';
  if (e?.status === 503 || /PYTHON_SERVICE_NOT_CONFIGURED/.test(code)) return 'The assistant is not reachable: the backend has no assistant service configured. Nothing was answered.';
  if (e?.status === 502 || e?.status === 504) return 'The assistant is not reachable right now (the backend could not get an answer from it). Nothing was answered; try again.';
  if (e?.status === 0) return `The assistant is not reachable: ${e.message}`;
  return `The assistant could not answer: ${e?.message || 'unknown error'}.`;
}

/* ------------------------------------------------------- page wiring */

// A neutral image in place of any local listing asset for a backend listing.
const NEUTRAL = (text, origin) => 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(
  `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 400 300" preserveAspectRatio="none"><rect width="400" height="300" fill="#ECEBE7"/><text x="200" y="140" text-anchor="middle" font-family="system-ui,sans-serif" font-size="17" fill="#6B6A66">${html(text)}</text><text x="200" y="168" text-anchor="middle" font-family="system-ui,sans-serif" font-size="13" fill="#8A8985">${html(origin)}</text></svg>`);
// The page asks for an image by (p.src, file). Every backend listing is answered here: synthetic ones by
// "live-<id>", real ones by their id (see poolEntry). The page's own listings get undefined (its own path).
export function asset(src, f, P = page?.P) {
  src = String(src);
  const p = P?.[src.startsWith('live-') ? src.slice(5) : src], L = p?.live;
  if (!L && !src.startsWith('live-')) return undefined;
  if (L?.local?.captions[f]) return `assets/${L.local.slug}/${f}`; // a saved copy from this suite's own scan or listing
  const still = MEDIA_PHOTO.exec(f);
  if (still && L?.interior && !L.local) return mediaUrl(L.interior.asset_id, `photo/${+still[1]}.jpg`); // the backend's still of the chosen scan
  if (L && f === 'listing-photo' && L.photo_url) return L.photo_url;
  if (L && (f === 'aerial-vexcel' || /vexcel/.test(f)) && L.aerial) return L.aerial.url; // the ortho image for every direction
  return NEUTRAL(/floorplan/.test(f) ? 'No floor plan image' : /vexcel/.test(f) ? 'No aerial image' : 'No photo',
    !L ? 'From the backend; no image provided' : L.fixture ? 'Demo data from the backend (fixture)' : `${L.source_label}; no image provided`);
}

let page = null, backend = null;
try { localStorage.removeItem('second-site-workspace-live'); } catch { /* storage unavailable */ } // live mode starts fresh

// Called once by workspace.js with its data tables (mutated in place) and a few of its functions.
export function bind(ctx) {
  page = ctx; backend = new Backend(config().api);
  for (const t of [ctx.POOL, ctx.CAT, ctx.FIRST, ctx.FILL]) t.length = 0;
  for (const t of [ctx.P, ctx.D, ctx.C, ctx.SPACE_NOTES]) for (const k of Object.keys(t)) delete t[k];
  const S = ctx.S(); S.slots = [null, null, null, null];
  viewerElements();
}
const markers = {};
function addPin(p) {
  const map = page.map();
  if (!map || !window.maplibregl || page.pins[p.id]) return; // no map: the page's renderMap() returns early
  const wrap = document.createElement('div'), el = document.createElement('button');
  el.type = 'button'; el.setAttribute('aria-label', p.address); el.dataset.property = p.id; el.onclick = () => page.selectSpace(p.id);
  wrap.append(el); page.pins[p.id] = el; // renderMap() styles every POOL entry's pin, so one exists even without coordinates
  if (typeof p.lat === 'number' && typeof p.lon === 'number') markers[p.id] = new window.maplibregl.Marker({ element: wrap }).setLngLat([p.lon, p.lat]).addTo(map);
}
function dropPin(id) { markers[id]?.remove(); delete markers[id]; delete page.pins[id]; }
// Stage 1: after each search-stage answer. Replaces the listing pool with the backend's results.
let waitNoted = false;
// One search at a time: the page's inline answer edit (workspace.js line 332) is not guarded by its
// `busy` flag, so a second search could start while the first is still creating the project.
let queue = Promise.resolve();
export function search(S) { const run = queue.then(() => searchNow(S)); queue = run.catch(() => {}); return run; }
async function searchNow(S) {
  if (!S.req.business) return; // nothing to search for until the business is known
  if (!roomsKnown(S.req)) { // said once, not after every answer
    if (!waitNoted) { waitNoted = true; page.note(page.esc('Live results start once you have said how many rooms you need.')); }
    return;
  }
  backend.lastRooms = Number.isInteger(S.req.rooms) ? { treatment: S.req.rooms, reception: 1, storage: 1 } : null;
  let results;
  try { results = await backend.search(S.req); } catch (e) { page.note(page.esc(`Live search failed: ${e.message}`)); return; }
  const { POOL, P, D, FILL, SPACE_NOTES } = page, index = await localPlaces();
  // A slug is used only when exactly one listing of this result has that suite scan.
  const hits = results.map(r => localMatch(r.rec, index)), uses = s => hits.filter(h => h?.slug === s).length;
  const entries = results.map((r, i) => { const p = poolEntry(r.rec, backend.api), h = hits[i]; return h && uses(h.slug) === 1 ? withLocal(p, h) : p; });
  for (const id of Object.keys(P)) if (!entries.some(p => p.id === id)) { dropPin(id); delete P[id]; }
  POOL.length = 0; FILL.length = 0;
  for (const [i, { rec, assessments, scenarios }] of results.entries()) {
    const p = entries[i]; POOL.push(p); P[p.id] = p; FILL.push(p.id);
    D[p.src] = { ...detailEntry(rec, assessments, scenarios), photos: p.live.images.length ? p.live.images : ['none'] };
    SPACE_NOTES[p.id] = spaceNotes(rec, backend.api, p.live.local || null); addPin(p);
  }
  disambiguate(POOL);
  // Remove saved or shown ids the backend no longer returns (the page would look them up and fail).
  S.slots = S.slots.map(id => (id && P[id] ? id : null));
  S.saved = S.saved.filter(id => P[id]); if (S.open && !P[S.open]) S.open = null;
  const msg = searchNote(POOL);
  if (msg) page.note(page.esc(msg));
}
// Stage 3: before the Furnish stage opens. False keeps the page in Compare.
export async function furnish(S) {
  const p = page.P[S.finalist], d = page.D[p?.src];
  if (!p || !d) return false;
  if (!d.rooms.length) {
    let out;
    try { out = await backend.layout(p.live?.property_id ?? p.id); } catch (e) { page.note(page.esc(`No layout for ${p.address}: ${e.message}`)); return false; }
    const { plan, rooms } = roomsFrom(out.layout, out.geometry), cat = catalogFrom(out.bom, rooms);
    Object.assign(d, { plan, rooms, bom: cat, geometry: { source: out.geometry.geometry_source, data_origin: out.geometry.data_origin, scale_verified: out.geometry.scale_verified } });
    for (const c of cat.cat) if (!page.C[c.id]) { page.CAT.push(c); page.C[c.id] = c; }
    const g = out.geometry;
    page.note(page.esc(`Layout version ${out.layout.version_number} for ${p.address}: ${out.layout.approval_status.toLowerCase()}, ${out.layout.validation_report?.disclaimer || 'not an approved plan'} Floor geometry: ${geometryWords(g)}. Room minimums used: demo defaults (treatment 10 m² and 2.8 m wide, reception 12 m², storage 4 m²), not your answers. Bill of materials ${words(cat.status)}: ${cat.cat.length} item types, prices: ${[...new Set((out.bom.items || []).map(i => priceWords(i.price_origin)))].join('; ') || 'none'}.`));
  }
  if (!S.placed.length || S.chosen !== S.finalist) S.placed = d.bom.placed.map(x => ({ ...x }));
  ownerTexts();
  return true;
}
const shown = new Set();
async function ownerTexts() {
  let list;
  try { list = ownerTextNotes(await backend.communications()); } catch { return; }
  for (const t of list) if (!shown.has(t.id)) {
    shown.add(t.id);
    page.note(`<b>Owner’s text message</b> (owner-reported; not applied to the layout, and it cannot approve anything): ${page.esc(t.text)}`);
  }
}
// Typed or chosen chat text. Null lets the page handle it (the search questions and the page's own
// navigation commands); everything else goes to the project chat.
// discover: the page's own answer edits after the questions (rent, rooms, area, opening) re-run the live search.
const PAGE_COMMANDS = { discover: /look into|compare|closer|ready|go ahead|\$|rent|budget|month|room|downtown|west|east|valley|anywhere|open/i, compare: /anyway|layout|try|choose|go with|pick|sign/i, furnish: /^$/ };
export function turn(stage, text, S, steps) {
  if (stage === 'discover' && S.step < steps) return null;
  if (PAGE_COMMANDS[stage].test(text)) return null;
  return (async () => {
    if (!backend.project) { page.note('The assistant is not connected until the search has created a project.'); return; }
    try {
      const out = await backend.chat(text);
      page.a(`${page.esc(out.reply).replace(/\n/g, '<br>')}<br><small>${out.mode === 'deterministic' ? 'Deterministic engine, no model configured' : 'Assistant'} · from the backend</small>`);
    } catch (e) { page.note(page.esc(chatFailure(e))); }
  })();
}
