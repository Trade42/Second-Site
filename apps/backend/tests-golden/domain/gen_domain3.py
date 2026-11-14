"""Generate ss-domain part 3 golden vectors (geometry contracts, layout validation, framing, layout
solver, finishes, BOM, catalog, timeline, IFC text reader) from the Python implementation, which
is the specification.

Run from the repository root:
    (cd apps/api && .venv/bin/python -B ../backend/tests-golden/domain/gen_domain3.py)

Pure functions only: no database, no network. Reads fixtures/ (never copies them) and writes
apps/backend/tests-golden/domain/part3_*.json:
- part3_geometry.json: Shapely primitives the port reimplements (explain_validity, area,
  min_dimension, buffered containment, segment-in-polygon length, intersection areas) and
  validate_ring messages;
- part3_contracts.json: geometry/procurement contract validation failures (Pydantic errors);
- part3_validation.json: validate_layout cases (input and report);
- part3_framing.json: dominant_angle, normalize_plan, largest_inner_rectangle, frame_for_solver;
- part3_solver.json: layout solver cases with, per topology and corridor offset, the strips,
  CP-SAT status and objective value and the solution, plus the candidates;
- part3_bom.json: finish specifications and bills of materials for designs built from the
  solver candidates, plus BOM rounding vectors;
- part3_misc.json: catalog, timeline, IFC STEP-text reader, and CPython built-ins fixed in part 3
  (str() of lists and dicts, float()/int() of Unicode digits).

All geometry is synthetic test data (FIXTURE_SYNTHETIC), except the repository fixture
fixtures/geometry/hollywood_302_synthetic.json, which is itself synthetic. The L-shaped Matterport
model is copied from tests/test_sponsor_providers.py::_l_shaped_model (synthetic).

Normalization (identical in crates/domain/tests/golden_layout.rs): `checked_at` is dropped from
reports; `bom_id` and `bom_item_id` are dropped from bills of materials. Nothing else.

Reproducibility: uuid4() behind app.contracts.common.new_id is seeded, so recorded ids are stable;
CP-SAT runs with Python's own parameters (one worker, seed 7, deterministic time), so its results
are deterministic on a given machine and OR-Tools version (9.15).
"""

from __future__ import annotations

import json
import math
import random
import sys
from decimal import Decimal
from pathlib import Path
from uuid import UUID

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from ortools.sat.python import cp_model  # noqa: E402
from pydantic import ValidationError  # noqa: E402
from shapely.geometry import LineString, Polygon, box  # noqa: E402
from shapely.validation import explain_validity  # noqa: E402

import app.contracts.common as _common  # noqa: E402
from app.contracts import (  # noqa: E402
    BillOfMaterials,
    BomItem,
    DesignRevisionRequest,
    DesignVersion,
    FinishSpecification,
    Fixture,
    FurniturePlacement,
    GeometryValidationReport,
    LayoutGenerationRequest,
    LayoutPreferences,
    Opening,
    RoomPolygon,
    RoomRequirement,
    SiteGeometry,
    SupplierOffer,
)
from app.contracts.geometry import validate_ring  # noqa: E402
from app.domains.design.geometry import framing as F  # noqa: E402
from app.domains.design.geometry import solver as S  # noqa: E402
from app.domains.design.interior.finishes import finish_specifications  # noqa: E402
from app.domains.logistics.timeline import opening_timeline  # noqa: E402
from app.domains.procurement.bom import generate_bom  # noqa: E402
from app.domains.spatial import validation as V  # noqa: E402
from app.integrations.catalog import ILLUSTRATIVE_UNIT_PRICES, FixtureCatalogProvider  # noqa: E402
from app.integrations.ifc import _read_step_text  # noqa: E402
from app.integrations.matterport import floor_geometry_from_model  # noqa: E402

_RNG = random.Random(0xDFF02230)
_common.uuid4 = lambda: UUID(int=_RNG.getrandbits(128), version=4)

OUT = HERE
PID = "dff02230-0000-4000-8000-0000000000b1"
PROJECT = "dff02230-0000-4000-8000-0000000000a1"


def dump(name: str, value) -> None:
    (OUT / name).write_text(json.dumps(value, indent=1, ensure_ascii=False) + "\n")


def jd(model) -> dict:
    return json.loads(model.model_dump_json())


def report_doc(rep: GeometryValidationReport) -> dict:
    d = jd(rep)
    d.pop("checked_at")
    return d


def site(boundary, entrances=(), exits=(), obstacles=(), fixtures=(), walls=(), windows=(), **kw):
    return SiteGeometry(
        property_id=PID,
        source_id=kw.pop("source_id", "synthetic-geometry"),
        geometry_source=kw.pop("geometry_source", "FIXTURE"),
        data_origin=kw.pop("data_origin", "FIXTURE_SYNTHETIC"),
        floor_boundary=boundary,
        fixed_obstacles=list(obstacles),
        fixed_walls=list(walls),
        entrances=[Opening(kind="entrance", start=a, end=b) for a, b in entrances],
        exits=[Opening(kind="exit", start=a, end=b) for a, b in exits],
        windows=[Opening(kind="window", start=a, end=b) for a, b in windows],
        fixture_locations=[{"kind": k, "location": loc} for k, loc in fixtures],
        **kw,
    )


def rect(x0, y0, x1, y1):
    return [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]


def hollywood() -> SiteGeometry:
    d = json.loads((REPO / "fixtures" / "geometry" / "hollywood_302_synthetic.json").read_text())
    return SiteGeometry(
        property_id=PID,
        source_id=d["source_id"],
        geometry_source="FIXTURE",
        data_origin="FIXTURE_SYNTHETIC",
        floor_boundary=d["floor_boundary"],
        fixed_obstacles=d["fixed_obstacles"],
        entrances=d["entrances"],
        exits=d["exits"],
        fixture_locations=d["fixture_locations"],
    )


def req(rt, count, area, width, plumbing=False):
    return RoomRequirement(
        room_type=rt, count=count, min_area_sqm=area, min_width_m=width, needs_plumbing=plumbing
    )


REQS = [req("treatment", 3, 10, 2.8, True), req("reception", 1, 12, 3), req("storage", 1, 4, 1.5)]


def outcome(fn):
    """Run fn; record its value or the exception Python raises."""
    try:
        return {"ok": fn()}
    except ValidationError as e:
        return {"validation_error": e.errors(include_url=False, include_context=False, include_input=False)}
    except Exception as e:  # noqa: BLE001
        return {"raises": f"{type(e).__name__}: {e}"}


# ---- part3_misc: CPython built-ins fixed in part 3 ----------------------------------------------


def py_units():
    values = [
        [1, 2], ["3/4"], [], {}, {"a": 1}, [1.0, True, None, "x"], [[1, 2], [3]], {"k": [1, "a'b"]},
        ["it's"], [0.1, 1e-05, 1e16], {"a": {"b": None}}, [False], ["tab\there"],
    ]
    strs = [
        "２００", "٣٠٠", " ٣ ", " 150 ", "1_000", "１_０", "٣.5", "12", "-٣", "1e3",
        "\U0001d7ce\U0001d7cf", "०१", "x", "", "²", "1\x1c", "\x1c1", "Ⅷ",
        "߀߁", "+１２", "１ 2",
    ]
    return {
        "str_of_value": [{"value": v, "str": str(v)} for v in values],
        "float_of_str": [{"s": s, **outcome(lambda s=s: float(s))} for s in strs],
        "int_of_str": [{"s": s, **outcome(lambda s=s: int(s))} for s in strs],
    }


# ---- part3_geometry: Shapely primitives --------------------------------------------------------


def rand_poly(rng: random.Random):
    n = rng.randint(3, 8)
    kind = rng.random()
    if kind < 0.35:
        return [(rng.randint(0, 6), rng.randint(0, 6)) for _ in range(n)]
    if kind < 0.7:
        return [(round(rng.uniform(0, 10), 2), round(rng.uniform(0, 10), 2)) for _ in range(n)]
    return [(rng.randint(0, 3) * 0.5, rng.randint(0, 3) * 0.5) for _ in range(n)]


def star(rng, n=7):
    cx, cy = rng.uniform(2, 8), rng.uniform(2, 8)
    pts = []
    for i in range(n):
        a = 2 * math.pi * i / n
        r = rng.uniform(1, 3)
        pts.append((round(cx + r * math.cos(a), 3), round(cy + r * math.sin(a), 3)))
    return pts


def geometry_vectors():
    rng = random.Random(7)
    explain = [
        [(0, 0), (2, 2), (2, 0), (0, 2)], [(0, 0), (1, 0), (1, 1), (0, 1), (0, 0)], [(0, 0), (1, 0), (2, 0)],
        [(0, 0), (1, 1), (0, 0)], [(0, 0), (3, 0), (3, 3), (0, 3), (1.5, 0)], [(0, 0), (4, 0), (4, 4), (2, 0), (0, 4)],
        [(0.1, 0.1), (2.25, 2.25), (2.25, 0.1), (0.1, 2.25)], [(0, 0), (1, 0), (1, 1), (1, 0), (0, 1)],
        [(0, 0), (1, 0), (1, 0), (1, 1), (0, 1)], [(0, 0), (0, 0), (0, 0)], [(0, 0), (1, 0), (0, 0), (1, 0)],
        [(0, 0), (4, 0), (4, 4), (0, 4), (2, 2)], [(0, 0), (10, 0), (10, 10), (5, 10), (5, 5), (5, 10), (0, 10)],
        [(0, 0), (1e-200, 0), (0, 1e-200)], [(0, 0), (1e-7, 0), (0, 1e-7)],
        [(0, 0), (12.0, 0), (12.0, 11.6), (0, 11.6)], [(0.2, 0.2), (0.8, 0.2), (0.8, 0.8), (0.2, 0.8)],
        [(0, 0), (3, 0), (3, 1), (1, 1), (1, 3), (0, 3)], [(1 / 3, 0), (2, 1 / 7), (1, 2)],
    ]
    for _ in range(260):
        explain.append(rand_poly(rng))
    for _ in range(40):
        explain.append(star(rng))
    ex = [{"points": p, "reason": explain_validity(Polygon(p)), "area": Polygon(p).area} for p in explain]

    mind = []
    shapes = [
        rect(0, 0, 3, 2), rect(0, 0, 2.8, 5.4), [(0, 0), (4, 1), (3, 5), (-1, 4)], [(0, 0), (6, 0), (3, 1)],
        [(0, 0), (5, 0), (6, 2), (1, 2)], [(0, 0), (3, 0), (3, 1), (1, 1), (1, 3), (0, 3)],
        [(0, 0), (10, 0.5), (9.5, 1.5), (-0.5, 1)], [(0, 0), (2, 2), (2, 0), (0, 2)],
    ]
    for _ in range(40):
        p = star(rng)
        shapes.append(p)
    for _ in range(20):
        a = rng.uniform(0, math.pi)
        w, h = rng.uniform(1, 6), rng.uniform(1, 6)
        cx, cy = rng.uniform(0, 10), rng.uniform(0, 10)
        pts = []
        for dx, dy in ((-w / 2, -h / 2), (w / 2, -h / 2), (w / 2, h / 2), (-w / 2, h / 2)):
            pts.append((round(cx + dx * math.cos(a) - dy * math.sin(a), 4), round(cy + dx * math.sin(a) + dy * math.cos(a), 4)))
        shapes.append(pts)
    for p in shapes:
        mind.append({"points": p, "min_dimension": V.min_dimension(Polygon(p))})

    contains = []
    outer_shapes = {
        "rect": rect(0, 0, 12, 11.6),
        "l": [(0, 0), (12, 0), (12, 7), (8, 7), (8, 10), (0, 10)],
        "u": [(0, 0), (10, 0), (10, 8), (7, 8), (7, 3), (3, 3), (3, 8), (0, 8)],
        "tilt": [(0, 0), (8, 2), (6, 9), (-2, 7)],
    }
    probes = [
        rect(1, 1, 3, 3), rect(0, 0, 12, 11.6), rect(-5e-7, 0, 4, 4), rect(-2e-6, 0, 4, 4), rect(11, 10, 13, 12),
        rect(8.5, 7.5, 9.5, 9.5), rect(7, 6, 9, 8), rect(0, 0, 12, 7 + 5e-7), rect(0, 0, 12, 7 + 3e-6),
        rect(3.5, 3.5, 6.5, 7.5), rect(3, 3, 7, 4), rect(1, 1, 2, 7.9), rect(0, 0, 3, 8.0000004),
        [(1, 1), (5, 1), (3, 4)], rect(-1, -1, 13, 12.6), rect(2, 0.5, 6, 2.5), rect(0.0000005, 9, 7.9999995, 9.9999995),
    ]
    for name, o in outer_shapes.items():
        for pr in probes:
            contains.append({"outer": name, "inner": pr, "eps": 1e-6, "contains": Polygon(o).buffer(1e-6).contains(Polygon(pr))})
        for _ in range(15):
            x, y = rng.uniform(-1, 11), rng.uniform(-1, 10)
            w, h = rng.uniform(0.1, 4), rng.uniform(0.1, 4)
            pr = rect(round(x, 2), round(y, 2), round(x + w, 2), round(y + h, 2))
            contains.append({"outer": name, "inner": pr, "eps": 1e-6, "contains": Polygon(o).buffer(1e-6).contains(Polygon(pr))})
        for pr in probes[:6]:
            contains.append({"outer": name, "inner": pr, "eps": 0.01, "contains": Polygon(o).buffer(0.01).contains(Polygon(pr))})

    seg = []
    polys = [rect(0, 0, 4, 4), rect(4.4, 0, 9.8, 2.8), [(0, 0), (3, 0), (3, 1), (1, 1), (1, 3), (0, 3)], [(0, 0), (4, 1), (3, 5), (-1, 4)]]
    segs = [((5.0, 0), (6.2, 0)), ((1, 0), (2.5, 0)), ((-1, 0), (5, 0)), ((4, 0), (5, 0)), ((3.99, 0), (6, 0)),
            ((0, -1), (0, 5)), ((2, -1), (2, 5)), ((-1, 2), (5, 2)), ((0.5, 0.5), (0.5, 0.5)), ((0, 4), (4, 0)),
            ((4.4, 0), (4.41, 0)), ((4.4, 0), (4.415, 0)), ((9.8, 0), (11, 0))]
    for i, p in enumerate(polys):
        for a, b in segs:
            seg.append({"polygon": p, "start": a, "end": b, "length": Polygon(p).intersection(LineString([a, b])).length})

    inter = []
    pairs = [(rect(0, 0, 4, 4), rect(3, 3, 7, 7)), (rect(0, 0, 4, 4), rect(4, 0, 8, 4)), (rect(0, 0, 4, 4), rect(1, 1, 2, 2)),
             ([(0, 0), (3, 0), (3, 1), (1, 1), (1, 3), (0, 3)], [(0.5, 0.5), (2.5, 0.5), (2.5, 2.5), (2, 2.5), (2, 1.5), (0.5, 1.5)]),
             ([(0, 0), (3, 0), (3, 1), (1, 1), (1, 3), (0, 3)], rect(0.5, 0.5, 4, 2)),
             (rect(0, 0, 4, 4), [(2, -1), (5, 2), (2, 5), (-1, 2)])]
    for _ in range(30):
        pairs.append((star(rng), star(rng)))
    for a, b in pairs:
        inter.append({"a": a, "b": b, "area": Polygon(a).intersection(Polygon(b)).area})

    rings = []
    for p in explain[:30] + [[(0, 0), (1, 0)], [(0, 0), (1e-200, 0), (1e-200, 1e-200)]]:
        r = outcome(lambda p=p: validate_ring(p, "room polygon"))
        rings.append({"points": p, **({"ok": True} if "ok" in r else r)})
    return {"explain": ex, "min_dimension": mind, "outer_shapes": outer_shapes, "contains": contains,
            "segment_length": seg, "intersection_area": inter, "validate_ring": rings}


# ---- part3_contracts --------------------------------------------------------------------------


def contract_vectors():
    u = str(UUID(int=1))
    base = dict(property_id=u, source_id="s", geometry_source="FIXTURE", data_origin="FIXTURE_SYNTHETIC",
                floor_boundary=[[0, 0], [1, 0], [1, 1]])
    rr = {"room_type": "t", "count": 1, "min_area_sqm": 1, "min_width_m": 1}
    vr = {"valid": False, "issues": [], "checked_at": "2026-11-14T00:00:00Z"}
    dv = dict(project_id=u, property_id=u, version_number=1, geometry_id=u,
              room_polygons=[{"room_id": "a", "room_type": "t", "polygon": [[0, 0], [1, 0], [1, 1]], "area_sqm": 0.5}],
              validation_report=vr)
    cases = [
        ("SiteGeometry", {**base, "floor_boundary": [[0, 0], [1, 0]], "units": "ft", "fixed_obstacles": [[[0, 0], [1, 1], [1, 0], [0, 1]]], "measurement_uncertainty_m": -1}),
        ("SiteGeometry", {**base, "geometry_source": "PHOTO_ESTIMATE", "scale_verified": True}),
        ("SiteGeometry", {**base, "restricted_areas": [[[0, 0], [1, 0], [1, 1]], [[0, 0], [0, 0], [0, 0]]]}),
        ("SiteGeometry", {**base, "floor_boundary": [[0, 0], [2, 2], [2, 0], [0, 2]]}),
        ("SiteGeometry", {**base, "fixed_obstacles": [[[0, 0], [1, 0], [2, 0]]], "geometry_source": "PHOTO_ESTIMATE", "scale_verified": True}),
        ("SiteGeometry", {**base, "floor_boundary": [[0, 0], [1e-200, 0], [0, 1e-200]]}),
        ("SiteGeometry", {**base, "extra": 1}),
        ("SiteGeometry", {**base, "geometry_source": "LIDAR"}),
        ("RoomPolygon", {"room_id": "a", "room_type": "t", "polygon": [[0, 0], [2, 2], [2, 0], [0, 2]], "area_sqm": 0}),
        ("RoomPolygon", {"room_id": "a", "room_type": "t", "polygon": [[0, 0], [1, 0]], "area_sqm": -1}),
        ("FurniturePlacement", {"placement_id": "p", "room_id": "a", "item_type": "bed", "footprint": [[0, 0], [1, 0], [0, 0]]}),
        ("FinishSpecification", {"surface": "floor", "material": "x", "area_sqm": 0}),
        ("RoomRequirement", {"room_type": "t", "count": 0, "min_area_sqm": 0, "min_width_m": -1}),
        ("LayoutPreferences", {"circulation_width_m": 0}),
        ("LayoutGenerationRequest", {"property_id": u, "room_requirements": [{**rr, "count": 0, "min_area_sqm": 0}], "candidate_count": 6}),
        ("LayoutGenerationRequest", {"property_id": u, "room_requirements": [rr], "candidate_count": 0, "preferences": {"circulation_width_m": -1}}),
        ("DesignRevisionRequest", {"project_id": u, "property_id": u, "base_design_version_id": u, "requested_changes": [], "requested_by": u, "source_channel": "web", "room_requirement_overrides": [{**rr, "count": 0}]}),
        ("DesignVersion", {**dv, "approval_status": "APPROVED"}),
        ("DesignVersion", {**dv, "version_number": 0, "estimated_construction_cost": -5}),
        ("DesignVersion", {**dv, "approval_status": "APPROVED", "validation_report": {**vr, "valid": True}, "room_polygons": [{"room_id": "a", "room_type": "t", "polygon": [[0, 0], [1, 0]], "area_sqm": 1}]}),
        ("BomItem", {"item_type": "x", "description": "d", "quantity": 0, "unit": "ea"}),
        ("BomItem", {"item_type": "x", "description": "d", "quantity": "1.5", "unit": "ea", "room_id": None, "unit_price_estimate": -1}),
        ("BillOfMaterials", {"project_id": u, "property_id": u, "design_version_id": u, "items": [], "estimated_total": -1, "approved_total": -2}),
        ("SupplierOffer", {"bom_item_id": u, "supplier": "s", "product_identifier": "p", "product_name": "n", "price": -1, "shipping_estimate_days": -1, "data_origin": "FIXTURE_SYNTHETIC"}),
    ]
    models = {m.__name__: m for m in (SiteGeometry, RoomPolygon, FurniturePlacement, FinishSpecification, RoomRequirement,
                                       LayoutPreferences, LayoutGenerationRequest, DesignRevisionRequest, DesignVersion,
                                       BomItem, BillOfMaterials, SupplierOffer)}
    out = []
    for name, doc in cases:
        r = outcome(lambda name=name, doc=doc: jd(models[name].model_validate_json(json.dumps(doc))))
        out.append({"model": name, "input": doc, **r})
    return out


# ---- part3_framing ----------------------------------------------------------------------------


def l_shaped_model() -> dict:
    """Copied from tests/test_sponsor_providers.py::_l_shaped_model (synthetic)."""
    pts = {"a": (0, 0), "b": (12, 0), "c": (12, 7), "d": (8, 7), "e": (8, 10), "f": (0, 10), "g": (3, 0), "h": (3, 10)}
    door = [{"type": "doorway", "width": 1.0, "relativeCenter": 1 / 3}]
    edges = [("a", "g", []), ("g", "b", door), ("b", "c", []), ("c", "d", []), ("d", "e", []), ("e", "h", []),
             ("h", "f", []), ("f", "a", []), ("g", "h", [])]
    return {
        "id": "TESTMODEL01", "name": "Suite 210",
        "floors": [{"id": "f1", "label": "Floor 1", "dimensions": {"areaFloor": 108.0},
                    "vertices": [{"id": k, "position": {"x": x, "y": y}} for k, (x, y) in pts.items()],
                    "edges": [{"id": f"e{i}", "type": "wall", "thickness": 0.15, "vertices": [{"id": a}, {"id": b}], "openings": ops}
                              for i, (a, b, ops) in enumerate(edges)]}],
        "rooms": [{"id": "r1", "floor": {"id": "f1"}, "label": "Office", "dimensions": {"areaFloor": 24.0}}],
    }


def rotate_pts(pts, deg, ox=0.0, oy=0.0, nd=4):
    a = math.radians(deg)
    return [(round(ox + x * math.cos(a) - y * math.sin(a), nd), round(oy + x * math.sin(a) + y * math.cos(a), nd)) for x, y in pts]


def frame_doc(frame):
    if frame is None:
        return None
    s = jd(frame.site)
    probes = [(0.0, 0.0), (1.0, 0.5), (2.25, 3.75), (s["floor_boundary"][2][0], s["floor_boundary"][2][1])]
    return {"site": s, "notes": frame.notes, "to_site": [{"p": p, "q": frame.to_site(p)} for p in probes]}


def framing_sites():
    out = []
    L = [(0, 0), (12, 0), (12, 7), (8, 7), (8, 10), (0, 10)]
    out.append(("hollywood_fixture", hollywood()))
    out.append(("l_bottom_door", site(L, entrances=[((5, 0), (6.2, 0))], walls=[((3, 0), (3, 10))])))
    out.append(("l_left_door", site(L, entrances=[((0, 4), (0, 5.2))])))
    out.append(("l_top_door", site(L, entrances=[((2, 10), (3.1, 10))], fixtures=[("sink", (1, 1)), ("sink", (10, 8.5)), ("drain", (6, 3))])))
    out.append(("l_right_door", site(L, entrances=[((12, 2), (12, 3.2))], obstacles=[[(11, 6), (13, 6), (13, 8), (11, 8)], rect(1, 1, 1.5, 1.5)])))
    out.append(("l_no_door", site(L)))
    out.append(("l_far_door", site(L, entrances=[((8.2, 7), (8.2, 8.5))])))
    out.append(("rect_offset_entrance_side", site(rect(2, 3, 14, 11), entrances=[((2, 5), (2, 6.5))])))
    out.append(("rect_ready", site(rect(2, 3, 14, 11), entrances=[((6, 3), (7.2, 3))])))
    out.append(("rotated_rect_30", site(rotate_pts(rect(0, 0, 12, 8), 30, 10, 0), entrances=[tuple(rotate_pts([(5, 0), (6.2, 0)], 30, 10, 0))])))
    out.append(("rotated_rect_minus_15", site(rotate_pts(rect(0, 0, 10, 9), -15, 0, 5))))
    out.append(("u_shape", site([(0, 0), (14, 0), (14, 10), (10, 10), (10, 4), (4, 4), (4, 10), (0, 10)], entrances=[((6, 0), (7.5, 0))])))
    out.append(("t_shape", site([(0, 0), (6, 0), (6, 6), (12, 6), (12, 10), (-6, 10), (-6, 6), (0, 6)], entrances=[((2, 0), (3, 0))], fixtures=[("sink", (3, 3)), ("sink", (-5, 9))])))
    out.append(("too_small", site([(0, 0), (4, 0), (4, 4), (2, 4), (2, 6), (0, 6)], entrances=[((1, 0), (2, 0))])))
    out.append(("narrow_door_near_corner", site([(0, 0), (9, 0), (9, 9), (0, 9.5)], entrances=[((0.1, 0), (0.4, 0))])))
    out.append(("pentagon", site([(0, 0), (10, 0), (12, 6), (5, 11), (-2, 6)], entrances=[((4, 0), (5, 0))], obstacles=[[(4, 4), (6, 4), (5, 6)]])))
    out.append(("door_on_short_side", site([(0, 0), (6, 0), (6, 15), (0, 15), (0, 14)], entrances=[((6, 7), (6, 8.5))])))
    out.append(("obstacle_crossing", site([(0, 0), (12, 0), (12, 9), (0, 9.2)], entrances=[((4, 0), (5.5, 0))], obstacles=[[(-1, 4), (13, 4), (13, 4.5), (-1, 4.5)], rect(5, 5, 7, 7)])))
    out.append(("concave_obstacle", site([(0, 0), (12, 0), (12, 9), (0, 9.2)], entrances=[((4, 0), (5.5, 0))], obstacles=[[(2, 2), (6, 2), (6, 6), (5, 6), (5, 3), (3, 3), (3, 6), (2, 6)], [(10, 8), (13, 8), (13, 10), (11.5, 9), (10, 10)]])))
    return out


def framing_vectors():
    model_doc = floor_geometry_from_model(l_shaped_model())
    cases = []
    for name, s in framing_sites():
        fb = [tuple(p) for p in s.floor_boundary]
        cases.append({
            "name": name, "site": jd(s), "dominant_angle": F.dominant_angle(fb),
            "largest_inner_rectangle": F.largest_inner_rectangle(Polygon(fb)),
            "solver_ready": F._is_solver_ready(s),
            "frame": frame_doc(F.frame_for_solver(s)),
        })
    plans = [
        ("l_shaped_model_doc", model_doc),
        ("rotated_doc", {"floor_boundary": rotate_pts(rect(0, 0, 9, 6), 22, 3, 4, 6), "entrances": [{"kind": "entrance", "start": rotate_pts([(1, 0)], 22, 3, 4, 6)[0], "end": rotate_pts([(2.2, 0)], 22, 3, 4, 6)[0]}],
                         "fixed_walls": [rotate_pts([(4, 0), (4, 6)], 22, 3, 4, 6)], "windows": [], "exits": [{"kind": "exit", "start": [1, 1], "end": [2, 2]}], "data_origin": "LIVE"}),
        ("axis_doc", {"floor_boundary": [[0, 0], [8, 0], [8, 5], [0, 5]], "interior_doors": [{"kind": "door", "start": [2, 0], "end": [3, 0], "extra": 1}]}),
        ("minus_40_doc", {"floor_boundary": rotate_pts(rect(-3, -2, 7, 6), -40, 0, 0, 6), "geometry_source": "MATTERPORT_MEASURED"}),
        ("short_walls_doc", {"floor_boundary": [[0, 0], [0.2, 0.1], [0.25, 0.3], [0.1, 0.25]]}),
        ("mixed_walls_doc", {"floor_boundary": [[0, 0], [10, 0], [10, 1], [12, 3], [12, 8], [0, 8]], "fixed_walls": [], "entrances": []}),
        ("rotated_45_doc", {"floor_boundary": rotate_pts(rect(0, 0, 6, 4), 45, 0, 0, 6)}),
    ]
    norm = []
    for name, d in plans:
        norm.append({"name": name, "input": d, "dominant_angle": F.dominant_angle([tuple(p) for p in d["floor_boundary"]]), **outcome(lambda d=d: F.normalize_plan(d))})
    norm.append({"name": "missing_boundary", "input": {"entrances": []}, **outcome(lambda: F.normalize_plan({"entrances": []}))})
    angles = []
    rng = random.Random(11)
    for _ in range(40):
        n = rng.randint(3, 7)
        angles.append([(round(rng.uniform(-10, 10), 2), round(rng.uniform(-10, 10), 2)) for _ in range(n)])
    for deg in (0, 10, 44.4, 44.6, 45, 46, 89.6, 90, 135, -30, 30.5, 29.5):
        angles.append(rotate_pts(rect(0, 0, 7, 3), deg, 1, 1, 6))
    return {
        "l_shaped_model_doc": model_doc,
        "cases": cases,
        "normalize_plan": norm,
        "dominant_angle": [{"coords": c, "angle": F.dominant_angle([tuple(p) for p in c])} for c in angles],
    }


# ---- part3_solver -----------------------------------------------------------------------------

REC: dict = {}
CALLS: list = []
LAST_STRIPS: dict = {}


class RecordingSolver(cp_model.CpSolver):
    def Solve(self, model, *args, **kwargs):  # noqa: N802 (OR-Tools name)
        st = super().Solve(model, *args, **kwargs)
        REC["status"] = self.StatusName(st)
        REC["objective"] = self.ObjectiveValue() if st in (cp_model.OPTIMAL, cp_model.FEASIBLE) else None
        return st


S.cp_model.CpSolver = RecordingSolver
_orig_solve = S._solve
_orig_strips = S._strips


def strip_doc(st):
    return {"name": st.name, "axis": st.axis, "origin": list(st.origin), "span": st.span, "depth": st.depth,
            "reserved": [list(r) for r in st.reserved]}


def _rec_strips(site_, topology, corridor, corridor_x0=None):
    out = _orig_strips(site_, topology, corridor, corridor_x0)
    LAST_STRIPS["offset"] = corridor_x0
    LAST_STRIPS["circulation"] = [[list(p) for p in ring] for ring in out[1]]
    return out


def _rec_solve(strips, rooms, site_, prefs, time_limit_s=0.5):
    REC.clear()
    sol = _orig_solve(strips, rooms, site_, prefs, time_limit_s)
    if "status" in REC:
        status, obj = REC["status"], REC["objective"]
    else:
        capacity = sum(st.depth * (st.span - sum(b - a for a, b in st.reserved)) for st in strips)
        status = "CAPACITY" if sum(r.min_area_sqm for r in rooms) > capacity else "NO_STRIP"
        obj = None
    CALLS.append({
        "offset": LAST_STRIPS.get("offset"), "strips": [strip_doc(s) for s in strips],
        "circulation": LAST_STRIPS.get("circulation"), "status": status, "objective": obj,
        "solution": None if sol is None else [[i, s, a, b] for i, s, a, b in sol],
        "solution_cm": None if sol is None else [[i, s, round(a * 100), round(b * 100)] for i, s, a, b in sol],
    })
    return sol


S._solve = _rec_solve
S._strips = _rec_strips


def solver_cases():
    hw = hollywood()
    cases = []

    def add(name, s, reqs, prefs=None, count=2):
        cases.append((name, s, reqs, prefs or LayoutPreferences(), count))

    add("hollywood_test_reqs", hw, REQS)
    add("hollywood_no_plumbing_pref", hw, REQS, LayoutPreferences(prefer_rooms_near_plumbing=False))
    add("hollywood_infeasible_12", hw, [req("treatment", 12, 12, 3)])
    add("hollywood_corridor_1_5", hw, REQS, LayoutPreferences(circulation_width_m=1.5))
    add("hollywood_corridor_1_15", hw, REQS, LayoutPreferences(circulation_width_m=1.15))
    add("hollywood_count_1", hw, REQS, count=1)
    add("hollywood_style", hw, REQS, LayoutPreferences(style="warm_minimalist"))
    add("hollywood_sink_near_column", hw.model_copy(update={"fixture_locations": [Fixture(kind="sink", location=(0.5, 1.5))]}), REQS)
    add("hollywood_six_treatment", hw, [req("treatment", 6, 10, 2.8, True), req("reception", 1, 10, 3), req("storage", 1, 4, 1.5)])
    add("hollywood_eight_rooms_tight", hw, [req("treatment", 7, 12, 3, True), req("reception", 1, 12, 3)])
    add("r8x6_center", site(rect(0, 0, 8, 6), entrances=[((3.4, 0), (4.6, 0))]), [req("treatment", 2, 9, 2.7, True), req("reception", 1, 8, 2.5)])
    add("r8x6_left_door", site(rect(0, 0, 8, 6), entrances=[((0.2, 0), (1.1, 0))], fixtures=[("sink", (7.5, 5.5))]), [req("treatment", 2, 9, 2.7, True), req("reception", 1, 8, 2.5)])
    add("r8x6_right_door", site(rect(0, 0, 8, 6), entrances=[((6.9, 0), (7.8, 0))]), [req("treatment", 2, 9, 2.7), req("reception", 1, 8, 2.5)])
    add("r10x8_no_door_sink", site(rect(0, 0, 10, 8), fixtures=[("sink", (9, 7))]), [req("treatment", 3, 10, 2.8, True)])
    add("r10x8_sink_outside", site(rect(0, 0, 10, 8), entrances=[((4, 0), (5.2, 0))], fixtures=[("sink", (12, 3))]), [req("treatment", 3, 10, 2.8, True)])
    add("r10x8_sink_far_outside", site(rect(0, 0, 10, 8), entrances=[((4, 0), (5.2, 0))], fixtures=[("sink", (40, 30))]), [req("treatment", 2, 10, 2.8, True)])
    add("r15x10_ten_rooms_plumbing", site(rect(0, 0, 15, 10), entrances=[((7, 0), (8.2, 0))], fixtures=[("sink", (14, 9))]),
        [req("treatment", 6, 9, 2.6, True), req("reception", 2, 8, 2.5), req("storage", 2, 3, 1.5)])
    add("r15x10_ten_rooms_no_pref", site(rect(0, 0, 15, 10), entrances=[((7, 0), (8.2, 0))], fixtures=[("sink", (14, 9))]),
        [req("treatment", 6, 9, 2.6, True), req("reception", 2, 8, 2.5), req("storage", 2, 3, 1.5)], LayoutPreferences(prefer_rooms_near_plumbing=False))
    add("r20x12_eleven_rooms", site(rect(0, 0, 20, 12), entrances=[((9, 0), (10.2, 0))], fixtures=[("sink", (2, 11))]),
        [req("treatment", 8, 10, 2.8, True), req("reception", 2, 12, 3), req("storage", 1, 4, 1.5)])
    add("r6x5_tight_one", site(rect(0, 0, 6, 5), entrances=[((2.5, 0), (3.5, 0))]), [req("treatment", 1, 10, 2.4, True)])
    add("r5x4_too_small", site(rect(0, 0, 5, 4), entrances=[((2, 0), (3, 0))]), [req("treatment", 1, 10, 2.8)])
    add("r12x9_no_strip", site(rect(0, 0, 12, 9), entrances=[((5, 0), (6, 0))]), [req("vault", 1, 10, 6.5)])
    add("r12x9_capacity", site(rect(0, 0, 12, 9), entrances=[((5, 0), (6, 0))]), [req("treatment", 9, 12, 2.5)])
    add("r10x6_exact_fill", site(rect(0, 0, 10, 6), entrances=[((4.4, 0), (5.6, 0))]), [req("treatment", 4, 6.3, 2.0, True)])
    add("r9x14_tall", site(rect(0, 0, 9, 14), entrances=[((3.9, 0), (5.1, 0))], fixtures=[("sink", (1, 7))]),
        [req("treatment", 4, 10, 2.8, True), req("reception", 1, 12, 3), req("storage", 1, 4, 1.5)])
    add("r14x7_wide", site(rect(0, 0, 14, 7), entrances=[((6.5, 0), (7.7, 0))], fixtures=[("sink", (13, 6))]),
        [req("treatment", 3, 9, 2.7, True), req("reception", 1, 9, 3), req("storage", 1, 3, 1.5)])
    add("r_offset_origin", site(rect(3, 2, 15, 12), entrances=[((8, 2), (9.2, 2))], fixtures=[("sink", (14, 11))]), REQS)
    add("r12x10_four_kinds", site(rect(0, 0, 12, 10), entrances=[((5.5, 0), (6.7, 0))], fixtures=[("sink", (11.5, 0.5))]),
        [req("treatment", 2, 10, 2.8, True), req("consult", 2, 8, 2.5, True), req("reception", 1, 12, 3), req("office", 1, 7, 2.4)])
    add("r12x10_five_kinds_ten", site(rect(0, 0, 12, 10), entrances=[((5.5, 0), (6.7, 0))], fixtures=[("sink", (6, 9.5))]),
        [req("a", 2, 6, 2.0, True), req("b", 2, 5.5, 2.1, True), req("c", 2, 5, 2.2), req("d", 2, 4.5, 2.3), req("e", 2, 4, 2.4, True)])
    add("r8x8_obstacle_corridor", site(rect(0, 0, 8, 8), entrances=[((3.4, 0), (4.6, 0))], obstacles=[rect(3.5, 3, 4.5, 4)]), [req("treatment", 2, 9, 2.7)])
    add("r8x8_door_wide", site(rect(0, 0, 8, 8), entrances=[((1, 0), (7, 0))]), [req("treatment", 2, 9, 2.7)])
    add("r10x9_two_doors", site(rect(0, 0, 10, 9), entrances=[((7, 0), (8, 0)), ((1, 0), (2, 0))]), [req("treatment", 3, 9, 2.7, True), req("reception", 1, 8, 2.5)])
    add("r11x9_no_requirements", site(rect(0, 0, 11, 9), entrances=[((5, 0), (6, 0))]), [])
    add("r11x9_zero_count_ignored", site(rect(0, 0, 11, 9), entrances=[((5, 0), (6, 0))]), [req("treatment", 2, 9, 2.7)], LayoutPreferences(circulation_width_m=0.9))
    # Measured (non-rectangular) floors: framed first.
    for name, s in framing_sites():
        if name in ("hollywood_fixture", "rect_ready"):
            continue
        add(f"framed_{name}", s, [req("treatment", 2, 9, 2.7, True), req("reception", 1, 6, 2.0)])
    rng = random.Random(3)
    for k in range(12):
        w, h = round(rng.uniform(6, 18), 1), round(rng.uniform(5, 14), 1)
        a = round(rng.uniform(0, w - 1.2), 2)
        door = ((a, 0), (round(a + rng.uniform(0.8, 1.6), 2), 0)) if rng.random() < 0.85 else None
        fx = [("sink", (round(rng.uniform(0, w), 2), round(rng.uniform(0, h), 2)))] if rng.random() < 0.7 else []
        program = []
        for t in range(rng.randint(1, 4)):
            program.append(req(f"room{t}", rng.randint(1, 3), round(rng.uniform(3, 14), 1), round(rng.uniform(1.5, 3.2), 1), rng.random() < 0.5))
        prefs = LayoutPreferences(prefer_rooms_near_plumbing=rng.random() < 0.7, circulation_width_m=rng.choice([1.0, 1.2, 1.2, 1.5]))
        add(f"random_{k}", site(rect(0, 0, w, h), entrances=[door] if door else [], fixtures=fx), program, prefs)
    return cases


def solver_vectors():
    out = []
    for name, s, reqs, prefs, count in solver_cases():
        CALLS.clear()
        cands = S.generate_candidates(s, reqs, prefs, count)
        frame = F.frame_for_solver(s)
        topologies = []
        # generate_candidates solves topologies in order; split CALLS by topology from strip names.
        cur, groups = None, []
        for c in CALLS:
            topo = "spine" if c["strips"][0]["name"] == "left" else "front"
            if topo != cur:
                groups.append((topo, []))
                cur = topo
            groups[-1][1].append(c)
        for topo, calls in groups:
            topologies.append({"topology": topo, "attempts": calls})
        out.append({
            "name": name, "site": jd(s), "requirements": [jd(r) for r in reqs], "preferences": jd(prefs), "count": count,
            "framed_site": None if frame is None else jd(frame.site),
            "topologies": topologies,
            "candidates": [{"name": c.name, "rooms": [jd(r) for r in c.rooms], "circulation": [[list(p) for p in ring] for ring in c.circulation],
                            "report": report_doc(c.report)} for c in cands],
        })
    return out


# ---- part3_validation --------------------------------------------------------------------------


def rp(rid, rt, pts, area=None):
    poly = Polygon(pts)
    return RoomPolygon.model_construct(room_id=rid, room_type=rt, polygon=[tuple(p) for p in pts], area_sqm=area if area is not None else round(poly.area, 2) or 1.0)


def validation_vectors(solver_out):
    cases = []

    def add(name, s, rooms, reqs, circ=None):
        rep = outcome(lambda: report_doc(V.validate_layout(s, rooms, reqs, circ)))
        cases.append({"name": name, "site": jd(s), "rooms": [r.model_dump(mode="json") for r in rooms],
                      "requirements": [jd(r) for r in reqs], "circulation": circ, **rep})

    # Valid layouts produced by the Python solver, validated on the frame they were solved in and
    # (for measured floors) on the measured plan they were mapped back to.
    taken = 0
    for case in solver_out:
        s = SiteGeometry.model_validate(case["site"])
        reqs = [RoomRequirement.model_validate(r) for r in case["requirements"]]
        for c in case["candidates"]:
            if not c["rooms"]:
                continue
            rooms = [RoomPolygon.model_validate(r) for r in c["rooms"]]
            add(f"solver_{case['name']}_{c['name']}", s, rooms, reqs, c["circulation"])
            taken += 1
        if taken >= 34:
            break
    hw = hollywood()
    T = lambda rid, pts: rp(rid, "treatment", pts)  # noqa: E731
    base_rooms = [T("treatment-1", rect(0, 0, 5.4, 2.8)), T("treatment-2", rect(0, 2.8, 5.4, 5.6)),
                  T("treatment-3", rect(6.6, 0, 12, 2.8)), rp("reception-1", "reception", rect(6.6, 2.8, 12, 6.8)),
                  rp("storage-1", "storage", rect(0, 5.6, 5.4, 7.1))]
    circ = [[[5.4, 0], [6.6, 0], [6.6, 11.6], [5.4, 11.6]]]
    add("hand_valid_spine", hw, base_rooms, REQS, circ)
    add("hand_door_blocked", hw, base_rooms[:2] + [T("treatment-3", rect(4.8, 0, 7.6, 3.6))] + base_rooms[3:], REQS, circ)
    add("hand_reception_on_door", hw, base_rooms[:3] + [rp("reception-1", "reception", rect(4.5, 0, 7.5, 4))] + base_rooms[4:], REQS)
    add("hand_room_touches_door_end", hw, [T("t", rect(6.2, 0, 9, 3.6))], [req("treatment", 1, 10, 2.8)])
    add("hand_room_covers_door_1cm", hw, [T("t", rect(6.19, 0, 9, 3.6))], [req("treatment", 1, 10, 2.8)])
    add("hand_room_covers_door_2cm", hw, [T("t", rect(6.18, 0, 9, 3.6))], [req("treatment", 1, 10, 2.8)])
    add("hand_circulation_intrusion", hw, base_rooms, REQS, [[[5.0, 0], [6.6, 0], [6.6, 11.6], [5.0, 11.6]]])
    add("hand_obstacle_in_corridor", hw, base_rooms, REQS, [[[0, 0], [1, 0], [1, 11.6], [0, 11.6]]])
    add("hand_overlap_tiny", hw, [T("a", rect(0, 3, 4, 6)), T("b", rect(3.9999999, 3, 8, 6))], [req("treatment", 2, 10, 2.8)])
    add("hand_overlap_small", hw, [T("a", rect(0, 3, 4, 6)), T("b", rect(3.999, 3, 8, 6))], [req("treatment", 2, 10, 2.8)])
    add("hand_outside_by_5e-7", hw, [T("a", rect(-5e-7, 3, 4, 6))], [req("treatment", 1, 10, 2.8)])
    add("hand_outside_by_2e-6", hw, [T("a", rect(-2e-6, 3, 4, 6))], [req("treatment", 1, 10, 2.8)])
    add("hand_below_area_and_width", hw, [T("a", rect(2, 3, 4.5, 6)), rp("s", "storage", rect(6, 6, 7.4, 8))], REQS)
    add("hand_area_tie_9_85", hw, [T("a", rect(2, 3, 4.5, 6.94))], [req("treatment", 1, 9.86, 2.5)])
    add("hand_area_equal", hw, [T("a", rect(2, 3, 4.5, 7))], [req("treatment", 1, 10, 2.5)])
    add("hand_width_equal", hw, [T("a", rect(2, 3, 4.8, 7))], [req("treatment", 1, 10, 2.8)])
    add("hand_obstacle_overlap_area", hw, [T("a", rect(0, 0, 3.2, 3.2))], [req("treatment", 1, 10, 2.8)])
    add("hand_obstacle_half", hw, [T("a", rect(0.5, 0, 3.7, 3.2))], [req("treatment", 1, 10, 2.8)])
    add("hand_missing_rooms", hw, base_rooms[:2], REQS)
    add("hand_unknown_type", hw, [rp("x-1", "vault", rect(8, 8, 9, 9))], REQS)
    add("hand_duplicate_ids", hw, [T("dup", rect(0, 3, 3, 7)), T("dup", rect(8, 3, 11, 7)), T("other", rect(4, 3, 7, 7))], [req("treatment", 3, 10, 2.8)])
    add("hand_rotated_room", hw, [T("r", rotate_pts(rect(0, 0, 4, 3), 25, 5, 4))], [req("treatment", 1, 10, 2.8)])
    add("hand_lshaped_rooms_overlap", hw, [T("a", [(1, 1), (5, 1), (5, 2), (2, 2), (2, 5), (1, 5)]), T("b", [(1.5, 1.5), (4, 1.5), (4, 4), (3, 4), (3, 3), (1.5, 3)])], [req("treatment", 2, 3, 0.9)])
    add("hand_bowtie_room", hw, [rp("bow", "treatment", [(1, 1), (3, 3), (3, 1), (1, 3)], 2.0)], [req("treatment", 1, 1, 0.5)])
    add("hand_requirement_override", hw, [T("a", rect(2, 3, 5, 7))], [req("treatment", 1, 10, 2.8), req("treatment", 2, 13, 3.5)])
    add("hand_exit_blocked", hw.model_copy(update={"exits": [Opening(kind="exit", start=(12, 4), end=(12, 5))]}), [T("a", rect(9, 3, 12, 7))], [req("treatment", 1, 10, 2.8)])
    L = site([(0, 0), (12, 0), (12, 7), (8, 7), (8, 10), (0, 10)], entrances=[((5, 0), (6.2, 0))])
    add("hand_l_notch", L, [T("a", rect(7, 6, 10, 9)), T("b", rect(0, 6, 3, 10))], [req("treatment", 2, 9, 2.7)])
    add("hand_empty", hw, [], [])
    add("hand_empty_circulation", hw, base_rooms, REQS, [])
    add("hand_float_messages", hw, [T("a", rect(2, 3, 3.1, 4))], [req("treatment", 1, 1e-05 + 3, 1.15)])
    return cases


# ---- part3_bom ----------------------------------------------------------------------------------


def bom_vectors(solver_out):
    designs = []
    for case in solver_out:
        for c in case["candidates"]:
            if not c["rooms"] or not c["report"]["valid"]:
                continue
            rooms = [RoomPolygon.model_validate(r) for r in c["rooms"]]
            style = case["preferences"]["style"]
            dv = DesignVersion(project_id=PROJECT, property_id=PID, version_number=1, geometry_id=case["site"]["site_geometry_id"],
                               room_polygons=rooms, finish_specifications=finish_specifications(rooms, style),
                               validation_report=GeometryValidationReport.model_validate({**c["report"], "checked_at": "2026-11-14T00:00:00Z"}),
                               approval_status="PROPOSED")
            bom = jd(generate_bom(dv))
            bom.pop("bom_id")
            for it in bom["items"]:
                it.pop("bom_item_id")
            designs.append({"name": f"{case['name']}_{c['name']}", "design": jd(dv), "bom": bom})
    finishes = []
    rooms = [RoomPolygon(room_id="t-1", room_type="treatment", polygon=rect(0, 0, 3, 4), area_sqm=12.0),
             RoomPolygon(room_id="x-1", room_type="vault", polygon=rect(0, 0, 1, 1), area_sqm=1.0)]
    for style in (None, "warm_minimalist", "industrial"):
        finishes.append({"style": style, "rooms": [jd(r) for r in rooms], "finishes": [jd(f) for f in finish_specifications(rooms, style)]})
    rng = random.Random(5)
    areas = [12.0, 10.24, 13.25, 0.05, 0.04, 9.95, 14.545, 2.675, 1e-3, 33.333, 15.12, 15.13, 22.5, 0.45]
    areas += [round(rng.uniform(0.5, 60), 2) for _ in range(60)]
    rounding = []
    for a in areas:
        q = Decimal(str(round(a * 1.1, 1)))
        rounding.append({"area": a, "quantity": str(q), "line_total": int(q * ILLUSTRATIVE_UNIT_PRICES["flooring_lvp_sqm"])})
    edge = []
    for a in (0.04, 0.01, 1e-9):
        dv = {"project_id": PROJECT, "property_id": PID, "version_number": 1, "geometry_id": PID,
              "room_polygons": [{"room_id": "s-1", "room_type": "storage", "polygon": [[0, 0], [1, 0], [1, 1]], "area_sqm": 0.5}],
              "finish_specifications": [{"room_id": "s-1", "surface": "floor", "material": "m", "area_sqm": a}],
              "validation_report": {"valid": True, "checked_at": "2026-11-14T00:00:00Z"}}
        r = outcome(lambda dv=dv: jd(generate_bom(DesignVersion.model_validate(dv))))
        if "ok" in r:
            r["ok"].pop("bom_id")
            for it in r["ok"]["items"]:
                it.pop("bom_item_id")
        edge.append({"design": dv, **r})
    return {"designs": designs, "finishes": finishes, "rounding": rounding, "edge_cases": edge,
            "illustrative_unit_prices": ILLUSTRATIVE_UNIT_PRICES}


# ---- part3_misc ---------------------------------------------------------------------------------

STEP_CASES = {
    "two_psets_and_bool": """ISO-10303-21;
DATA;
#1=IFCPUMP('PUMPGUID','Booster pump',$,$);
#2=IFCPROPERTYSINGLEVALUE('Running',$,IFCBOOLEAN(.T.),$);
#3=IFCPROPERTYSINGLEVALUE('Spare',$,IFCBOOLEAN(.F.),$);
#4=IFCPROPERTYSINGLEVALUE('Label',$,IFCLABEL('P-1'),$);
#5=IFCPROPERTYSET('PSETGUID',$,'Pset_PumpCommon',$,(#2,#3,#4));
#6=IFCRELDEFINESBYPROPERTIES('RELGUID',$,$,$,(#1,#7),#5);
#7=IFCVALVE('VALVEGUID',$,$,$);
#8=IFCPROPERTYSINGLEVALUE('Flow',$,IFCVOLUMETRICFLOWRATEMEASURE(1.5E-2),$);
#9=IFCPROPERTYSET('PSET2',$,'Pset_Other',$,(#8,#99));
#10=IFCRELDEFINESBYPROPERTIES('REL2',$,$,$,(#1),#9);
#11=IFCWALL('WALLGUID','Not MEP',$,$);
#12=IFCAIRTERMINAL($,$,$,$);
#13=IFCFLOWTERMINAL('FT','It''s a terminal',$,$);
ENDSEC;
END-ISO-10303-21;
""",
    "no_name_pset_and_spaces": """DATA;
#20 = IFCDUCTSEGMENT('D1','Duct',$);
#21= IFCPROPERTYSINGLEVALUE('Width',$,IFCPOSITIVELENGTHMEASURE(  0.4 ),$);
#22=IFCPROPERTYSET('G',$,$,$,(#21));
#23=IFCRELDEFINESBYPROPERTIES('R',$,$,$,(#20),#22);
#24=IFCPROPERTYSINGLEVALUE('Count',$,IFCINTEGER(3),$);
#25=IFCPROPERTYSINGLEVALUE('Note',$,$,$);
#26=IFCPROPERTYSET('G2',$,'Pset_A',$,(#24,#25));
#27=IFCRELDEFINESBYPROPERTIES('R2',$,$,$,(#20),#26);
#28=IFCPIPESEGMENT('P1','Pipe',$);
#29=IFCSANITARYTERMINAL('S1','Sink',$);trailing
""",
}


def misc_vectors():
    cat = FixtureCatalogProvider()
    bom_item = "dff02230-0000-4000-8000-0000000000c1"
    offers = {}
    for t in ["treatment_bed", "hand_sink", "flooring_lvp_sqm", "unknown_item"]:
        docs = [jd(o) for o in cat.offers_for(UUID(bom_item), t)]
        for d in docs:
            d.pop("offer_id")
            d.pop("retrieved_at")
        offers[t] = docs
    from datetime import date
    timeline = {d: opening_timeline(date.fromisoformat(d)) for d in ("2026-11-14", "2027-02-20", "2028-12-31")}
    fixture = _read_step_text(REPO / "fixtures" / "ifc" / "mini_mep.ifc")
    tmp = OUT / "_step_tmp"
    tmp.mkdir(exist_ok=True)
    step = {}
    for name, text in STEP_CASES.items():
        p = tmp / f"{name}.ifc"
        p.write_text(text)
        step[name] = {"text": text, "elements": [e.__dict__ for e in _read_step_text(p)]}
        p.unlink()
    tmp.rmdir()
    return {
        "catalog": {"mode": cat.mode, "status": jd(cat.status()), "bom_item_id": bom_item, "offers": offers},
        "timeline": timeline,
        "ifc_fixture": [e.__dict__ for e in fixture],
        "ifc_step_cases": step,
        "py": py_units(),
    }


def main() -> None:
    dump("part3_geometry.json", geometry_vectors())
    dump("part3_contracts.json", contract_vectors())
    dump("part3_framing.json", framing_vectors())
    solver_out = solver_vectors()
    dump("part3_solver.json", solver_out)
    dump("part3_validation.json", validation_vectors(solver_out))
    dump("part3_bom.json", bom_vectors(solver_out))
    dump("part3_misc.json", misc_vectors())


if __name__ == "__main__":
    main()
