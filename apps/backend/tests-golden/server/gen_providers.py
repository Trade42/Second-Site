"""Request/response vectors for the live provider adapters (Socrata permits, Matterport, Vexcel).

Runs Python's live adapters against httpx MockTransport handlers that replay scripted responses
(shaped like those in apps/api/tests/test_sponsor_providers.py and test_identity_permits.py) and
records every request the adapter sends: method, path, decoded query pairs in order, the auth
headers and the JSON body. No network, no database.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_providers.py)
Output: provider_requests.json (deterministic; regenerates byte-identically).
"""

from __future__ import annotations

import base64
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
sys.path.insert(0, str(REPO / "apps" / "api"))

import httpx  # noqa: E402

from app.integrations.base import ProviderError, RetryPolicy  # noqa: E402
from app.integrations.la_permits import LiveLAPermitProvider  # noqa: E402
from app.integrations.matterport import LiveMatterportProvider  # noqa: E402
from app.integrations.vexcel import LiveVexcelProvider  # noqa: E402

BASE = {"socrata": "https://data.test", "matterport": "https://mp.test/graph", "vexcel": "https://vx.test/v2"}
HEADERS = ("authorization", "x-api-key", "x-app-token", "content-type")
# Each adapter's own attempt count, without the backoff sleeps.
WAIT0 = {"socrata": {"attempts": 3, "backoff_s": 0}, "matterport": {"attempts": 2, "backoff_s": 0},
         "vexcel": {"attempts": 2, "backoff_s": 0}}


def resp(status=200, json_body=None, content=None, content_type=None):
    out = {"status": status}
    if json_body is not None:
        out["json"] = json_body
    if content is not None:
        out["body_b64"] = base64.b64encode(content).decode()
    if content_type is not None:
        out["content_type"] = content_type
    return out


def transport(responses, seen):
    queue = list(responses)

    def handler(req: httpx.Request) -> httpx.Response:
        rec = {
            "method": req.method,
            "path": req.url.path,
            "query": [list(kv) for kv in req.url.params.multi_items()],
            "headers": {h: req.headers[h] for h in HEADERS if h in req.headers},
        }
        if req.content:
            rec["json"] = json.loads(req.content)
        seen.append(rec)
        r = queue.pop(0)
        headers = {"content-type": r["content_type"]} if "content_type" in r else {}
        if "json" in r:
            return httpx.Response(r["status"], json=r["json"], headers=headers)
        if "body_b64" in r:
            return httpx.Response(r["status"], content=base64.b64decode(r["body_b64"]), headers=headers)
        return httpx.Response(r["status"], headers=headers)

    return httpx.MockTransport(handler)


def l_shaped_model() -> dict:
    """The 12 x 10 m L-shaped suite of test_sponsor_providers.py."""
    pts = {"a": (0, 0), "b": (12, 0), "c": (12, 7), "d": (8, 7), "e": (8, 10), "f": (0, 10), "g": (3, 0), "h": (3, 10)}
    door = [{"type": "doorway", "width": 1.0, "relativeCenter": 1 / 3}]
    edges = [("a", "g", []), ("g", "b", door), ("b", "c", []), ("c", "d", []), ("d", "e", []), ("e", "h", []),
             ("h", "f", []), ("f", "a", []), ("g", "h", [])]
    return {
        "id": "TESTMODEL01",
        "name": "Suite 210",
        "floors": [{
            "id": "f1", "label": "Floor 1", "dimensions": {"areaFloor": 108.0},
            "vertices": [{"id": k, "position": {"x": x, "y": y}} for k, (x, y) in pts.items()],
            "edges": [{"id": f"e{i}", "type": "wall", "thickness": 0.15, "vertices": [{"id": a}, {"id": b}],
                       "openings": ops} for i, (a, b, ops) in enumerate(edges)],
        }],
        "rooms": [{"id": "r1", "floor": {"id": "f1"}, "label": "Office", "dimensions": {"areaFloor": 24.0}}],
    }


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


def raw_permits(out):
    return [{"dataset": p.dataset, "data_origin": p.data_origin.value, "source_url": p.source_url,
             "record": p.record} for p in out]


def scenarios():
    snapshot = json.loads((REPO / "fixtures" / "permits" / "la_permits_snapshot.json").read_text())["records"]
    page = [{"permit_nbr": f"P{i:04d}", "primary_address": "5380 E ALHAMBRA AVE"} for i in range(1000)]
    yield ("socrata_search_paginated_with_token", "socrata",
           {"electrical": "ysqd-apz7", "mechanical": "67is-svtd", "app_token": "tok-123", "policy": WAIT0["socrata"]},
           ("search_by_address_prefix", ["5380", "E", "ALHAMBRA", "90032"]),
           [resp(json_body=page), resp(json_body=snapshot["ysqd-apz7"][:2]), resp(json_body=[])], raw_permits)
    yield ("socrata_search_quote_no_zip_no_token", "socrata",
           {"electrical": "ysqd-apz7", "mechanical": "67is-svtd", "app_token": None, "policy": WAIT0["socrata"]},
           ("search_by_address_prefix", ["100", None, "O'NEIL", None]),
           [resp(json_body=[]), resp(json_body=[])], raw_permits)
    yield ("socrata_get_permit", "socrata",
           {"electrical": "ysqd-apz7", "mechanical": "67is-svtd", "app_token": None, "policy": WAIT0["socrata"]},
           ("get_permit", ["19041-10000-39945"]),
           [resp(json_body=snapshot["ysqd-apz7"][:1]), resp(json_body=[])], raw_permits)
    yield ("socrata_503_is_retried_then_raised", "socrata",
           {"electrical": "a", "mechanical": "b", "app_token": None, "policy": {"attempts": 2, "backoff_s": 0}},
           ("get_permit", ["x"]), [resp(503), resp(503)], raw_permits)
    yield ("socrata_404_is_not_retried", "socrata",
           {"electrical": "a", "mechanical": "b", "app_token": None, "policy": WAIT0["socrata"]},
           ("get_permit", ["x"]), [resp(404)], raw_permits)
    yield ("socrata_body_not_json", "socrata",
           {"electrical": "a", "mechanical": "b", "app_token": None, "policy": WAIT0["socrata"]},
           ("get_permit", ["x"]), [resp(200, content=b"<html>", content_type="text/html")], raw_permits)

    summaries = lambda out: out  # noqa: E731
    yield ("matterport_auth_and_batching", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [["AAAA1111", "BBBB2222"]]),
           [resp(json_body={"data": {"m0": {"id": "AAAA1111", "name": "Suite 5", "dimensions": {"areaFloor": 90.5},
                                            "floors": [{"id": "f"}]}, "m1": None}})], summaries)
    ids = [f"MODEL{i:03d}" for i in range(25)]
    yield ("matterport_batches_of_20", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [ids]),
           [resp(json_body={"data": {f"m{j}": {"id": m, "name": f"  Unit {j}  ", "dimensions": None, "floors": []}
                                     for j, m in enumerate(ids[:20])}}),
            resp(json_body={"data": {f"m{j}": {"id": m, "name": "", "dimensions": {"areaFloor": 12},
                                               "floors": [{"id": "a"}, {"id": "b"}]}
                                     for j, m in enumerate(ids[20:])}})], summaries)
    yield ("matterport_invalid_model_id", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [['x") { id } evil: model(id: "y']]), [], summaries)
    yield ("matterport_graphql_auth_error", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [["AAAA1111"]]),
           [resp(json_body={"errors": [{"message": "Not Authorized for model"}, {"message": "x"}]})], summaries)
    yield ("matterport_graphql_other_error", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [["AAAA1111"]]),
           [resp(json_body={"errors": [{"message": "Cannot query field 'bogus'"}], "data": None})], summaries)
    yield ("matterport_http_401", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [["AAAA1111"]]), [resp(401)], summaries)
    yield ("matterport_http_500_retried", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("model_summaries", [["AAAA1111"]]), [resp(500), resp(500)], summaries)
    yield ("matterport_not_configured", "matterport", {"id": None, "secret": "tsecret", "policy": None},
           ("model_summaries", [["AAAA1111"]]), [], summaries)
    yield ("matterport_floor_geometry", "matterport", {"id": "tid", "secret": "tsecret", "policy": None},
           ("get_floor_geometry", ["TESTMODEL01"]), [resp(json_body={"data": {"model": l_shaped_model()}})],
           summaries)

    jpeg = b"\xff\xd8\xff\xe0jpeg-bytes"
    image = lambda out: base64.b64encode(out).decode() if isinstance(out, bytes) else out  # noqa: E731
    yield ("vexcel_ortho_image", "vexcel", {"key": "vk", "policy": None},
           ("ortho_image", [34.05, -118.25]), [resp(200, content=jpeg, content_type="image/jpeg")], image)
    yield ("vexcel_ortho_not_an_image", "vexcel", {"key": "vk", "policy": None},
           ("ortho_image", [34.0501835, -118.4604085]),
           [resp(200, json_body={"error": "no"}, content_type="application/json")], image)
    yield ("vexcel_property_elements", "vexcel", {"key": "vk", "policy": None},
           ("property_elements", [34.05, -118.25]), [resp(json_body={"features": [{"properties": VEXCEL_PROPS}]})],
           image)
    yield ("vexcel_capture_dates", "vexcel", {"key": "vk", "policy": None},
           ("capture_dates", [34.05, -118.25]),
           [resp(json_body={"features": [{"properties": {"layer": "urban", "last-capture-date": "2025-11-02"}},
                                         {"properties": None}, {}]})], image)
    yield ("vexcel_429_then_image", "vexcel", {"key": "vk", "policy": {"attempts": 2, "backoff_s": 0}},
           ("ortho_image", [34.05, -118.25]), [resp(429), resp(200, content=jpeg, content_type="image/jpeg")],
           image)
    yield ("vexcel_coordinates_out_of_range", "vexcel", {"key": "vk", "policy": None},
           ("ortho_image", [91.0, 0.0]), [], image)
    yield ("vexcel_not_configured", "vexcel", {"key": None, "policy": None},
           ("property_elements", [34.05, -118.25]), [], image)


def build(kind, args, client):
    pol = RetryPolicy(**(args["policy"] or WAIT0[kind]))
    if kind == "socrata":
        return LiveLAPermitProvider(BASE["socrata"], args["electrical"], args["mechanical"], args["app_token"],
                                    policy=pol, client=client)
    if kind == "matterport":
        return LiveMatterportProvider(BASE["matterport"], args["id"], args["secret"], policy=pol, client=client)
    return LiveVexcelProvider(BASE["vexcel"], args["key"], policy=pol, client=client)


def client_for(kind, args, tr):
    # The headers each adapter's own default client would carry.
    if kind == "socrata":
        return httpx.Client(transport=tr, headers={"X-App-Token": args["app_token"]} if args["app_token"] else {})
    if kind == "matterport":
        return httpx.Client(transport=tr, auth=(args["id"] or "", args["secret"] or ""))
    return httpx.Client(transport=tr, headers={"X-API-KEY": args["key"] or ""})


def main() -> None:
    out = []
    for name, kind, args, (method, call_args), responses, render in scenarios():
        seen: list = []
        prov = build(kind, args, client_for(kind, args, transport(responses, seen)))
        result: dict = {}
        try:
            result["ok"] = render(getattr(prov, method)(*call_args))
        except ProviderError as e:
            result["error"] = {"provider": e.provider, "kind": e.kind.value, "message": e.message,
                               "retryable": e.retryable}
        args = {**args, "policy": args["policy"] or WAIT0[kind]}
        out.append({"name": name, "kind": kind, "args": args, "base_url": BASE[kind], "method": method,
                    "call_args": call_args, "responses": responses, "requests": seen, "result": result})
    (HERE / "provider_requests.json").write_text(json.dumps(out, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote provider_requests.json: {len(out)} scenarios")


if __name__ == "__main__":
    main()
