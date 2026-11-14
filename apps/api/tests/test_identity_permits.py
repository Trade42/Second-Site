"""Address normalization, identity matching, permit extraction and provider adapters."""

from __future__ import annotations

import json

import httpx
import pytest

from app.contracts import MatchStatus, PermitCategory, PermitScope
from app.domains.permits.extract import extract_facts
from app.domains.permits.service import PermitIntelligenceService
from app.domains.property.address import parse_street
from app.domains.property.identity import build_identity, match_record
from app.integrations.la_permits import FixtureLAPermitProvider, LiveLAPermitProvider

HOLLYWOOD = build_identity("6922 W Hollywood Blvd", "302", "Los Angeles", "CA", "90028", "5548006011")
ALHAMBRA = build_identity("5380 E Alhambra Ave", None, "Los Angeles", "CA", "90032", "5220031007")


def test_normalization_preserves_direction_unit_zip():
    assert HOLLYWOOD.normalized_address == "6922 W HOLLYWOOD BLVD STE 302, LOS ANGELES, CA 90028"
    assert ALHAMBRA.normalized_address == "5380 E ALHAMBRA AVE, LOS ANGELES, CA 90032"
    assert parse_street("5380 East Alhambra Avenue").street_line == "5380 E ALHAMBRA AVE"
    for raw in [
        "6922 W HOLLYWOOD BLVD 302",
        "6922 W HOLLYWOOD BLVD STE 302",
        "6922 W Hollywood Blvd Suite 302",
        "6922 w hollywood blvd #302",
    ]:
        assert parse_street(raw).unit == "302", raw


def test_suite_specific_matching():
    m = match_record(HOLLYWOOD, "6922 W HOLLYWOOD BLVD STE 302", "90028", "5548006011")
    assert m.matched and m.scope == PermitScope.UNIT and m.status == MatchStatus.EXACT
    assert not match_record(HOLLYWOOD, "6922 W HOLLYWOOD BLVD 304", "90028", "5548006011").matched
    b = match_record(HOLLYWOOD, "6922 W HOLLYWOOD BLVD", "90028", "5548006011")
    assert b.matched and b.scope == PermitScope.BUILDING


def test_never_match_on_street_name_alone_or_conflicts():
    assert not match_record(ALHAMBRA, "5382 E ALHAMBRA AVE", "90032", None).matched  # neighbor building
    assert match_record(ALHAMBRA, "5380 W ALHAMBRA AVE", "90032", None).status == MatchStatus.CONFLICTING
    assert match_record(ALHAMBRA, "5380 E ALHAMBRA AVE", "90031", None).status == MatchStatus.CONFLICTING
    assert (
        match_record(ALHAMBRA, "5380 E ALHAMBRA AVE", "90032", "9999999999").status == MatchStatus.CONFLICTING
    )
    assert not match_record(ALHAMBRA, "5380 E ALHAMBRA ST", "90032", None).matched


def test_ambiguous_floor_level_records_need_review():
    m = match_record(HOLLYWOOD, "6922 W HOLLYWOOD BLVD 3rd Floor", "90028", "5548006011")
    assert m.matched and m.status == MatchStatus.NEEDS_REVIEW and m.scope == PermitScope.FLOOR
    assert not match_record(HOLLYWOOD, "6922 W HOLLYWOOD BLVD 5th Floor", "90028", "5548006011").matched


def test_missing_direction_is_not_used_as_evidence():
    ident = build_identity("6922 Hollywood Blvd", "Suite 302", "Los Angeles", "CA", "90028", None)
    m = match_record(ident, "6922 W HOLLYWOOD BLVD 302", "90028", "5548006011")
    assert m.matched and m.status == MatchStatus.PROBABLE and "direction missing" in m.explanation


def test_extraction_from_real_permit_text():
    elec = extract_facts(
        PermitCategory.ELECTRICAL,
        "SCOPE OF WORK INCLUDES A NEW 800A (120/240 3P 4W) SERVICE FEEDING VARIOUS SUBPANELS, AND ONE TRANSFORMER. TOTAL AFFECTED AREA IS 9840 SFQT.",
    )
    got = {f.fact_type: f.value for f in elec}
    assert got["electrical_service_amps"] == 800 and got["electrical_service_voltage"] == "120/240"
    assert got["electrical_service_phases"] == 3 and got["affected_area_sqft"] == 9840
    assert all(f.limitations for f in elec)
    water = {
        f.fact_type: f.value
        for f in extract_facts(
            PermitCategory.PLUMBING,
            "POTABLE WATER AND WASTE/VENT. NEW 2 INCH WATER SERVICE WITH BACKFLOW PREVENTER.",
        )
    }
    assert water["water_service_diameter"] == 2.0 and water["backflow_preventer"] is True
    sink = {
        f.fact_type: f.value
        for f in extract_facts(PermitCategory.PLUMBING, "Rough and finish for 1 break room sink")
    }
    assert sink == {"sink_installed": 1, "sink_type": "break_room"}
    assert {f.fact_type: f.value for f in extract_facts(PermitCategory.PLUMBING, "cap 2 sinks")} == {
        "sinks_capped": 2
    }


def test_permit_number_lookup_and_historical_preservation():
    raws = FixtureLAPermitProvider().get_permit("19041-10000-39945")
    assert len(raws) == 1 and raws[0].record["status_desc"] == "Permit Finaled"
    res = PermitIntelligenceService(FixtureLAPermitProvider()).fetch_and_match(ALHAMBRA)
    rec = next(p for p in res.permits if p.permit_number == "19041-10000-39945")
    assert rec.permit_status == "Permit Finaled" and str(rec.finaled_date) == "2022-07-20"
    ev = [
        e
        for e in res.evidence
        if e.source_record_id == rec.permit_number and e.observation_type == "electrical_service_amps"
    ]
    assert ev and ev[0].evidence_classification.value == "DOCUMENTED_HISTORICAL"
    assert any("spare capacity" in lim for lim in ev[0].limitations)
    assert ev[0].verification_status.value == "SOURCE_CONFIRMED"  # record exists; not a capacity verification


def test_suite_302_gets_its_sink_but_not_suite_304s():
    res = PermitIntelligenceService(FixtureLAPermitProvider()).fetch_and_match(HOLLYWOOD)
    nums = {p.permit_number for p in res.permits}
    assert "20042-90000-08431" in nums  # suite 302 break-room sink
    assert "20042-90000-08432" not in nums  # suite 304's identical sink permit
    assert any(n == "20042-90000-08432" for n, _ in res.rejected)


def test_missing_data_returns_no_matches():
    ident = build_identity("1 Nowhere St", None, "Los Angeles", "CA", "90001", None)
    res = PermitIntelligenceService(FixtureLAPermitProvider()).fetch_and_match(ident)
    assert res.permits == [] and res.evidence == []


def test_live_provider_query_shape_and_pagination():
    calls = []
    snapshot = json.loads((FixtureLAPermitProvider().path).read_text())["records"]["ysqd-apz7"][:3]

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(dict(request.url.params))
        return httpx.Response(
            200, json=snapshot if request.url.params["$offset"] == "0" and "ysqd" in request.url.path else []
        )

    prov = LiveLAPermitProvider(
        "https://data.lacity.org",
        "ysqd-apz7",
        "67is-svtd",
        client=httpx.Client(transport=httpx.MockTransport(handler)),
    )
    out = prov.search_by_address_prefix("5380", "E", "ALHAMBRA", "90032")
    assert len(out) == 3 and calls[0]["$where"].startswith("upper(primary_address) like '5380 E ALHAMBRA%'")
    assert "zip_code = '90032'" in calls[0]["$where"]


def test_live_provider_classifies_errors():
    from app.integrations.base import ProviderError, RetryPolicy

    prov = LiveLAPermitProvider(
        "https://data.lacity.org",
        "a",
        "b",
        policy=RetryPolicy(attempts=2, backoff_s=0),
        client=httpx.Client(transport=httpx.MockTransport(lambda r: httpx.Response(503))),
    )
    with pytest.raises(ProviderError) as ei:
        prov.get_permit("x")
    assert ei.value.retryable


@pytest.mark.live
def test_live_la_open_data_permit():
    import os

    if os.environ.get("RUN_LIVE_TESTS") != "1":
        pytest.skip("set RUN_LIVE_TESTS=1 to call data.lacity.org")
    prov = LiveLAPermitProvider("https://data.lacity.org", "ysqd-apz7", "67is-svtd")
    raws = prov.get_permit("19041-10000-39945")
    assert raws and raws[0].record["primary_address"] == "5380 E ALHAMBRA AVE"
