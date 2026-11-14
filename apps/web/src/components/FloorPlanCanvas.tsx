"use client";

import { Layer, Line, Stage, Text } from "react-konva";

const COLORS: Record<string, string> = { treatment: "#e6ebe6", reception: "#f4ecd2", storage: "#ece7df" };

/** Read-only plan view. Geometry arrives in meters; scaled for display only. Editing lands in dev/frontend-product. */
type PlanRoom = { room_id: string; room_type: string; area_sqm: number; polygon: ReadonlyArray<ReadonlyArray<number>> };

export default function FloorPlanCanvas({ design, width = 560 }: { design: { room_polygons: PlanRoom[] }; width?: number }) {
  const pts = design.room_polygons.flatMap((r) => r.polygon);
  const maxX = Math.max(...pts.map((p) => p[0]), 1);
  const maxY = Math.max(...pts.map((p) => p[1]), 1);
  const scale = (width - 20) / maxX;
  const height = maxY * scale + 20;
  return (
    <Stage width={width} height={height}>
      <Layer>
        {design.room_polygons.map((r) => (
          <Line
            key={r.room_id}
            points={r.polygon.flatMap(([x, y]) => [10 + x * scale, height - 10 - y * scale])}
            closed
            fill={COLORS[r.room_type] ?? "#eee"}
            stroke="#2b2b2b"
            strokeWidth={1}
          />
        ))}
        {design.room_polygons.map((r) => {
          const cx = r.polygon.reduce((a, p) => a + p[0], 0) / r.polygon.length;
          const cy = r.polygon.reduce((a, p) => a + p[1], 0) / r.polygon.length;
          return <Text key={`${r.room_id}-t`} x={10 + cx * scale - 30} y={height - 10 - cy * scale - 6} text={`${r.room_id}\n${r.area_sqm} m²`} fontSize={11} fill="#2b2b2b" />;
        })}
      </Layer>
    </Stage>
  );
}
