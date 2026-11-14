"""Fit the rectangular CP-SAT solver to real (measured, non-rectangular) floors.

1. `normalize_plan` (used when ingesting Matterport geometry): rotate the plan so its dominant wall direction
   is axis-aligned and translate it to a positive local frame.
2. `frame_for_solver`: find the largest axis-aligned rectangle inside the floor boundary, pick the measured
   entrance nearest to it, and rotate the rectangle (multiples of 90 degrees) so that entrance lies on y=0.
   The solver runs in that frame; `to_site` maps results back onto the real plan.
Assumptions are returned as notes and surfaced in the validation report: space outside the rectangle is left
unprogrammed, and existing interior partitions are treated as removable (verify they are non-structural).
"""

from __future__ import annotations

import math
from collections.abc import Callable
from dataclasses import dataclass

from shapely import affinity
from shapely.geometry import LineString, Point, Polygon, box
from shapely.prepared import prep

from app.contracts import Opening, SiteGeometry

Pt = tuple[float, float]


def dominant_angle(coords: list[Pt]) -> float:
    """Length-weighted wall direction in degrees, folded into [-45, 45)."""
    acc: dict[int, float] = {}
    for (x0, y0), (x1, y1) in zip(coords, coords[1:] + coords[:1], strict=False):
        length = math.hypot(x1 - x0, y1 - y0)
        if length < 0.3:
            continue
        ang = math.degrees(math.atan2(y1 - y0, x1 - x0)) % 90
        if ang >= 45:
            ang -= 90
        acc[round(ang)] = acc.get(round(ang), 0) + length
    return float(max(acc, key=acc.get)) if acc else 0.0


def normalize_plan(doc: dict) -> dict:
    """Rotate/translate every coordinate in a geometry document into an axis-aligned positive frame."""
    angle = dominant_angle([tuple(p) for p in doc["floor_boundary"]])
    poly = affinity.rotate(Polygon(doc["floor_boundary"]), -angle, origin=(0, 0))
    minx, miny = poly.bounds[0], poly.bounds[1]

    def f(p) -> list[float]:
        x, y = p
        r = math.radians(-angle)
        xr, yr = x * math.cos(r) - y * math.sin(r), x * math.sin(r) + y * math.cos(r)
        return [round(xr - minx, 3), round(yr - miny, 3)]

    out = dict(doc)
    out["floor_boundary"] = [f(p) for p in doc["floor_boundary"]]
    out["fixed_walls"] = [[f(a), f(b)] for a, b in doc.get("fixed_walls", [])]
    for key in ("entrances", "windows", "interior_doors"):
        out[key] = [{**o, "start": f(o["start"]), "end": f(o["end"])} for o in doc.get(key, [])]
    out["coordinate_reference"] = f"local_floor_origin (rotated {-angle:.0f} deg from provider frame)"
    return out


def largest_inner_rectangle(poly: Polygon, cell: float = 0.2) -> tuple[float, float, float, float] | None:
    """Largest axis-aligned rectangle (on a `cell` grid) contained in `poly`. Classic histogram method."""
    minx, miny, maxx, maxy = poly.bounds
    nx, ny = int((maxx - minx) / cell), int((maxy - miny) / cell)
    if nx < 2 or ny < 2:
        return None
    pp = prep(poly.buffer(1e-6))
    inside = [
        [
            pp.contains(box(minx + i * cell, miny + j * cell, minx + (i + 1) * cell, miny + (j + 1) * cell))
            for i in range(nx)
        ]
        for j in range(ny)
    ]
    best, best_rect = 0.0, None
    heights = [0] * nx
    for j in range(ny):
        for i in range(nx):
            heights[i] = heights[i] + 1 if inside[j][i] else 0
        stack: list[int] = []
        for i in range(nx + 1):
            h = heights[i] if i < nx else 0
            while stack and heights[stack[-1]] >= h:
                top = stack.pop()
                left = stack[-1] + 1 if stack else 0
                area = heights[top] * (i - left)
                if area > best:
                    best = area
                    best_rect = (left, j - heights[top] + 1, i, j + 1)
            stack.append(i)
    if best_rect is None:
        return None
    i0, j0, i1, j1 = best_rect
    return (minx + i0 * cell, miny + j0 * cell, minx + i1 * cell, miny + j1 * cell)


@dataclass
class Frame:
    site: SiteGeometry
    to_site: Callable[[Pt], Pt]
    notes: list[str]


def _is_solver_ready(site: SiteGeometry) -> bool:
    poly = Polygon(site.floor_boundary)
    if abs(poly.area - poly.envelope.area) > 1e-6:
        return False
    y0 = poly.bounds[1]
    return not site.entrances or all(
        abs(o.start[1] - y0) < 1e-6 and abs(o.end[1] - y0) < 1e-6 for o in site.entrances
    )


def frame_for_solver(site: SiteGeometry, min_area: float = 20.0) -> Frame | None:
    if _is_solver_ready(site):
        return Frame(site, lambda p: p, [])
    poly = Polygon(site.floor_boundary)
    rect = largest_inner_rectangle(poly)
    if rect is None:
        return None
    x0, y0, x1, y1 = rect
    W, H = x1 - x0, y1 - y0
    if W * H < min_area:
        return None
    notes = [
        f"Layout uses the largest rectangular area of the measured floor ({W:.1f} x {H:.1f} m = {W * H:.0f} m2 "
        f"of {poly.area:.0f} m2); the rest is left unprogrammed.",
    ]
    if site.fixed_walls:
        notes.append(
            f"{len(site.fixed_walls)} existing interior partition(s) assumed removable; verify they are non-structural."
        )
    rbox = box(x0, y0, x1, y1)
    sides = {
        "bottom": LineString([(x0, y0), (x1, y0)]),
        "top": LineString([(x0, y1), (x1, y1)]),
        "left": LineString([(x0, y0), (x0, y1)]),
        "right": LineString([(x1, y0), (x1, y1)]),
    }
    best = None
    for o in site.entrances:
        mid = Point((o.start[0] + o.end[0]) / 2, (o.start[1] + o.end[1]) / 2)
        for name, line in sides.items():
            d = line.distance(mid)
            if best is None or d < best[0]:
                best = (d, name, o)
    side = best[1] if best else "bottom"
    if best is None:
        notes.append("No measured entrance on the outer wall; entrance assumed at the centre of one side.")
    elif best[0] > 1.0:
        notes.append(
            f"Nearest measured entrance is {best[0]:.1f} m from the layout area; a corridor connection is assumed."
        )

    # forward (site -> frame) and inverse maps for the chosen side
    def fwd(p: Pt) -> Pt:
        x, y = p[0] - x0, p[1] - y0
        return {"bottom": (x, y), "top": (W - x, H - y), "left": (H - y, x), "right": (y, W - x)}[side]

    def inv(p: Pt) -> Pt:
        u, v = p
        x, y = {"bottom": (u, v), "top": (W - u, H - v), "left": (v, H - u), "right": (W - v, u)}[side]
        return (round(x + x0, 3), round(y + y0, 3))

    fw, fh = (W, H) if side in ("bottom", "top") else (H, W)
    if best is not None:
        (u0, _), (u1, _) = fwd(best[2].start), fwd(best[2].end)
        lo, hi = sorted((min(max(u0, 0.0), fw), min(max(u1, 0.0), fw)))
    else:
        lo = hi = fw / 2
    if hi - lo < 0.9:
        c = min(max((lo + hi) / 2, 0.45), fw - 0.45)
        lo, hi = c - 0.45, c + 0.45
    obstacles = []
    for ob in site.fixed_obstacles:
        op = Polygon(ob)
        if op.intersects(rbox):
            clipped = op.intersection(rbox)
            if clipped.area > 0.01 and isinstance(clipped, Polygon):
                obstacles.append([fwd(p) for p in list(clipped.exterior.coords)[:-1]])
    framed = site.model_copy(
        update={
            "floor_boundary": [(0.0, 0.0), (fw, 0.0), (fw, fh), (0.0, fh)],
            "fixed_obstacles": obstacles,
            "fixed_walls": [],
            "entrances": [Opening(kind="entrance", start=(round(lo, 3), 0.0), end=(round(hi, 3), 0.0))],
            "exits": [],
            "windows": [],
            "fixture_locations": [
                fx.model_copy(update={"location": fwd(fx.location)})
                for fx in site.fixture_locations
                if rbox.contains(Point(fx.location))
            ],
            "restricted_areas": [],
        }
    )
    return Frame(framed, inv, notes)
