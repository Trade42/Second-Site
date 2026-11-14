"""Required integrated smoke test (Phase 7.3) through the real HTTP API, services, DB and events."""

from __future__ import annotations

from uuid import uuid4


def test_end_to_end_property_slice(client, auth, demo_project_body):
    h, _user = auth
    assert client.get("/health").json()["status"] == "ok"
    ready = client.get("/ready").json()
    assert ready["checks"]["database"] == "ok"
    assert {p["provider"]: p["status"] for p in ready["providers"]}["costar"] == "MOCK"

    r = client.post("/api/v1/projects", json=demo_project_body, headers=h)
    assert r.status_code == 201, r.text
    pid = r.json()["project"]["project_id"]
    assert r.json()["business_profile"]["confirmation_status"] == "CONFIRMED"

    r = client.post(
        f"/api/v1/projects/{pid}/properties/search", json={"city": "Los Angeles", "state": "CA"}, headers=h
    )
    assert r.status_code == 202, r.text
    task = r.json()["task"]
    assert task["status"] == "COMPLETED", task
    assert client.get(r.json()["status_url"], headers=h).json()["status"] == "COMPLETED"

    props = client.get(f"/api/v1/projects/{pid}/properties", headers=h).json()
    assert len(props) == 2
    by_addr = {p["property_identity"]["street_address"]: p for p in props}
    alh = by_addr["5380 E ALHAMBRA AVE"]
    assert alh["commercial_listing"]["provider"] == "fixture"  # never presented as CoStar
    assert alh["commercial_listing"]["data_origin"] == "FIXTURE_SYNTHETIC"

    permits = client.get(f"/api/v1/projects/{pid}/properties/{alh['property_id']}/permits", headers=h).json()
    p800 = next(p for p in permits if p["permit_number"] == "19041-10000-39945")
    assert p800["data_origin"] == "FIXTURE_REAL" and p800["permit_status"] == "Permit Finaled"

    feas = client.get(f"/api/v1/projects/{pid}/properties/{alh['property_id']}/feasibility", headers=h).json()
    assert any(a["status"] == "UNKNOWN" for a in feas)

    rankings = client.get(f"/api/v1/projects/{pid}/rankings", headers=h).json()
    assert len(rankings) == 1 and len(rankings[0]["ranked_properties"]) == 2
    assert len(client.get(f"/api/v1/projects/{pid}/cost-scenarios", headers=h).json()) == 2

    events = client.get(f"/api/v1/projects/{pid}/events", headers=h).json()["events"]
    types = [e["event_type"] for e in events]
    for t in [
        "project.profile.confirmed",
        "property.search.completed",
        "property.identity.resolved",
        "property.permits.ingested",
        "property.analysis.completed",
        "infrastructure.unknown_detected",
        "financial.scenario.updated",
        "property.ranking.updated",
        "task.completed",
    ]:
        assert t in types, t
    assert len({e["correlation_id"] for e in events if e["event_type"].startswith("property.permits")}) == 1

    # Design studio: generate -> BOM linked to rooms -> revision (4th room) -> infeasible revision keeps last valid.
    hw = by_addr["6922 W HOLLYWOOD BLVD"]
    reqs = [
        {
            "room_type": "treatment",
            "count": 3,
            "min_area_sqm": 10,
            "min_width_m": 2.8,
            "needs_plumbing": True,
        },
        {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
        {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
    ]
    r = client.post(
        f"/api/v1/projects/{pid}/layouts/generate",
        headers=h,
        json={"property_id": hw["property_id"], "room_requirements": reqs},
    )
    assert r.json()["task"]["status"] == "COMPLETED", r.text
    layouts = client.get(f"/api/v1/projects/{pid}/layouts", headers=h).json()
    assert len(layouts) == 2 and all(lv["validation_report"]["valid"] for lv in layouts)
    v1 = layouts[0]
    bom = client.get(f"/api/v1/projects/{pid}/layouts/{v1['design_version_id']}/bom", headers=h).json()
    assert bom["design_version_id"] == v1["design_version_id"]
    assert sum(1 for i in bom["items"] if i["item_type"] == "treatment_bed") == 3
    assert all(i["room_id"] for i in bom["items"]) and all(
        i["price_origin"] == "FIXTURE_SYNTHETIC" for i in bom["items"]
    )

    reqs4 = [dict(reqs[0], count=4)] + reqs[1:]
    r = client.post(
        f"/api/v1/projects/{pid}/layouts/{v1['design_version_id']}/revisions",
        headers=h,
        json={"requested_changes": ["add a fourth treatment room"], "room_requirement_overrides": reqs4},
    )
    out = r.json()["task"]["output_reference"]
    assert out["revision_status"] == "APPLIED" and len(out["design_version_ids"]) == 1
    v3 = next(
        lv
        for lv in client.get(f"/api/v1/projects/{pid}/layouts", headers=h).json()
        if lv["design_version_id"] == out["design_version_ids"][0]
    )
    assert v3["parent_version_id"] == v1["design_version_id"] and v3["version_number"] == 3

    r = client.post(
        f"/api/v1/projects/{pid}/layouts/{v1['design_version_id']}/revisions",
        headers=h,
        json={
            "requested_changes": ["twelve rooms"],
            "room_requirement_overrides": [dict(reqs[0], count=12, min_area_sqm=12)],
        },
    )
    out = r.json()["task"]["output_reference"]
    assert (
        out["revision_status"] == "INFEASIBLE" and out["current_valid_version_id"] == v1["design_version_id"]
    )
    assert len(client.get(f"/api/v1/projects/{pid}/layouts", headers=h).json()) == 3  # nothing overwritten


def test_unauthorized_project_access(client, auth, demo_project_body):
    h, _ = auth
    pid = client.post("/api/v1/projects", json=demo_project_body, headers=h).json()["project"]["project_id"]
    assert client.get(f"/api/v1/projects/{pid}").status_code == 401
    from app.core.container import get_container
    from app.core.security import issue_token

    other = {
        "Authorization": "Bearer "
        + issue_token(get_container().settings.auth_secret.get_secret_value(), uuid4())
    }
    r = client.get(f"/api/v1/projects/{pid}", headers=other)
    assert r.status_code == 404 and r.json()["error_code"] == "NOT_FOUND"
    assert client.get(f"/api/v1/projects/{pid}/events", headers=other).status_code == 404
    bad = client.get(f"/api/v1/projects/{pid}", headers={"Authorization": "Bearer junk"})
    assert bad.status_code == 401 and set(bad.json()) == {
        "error_code",
        "message",
        "details",
        "request_id",
        "retryable",
    }


def test_missing_profile_is_explicit(client, auth):
    h, _ = auth
    pid = client.post(
        "/api/v1/projects",
        json={"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}},
        headers=h,
    ).json()["project"]["project_id"]
    r = client.post(f"/api/v1/projects/{pid}/properties/search", json={}, headers=h)
    assert r.json()["task"]["status"] == "FAILED" and r.json()["task"]["error_code"] == "NotFoundError"


def test_chat_deterministic_supervisor_is_grounded(client, auth, demo_project_body):
    h, _ = auth
    pid = client.post("/api/v1/projects", json=demo_project_body, headers=h).json()["project"]["project_id"]
    r = client.post(
        f"/api/v1/projects/{pid}/chat", json={"message": "Find and compare properties"}, headers=h
    ).json()
    assert r["mode"] == "deterministic" and "#1:" in r["reply"] and r["grounded_record_ids"]
