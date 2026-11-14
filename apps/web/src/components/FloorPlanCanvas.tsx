"use client";

import { Layer, Line, Stage, Text } from "react-konva";

const COLORS: Record<string, string> = { treatment: "#e6ebe6", reception: "#f4ecd2", storage: "#ece7df" };

/** Read-only plan view. Geometry arrives in meters; scaled for display only. Editing lands in dev/frontend-product. */
type Pt = ReadonlyArray<number>;
type PlanRoom = { room_id: string; room_type: string; area_sqm: number; polygon: ReadonlyArray<Pt> };
export type PlanSite = {
  floor_boundary: ReadonlyArray<Pt>;
  fixed_walls?: ReadonlyArray<ReadonlyArray<Pt>>;
  entrances?: ReadonlyArray<{ start: Pt; end: Pt }>;
};

export default function FloorPlanCanvas({ design, site, width = 560 }: { design: { room_polygons: PlanRoom[] }; site?: PlanSite | null; width?: number }) {
  const pts = [...design.room_polygons.flatMap((r) => r.polygon), ...(site?.floor_boundary ?? [])];
  const minX = Math.min(...pts.map((p) => p[0]), 0);
  const minY = Math.min(...pts.map((p) => p[1]), 0);
  const maxX = Math.max(...pts.map((p) => p[0]), 1);
  const maxY = Math.max(...pts.map((p) => p[1]), 1);
  const scale = (width - 20) / (maxX - minX);
  const height = (maxY - minY) * scale + 20;
  const X = (x: number) => 10 + (x - minX) * scale;
  const Y = (y: number) => height - 10 - (y - minY) * scale;
  const flat = (ring: ReadonlyArray<Pt>) => ring.flatMap(([x, y]) => [X(x), Y(y)]);
  return (
    <Stage width={width} height={height}>
      <Layer>
        {site && <Line points={flat(site.floor_boundary)} closed fill="#fbfaf7" stroke="#2b2b2b" strokeWidth={3} />}
        {site?.fixed_walls?.map((w, i) => (
          <Line key={`w${i}`} points={flat(w)} stroke="#a7a39b" strokeWidth={1.5} dash={[5, 4]} />
        ))}
        {design.room_polygons.map((r) => (
          <Line key={r.room_id} points={flat(r.polygon)} closed fill={COLORS[r.room_type] ?? "#eee"} stroke="#2b2b2b" strokeWidth={1} opacity={0.92} />
        ))}
        {site?.entrances?.map((e, i) => (
          <Line key={`e${i}`} points={flat([e.start, e.end])} stroke="#b8932f" strokeWidth={5} />
        ))}
        {design.room_polygons.map((r) => {
          const cx = r.polygon.reduce((a, p) => a + p[0], 0) / r.polygon.length;
          const cy = r.polygon.reduce((a, p) => a + p[1], 0) / r.polygon.length;
          return <Text key={`${r.room_id}-t`} x={X(cx) - 30} y={Y(cy) - 6} text={`${r.room_id}\n${r.area_sqm} m²`} fontSize={11} fill="#2b2b2b" />;
        })}
      </Layer>
    </Stage>
  );
}
