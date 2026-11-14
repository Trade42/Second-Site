"""Vectors for the pure parts of the design service port (`services/design.rs`).

- `room_requirements_from_profile(rooms, plumbing_rooms)` from `domains/design/service.py`,
  dumped with `model_dump(mode="json")`;
- the advisory lock keys the Rust service takes (version numbering per property, first bill of
  materials per design version), computed with the formula a Python writer would use to take the
  same locks: `int.from_bytes(sha256(f"second-site:{ns}:{id}").digest()[:8], "big", signed=True)`;
- the `SiteGeometry(...)` that `DesignService.site_geometry` builds from a provider document
  (`normalize_plan` first when the document is `LIVE`), for the fixture floor, every non-null
  `floor_geometry_from_model` result in matterport_geometry.json, and malformed documents
  (recorded as the exception class Python raises). `site_geometry_id` is a fresh uuid4 and is
  left out.

Pure functions, no database, no network. Deterministic: it regenerates byte-identically.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_design_service.py)
Output: design_service.json
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from app.contracts import DataOrigin, GeometrySource, SiteGeometry  # noqa: E402
from app.domains.design.geometry.framing import normalize_plan  # noqa: E402
from app.domains.design.service import room_requirements_from_profile  # noqa: E402

ROOMS = [
    ({"reception": 1, "storage": 1, "treatment": 3}, None),
    ({"treatment": 3, "reception": 1, "storage": 1}, None),
    ({"treatment": 2, "office": 1, "reception": 0, "storage": -1}, None),
    ({"treatment": 2, "laundry": 1, "staff_room": 1}, ["laundry", "staff_room"]),
    ({"treatment": 2, "reception": 1}, []),
    ({}, None),
    ({"unknown_room": 4}, ["unknown_room"]),
]

LOCKS = [
    ("design_versions", "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e"),
    ("design_versions", "00000000-0000-4000-8000-000000000000"),
    ("bills_of_materials", "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e"),
    ("activity_events", "6a8f3a52-1d3b-4f6e-9c4e-0b7c1f2a3d4e"),
]


PROPERTY_ID = "3f1c2b9a-6d4e-4f1a-8b2c-9d0e1f2a3b4c"
ASSET_ID = "fixture-matterport-hollywood-302"


def site_geometry(doc: dict) -> dict:
    """The body of `DesignService.site_geometry` after `doc = get_floor_geometry(...)`."""
    live = doc.get("data_origin") == "LIVE"
    if live:
        doc = normalize_plan(doc)
    geo = SiteGeometry(
        property_id=PROPERTY_ID,
        source_id=ASSET_ID,
        geometry_source=GeometrySource(doc.get("geometry_source", "FIXTURE")),
        data_origin=DataOrigin.LIVE if live else DataOrigin.FIXTURE_SYNTHETIC,
        coordinate_reference=doc.get("coordinate_reference", "local_floor_origin"),
        floor_boundary=doc["floor_boundary"],
        fixed_obstacles=doc.get("fixed_obstacles", []),
        fixed_walls=[tuple(tuple(p) for p in w) for w in doc.get("fixed_walls", [])],
        entrances=doc.get("entrances", []),
        exits=doc.get("exits", []),
        windows=doc.get("windows", []),
        fixture_locations=doc.get("fixture_locations", []),
        scale_verified=bool(doc.get("scale_verified", False)),
        measurement_uncertainty_m=doc.get("measurement_uncertainty_m"),
    )
    out = geo.model_dump(mode="json")
    out.pop("site_geometry_id")
    return out


def geometry_cases():
    fixture = json.loads((REPO / "fixtures" / "geometry" / "hollywood_302_synthetic.json").read_text())
    yield "hollywood_fixture", fixture
    for c in json.loads((HERE / "matterport_geometry.json").read_text()):
        if c["result"] is not None:
            yield f"live_{c['name']}", c["result"]
    yield "rotated_three_rooms_python", json.loads((HERE / "matterport_geometry_rotated.json").read_text())["python_result"]
    yield "unknown_geometry_source", {**fixture, "geometry_source": "SATELLITE"}
    yield "no_floor_boundary", {k: v for k, v in fixture.items() if k != "floor_boundary"}
    yield "photo_estimate_scale_verified", {**fixture, "geometry_source": "PHOTO_ESTIMATE", "scale_verified": True}
    yield "owner_measured_truthy_scale", {**fixture, "geometry_source": "OWNER_MEASURED", "scale_verified": 1,
                                          "coordinate_reference": "owner_sketch"}
    yield "negative_uncertainty", {**fixture, "measurement_uncertainty_m": -0.1}
    yield "degenerate_boundary", {**fixture, "floor_boundary": [[0, 0], [1, 0]]}


def lock_key(ns: str, ident: str) -> int:
    return int.from_bytes(hashlib.sha256(f"second-site:{ns}:{ident}".encode()).digest()[:8], "big", signed=True)


def main() -> None:
    out = {
        "room_requirements_from_profile": [
            {
                # Pairs, in dict order (the order is part of the result).
                "rooms": list(rooms.items()),
                "plumbing_rooms": plumbing,
                "result": [
                    r.model_dump(mode="json")
                    for r in room_requirements_from_profile(rooms, set(plumbing) if plumbing is not None else None)
                ],
            }
            for rooms, plumbing in ROOMS
        ],
        "lock_keys": [{"namespace": ns, "id": i, "key": lock_key(ns, i)} for ns, i in LOCKS],
        "site_geometry": {"property_id": PROPERTY_ID, "source_id": ASSET_ID, "cases": []},
    }
    for name, doc in geometry_cases():
        try:
            result, error = site_geometry(doc), None
        except Exception as exc:  # noqa: BLE001 - recorded, as TaskService.run records it
            result, error = None, type(exc).__name__
        out["site_geometry"]["cases"].append({"name": name, "doc": doc, "result": result, "error": error})
    (HERE / "design_service.json").write_text(json.dumps(out, indent=1) + "\n")
    print("wrote design_service.json:", len(out["room_requirements_from_profile"]), "profiles,",
          len(out["lock_keys"]), "lock keys,", len(out["site_geometry"]["cases"]), "geometry documents")


if __name__ == "__main__":
    main()
