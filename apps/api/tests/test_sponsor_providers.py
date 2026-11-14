"""CoStar data kit, Matterport Model API and Vexcel API 2.0 adapters.

Offline tests use a tiny synthetic kit and synthetic API payloads shaped like the documented responses.
Live smoke tests (RUN_LIVE_TESTS=1 plus credentials in .env) call the real services read-only.
"""

from __future__ import annotations

import base64
import csv
import json
import os
from pathlib import Path
from uuid import uuid4

import httpx
import pytest
from shapely.geometry import Polygon

from app.contracts import (
    DataOrigin,
    GeometrySource,
    LayoutPreferences,
    PropertySearchRequest,
    RoomRequirement,
    SiteGeometry,
)
from app.domains.design.geometry.framing import frame_for_solver, largest_inner_rectangle, normalize_plan
from app.domains.design.geometry.solver import generate_candidates
from app.domains.property.enrichment import enrich_exterior, enrich_interior
from app.domains.property.service import profile_from_listing
from app.integrations.costar import KitCoStarProvider
from app.integrations.matterport import LiveMatterportProvider, floor_geometry_from_model, suite_matches
from app.integrations.vexcel import LiveVexcelProvider, summarize_property

# ---- synthetic CoStar kit ---------------------------------------------------------------------


def _space(sid, suite, use=5, sqft=1200, rate=4.0, basis=1, service=13, **kw):
    return {
        "id": sid,
        "propertyId": kw.pop("pid", 100),
        "suite": suite,
        "spaceUse": use,
        "serviceType": service,
        "squareFeetMax": sqft,
        "rateLow": rate,
        "rateHigh": rate,
        "rateBasis": basis,
        "isLeasePending": False,
        "availableOnDate": "2026-12-01T00:00:00-05:00",
        "leaseTermLow": 5,
        "leaseTermType": 1,
        **kw,
    }


def _listing(lid, spaces, state="CA", pid=100, **kw):
    return {
        "id": lid,
        "listingType": 14,
        "listingStatus": 1,
        "propertyId": pid,
        "buildingName": "Test Plaza",
        "brokerageCompanyName": "Example Brokerage",
        "certifiedFreshDate": "2026-10-01T00:00:00-04:00",
        "monetaryUnit": 1,
        "address": {
            "streetNumber": "100",
            "streetDirection": "W",
            "streetName": "Main",
            "streetType": "St",
            "city": "Los Angeles" if state == "CA" else "Houston",
            "state": state,
            "postcode": "900121234",
            "latitude": 34.05,
            "longitude": -118.25,
        },
        "attachments": [
            {"attachmentUri": "https://cdn.example/i/abc/112/image.jpg", "videoExternalReferenceType": 0},
            {"videoKey": "MODEL1", "videoExternalReferenceType": 36},
        ],
        "forLeaseSummary": {"spaces": spaces},
        **kw,
    }


@pytest.fixture
def kit(tmp_path: Path) -> Path:
    (tmp_path / "data").mkdir()
    (tmp_path / "schemas").mkdir()
    (tmp_path / "schemas" / "enums.json").write_text(
        json.dumps(
            {
                "ServiceType": {"7": "FullService", "13": "TripleNet", "9": "ModifiedGross"},
                "SpaceUse": {"1": "Office", "4": "Medical", "5": "Retail", "7": "Industrial"},
            }
        )
    )
    listings = [
        _listing(
            1,
            [
                _space(11, "101", rate=4.0, basis=1, sqft=1200),  # $4/SF/mo NNN -> $4,800/mo
                _space(12, "102", rate=48.0, basis=2, sqft=1000, service=7),  # $48/SF/yr FS -> $4,000/mo
                _space(13, "103", rate=0, sqft=1100),  # call for rate -> skipped
                _space(14, "104", sqft=1300, dateOffMarket="2026-01-01T00:00:00"),  # off market
                _space(15, "105", use=7),  # industrial -> excluded
                _space(16, "106", rate=3.0, sqft=1150, notes=[{"value": "Former salon, plumbing in place"}]),
            ],
        ),
        _listing(2, [_space(21, "A", rate=2.0, sqft=1500, pid=200)], pid=200),
        _listing(3, [_space(31, "9", rate=1.0, sqft=1500, pid=300)], state="TX", pid=300),
        {**_listing(4, [_space(41, "X", pid=400)], pid=400), "listingStatus": 3},  # not active
    ]
    (tmp_path / "data" / "commercial-listing-summary.jsonl").write_text(
        "\n".join(json.dumps(r) for r in listings) + "\n"
    )
    with (tmp_path / "commercial-properties.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["PropertyId", "MatterportModelId", "Address", "City", "State"])
        w.writerow([100, "MODEL1", "", "", ""])
        w.writerow([100, "MODEL2", "", "", ""])
        w.writerow([200, "", "", "", ""])
    return tmp_path


def test_kit_search_maps_real_fields(kit):
    p = KitCoStarProvider(kit)
    res = p.search(PropertySearchRequest(city="Los Angeles", state="CA", limit=10))
    ids = [r.provider_listing_id for r in res]
    assert "1:13" not in ids and "1:14" not in ids and "1:15" not in ids  # no rate / off market / industrial
    assert p.skipped_no_rate == 1
    assert all(r.provider == "costar" and r.data_origin == DataOrigin.FIXTURE_REAL for r in res)
    by = {r.provider_listing_id: r for r in res}
    assert len([i for i in ids if i.startswith("1:")]) == 2  # at most two spaces per building
    cheapest = res[0]
    assert cheapest.provider_listing_id == "2:21" and cheapest.listing["asking_rent_monthly"] == 300000
    s16 = by["1:16"].listing
    assert s16["asking_rent_monthly"] == 345000 and s16["nnn_charges_monthly"] is None
    assert s16["lease_service_type"] == "TripleNet" and s16["existing_buildout"] == "second_generation_salon"
    assert s16["lease_term_months"] == 60 and str(s16["available_from"]) == "2026-12-01"
    assert s16["photo_urls"] == ["https://cdn.example/i/abc/113/image.jpg"]
    assert (
        by["1:16"].address["street_address"] == "100 W Main St"
        and by["1:16"].address["postal_code"] == "90012"
    )
    mp = [a for a in by["1:16"].interior_assets if a["provider"] == "matterport"]
    assert [a["asset_id"] for a in mp] == ["MODEL1", "MODEL2"] and not any(
        a["has_measured_geometry"] for a in mp
    )
    houston = p.search(PropertySearchRequest(city="Houston", state="TX"))
    assert [r.provider_listing_id for r in houston] == ["3:31"]


def test_kit_filters_and_profile(kit):
    p = KitCoStarProvider(kit)
    res = p.search(
        PropertySearchRequest(city="Los Angeles", state="CA", max_monthly_rent=400000, min_area_sqm=90)
    )
    assert {r.provider_listing_id for r in res} == {"1:12", "1:16", "2:21"}  # 1:11 is over budget
    prof = profile_from_listing(uuid4(), next(r for r in res if r.provider_listing_id == "1:12"))
    assert prof.commercial_listing.nnn_charges_monthly == 0  # full service: expenses included in rent
    assert prof.source_manifest[0].status == "SNAPSHOT"
    assert prof.property_identity.provider_property_ids["costar_property"] == "100"
    assert prof.interior_assets[0].url == "https://my.matterport.com/show/?m=MODEL1"


def test_kit_missing_is_explicit(tmp_path):
    p = KitCoStarProvider(tmp_path / "nope")
    assert p.status().status.value == "BLOCKED"
    with pytest.raises(Exception, match="NOT_CONFIGURED"):
        p.search(PropertySearchRequest())


# ---- Matterport ---------------------------------------------------------------------------------


def _l_shaped_model() -> dict:
    """12 x 10 m L-shaped suite (a 4 x 3 m notch removed), one interior wall, entrance on the south wall."""
    pts = {
        "a": (0, 0),
        "b": (12, 0),
        "c": (12, 7),
        "d": (8, 7),
        "e": (8, 10),
        "f": (0, 10),
        "g": (3, 0),
        "h": (3, 10),
    }
    door = [{"type": "doorway", "width": 1.0, "relativeCenter": 1 / 3}]
    edges = [
        ("a", "g", []),
        ("g", "b", door),
        ("b", "c", []),
        ("c", "d", []),
        ("d", "e", []),
        ("e", "h", []),
        ("h", "f", []),
        ("f", "a", []),
        ("g", "h", []),
    ]
    return {
        "id": "TESTMODEL01",
        "name": "Suite 210",
        "floors": [
            {
                "id": "f1",
                "label": "Floor 1",
                "dimensions": {"areaFloor": 108.0},
                "vertices": [{"id": k, "position": {"x": x, "y": y}} for k, (x, y) in pts.items()],
                "edges": [
                    {
                        "id": f"e{i}",
                        "type": "wall",
                        "thickness": 0.15,
                        "vertices": [{"id": a}, {"id": b}],
                        "openings": ops,
                    }
                    for i, (a, b, ops) in enumerate(edges)
                ],
            }
        ],
        "rooms": [{"id": "r1", "floor": {"id": "f1"}, "label": "Office", "dimensions": {"areaFloor": 24.0}}],
    }


def test_floor_geometry_from_model_and_framing():
    doc = floor_geometry_from_model(_l_shaped_model())
    assert doc["data_origin"] == "LIVE" and doc["geometry_source"] == "MATTERPORT_MEASURED"
    assert Polygon(doc["floor_boundary"]).area == pytest.approx(108.0, abs=0.1)
    assert len(doc["entrances"]) == 1 and len(doc["fixed_walls"]) == 1  # the interior partition
    doc = normalize_plan(doc)
    site = SiteGeometry(
        property_id=uuid4(),
        source_id="TESTMODEL01",
        geometry_source=GeometrySource.MATTERPORT_MEASURED,
        data_origin=DataOrigin.LIVE,
        floor_boundary=doc["floor_boundary"],
        fixed_walls=[tuple(tuple(p) for p in w) for w in doc["fixed_walls"]],
        entrances=doc["entrances"],
        scale_verified=True,
    )
    rect = largest_inner_rectangle(Polygon(site.floor_boundary))
    assert (rect[2] - rect[0]) * (rect[3] - rect[1]) == pytest.approx(84.0, abs=1.5)  # 12 x 7 band
    frame = frame_for_solver(site)
    assert frame is not None and any("partition" in n for n in frame.notes)
    reqs = [
        RoomRequirement(room_type="treatment", count=2, min_area_sqm=9, min_width_m=2.7),
        RoomRequirement(room_type="reception", count=1, min_area_sqm=6, min_width_m=2.0),
    ]
    cands = generate_candidates(site, reqs, LayoutPreferences(), 2)
    valid = [c for c in cands if c.report.valid]
    assert valid, [c.report.issues for c in cands]
    outline = Polygon(site.floor_boundary).buffer(0.01)
    for c in valid:
        assert all(outline.contains(Polygon(r.polygon)) for r in c.rooms)  # mapped back onto the real plan
        assert "largest rectangular area" in c.report.disclaimer


def test_suite_matching():
    assert suite_matches("Suite 110B", "110 B")
    assert suite_matches("Suite 787", "787") and suite_matches("STE 300 - Medical", "300")
    assert suite_matches("Unit 12", "Unit 12") and suite_matches("#4", "4")
    assert not suite_matches("2BR,3BA - 2,280 SF", "2")  # numbers inside other text never match
    assert not suite_matches("Suite 1100", "110") and not suite_matches("Lobby", "Retail")


def test_matterport_client_auth_and_batching():
    seen = []

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        assert req.headers["authorization"] == "Basic " + base64.b64encode(b"tid:tsecret").decode()
        q = json.loads(req.content)["query"]
        assert 'm0: model(id: "AAAA1111")' in q and 'm1: model(id: "BBBB2222")' in q
        return httpx.Response(
            200,
            json={
                "data": {
                    "m0": {
                        "id": "AAAA1111",
                        "name": "Suite 5",
                        "dimensions": {"areaFloor": 90.5},
                        "floors": [{"id": "f"}],
                    },
                    "m1": None,
                }
            },
        )

    client = httpx.Client(transport=httpx.MockTransport(handler), auth=("tid", "tsecret"))
    mp = LiveMatterportProvider("https://mp.test/graph", "tid", "tsecret", client=client)
    out = mp.model_summaries(["AAAA1111", "BBBB2222"])
    assert out == {"AAAA1111": {"name": "Suite 5", "area_sqm": 90.5, "floor_count": 1}}
    with pytest.raises(Exception, match="invalid model id"):
        mp.model_summaries(['x") { id } evil: model(id: "y'])


def test_interior_enrichment_marks_only_suite_scans(kit):
    res = KitCoStarProvider(kit).search(PropertySearchRequest(city="Los Angeles", state="CA"))
    prof = profile_from_listing(uuid4(), next(r for r in res if r.provider_listing_id == "1:16"))

    class FakeMP:
        mode = "live"

        def model_summaries(self, ids):
            return {
                "MODEL1": {"name": "Suite 106", "area_sqm": 105.0, "floor_count": 1},
                "MODEL2": {"name": "Lobby", "area_sqm": 40.0, "floor_count": 1},
            }

    out = enrich_interior(prof, FakeMP())
    mp = [a for a in out.interior_assets if a.provider == "matterport"]
    assert mp[0].asset_id == "MODEL1" and mp[0].matches_listing_unit and mp[0].has_measured_geometry
    assert mp[1].label == "Lobby" and not mp[1].has_measured_geometry
    assert any(m.provider == "matterport" and m.status == "LIVE" for m in out.source_manifest)


# ---- Vexcel -------------------------------------------------------------------------------------

VEXCEL_PROPS = {
    "structures": [
        {
            "primary-structure": True,
            "footprint.area": 812.4,
            "roof-characteristics.condition": 4.0,
            "roof-characteristics.material.class": "membrane",
            "roof-characteristics.material.score": 0.8,
            "roof-objects.ac-unit.count": 6,
            "roof-defects.structural-damage.detected": False,
            "roof-defects.discoloration.types.water-pooling.detected": True,
            "structure-distance": 0.0,
        }
    ],
    "metadata.ortho-date-estimate": "2025-12-01T10:00:00",
    "metadata.collection": "us-ca-losangeles-2025",
    "request-information.inside-parcel": True,
}


def test_vexcel_summary_and_client():
    s = summarize_property(VEXCEL_PROPS)
    assert (
        s["footprint_area_sqm"] == 812.4
        and s["rooftop_ac_units"] == 6
        and s["image_date"].startswith("2025-12")
    )

    def handler(req: httpx.Request) -> httpx.Response:
        assert req.headers["x-api-key"] == "vk"
        if req.url.path.endswith("/property/extract"):
            assert req.url.params["wkt"] == "POINT(-118.2500000 34.0500000)"
            return httpx.Response(200, json={"features": [{"properties": VEXCEL_PROPS}]})
        assert req.url.params["layer"] == "urban" and req.url.params["image-format"] == "jpeg"
        return httpx.Response(200, content=b"\xff\xd8jpeg", headers={"content-type": "image/jpeg"})

    vx = LiveVexcelProvider(
        "https://vx.test/v2",
        "vk",
        client=httpx.Client(transport=httpx.MockTransport(handler), headers={"X-API-KEY": "vk"}),
    )
    assert vx.ortho_image(34.05, -118.25).startswith(b"\xff\xd8")
    assert vx.property_elements(34.05, -118.25)["roof_material"] == "membrane"


def test_exterior_enrichment(kit):
    res = KitCoStarProvider(kit).search(PropertySearchRequest(city="Los Angeles", state="CA"))
    prof = profile_from_listing(uuid4(), res[0])

    class FakeVX:
        mode = "live"

        def property_elements(self, lat, lon):
            return summarize_property(VEXCEL_PROPS)

    out = enrich_exterior(prof, FakeVX(), "http://api.test")
    assert out.exterior_assets[-1].url == f"http://api.test/api/v1/media/vexcel/ortho/{prof.property_id}"
    assert str(out.exterior_assets[-1].captured_at) == "2025-12-01"
    types = {o.observation_type: o for o in out.building_observations}
    assert (
        types["rooftop_hvac_units"].value == 6
        and types["rooftop_hvac_units"].classification.value == "VISUALLY_OBSERVED"
    )
    assert "water pooling" in types["roof_defects"].value
    assert "No exterior imagery" not in out.missing_information


# ---- live smoke tests (opt-in) ----------------------------------------------------------------

live = pytest.mark.skipif(os.environ.get("RUN_LIVE_TESTS") != "1", reason="set RUN_LIVE_TESTS=1")


def _env(name: str) -> str:
    from app.core.config import Settings

    v = getattr(Settings(), name.lower())
    v = v.get_secret_value() if hasattr(v, "get_secret_value") else v
    if not v:
        pytest.skip(f"{name} not configured")
    return v


@pytest.mark.live
@live
def test_live_matterport_model_api():
    mp = LiveMatterportProvider(
        "https://api.matterport.com/api/models/graph",
        _env("MATTERPORT_API_KEY"),
        _env("MATTERPORT_API_TOKEN"),
    )
    doc = mp.get_floor_geometry("kvEQYM7yddK")  # CoStar kit: 630 W 6th St, Suite 110B
    assert doc and doc["model_name"] == "Suite 110B" and Polygon(doc["floor_boundary"]).area > 80


@pytest.mark.live
@live
def test_live_vexcel_api():
    vx = LiveVexcelProvider("https://api.vexcelgroup.com/v2", _env("VEXCEL_API_KEY"))
    assert vx.ortho_image(34.0501835, -118.4604085)[:2] == b"\xff\xd8"
    assert vx.property_elements(34.0501835, -118.4604085)["footprint_area_sqm"] > 0


@pytest.mark.live
@live
def test_live_costar_kit():
    from app.core.config import Settings

    p = KitCoStarProvider(Settings().costar_kit_dir)
    if not p.available():
        pytest.skip("CoStar kit not extracted")
    res = p.search(PropertySearchRequest(city="Los Angeles", state="CA", min_area_sqm=90))
    assert res and all(r.provider == "costar" for r in res)


def test_unit_letter_is_kept():
    from app.domains.property.address import _norm_unit

    assert _norm_unit("110 B") == "110B" and _norm_unit("Ste 110 B") == "110B" and _norm_unit("302") == "302"


def test_missing_net_lease_opex_is_estimated_and_flagged(kit):
    path = kit / "data" / "commercial-listing-summary.jsonl"
    quoted = [
        _space(900 + i, f"Q{i}", rate=36.0, basis=2, sqft=1200, pid=900 + i, commonAreaMaintenanceAmount=c)
        for i, c in enumerate([10, 11, 12, 12, 12, 13, 14, 1.0, 1.0, 1.0, 1.0])  # $/SF/yr (yearly basis)
    ]
    with path.open("a") as f:
        for i, sp in enumerate(quoted):
            f.write(json.dumps(_listing(900 + i, [sp], pid=900 + i)) + "\n")
    res = {r.provider_listing_id: r for r in KitCoStarProvider(kit).search(PropertySearchRequest(limit=50))}
    s16 = res["1:16"].listing  # TripleNet, no CAM quoted, 1,150 SF
    assert s16["nnn_is_estimate"] is True and "median CAM" in s16["disclaimer"]
    assert s16["nnn_charges_monthly"] == 105417  # median $11/SF/yr x 1,150 SF / 12
    assert res["900:900"].listing["nnn_is_estimate"] is False  # quoted CAM is used as-is
    assert res["1:12"].listing["nnn_charges_monthly"] == 0  # full service stays included
