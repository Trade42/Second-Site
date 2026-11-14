"""Oracle for the documented F5 deviation in `floor_geometry_from_model` (part 2 review).

A rotated suite whose rooms are drawn as separate boxes that share only part of a wall: after
the rotation the shared walls are collinear only up to float rounding, with T-junctions that
have no shared vertex. GEOS (Shapely 2.2 / GEOS 3.14) leaves the rooms as separate polygons, so
Python returns the largest single room as the outline. Rust nodes within 1e-12 and returns the
whole suite. This script records Python's answer for one such plan; the Rust test
`providers::rotated_multi_room_plan_keeps_the_whole_suite` pins Rust's answer against it.

Pure function, no database, no network. Deterministic: it regenerates byte-identically.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_geometry_rotated.py)
Output: matterport_geometry_rotated.json
"""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from shapely.geometry import Polygon  # noqa: E402
from shapely.ops import unary_union  # noqa: E402

from app.integrations.matterport import floor_geometry_from_model  # noqa: E402

DEGREES = 11.0
ORIGIN = (2.5, -1.25)
# Three rooms of a 12 m x 8 m suite, each drawn as its own closed box (local frame, meters).
BOXES = [(0.0, 0.0, 5.0, 8.0), (5.0, 1.0, 12.0, 4.5), (5.0, 5.25, 11.4, 8.0)]


def rotate(x: float, y: float) -> tuple[float, float]:
    t = math.radians(DEGREES)
    c, s = math.cos(t), math.sin(t)
    return (ORIGIN[0] + x * c - y * s, ORIGIN[1] + x * s + y * c)


def model() -> dict:
    pts: dict[str, tuple[float, float]] = {}
    edges = []
    for bi, (x0, y0, x1, y1) in enumerate(BOXES):
        ids = [f"b{bi}v{k}" for k in range(4)]
        for k, (x, y) in zip(ids, [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]):
            pts[k] = rotate(x, y)
        edges += [(ids[k], ids[(k + 1) % 4]) for k in range(4)]
    floor = {
        "id": "f",
        "label": "Floor 1",
        "dimensions": {"areaFloor": 80.1},
        "vertices": [{"id": k, "position": {"x": x, "y": y}} for k, (x, y) in pts.items()],
        "edges": [
            {"id": f"e{i}", "type": "wall", "thickness": 0.1, "vertices": [{"id": a}, {"id": b}], "openings": []}
            for i, (a, b) in enumerate(edges)
        ],
    }
    return {"id": "ROTATED3", "name": "Rotated three-room suite", "floors": [floor], "rooms": []}


def main() -> None:
    m = model()
    result = floor_geometry_from_model(m)
    rooms = [Polygon([rotate(x, y) for x, y in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]]) for x0, y0, x1, y1 in BOXES]
    out = {
        "model": m,
        "python_result": result,
        "python_footprint_area_sqm": Polygon(result["floor_boundary"]).area if result else None,
        "room_areas_sqm": [r.area for r in rooms],
        # Shapely's own check of the cause: the faces merge once buffered by 1e-7.
        "union_of_rooms_buffered_1e_7_sqm": unary_union([r.buffer(1e-7) for r in rooms]).area,
    }
    (HERE / "matterport_geometry_rotated.json").write_text(json.dumps(out, indent=1) + "\n")
    print("wrote matterport_geometry_rotated.json:", out["python_footprint_area_sqm"], out["room_areas_sqm"])


if __name__ == "__main__":
    main()
