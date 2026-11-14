"""Deterministic geometry validation (Shapely). Owner: property-spatial.

Checks: rooms inside the floor boundary, no room overlaps, no rooms in circulation, entrances/exits
kept clear, fixed obstacles respected (usable area excludes them), minimum area and width.
"""

from __future__ import annotations

from shapely.geometry import LineString, Polygon
from shapely.ops import unary_union

from app.contracts import (
    GeometryValidationReport,
    RoomPolygon,
    RoomRequirement,
    SiteGeometry,
    ValidationIssue,
)

EPS = 1e-6


def min_dimension(poly: Polygon) -> float:
    rect = poly.minimum_rotated_rectangle
    xs = list(rect.exterior.coords)
    edges = [LineString([xs[i], xs[i + 1]]).length for i in range(4)]
    return min(edges)


def validate_layout(
    site: SiteGeometry,
    rooms: list[RoomPolygon],
    requirements: list[RoomRequirement],
    circulation: list[list[tuple[float, float]]] | None = None,
) -> GeometryValidationReport:
    issues: list[ValidationIssue] = []
    boundary = Polygon(site.floor_boundary)
    polys = {r.room_id: Polygon(r.polygon) for r in rooms}
    obstacles = unary_union([Polygon(o) for o in site.fixed_obstacles]) if site.fixed_obstacles else None
    req_by_type = {r.room_type: r for r in requirements}

    for rid, poly in polys.items():
        if not poly.is_valid:
            issues.append(
                ValidationIssue(code="INVALID_POLYGON", message=f"{rid} polygon invalid", room_ids=[rid])
            )
            continue
        if not boundary.buffer(EPS).contains(poly):
            issues.append(
                ValidationIssue(
                    code="OUTSIDE_BOUNDARY", message=f"{rid} extends outside the floor", room_ids=[rid]
                )
            )
    ids = list(polys)
    for i, a in enumerate(ids):
        for b in ids[i + 1 :]:
            if polys[a].intersection(polys[b]).area > EPS:
                issues.append(
                    ValidationIssue(code="ROOM_OVERLAP", message=f"{a} overlaps {b}", room_ids=[a, b])
                )
    if circulation:
        circ = unary_union([Polygon(c) for c in circulation])
        for rid, poly in polys.items():
            if poly.intersection(circ).area > EPS:
                issues.append(
                    ValidationIssue(
                        code="BLOCKS_CIRCULATION", message=f"{rid} intrudes into circulation", room_ids=[rid]
                    )
                )
        if obstacles is not None and circ.intersection(obstacles).area > EPS:
            issues.append(
                ValidationIssue(code="OBSTACLE_IN_CIRCULATION", message="fixed obstacle blocks circulation")
            )
    for opening in list(site.entrances) + list(site.exits):
        seg = LineString([opening.start, opening.end])
        for rid, poly in polys.items():
            rt = next((r.room_type for r in rooms if r.room_id == rid), "")
            # Blocked when a non-reception room covers part of the opening itself (touching its end is fine).
            if rt != "reception" and poly.intersection(seg).length > 0.01:
                issues.append(
                    ValidationIssue(
                        code="OPENING_BLOCKED", message=f"{rid} blocks {opening.kind}", room_ids=[rid]
                    )
                )
    for r in rooms:
        poly = polys[r.room_id]
        usable = poly.difference(obstacles) if obstacles is not None else poly
        req = req_by_type.get(r.room_type)
        if req is None:
            continue
        if usable.area + EPS < req.min_area_sqm:
            issues.append(
                ValidationIssue(
                    code="BELOW_MIN_AREA",
                    message=f"{r.room_id} usable area {usable.area:.1f} m2 < {req.min_area_sqm}",
                    room_ids=[r.room_id],
                )
            )
        if min_dimension(poly) + EPS < req.min_width_m:
            issues.append(
                ValidationIssue(
                    code="BELOW_MIN_WIDTH",
                    message=f"{r.room_id} narrower than {req.min_width_m} m",
                    room_ids=[r.room_id],
                )
            )
    expected = sum(r.count for r in requirements)
    if len(rooms) < expected:
        issues.append(
            ValidationIssue(code="MISSING_ROOMS", message=f"{len(rooms)} of {expected} required rooms placed")
        )
    return GeometryValidationReport(valid=not issues, issues=issues)
