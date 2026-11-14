from __future__ import annotations

import json
import uuid

from app.contracts import (
    DataOrigin,
    GeometrySource,
    LayoutPreferences,
    RoomPolygon,
    RoomRequirement,
    SiteGeometry,
)
from app.domains.design.geometry.solver import generate_candidates
from app.domains.spatial.validation import validate_layout

from .conftest import FIXTURES

REQS = [
    RoomRequirement(room_type="treatment", count=3, min_area_sqm=10, min_width_m=2.8, needs_plumbing=True),
    RoomRequirement(room_type="reception", count=1, min_area_sqm=12, min_width_m=3),
    RoomRequirement(room_type="storage", count=1, min_area_sqm=4, min_width_m=1.5),
]


def site() -> SiteGeometry:
    d = json.loads((FIXTURES / "geometry" / "hollywood_302_synthetic.json").read_text())
    return SiteGeometry(
        property_id=uuid.uuid4(),
        source_id=d["source_id"],
        geometry_source=GeometrySource.FIXTURE,
        data_origin=DataOrigin.FIXTURE_SYNTHETIC,
        floor_boundary=d["floor_boundary"],
        fixed_obstacles=d["fixed_obstacles"],
        entrances=d["entrances"],
        exits=d["exits"],
        fixture_locations=d["fixture_locations"],
    )


def test_two_valid_non_overlapping_candidates():
    cands = generate_candidates(site(), REQS, LayoutPreferences(), 2)
    assert len(cands) == 2
    for c in cands:
        assert c.report.valid, c.report.issues
        assert len(c.rooms) == 5
    assert {tuple(map(tuple, r.polygon)) for r in cands[0].rooms} != {
        tuple(map(tuple, r.polygon)) for r in cands[1].rooms
    }


def test_infeasible_program_returns_no_rooms():
    big = [RoomRequirement(room_type="treatment", count=12, min_area_sqm=12, min_width_m=3)]
    for c in generate_candidates(site(), big, LayoutPreferences(), 2):
        assert not c.report.valid and c.rooms == [] and c.report.issues[0].code == "INFEASIBLE"


def test_validation_rejects_overlap_and_outside():
    s = site()
    rooms = [
        RoomPolygon(
            room_id="a", room_type="treatment", polygon=[(0, 0), (4, 0), (4, 4), (0, 4)], area_sqm=16
        ),
        RoomPolygon(
            room_id="b", room_type="treatment", polygon=[(3, 3), (7, 3), (7, 7), (3, 7)], area_sqm=16
        ),
        RoomPolygon(
            room_id="c", room_type="storage", polygon=[(11, 10), (13, 10), (13, 12), (11, 12)], area_sqm=4
        ),
    ]
    codes = {i.code for i in validate_layout(s, rooms, REQS).issues}
    assert {"ROOM_OVERLAP", "OUTSIDE_BOUNDARY", "MISSING_ROOMS"} <= codes


def test_obstacles_reduce_usable_area():
    s = site()
    rooms = [
        RoomPolygon(
            room_id="t",
            room_type="treatment",
            polygon=[(0, 0), (3.2, 0), (3.2, 3.2), (0, 3.2)],
            area_sqm=10.24,
        )
    ]
    issues = validate_layout(s, rooms, [REQS[0].model_copy(update={"count": 1})]).issues
    assert any(i.code == "BELOW_MIN_AREA" for i in issues)  # 10.24 - 0.36 column < 10
