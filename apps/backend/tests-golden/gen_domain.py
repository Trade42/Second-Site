"""Generate ss-domain golden vectors from the Python implementation (the specification).

Run from the repository root:
    (cd apps/api && .venv/bin/python ../backend/tests-golden/gen_domain.py)

Pure functions and file-based fixture providers only: no database, no network.
Reads fixtures/ (never copies them). Writes apps/backend/tests-golden/domain/*.json and the two
SYNTHETIC CoStar-kit-shaped test directories (the same synthetic kit apps/api/tests/
test_sponsor_providers.py builds; it is not CoStar data).

Freshly generated ids and timestamps are normalized identically on both sides (see normalize()).
"""

from __future__ import annotations

import csv
import dataclasses
import json
import shutil
import sys
from datetime import date
from pathlib import Path
from uuid import UUID

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.path.insert(0, str(REPO / "apps" / "api"))

from app.contracts import BusinessExpansionProfile, PermitCategory, PropertySearchRequest  # noqa: E402
from app.domains.permits.extract import category_for, extract_facts  # noqa: E402
from app.domains.permits.service import PermitIntelligenceService  # noqa: E402
from app.domains.property.address import _norm_unit, classify_designator, parse_street  # noqa: E402
from app.domains.property.identity import build_identity, match_record  # noqa: E402
from app.domains.property.service import PropertyScoutService, attach_permits, profile_from_listing  # noqa: E402
from app.domains.requirements.templates import min_area_sqm, requirements_for  # noqa: E402
from app.integrations.costar import SQFT_TO_SQM, FixtureCoStarProvider, KitCoStarProvider  # noqa: E402
from app.integrations.la_permits import FixtureLAPermitProvider  # noqa: E402

OUT = HERE / "domain"
PROJECT_ID = UUID("6b1727f7-0000-4000-8000-0000000000a1")
PROPERTY_ID = UUID("6b1727f7-0000-4000-8000-0000000000b1")
ID_KEYS = {"evidence_id", "permit_record_id"}
TS_KEYS = {"retrieved_at"}


def dump(name: str, value) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / name).write_text(json.dumps(value, indent=1, ensure_ascii=False, default=_default) + "\n")


def _default(o):
    if isinstance(o, date):
        return o.isoformat()
    raise TypeError(type(o))


def normalize(doc, property_id: str | None = None, evidence_ids: list[str] | None = None):
    """Mirror of normalize() in crates/domain/tests/golden.rs."""
    order = {e: i for i, e in enumerate(evidence_ids or [])}

    def walk(v, key=None):
        if key in ID_KEYS:
            return "<id>"
        if key in TS_KEYS and v is not None:
            return "<timestamp>"
        if key == "source_evidence_ids":
            return [order.get(x, "<unknown evidence>") for x in v]
        if isinstance(v, dict):
            return {k: walk(x, k) for k, x in v.items()}
        if isinstance(v, list):
            return [walk(x) for x in v]
        if property_id is not None and v == property_id:
            return "<property_id>"
        return v

    return walk(doc)


def parsed(p) -> dict:
    d = dataclasses.asdict(p)
    d.update(
        street_line=p.street_line,
        normalized=p.normalized(),
        normalized_without_unit=p.normalized(include_unit=False),
        implied_floor=p.implied_floor,
    )
    return d


def identity_dump(ident) -> dict:
    d = ident.model_dump(mode="json")
    d.pop("property_id")
    return d


def raw_listing_dump(r) -> dict:
    return {
        "provider": r.provider,
        "provider_listing_id": r.provider_listing_id,
        "data_origin": r.data_origin.value,
        "address": r.address,
        "listing": r.listing,
        "interior_assets": r.interior_assets,
        "exterior_assets": r.exterior_assets,
    }


def fetch_dump(res) -> dict:
    ev_ids = [str(e.evidence_id) for e in res.evidence]
    doc = {
        "permits": [p.model_dump(mode="json") for p in res.permits],
        "evidence": [e.model_dump(mode="json") for e in res.evidence],
        "rejected": [list(x) for x in res.rejected],
        "datasets": res.datasets,
    }
    return normalize(doc, None, ev_ids)


def main() -> None:
    candidates = json.loads((REPO / "fixtures" / "properties" / "la_candidates.json").read_text())["properties"]
    snapshot = json.loads((REPO / "fixtures" / "permits" / "la_permits_snapshot.json").read_text())["records"]
    permit_rows = [(ds, r) for ds, rows in snapshot.items() for r in rows]

    # ---- Python built-in semantics ------------------------------------------------------------
    floats = [0.25, 0.35, 0.45, 2.675, 1.05, 0.125, 0.375, 110.04999999999998, 148.64486, 1e-5, 1.5e16,
              1e16, 123456789.123, 0.1 + 0.2, 0.6 + 0.15 + 0.1 + 0.15, 0.6 + 0.15 - 0.25, 0.6 - 0.3,
              0.6 + 0.15 + 0.1 - 0.3, 4.0, 36.123456, 1234567.0, 0.0001234, 0.00001234, 999999.5, 11.0,
              -2.5, 5e-324, 1.7976931348623157e308]
    floats += [sq * SQFT_TO_SQM for sq in range(100, 6000, 37)]
    dump(
        "py_semantics.json",
        {
            "floats": [
                {"x": x, "repr": repr(x), "g": format(x, "g"), "round1": round(x, 1), "round3": round(x, 3)}
                for x in floats
            ],
            "str_repr": [{"s": s, "repr": repr(s)} for s in
                         ["ROOF", "LOBBY AND", "a'b", 'a"b', "a'b\"c", "tab\tx", "\x01", "é", "back\\slash", "",
                          # Non-printable (Cc/Cf/Co/Cn/Zl/Zp/Zs) code points CPython escapes (review 2026-11-14).
                          "\x7f", "\x85", "\x9f", "\xa0", "\xad", "STE\xa0302", "\u200bROOF", "\u2028", "\u2060",
                          "\u3000", "\u1680", "\ufeff", "\u0378", "\ud7ff", "\U000e0001", "\U0010ffff",
                          "\u65e5\u672c", "\U0001f600", "\xe9\u0301"]],
            "title": [{"s": s, "title": s.title()} for s in ["LOS ANGELES", "HOUSTON", "new york", "o'neil st"]],
        },
    )

    # ---- address normalization and identity -----------------------------------------------------
    addr = {"listings": [], "permit_addresses": [], "units": [], "designators": [], "errors": []}
    for p in candidates:
        args = (p["street_address"], p["unit_or_suite"], p["city"], p["state"], p["postal_code"])
        ident = build_identity(*args, p["parcel_id"], p["latitude"], p["longitude"], {"fixture": p["fixture_key"]})
        addr["listings"].append({
            "args": list(args) + [p["parcel_id"], p["latitude"], p["longitude"], p["fixture_key"]],
            "parsed": parsed(parse_street(*args)),
            "identity": identity_dump(ident),
        })
    extra_streets = [
        "5380 East Alhambra Avenue", "6922 W HOLLYWOOD BLVD 302", "6922 W HOLLYWOOD BLVD STE 302",
        "6922 W Hollywood Blvd Suite 302", "6922 w hollywood blvd #302", "100 N Main", "100 West St",
        "12B Ocean Ave Rm 4", "1 Nowhere St", "630 W 6th St Ste 110 B", "77 N.  Spring   St., Unit 5",
        "900 S Figueroa St Level P2", "900 S FIGUEROA ST, LOS ANGELES", "42 Elm Way 3rd Flr",
    ]
    for ds, r in permit_rows:
        addr["permit_addresses"].append({"street": r["primary_address"], "parsed": parsed(parse_street(r["primary_address"]))})
    for s in extra_streets:
        addr["permit_addresses"].append({"street": s, "parsed": parsed(parse_street(s, None, "Los Angeles", "ca", "90012"))})
    for u in ["110 B", "Ste 110 B", "302", "Suite 302", "#314", "UNIT 7", "5th Floor", "ROOF", "", None, "A",
              "Suite #304", "ste. 12-a", "  9  "]:
        addr["units"].append({"unit": u, "norm": _norm_unit(u)})
    for d in ["", "STE 302", "# 314", "5TH FLOOR", "FLOOR 3", "LEVEL P1", "P1 ELEVATO", "ROOF", "LOBBY AND",
              "3RD FLOOR", "2ND FL", "12TH FLOOR", "5TH FLOOR-LOBBY CORR", "302", "A12", "TRAILER PA",
              "1/2 FLOORS", "UNIT 200", "APT 4B", "SPACE 9"]:
        addr["designators"].append({"rest": d, "result": list(classify_designator(d))})
    for bad in ["Main St", "", "   ", "#12 Main St", "ONE MAIN ST"]:
        try:
            parse_street(bad)
            raise SystemExit(f"expected failure for {bad!r}")
        except ValueError as exc:
            addr["errors"].append({"street": bad, "message": str(exc)})
    dump("address.json", addr)

    # ---- identity matching over every snapshot permit -----------------------------------------
    idents = [
        ("hollywood_302", build_identity("6922 W Hollywood Blvd", "302", "Los Angeles", "CA", "90028", "5548006011")),
        ("alhambra", build_identity("5380 E Alhambra Ave", None, "Los Angeles", "CA", "90032", "5220031007")),
        ("hollywood_no_direction", build_identity("6922 Hollywood Blvd", "Suite 302", "Los Angeles", "CA", "90028", None)),
        ("hollywood_building", build_identity("6922 W Hollywood Blvd", None, "Los Angeles", "CA", "90028", None)),
        ("hollywood_314", build_identity("6922 W Hollywood Blvd", "314", "Los Angeles", "CA", None, "5548006011")),
        ("nowhere", build_identity("1 Nowhere St", None, "Los Angeles", "CA", "90001", None)),
    ]
    for p in candidates:
        idents.append((p["fixture_key"], build_identity(p["street_address"], p["unit_or_suite"], p["city"],
                                                         p["state"], p["postal_code"], p["parcel_id"])))
    idents = [(k, i.model_copy(update={"property_id": PROPERTY_ID})) for k, i in idents]
    probes = [(r["primary_address"], r.get("zip_code"), r.get("apn")) for _, r in permit_rows]
    probes += [
        ("5382 E ALHAMBRA AVE", "90032", None), ("5380 W ALHAMBRA AVE", "90032", None),
        ("5380 E ALHAMBRA AVE", "90031", None), ("5380 E ALHAMBRA AVE", "90032", "9999999999"),
        ("5380 E ALHAMBRA ST", "90032", None), ("6922 W HOLLYWOOD BLVD 3rd Floor", "90028", "5548006011"),
        ("6922 W HOLLYWOOD BLVD 5th Floor", "90028", "5548006011"), ("not an address", None, None),
        ("6922 HOLLYWOOD BLVD", "", ""), ("6922 W HOLLYWOOD BLVD ROOF", None, None),
    ]
    matching = []
    for key, ident in idents:
        for a, z, apn in probes:
            m = match_record(ident, a, z, apn)
            matching.append({"identity": key, "address": a, "zip": z, "apn": apn, "matched": m.matched,
                             "status": m.status.value, "confidence": m.confidence, "scope": m.scope.value,
                             "explanation": m.explanation})
    dump("identities.json", {k: i.model_dump(mode="json") for k, i in idents})
    dump("matching.json", matching)

    # ---- permit extraction ---------------------------------------------------------------------
    extraction = []
    texts = [r.get("work_desc", "") for _, r in permit_rows] + [
        "SCOPE OF WORK INCLUDES A NEW 800A (120/240 3P 4W) SERVICE FEEDING VARIOUS SUBPANELS, AND ONE TRANSFORMER. TOTAL AFFECTED AREA IS 9840 SFQT.",
        "POTABLE WATER AND WASTE/VENT. NEW 2 INCH WATER SERVICE WITH BACKFLOW PREVENTER.",
        "Rough and finish for 1 break room sink", "cap 2 sinks", "NEW 1 1/2 INCH WATER SERVICE",
        "3/4\" WATER SERVICE", "install (3) circuits", "add two circuits", "SERVICE UPGRADE TO 400 AMPS",
        "(2) 5-TON RTU AND (4) VAV BOXES HVAC TI", "relocate (12) fire sprinkler heads", "KITCHEN SINK REPLACEMENT",
        "THREE LAV SINKS AND WATER HEATER, RESTROOM", "CHANGE OF USE 1,250 SQ FT", "café 12 sprk heads",
    ]
    for t in texts:
        for cat in PermitCategory:
            facts = extract_facts(cat, t)
            if facts or cat in (PermitCategory.ELECTRICAL, PermitCategory.PLUMBING):
                extraction.append({"category": cat.value, "text": t, "facts": [f.model_dump(mode="json") for f in facts]})
    categories = [
        {"group": r.get("permit_group"), "type": r.get("permit_type"),
         "category": category_for(r.get("permit_group"), r.get("permit_type")).value}
        for _, r in permit_rows
    ] + [{"group": g, "type": t, "category": category_for(g, t).value} for g, t in
         [(None, None), ("Mechanical", "Pressure Vessel"), ("mechanical", "hvac"), (None, "Electrical"),
          ("Building", "Fire Sprinkler"), ("Other", "Plumbing")]]
    failures = []
    for cat, t in [(PermitCategory.PLUMBING, "1/0 INCH WATER SERVICE"), (PermitCategory.OTHER, ",,, SF")]:
        try:
            extract_facts(cat, t)
            raise SystemExit("expected failure")
        except (ValueError, ZeroDivisionError) as exc:
            failures.append({"category": cat.value, "text": t, "error": type(exc).__name__})
    dump("extraction.json", {"facts": extraction, "categories": categories, "failures": failures})

    # ---- permit matching through the fixture provider ------------------------------------------
    svc = PermitIntelligenceService(FixtureLAPermitProvider())
    fetch = {}
    for key, ident in idents:
        fetch[key] = fetch_dump(svc.fetch_and_match(ident, PROJECT_ID))
    fetch["alhambra_no_project"] = fetch_dump(svc.fetch_and_match(dict(idents)["alhambra"]))
    dump("permits_fetch.json", fetch)

    # ---- Property Scout + attach_permits over the fixture listings ------------------------------
    scout = PropertyScoutService(FixtureCoStarProvider())
    requests = {
        "default": PropertySearchRequest(),
        "max_rent_400000": PropertySearchRequest(max_monthly_rent=400000),
        "min_area_150": PropertySearchRequest(min_area_sqm=150),
        "limit_1": PropertySearchRequest(limit=1),
        "lowercase_city": PropertySearchRequest(city="los angeles", state="ca"),
        "houston": PropertySearchRequest(city="Houston", state="TX"),
        "unsupported": PropertySearchRequest(city="Chicago", state="IL"),
    }
    profiles = {}
    for name, req in requests.items():
        out = []
        for prof in scout.search(PROJECT_ID, req):
            res = svc.fetch_and_match(prof.property_identity, PROJECT_ID)
            full = attach_permits(prof, res.permits, res.datasets, "FIXTURE")
            pid = str(prof.property_id)
            ev_ids = [str(e.evidence_id) for e in res.evidence]
            out.append({
                "search_profile": normalize(prof.model_dump(mode="json"), pid),
                "profile": normalize(full.model_dump(mode="json"), pid, ev_ids),
                "evidence": normalize([e.model_dump(mode="json") for e in res.evidence], pid, ev_ids),
            })
        profiles[name] = {"request": req.model_dump(mode="json"), "profiles": out}
    dump("property_profiles.json", profiles)

    # ---- CoStar data kit provider over the SYNTHETIC kit from test_sponsor_providers.py ------------
    kit_dir, opex_dir = HERE / "costar_kit_synthetic", HERE / "costar_kit_synthetic_opex"
    build_synthetic_kit(kit_dir, opex=False)
    build_synthetic_kit(opex_dir, opex=True)
    kit_requests = {
        "la": PropertySearchRequest(city="Los Angeles", state="CA", limit=10),
        "houston": PropertySearchRequest(city="Houston", state="TX"),
        "budget_area": PropertySearchRequest(city="Los Angeles", state="CA", max_monthly_rent=400000, min_area_sqm=90),
        "limit_1": PropertySearchRequest(limit=1),
        "neighborhood_hit": PropertySearchRequest(neighborhoods=["angeles"]),
        "neighborhood_miss": PropertySearchRequest(neighborhoods=["Pasadena"]),
        "other_city": PropertySearchRequest(city="Pasadena", state="CA"),
        "unsupported_state": PropertySearchRequest(city="New York", state="NY"),
        "min_area_100": PropertySearchRequest(min_area_sqm=100),
    }
    kit = {}
    for label, d, reqs in [("kit", kit_dir, kit_requests), ("opex", opex_dir, {"limit_50": PropertySearchRequest(limit=50)})]:
        prov = KitCoStarProvider(d)
        kit[label] = {}
        for name, req in reqs.items():
            res = prov.search(req)
            kit[label][name] = {
                "request": req.model_dump(mode="json"),
                "skipped_no_rate": prov.skipped_no_rate,
                "listings": [raw_listing_dump(r) for r in res],
                "profiles": [
                    normalize(pr.model_dump(mode="json"), str(pr.property_id))
                    for pr in (profile_from_listing(PROJECT_ID, r) for r in res)
                ],
            }
        kit[label]["status"] = prov.status().model_dump(mode="json")
    missing = KitCoStarProvider(HERE / "costar_kit_does_not_exist")
    kit["missing_status"] = missing.status().model_dump(mode="json")
    dump("costar_kit.json", kit)

    # ---- requirement templates -----------------------------------------------------------------
    base = json.loads((REPO / "fixtures" / "projects" / "facial_studio.json").read_text())["business_profile"]
    base = dict(base, project_id=str(PROJECT_ID))

    def variant(**changes):
        d = json.loads(json.dumps(base))
        for path, value in changes.items():
            cur = d
            keys = path.split("__")
            for k in keys[:-1]:
                cur = cur.setdefault(k, {})
            cur[keys[-1]] = value
        return d

    variants = {
        "facial_studio": variant(),
        "no_rooms": variant(space_requirements__required_rooms={}),
        "no_treatment": variant(space_requirements__required_rooms={"reception": 1, "office": 2}),
        "large_min_area": variant(space_requirements__min_area_sqm=500),
        "restaurant": variant(business_type="restaurant"),
        "salon_200a": variant(business_type="salon", infrastructure_requirements__min_electrical_service_amps=200),
        "two_sinks_each": variant(infrastructure_requirements__sinks_per_treatment_room=2),
        "zero_sinks": variant(infrastructure_requirements__sinks_per_treatment_room=0),
        "unknown_room": variant(space_requirements__required_rooms={"treatment": 2, "lab": 3, "restroom": 1}),
        "fractional": variant(space_requirements__required_rooms={"treatment": 7, "storage": 3}),
    }
    templates = {}
    for name, data in variants.items():
        prof = BusinessExpansionProfile.model_validate_json(json.dumps(data))
        templates[name] = {
            "profile": data,
            "min_area_sqm": min_area_sqm(prof),
            "requirements": [r.model_dump(mode="json") for r in requirements_for(prof)],
        }
    dump("templates.json", templates)
    print(f"wrote golden vectors to {OUT}")


# ---- synthetic kit (copied from apps/api/tests/test_sponsor_providers.py) -------------------------


def _space(sid, suite, use=5, sqft=1200, rate=4.0, basis=1, service=13, **kw):
    return {
        "id": sid, "propertyId": kw.pop("pid", 100), "suite": suite, "spaceUse": use, "serviceType": service,
        "squareFeetMax": sqft, "rateLow": rate, "rateHigh": rate, "rateBasis": basis, "isLeasePending": False,
        "availableOnDate": "2026-12-01T00:00:00-05:00", "leaseTermLow": 5, "leaseTermType": 1, **kw,
    }


def _listing(lid, spaces, state="CA", pid=100, **kw):
    return {
        "id": lid, "listingType": 14, "listingStatus": 1, "propertyId": pid, "buildingName": "Test Plaza",
        "brokerageCompanyName": "Example Brokerage", "certifiedFreshDate": "2026-10-01T00:00:00-04:00",
        "monetaryUnit": 1,
        "address": {
            "streetNumber": "100", "streetDirection": "W", "streetName": "Main", "streetType": "St",
            "city": "Los Angeles" if state == "CA" else "Houston", "state": state, "postcode": "900121234",
            "latitude": 34.05, "longitude": -118.25,
        },
        "attachments": [
            {"attachmentUri": "https://cdn.example/i/abc/112/image.jpg", "videoExternalReferenceType": 0},
            {"videoKey": "MODEL1", "videoExternalReferenceType": 36},
        ],
        "forLeaseSummary": {"spaces": spaces},
        **kw,
    }


def build_synthetic_kit(root: Path, opex: bool) -> None:
    if root.exists():
        shutil.rmtree(root)
    (root / "data").mkdir(parents=True)
    (root / "schemas").mkdir()
    (root / "README.md").write_text(
        "SYNTHETIC TEST DATA shaped like the CoStar data kit. Not CoStar data and not a real listing.\n"
        "Generated by ../gen_domain.py from the synthetic kit in apps/api/tests/test_sponsor_providers.py.\n"
        "KitCoStarProvider labels whatever it reads as provider='costar'; that label is the code under test,\n"
        "not a claim about this data.\n"
    )
    (root / "schemas" / "enums.json").write_text(json.dumps({
        "ServiceType": {"7": "FullService", "13": "TripleNet", "9": "ModifiedGross"},
        "SpaceUse": {"1": "Office", "4": "Medical", "5": "Retail", "7": "Industrial"},
    }))
    listings = [
        _listing(1, [
            _space(11, "101", rate=4.0, basis=1, sqft=1200),
            _space(12, "102", rate=48.0, basis=2, sqft=1000, service=7),
            _space(13, "103", rate=0, sqft=1100),
            _space(14, "104", sqft=1300, dateOffMarket="2026-01-01T00:00:00"),
            _space(15, "105", use=7),
            _space(16, "106", rate=3.0, sqft=1150, notes=[{"value": "Former salon, plumbing in place"}]),
        ]),
        _listing(2, [_space(21, "A", rate=2.0, sqft=1500, pid=200)], pid=200),
        _listing(3, [_space(31, "9", rate=1.0, sqft=1500, pid=300)], state="TX", pid=300),
        {**_listing(4, [_space(41, "X", pid=400)], pid=400), "listingStatus": 3},
        # Extra coverage beyond the Python test: floor plan, contacts, shell build-out, sqft min,
        # yearly CAM, a pending space and a marketing address.
        _listing(5, [
            _space(51, " 7B ", pid=500, use=4, rate=30.0, basis=2, sqft=None, squareFeetMin=900, service=9,
                   floorPlan={"attachmentUri": "https://cdn.example/fp/112/plan.png"},
                   notes=[{"value": "Warm shell"}, {"value": None}], commonAreaMaintenanceAmount=6.5),
            _space(52, "8", pid=500, isLeasePending=True),
            _space(53, None, pid=500, rate=5.5, basis=1, sqft=1010, leaseTermType=2, leaseTermLow=36,
                   notes=[{"value": "Turn-key office"}],
                   marketingAddress={"streetNumber": "12", "streetName": "Spring", "streetType": "St",
                                     "city": "", "state": "ca", "postcode": "90013"}),
        ], pid=500, listingContacts=[{"firstName": "Pat", "phoneNumber": "123"},
                                     {"firstName": "Sam", "lastName": "Doe", "phoneNumber": "(555) 555-0100",
                                      "locationName": "Example Brokerage"}]),
    ]
    if opex:
        for i, c in enumerate([10, 11, 12, 12, 12, 13, 14, 1.0, 1.0, 1.0, 1.0]):
            sp = _space(900 + i, f"Q{i}", rate=36.0, basis=2, sqft=1200, pid=900 + i, commonAreaMaintenanceAmount=c)
            listings.append(_listing(900 + i, [sp], pid=900 + i))
    (root / "data" / "commercial-listing-summary.jsonl").write_text("\n".join(json.dumps(r) for r in listings) + "\n")
    with (root / "commercial-properties.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["PropertyId", "MatterportModelId", "Address", "City", "State"])
        w.writerow([100, "MODEL1", "", "", ""])
        w.writerow([100, "MODEL2", "", "", ""])
        w.writerow([100, "MODEL1", "", "", ""])
        w.writerow([200, "", "", "", ""])
        w.writerow([500, " MODEL5 ", "", "", ""])


if __name__ == "__main__":
    main()
