"use client";

import { OrbitControls } from "@react-three/drei";
import { Canvas, useThree } from "@react-three/fiber";
import { useEffect, useMemo, useRef } from "react";
import * as THREE from "three";
import type { Door, Item, Room } from "./demo";

type Bounds = { minX: number; minY: number; maxX: number; maxY: number };

const FLOOR: Record<string, string> = {
  treatment: "#e7d3b8",
  reception: "#efe2cf",
  storage: "#d9cbb8",
};

function shapeGeometry(polygon: [number, number][]) {
  const shape = new THREE.Shape();
  polygon.forEach(([x, y], i) => {
    if (i === 0) shape.moveTo(x, -y);
    else shape.lineTo(x, -y);
  });
  const geometry = new THREE.ShapeGeometry(shape);
  geometry.rotateX(-Math.PI / 2);
  return geometry;
}

function RoomLabel({ text, position }: { text: string; position: [number, number, number] }) {
  const texture = useMemo(() => {
    const canvas = document.createElement("canvas");
    canvas.width = 256;
    canvas.height = 64;
    const ctx = canvas.getContext("2d");
    if (!ctx) return null;
    ctx.fillStyle = "rgba(255,253,249,0.94)";
    ctx.fillRect(16, 8, 224, 48);
    ctx.fillStyle = "#2b2b2b";
    ctx.font = "28px sans-serif";
    ctx.textAlign = "center";
    ctx.fillText(text, 128, 42);
    const map = new THREE.CanvasTexture(canvas);
    map.needsUpdate = true;
    return map;
  }, [text]);
  if (!texture) return null;
  return (
    <sprite position={position} scale={[1.35, 0.34, 1]}>
      <spriteMaterial map={texture} transparent depthTest={false} />
    </sprite>
  );
}

function FloorMesh({ polygon, color }: { polygon: [number, number][]; color: string }) {
  const geometry = useMemo(() => shapeGeometry(polygon), [polygon]);
  return (
    <mesh geometry={geometry} receiveShadow>
      <meshStandardMaterial color={color} />
    </mesh>
  );
}

function wallPieces(polygon: [number, number][], doors: Door[]) {
  const pieces: { mid: [number, number]; length: number; angle: number }[] = [];
  for (let i = 0; i < polygon.length; i++) {
    const a = polygon[i];
    const b = polygon[(i + 1) % polygon.length];
    const dx = b[0] - a[0];
    const dy = b[1] - a[1];
    const len = Math.hypot(dx, dy);
    if (len < 0.05) continue;
    const door = doors.find((opening) => {
      const t = ((opening.x_m - a[0]) * dx + (opening.y_m - a[1]) * dy) / (len * len);
      if (t <= 0.08 || t >= 0.92) return false;
      const px = a[0] + t * dx;
      const py = a[1] + t * dy;
      return Math.hypot(opening.x_m - px, opening.y_m - py) < 0.35;
    });
    if (!door) {
      pieces.push({ mid: [(a[0] + b[0]) / 2, (a[1] + b[1]) / 2], length: len, angle: Math.atan2(dx, dy) });
      continue;
    }
    const t = ((door.x_m - a[0]) * dx + (door.y_m - a[1]) * dy) / (len * len);
    const gap = 0.9 / len;
    const spans: [number, number][] = [
      [0, Math.max(0, t - gap / 2)],
      [Math.min(1, t + gap / 2), 1],
    ];
    for (const [t0, t1] of spans) {
      if (t1 - t0 < 0.04) continue;
      const x0 = a[0] + dx * t0;
      const y0 = a[1] + dy * t0;
      const x1 = a[0] + dx * t1;
      const y1 = a[1] + dy * t1;
      pieces.push({
        mid: [(x0 + x1) / 2, (y0 + y1) / 2],
        length: Math.hypot(x1 - x0, y1 - y0),
        angle: Math.atan2(dx, dy),
      });
    }
  }
  return pieces;
}

function Furniture({ item }: { item: Item }) {
  const { width_m: w, depth_m: d, height_m: h, category } = item;
  const cloth = "#f6f1ea";
  const wood = "#8d6b4a";
  const metal = "#8d9390";
  if (category === "treatment_bed") {
    return (
      <group>
        <mesh position={[0, h * 0.16, 0]} castShadow><boxGeometry args={[w * 0.86, h * 0.22, d * 0.9]} /><meshStandardMaterial color={wood} /></mesh>
        <mesh position={[0, h * 0.38, 0]} castShadow><boxGeometry args={[w, h * 0.22, d]} /><meshStandardMaterial color={cloth} /></mesh>
        <mesh position={[0, h * 0.56, -d * 0.36]} castShadow><boxGeometry args={[w * 0.7, h * 0.16, d * 0.16]} /><meshStandardMaterial color="#e4d9cc" /></mesh>
        <mesh position={[0, h * 0.08, d * 0.28]}><boxGeometry args={[w * 0.55, h * 0.06, d * 0.08]} /><meshStandardMaterial color="#6f7f73" /></mesh>
      </group>
    );
  }
  if (category === "practitioner_stool") {
    return (
      <group>
        <mesh position={[0, h * 0.72, 0]} castShadow><cylinderGeometry args={[w * 0.42, w * 0.42, h * 0.16, 20]} /><meshStandardMaterial color="#6f7f73" /></mesh>
        <mesh position={[0, h * 0.4, 0]}><cylinderGeometry args={[0.03, 0.03, h * 0.48, 8]} /><meshStandardMaterial color={metal} /></mesh>
        <mesh position={[0, h * 0.08, 0]}><cylinderGeometry args={[w * 0.42, w * 0.46, h * 0.08, 6]} /><meshStandardMaterial color="#3d403e" /></mesh>
      </group>
    );
  }
  if (category === "rolling_cart") {
    return (
      <group>
        <mesh position={[0, h * 0.72, 0]} castShadow><boxGeometry args={[w * 0.92, 0.04, d * 0.92]} /><meshStandardMaterial color="#f7f4ee" /></mesh>
        <mesh position={[0, h * 0.48, 0]}><boxGeometry args={[w * 0.92, 0.04, d * 0.92]} /><meshStandardMaterial color="#f7f4ee" /></mesh>
        <mesh position={[0, h * 0.24, 0]}><boxGeometry args={[w * 0.92, 0.04, d * 0.92]} /><meshStandardMaterial color="#f7f4ee" /></mesh>
        {[[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([sx, sz]) => (
          <mesh key={`${sx}${sz}`} position={[sx * w * 0.38, h * 0.46, sz * d * 0.38]}><boxGeometry args={[0.03, h * 0.8, 0.03]} /><meshStandardMaterial color="#b8962e" /></mesh>
        ))}
        {[[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([sx, sz]) => (
          <mesh key={`w${sx}${sz}`} position={[sx * w * 0.38, 0.05, sz * d * 0.38]}><cylinderGeometry args={[0.04, 0.04, 0.06, 10]} /><meshStandardMaterial color="#2b2b2b" /></mesh>
        ))}
      </group>
    );
  }
  if (category === "cabinet") {
    return (
      <group>
        <mesh position={[0, h * 0.46, 0]} castShadow><boxGeometry args={[w, h * 0.84, d]} /><meshStandardMaterial color="#2f3332" /></mesh>
        <mesh position={[0, h * 0.9, 0]}><boxGeometry args={[w * 1.02, h * 0.06, d * 1.02]} /><meshStandardMaterial color="#f4efe6" /></mesh>
        <mesh position={[-w * 0.22, h * 0.46, d * 0.48]}><boxGeometry args={[0.03, 0.08, 0.02]} /><meshStandardMaterial color="#b8962e" /></mesh>
        <mesh position={[w * 0.22, h * 0.46, d * 0.48]}><boxGeometry args={[0.03, 0.08, 0.02]} /><meshStandardMaterial color="#b8962e" /></mesh>
      </group>
    );
  }
  if (category === "reception_desk") {
    return (
      <group>
        <mesh position={[0, h * 0.7, 0]} castShadow><boxGeometry args={[w, h * 0.16, d]} /><meshStandardMaterial color="#f7f1e8" /></mesh>
        <mesh position={[0, h * 0.32, d * 0.12]}><boxGeometry args={[w * 0.96, h * 0.55, d * 0.55]} /><meshStandardMaterial color="#6f7f73" /></mesh>
      </group>
    );
  }
  if (category === "waiting_chair") {
    return (
      <group>
        <mesh position={[0, h * 0.42, 0.02]} castShadow><boxGeometry args={[w * 0.86, h * 0.12, d * 0.8]} /><meshStandardMaterial color="#6f7f73" /></mesh>
        <mesh position={[0, h * 0.7, -d * 0.32]} castShadow><boxGeometry args={[w * 0.86, h * 0.42, d * 0.12]} /><meshStandardMaterial color="#6f7f73" /></mesh>
        {[[-1, -1], [1, -1], [-1, 1], [1, 1]].map(([sx, sz]) => (
          <mesh key={`${sx}${sz}`} position={[sx * w * 0.32, h * 0.16, sz * d * 0.28]}><boxGeometry args={[0.04, h * 0.32, 0.04]} /><meshStandardMaterial color={wood} /></mesh>
        ))}
      </group>
    );
  }
  return (
    <group>
      <mesh position={[0, 0.04, 0]}><cylinderGeometry args={[w * 0.35, w * 0.4, 0.06, 12]} /><meshStandardMaterial color="#2b2b2b" /></mesh>
      <mesh position={[0, h * 0.45, 0]}><cylinderGeometry args={[0.02, 0.02, h * 0.7, 8]} /><meshStandardMaterial color={metal} /></mesh>
      <mesh position={[w * 0.2, h * 0.82, 0]} rotation={[0, 0, -0.6]}><cylinderGeometry args={[0.015, 0.015, w * 0.55, 8]} /><meshStandardMaterial color={metal} /></mesh>
      <mesh position={[w * 0.35, h * 0.72, 0]}><sphereGeometry args={[0.07, 16, 12]} /><meshStandardMaterial color="#f4ecd2" emissive="#b8962e" emissiveIntensity={0.35} /></mesh>
    </group>
  );
}

function Piece({
  item,
  selected,
  invalid,
  onSelect,
  onMove,
  onDrop,
}: {
  item: Item;
  selected: boolean;
  invalid: boolean;
  onSelect: (id: string) => void;
  onMove: (id: string, x: number, y: number) => void;
  onDrop: (id: string) => void;
}) {
  const controls = useThree((state) => state.controls) as { enabled: boolean } | null;
  const camera = useThree((state) => state.camera);
  const gl = useThree((state) => state.gl);
  const dragging = useRef(false);
  const plane = useMemo(() => new THREE.Plane(new THREE.Vector3(0, 1, 0), 0), []);
  const hit = useMemo(() => new THREE.Vector3(), []);
  const pointer = useMemo(() => new THREE.Vector2(), []);
  const raycaster = useMemo(() => new THREE.Raycaster(), []);

  function track(event: PointerEvent) {
    if (!dragging.current) return;
    const rect = gl.domElement.getBoundingClientRect();
    pointer.x = ((event.clientX - rect.left) / rect.width) * 2 - 1;
    pointer.y = -((event.clientY - rect.top) / rect.height) * 2 + 1;
    raycaster.setFromCamera(pointer, camera);
    if (!raycaster.ray.intersectPlane(plane, hit)) return;
    onMove(item.instance_id, hit.x, hit.z);
  }

  function end() {
    if (!dragging.current) return;
    dragging.current = false;
    if (controls) controls.enabled = true;
    window.removeEventListener("pointermove", track);
    window.removeEventListener("pointerup", end);
    onDrop(item.instance_id);
  }

  return (
    <group
      position={[item.x_m, 0, item.y_m]}
      rotation={[0, -THREE.MathUtils.degToRad(item.rotation_deg), 0]}
      onClick={(event) => {
        event.stopPropagation();
        onSelect(item.instance_id);
      }}
      onPointerDown={(event) => {
        event.stopPropagation();
        dragging.current = true;
        if (controls) controls.enabled = false;
        onSelect(item.instance_id);
        window.addEventListener("pointermove", track);
        window.addEventListener("pointerup", end);
      }}
    >
      <Furniture item={item} />
      <mesh rotation={[-Math.PI / 2, 0, 0]} position={[0, 0.02, 0]}>
        <ringGeometry args={[Math.max(item.width_m, item.depth_m) * 0.55, Math.max(item.width_m, item.depth_m) * 0.62, 28]} />
        <meshBasicMaterial color={invalid ? "#a14a3b" : "#b8962e"} transparent opacity={selected || invalid ? 0.9 : 0} />
      </mesh>
    </group>
  );
}

function CameraRig({ bounds, view, nonce }: { bounds: Bounds; view: "iso" | "top"; nonce: number }) {
  const camera = useThree((state) => state.camera);
  const size = useThree((state) => state.size);
  const controls = useThree((state) => state.controls) as { target: THREE.Vector3; update: () => void } | null;
  useEffect(() => {
    const persp = camera as THREE.PerspectiveCamera;
    const cx = (bounds.minX + bounds.maxX) / 2;
    const cz = (bounds.minY + bounds.maxY) / 2;
    const spanX = bounds.maxX - bounds.minX + 2;
    const spanZ = bounds.maxY - bounds.minY + 2;
    const vFov = THREE.MathUtils.degToRad(persp.fov);
    const aspect = size.width / Math.max(size.height, 1);
    const hFov = 2 * Math.atan(Math.tan(vFov / 2) * aspect);
    const dist = Math.max(spanZ / 2 / Math.tan(vFov / 2), spanX / 2 / Math.tan(hFov / 2)) * 1.05;
    camera.position.set(cx + (view === "top" ? 0 : dist * 0.2), dist * (view === "top" ? 1 : 0.72), cz + (view === "top" ? 0.01 : dist * 0.62));
    camera.lookAt(cx, 0, cz);
    if (controls) {
      controls.target.set(cx, 0, cz);
      controls.update();
    }
  }, [bounds.maxX, bounds.maxY, bounds.minX, bounds.minY, camera, controls, nonce, size.height, size.width, view]);
  return null;
}

export function Dollhouse({
  rooms,
  doors,
  items,
  selectedId,
  invalidId,
  view,
  nonce,
  onSelect,
  onMove,
  onDrop,
}: {
  rooms: Room[];
  doors: Door[];
  items: Item[];
  selectedId: string | null;
  invalidId: string | null;
  view: "iso" | "top";
  nonce: number;
  onSelect: (id: string | null) => void;
  onMove: (id: string, x: number, y: number) => void;
  onDrop: (id: string) => void;
}) {
  const bounds = useMemo(() => {
    const pts = rooms.flatMap((room) => room.polygon);
    const xs = pts.map((p) => p[0]);
    const ys = pts.map((p) => p[1]);
    return { minX: Math.min(...xs), minY: Math.min(...ys), maxX: Math.max(...xs), maxY: Math.max(...ys) };
  }, [rooms]);

  return (
    <Canvas shadows camera={{ position: [6, 8, 12], fov: 38 }} onPointerMissed={() => onSelect(null)}>
      <color attach="background" args={["#f3eee6"]} />
      <ambientLight intensity={0.55} />
      <directionalLight position={[4, 12, 6]} intensity={1.35} castShadow shadow-mapSize={[1024, 1024]} />
      <hemisphereLight args={["#fff6ea", "#c8b8a2", 0.45]} />
      {rooms.map((room) => (
        <group key={room.room_id}>
          <FloorMesh polygon={room.polygon} color={FLOOR[room.room_type] ?? "#e7d3b8"} />
          {wallPieces(room.polygon, doors).map((wall) => (
            <mesh
              key={`${wall.mid.join(",")}-${wall.length}`}
              position={[wall.mid[0], 0.52, wall.mid[1]]}
              rotation={[0, wall.angle, 0]}
              castShadow
              receiveShadow
            >
              <boxGeometry args={[0.08, 1.05, wall.length]} />
              <meshStandardMaterial color="#f7f4ee" />
            </mesh>
          ))}
          <RoomLabel
            text={room.label}
            position={[
              room.polygon.reduce((sum, point) => sum + point[0], 0) / room.polygon.length,
              1.2,
              room.polygon.reduce((sum, point) => sum + point[1], 0) / room.polygon.length,
            ]}
          />
        </group>
      ))}
      {items.map((item) => (
        <Piece
          key={item.instance_id}
          item={item}
          selected={item.instance_id === selectedId}
          invalid={item.instance_id === invalidId}
          onSelect={onSelect}
          onMove={onMove}
          onDrop={onDrop}
        />
      ))}
      <CameraRig bounds={bounds} view={view} nonce={nonce} />
      <OrbitControls makeDefault maxPolarAngle={Math.PI / 2.15} minDistance={3} maxDistance={22} />
    </Canvas>
  );
}
