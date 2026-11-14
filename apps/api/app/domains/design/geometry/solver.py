"""Constraint-based layout generator (OR-Tools CP-SAT) for rectangular measured spaces.

Foundation scope (property-spatial extends): rectangle boundary, entrance on the y=0 wall.
Two topologies:
  A "spine"  — corridor perpendicular to the entrance wall, rooms in strips on both sides.
  B "front"  — corridor parallel to the entrance wall; reception/storage in front band, rooms behind.
Rooms are assigned to strips and given lengths by CP-SAT (cm integers); output is validated with Shapely.
An infeasible request returns a report with issues and NO design — it never overwrites a valid version.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

from ortools.sat.python import cp_model
from shapely.geometry import Polygon

from app.contracts import (
    GeometryValidationReport,
    LayoutPreferences,
    RoomPolygon,
    RoomRequirement,
    SiteGeometry,
    ValidationIssue,
)
from app.domains.spatial.validation import validate_layout


@dataclass
class Strip:
    name: str
    axis: str  # "x": rooms laid along x; "y": rooms laid along y
    origin: tuple[float, float]
    span: float  # length available along the axis (m)
    depth: float  # room depth perpendicular to the axis (m)
    reserved: list[tuple[float, float]]  # (start, end) along axis kept clear (m)


@dataclass
class Candidate:
    name: str
    rooms: list[RoomPolygon]
    circulation: list[list[tuple[float, float]]]
    report: GeometryValidationReport


def _bbox(site: SiteGeometry) -> tuple[float, float, float, float]:
    poly = Polygon(site.floor_boundary)
    if abs(poly.area - poly.envelope.area) > 1e-6:
        raise ValueError("foundation solver supports rectangular floors only")
    return poly.bounds


def _expand(reqs: list[RoomRequirement]) -> list[RoomRequirement]:
    out = []
    for r in reqs:
        out += [r] * r.count
    return out


def _strips(
    site: SiteGeometry, topology: str, corridor: float
) -> tuple[list[Strip], list[list[tuple[float, float]]]]:
    x0, y0, x1, y1 = _bbox(site)
    W, H = x1 - x0, y1 - y0
    ent = site.entrances[0] if site.entrances else None
    if topology == "spine":
        cx = ((ent.start[0] + ent.end[0]) / 2) if ent else x0 + W / 2
        c0, c1 = cx - corridor / 2, cx + corridor / 2
        circ = [[(c0, y0), (c1, y0), (c1, y1), (c0, y1)]]
        return [
            Strip("left", "y", (x0, y0), H, c0 - x0, []),
            Strip("right", "y", (c1, y0), H, x1 - c1, []),
        ], circ
    front = min(4.0, (H - corridor) / 2)
    cy0, cy1 = y0 + front, y0 + front + corridor
    circ = [[(x0, cy0), (x1, cy0), (x1, cy1), (x0, cy1)]]
    reserved = []
    if ent:
        a, b = sorted([ent.start[0], ent.end[0]])
        reserved.append((a - x0 - 0.3, b - x0 + 0.3))
        circ.append([(a - 0.3, y0), (b + 0.3, y0), (b + 0.3, cy0), (a - 0.3, cy0)])
    return [
        Strip("front", "x", (x0, y0), W, front, reserved),
        Strip("back", "x", (x0, cy1), W, y1 - cy1, []),
    ], circ


def _solve(
    strips: list[Strip],
    rooms: list[RoomRequirement],
    site: SiteGeometry,
    prefs: LayoutPreferences,
    time_limit_s: float = 0.5,
) -> list[tuple[int, int, float, float]] | None:
    """Return (room_idx, strip_idx, start_m, length_m) or None if infeasible."""
    capacity = sum(st.depth * (st.span - sum(b - a for a, b in st.reserved)) for st in strips)
    if sum(r.min_area_sqm for r in rooms) > capacity:
        return None  # fast infeasibility: program exceeds usable strip area
    m = cp_model.CpModel()
    CM = 100
    sink = next((f.location for f in site.fixture_locations if f.kind == "sink"), None)
    place: dict[tuple[int, int], tuple] = {}
    for i, r in enumerate(rooms):
        lits = []
        for s, st in enumerate(strips):
            if st.depth + 1e-9 < r.min_width_m:
                continue
            min_len = max(r.min_width_m, r.min_area_sqm / st.depth)
            lo, hi = math.ceil(min_len * CM), int(st.span * CM)
            if lo > hi:
                continue
            p = m.NewBoolVar(f"p{i}_{s}")
            start = m.NewIntVar(0, hi, f"s{i}_{s}")
            length = m.NewIntVar(lo, hi, f"l{i}_{s}")
            end = m.NewIntVar(0, hi, f"e{i}_{s}")
            iv = m.NewOptionalIntervalVar(start, length, end, p, f"iv{i}_{s}")
            place[(i, s)] = (p, start, length, end, iv, lo)
            lits.append(p)
        if not lits:
            return None
        m.AddExactlyOne(lits)
    for s, st in enumerate(strips):
        ivs = [v[4] for (i, ss), v in place.items() if ss == s]
        for k, (a, b) in enumerate(st.reserved):
            a_c, b_c = max(0, int(a * CM)), min(int(st.span * CM), int(b * CM))
            if b_c > a_c:
                ivs.append(m.NewIntervalVar(a_c, b_c - a_c, b_c, f"res{s}_{k}"))
        m.AddNoOverlap(ivs)
    # Objective: use space (sum of lengths) but keep rooms near min size balanced; pull plumbing rooms toward the sink.
    terms = []
    for (i, s), (p, start, length, end, _iv, _lo) in place.items():
        terms.append(length)
        if sink and prefs.prefer_rooms_near_plumbing and rooms[i].needs_plumbing:
            st = strips[s]
            target = int(((sink[1] - st.origin[1]) if st.axis == "y" else (sink[0] - st.origin[0])) * CM)
            d = m.NewIntVar(0, int(st.span * CM) * 2, f"d{i}_{s}")
            mid2 = m.NewIntVar(0, int(st.span * CM) * 2, f"mid{i}_{s}")
            m.Add(mid2 == start + end).OnlyEnforceIf(p)
            m.Add(mid2 == 0).OnlyEnforceIf(p.Not())
            m.AddAbsEquality(d, mid2 - 2 * target)
            dd = m.NewIntVar(0, int(st.span * CM) * 2, f"dd{i}_{s}")
            m.Add(dd == d).OnlyEnforceIf(p)
            m.Add(dd == 0).OnlyEnforceIf(p.Not())
            terms.append(-dd)
    # Cap each room length at 1.6x its minimum to avoid one giant room.
    for (_i, _s), (p, _start, length, _end, _iv, lo) in place.items():
        cap = int(lo * 1.6)
        m.Add(length <= max(cap, lo)).OnlyEnforceIf(p)
    m.Maximize(sum(terms))
    solver = cp_model.CpSolver()
    solver.parameters.max_deterministic_time = time_limit_s  # deterministic work budget, not wall clock
    solver.parameters.max_time_in_seconds = 30.0
    solver.parameters.num_workers = 1
    solver.parameters.random_seed = 7  # deterministic
    status = solver.Solve(m)
    if status not in (cp_model.OPTIMAL, cp_model.FEASIBLE):
        return None
    out = []
    for (i, s), (p, start, length, _end, _iv, _lo) in place.items():
        if solver.Value(p):
            out.append((i, s, solver.Value(start) / CM, solver.Value(length) / CM))
    return sorted(out)


def _to_polygon(st: Strip, start: float, length: float) -> list[tuple[float, float]]:
    ox, oy = st.origin
    if st.axis == "y":
        return [
            (ox, oy + start),
            (ox + st.depth, oy + start),
            (ox + st.depth, oy + start + length),
            (ox, oy + start + length),
        ]
    return [
        (ox + start, oy),
        (ox + start + length, oy),
        (ox + start + length, oy + st.depth),
        (ox + start, oy + st.depth),
    ]


def generate_candidates(
    site: SiteGeometry, requirements: list[RoomRequirement], prefs: LayoutPreferences, count: int = 2
) -> list[Candidate]:
    rooms = _expand(requirements)
    out: list[Candidate] = []
    for topology in ("spine", "front")[: max(1, count)]:
        strips, circ = _strips(site, topology, prefs.circulation_width_m)
        sol = _solve(strips, rooms, site, prefs)
        if sol is None:
            out.append(
                Candidate(
                    topology,
                    [],
                    circ,
                    GeometryValidationReport(
                        valid=False,
                        issues=[
                            ValidationIssue(
                                code="INFEASIBLE", message=f"No {topology} layout satisfies the room program"
                            )
                        ],
                    ),
                )
            )
            continue
        counters: dict[str, int] = {}
        polys = []
        for i, s, start, length in sol:
            rt = rooms[i].room_type
            counters[rt] = counters.get(rt, 0) + 1
            pts = [(round(x, 3), round(y, 3)) for x, y in _to_polygon(strips[s], start, length)]
            polys.append(
                RoomPolygon(
                    room_id=f"{rt}-{counters[rt]}",
                    room_type=rt,
                    polygon=pts,
                    area_sqm=round(Polygon(pts).area, 2),
                )
            )
        out.append(Candidate(topology, polys, circ, validate_layout(site, polys, requirements, circ)))
    return out
