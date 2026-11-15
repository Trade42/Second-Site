/** Local demo adapter. It does not call the API and does not weaken project auth. */

export type Origin = "FIXTURE_REAL" | "FIXTURE_SYNTHETIC" | "LIVE";

export type Product = {
  product_id: string;
  name: string;
  category: string;
  price_cents: number | null;
  width_m: number | null;
  depth_m: number | null;
  height_m: number | null;
  product_url: string | null;
  image_urls: string[];
  data_origin: Origin;
  merchant: string;
  notes: string;
  supports_overlap: boolean;
};

export type Item = {
  instance_id: string;
  product_id: string;
  room_id: string;
  category: string;
  name: string;
  x_m: number;
  y_m: number;
  rotation_deg: number;
  width_m: number;
  depth_m: number;
  height_m: number;
  price_cents: number | null;
  owned: boolean;
  locked: boolean;
  product_url: string | null;
  image_url: string | null;
  data_origin: Origin;
  supports_overlap: boolean;
};

export type Room = { room_id: string; room_type: string; label: string; polygon: [number, number][] };
export type Door = { x_m: number; y_m: number; clearance_m: number; note: string };

export type Plan = {
  revision: number;
  title: string;
  geometry_label: string;
  rooms: Room[];
  doors: Door[];
  instances: Item[];
  budget_cents: number;
  undo: Item[][];
  redo: Item[][];
};

export type BudgetLine = {
  key: string;
  name: string;
  room_id: string;
  quantity: number;
  line_cents: number | null;
  owned: boolean;
  product_url: string | null;
  data_origin: Origin;
  unresolved: boolean;
  instance_ids: string[];
};

const SNAPSHOT =
  "Snapshot of a Minerva Beauty page. The price can change and this is not a live check.";
const SAMPLE = "Sample fixture. The price and the drawing are not a supplier offer.";

function svgPreview(label: string, fill: string) {
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 320 240"><rect width="320" height="240" fill="#f4efe6"/><rect x="48" y="36" width="224" height="168" rx="16" fill="${fill}"/><text x="160" y="128" text-anchor="middle" font-family="sans-serif" font-size="18" fill="#2b2b2b">${label}</text></svg>`;
  return `data:image/svg+xml,${encodeURIComponent(svg)}`;
}

function sample(
  id: string,
  category: string,
  name: string,
  price: number,
  w: number,
  d: number,
  h: number,
  fill: string,
): Product {
  return {
    product_id: id,
    name,
    category,
    merchant: "Sample fixture",
    price_cents: price,
    width_m: w,
    depth_m: d,
    height_m: h,
    product_url: null,
    image_urls: [svgPreview(name.replace("Sample ", ""), fill)],
    data_origin: "FIXTURE_SYNTHETIC",
    notes: SAMPLE,
    supports_overlap: false,
  };
}

export const CURATED: Product[] = [
  {
    product_id: "minerva-facial-bed",
    name: "Facial Bed in Black",
    category: "treatment_bed",
    merchant: "Minerva Beauty",
    price_cents: 34320,
    width_m: 0.8382,
    depth_m: 1.8288,
    height_m: 0.6858,
    product_url: "https://www.minervabeauty.com/product/facial-bed?variant=color:8312",
    image_urls: [
      "https://cdn11.bigcommerce.com/s-7holj19jgh/images/stencil/1280x1280/products/366/28664/8832w-main_1__96438.1745926585.png?c=1",
    ],
    data_origin: "FIXTURE_REAL",
    notes: SNAPSHOT,
    supports_overlap: false,
  },
  {
    product_id: "minerva-enigma",
    name: "Enigma Massage Table in Black",
    category: "treatment_bed",
    merchant: "Minerva Beauty",
    price_cents: 44720,
    width_m: 0.8128,
    depth_m: 2.1082,
    height_m: 0.7366,
    product_url: "https://www.minervabeauty.com/product/enigma-massage-table/?variant=color:4184",
    image_urls: [
      "https://cdn11.bigcommerce.com/s-7holj19jgh/images/stencil/1280x1280/products/757/28986/8923b_1__04888.1745926907.jpg?c=1",
    ],
    data_origin: "FIXTURE_REAL",
    notes: SNAPSHOT,
    supports_overlap: false,
  },
  {
    product_id: "minerva-kato",
    name: "Kato 2-Tier Salon Tray in Gold",
    category: "rolling_cart",
    merchant: "Minerva Beauty",
    price_cents: 15920,
    width_m: 0.4953,
    depth_m: 0.3937,
    height_m: 0.87,
    product_url: "https://www.minervabeauty.com/product/kato-mobile-service-tray-1?variant=color:7764",
    image_urls: [
      "https://cdn11.bigcommerce.com/s-7holj19jgh/images/stencil/1280x1280/products/534/28801/4429bg-2_1__38839.1745926718.jpg?c=1",
    ],
    data_origin: "FIXTURE_REAL",
    notes: SNAPSHOT,
    supports_overlap: false,
  },
  {
    product_id: "minerva-modera",
    name: "Modera Pedestal Styling Station",
    category: "cabinet",
    merchant: "Minerva Beauty",
    price_cents: 59920,
    width_m: 0.4572,
    depth_m: 0.4572,
    height_m: 0.9398,
    product_url:
      "https://www.minervabeauty.com/product/modera-pedestal-styling-station/?variant=body_laminate_color:9763-countertop_and_toe_kick_color:9768-handles:9770",
    image_urls: [
      "https://cdn11.bigcommerce.com/s-7holj19jgh/images/stencil/1280x1280/products/5617/32250/MD-MODERA-PEDESTAL-NSA-B-B__31237.1755088925.jpg?c=1",
    ],
    data_origin: "FIXTURE_REAL",
    notes: SNAPSHOT,
    supports_overlap: false,
  },
];

export const SAMPLES: Product[] = [
  sample("sample-stool", "practitioner_stool", "Sample practitioner stool", 18000, 0.4, 0.4, 0.55, "#d5ddd6"),
  sample("sample-cart", "rolling_cart", "Sample rolling cart", 16000, 0.45, 0.4, 0.85, "#e4d8c4"),
  sample("sample-light", "task_light", "Sample task light", 9500, 0.22, 0.22, 1.45, "#f4ecd2"),
  sample("sample-desk", "reception_desk", "Sample reception desk", 45000, 1.6, 0.7, 0.75, "#cbb892"),
  sample("sample-chair", "waiting_chair", "Sample waiting chair", 11000, 0.55, 0.55, 0.82, "#e7dfd4"),
  sample("sample-bed-compact", "treatment_bed", "Sample compact treatment bed", 120000, 0.75, 1.8, 0.62, "#eadfce"),
];

export const CATALOG: Product[] = [...CURATED, ...SAMPLES];

export const CATEGORIES: { id: string; label: string }[] = [
  { id: "all", label: "All" },
  { id: "treatment_bed", label: "Treatment beds" },
  { id: "practitioner_stool", label: "Stools" },
  { id: "rolling_cart", label: "Carts" },
  { id: "task_light", label: "Task lights" },
  { id: "cabinet", label: "Cabinets" },
  { id: "reception_desk", label: "Reception" },
  { id: "waiting_chair", label: "Waiting chairs" },
];

export function originLabel(origin: Origin) {
  if (origin === "FIXTURE_REAL") return "Supplier snapshot";
  if (origin === "LIVE") return "Live";
  return "Sample";
}

const DOOR_NOTE = "Door swing is unknown. 0.75 m is a clearance assumption, not a measured hinge.";

export const ROOMS: Room[] = [
  { room_id: "treatment-a", room_type: "treatment", label: "Treatment A", polygon: [[0, 0], [4, 0], [4, 3.2], [0, 3.2]] },
  { room_id: "treatment-b", room_type: "treatment", label: "Treatment B", polygon: [[4.4, 0], [8.4, 0], [8.4, 3.2], [4.4, 3.2]] },
  { room_id: "reception", room_type: "reception", label: "Reception", polygon: [[0, 3.6], [5.2, 3.6], [5.2, 7.2], [0, 7.2]] },
  { room_id: "storage", room_type: "storage", label: "Storage", polygon: [[5.4, 3.6], [8.4, 3.6], [8.4, 5.6], [5.4, 5.6]] },
];

export const DOORS: Door[] = [
  { x_m: 2, y_m: 3.2, clearance_m: 0.75, note: DOOR_NOTE },
  { x_m: 6.4, y_m: 3.2, clearance_m: 0.75, note: DOOR_NOTE },
  { x_m: 5.2, y_m: 5.4, clearance_m: 0.75, note: DOOR_NOTE },
];

const STORAGE_KEY = "second-site.studio-demo.v1";

function place(product: Product, room_id: string, x: number, y: number, rotation = 0): Item {
  return {
    instance_id: crypto.randomUUID(),
    product_id: product.product_id,
    room_id,
    category: product.category,
    name: product.name,
    x_m: x,
    y_m: y,
    rotation_deg: rotation,
    width_m: product.width_m ?? 0,
    depth_m: product.depth_m ?? 0,
    height_m: product.height_m ?? 0,
    price_cents: product.price_cents,
    owned: false,
    locked: false,
    product_url: product.product_url,
    image_url: product.image_urls[0] ?? null,
    data_origin: product.data_origin,
    supports_overlap: product.supports_overlap,
  };
}

export function freshPlan(): Plan {
  const byId = Object.fromEntries(CATALOG.map((p) => [p.product_id, p]));
  const at = (id: string, room: string, x: number, y: number, rotation = 0) => place(byId[id], room, x, y, rotation);
  return {
    revision: 1,
    title: "Sample facial studio",
    geometry_label: "Sample studio. This floor is not a measured property.",
    rooms: ROOMS,
    doors: DOORS,
    budget_cents: 800_000,
    undo: [],
    redo: [],
    instances: [
      at("minerva-facial-bed", "treatment-a", 2, 1.35),
      at("sample-stool", "treatment-a", 3.35, 2.55),
      at("sample-cart", "treatment-a", 0.55, 0.5),
      at("sample-light", "treatment-a", 3.5, 0.4),
      at("minerva-enigma", "treatment-b", 6.4, 1.3),
      at("sample-stool", "treatment-b", 7.75, 2.6),
      at("minerva-kato", "treatment-b", 4.9, 0.5),
      at("sample-light", "treatment-b", 7.95, 0.4),
      at("sample-desk", "reception", 2.5, 6.35),
      at("sample-chair", "reception", 1.05, 4.85),
      at("sample-chair", "reception", 2.45, 4.85),
      at("sample-chair", "reception", 3.85, 4.85),
      at("minerva-modera", "storage", 6.35, 4.55),
    ],
  };
}

export function loadDemo(): Plan {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return freshPlan();
    const parsed = JSON.parse(raw) as Plan;
    if (!parsed.instances || !parsed.rooms) return freshPlan();
    return parsed;
  } catch {
    return freshPlan();
  }
}

export function saveDemo(plan: Plan) {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(plan));
}

export function resetDemo() {
  localStorage.removeItem(STORAGE_KEY);
  return freshPlan();
}

const EPS = 1e-6;

function corners(item: Item) {
  const hw = item.width_m / 2;
  const hd = item.depth_m / 2;
  const rad = (item.rotation_deg * Math.PI) / 180;
  const c = Math.cos(rad);
  const s = Math.sin(rad);
  return [ [hw, hd], [hw, -hd], [-hw, -hd], [-hw, hd] ].map(([x, y]) => [
    item.x_m + x * c - y * s,
    item.y_m + x * s + y * c,
  ]) as [number, number][];
}

function onEdge(px: number, py: number, ax: number, ay: number, bx: number, by: number) {
  const cross = (px - ax) * (by - ay) - (py - ay) * (bx - ax);
  if (Math.abs(cross) > EPS) return false;
  const dot = (px - ax) * (bx - ax) + (py - ay) * (by - ay);
  if (dot < -EPS) return false;
  return dot <= (bx - ax) ** 2 + (by - ay) ** 2 + EPS;
}

function pointInRing(x: number, y: number, ring: [number, number][]) {
  const pts = ring[0][0] === ring.at(-1)?.[0] && ring[0][1] === ring.at(-1)?.[1] ? ring : [...ring, ring[0]];
  for (let i = 0; i < pts.length - 1; i++) if (onEdge(x, y, pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1])) return true;
  let inside = false;
  for (let i = 0, j = pts.length - 2; i < pts.length - 1; j = i++) {
    const [xi, yi] = pts[i];
    const [xj, yj] = pts[j];
    if ((yi > y) !== (yj > y) && x < ((xj - xi) * (y - yi)) / ((yj - yi) || EPS) + xi) inside = !inside;
  }
  return inside;
}

function overlap(a: [number, number][], b: [number, number][]) {
  const axes: [number, number][] = [];
  for (const poly of [a, b]) {
    for (let i = 0; i < poly.length; i++) {
      const [x1, y1] = poly[i];
      const [x2, y2] = poly[(i + 1) % poly.length];
      const len = Math.hypot(x2 - x1, y2 - y1) || 1;
      axes.push([-(y2 - y1) / len, (x2 - x1) / len]);
    }
  }
  for (const [ax, ay] of axes) {
    const project = (poly: [number, number][]) => poly.map(([x, y]) => x * ax + y * ay);
    const pa = project(a);
    const pb = project(b);
    if (Math.max(...pa) < Math.min(...pb) - EPS || Math.max(...pb) < Math.min(...pa) - EPS) return false;
  }
  return true;
}

export function issuesFor(item: Item, plan: Plan) {
  const messages: string[] = [];
  if (![0, 90, 180, 270].includes(item.rotation_deg)) messages.push("Rotation is limited to 90-degree steps.");
  const room = plan.rooms.find((r) => r.room_id === item.room_id);
  if (!room) return ["That room is not on this plan."];
  const box = corners(item);
  if (box.some(([x, y]) => !pointInRing(x, y, room.polygon))) messages.push(`${item.name} is outside ${room.label}.`);
  for (const door of plan.doors) {
    if (box.some(([x, y]) => Math.hypot(x - door.x_m, y - door.y_m) < door.clearance_m)) {
      messages.push(`${item.name} is inside the door clearance.`);
      break;
    }
  }
  if (!item.supports_overlap) {
    for (const other of plan.instances) {
      if (other.instance_id === item.instance_id || other.supports_overlap) continue;
      if (overlap(box, corners(other))) messages.push(`${item.name} overlaps ${other.name}.`);
    }
  }
  return messages;
}

function commit(plan: Plan, instances: Item[]): { plan: Plan; error: string | null } {
  const next = { ...plan, instances, revision: plan.revision + 1, undo: [...plan.undo, plan.instances].slice(-30), redo: [] };
  const problems = instances.flatMap((item) => issuesFor(item, next));
  if (problems.length) return { plan, error: problems[0] };
  return { plan: next, error: null };
}

export function moveItem(plan: Plan, id: string, x: number, y: number) {
  const item = plan.instances.find((i) => i.instance_id === id);
  if (!item) return { plan, error: "That piece is not on the plan." };
  if (item.locked) return { plan, error: `${item.name} is locked.` };
  return commit(plan, plan.instances.map((i) => (i.instance_id === id ? { ...i, x_m: x, y_m: y } : i)));
}

export function rotateItem(plan: Plan, id: string) {
  const item = plan.instances.find((i) => i.instance_id === id);
  if (!item) return { plan, error: "That piece is not on the plan." };
  if (item.locked) return { plan, error: `${item.name} is locked.` };
  const rotation_deg = (item.rotation_deg + 90) % 360;
  return commit(plan, plan.instances.map((i) => (i.instance_id === id ? { ...i, rotation_deg } : i)));
}

export function removeItem(plan: Plan, id: string) {
  const item = plan.instances.find((i) => i.instance_id === id);
  if (!item) return { plan, error: "That piece is not on the plan." };
  if (item.locked) return { plan, error: `${item.name} is locked.` };
  return commit(plan, plan.instances.filter((i) => i.instance_id !== id));
}

export function lockItem(plan: Plan, id: string, locked: boolean) {
  const item = plan.instances.find((i) => i.instance_id === id);
  if (!item) return { plan, error: "That piece is not on the plan." };
  return commit(plan, plan.instances.map((i) => (i.instance_id === id ? { ...i, locked } : i)));
}

export function duplicateItem(plan: Plan, id: string) {
  const item = plan.instances.find((i) => i.instance_id === id);
  if (!item) return { plan, error: "That piece is not on the plan." };
  const copy = { ...item, instance_id: crypto.randomUUID(), x_m: item.x_m + 0.8, locked: false };
  return commit(plan, [...plan.instances, copy]);
}

export function replaceItem(plan: Plan, id: string, product: Product) {
  const item = plan.instances.find((i) => i.instance_id === id);
  if (!item) return { plan, error: "That piece is not on the plan." };
  if (item.locked) return { plan, error: `${item.name} is locked.` };
  if (product.width_m == null || product.depth_m == null || product.height_m == null) {
    return { plan, error: `${product.name} has unknown dimensions and cannot be placed.` };
  }
  const nextItem = {
    ...place(product, item.room_id, item.x_m, item.y_m, item.rotation_deg),
    instance_id: item.instance_id,
    locked: item.locked,
  };
  return commit(plan, plan.instances.map((i) => (i.instance_id === id ? nextItem : i)));
}

export function addProduct(plan: Plan, product: Product, roomId: string) {
  if (product.width_m == null || product.depth_m == null || product.height_m == null) {
    return { plan, error: `${product.name} has unknown dimensions and cannot be placed.` };
  }
  const room = plan.rooms.find((r) => r.room_id === roomId);
  if (!room) return { plan, error: "Choose a room first." };
  const cx = room.polygon.reduce((s, p) => s + p[0], 0) / room.polygon.length;
  const cy = room.polygon.reduce((s, p) => s + p[1], 0) / room.polygon.length;
  const spots: [number, number][] = [
    [cx, cy],
    [cx - 0.6, cy],
    [cx + 0.6, cy],
    [cx, cy - 0.6],
    [cx, cy + 0.6],
    [cx - 0.8, cy - 0.4],
  ];
  for (const [x, y] of spots) {
    const item = place(product, roomId, x, y);
    const trial = { ...plan, instances: [...plan.instances, item] };
    if (issuesFor(item, trial).length === 0) return commit(plan, trial.instances);
  }
  return { plan, error: `${product.name} does not fit in ${room.label} without overlapping.` };
}

export function undo(plan: Plan) {
  const previous = plan.undo.at(-1);
  if (!previous) return { plan, error: "Nothing to undo." };
  return {
    plan: { ...plan, instances: previous, undo: plan.undo.slice(0, -1), redo: [...plan.redo, plan.instances], revision: plan.revision + 1 },
    error: null,
  };
}

export function redo(plan: Plan) {
  const next = plan.redo.at(-1);
  if (!next) return { plan, error: "Nothing to redo." };
  return {
    plan: { ...plan, instances: next, redo: plan.redo.slice(0, -1), undo: [...plan.undo, plan.instances], revision: plan.revision + 1 },
    error: null,
  };
}

export function budget(plan: Plan): { subtotal: number; remaining: number | null; unresolved: number; lines: BudgetLine[] } {
  const groups = new Map<string, BudgetLine>();
  for (const item of plan.instances) {
    const key = `${item.product_id}|${item.room_id}|${item.owned}`;
    const current = groups.get(key) ?? {
      key,
      name: item.name,
      room_id: item.room_id,
      quantity: 0,
      line_cents: 0,
      owned: item.owned,
      product_url: item.product_url,
      data_origin: item.data_origin,
      unresolved: false,
      instance_ids: [],
    };
    current.quantity += 1;
    current.instance_ids.push(item.instance_id);
    if (item.owned) current.line_cents = 0;
    else if (item.price_cents == null) {
      current.unresolved = true;
      current.line_cents = null;
    } else if (current.line_cents != null) current.line_cents += item.price_cents;
    groups.set(key, current);
  }
  const lines = [...groups.values()];
  const unresolved = lines.filter((l) => l.unresolved).length;
  const subtotal = lines.reduce((sum, line) => sum + (line.owned || line.line_cents == null ? 0 : line.line_cents), 0);
  return { subtotal, remaining: unresolved ? null : plan.budget_cents - subtotal, unresolved, lines };
}

export function roomLabel(plan: Plan, roomId: string) {
  return plan.rooms.find((r) => r.room_id === roomId)?.label ?? roomId;
}

export function assistant(plan: Plan, instruction: string, selectedId: string | null): { plan: Plan; message: string } {
  const text = instruction.toLowerCase();
  if (text.includes("shopping")) return { plan, message: "The shopping list is open." };
  if (text.includes("furnish") && text.includes("treatment")) {
    let next = plan;
    const notes: string[] = [];
    for (const room of plan.rooms.filter((r) => r.room_type === "treatment")) {
      const hasBed = next.instances.some((i) => i.room_id === room.room_id && i.category === "treatment_bed");
      if (hasBed) {
        notes.push(`${room.label} already has a treatment bed.`);
        continue;
      }
      const result = addProduct(next, CURATED[0], room.room_id);
      if (result.error) notes.push(result.error);
      else {
        next = result.plan;
        notes.push(`Added the Facial Bed to ${room.label}.`);
      }
    }
    return { plan: next, message: notes.join(" ") };
  }
  if (text.includes("cheaper") && text.includes("bed")) {
    const beds = CATALOG.filter((p) => p.category === "treatment_bed" && p.price_cents != null);
    const cheapest = beds.reduce((a, b) => ((a.price_cents ?? 0) <= (b.price_cents ?? 0) ? a : b));
    const targets = plan.instances.filter((i) => i.category === "treatment_bed" && (!selectedId || i.instance_id === selectedId));
    let next = plan;
    let changed = 0;
    for (const item of targets) {
      if (item.product_id === cheapest.product_id) continue;
      const result = replaceItem(next, item.instance_id, cheapest);
      if (result.error) return { plan, message: result.error };
      next = result.plan;
      changed += 1;
    }
    const price = `$${((cheapest.price_cents ?? 0) / 100).toFixed(2)}`;
    if (!changed) return { plan, message: `${cheapest.name} is already the lowest priced treatment bed in this catalog (${price}).` };
    return { plan: next, message: `Replaced ${changed} treatment bed${changed === 1 ? "" : "s"} with ${cheapest.name} (${price}).` };
  }
  if (text.includes("keep") && (text.includes("cabinet") || text.includes("station"))) {
    const cabinets = plan.instances.filter((i) => i.category === "cabinet");
    if (!cabinets.length) return { plan, message: "There is no cabinet on the plan to keep." };
    let next = plan;
    for (const item of cabinets) {
      const result = lockItem(next, item.instance_id, true);
      next = result.plan;
    }
    return { plan: next, message: `Locked ${cabinets.length} cabinet${cabinets.length === 1 ? "" : "s"}. Locked pieces stay where they are.` };
  }
  return {
    plan,
    message: "Demo assistant only. Try “Furnish both treatment rooms,” “Find a cheaper treatment bed,” “Keep the selected cabinet,” or “Show my shopping list.”",
  };
}

export function shoppingCsv(plan: Plan) {
  const rows = [["name", "room", "quantity", "line_usd", "origin", "url"]];
  for (const line of budget(plan).lines) {
    rows.push([
      line.name,
      roomLabel(plan, line.room_id),
      String(line.quantity),
      line.line_cents == null ? "" : (line.line_cents / 100).toFixed(2),
      originLabel(line.data_origin),
      line.product_url ?? "",
    ]);
  }
  return rows.map((row) => row.map((cell) => `"${cell.replaceAll('"', '""')}"`).join(",")).join("\n");
}
