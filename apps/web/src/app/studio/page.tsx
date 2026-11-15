"use client";

import dynamic from "next/dynamic";
import { useEffect, useMemo, useState } from "react";
import { Badge, Button } from "@/components/ui/primitives";
import {
  CATALOG,
  CATEGORIES,
  type Item,
  type Plan,
  type Product,
  addProduct,
  assistant,
  budget,
  duplicateItem,
  issuesFor,
  loadDemo,
  lockItem,
  moveItem,
  originLabel,
  redo,
  removeItem,
  replaceItem,
  resetDemo,
  roomLabel,
  rotateItem,
  saveDemo,
  shoppingCsv,
  undo,
} from "./demo";

const Dollhouse = dynamic(() => import("./Dollhouse").then((m) => m.Dollhouse), { ssr: false });

const money = (cents: number | null | undefined) => (cents == null ? "Unknown" : `$${(cents / 100).toFixed(2)}`);

const SUGGESTIONS = [
  "Furnish both treatment rooms.",
  "Find a cheaper treatment bed.",
  "Keep the selected cabinet.",
  "Show my shopping list.",
];

export default function StudioPage() {
  const [plan, setPlan] = useState<Plan | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [category, setCategory] = useState("all");
  const [roomId, setRoomId] = useState("treatment-a");
  const [tab, setTab] = useState<"assistant" | "item" | "list" | "trades">("assistant");
  const [catalogOpen, setCatalogOpen] = useState(true);
  const [mapOpen, setMapOpen] = useState(false);
  const [view, setView] = useState<"iso" | "top">("iso");
  const [cameraNonce, setCameraNonce] = useState(0);
  const [notice, setNotice] = useState<string | null>(null);
  const [chat, setChat] = useState<{ role: "you" | "assistant"; text: string }[]>([
    { role: "assistant", text: "This is a demo assistant. I can furnish the treatment rooms, swap in a cheaper bed, or lock the cabinet. I am not a connected model." },
  ]);
  const [draft, setDraft] = useState("");
  const [drag, setDrag] = useState<{ id: string; x: number; y: number } | null>(null);
  const [savedTick, setSavedTick] = useState(0);

  useEffect(() => {
    const timer = window.setTimeout(() => setPlan(loadDemo()), 0);
    return () => window.clearTimeout(timer);
  }, []);

  useEffect(() => {
    if (plan) saveDemo(plan);
  }, [plan, savedTick]);

  const shown = useMemo(() => {
    if (!plan) return [];
    if (!drag) return plan.instances;
    return plan.instances.map((item) => (item.instance_id === drag.id ? { ...item, x_m: drag.x, y_m: drag.y } : item));
  }, [drag, plan]);

  const selectedItem = shown.find((item) => item.instance_id === selected) ?? null;
  const totals = plan ? budget({ ...plan, instances: shown }) : null;
  const invalidId = plan && drag
    ? issuesFor(shown.find((item) => item.instance_id === drag.id) as Item, { ...plan, instances: shown }).length
      ? drag.id
      : null
    : null;

  function apply(result: { plan: Plan; error: string | null }, ok: string) {
    if (result.error) {
      setNotice(result.error);
      return;
    }
    setPlan(result.plan);
    setNotice(ok);
    setSavedTick((n) => n + 1);
  }

  function onDrop(id: string) {
    if (!plan || !drag || drag.id !== id) {
      setDrag(null);
      return;
    }
    const result = moveItem(plan, id, drag.x, drag.y);
    setDrag(null);
    if (result.error) setNotice(`${result.error} It stayed in the last valid spot.`);
    else {
      setPlan(result.plan);
      setNotice(null);
      setSavedTick((n) => n + 1);
    }
  }

  function ask(text: string) {
    if (!plan) return;
    const result = assistant(plan, text, selected);
    setChat((items) => [...items, { role: "you", text }, { role: "assistant", text: result.message }]);
    if (result.plan !== plan) {
      setPlan(result.plan);
      setSavedTick((n) => n + 1);
    }
    if (text.toLowerCase().includes("shopping")) setTab("list");
  }

  function exportList() {
    if (!plan) return;
    const blob = new Blob([shoppingCsv(plan)], { type: "text/csv" });
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = "studio-shopping-list.csv";
    link.click();
    URL.revokeObjectURL(url);
  }

  const products = CATALOG.filter((product) => (category === "all" || product.category === category) && product.name.toLowerCase().includes(query.toLowerCase()));

  if (!plan || !totals) return <div className="p-6 text-sm text-ink-muted">Opening the sample studio…</div>;

  return (
    <div className="@container flex h-full min-h-0 flex-col overflow-hidden bg-canvas">
      <header className="flex shrink-0 items-center gap-3 border-b border-line bg-surface px-3 py-2">
        <div className="w-28 shrink-0 @3xl:w-auto">
          <div className="truncate text-sm font-semibold">Design Studio</div>
          <div className="truncate text-[11px] text-ink-muted">{plan.title}</div>
        </div>
        <div className="flex min-w-0 flex-1 items-center justify-end gap-1 overflow-x-auto">
        <Badge tone="warn">Sample studio</Badge>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => apply(undo(plan), "Undid the last change.")}>Undo</Button>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => apply(redo(plan), "Redid the change.")}>Redo</Button>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => setView(view === "iso" ? "top" : "iso")}>{view === "iso" ? "Top down" : "Isometric"}</Button>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => { setView("iso"); setCameraNonce((n) => n + 1); }}>Reset view</Button>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => setMapOpen((open) => !open)}>{mapOpen ? "Hide plan" : "Floor plan"}</Button>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => setCatalogOpen((open) => !open)}>{catalogOpen ? "Hide products" : "Products"}</Button>
        <Button variant="ghost" className="shrink-0 px-2 py-1 text-xs" onClick={() => { setPlan(resetDemo()); setSelected(null); setChat([{ role: "assistant", text: "This is a demo assistant. I can furnish the treatment rooms, swap in a cheaper bed, or lock the cabinet. I am not a connected model." }]); setNotice("Restored the sample studio."); }}>Reset</Button>
        </div>
      </header>
      <div className={`grid min-h-0 flex-1 grid-rows-[minmax(0,1fr)_minmax(230px,34%)] ${catalogOpen ? "grid-cols-2 @3xl:grid-cols-[16rem_minmax(0,1fr)_16rem]" : "grid-cols-1 @3xl:grid-cols-[minmax(0,1fr)_16rem]"} @3xl:grid-rows-1`}>
        {catalogOpen && (
          <aside className="col-start-1 row-start-2 min-h-0 min-w-0 overflow-auto border-t border-line bg-surface @3xl:row-start-1 @3xl:border-r @3xl:border-t-0">
            <div className="space-y-2 border-b border-line p-3">
              <input className="w-full rounded-lg border border-line px-3 py-2 text-sm" placeholder="Search products" value={query} onChange={(e) => setQuery(e.target.value)} />
              <div className="flex flex-wrap gap-1">
                {CATEGORIES.map((entry) => (
                  <button key={entry.id} className={`rounded-full px-2 py-1 text-xs ${category === entry.id ? "bg-sage text-white" : "bg-sage-soft"}`} onClick={() => setCategory(entry.id)}>{entry.label}</button>
                ))}
              </div>
              <label className="block text-xs text-ink-muted">Add to
                <select className="mt-1 w-full rounded-lg border border-line bg-surface p-2 text-sm text-ink" value={roomId} onChange={(e) => setRoomId(e.target.value)}>
                  {plan.rooms.map((room) => <option key={room.room_id} value={room.room_id}>{room.label}</option>)}
                </select>
              </label>
            </div>
            <ul className="space-y-3 p-3">
              {products.map((product) => (
                <ProductCard
                  key={product.product_id}
                  product={product}
                  canReplace={Boolean(selectedItem)}
                  onAdd={() => apply(addProduct(plan, product, roomId), `Added ${product.name} to ${roomLabel(plan, roomId)}.`) }
                  onReplace={() => selected && apply(replaceItem(plan, selected, product), `Replaced the selection with ${product.name}.`)}
                />
              ))}
            </ul>
          </aside>
        )}
        <div className={`relative row-start-1 min-h-0 min-w-0 ${catalogOpen ? "col-span-2 @3xl:col-span-1 @3xl:col-start-2" : "col-span-1 @3xl:col-start-1"}`}>
          <div className="absolute inset-0">
          <Dollhouse
            rooms={plan.rooms}
            doors={plan.doors}
            items={shown}
            selectedId={selected}
            invalidId={invalidId}
            view={view}
            nonce={cameraNonce}
            onSelect={setSelected}
            onMove={(id, x, y) => setDrag({ id, x, y })}
            onDrop={onDrop}
          />
          </div>
          {notice && <div className="absolute left-3 top-3 z-10 max-w-sm rounded-xl bg-surface px-3 py-2 text-sm shadow">{notice}</div>}
          <div className="absolute bottom-3 left-3 right-3 flex items-end justify-between gap-3">
            <div className="rounded-xl bg-surface/95 px-3 py-2 text-sm shadow">
              <div className="font-medium">Furniture {money(totals.subtotal)}</div>
              <div className="text-xs text-ink-muted">{totals.remaining == null ? "Remaining unknown until every price is known." : `${money(totals.remaining)} left of the $8,000 demo budget`}</div>
            </div>
            {mapOpen && <Minimap plan={plan} items={shown} selected={selected} onSelect={setSelected} />}
          </div>
        </div>
        <aside className={`row-start-2 flex min-h-0 min-w-0 flex-col overflow-hidden border-t border-line bg-surface @3xl:row-start-1 @3xl:border-l @3xl:border-t-0 ${catalogOpen ? "col-start-2 @3xl:col-start-3" : "col-start-1 @3xl:col-start-2"}`}>
          <div className="flex border-b border-line text-sm">
            {(["assistant", "item", "list", "trades"] as const).map((name) => (
              <button key={name} className={`flex-1 px-2 py-2 ${tab === name ? "border-b-2 border-sage font-medium" : "text-ink-muted"}`} onClick={() => setTab(name)}>
                {name === "assistant" ? "Assistant" : name === "item" ? "Selected" : name === "list" ? "List" : "Trades"}
              </button>
            ))}
          </div>
          <div className="min-h-0 flex-1 overflow-auto p-3">
            {tab === "assistant" && (
              <div className="space-y-3">
                <p className="text-xs text-ink-muted">Demo assistant. It edits this same scene. It does not call a model.</p>
                <div className="space-y-2">
                  {chat.map((entry, index) => (
                    <div key={index} className={`rounded-xl px-3 py-2 text-sm ${entry.role === "you" ? "bg-sage-soft" : "bg-canvas"}`}>{entry.text}</div>
                  ))}
                </div>
                <div className="flex flex-wrap gap-1">
                  {SUGGESTIONS.map((text) => <button key={text} className="rounded-full bg-gold-soft px-2 py-1 text-left text-xs" onClick={() => ask(text)}>{text}</button>)}
                </div>
                <form className="flex gap-2" onSubmit={(event) => { event.preventDefault(); if (!draft.trim()) return; ask(draft.trim()); setDraft(""); }}>
                  <input className="min-w-0 flex-1 rounded-lg border border-line px-2 py-1 text-sm" value={draft} onChange={(e) => setDraft(e.target.value)} placeholder="Ask the demo assistant" />
                  <Button type="submit">Send</Button>
                </form>
              </div>
            )}
            {tab === "item" && <Inspector plan={plan} item={selectedItem} onMove={(x, y) => selected && apply(moveItem(plan, selected, x, y), "Moved.")} onRotate={() => selected && apply(rotateItem(plan, selected), "Rotated 90°.")} onDuplicate={() => selected && apply(duplicateItem(plan, selected), "Duplicated.")} onLock={() => selectedItem && apply(lockItem(plan, selectedItem.instance_id, !selectedItem.locked), selectedItem.locked ? "Unlocked." : "Locked.")} onRemove={() => selected && apply(removeItem(plan, selected), "Removed.")} />}
            {tab === "list" && <Shopping plan={plan} lines={totals.lines} selected={selected} onSelect={(id) => { setSelected(id); setTab("item"); }} onExport={exportList} />}
            {tab === "trades" && <p className="text-sm text-ink-muted">Contractor discovery stays a draft. This view does not call, text, or hire anyone. Use the signed-in studio API when a project is open.</p>}
          </div>
          <div className="border-t border-line p-3 text-xs text-ink-muted">
            Tax, shipping, and installation are unknown. They are not shown as zero.
          </div>
        </aside>
      </div>
    </div>
  );
}

function ProductCard({ product, canReplace, onAdd, onReplace }: { product: Product; canReplace: boolean; onAdd: () => void; onReplace: () => void }) {
  return (
    <li className="rounded-xl border border-line p-2">
      <div className="flex h-36 items-center justify-center rounded-lg bg-canvas">
        {product.image_urls[0] && (
          // Supplier and sample previews are remote or inline; they are not in the Next image allowlist.
          // eslint-disable-next-line @next/next/no-img-element
          <img alt="" src={product.image_urls[0]} className="max-h-36 max-w-full object-contain" />
        )}
      </div>
      <div className="mt-2 text-sm font-medium">{product.name}</div>
      <div className="text-xs text-ink-muted">{product.merchant} · {money(product.price_cents)}</div>
      <div className="text-xs text-ink-muted">{product.width_m} × {product.depth_m} × {product.height_m} m</div>
      <div className="mt-1"><Badge tone={product.data_origin === "FIXTURE_REAL" ? "gold" : "neutral"}>{originLabel(product.data_origin)}</Badge></div>
      <p className="mt-1 text-[11px] text-ink-muted">{product.notes}</p>
      <div className="mt-2 flex flex-wrap gap-1">
        <Button className="px-2 py-1" onClick={onAdd}>Add to room</Button>
        {product.product_url && <a className="rounded-lg border border-line px-2 py-1 text-xs" href={product.product_url} target="_blank" rel="noreferrer">View product</a>}
        <Button variant="ghost" className="px-2 py-1" disabled={!canReplace} onClick={onReplace}>Replace selected</Button>
      </div>
    </li>
  );
}

function Inspector({ plan, item, onMove, onRotate, onDuplicate, onLock, onRemove }: { plan: Plan; item: Item | null; onMove: (x: number, y: number) => void; onRotate: () => void; onDuplicate: () => void; onLock: () => void; onRemove: () => void }) {
  if (!item) return <p className="text-sm text-ink-muted">Select a piece in the studio, the floor plan, or the shopping list.</p>;
  return <InspectorFields key={`${item.instance_id}:${item.x_m.toFixed(2)}:${item.y_m.toFixed(2)}`} item={item} onMove={onMove} onRotate={onRotate} onDuplicate={onDuplicate} onLock={onLock} onRemove={onRemove} plan={plan} />;
}

function InspectorFields({ plan, item, onMove, onRotate, onDuplicate, onLock, onRemove }: { plan: Plan; item: Item; onMove: (x: number, y: number) => void; onRotate: () => void; onDuplicate: () => void; onLock: () => void; onRemove: () => void }) {
  const [x, setX] = useState(item.x_m.toFixed(2));
  const [y, setY] = useState(item.y_m.toFixed(2));
  return (
    <div className="space-y-2 text-sm">
      <div className="font-medium">{item.name}</div>
      <div className="text-xs text-ink-muted">{roomLabel(plan, item.room_id)} · {money(item.price_cents)} · {item.locked ? "Locked" : "Unlocked"}</div>
      <Badge tone={item.data_origin === "FIXTURE_REAL" ? "gold" : "neutral"}>{originLabel(item.data_origin)}</Badge>
      {item.image_url && (
        // eslint-disable-next-line @next/next/no-img-element
        <img alt="" src={item.image_url} className="h-28 w-full rounded-lg bg-canvas object-contain" />
      )}
      {item.product_url && <a className="block text-xs underline" href={item.product_url} target="_blank" rel="noreferrer">View product</a>}
      <div className="flex gap-2">
        <input className="w-20 rounded border border-line px-2 py-1" value={x} onChange={(e) => setX(e.target.value)} />
        <input className="w-20 rounded border border-line px-2 py-1" value={y} onChange={(e) => setY(e.target.value)} />
        <Button className="px-2 py-1" onClick={() => onMove(Number(x), Number(y))}>Move</Button>
      </div>
      <div className="flex flex-wrap gap-1">
        <Button variant="ghost" className="px-2 py-1" onClick={onRotate}>Rotate</Button>
        <Button variant="ghost" className="px-2 py-1" onClick={onDuplicate}>Duplicate</Button>
        <Button variant="ghost" className="px-2 py-1" onClick={onLock}>{item.locked ? "Unlock" : "Lock"}</Button>
        <Button variant="ghost" className="px-2 py-1" onClick={onRemove}>Delete</Button>
      </div>
    </div>
  );
}

function Shopping({ plan, lines, selected, onSelect, onExport }: { plan: Plan; lines: ReturnType<typeof budget>["lines"]; selected: string | null; onSelect: (id: string) => void; onExport: () => void }) {
  return (
    <div className="space-y-2">
      <Button variant="ghost" onClick={onExport}>Export list</Button>
      {lines.map((line) => (
        <button key={line.key} className={`block w-full rounded-lg border px-2 py-2 text-left text-sm ${line.instance_ids.includes(selected ?? "") ? "border-gold bg-gold-soft" : "border-line"}`} onClick={() => onSelect(line.instance_ids[0])}>
          <div className="font-medium">{line.quantity} × {line.name}</div>
          <div className="text-xs text-ink-muted">{roomLabel(plan, line.room_id)} · {originLabel(line.data_origin)} · {line.owned ? "Owned" : money(line.line_cents)}</div>
        </button>
      ))}
    </div>
  );
}

function Minimap({ plan, items, selected, onSelect }: { plan: Plan; items: Item[]; selected: string | null; onSelect: (id: string) => void }) {
  const pts = plan.rooms.flatMap((room) => room.polygon);
  const xs = pts.map((p) => p[0]);
  const ys = pts.map((p) => p[1]);
  const minX = Math.min(...xs) - 0.3;
  const minY = Math.min(...ys) - 0.3;
  const maxX = Math.max(...xs) + 0.3;
  const maxY = Math.max(...ys) + 0.3;
  return (
    <svg viewBox={`${minX} ${-maxY} ${maxX - minX} ${maxY - minY}`} className="h-36 w-56 rounded-xl border border-line bg-surface shadow">
      {plan.rooms.map((room) => <polygon key={room.room_id} points={room.polygon.map(([x, y]) => `${x},${-y}`).join(" ")} fill="#f4efe6" stroke="#6f7f73" strokeWidth="0.04" />)}
      {items.map((item) => (
        <rect
          key={item.instance_id}
          x={-item.width_m / 2}
          y={-item.depth_m / 2}
          width={item.width_m}
          height={item.depth_m}
          transform={`translate(${item.x_m} ${-item.y_m}) rotate(${-item.rotation_deg})`}
          fill={item.instance_id === selected ? "#b8962e" : "#6f7f73"}
          onClick={() => onSelect(item.instance_id)}
        />
      ))}
    </svg>
  );
}
