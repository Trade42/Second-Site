"""Golden vectors for interior (Matterport) and exterior (Vexcel) enrichment.

Runs Python's enrich_interior / enrich_exterior with scripted fake providers (shaped like the
fakes in apps/api/tests/test_sponsor_providers.py) over a profile built from the synthetic CoStar
kit (tests-golden/costar_kit_synthetic, synthetic data, not CoStar data). Pure functions: no
database, no network. The input profile is recorded with a fixed property id.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_enrichment.py)
Output: enrichment.json. Fresh values (retrieved_at, observation_id) differ between runs; the
Rust test ignores exactly those two fields.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path
from uuid import UUID

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

from app.contracts import PropertySearchRequest  # noqa: E402
from app.domains.property.enrichment import enrich_exterior, enrich_interior  # noqa: E402
from app.domains.property.service import profile_from_listing  # noqa: E402
from app.integrations.base import ProviderError, ProviderErrorKind  # noqa: E402
from app.integrations.costar import KitCoStarProvider  # noqa: E402
from app.integrations.vexcel import summarize_property  # noqa: E402

PROJECT = UUID("a82a8700-0000-4000-8000-0000000000a1")
PROPERTY = UUID("a82a8700-0000-4000-8000-0000000000c1")
MEDIA = "http://api.test"

VEXCEL_PROPS = {
    "structures": [{
        "primary-structure": True, "footprint.area": 812.4, "roof-characteristics.condition": 4.0,
        "roof-characteristics.material.class": "membrane", "roof-characteristics.material.score": 0.8,
        "roof-objects.ac-unit.count": 6, "roof-defects.structural-damage.detected": False,
        "roof-defects.discoloration.types.water-pooling.detected": True, "structure-distance": 0.0,
    }],
    "metadata.ortho-date-estimate": "2025-12-01T10:00:00",
    "metadata.collection": "us-ca-losangeles-2025",
    "request-information.inside-parcel": True,
}


class FakeMP:
    """`summaries`: model id -> summary, or `error`: a ProviderErrorKind value."""

    def __init__(self, spec):
        self.mode = spec.get("mode", "live")
        self.spec = spec
        self.calls = []

    def model_summaries(self, ids):
        self.calls.append(list(ids))
        if "error" in self.spec:
            raise ProviderError("matterport", ProviderErrorKind(self.spec["error"]), "scripted failure")
        return {k: v for k, v in self.spec["summaries"].items() if k in ids}


class FakeVX:
    def __init__(self, spec):
        self.mode = spec.get("mode", "live")
        self.spec = spec

    def property_elements(self, lat, lon):
        if "elements_error" in self.spec:
            raise ProviderError("vexcel", ProviderErrorKind(self.spec["elements_error"]), "scripted failure")
        p = self.spec.get("properties")
        return summarize_property(p) if p is not None else None

    def capture_dates(self, lat, lon):
        if "captures_error" in self.spec:
            raise ProviderError("vexcel", ProviderErrorKind(self.spec["captures_error"]), "scripted failure")
        return self.spec.get("captures", [])


def base_profile():
    kit = REPO / "apps" / "backend" / "tests-golden" / "costar_kit_synthetic"
    res = KitCoStarProvider(kit).search(PropertySearchRequest(city="Los Angeles", state="CA", limit=50))
    raw = next(r for r in res if r.provider_listing_id == "1:16")
    prof = profile_from_listing(PROJECT, raw)
    ident = prof.property_identity.model_copy(update={"property_id": PROPERTY})
    return prof.model_copy(update={"property_id": PROPERTY, "property_identity": ident})


def interior_cases(prof):
    mp_assets = [a for a in prof.interior_assets if a.provider == "matterport"]
    assert [a.asset_id for a in mp_assets][:2] == ["MODEL1", "MODEL2"], [a.asset_id for a in mp_assets]
    many = prof.model_copy(update={"interior_assets": [
        a.model_copy(update={"asset_id": f"MANY{i:03d}"}) for i in range(45) for a in mp_assets[:1]
    ] + [a for a in prof.interior_assets if a.provider != "matterport"]})
    no_suite = prof.model_copy(update={"property_identity": prof.property_identity.model_copy(
        update={"unit_or_suite": None})})
    yield "suite_scan_marked", prof, {"summaries": {
        "MODEL1": {"name": "Suite 106", "area_sqm": 105.0, "floor_count": 1},
        "MODEL2": {"name": "Lobby", "area_sqm": 40.0, "floor_count": 1}}}
    yield "no_suite_scan", prof, {"summaries": {
        "MODEL1": {"name": "Lobby", "area_sqm": 40.0, "floor_count": 1},
        "MODEL2": {"name": "2BR,3BA - 2,280 SF", "area_sqm": 0.0, "floor_count": 0}}}
    yield "suite_scan_without_floors", prof, {"summaries": {
        "MODEL2": {"name": "STE 106", "area_sqm": None, "floor_count": 0}}}
    yield "provider_error", prof, {"error": "AUTH"}
    yield "fixture_mode_is_a_no_op", prof, {"mode": "fixture", "summaries": {}}
    yield "listing_without_suite", no_suite, {"summaries": {
        "MODEL1": {"name": "Suite 106", "area_sqm": 105.0, "floor_count": 1}}}
    yield "forty_model_cap_and_six_building_scans", many, {"summaries": {
        f"MANY{i:03d}": {"name": None if i % 7 == 0 else f"Space {i}", "area_sqm": 50.0 + i * 3.5,
                         "floor_count": i % 2} for i in range(45)}}


def exterior_cases(prof):
    no_coords = prof.model_copy(update={"property_identity": prof.property_identity.model_copy(
        update={"latitude": None})})
    yield "elements", prof, {"properties": VEXCEL_PROPS}
    yield "fixture_mode_is_a_no_op", prof, {"mode": "fixture"}
    yield "no_coordinates", no_coords, {"properties": VEXCEL_PROPS}
    yield "elements_error", prof, {"elements_error": "RATE_LIMITED"}
    yield "no_parcel_capture_date_fallback", prof, {"properties": None, "captures": [
        {"layer": "urban", "last-capture-date": "2024-03-05T00:00:00Z"},
        {"layer": "urban", "last-capture-date": "2025-01-20"},
        {"layer": "rural", "last-capture-date": "2026-02-01"},
        {"layer": "urban"}]}
    yield "no_parcel_capture_error", prof, {"properties": None, "captures_error": "UNAVAILABLE"}
    yield "neighbour_structure_and_defects", prof, {"properties": {
        "structures": [{"footprint.area": 300.04, "roof-defects.structural-damage.detected": True,
                        "roof-defects.discoloration.types.water-pooling.detected": True,
                        "structure-distance": 12.5, "roof-characteristics.material.class": "tile"},
                       {"primary-structure": False}],
        "metadata.collection": None, "metadata.ortho-date-estimate": None}, "captures": []}
    yield "date_only_and_no_elements", prof, {"properties": {"structures": [],
                                                              "metadata.ortho-date-estimate": "20250704"}}


def dump(p):
    return p.model_dump(mode="json")


def main() -> None:
    prof = base_profile()
    out = {"_meta": {"warning": "Synthetic kit and scripted provider answers: test data, not CoStar, "
                                "Matterport or Vexcel data."},
           "media_base": MEDIA, "interior": [], "exterior": []}
    for name, p, spec in interior_cases(prof):
        fake = FakeMP(spec)
        res = enrich_interior(p, fake)
        out["interior"].append({"name": name, "input": dump(p), "provider": spec, "requested": fake.calls,
                                "output": dump(res)})
    for name, p, spec in exterior_cases(prof):
        res = enrich_exterior(p, FakeVX(spec), MEDIA)
        # Enrich twice: earlier Vexcel observations are replaced, not duplicated.
        again = enrich_exterior(res, FakeVX(spec), MEDIA)
        out["exterior"].append({"name": name, "input": dump(p), "provider": spec, "output": dump(res),
                                "output_twice": dump(again)})
    (HERE / "enrichment.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote enrichment.json: {len(out['interior'])} interior, {len(out['exterior'])} exterior cases")


if __name__ == "__main__":
    main()
