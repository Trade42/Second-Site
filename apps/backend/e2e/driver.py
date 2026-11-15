"""End-to-end driver, part 1: numbered checks against the running Rust service and the running Python app.

Started by run.sh (do not run it by hand unless both processes are up): apps/api/.venv/bin/python -I -B driver.py
with these variables, all set by run.sh and nothing else inherited:
  E2E_REPO, E2E_RUST_URL, E2E_PY_URL       repository root and the two loopback origins
  E2E_RUST_LOG, E2E_PY_LOG                 the two processes' log files (JSON lines; used to count requests)
  E2E_AUTH_SECRET, E2E_PHOTON_SECRET       the per-run secrets both processes were started with (never printed)
  E2E_RESULTS                              where to write the results as JSON (optional)
  E2E_DATABASE_URL                         the shared DATABASE_URL (D12 reads the rows in-process, read only)

Every check prints exactly one line: `<id> PASS|FAIL|SKIP <title> :: <detail>`. A failed check never stops the run.
A check whose input comes from an earlier check that did not pass is a SKIP that names that check.
Exit code: 1 when any check failed, else 3 when a check was skipped for any reason but a fallback that
was not needed (G02), else 0. Uses httpx only, plus the Python app's own modules (token minting,
webhook signing, the supervisor tools) imported from apps/api.
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import re
import sys
import time
import traceback
import uuid
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import httpx

REPO = Path(os.environ["E2E_REPO"])
RS = os.environ["E2E_RUST_URL"]
PY = os.environ["E2E_PY_URL"]
RUST_LOG = Path(os.environ["E2E_RUST_LOG"])
PY_LOG = Path(os.environ["E2E_PY_LOG"])
SECRET = os.environ["E2E_AUTH_SECRET"]
PHOTON_SECRET = os.environ["E2E_PHOTON_SECRET"]
TELNYX_TEST_KEY = os.environ.get("E2E_TELNYX_TEST_KEY", "")  # private half of the run's test key (K06, K08)
RESULTS = os.environ.get("E2E_RESULTS")
DATABASE_URL = os.environ.get("E2E_DATABASE_URL")
sys.path.insert(0, str(REPO / "apps" / "api"))

from app.core.security import SERVICE_PURPOSE, issue_token, verify_token  # noqa: E402
from app.integrations.webhook_signatures import standard_webhooks_sign  # noqa: E402

FIXTURE_PROJECT = REPO / "fixtures" / "projects" / "facial_studio.json"
PHOTON_INBOUND = REPO / "fixtures" / "communications" / "photon_inbound_text.json"
HOLLYWOOD = "6922 W HOLLYWOOD BLVD"
ALHAMBRA = "5380 E ALHAMBRA AVE"
NOT_APPROVED = "Not a code-compliance review or professionally approved plan."
OWNER_HANDLE = "(555) 555-0142"  # 555-01xx: a fictional number range
BROKER = {"name": "E2E Fixture Broker", "phone": "+15555550199"}
RUN = uuid.uuid4().hex[:8]
ROOMS = [
    {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8, "needs_plumbing": True},
    {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
    {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
]
SMOKE_EVENT_TYPES = [
    "project.profile.confirmed",
    "property.search.completed",
    "property.identity.resolved",
    "property.permits.ingested",
    "property.analysis.completed",
    "infrastructure.unknown_detected",
    "financial.scenario.updated",
    "property.ranking.updated",
    "task.completed",
]


# ---- outcome plumbing ------------------------------------------------------------------------------------------
class Fail(Exception):
    def __init__(self, expected: str, got: Any):
        super().__init__(f"expected {expected}; got {_short(got, 1500)}")


class Skip(Exception):
    pass


class NotNeeded(Skip):
    """A SKIP that is the expected outcome (a fallback that was not needed); every other SKIP makes the exit code 3."""


def _short(v: Any, n: int = 200) -> str:
    s = v if isinstance(v, str) else json.dumps(v, default=str)
    s = s.replace("\n", " ")
    return s if len(s) <= n else s[: n - 3] + "..."


def expect(cond: bool, expected: str, got: Any) -> None:
    if not cond:
        raise Fail(expected, got)


S: dict[str, Any] = {}  # state shared between checks
PASSED: set[str] = set()


def need(*keys: str) -> list[Any]:
    """The state values these checks produced, or a SKIP naming the check that did not pass."""
    out = []
    for k in keys:
        if k not in S:
            producer = PRODUCERS.get(k, "an earlier check")
            raise Skip(f"needs '{k}' from {producer}, which did not pass")
        out.append(S[k])
    return out


PRODUCERS: dict[str, str] = {}


def put(check_id: str, **values: Any) -> None:
    for k, v in values.items():
        S[k] = v
        PRODUCERS[k] = check_id


CHECKS: list[tuple[str, str, Callable[[], str | None]]] = []


def check(cid: str, title: str):
    def deco(fn: Callable[[], str | None]):
        CHECKS.append((cid, title, fn))
        return fn

    return deco


# ---- HTTP ------------------------------------------------------------------------------------------------------
@dataclass
class R:
    method: str
    url: str
    status: int
    body: Any
    text: str
    headers: httpx.Headers

    def show(self, n: int = 300) -> str:
        return f"{self.method} {self.url} -> {self.status} {_short(self.text, n)}"


_CLIENTS: dict[str, httpx.Client] = {}
TOKENS_SENT: set[str] = set()


def http(
    base: str,
    method: str,
    path: str,
    *,
    token: str | None = None,
    body: Any = None,
    content: bytes | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = 120.0,
) -> R:
    client = _CLIENTS.get(base)
    if client is None:
        client = _CLIENTS[base] = httpx.Client(base_url=base, trust_env=False, follow_redirects=False)
    h = {"Accept": "application/json"}
    if token:
        h["Authorization"] = f"Bearer {token}"
        TOKENS_SENT.add(token)
    if headers:
        h.update(headers)
    resp = client.request(method, path, json=body, content=content, headers=h, timeout=timeout)
    try:
        parsed = resp.json()
    except ValueError:
        parsed = None
    side = "rust" if base == RS else "python"
    return R(method, f"{side}:{path}", resp.status_code, parsed, resp.text, resp.headers)


def rs(method: str, path: str, **kw: Any) -> R:
    return http(RS, method, path, **kw)


def py(method: str, path: str, **kw: Any) -> R:
    return http(PY, method, path, **kw)


def status_is(r: R, code: int) -> None:
    expect(r.status == code, f"HTTP {code}", r.show())


def proj(pid: str, rest: str = "") -> str:
    return f"/api/v1/projects/{pid}{rest}"


# ---- the two processes' request logs ---------------------------------------------------------------------------
def log_offset(path: Path) -> int:
    return path.stat().st_size if path.exists() else 0


def requests_since(path: Path, offset: int) -> list[dict[str, Any]]:
    """Request log lines ({"msg": "request", method, path, status}) written after `offset`."""
    with path.open("rb") as f:
        f.seek(offset)
        data = f.read().decode("utf-8", "replace")
    out = []
    for line in data.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if rec.get("msg") == "request" and "path" in rec:
            out.append(rec)
    return out


def count(reqs: list[dict[str, Any]], method: str, path: str) -> int:
    return sum(1 for r in reqs if r.get("method") == method and r.get("path") == path)


# ---- comparison ------------------------------------------------------------------------------------------------
def strict_diff(a: Any, b: Any, path: str = "$", out: list[str] | None = None) -> list[str]:
    """Differences between two JSON values. Key order is ignored; types are not (1 != 1.0, true != 1)."""
    out = [] if out is None else out
    if type(a) is not type(b):
        out.append(
            f"{path}: {type(a).__name__} {_short(a, 80)} (rust) vs {type(b).__name__} {_short(b, 80)} (python)"
        )
    elif isinstance(a, dict):
        for k in sorted(set(a) | set(b)):
            if k not in b:
                out.append(f"{path}.{k}: only in the rust answer")
            elif k not in a:
                out.append(f"{path}.{k}: only in the python answer")
            else:
                strict_diff(a[k], b[k], f"{path}.{k}", out)
    elif isinstance(a, list):
        if len(a) != len(b):
            out.append(f"{path}: {len(a)} items (rust) vs {len(b)} items (python)")
        for i, (x, y) in enumerate(zip(a, b, strict=False)):
            strict_diff(x, y, f"{path}[{i}]", out)
    elif a != b:
        out.append(f"{path}: {_short(a, 80)} (rust) vs {_short(b, 80)} (python)")
    return out


def _canon(v: Any) -> str:
    return json.dumps(v, sort_keys=True)


def explain_text_diff(rust_text: str, py_text: str) -> str:
    """Why two tool texts differ: JSON content, list order only, or byte order only (keys / spacing)."""
    a, b = json.loads(rust_text), json.loads(py_text)
    d = strict_diff(a, b)
    if not d:
        i = next((k for k, (x, y) in enumerate(zip(rust_text, py_text, strict=False)) if x != y), 0)
        return (
            f"same JSON, different bytes (key order) from offset {i}: "
            f"http ...{rust_text[max(0, i - 40) : i + 40]!r} vs in-process ...{py_text[max(0, i - 40) : i + 40]!r}"
        )
    if isinstance(a, list) and isinstance(b, list) and sorted(map(_canon, a)) == sorted(map(_canon, b)):
        key = next(
            (k for k in ("requirement", "id", "property_id") if a and isinstance(a[0], dict) and k in a[0]),
            None,
        )
        order = (lambda xs: [x.get(key) for x in xs]) if key else (lambda xs: len(xs))
        return f"same {len(a)} items in a different order: http {order(a)} vs in-process {order(b)}"
    return f"{len(d)} content differences (http first, in-process second): " + "; ".join(d[:3])


def same_answer(path: str, token: str, what: str) -> str:
    """GET `path` from both origins with one token; FAIL unless both are 200 and the JSON is identical."""
    a, b = rs("GET", path, token=token), py("GET", path, token=token)
    expect(a.status == 200 and b.status == 200, "HTTP 200 from both", f"{a.show(150)} | {b.show(150)}")
    diffs = strict_diff(a.body, b.body)
    if diffs:
        order_only = (
            isinstance(a.body, list)
            and isinstance(b.body, list)
            and sorted(json.dumps(x, sort_keys=True) for x in a.body)
            == sorted(json.dumps(x, sort_keys=True) for x in b.body)
        )
        note = " (same items, different order)" if order_only else ""
        raise Fail(
            f"identical {what} from both services", f"{len(diffs)} differences{note}: " + "; ".join(diffs[:6])
        )
    size = len(a.body) if isinstance(a.body, list) else len(a.text)
    raw = "bytes identical too" if a.text == b.text else "bytes differ (object key order)"
    return f"identical ({size} {'items' if isinstance(a.body, list) else 'bytes'}; types compared, key order ignored; {raw})"


def is_int(v: Any) -> bool:
    return isinstance(v, int) and not isinstance(v, bool)


def fixture_body() -> dict[str, Any]:
    f = json.loads(FIXTURE_PROJECT.read_text())
    return {k: f[k] for k in ("project_name", "market", "business_profile")}


# =================================================================================================================
# A. The demo path through the Rust origin
# =================================================================================================================
@check("A01", "rust /health")
def a01() -> str:
    r = rs("GET", "/health")
    status_is(r, 200)
    expect(r.body.get("status") == "ok", "status ok", r.show())
    return f"ok, version {r.body.get('version')}"


@check("A02", "rust /ready (database ok on the private schema)")
def a02() -> str:
    r = rs("GET", "/ready")
    status_is(r, 200)
    checks = r.body.get("checks", {})
    expect(r.body.get("status") == "ready" and checks.get("database") == "ok", "ready, database ok", r.show())
    providers = {p.get("provider"): p.get("status") for p in r.body.get("providers", [])}
    link = next((p for p in r.body.get("providers", []) if p.get("provider") == "python_service"), None)
    if link is not None:  # the forwarder's entry; run.sh waited for it to turn LIVE
        expect(link.get("status") == "LIVE", "python_service LIVE (the python app is up)", link)
    put("A02", rust_ready=r.body)
    # For the report: where the two services' /ready disagree about the same provider (not a check by itself).
    p = py("GET", "/ready")
    py_providers = {x.get("provider"): x.get("status") for x in (p.body or {}).get("providers", [])}
    differ = {
        k: f"{providers.get(k)} (rust) / {py_providers.get(k)} (python)"
        for k in sorted(set(providers) | set(py_providers))
        if providers.get(k) != py_providers.get(k)
    }
    return f"ready; checks {checks}; providers {providers}; differs from python /ready: {differ or 'nothing'}"


@check("A03", "rust dev token (POST /api/v1/dev/token)")
def a03() -> str:
    r = rs("POST", "/api/v1/dev/token")
    status_is(r, 200)
    tok, uid = r.body.get("access_token"), r.body.get("user_id")
    expect(bool(tok) and bool(uid), "access_token and user_id", r.show())
    expect(str(verify_token(SECRET, tok)) == uid, "a token Python's verify_token accepts for user_id", uid)
    put("A03", rs_token=tok, owner=uid)
    return f"demo owner {uid}; token verifies with Python's verify_token (purpose access)"


@check("A04", "rust demo fixture inputs are labelled as fixture data")
def a04() -> str:
    r = rs("GET", "/api/v1/dev/fixtures/project")
    status_is(r, 200)
    expect(r.body == json.loads(FIXTURE_PROJECT.read_text()), "the fixture file's JSON", r.show(120))
    origin = (r.body.get("_meta") or {}).get("data_origin")
    expect(origin == "FIXTURE_SYNTHETIC", "_meta.data_origin FIXTURE_SYNTHETIC", origin)
    return "same document as fixtures/projects/facial_studio.json; _meta.data_origin FIXTURE_SYNTHETIC"


@check("A05", "create the fixture project through rust")
def a05() -> str:
    (tok, owner) = need("rs_token", "owner")
    r = rs("POST", "/api/v1/projects", token=tok, body=fixture_body())
    status_is(r, 201)
    p, prof = r.body.get("project", {}), r.body.get("business_profile") or {}
    expect(p.get("owner_id") == owner, f"owner_id {owner}", p)
    expect(prof.get("confirmation_status") == "CONFIRMED", "business_profile CONFIRMED", prof)
    put("A05", pid=p["project_id"], project_name=p.get("project_name"))
    return f"201, project {p['project_id']} ({p.get('project_status')}), profile CONFIRMED"


@check("A06", "property search and analysis (202, runs inline)")
def a06() -> str:
    (tok, pid) = need("rs_token", "pid")
    r = rs("POST", proj(pid, "/properties/search"), token=tok, body={"city": "Los Angeles", "state": "CA"})
    status_is(r, 202)
    task = r.body.get("task", {})
    expect(task.get("status") == "COMPLETED", "task COMPLETED", task)
    st = rs("GET", r.body["status_url"], token=tok)
    expect(st.status == 200 and st.body.get("status") == "COMPLETED", "status_url COMPLETED", st.show())
    out = task.get("output_reference") or {}
    expect(len(out.get("property_ids", [])) >= 2, ">= 2 property_ids", out)
    put("A06", search_task=task["task_id"])
    return f"task {task['task_id']} COMPLETED; {len(out['property_ids'])} properties; ranking v{out.get('ranking_version')}"


@check("A07", "properties: fixture listings labelled as fixture data, never CoStar")
def a07() -> str:
    (tok, pid) = need("rs_token", "pid")
    r = rs("GET", proj(pid, "/properties"), token=tok)
    status_is(r, 200)
    expect(isinstance(r.body, list) and len(r.body) >= 2, ">= 2 properties", r.show())
    by = {p["property_identity"]["street_address"]: p for p in r.body}
    expect(HOLLYWOOD in by and ALHAMBRA in by, f"{HOLLYWOOD} and {ALHAMBRA}", sorted(by))
    labels = []
    for p in r.body:
        cl = p.get("commercial_listing") or {}
        expect(cl.get("provider") == "fixture", "commercial_listing.provider fixture", cl.get("provider"))
        expect(
            str(cl.get("data_origin", "")).startswith("FIXTURE_"),
            "commercial_listing.data_origin FIXTURE_*",
            cl,
        )
        for kind in ("interior_assets", "exterior_assets"):  # Matterport scans, Vexcel imagery: fixtures too
            for a in p.get(kind) or []:
                expect(
                    a.get("provider") == "fixture" and str(a.get("data_origin")).startswith("FIXTURE_"),
                    f"{kind}: provider fixture, data_origin FIXTURE_*",
                    a,
                )
        labels.append(
            f"{p['property_identity']['street_address']}: {cl.get('provider')}/{cl.get('data_origin')}"
        )
    put("A07", props=r.body, hollywood=by[HOLLYWOOD]["property_id"], alhambra=by[ALHAMBRA]["property_id"])
    return "; ".join(labels)


@check("A08", "permit evidence attached, labelled, and historical status kept as such")
def a08() -> str:
    (tok, pid, props, alh) = need("rs_token", "pid", "props", "alhambra")
    n = sum(len(p.get("permit_records", [])) for p in props)
    expect(n > 0, "permit_records on the properties", n)
    r = rs("GET", proj(pid, f"/properties/{alh}/permits"), token=tok)
    status_is(r, 200)
    p800 = next((p for p in r.body if p.get("permit_number") == "19041-10000-39945"), None)
    expect(p800 is not None, "permit 19041-10000-39945", [p.get("permit_number") for p in r.body])
    expect(
        p800.get("data_origin") == "FIXTURE_REAL" and p800.get("permit_status") == "Permit Finaled",
        "data_origin FIXTURE_REAL, permit_status 'Permit Finaled'",
        {k: p800.get(k) for k in ("data_origin", "permit_status")},
    )
    every = list(r.body) + [x for p in props for x in p.get("permit_records", [])]
    bad = [x.get("permit_number") for x in every if not str(x.get("data_origin")).startswith("FIXTURE_")]
    expect(not bad, "every permit record (route and embedded) labelled FIXTURE_*", bad)
    origins = sorted({p.get("data_origin") for p in r.body})
    return f"{n} permit records in total; {len(r.body)} for Alhambra, data_origin {origins}"


@check("A09", "infrastructure assessments include UNKNOWN (feasibility route and property records)")
def a09() -> str:
    (tok, pid, props, alh) = need("rs_token", "pid", "props", "alhambra")
    r = rs("GET", proj(pid, f"/properties/{alh}/feasibility"), token=tok)
    status_is(r, 200)
    unknown = [a for a in r.body if a.get("status") == "UNKNOWN"]
    expect(len(unknown) > 0, "an UNKNOWN assessment", [a.get("status") for a in r.body])
    embedded = sum(
        a.get("status") == "UNKNOWN" for p in props for a in p.get("infrastructure_assessments", [])
    )
    expect(embedded > 0, "UNKNOWN in the embedded assessments", embedded)
    statuses = sorted({a.get("status") for a in r.body})
    return f"Alhambra: {len(r.body)} assessments, {len(unknown)} UNKNOWN, statuses {statuses}; {embedded} UNKNOWN embedded"


@check("A10", "cost scenarios (integer minor units)")
def a10() -> str:
    (tok, pid) = need("rs_token", "pid")
    r = rs("GET", proj(pid, "/cost-scenarios"), token=tok)
    status_is(r, 200)
    expect(len(r.body) == 2, "2 scenarios", len(r.body))
    for s in r.body:
        for k in ("opening_cost_expected", "monthly_operating_cost", "long_term_cost"):
            expect(is_int(s.get(k)), f"{k} an integer", s.get(k))
    return "2 scenarios; long_term_cost " + ", ".join(str(s["long_term_cost"]) for s in r.body)


@check("A11", "ranking persisted")
def a11() -> str:
    (tok, pid) = need("rs_token", "pid")
    r = rs("GET", proj(pid, "/rankings"), token=tok)
    status_is(r, 200)
    expect(len(r.body) == 1 and len(r.body[0].get("ranked_properties", [])) == 2, "1 ranking of 2", r.show())
    ranks = [x.get("rank") for x in r.body[0]["ranked_properties"]]
    expect(ranks == [1, 2], "ranks [1, 2]", ranks)
    return f"ranking v{r.body[0].get('ranking_version')} ranks {ranks}"


@check("A12", "activity events of the search")
def a12() -> str:
    (tok, pid) = need("rs_token", "pid")
    r = rs("GET", proj(pid, "/events?limit=500"), token=tok)
    status_is(r, 200)
    types = [e.get("event_type") for e in r.body.get("events", [])]
    missing = [t for t in SMOKE_EVENT_TYPES if t not in types]
    expect(not missing, "every event type of the Python smoke test", {"missing": missing})
    return f"{len(types)} events, all {len(SMOKE_EVENT_TYPES)} expected types present"


@check("A13", "layout generation for the property with measured geometry")
def a13() -> str:
    (tok, pid, hw, props) = need("rs_token", "pid", "hollywood", "props")
    hw_prop = next(p for p in props if p["property_id"] == hw)
    assets = [a.get("asset_id") for a in hw_prop.get("interior_assets", []) if a.get("has_measured_geometry")]
    expect(bool(assets), "an interior asset with has_measured_geometry on the Hollywood property", hw_prop)
    r = rs(
        "POST",
        proj(pid, "/layouts/generate"),
        token=tok,
        body={"property_id": hw, "room_requirements": ROOMS},
    )
    status_is(r, 202)
    task = r.body.get("task", {})
    expect(task.get("status") == "COMPLETED", "task COMPLETED", task)
    ids = (task.get("output_reference") or {}).get("design_version_ids", [])
    expect(len(ids) == 2, "2 design_version_ids", task.get("output_reference"))
    put("A13", layout_task_url=r.body["status_url"], layout_task=task["task_id"], layout_ids=ids)
    return f"asset {assets[0]} has measured geometry; task {task['task_id']} COMPLETED, 2 versions"


@check("A14", "the layout task through its status_url")
def a14() -> str:
    (tok, url) = need("rs_token", "layout_task_url")
    r = rs("GET", url, token=tok)
    status_is(r, 200)
    expect(
        r.body.get("status") == "COMPLETED" and r.body.get("task_type") == "generate_layout",
        "COMPLETED generate_layout",
        r.show(),
    )
    return f"COMPLETED {r.body.get('task_type')} ({r.body.get('assigned_subsystem')})"


@check("A15", "layout list: proposals only, valid, with the not-approved disclaimer")
def a15() -> str:
    (tok, pid, ids) = need("rs_token", "pid", "layout_ids")
    r = rs("GET", proj(pid, "/layouts"), token=tok)
    status_is(r, 200)
    expect([lv.get("design_version_id") for lv in r.body] == ids, f"versions {ids}", r.show())
    for lv in r.body:
        expect(lv.get("approval_status") == "PROPOSED", "approval_status PROPOSED", lv.get("approval_status"))
        rep = lv.get("validation_report") or {}
        expect(rep.get("valid") is True, "validation_report.valid", rep)
        expect(NOT_APPROVED in str(rep.get("disclaimer")), f"disclaimer containing '{NOT_APPROVED}'", rep)
    put("A15", v1=ids[0], geometry_id=r.body[0].get("geometry_id"))
    return (
        f"2 versions, both PROPOSED and valid; v{r.body[0]['version_number']}, v{r.body[1]['version_number']}"
    )


@check("A16", "the layouts' geometry: the fixture rectangle, labelled as fixture data, never as measured")
def a16() -> str:
    # In fixture mode the "measured" asset resolves to fixtures/geometry/hollywood_302_synthetic.json, an invented
    # rectangle. Python labels it geometry_source FIXTURE, data_origin FIXTURE_SYNTHETIC, scale_verified false
    # (domains/design/service.py, site_geometry); presenting it as MATTERPORT_MEASURED would be a truth defect.
    (tok, pid, gid) = need("rs_token", "pid", "geometry_id")
    r = rs("GET", proj(pid, f"/layouts/geometry/{gid}"), token=tok)
    status_is(r, 200)
    got = {k: r.body.get(k) for k in ("geometry_source", "data_origin", "scale_verified", "units")}
    want = {
        "geometry_source": "FIXTURE",
        "data_origin": "FIXTURE_SYNTHETIC",
        "scale_verified": False,
        "units": "m",
    }
    expect(got == want, str(want), got)
    return f"{got['geometry_source']}, {got['data_origin']}, scale_verified false, meters (source {r.body.get('source_id')})"


@check("A17", "one revision (a fourth treatment room)")
def a17() -> str:
    (tok, pid, v1) = need("rs_token", "pid", "v1")
    rooms4 = [dict(ROOMS[0], count=4)] + ROOMS[1:]
    r = rs(
        "POST",
        proj(pid, f"/layouts/{v1}/revisions"),
        token=tok,
        body={"requested_changes": ["add a fourth treatment room"], "room_requirement_overrides": rooms4},
    )
    status_is(r, 202)
    out = (r.body.get("task") or {}).get("output_reference") or {}
    expect(
        out.get("revision_status") == "APPLIED" and len(out.get("design_version_ids", [])) == 1,
        "APPLIED, 1 version",
        out,
    )
    lv = rs("GET", proj(pid, "/layouts"), token=tok).body
    v3 = next((x for x in lv if x["design_version_id"] == out["design_version_ids"][0]), {})
    expect(
        v3.get("parent_version_id") == v1
        and v3.get("version_number") == 3
        and v3.get("approval_status") == "PROPOSED",
        "version 3, child of v1, PROPOSED",
        {k: v3.get(k) for k in ("parent_version_id", "version_number", "approval_status")},
    )
    return "APPLIED as version 3 (parent v1, PROPOSED)"


@check("A18", "bill of materials: fixture prices, not supplier quotes")
def a18() -> str:
    (tok, pid, v1) = need("rs_token", "pid", "v1")
    r = rs("GET", proj(pid, f"/layouts/{v1}/bom"), token=tok)
    status_is(r, 200)
    items = r.body.get("items", [])
    beds = sum(1 for i in items if i.get("item_type") == "treatment_bed")
    expect(beds == 3, "3 treatment_bed items", beds)
    for i in items:
        expect(i.get("price_origin") == "FIXTURE_SYNTHETIC", "price_origin FIXTURE_SYNTHETIC", i)
        expect("not a supplier quote" in str(i.get("price_note")), "price_note 'not a supplier quote'", i)
        expect(bool(i.get("room_id")), "room_id on every item", i)
    total = r.body.get("estimated_total")
    return f"{len(items)} items, all FIXTURE_SYNTHETIC and 'not a supplier quote'; estimated_total {_short(total, 80)}"


# =================================================================================================================
# B. One token for both services
# =================================================================================================================
@check("B01", "a token minted by rust is accepted by the python app")
def b01() -> str:
    (tok, pid) = need("rs_token", "pid")
    r = py("GET", "/api/v1/projects", token=tok)
    status_is(r, 200)
    ids = [p.get("project_id") for p in r.body]
    expect(pid in ids, f"the rust-created project {pid} in python's list", ids)
    return f"python lists {len(ids)} project(s) for the rust token, including {pid}"


@check("B02", "the python dev token names the same owner")
def b02() -> str:
    (owner,) = need("owner")
    r = py("POST", "/api/v1/dev/token")
    status_is(r, 200)
    expect(r.body.get("user_id") == owner, f"user_id {owner}", r.body.get("user_id"))
    put("B02", py_token=r.body["access_token"])
    return f"same demo owner {owner} (one users row in the shared schema)"


@check("B03", "a token minted by python is accepted by rust")
def b03() -> str:
    (tok, pid, owner) = need("py_token", "pid", "owner")
    r = rs("GET", proj(pid), token=tok)
    status_is(r, 200)
    p = r.body.get("project", {})
    expect(p.get("project_id") == pid and p.get("owner_id") == owner, f"project {pid} of {owner}", p)
    return "rust serves the project for the python token (same owner id)"


@check("B04", "a token signed with another secret is refused by both")
def b04() -> str:
    (owner, pid) = need("owner", "pid")
    bad = issue_token("not-the-shared-secret", uuid.UUID(owner))
    a, b = rs("GET", proj(pid), token=bad), py("GET", proj(pid), token=bad)
    expect(a.status == 401 and b.status == 401, "401 from both", f"{a.show(120)} | {b.show(120)}")
    return f"401 {a.body.get('error_code')} (rust), 401 {b.body.get('error_code')} (python)"


@check("B05", "a service-purpose token is refused on ordinary routes by both")
def b05() -> str:
    (owner, pid) = need("owner", "pid")
    svc = issue_token(SECRET, uuid.UUID(owner), ttl_s=60, purpose=SERVICE_PURPOSE)
    a, b = rs("GET", proj(pid), token=svc), py("GET", proj(pid), token=svc)
    expect(a.status == 401 and b.status == 401, "401 from both", f"{a.show(120)} | {b.show(120)}")
    return "401 from both"


# =================================================================================================================
# C. Each service reads what the other wrote
# =================================================================================================================
@check("C01", "project written by rust, read by both")
def c01() -> str:
    (tok, pid) = need("rs_token", "pid")
    return same_answer(proj(pid), tok, "project envelope")


@check("C02", "properties written by rust, read by both")
def c02() -> str:
    (tok, pid) = need("rs_token", "pid")
    return same_answer(proj(pid, "/properties"), tok, "property list")


@check("C03", "rankings written by rust, read by both")
def c03() -> str:
    (tok, pid) = need("rs_token", "pid")
    return same_answer(proj(pid, "/rankings"), tok, "ranking list")


@check("C04", "cost scenarios written by rust, read by both")
def c04() -> str:
    (tok, pid) = need("rs_token", "pid")
    return same_answer(proj(pid, "/cost-scenarios"), tok, "cost scenario list")


@check("C05", "events written by rust, read by both")
def c05() -> str:
    (tok, pid) = need("rs_token", "pid")
    return same_answer(proj(pid, "/events?limit=500"), tok, "event page")


@check("C06", "layouts, geometry and bill written by rust, read by both")
def c06() -> str:
    (tok, pid, gid, v1) = need("rs_token", "pid", "geometry_id", "v1")
    parts = [
        same_answer(proj(pid, "/layouts"), tok, "layout list"),
        same_answer(proj(pid, f"/layouts/geometry/{gid}"), tok, "site geometry"),
        same_answer(proj(pid, f"/layouts/{v1}/bom"), tok, "stored bill of materials"),
    ]
    return "layouts " + parts[0] + "; geometry " + parts[1] + "; bill " + parts[2]


@check("C07", "task records written by rust, read by both")
def c07() -> str:
    (tok, pid, t1, t2) = need("rs_token", "pid", "search_task", "layout_task")
    a = same_answer(proj(pid, f"/tasks/{t1}"), tok, "search task")
    b = same_answer(proj(pid, f"/tasks/{t2}"), tok, "layout task")
    return f"search task {a}; layout task {b}"


@check("C08", "permits and feasibility of each property written by rust, read by both")
def c08() -> str:
    (tok, pid, alh, hw) = need("rs_token", "pid", "alhambra", "hollywood")
    out = []
    for label, p in (("alhambra", alh), ("hollywood", hw)):
        for rest in ("permits", "feasibility"):
            out.append(
                f"{label} {rest} "
                + same_answer(proj(pid, f"/properties/{p}/{rest}"), tok, rest).split(" (")[0]
            )
    return ", ".join(out)


@check("C09", "a second project created and searched through the python app")
def c09() -> str:
    (tok,) = need("py_token")
    r = py(
        "POST", "/api/v1/projects", token=tok, body=fixture_body() | {"project_name": f"E2E via Python {RUN}"}
    )
    status_is(r, 201)
    pid2 = r.body["project"]["project_id"]
    s = py("POST", proj(pid2, "/properties/search"), token=tok, body={"city": "Los Angeles", "state": "CA"})
    expect(s.status == 202 and s.body["task"]["status"] == "COMPLETED", "202 COMPLETED search", s.show())
    # Python answers before it commits (get_db commits in the dependency teardown, after the response; see
    # C12). Wait until rust sees the completed task, so C10/C11 compare committed rows, and say how long it took.
    t0, polls, seen_by_rust = time.monotonic(), 0, None
    while time.monotonic() - t0 < 5:
        polls += 1
        t = rs("GET", proj(pid2, f"/tasks/{s.body['task']['task_id']}"), token=tok)
        if t.status == 200 and t.body.get("status") == "COMPLETED":
            seen_by_rust = time.monotonic() - t0
            break
        time.sleep(0.02)
    expect(seen_by_rust is not None, "rust to see python's completed search task within 5 s", t.show())
    put("C09", pid2=pid2, search_task2=s.body["task"]["task_id"])
    return (
        f"project {pid2}, search task COMPLETED (python); visible to rust on poll {polls} "
        f"({seen_by_rust * 1000:.0f} ms after python answered)"
    )


@check("C10", "rust reads the project python wrote")
def c10() -> str:
    (tok, pid2) = need("py_token", "pid2")
    return same_answer(proj(pid2), tok, "project envelope")


@check(
    "C11",
    "rust reads python's properties, rankings, cost scenarios, events and task; same assessment statuses",
)
def c11() -> str:
    (tok, pid2, t) = need("py_token", "pid2", "search_task2")
    out, failures = [], []
    for rest, what in (
        ("/properties", "properties"),
        ("/rankings", "rankings"),
        ("/cost-scenarios", "cost scenarios"),
        ("/events?limit=500", "events"),
        (f"/tasks/{t}", "task"),
    ):
        try:
            out.append(f"{what} {same_answer(proj(pid2, rest), tok, what).split(' (')[0]}")
        except Fail as f:
            failures.append(f"{what}: {f}")

    # Engine parity: rust analysed the first project (A06), python this one (C09), from the same fixture inputs. A09
    # asks only for one UNKNOWN, so an engine that shows one UNKNOWN as MEETS would pass it; this would not.
    def statuses(p: str) -> dict[str, list[Any]]:
        body = rs("GET", proj(p, "/properties"), token=tok).body
        return {
            x["property_identity"]["street_address"]: sorted(
                (a.get("requirement_id"), a.get("status")) for a in x.get("infrastructure_assessments", [])
            )
            for x in body
        }

    mine, theirs = statuses(need("pid")[0]), statuses(pid2)
    if mine != theirs:
        failures.append(f"assessment statuses: rust's analysis {mine} vs python's {theirs}")
    else:
        out.append("assessment statuses of both engines identical")
    if failures:
        raise Fail(
            "identical answers for all five, and the same assessment statuses",
            " | ".join(failures) + (" || ok: " + ", ".join(out) if out else ""),
        )
    return ", ".join(out)


@check("C12", "read-after-write across the services: a write python answered is visible to rust at once")
def c12() -> str:
    """Python's get_db commits after the response is sent (FastAPI runs yield-dependency teardown after
    `await response(...)`), so a client can read through rust before python's commit. Rust commits before it
    answers. Measured both ways with project creation and an immediate GET on the other service."""
    (tok,) = need("rs_token")
    n = 30
    stale_py_to_rs, stale_rs_to_py, stale_py_to_py, examples = 0, 0, 0, []
    for i in range(n):
        body = {"project_name": f"E2E race {RUN} {i}", "market": {"city": "Los Angeles", "state": "CA"}}
        w = py("POST", "/api/v1/projects", token=tok, body=body)
        status_is(w, 201)
        r = rs("GET", proj(w.body["project"]["project_id"]), token=tok)
        if r.status != 200:
            stale_py_to_rs += 1
            if len(examples) < 2:
                examples.append(r.show(140))
        w = rs("POST", "/api/v1/projects", token=tok, body=body)
        status_is(w, 201)
        r = py("GET", proj(w.body["project"]["project_id"]), token=tok)
        if r.status != 200:
            stale_rs_to_py += 1
        w = py("POST", "/api/v1/projects", token=tok, body=body)  # baseline: python alone, no link involved
        status_is(w, 201)
        if py("GET", proj(w.body["project"]["project_id"]), token=tok).status != 200:
            stale_py_to_py += 1
    expect(
        stale_py_to_rs == 0 and stale_rs_to_py == 0,
        "every project created through one service readable through the other immediately",
        f"python-created then rust GET: {stale_py_to_rs}/{n} not found; rust-created then python GET: "
        f"{stale_rs_to_py}/{n} not found; baseline python-created then python GET: {stale_py_to_py}/{n} not found; "
        f"e.g. {examples}",
    )
    return (
        f"0/{n} stale reads in each direction (baseline python->python {stale_py_to_py}/{n}); the window is "
        "timing-dependent, so a PASS here does not prove it closed"
    )


# =================================================================================================================
# D. The supervisor reaches Rust
# =================================================================================================================
def chat_via_python(message: str, request_id: str | None = None) -> tuple[R, list[dict[str, Any]]]:
    (tok, pid) = need("rs_token", "pid")
    off = log_offset(RUST_LOG)
    headers = {"x-request-id": request_id} if request_id else None
    r = py("POST", proj(pid, "/chat"), token=tok, body={"message": message}, headers=headers)
    return r, requests_since(RUST_LOG, off)


@check("D01", "chat 'rank the properties' (model-free supervisor) answers with rust data")
def d01() -> str:
    (tok, pid) = need("rs_token", "pid")
    rid = f"e2e-chat-{RUN}"
    r, reqs = chat_via_python("rank the properties", request_id=rid)
    status_is(r, 200)
    reply = str(r.body.get("reply"))
    expect(r.body.get("mode") == "deterministic", "mode deterministic", r.body)
    expect("CONTRACT_MISMATCH" not in reply and "ERROR" not in reply, "no tool error in the reply", reply)
    expect(reply.startswith("Ranking (deterministic engine):") and "#1:" in reply, "a ranking reply", reply)
    n = count(reqs, "GET", proj(pid, "/rankings"))
    expect(
        n >= 1,
        "GET .../rankings in the rust request log during the chat",
        [f"{x['method']} {x['path']}" for x in reqs],
    )
    latest = rs("GET", proj(pid, "/rankings"), token=tok).body[-1]["ranking_id"]
    expect(latest in r.body.get("grounded_record_ids", []), f"grounded on ranking {latest}", r.body)
    # Recorded, not checked (no contract asks for it): does the chat's request id reach rust's log?
    propagated = any(x.get("request_id") == rid for x in reqs)
    return (
        f"{n} rust GET rankings during the chat; reply '{_short(reply, 90)}'; grounded on {latest}; "
        f"chat x-request-id {'reaches' if propagated else 'does NOT reach'} rust "
        f"(rust logged {sorted({str(x.get('request_id'))[:8] for x in reqs})})"
    )


@check("D02", 'chat "what\'s the status?" answers with rust data')
def d02() -> str:
    (pid, name) = need("pid", "project_name")
    r, reqs = chat_via_python("what's the status?")
    status_is(r, 200)
    reply = str(r.body.get("reply"))
    expect(
        reply.startswith(f"Project '{name}' is in status "), f"\"Project '{name}' is in status ...\"", reply
    )
    n = count(reqs, "GET", proj(pid))
    expect(
        n >= 1,
        "GET /api/v1/projects/{id} in the rust request log",
        [f"{x['method']} {x['path']}" for x in reqs],
    )
    return f"{n} rust GET project during the chat; reply '{_short(reply, 90)}'"


def _tools() -> dict[str, Any]:
    if "tools" not in S:
        (pid, owner) = need("pid", "owner")
        from app.agents.tools import ToolContext, build_tools
        from app.core.config import Settings

        # Explicit settings only: no .env file, no environment beyond what run.sh passed (env -i).
        settings = Settings(_env_file=None, AUTH_SECRET=SECRET, BACKEND_API_URL=RS, SUPERVISOR_MODEL="")
        ctx = ToolContext(
            db=None,
            c=SimpleNamespace(settings=settings),
            project_id=uuid.UUID(pid),
            owner_id=uuid.UUID(owner),
        )
        S["tool_ctx"] = ctx
        S["tools"] = {t.name: t for t in build_tools(ctx)}
    return S["tools"]


def run_tool(name: str, args: dict[str, Any]) -> tuple[Any, str, list[dict[str, Any]]]:
    tools = _tools()
    off = log_offset(RUST_LOG)
    text = tools[name].invoke(args)
    reqs = requests_since(RUST_LOG, off)
    try:
        out = json.loads(text)
    except (TypeError, ValueError):
        out = None
    return out, text, reqs


def tool_error(out: Any) -> dict[str, Any] | None:
    return out if isinstance(out, dict) and out.get("status") == "ERROR" else None


def seen(reqs: list[dict[str, Any]]) -> str:
    return (
        ", ".join(
            f"{x['method']} {x['path'].replace(S.get('pid', '#'), '{pid}')} {x['status']}" for x in reqs
        )
        or "none"
    )


def tool_check(
    name: str, args: dict[str, Any], verify: Callable[[Any], str], skip_404: str | None = None
) -> str:
    out, text, reqs = run_tool(name, args)
    err = tool_error(out)
    if err is not None:
        if skip_404 and err.get("http_status") == 404:
            raise Skip(
                f"{skip_404}; tool error {err.get('error_code')}: {_short(err.get('message'), 120)}; rust saw {seen(reqs)}"
            )
        raise Fail(f"{name} to succeed against rust", f"tool error {text}; rust saw {seen(reqs)}")
    expect(out is not None, f"{name} JSON text", text)
    expect(len(reqs) >= 1, "the call in the rust request log", "no rust request logged")
    return verify(out) + f"; rust saw {seen(reqs)}"


@check("D03", "tool get_project_state over HTTP against rust")
def d03() -> str:
    (pid,) = need("pid")

    def v(out: Any) -> str:
        expect(
            out.get("project", {}).get("project_id") == pid and out.get("business_profile"),
            "project + profile",
            out,
        )
        return f"project {pid}, profile {out['business_profile'].get('confirmation_status')}"

    return tool_check("get_project_state", {}, v)


@check("D04", "tool run_property_analysis over HTTP against rust")
def d04() -> str:
    def v(out: Any) -> str:
        expect(
            len(out.get("property_ids", [])) >= 2 and out.get("ranking_id"),
            ">= 2 property_ids and a ranking",
            out,
        )
        put("D04", search_rerun=True)
        return f"{len(out['property_ids'])} properties, ranking v{out.get('ranking_version')}"

    return tool_check("run_property_analysis", {"city": "Los Angeles", "state": "CA"}, v)


@check("D05", "tool get_ranking over HTTP against rust")
def d05() -> str:
    (tok, pid) = need("rs_token", "pid")

    def v(out: Any) -> str:
        latest = rs("GET", proj(pid, "/rankings"), token=tok).body[-1]["ranking_id"]
        expect(
            out.get("ranking_id") == latest and len(out.get("ranked", [])) == 2,
            f"latest ranking {latest} of 2",
            out,
        )
        return f"ranking {latest}, {len(out['ranked'])} ranked"

    return tool_check("get_ranking", {}, v)


@check("D06", "tool get_feasibility over HTTP against rust")
def d06() -> str:
    (alh,) = need("alhambra")

    def v(out: Any) -> str:
        expect(
            isinstance(out, list) and any(a.get("status") == "UNKNOWN" for a in out),
            "assessments incl. UNKNOWN",
            out,
        )
        return f"{len(out)} assessments, {sum(a['status'] == 'UNKNOWN' for a in out)} UNKNOWN"

    return tool_check("get_feasibility", {"property_id": alh}, v)


@check("D07", "tool get_property_evidence over HTTP against rust")
def d07() -> str:
    (alh,) = need("alhambra")

    def v(out: Any) -> str:
        expect(isinstance(out, list) and len(out) > 0, "a non-empty evidence list", out)
        return f"{len(out)} evidence items, classes {sorted({e.get('class') for e in out})}"

    return tool_check(
        "get_property_evidence",
        {"property_id": alh},
        v,
        skip_404="the rust evidence route answers 404 (not built yet)",
    )


@check("D08", "tool get_cost_scenarios over HTTP against rust")
def d08() -> str:
    def v(out: Any) -> str:
        expect(
            isinstance(out, list) and len(out) == 2 and all(is_int(s.get("long_term")) for s in out),
            "2 integer scenarios",
            out,
        )
        return f"{len(out)} scenarios"

    return tool_check("get_cost_scenarios", {}, v)


@check("D09", "tool validate_evidence over HTTP against rust")
def d09() -> str:
    (alh,) = need("alhambra")

    def v(out: Any) -> str:
        expect(isinstance(out, dict) and isinstance(out.get("problems"), list), '{"problems": [...]}', out)
        return f"{len(out['problems'])} problems"

    return tool_check(
        "validate_evidence",
        {"property_id": alh},
        v,
        skip_404="the rust evidence validation route answers 404 (not built yet)",
    )


@check("D10", "tool request_outreach_approval over HTTP against rust (creates a PENDING approval at most)")
def d10() -> str:
    (alh,) = need("alhambra")

    def v(out: Any) -> str:
        # The HTTP tool answers APPROVAL_REQUIRED for an approval in any status; only this message means PENDING.
        expect(
            out.get("status") == "APPROVAL_REQUIRED"
            and out.get("approval_id")
            and out.get("message") == "Owner approval requested; nothing was sent.",
            "APPROVAL_REQUIRED with a new PENDING approval",
            out,
        )
        return f"approval {out['approval_id']}: {out.get('message')}"

    args = {
        "property_id": alh,
        "recipient_name": BROKER["name"],
        "recipient_phone": BROKER["phone"],
        "questions": "Does each treatment room have a sink?",
    }
    return tool_check(
        "request_outreach_approval",
        args,
        v,
        skip_404="rust has no outreach route yet (not native, not forwarded)",
    )


@check("D11", "chat on a fresh rust project with no ranking: the supervisor runs the search through rust")
def d11() -> str:
    (tok,) = need("rs_token")
    r = rs(
        "POST", "/api/v1/projects", token=tok, body=fixture_body() | {"project_name": f"E2E chat-first {RUN}"}
    )
    status_is(r, 201)
    pid3 = r.body["project"]["project_id"]
    off = log_offset(RUST_LOG)
    c = py("POST", proj(pid3, "/chat"), token=tok, body={"message": "find and rank properties"})
    reqs = requests_since(RUST_LOG, off)
    status_is(c, 200)
    reply = str(c.body.get("reply"))
    expect(reply.startswith("Ranking (deterministic engine):"), "a ranking reply", reply)
    searches = count(reqs, "POST", proj(pid3, "/properties/search"))
    expect(searches == 1, "one POST .../properties/search at rust", seen(reqs))
    ranks = rs("GET", proj(pid3, "/rankings"), token=tok).body
    expect(len(ranks) == 1, "one ranking stored by rust", ranks)
    return f"rust saw {seen(reqs).replace(pid3, '{pid3}')}; reply '{_short(reply, 70)}'"


@check("D12", "read-only tools give the same text in-process (python, shared schema) and over HTTP (rust)")
def d12() -> str:
    """The two tool modes on the same rows: in-process reads the database through Python's repositories, HTTP
    mode reads the Rust routes. The python-link writers compared them only against the Python app itself."""
    (pid, owner, alh, hw) = need("pid", "owner", "alhambra", "hollywood")
    if not DATABASE_URL:
        raise Skip("E2E_DATABASE_URL not set by run.sh")
    from app.agents.tools import ToolContext, build_tools
    from app.core.config import Settings
    from app.core.container import build_container
    from sqlalchemy import create_engine
    from sqlalchemy.orm import Session

    modes = ("PROPERTY", "PERMIT", "MATTERPORT", "VEXCEL", "MESSAGING", "VOICE", "PROCUREMENT")
    settings = Settings(
        _env_file=None,
        AUTH_SECRET=SECRET,
        DATABASE_URL=DATABASE_URL,
        SUPERVISOR_MODEL="",
        BACKEND_API_URL="",
        OBJECT_STORAGE_LOCAL_DIR=str(Path(os.environ.get("HOME", "/nonexistent")) / "objects-driver"),
        **{f"{m}_PROVIDER_MODE": "fixture" for m in modes},
    )
    engine = create_engine(DATABASE_URL, pool_pre_ping=True)
    same, key_order, differ, not_comparable = [], [], [], []
    try:
        with Session(engine) as db:
            ctx = ToolContext(
                db=db, c=build_container(settings), project_id=uuid.UUID(pid), owner_id=uuid.UUID(owner)
            )
            local = {t.name: t for t in build_tools(ctx)}
            remote = _tools()
            calls = [("get_project_state", {}), ("get_ranking", {}), ("get_cost_scenarios", {})]
            for label, p in (("alhambra", alh), ("hollywood", hw)):
                calls += [
                    (n, {"property_id": p}, label)
                    for n in ("get_feasibility", "get_property_evidence", "validate_evidence")
                ]
            for name, args, *label in calls:
                tag = f"{name}({label[0]})" if label else name
                a, b = local[name].invoke(args), remote[name].invoke(args)
                err = tool_error(json.loads(b))
                if err is not None:
                    not_comparable.append(f"{tag}: http {err.get('error_code')}, in-process {len(a)} chars")
                elif a == b:
                    same.append(tag)
                elif key_order_only(b, a):
                    key_order.append(tag)
                else:
                    differ.append(f"{tag}: {explain_text_diff(b, a)}")
            db.rollback()
    finally:
        engine.dispose()
    note = f"; not comparable (HTTP mode failed): {not_comparable}" if not_comparable else ""
    if differ:
        raise Fail(
            "the same JSON value in both modes (object key order aside)",
            " | ".join(differ) + f" || bytes identical: {same}; key order only: {key_order}{note}",
        )
    ko = f"; bytes differ (object key order): {', '.join(key_order)}" if key_order else ""
    return f"the same JSON value in both modes; bytes identical: {', '.join(same) or 'none'}{ko}{note}"


def key_order_only(a: str, b: str) -> bool:
    """True when two tool texts are the same JSON value and differ only in the order of object keys: each text is
    exactly what json.dumps prints for its own value (so spacing and number spelling are the same), and the values
    are equal with types compared (strict_diff)."""
    try:
        ja, jb = json.loads(a), json.loads(b)
    except ValueError:
        return False
    return json.dumps(ja) == a and json.dumps(jb) == b and not strict_diff(ja, jb)


@check(
    "D13", "after the second analysis (D04): feasibility routes of both services vs the embedded assessments"
)
def d13() -> str:
    (tok, pid, alh, hw) = need("rs_token", "pid", "alhambra", "hollywood")
    need("search_rerun")
    props = {p["property_id"]: p for p in rs("GET", proj(pid, "/properties"), token=tok).body}
    out, problems = [], []
    for label, p in (("alhambra", alh), ("hollywood", hw)):
        a = rs("GET", proj(pid, f"/properties/{p}/feasibility"), token=tok)
        b = py("GET", proj(pid, f"/properties/{p}/feasibility"), token=tok)
        embedded = props[p].get("infrastructure_assessments", [])
        if strict_diff(a.body, b.body):
            problems.append(
                f"{label}: rust and python feasibility routes differ: {explain_text_diff(a.text, b.text)}"
            )
        ids = lambda xs: [x.get("assessment_id") for x in xs]  # noqa: E731
        if sorted(map(_canon, a.body)) != sorted(map(_canon, embedded)):
            problems.append(f"{label}: route and embedded assessments are different sets")
        elif ids(a.body) != ids(embedded):
            # Recorded, not failed here: both services order by a non-unique created_at (Python behaviour, kept
            # for parity); D12 already fails on the tool-text difference this causes.
            out.append(
                f"{label}: routes agree; same set as the property record but in another order: route "
                f"{[x.get('requirement_id') for x in a.body]} vs embedded {[x.get('requirement_id') for x in embedded]}"
            )
        else:
            out.append(f"{label}: routes agree, same order as the property record")
    if problems:
        raise Fail(
            "rust and python feasibility routes to agree, as a set equal to the property record",
            " | ".join(problems),
        )
    return " | ".join(out)


# =================================================================================================================
# E. Rust's owner notify path as Python sees it
# =================================================================================================================
def notify(key: str, text: str, token: str | None = None) -> R:
    (owner, pid) = need("owner", "pid")
    svc = token or issue_token(SECRET, uuid.UUID(owner), ttl_s=300, purpose=SERVICE_PURPOSE)
    body = {
        "owner_id": owner,
        "project_id": pid,
        "text": text,
        "purpose": "e2e_check",
        "idempotency_key": key,
    }
    return py("POST", "/internal/v1/owner/notify", token=svc, body=body)


def e2e_comms() -> list[dict[str, Any]]:
    (tok, pid) = need("rs_token", "pid")
    r = py("GET", proj(pid, "/communications"), token=tok)
    status_is(r, 200)
    return [c for c in r.body if c.get("purpose") == "e2e_check"]


@check("E01", "notify refuses an access token; with a service token before the channel is linked: not_linked")
def e01() -> str:
    key = f"e2e-{RUN}-unlinked"
    (tok,) = need("rs_token")  # B05 covers the other direction: a service token on an ordinary route
    bad = notify(f"e2e-{RUN}-access", "must never be sent", token=tok)
    expect(bad.status == 401, "401 for an ordinary access token on /internal", bad.show())
    r = notify(key, "E2E check: you should not receive this (no channel linked).")
    status_is(r, 200)
    expect(r.body.get("delivery") == "not_linked", "delivery not_linked", r.body)
    put("E01", key_unlinked=key, comm_unlinked=r.body["communication_id"])
    return f"access token refused (401); 200 not_linked, communication {r.body['communication_id']}"


@check("E02", "link the owner's channel through the python owner-channel routes (fixture Photon)")
def e02() -> str:
    (tok,) = need("rs_token")
    r = py("PUT", "/api/v1/owner/channel", token=tok, body={"handle": OWNER_HANDLE})
    status_is(r, 200)
    first = (r.body.get("channel") or {}).get("status")
    s = py("POST", "/api/v1/owner/channel/simulate-text", token=tok, body={"text": "hi"})
    status_is(s, 200)
    expect(s.body.get("routed") is True, "routed true", s.body)
    g = py("GET", "/api/v1/owner/channel", token=tok)
    status = (g.body.get("channel") or {}).get("status")
    expect(status == "LINKED", "channel LINKED", g.show())
    # The web UI's polling position on the rust origin just before the next notify (used by E06 and E07).
    cur = rs("GET", proj(need("pid")[0], "/events?limit=500"), token=tok)
    status_is(cur, 200)
    put("E02", linked=True, cursor_before_notify=cur.body.get("next_cursor"))
    return f"PUT -> {first}; simulate-text 'hi' routed; GET -> LINKED; rust next_cursor {cur.body.get('next_cursor')}"


@check("E03", "notify after linking: recorded (fixture provider)")
def e03() -> str:
    need("linked")
    key = f"e2e-{RUN}-linked"
    before = len(e2e_comms())
    r = notify(key, "E2E check: your call summary is ready in the app.")
    status_is(r, 200)
    expect(r.body.get("delivery") == "recorded", "delivery recorded", r.body)
    after = len(e2e_comms())
    expect(after == before + 1, f"{before + 1} e2e communications", after)
    put("E03", key_linked=key, notify_answer=r.body)
    return f"200 recorded, communication {r.body['communication_id']}"


@check("E04", "the same key again answers the same and sends nothing more")
def e04() -> str:
    (key, first) = need("key_linked", "notify_answer")
    (tok, pid) = need("rs_token", "pid")
    before = len(e2e_comms())
    r = notify(key, "a different text with the same key")
    status_is(r, 200)
    expect(r.body == first, f"the earlier answer {first}", r.body)
    expect(len(e2e_comms()) == before, f"still {before} e2e communications", len(e2e_comms()))
    ev = py("GET", proj(pid, "/events?limit=500"), token=tok).body.get("events", [])
    n = sum(1 for e in ev if e.get("idempotency_key") == f"notify:{key}")
    expect(n == 1, "exactly one notify event for the key", n)
    return "same answer; no new communication; one event for the key"


@check("E05", "the notify events are visible through the rust origin")
def e05() -> str:
    (tok, pid, k1, k2, first) = need("rs_token", "pid", "key_unlinked", "key_linked", "notify_answer")
    r = rs("GET", proj(pid, "/events?limit=500"), token=tok)
    status_is(r, 200)
    ev = {e.get("idempotency_key"): e for e in r.body.get("events", [])}
    e2 = ev.get(f"notify:{k2}")
    expect(
        e2 is not None and e2.get("event_type") == "communication.owner_notified",
        "the notify event",
        sorted(ev)[-5:],
    )
    expect(
        e2["payload"].get("communication_id") == first["communication_id"]
        and e2["payload"].get("delivery") == "recorded",
        "payload communication_id/delivery of the answer",
        e2["payload"],
    )
    expect(f"notify:{k1}" in ev, "the not_linked notify event too", sorted(ev)[-5:])
    return "both notify events (recorded, not_linked) served by rust with the payload python wrote"


@check("E06", "rust's event cursor (GET /events?after=) delivers the python-written notify event")
def e06() -> str:
    (tok, pid, cursor, k2) = need("rs_token", "pid", "cursor_before_notify", "key_linked")
    r = rs("GET", proj(pid, f"/events?after={cursor}&limit=500"), token=tok)
    status_is(r, 200)
    keys = [e.get("idempotency_key") for e in r.body.get("events", [])]
    expect(f"notify:{k2}" in keys, f"notify:{k2} after cursor {cursor}", keys)
    expect(r.body.get("next_cursor", 0) > cursor, f"next_cursor beyond {cursor}", r.body.get("next_cursor"))
    return f"after={cursor}: {len(keys)} event(s) {keys}; next_cursor {r.body.get('next_cursor')}"


@check("E07", "rust's event stream (SSE /events/stream) delivers the python-written notify event")
def e07() -> str:
    (tok, pid, cursor) = need("rs_token", "pid", "cursor_before_notify")
    seen_types: list[str] = []
    deadline = time.monotonic() + 8
    with httpx.Client(base_url=RS, trust_env=False) as c:
        with c.stream(
            "GET",
            proj(pid, f"/events/stream?after={cursor}"),
            headers={"Authorization": f"Bearer {tok}", "Accept": "text/event-stream"},
            timeout=httpx.Timeout(10.0),
        ) as resp:
            expect(resp.status_code == 200, "HTTP 200 text/event-stream", resp.status_code)
            ctype = resp.headers.get("content-type", "")
            for line in resp.iter_lines():
                if line.startswith("event: "):
                    seen_types.append(line[7:])
                    if line[7:] == "communication.owner_notified":
                        break
                if time.monotonic() > deadline:
                    break
    expect("communication.owner_notified" in seen_types, "event: communication.owner_notified", seen_types)
    return f"{ctype}; events after {cursor}: {seen_types}"


# =================================================================================================================
# F. Through the forwarder
# =================================================================================================================
def forwarder() -> None:
    """SKIP unless the Rust origin forwards the Python-owned paths."""
    if "forwarder" not in S:
        (tok,) = need("rs_token")
        r = rs("GET", "/api/v1/owner/channel", token=tok)
        S["forwarder"] = r.status != 404
        S["forwarder_probe"] = r.show(200)
    if not S["forwarder"]:
        raise Skip(f"forwarder not built yet: {S['forwarder_probe']}")


def via_rust(method: str, path: str, py_status: int | None = None, **kw: Any) -> R:
    """Send through the rust origin with a fresh x-request-id. FAIL unless rust logged that request and python
    logged the forwarded one with the same id (so the check cannot pass with python alone), and, when given,
    python itself answered py_status."""
    rid = f"e2e-fwd-{uuid.uuid4().hex[:12]}"
    off_rs, off_py = log_offset(RUST_LOG), log_offset(PY_LOG)
    r = rs(method, path, headers={"x-request-id": rid} | kw.pop("headers", {}), **kw)
    at = {
        side: [x for x in requests_since(log, off) if x.get("request_id") == rid and x.get("path") == path]
        for side, log, off in (("rust", RUST_LOG, off_rs), ("python", PY_LOG, off_py))
    }
    expect(
        len(at["rust"]) == 1 and len(at["python"]) == 1,
        f"{method} {path} once in rust's request log and once, forwarded with request id {rid}, in python's",
        at,
    )
    if py_status is not None:
        expect(
            at["python"][0].get("status") == py_status,
            f"python answered the forwarded request {py_status}",
            at,
        )
    return r


@check("F01", "chat through the rust origin gives the python answer")
def f01() -> str:
    forwarder()
    (tok, pid) = need("rs_token", "pid")
    body = {"message": "what's the status?"}
    a, b = (
        via_rust("POST", proj(pid, "/chat"), token=tok, body=body),
        py("POST", proj(pid, "/chat"), token=tok, body=body),
    )
    expect(a.status == 200 and a.body == b.body, "the python answer", f"{a.show(150)} | {b.show(150)}")
    return "identical"


@check("F02", "GET /api/v1/owner/channel through the rust origin")
def f02() -> str:
    forwarder()
    (tok,) = need("rs_token")
    a, b = via_rust("GET", "/api/v1/owner/channel", token=tok), py("GET", "/api/v1/owner/channel", token=tok)
    expect(
        a.status == b.status == 200 and a.body == b.body,
        "the python answer",
        f"{a.show(150)} | {b.show(150)}",
    )
    return "identical"


@check("F03", "PUT /api/v1/owner/channel through the rust origin")
def f03() -> str:
    forwarder()
    (tok,) = need("rs_token")
    body = {"handle": OWNER_HANDLE}
    a, b = (
        via_rust("PUT", "/api/v1/owner/channel", token=tok, body=body),
        py("PUT", "/api/v1/owner/channel", token=tok, body=body),
    )
    expect(
        a.status == b.status == 200 and a.body == b.body,
        "the python answer",
        f"{a.show(150)} | {b.show(150)}",
    )
    return "identical (same handle: the current channel)"


@check("F04", "POST /api/v1/owner/channel/simulate-text through the rust origin")
def f04() -> str:
    forwarder()
    (tok,) = need("rs_token")
    body = {"text": "status"}
    a = via_rust("POST", "/api/v1/owner/channel/simulate-text", token=tok, body=body)
    b = py("POST", "/api/v1/owner/channel/simulate-text", token=tok, body=body)
    expect(
        a.status == b.status == 200 and a.body == b.body,
        "the python answer",
        f"{a.show(150)} | {b.show(150)}",
    )
    return f"identical: {_short(a.body, 100)}"


def photon_headers(body: bytes, msg_id: str) -> dict[str, str]:
    ts = int(time.time())
    return {
        "webhook-id": msg_id,
        "webhook-timestamp": str(ts),
        "webhook-signature": standard_webhooks_sign(PHOTON_SECRET, msg_id, ts, body),
        "content-type": "application/json",
    }


@check("F05", "a signed POST /webhooks/photon through the rust origin is accepted")
def f05() -> str:
    forwarder()
    body = PHOTON_INBOUND.read_bytes()
    h = photon_headers(body, f"evt_e2e_{RUN}")
    a = via_rust("POST", "/webhooks/photon", py_status=204, content=body, headers=h)
    expect(a.status == 204, "204", a.show())
    return "204; python verified the signature on the forwarded raw body"


@check("F06", "a POST /webhooks/photon with a broken signature through the rust origin is refused")
def f06() -> str:
    forwarder()
    body = PHOTON_INBOUND.read_bytes()
    h = photon_headers(body, f"evt_e2e_bad_{RUN}")
    h["webhook-signature"] = "v1,AAAA" + h["webhook-signature"][7:]
    a = via_rust("POST", "/webhooks/photon", py_status=400, content=body, headers=h)
    expect(
        a.status == 400 and (a.body or {}).get("error_code") == "INVALID_SIGNATURE",
        "400 INVALID_SIGNATURE",
        a.show(),
    )
    return "400 INVALID_SIGNATURE (python refused the forwarded raw body)"


@check("F07", "/internal/v1/owner/notify through the rust origin: rust 404, python sees nothing")
def f07() -> str:
    # Runs with or without the forwarder: its expectation does not depend on it.
    (owner, pid) = need("owner", "pid")
    svc = issue_token(SECRET, uuid.UUID(owner), ttl_s=60, purpose=SERVICE_PURPOSE)
    body = {
        "owner_id": owner,
        "project_id": pid,
        "text": "must never be sent",
        "purpose": "e2e_check",
        "idempotency_key": f"e2e-{RUN}-via-rust",
    }
    off = log_offset(PY_LOG)
    a = rs("POST", "/internal/v1/owner/notify", token=svc, body=body)
    b = rs("GET", "/internal/v1/owner/notify", token=svc)
    c = rs("POST", "/internal/v1/owner/notify/", token=svc, body=body)
    reqs = [x for x in requests_since(PY_LOG, off) if "/internal" in str(x.get("path"))]
    for x in (a, b, c):
        expect(x.status == 404, "404 from rust", x.show())
    expect(not reqs, "no /internal request at python", reqs)
    return f"404 x3 from rust ({_short(a.text, 100)}); 0 /internal requests at python"


# =================================================================================================================
# G. Outreach and calls as far as they exist
# =================================================================================================================
def outreach_body(pid_prop: str) -> dict[str, Any]:
    return {
        "property_id": pid_prop,
        "channel": "VOICE",
        "recipient": BROKER,
        "purpose": "E2E: confirm the sinks per treatment room",
        "questions": ["Does each treatment room have a sink?"],
    }


def check_outreach_answer(cid: str, r: R, pid: str, owner: str) -> str:
    status_is(r, 202)
    comm, appr = r.body.get("communication", {}), r.body.get("approval", {})
    expect(
        comm.get("state") == "AWAITING_APPROVAL",
        "communication AWAITING_APPROVAL (a draft)",
        comm.get("state"),
    )
    expect(
        appr.get("approval_status") == "PENDING"
        and appr.get("action_type") == "PLACE_CALL"
        and appr.get("project_id") == pid
        and appr.get("authorized_owner_id") == owner,
        "a PENDING PLACE_CALL approval of this project and owner",
        appr,
    )
    put(cid, approval=appr, communication=comm)
    return f"draft {comm.get('communication_id')} + PENDING approval {appr.get('approval_id')}"


@check(
    "G01",
    "POST .../outreach through the rust origin: a draft, a PENDING approval, the owner notified via python",
)
def g01() -> str:
    (tok, pid, owner, alh) = need("rs_token", "pid", "owner", "alhambra")
    off_py, off_rs = log_offset(PY_LOG), log_offset(RUST_LOG)
    r = rs("POST", proj(pid, "/outreach"), token=tok, body=outreach_body(alh))
    if r.status == 404:
        S["g01_absent"] = r.show(200)
        raise Skip(f"rust has no outreach route (not native, not forwarded): {r.show(200)}")
    out = check_outreach_answer("G01", r, pid, owner)
    # Rust calls python's owner notify after it has answered (after the commit), so wait for its log line.
    key = f"approval-notify:{r.body['approval']['approval_id']}"
    delivery: list[Any] = []
    for _ in range(50):  # until the linked owner's text is recorded (a not_linked answer may come first)
        notes = [
            ln for ln in RUST_LOG.read_bytes()[off_rs:].decode(errors="replace").splitlines() if key in ln
        ]
        delivery = [json.loads(ln).get("delivery") for ln in notes if ln.startswith("{")]
        if "recorded" in delivery:
            break
        time.sleep(0.1)
    py_reqs = requests_since(PY_LOG, off_py)
    how = "forwarded to python" if count(py_reqs, "POST", proj(pid, "/outreach")) else "native in rust"
    calls = [x for x in py_reqs if x.get("method") == "POST" and x.get("path") == "/internal/v1/owner/notify"]
    expect(
        calls and all(x.get("status") == 200 for x in calls),
        "rust's notifier at python's /internal notify, answered 200 (python accepted rust's service token)",
        calls,
    )
    expect("recorded" in delivery, "an owner text recorded for the approval (linked since E02)", delivery)
    return f"{out}; {how}; rust called python's owner notify {len(calls)}x (delivery {delivery})"


@check("G02", "fallback: the same outreach on the python port, so rust's approval routes can be exercised")
def g02() -> str:
    if "g01_absent" not in S:
        raise (NotNeeded if "approval" in S else Skip)(
            "not needed: G01 created the approval through the rust origin"
            if "approval" in S
            else "G01 did not pass"
        )
    (tok, pid, owner, alh) = need("rs_token", "pid", "owner", "alhambra")
    r = py("POST", proj(pid, "/outreach"), token=tok, body=outreach_body(alh))
    return check_outreach_answer("G02", r, pid, owner) + " (python-written)"


@check("G03", "the approval appears in the rust approvals list; no call while it is PENDING")
def g03() -> str:
    (tok, pid, appr) = need("rs_token", "pid", "approval")
    r = rs("GET", proj(pid, "/approvals"), token=tok)
    status_is(r, 200)
    mine = next((a for a in r.body if a.get("approval_id") == appr["approval_id"]), None)
    expect(mine is not None, f"approval {appr['approval_id']}", [a.get("approval_id") for a in r.body])
    diffs = strict_diff(mine, appr)
    expect(not diffs, "the same approval document", diffs[:5])
    # Payload parity: python's execution boundary (require_approved) recomputes payload_hash(call_payload(...)).
    from app.contracts.approvals import payload_hash
    from app.services.call_orchestration import call_payload

    (alh,) = need("alhambra")
    canonical = call_payload(uuid.UUID(pid), BROKER, uuid.UUID(alh))
    expect(mine.get("action_payload") == canonical, f"action_payload {canonical}", mine.get("action_payload"))
    expect(
        payload_hash(canonical) == mine.get("action_payload_hash"),
        f"python payload_hash {payload_hash(canonical)[:16]}...",
        mine.get("action_payload_hash"),
    )
    # The gate itself: no call on a PENDING approval, through either service (a route that is absent is G05's SKIP).
    (comm,) = need("communication")
    for side, call in (("rust", rs), ("python", py)):
        early = call(
            "POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]}
        )
        expect(
            early.status in (404, 409)
            and (early.status == 404 or early.body.get("error_code") == "APPROVAL_REQUIRED"),
            f"409 APPROVAL_REQUIRED from {side} for a call while the approval is PENDING",
            early.show(),
        )
    return (
        f"listed, PENDING, identical document ({len(r.body)} approval(s) in the project); action_payload is "
        f"python's call_payload and its hash equals python's payload_hash; a call while PENDING is refused by both"
    )


@check("G04", "approve it through the rust approval route; python sees it APPROVED")
def g04() -> str:
    (tok, pid, appr) = need("rs_token", "pid", "approval")
    r = rs(
        "POST",
        f"/api/v1/approvals/{appr['approval_id']}/decision",
        token=tok,
        body={"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]},
    )
    status_is(r, 200)
    expect(r.body.get("approval_status") == "APPROVED", "APPROVED", r.body)
    p = py("GET", proj(pid, "/approvals"), token=tok)
    seen_py = next((a for a in p.body if a.get("approval_id") == appr["approval_id"]), {})
    expect(seen_py.get("approval_status") == "APPROVED", "python reads APPROVED", seen_py)
    put("G04", approved=True)
    return "200 APPROVED (rust); python reads APPROVED"


@check("G05", "POST .../calls through the rust origin")
def g05() -> str:
    (tok, pid, comm) = need("rs_token", "pid", "communication")
    need("approved")
    r = rs("POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]})
    if r.status == 404:
        raise Skip(f"rust has no calls route (not native, not forwarded): {r.show(200)}")
    status_is(r, 202)
    expect(r.body.get("simulated") is True, "a labelled simulation (fixture telephony)", r.body)
    appr = next(
        (
            a
            for a in rs("GET", proj(pid, "/approvals"), token=tok).body
            if a["approval_id"] == comm["approval_id"]
        ),
        {},
    )
    put("G05", call_via_rust=r.body)
    return (
        f"202 call {r.body.get('call_id')} {r.body.get('state')}, simulated (fixture telephony); "
        f"approval now {appr.get('approval_status')}. Stopped here: nothing else is driven on the call."
    )


@check("G06", "POST .../calls on the python port after that: single use holds across both services")
def g06() -> str:
    """If rust executed the call (G05), python must refuse a second execution of the same approval. If rust has
    no calls route, record what python's route does with the rust-decided approval (fixture telephony)."""
    (tok, pid, comm, appr) = need("rs_token", "pid", "communication", "approval")
    need("approved")
    r = py("POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]})
    if "call_via_rust" in S:
        expect(
            r.status == 409 and (r.body or {}).get("error_code") == "APPROVAL_REQUIRED",
            "409 APPROVAL_REQUIRED (the approval was executed by rust)",
            r.show(),
        )
        again = rs(
            "POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]}
        )
        expect(
            again.status == 409 and (again.body or {}).get("error_code") == "APPROVAL_REQUIRED",
            "409 APPROVAL_REQUIRED from rust too for a second execution",
            again.show(),
        )
        return f"python and rust refuse the second execution: 409 {r.body.get('message')}"
    status_is(r, 202)
    expect(r.body.get("simulated") is True, "simulated true (fixture telephony, no carrier)", r.body)
    after = next(
        (
            a
            for a in rs("GET", proj(pid, "/approvals"), token=tok).body
            if a["approval_id"] == appr["approval_id"]
        ),
        {},
    )
    return (
        f"202 call {r.body.get('call_id')} state {r.body.get('state')}, simulated; "
        f"approval now {after.get('approval_status')} (read through rust). Stopped here."
    )


# =================================================================================================================
# Shared by J and I: request ids, both request logs, and a digest of every table of the private schema
# =================================================================================================================
def _engine() -> Any:
    if "engine" not in S:
        if not DATABASE_URL:
            raise Skip("E2E_DATABASE_URL not set by run.sh")
        from sqlalchemy import create_engine

        S["engine"] = create_engine(DATABASE_URL, pool_pre_ping=True)
    return S["engine"]


def db_digest() -> dict[str, tuple[int, str]]:
    """(row count, md5 of every row's text) for each table of the run's schema: any insert, update or delete
    by either service changes it."""
    from sqlalchemy import text

    out = {}
    with _engine().connect() as c:
        tables = c.execute(text("SELECT tablename FROM pg_tables WHERE schemaname = current_schema()")).all()
        for (t,) in tables:
            row = c.execute(
                text(
                    f"SELECT count(*), md5(coalesce(string_agg(x::text, ',' ORDER BY x::text), '')) FROM \"{t}\" x"
                )
            ).one()
            out[t] = (row[0], row[1])
    return out


def quiet_digest(limit_s: float = 8.0) -> dict[str, tuple[int, str]]:
    """db_digest once nothing has changed for 0.5 s: owner texts the services send after answering (rust's notifier
    calls python after its commit) must not be taken for a write of the next request."""
    last, t0 = db_digest(), time.monotonic()
    while time.monotonic() - t0 < limit_s:
        time.sleep(0.5)
        now = db_digest()
        if now == last:
            return now
        last = now
    return last


def digest_diff(a: dict[str, tuple[int, str]], b: dict[str, tuple[int, str]]) -> list[str]:
    return [
        f"{t}: {a.get(t, (0, ''))[0]} -> {b.get(t, (0, ''))[0]} rows{'' if a.get(t, (0,))[0] != b.get(t, (0,))[0] else ' (changed)'}"
        for t in sorted(set(a) | set(b))
        if a.get(t) != b.get(t)
    ]


def rid_for(cid: str) -> str:
    return f"e2e-{cid}-{uuid.uuid4().hex[:10]}"


def logged(path: Path, off: int, rid: str) -> list[dict[str, Any]]:
    return [x for x in requests_since(path, off) if x.get("request_id") == rid]


def tagged(side: str, cid: str, method: str, path: str, **kw: Any) -> tuple[R, list[dict], list[dict]]:
    """One request with a fresh x-request-id; returns the answer and the lines with that id in rust's and
    python's request logs."""
    rid = rid_for(cid)
    off_rs, off_py = log_offset(RUST_LOG), log_offset(PY_LOG)
    r = http(RS if side == "rust" else PY, method, path, headers={"x-request-id": rid}, **kw)
    time.sleep(0.05)  # the access log line is written as the answer is sent
    return r, logged(RUST_LOG, off_rs, rid), logged(PY_LOG, off_py, rid)


def one_approval(tok: str, pid: str, approval_id: str, side: str = "rust") -> dict[str, Any]:
    r = http(RS if side == "rust" else PY, "GET", proj(pid, "/approvals"), token=tok)
    status_is(r, 200)
    a = next((x for x in r.body if x.get("approval_id") == approval_id), None)
    expect(
        a is not None, f"approval {approval_id} in the {side} list", [x.get("approval_id") for x in r.body]
    )
    return a


def both_read_same(tok: str, pid: str, approval_id: str) -> dict[str, Any]:
    a, b = one_approval(tok, pid, approval_id, "rust"), one_approval(tok, pid, approval_id, "python")
    d = strict_diff(a, b)
    expect(not d, "the same approval document from rust and python", d[:4])
    return a


def new_outreach(cid: str, name: str, phone: str) -> dict[str, Any]:
    """A fresh outreach in the first project through rust: its PENDING approval and draft communication."""
    (tok, pid, alh) = need("rs_token", "pid", "alhambra")
    body = outreach_body(alh) | {"recipient": {"name": name, "phone": phone}}
    r = rs("POST", proj(pid, "/outreach"), token=tok, body=body)
    status_is(r, 202)
    expect(r.body["approval"].get("approval_status") == "PENDING", "a PENDING approval", r.body["approval"])
    return r.body


def decide(side: str, approval: dict[str, Any], decision: str, hash_: str | None = None, cid: str = "J") -> R:
    (tok,) = need("rs_token")
    body = {"decision": decision, "action_payload_hash": hash_ or approval["action_payload_hash"]}
    r, _, _ = tagged(
        side, cid, "POST", f"/api/v1/approvals/{approval['approval_id']}/decision", token=tok, body=body
    )
    return r


def expire_in_db(approval_id: str) -> str:
    """Move an approval's expires_at an hour into the past, as the services' own tests do (they rewrite the stored
    document instead of waiting 24 h). The only write the driver makes to the shared schema."""
    from sqlalchemy import text

    past = time.strftime("%Y-%m-%dT%H:%M:%S.000000Z", time.gmtime(time.time() - 3600))
    with _engine().begin() as c:
        n = c.execute(
            text(
                "UPDATE approvals SET data = jsonb_set(data::jsonb, '{expires_at}', to_jsonb(CAST(:t AS text))), "
                "record_version = record_version + 1, updated_at = now() WHERE id = CAST(:id AS uuid)"
            ),
            {"t": past, "id": approval_id},
        ).rowcount
    expect(n == 1, "one approval row updated", n)
    return past


def calls_of(pid: str) -> list[str]:
    (tok,) = need("rs_token")
    r = rs("GET", proj(pid, "/calls"), token=tok)
    status_is(r, 200)
    return [c["call_id"] for c in r.body]


# =================================================================================================================
# J. The approval decision (through rust; python reads every state identically)
# =================================================================================================================
@check("J01", "a decision with a wrong action_payload_hash is refused by rust and changes nothing")
def j01() -> str:
    (tok, pid) = need("rs_token", "pid")
    o = new_outreach("J01", "E2E Hash Broker", "+15555550161")
    appr = o["approval"]
    before, rows = both_read_same(tok, pid, appr["approval_id"]), quiet_digest()
    r = decide("rust", appr, "APPROVED", "0" * 64, "J01")
    expect(
        r.status == 409 and (r.body or {}).get("error_code") == "APPROVAL_PAYLOAD_MISMATCH",
        "409 APPROVAL_PAYLOAD_MISMATCH",
        r.show(),
    )
    after = both_read_same(tok, pid, appr["approval_id"])
    expect(
        not strict_diff(before, after),
        "the approval unchanged (still PENDING, undecided)",
        strict_diff(before, after),
    )
    changed = digest_diff(rows, db_digest())
    expect(not changed, "no row written or changed anywhere in the schema", changed)
    put("J01", j_pending=appr)
    return "409 APPROVAL_PAYLOAD_MISMATCH; approval still PENDING (rust and python read the same document); no row changed"


@check("J02", "REJECTED: rust and python read REJECTED, a call through rust is refused, no later decision")
def j02() -> str:
    (tok, pid, appr) = need("rs_token", "pid", "j_pending")
    r = decide("rust", appr, "REJECTED", cid="J02")
    status_is(r, 200)
    expect(r.body.get("approval_status") == "REJECTED", "REJECTED", r.body)
    seen = both_read_same(tok, pid, appr["approval_id"])
    expect(seen.get("approval_status") == "REJECTED", "both read REJECTED", seen)
    calls, rows = calls_of(pid), quiet_digest()
    comm = next(
        c
        for c in rs("GET", proj(pid, "/communications"), token=tok).body
        if c.get("approval_id") == appr["approval_id"]
    )
    c, _, _ = tagged(
        "rust",
        "J02",
        "POST",
        proj(pid, "/calls"),
        token=tok,
        body={"communication_id": comm["communication_id"]},
    )
    expect(
        c.status == 409 and (c.body or {}).get("error_code") == "APPROVAL_REQUIRED",
        "409 APPROVAL_REQUIRED for a call on a REJECTED approval",
        c.show(),
    )
    again = decide("rust", appr, "APPROVED", cid="J02")
    expect(
        again.status == 409 and (again.body or {}).get("error_code") == "INVALID_STATE_TRANSITION",
        "409 INVALID_STATE_TRANSITION for APPROVED after REJECTED",
        again.show(),
    )
    expect(calls_of(pid) == calls, "no call created", "a call was created")
    changed = digest_diff(rows, db_digest())
    expect(not changed, "no row written by the refused call or decision", changed)
    p = py("POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]})
    return (
        f"200 REJECTED, read identically by both; call refused 409 '{c.body.get('message')}'; APPROVED afterwards "
        f"refused 409 INVALID_STATE_TRANSITION; nothing written (python's call route: {p.status} {(p.body or {}).get('error_code')})"
    )


@check("J03", "an expired PENDING approval cannot be decided (rust), and python reads it identically")
def j03() -> str:
    (tok, pid) = need("rs_token", "pid")
    appr = new_outreach("J03", "E2E Expiry Broker", "+15555550162")["approval"]
    past = expire_in_db(appr["approval_id"])
    before, rows = both_read_same(tok, pid, appr["approval_id"]), quiet_digest()
    r = decide("rust", appr, "APPROVED", cid="J03")
    expect(
        r.status == 409
        and (r.body or {}).get("error_code") == "INVALID_STATE_TRANSITION"
        and ((r.body or {}).get("details") or {}).get("current") == "EXPIRED",
        "409 INVALID_STATE_TRANSITION, current EXPIRED",
        r.show(),
    )
    after = both_read_same(tok, pid, appr["approval_id"])
    expect(not strict_diff(before, after), "the approval unchanged", strict_diff(before, after))
    changed = digest_diff(rows, db_digest())
    expect(not changed, "no row changed", changed)
    p = decide("python", appr, "APPROVED", cid="J03")
    expect(p.status == 409, "python refuses the decision too (409)", p.show())
    return (
        f"expires_at set to {past} in the stored document (as the services' tests do); rust 409 "
        f"INVALID_STATE_TRANSITION (current EXPIRED), python 409 {(p.body or {}).get('error_code')}; both read status "
        f"{after.get('approval_status')} (the refused request rolls back its EXPIRED write); nothing changed"
    )


@check(
    "J04",
    "an approval that expired after APPROVED cannot be used for a call (rust), python reads it identically",
)
def j04() -> str:
    (tok, pid) = need("rs_token", "pid")
    o = new_outreach("J04", "E2E Late Broker", "+15555550163")
    appr, comm = o["approval"], o["communication"]
    ok = decide("rust", appr, "APPROVED", cid="J04")
    status_is(ok, 200)
    past = expire_in_db(appr["approval_id"])
    before, calls, rows = both_read_same(tok, pid, appr["approval_id"]), calls_of(pid), quiet_digest()
    c, _, _ = tagged(
        "rust",
        "J04",
        "POST",
        proj(pid, "/calls"),
        token=tok,
        body={"communication_id": comm["communication_id"]},
    )
    expect(
        c.status == 409
        and (c.body or {}).get("error_code") == "APPROVAL_REQUIRED"
        and "EXPIRED" in str(c.body),
        "409 APPROVAL_REQUIRED, approval EXPIRED",
        c.show(),
    )
    expect(calls_of(pid) == calls, "no call created", "a call was created")
    after = both_read_same(tok, pid, appr["approval_id"])
    expect(not strict_diff(before, after), "the approval unchanged", strict_diff(before, after))
    changed = digest_diff(rows, db_digest())
    expect(not changed, "no row changed", changed)
    p = py("POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]})
    expect(p.status == 409, "python refuses the call too (409)", p.show())
    return (
        f"APPROVED, then expires_at {past}; rust call 409 '{c.body.get('message')}', python 409; no call, no row "
        f"changed; both read {after.get('approval_status')} with the past expires_at"
    )


# =================================================================================================================
# I. Cross-project isolation (review R23): a record of project B under project A's URL, and another owner
# =================================================================================================================
@check(
    "I01",
    "set-up: a second owner with a project of its own (search, outreach, a simulated call with a handoff)",
)
def i01() -> str:
    owner2 = str(uuid.uuid4())
    tok2 = issue_token(SECRET, uuid.UUID(owner2))
    r = rs(
        "POST",
        "/api/v1/projects",
        token=tok2,
        body=fixture_body() | {"project_name": f"E2E other owner {RUN}"},
    )
    status_is(r, 201)
    pb = r.body["project"]["project_id"]
    s = rs("POST", proj(pb, "/properties/search"), token=tok2, body={"city": "Los Angeles", "state": "CA"})
    status_is(s, 202)
    props = {
        p["property_identity"]["street_address"]: p["property_id"]
        for p in rs("GET", proj(pb, "/properties"), token=tok2).body
    }
    alb = props[ALHAMBRA]
    o = rs(
        "POST",
        proj(pb, "/outreach"),
        token=tok2,
        body=outreach_body(alb) | {"recipient": {"name": "E2E B Broker", "phone": "+15555550171"}},
    )
    status_is(o, 202)
    o2 = rs(
        "POST",
        proj(pb, "/outreach"),
        token=tok2,
        body=outreach_body(alb) | {"recipient": {"name": "E2E B Caller", "phone": "+15555550172"}},
    )
    status_is(o2, 202)
    a2 = o2.body["approval"]
    d = rs(
        "POST",
        f"/api/v1/approvals/{a2['approval_id']}/decision",
        token=tok2,
        body={"decision": "APPROVED", "action_payload_hash": a2["action_payload_hash"]},
    )
    status_is(d, 200)
    c = rs(
        "POST",
        proj(pb, "/calls"),
        token=tok2,
        body={"communication_id": o2.body["communication"]["communication_id"]},
    )
    status_is(c, 202)
    sim = rs(
        "POST", proj(pb, f"/calls/{c.body['call_id']}/simulate"), token=tok2, body={"owner_action": "later"}
    )
    status_is(sim, 200)
    hs = rs("GET", proj(pb, "/handoffs"), token=tok2).body
    expect(len(hs) == 1, "one handoff in the second owner's project", hs)
    env = rs("GET", proj(pb), token=tok2).body
    lay = rs("GET", proj(pb, "/layouts"), token=tok2).body
    put(
        "I01",
        tok2=tok2,
        owner2=owner2,
        pb=pb,
        alb=alb,
        hwb=props[HOLLYWOOD],
        comm_b=o.body["communication"],
        appr_b=o.body["approval"],
        handoff_b=hs[0],
        call_b=c.body["call_id"],
        profile_b=env["business_profile"],
        task_b=s.body["task"]["task_id"],
        layouts_b=lay,
    )
    return (
        f"owner {owner2} (a token signed with the run's secret), project {pb}: 2 properties, a PENDING outreach, "
        f"a simulated call with handoff {hs[0]['handoff_request_id']} ({hs[0]['status']})"
    )


def isolation_case(
    cid: str,
    method: str,
    path: str,
    token: str,
    body: Any,
    py_defect: str | None,
    forwarded: bool = False,
    allow: tuple[int, ...] = (404,),
    secret_values: tuple[str, ...] = (),
) -> tuple[list[str], str]:
    """Rust must answer 404, log the request (and python must not, unless the route is forwarded), and no row may
    change. Then the same request on the python port: its answer and what it wrote are reported; a 2xx fails only
    where the shared plan lists no known python defect (py_defect)."""
    problems = []
    rows = quiet_digest()
    r, at_rs, at_py = tagged("rust", cid, method, path, token=token, body=body)
    if r.status not in allow or (r.status == 403 and (r.body or {}).get("error_code") != "FORBIDDEN"):
        problems.append(f"rust answered {r.show(200)}")
    leaked = [v for v in secret_values if v and v in r.text]
    if leaked:
        problems.append(f"rust's refusal carries fields of the record: {leaked}")
    if not (len(at_rs) == 1 and at_rs[0].get("status") == r.status and len(at_py) == (1 if forwarded else 0)):
        problems.append(
            f"request id: rust log {[(x.get('path'), x.get('status')) for x in at_rs]}, python log {[(x.get('path'), x.get('status')) for x in at_py]}"
        )
    changed = digest_diff(rows, db_digest())
    if changed:
        problems.append(f"rows changed by the rust request: {changed}")
    rows = quiet_digest()
    p, _, _ = tagged("python", cid, method, path, token=token, body=body)
    wrote = digest_diff(rows, quiet_digest())
    pb = p.body if isinstance(p.body, dict) else {}
    pnote = f"python {p.status}" + (
        "" if 200 <= p.status < 300 else f" {pb.get('error_code') or pb.get('detail')}"
    )
    if wrote:
        pnote += f", wrote {wrote}"
    if 200 <= p.status < 300:
        if py_defect:
            pnote += f" (known python defect: {py_defect})"
        else:
            problems.append(
                f"python answered {p.status} where the shared plan lists no defect: {p.show(160)}"
            )
    msg = (r.body or {}).get("message") if isinstance(r.body, dict) else r.text[:60]
    return problems, f"rust {r.status} '{msg}'; {pnote}"


def isolation_check(cases: list[tuple[str, str, str, Any, str | None]], cid: str) -> str:
    out, problems = [], []
    for label, method, path, body, py_defect in cases:
        (tok,) = need("rs_token")
        pr, note = isolation_case(cid, method, path, tok, body, py_defect)
        out.append(f"{label}: {note}")
        problems += [f"{label}: {x}" for x in pr]
    if problems:
        raise Fail(
            "404 from rust, nothing written, request in rust's log only",
            " | ".join(problems) + " || " + " | ".join(out),
        )
    return " | ".join(out)


@check("I02", "layout generation in project A with a property of another project: rust 404, nothing written")
def i02() -> str:
    (pid, hwb, pid2) = need("pid", "hwb", "pid2")
    hw2 = next(
        p["property_id"]
        for p in rs("GET", proj(pid2, "/properties"), token=need("rs_token")[0]).body
        if p["property_identity"]["street_address"] == HOLLYWOOD
    )
    d = "layout generation by property_id"
    return isolation_check(
        [
            (
                "other owner's property",
                "POST",
                proj(pid, "/layouts/generate"),
                {"property_id": hwb, "room_requirements": ROOMS},
                d,
            ),
            (
                "same owner, other project",
                "POST",
                proj(pid, "/layouts/generate"),
                {"property_id": hw2, "room_requirements": ROOMS},
                d,
            ),
        ],
        "I02",
    )


@check(
    "I03", "outreach in project A with a property of another project: rust 404, no approval, nothing written"
)
def i03() -> str:
    (pid, alb, pid2) = need("pid", "alb", "pid2")
    al2 = next(
        p["property_id"]
        for p in rs("GET", proj(pid2, "/properties"), token=need("rs_token")[0]).body
        if p["property_identity"]["street_address"] == ALHAMBRA
    )
    d = "outreach by property_id"
    rec = {"name": "E2E Isolation Broker", "phone": "+15555550181"}
    return isolation_check(
        [
            (
                "other owner's property",
                "POST",
                proj(pid, "/outreach"),
                outreach_body(alb) | {"recipient": rec},
                d,
            ),
            (
                "same owner, other project",
                "POST",
                proj(pid, "/outreach"),
                outreach_body(al2) | {"recipient": rec},
                d,
            ),
        ],
        "I03",
    )


@check(
    "I04",
    "a call start in project A with a communication of another owner's project: rust 404, nothing written",
)
def i04() -> str:
    (pid, comm) = need("pid", "comm_b")
    return isolation_check(
        [
            (
                "other owner's communication",
                "POST",
                proj(pid, "/calls"),
                {"communication_id": comm["communication_id"]},
                "starting a call by communication_id",
            ),
        ],
        "I04",
    )


@check(
    "I05",
    "another project's property under project A's URL: permits, feasibility, evidence, validation all 404",
)
def i05() -> str:
    (pid, alb) = need("pid", "alb")
    return isolation_check(
        [
            (rest, "GET", proj(pid, f"/properties/{alb}/{rest}"), None, None)
            for rest in ("permits", "feasibility", "evidence", "evidence/validation")
        ],
        "I05",
    )


@check(
    "I06",
    "a handoff response in project A for a handoff of another owner's project: rust 404, nothing written",
)
def i06() -> str:
    (pid, h) = need("pid", "handoff_b")
    return isolation_check(
        [
            (
                "other owner's handoff",
                "POST",
                proj(pid, f"/handoffs/{h['handoff_request_id']}/response"),
                {"response": "ACCEPTED"},
                "handoff response",
            ),
        ],
        "I06",
    )


@check(
    "I08",
    "the decision on another owner's approval: refused (403 FORBIDDEN or 404), no field leaked, nothing written",
)
def i08() -> str:
    # 403 FORBIDDEN is accepted here (orchestrator, 2026-11-15): Python's behaviour, ported; the route is not below a
    # project URL. It reveals that the approval id exists; it must carry none of the approval's fields.
    (appr, tok) = need("appr_b", "rs_token")
    keys = ("action_payload_hash", "project_id", "authorized_owner_id", "idempotency_key", "summary")
    fields = tuple(str(appr[k]) for k in keys if appr.get(k)) + tuple(
        v for v in (appr.get("action_payload") or {}).values() if isinstance(v, str) and len(v) >= 8
    )
    pr, note = isolation_case(
        "I08",
        "POST",
        f"/api/v1/approvals/{appr['approval_id']}/decision",
        tok,
        {"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]},
        None,
        allow=(403, 404),
        secret_values=fields,
    )
    if pr:
        raise Fail(
            "403 FORBIDDEN or 404 from rust, no field of the approval, nothing written",
            " | ".join(pr) + " || " + note,
        )
    return f"other owner's approval: {note}"


@check(
    "I09",
    "every project route of another owner's project: 404 'project not found' (never 403), nothing written",
)
def i09() -> str:
    (tok, pb, alb, comm, h, call, task) = need(
        "rs_token", "pb", "alb", "comm_b", "handoff_b", "call_b", "task_b"
    )
    lays = need("layouts_b")[0]
    lid = lays[0]["design_version_id"] if lays else str(uuid.uuid4())
    gid = (lays[0].get("geometry_id") if lays else None) or str(uuid.uuid4())
    item = str(uuid.uuid4())
    P = lambda rest="": proj(pb, rest)  # noqa: E731
    routes = [
        ("GET", P(), None),
        ("PUT", P("/business-profile"), {}),
        ("GET", P("/properties"), None),
        ("POST", P("/properties/search"), {"city": "Los Angeles", "state": "CA"}),
        ("GET", P(f"/properties/{alb}"), None),
        ("POST", P(f"/properties/{alb}/analyze"), {}),
        ("GET", P(f"/properties/{alb}/permits"), None),
        ("GET", P(f"/properties/{alb}/feasibility"), None),
        ("GET", P(f"/properties/{alb}/evidence"), None),
        ("GET", P(f"/properties/{alb}/evidence/validation"), None),
        ("GET", P("/rankings"), None),
        ("GET", P("/cost-scenarios"), None),
        ("GET", P("/approvals"), None),
        ("GET", P(f"/tasks/{task}"), None),
        ("GET", P("/events?limit=5"), None),
        ("POST", P("/layouts/generate"), {"property_id": alb, "room_requirements": ROOMS}),
        ("GET", P("/layouts"), None),
        ("GET", P(f"/layouts/geometry/{gid}"), None),
        ("POST", P(f"/layouts/{lid}/revisions"), {"requested_changes": ["x"]}),
        ("GET", P(f"/layouts/{lid}/bom"), None),
        ("POST", P("/outreach"), outreach_body(alb)),
        ("GET", P("/communications"), None),
        ("POST", P("/calls"), {"communication_id": comm["communication_id"]}),
        ("GET", P("/calls"), None),
        ("POST", P(f"/calls/{call}/simulate"), {"owner_action": "later"}),
        ("GET", P("/handoffs"), None),
        ("POST", P(f"/handoffs/{h['handoff_request_id']}/response"), {"response": "ACCEPTED"}),
        ("POST", P("/chat"), {"message": "what's the status?"}),
        ("POST", P("/nessie/sandbox"), {}),
        ("GET", P("/nessie/sandbox"), None),
        ("GET", P(f"/properties/{alb}/nessie/plan"), None),
        ("GET", P(f"/properties/{alb}/nessie/affordability"), None),
        ("POST", P(f"/properties/{alb}/nessie/payments/{item}/approval"), {}),
        ("POST", P(f"/properties/{alb}/nessie/payments/{item}/execute"), {}),
        ("POST", P(f"/properties/{alb}/nessie/payments/{item}/refresh"), {}),
    ]
    problems, py_answers, rows = [], {}, quiet_digest()
    for method, path, body in routes:
        r, at_rs, at_py = tagged("rust", "I09", method, path, token=tok, body=body)
        short = f"{method} {path.replace(pb, '{B}')}"
        b = r.body if isinstance(r.body, dict) else {}
        if not (r.status == 404 and b.get("message") == "project not found" and not b.get("details")):
            problems.append(f"{short}: rust {r.show(140)}")
        fwd = path.endswith("/chat")
        if not (len(at_rs) == 1 and len(at_py) == (1 if fwd else 0)):
            problems.append(f"{short}: request id in rust log {len(at_rs)}x, python log {len(at_py)}x")
        p = py(method, path, token=tok, body=body)
        pb_ = p.body if isinstance(p.body, dict) else {}
        py_answers.setdefault(f"{p.status} {pb_.get('message') or pb_.get('detail')}", []).append(
            short.split(" ", 1)[1].split("/")[-1] or "project"
        )
        if 200 <= p.status < 300 or p.status == 403:
            problems.append(f"{short}: python {p.show(140)}")
    # the event stream separately: an answer that starts streaming must not be taken for a 404
    with httpx.Client(base_url=RS, trust_env=False) as c:
        with c.stream("GET", P("/events/stream"), headers={"Authorization": f"Bearer {tok}"}, timeout=5) as s:
            if s.status_code != 404:
                problems.append(f"GET {{B}}/events/stream: rust {s.status_code}")
    changed = digest_diff(rows, db_digest())
    if changed:
        problems.append(f"rows changed: {changed}")
    if problems:
        raise Fail("404 project not found for every route, nothing written", " | ".join(problems))
    return (
        f"{len(routes) + 1} routes (every project route rust serves or forwards, with the other owner's real record "
        f"ids): 404 'project not found', no details, each request id in rust's log (the chat also in python's, "
        f"forwarded); nothing written. python: {py_answers}"
    )


@check(
    "I10",
    "supervisor tools bound to project A given another project's property: errors from rust, nothing written",
)
def i10() -> str:
    (alb,) = need("alb")
    rows, out, problems = quiet_digest(), [], []
    for name, args in (
        ("get_feasibility", {"property_id": alb}),
        ("get_property_evidence", {"property_id": alb}),
        ("validate_evidence", {"property_id": alb}),
        (
            "request_outreach_approval",
            {
                "property_id": alb,
                "recipient_name": "E2E Tool Broker",
                "recipient_phone": "+15555550191",
                "questions": "sink?",
            },
        ),
    ):
        o, text, reqs = run_tool(name, args)
        err = tool_error(o)
        if err is None or err.get("http_status") != 404:
            problems.append(f"{name}: {_short(text, 160)}")
        elif not any(x.get("status") == 404 for x in reqs):
            problems.append(f"{name}: no 404 in rust's log ({seen(reqs)})")
        else:
            out.append(f"{name}: {err.get('error_code')} 404")
    changed = digest_diff(rows, db_digest())
    if changed:
        problems.append(f"rows changed: {changed}")
    if problems:
        raise Fail("a 404 tool error for each, nothing written", " | ".join(problems))
    return "; ".join(out) + "; nothing written (in-process mode, python's known defect, not exercised)"


@check(
    "I07",
    "PUT business-profile of project A carrying another project's profile_id: rust 404, nothing written",
)
def i07() -> str:
    # Last of I on purpose: python's known defect moves the other project's profile to project A, after which the
    # other project's call flow answers PROFILE_REQUIRED.
    (pid, prof) = need("pid", "profile_b")
    return isolation_check(
        [
            (
                "other owner's profile",
                "PUT",
                proj(pid, "/business-profile"),
                prof,
                "PUT business-profile overwrites another project's profile",
            ),
        ],
        "I07",
    )


# =================================================================================================================
# K. Photon (the owner's text channel, Python) through the Rust origin
# =================================================================================================================
SIMULATE_TEXT = "/api/v1/owner/channel/simulate-text"
# What an owner text may write: the communication, the event, the dedupe row and the channel's link fields.
TEXT_TABLES = {"communications", "activity_events", "processed_webhook_events", "owner_channels"}
DESIGN_REPLY = "Noted. I saved this design change: {}. Layout edits still happen in the app."


def ok(r: R, code: int) -> None:
    """status_is, except that a route neither service has (the ordinary 404) is a SKIP, never a FAIL or a PASS."""
    if r.status == 404 and r.body == {"detail": "Not Found"}:
        raise Skip(f"route missing: {r.show(150)}")
    status_is(r, code)


@check("K01", "set-up: a fresh project through rust is the one python files the owner's texts under")
def k01() -> str:
    """Python files an owner text under the owner's latest project (`latest_project_id`: the newest created_at),
    not under a project the text names. This owner has some 90 projects by now (C12), so K01 first creates one
    through rust, searches it, and asks python's own function which project an owner text would land in."""
    from app.domains.communications.owner_channel import latest_project_id
    from sqlalchemy.orm import Session

    (tok, owner) = need("rs_token", "owner")
    r = rs("POST", "/api/v1/projects", token=tok, body=fixture_body() | {"project_name": f"E2E texts {RUN}"})
    ok(r, 201)
    tp = r.body["project"]["project_id"]
    s = rs("POST", proj(tp, "/properties/search"), token=tok, body={"city": "Los Angeles", "state": "CA"})
    ok(s, 202)
    rk = rs("GET", proj(tp, "/rankings"), token=tok)
    ok(rk, 200)
    expect(len(rk.body) == 1, "one ranking (for STATUS)", rk.show(200))
    with Session(_engine()) as db:  # read only
        latest = latest_project_id(db, uuid.UUID(owner))
    expect(str(latest) == tp, f"python's latest_project_id is the new project {tp}", latest)
    put("K01", text_pid=tp)
    return f"project {tp} created and searched through rust; python's latest_project_id for the owner is it"


def received_event(text: str) -> dict[str, Any]:
    """The communication.message_received event Python wrote for this owner text, read through the rust origin."""
    (tok, pid) = need("rs_token", "text_pid")
    r = rs("GET", proj(pid, "/events?limit=500"), token=tok)
    ok(r, 200)
    ev = [
        e
        for e in r.body.get("events", [])
        if e.get("event_type") == "communication.message_received"
        and (e.get("payload") or {}).get("summary") == text[:120]
    ]
    expect(len(ev) == 1, f"one communication.message_received event for {text!r} (read through rust)", ev)
    return ev[0]


def design_comms(words: str) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """The 'design change' communications with these words, as rust and as python list them."""
    (tok, pid) = need("rs_token", "text_pid")
    out = []
    for side in (rs, py):
        r = side("GET", proj(pid, "/communications"), token=tok)
        ok(r, 200)
        out.append(
            [
                c
                for c in r.body
                if c.get("purpose") == "design change"
                and any(f.get("answer") == words for f in c.get("structured_findings") or [])
            ]
        )
    return out[0], out[1]


def owner_text(text: str) -> tuple[R, list[dict[str, Any]], list[str]]:
    """The owner texts `text` (fixture Photon) through the rust origin's forwarded simulate-text. Returns the
    answer, the rust request-log lines written meanwhile, and the tables that changed."""
    forwarder()
    (tok,) = need("rs_token")
    before = quiet_digest()
    off_rs = log_offset(RUST_LOG)
    r = via_rust("POST", SIMULATE_TEXT, py_status=200, token=tok, body={"text": text})
    ok(r, 200)
    rust_reqs = requests_since(RUST_LOG, off_rs)
    changed = digest_diff(before, quiet_digest())
    return r, rust_reqs, changed


def only_text_tables(changed: list[str]) -> None:
    tables = {c.split(":", 1)[0] for c in changed}
    expect(
        tables <= TEXT_TABLES,
        f"only {sorted(TEXT_TABLES)} changed (no layout, design revision, task, approval or call)",
        changed,
    )


def design_change_check(prefix: str) -> str:
    (tok, pid) = need("rs_token", "pid")
    need("linked")
    words = f"{'move the reception desk to the front' if prefix == 'revise' else 'add a second sink'} {RUN}"
    text = f"{prefix}: {words}"
    r, rust_reqs, changed = owner_text(text)
    expect(r.body.get("routed") is True, "routed true", r.body)
    expect(
        r.body.get("reply") == DESIGN_REPLY.format(words),
        f"Jasleen's reply {DESIGN_REPLY.format(words)!r}",
        r.body,
    )
    only_text_tables(changed)
    others = [x for x in rust_reqs if x.get("path") != SIMULATE_TEXT]
    expect(
        not others, "no other request at rust while the text was handled (no supervisor tool call)", others
    )
    ev = received_event(text)
    expect(
        ev["payload"].get("handled_as") == "command", "handled_as command (never sent to chat)", ev["payload"]
    )
    in_rs, in_py = design_comms(words)
    expect(len(in_py) == 1, "one 'design change' communication with the words (python's list)", in_py)
    expect(len(in_rs) == 1, "the same record in rust's own communications list", [len(in_rs), in_py[0]])
    d = strict_diff(in_rs[0], in_py[0])
    expect(not d, "the same document from rust and python", d[:5])
    c = in_rs[0]
    f = c["structured_findings"]
    expect(
        len(f) == 1
        and f[0].get("classification") == "OWNER_REPORTED"
        and f[0].get("question") == "design change"
        and c.get("state") == "COMPLETED"
        and c.get("channel") == "PHOTON_IMESSAGE"
        and c.get("provider_message_ids") == [ev["payload"].get("provider_message_id")],
        "one OWNER_REPORTED finding, COMPLETED, PHOTON_IMESSAGE, the inbound message id",
        c,
    )
    put(f"K0{2 if prefix == 'revise' else 3}", **{f"design_{prefix}": c["communication_id"]})
    return (
        f"reply is Jasleen's; communication {c['communication_id']} ('design change', OWNER_REPORTED "
        f"{words!r}) listed identically by rust and python; event handled_as=command; changed only "
        f"{sorted({x.split(':', 1)[0] for x in changed})}; no other rust request; request id in both logs"
    )


@check("K02", "an owner text 'revise: ...' through the rust origin is saved as a design change, nothing else")
def k02() -> str:
    return design_change_check("revise")


@check("K03", "an owner text 'change: ...' through the rust origin is saved as a design change, nothing else")
def k03() -> str:
    return design_change_check("change")


def photon_event(event_id: str, msg_id: str, text: str) -> bytes:
    """The fixture Photon webhook body with this owner's handle, space and agent, and the given ids and text."""
    ev = json.loads(PHOTON_INBOUND.read_text())
    ev.pop("_meta", None)
    ev["id"] = event_id
    ev["data"]["id"] = msg_id
    ev["data"]["sentBy"]["handle"] = "+15555550142"  # OWNER_HANDLE, normalised
    ev["data"]["receivedBy"]["id"] = "usr_fixture_agent"
    ev["data"]["space"]["id"] = "spc_fixture_owner"
    ev["data"]["content"]["text"] = text
    return json.dumps(ev).encode()


@check("K04", "a signed Photon webhook 'change: ...' through the rust origin: stored once, replays ignored")
def k04() -> str:
    forwarder()
    need("linked")
    words = f"put the retail shelf by the window {RUN}"
    text = f"change: {words}"
    body = photon_event(f"evt_e2e_k04_{RUN}", f"msg_e2e_k04_{RUN}", text)
    before = quiet_digest()
    h = photon_headers(body, f"evt_e2e_k04_{RUN}")
    a = via_rust("POST", "/webhooks/photon", py_status=204, content=body, headers=h)
    expect(a.status == 204, "204", a.show())
    mid = quiet_digest()
    only_text_tables(digest_diff(before, mid))
    in_rs, in_py = design_comms(words)
    expect(
        len(in_rs) == 1 and len(in_py) == 1, "one design change in rust's and python's list", [in_rs, in_py]
    )
    expect(not strict_diff(in_rs[0], in_py[0]), "the same document", strict_diff(in_rs[0], in_py[0])[:5])
    expect(
        in_rs[0].get("provider_message_ids") == [f"msg_e2e_k04_{RUN}"],
        "the webhook's message id",
        in_rs[0].get("provider_message_ids"),
    )
    ev = received_event(text)
    expect(ev["payload"].get("handled_as") == "command", "handled_as command", ev["payload"])
    # The same delivery again (same webhook-id), then a new delivery of the same message (new webhook-id).
    again = via_rust(
        "POST", "/webhooks/photon", py_status=204, content=body, headers=photon_headers(body, h["webhook-id"])
    )
    expect(again.status == 204, "204 for the replay", again.show())
    same = quiet_digest()
    expect(same == mid, "nothing changed on the replay of the same webhook-id", digest_diff(mid, same))
    body2 = photon_event(f"evt_e2e_k04b_{RUN}", f"msg_e2e_k04_{RUN}", text)
    other = via_rust(
        "POST",
        "/webhooks/photon",
        py_status=204,
        content=body2,
        headers=photon_headers(body2, f"evt_e2e_k04b_{RUN}"),
    )
    expect(other.status == 204, "204 for a new delivery of the same message", other.show())
    last = quiet_digest()
    moved = digest_diff(same, last)
    expect(
        {x.split(":", 1)[0] for x in moved} <= {"processed_webhook_events"},
        "only the new delivery's dedupe row (the message itself is ignored)",
        moved,
    )
    in_rs2, in_py2 = design_comms(words)
    expect(len(in_rs2) == 1 and len(in_py2) == 1, "still one design change", [len(in_rs2), len(in_py2)])
    received_event(text)  # still exactly one message_received event
    return (
        f"204; communication {in_rs[0]['communication_id']} stored once, identical in both lists, handled_as "
        f"command; same webhook-id again: 204, no row changed; same message under a new webhook-id: 204, "
        f"only {moved or 'nothing'}"
    )


@check(
    "K05", "STATUS, an ordinary text and STOP through the rust origin; both services read the same afterwards"
)
def k05() -> str:
    (tok, pid) = need("rs_token", "pid")
    need("linked")
    st, _, changed = owner_text("STATUS")
    only_text_tables(changed)
    direct = py("POST", SIMULATE_TEXT, token=tok, body={"text": "STATUS"})
    ok(direct, 200)
    expect(
        st.body.get("reply") == direct.body.get("reply") and "Top pick:" in (st.body.get("reply") or ""),
        "the python STATUS reply (with the top pick)",
        [st.body, direct.body],
    )
    chat_text = f"Which property is best? {RUN}"
    ch, chat_reqs, _ = owner_text(chat_text)
    ev = received_event(chat_text)
    expect(ev["payload"].get("handled_as") == "chat", "handled_as chat for an ordinary text", ev["payload"])
    expect(bool(ch.body.get("reply")), "a chat reply", ch.body)
    tools = sorted({x.get("path") for x in chat_reqs if x.get("path") != SIMULATE_TEXT})
    expect(bool(tools), "the supervisor's tool called rust while the ordinary text was handled", chat_reqs)
    sp, _, _ = owner_text("STOP")
    expect(
        sp.body.get("reply") == "Okay, I'll stop texting you. Text START to turn updates back on.",
        "Jasleen's STOP reply",
        sp.body,
    )
    a, b = rs("GET", "/api/v1/owner/channel", token=tok), py("GET", "/api/v1/owner/channel", token=tok)
    expect(
        a.body == b.body and (a.body.get("channel") or {}).get("status") == "PENDING_FIRST_MESSAGE",
        "PENDING_FIRST_MESSAGE from both origins",
        [a.show(200), b.show(200)],
    )
    (tp,) = need("text_pid")
    comms = same_answer(proj(tp, "/communications"), tok, "communications list of the texts' project")
    comms_main = same_answer(proj(pid, "/communications"), tok, "communications list of the first project")
    # Link again for what follows (the first text after STOP links the channel).
    hi, _, _ = owner_text("hi")
    g = py("GET", "/api/v1/owner/channel", token=tok)
    expect(
        (g.body.get("channel") or {}).get("status") == "LINKED",
        "LINKED again after 'hi'",
        [hi.body, g.show()],
    )
    return (
        f"STATUS reply = python's ({_short(st.body.get('reply'), 80)}); ordinary text handled_as chat, rust "
        f"tool requests {tools}; STOP -> PENDING_FIRST_MESSAGE on both origins; communications of the texts' project {comms}, "
        f"of the first project {comms_main}; relinked"
    )


ESCALATION = "Before we go further, the landlord will want first month and a deposit by Friday. Can you commit to that?"


def telnyx_send(event_type: str, payload: dict[str, Any]) -> R:
    """One Telnyx-shaped event to rust's native webhook, signed with the run's test key (rust has the public half)."""
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    if not TELNYX_TEST_KEY:
        raise Skip("run.sh passed no E2E_TELNYX_TEST_KEY (rust has no TELNYX_PUBLIC_KEY)")
    key = Ed25519PrivateKey.from_private_bytes(base64.b64decode(TELNYX_TEST_KEY))
    ev = {
        "data": {
            "record_type": "event",
            "event_type": event_type,
            "id": str(uuid.uuid4()),
            "payload": payload,
        }
    }
    body, ts = json.dumps(ev).encode(), str(int(time.time()))
    sig = base64.b64encode(key.sign(ts.encode() + b"|" + body)).decode()
    h = {"telnyx-signature-ed25519": sig, "telnyx-timestamp": ts, "content-type": "application/json"}
    return rs("POST", "/webhooks/telnyx", content=body, headers=h)


def call_to_handoff(name: str, phone: str) -> dict[str, Any]:
    """A fresh outreach, approval and simulated call through rust, then three signed Telnyx events (answered, the
    intro ended, the broker asks for a commitment) to rust: the call is HANDOFF_PENDING, every row written by rust."""
    (tok, pid) = need("rs_token", "pid")
    o = new_outreach("K06", name, phone)
    a = o["approval"]
    d = rs(
        "POST",
        f"/api/v1/approvals/{a['approval_id']}/decision",
        token=tok,
        body={"decision": "APPROVED", "action_payload_hash": a["action_payload_hash"]},
    )
    ok(d, 200)
    c = rs(
        "POST",
        proj(pid, "/calls"),
        token=tok,
        body={"communication_id": o["communication"]["communication_id"]},
    )
    ok(c, 202)
    call = c.body
    state = base64.b64encode(
        json.dumps({"call_id": call["call_id"], "role": "counterparty"}).encode()
    ).decode()
    p = {"call_control_id": call["provider_call_id"], "client_state": state}
    for et, extra in (
        ("call.answered", {}),
        ("call.speak.ended", {"status": "completed"}),
        (
            "call.transcription",
            {"transcription_data": {"transcript": ESCALATION, "is_final": True, "confidence": 0.95}},
        ),
    ):
        r = telnyx_send(et, p | extra)
        ok(r, 204)
    now = next(x for x in rs("GET", proj(pid, "/calls"), token=tok).body if x["call_id"] == call["call_id"])
    expect(now.get("state") == "HANDOFF_PENDING" and now.get("handoff_request_id"), "HANDOFF_PENDING", now)
    return now


def call_and_handoff(call_id: str, handoff_id: str) -> tuple[dict[str, Any], dict[str, Any]]:
    """The call and its handoff, read through rust and through python; FAIL unless both read the same documents."""
    (tok, pid) = need("rs_token", "pid")
    out = {}
    for side in (rs, py):
        cs, hs = side("GET", proj(pid, "/calls"), token=tok), side("GET", proj(pid, "/handoffs"), token=tok)
        ok(cs, 200)
        ok(hs, 200)
        out[side] = (
            next((x for x in cs.body if x["call_id"] == call_id), None),
            next((x for x in hs.body if x["handoff_request_id"] == handoff_id), None),
        )
    d = strict_diff(out[rs][0], out[py][0]) + strict_diff(out[rs][1], out[py][1])
    expect(not d, "the same call and handoff from rust and python", d[:5])
    return out[rs]


@check("K06", "JOIN and LATER by text through the rust origin against calls and handoffs rust wrote")
def k06() -> str:
    forwarder()
    (tok, pid) = need("rs_token", "pid")
    need("linked")
    notes = []
    for word, expect_status in (("JOIN", "ACCEPTED"), ("LATER", "DEFERRED")):
        call = call_to_handoff(f"E2E {word} Broker", "+15555550161" if word == "JOIN" else "+15555550162")
        hid = call["handoff_request_id"]
        _, h0 = call_and_handoff(call["call_id"], hid)
        expect(h0 and h0.get("status") in ("PENDING", "NOTIFIED"), "an open handoff before the text", h0)
        r = via_rust("POST", SIMULATE_TEXT, py_status=200, token=tok, body={"text": word})
        ok(r, 200)
        c1, h1 = call_and_handoff(call["call_id"], hid)
        expect(h1.get("status") == expect_status, f"handoff {expect_status} after {word}", h1)
        if word == "JOIN":
            expect(
                c1.get("state") == "OWNER_JOINING"
                and r.body.get("reply") == "Calling you now. Pick up to join the broker.",
                "call OWNER_JOINING and Jasleen's 'Calling you now' reply",
                [c1.get("state"), r.body],
            )
        else:
            expect(
                c1.get("state") == "AI_RESUMED"
                and c1.get("deferred_questions")
                and r.body.get("reply")
                == "Okay. I told them you'll follow up directly, and I didn't agree to anything.",
                "call AI_RESUMED with deferred questions and Jasleen's LATER reply",
                [c1.get("state"), c1.get("deferred_questions"), r.body],
            )
        # A second answer to the same handoff: refused by both services, and a second text finds no call waiting.
        again = []
        for side in ("rust", "python"):
            a, _, _ = tagged(
                side,
                "K06",
                "POST",
                proj(pid, f"/handoffs/{hid}/response"),
                token=tok,
                body={"response": "ACCEPTED"},
            )
            ok(a, 409)
            again.append(f"{side} 409 {(a.body or {}).get('error_code')}")
        t2 = via_rust("POST", SIMULATE_TEXT, py_status=200, token=tok, body={"text": word})
        expect(
            t2.body.get("reply") == "There's no call waiting for you right now.",
            "'no call waiting' for the second text",
            t2.body,
        )
        c2, h2 = call_and_handoff(call["call_id"], hid)
        expect(h2.get("status") == expect_status, f"handoff still {expect_status}", h2)
        if word == "JOIN":
            put("K06", joined_call=c2)
        notes.append(
            f"{word}: call {call['call_id'][:8]} -> {c1.get('state')}, handoff {expect_status}; {', '.join(again)}"
        )
    return "; ".join(notes) + "; calls, handoffs and texts all read identically by rust and python"


def python_photon_routes() -> list[tuple[str, str]]:
    """Photon and owner-channel routes of the live python app (its OpenAPI document)."""
    r = py("GET", "/openapi.json")
    ok(r, 200)
    out = []
    for path, ops in r.body.get("paths", {}).items():
        if "/owner" in path or "photon" in path:
            out += [(m.upper(), path) for m in ops if m in ("get", "put", "post", "patch", "delete")]
    return sorted(out)


@check("K07", "every photon and owner-channel route of the python app is reachable through the rust origin")
def k07() -> str:
    forwarder()
    (tok,) = need("rs_token")
    routes = python_photon_routes()
    src = (REPO / "apps/backend/crates/server/src/http/routes/forward.rs").read_text()
    listed = set(re.findall(r'(?:route|long)\("([A-Z]+)", "([^"]+)"\)', src))
    expect(
        ("GET", "/api/v1/owner/channel") in listed, "the forward list parsed from forward.rs", sorted(listed)
    )
    missing, reached, internal = [], [], []
    for method, path in routes:
        if path.startswith("/internal/"):
            internal.append(f"{method} {path}")  # rust's own 404 by design (F07)
            continue
        if (method, path) not in listed:
            missing.append(f"{method} {path} not in forward.rs")
        kw: dict[str, Any] = {"token": tok}
        if method == "PUT":
            kw["body"] = {"handle": OWNER_HANDLE}
        elif path == "/webhooks/photon":
            kw = {"content": b"{}", "headers": {"content-type": "application/json"}}  # unsigned: python's 400
        elif path == SIMULATE_TEXT:
            kw["body"] = {"text": "STATUS"}
        try:
            r = via_rust(method, path, **kw)
        except Fail as f:
            missing.append(f"{method} {path} not forwarded: {f}")
            continue
        if r.status == 404:
            missing.append(f"{method} {path}: 404 through rust")
        reached.append(f"{method} {path} {r.status}")
    if missing:
        S["k07_missing"] = missing
    expect(not missing, "every route forwarded and answered by python", missing)
    expect(len(reached) >= 5, "at least the five known photon routes", reached)
    return f"{len(reached)} routes, each once in both logs: {reached}; not forwarded by design: {internal}"


@check("K08", "after the JOIN text: the owner leg python dialed, driven by signed telnyx events to rust")
def k08() -> str:
    """Python's owner_response (K06) dialed the owner and stored the owner leg and the conference on the call rust
    wrote; the carrier's events for that leg come to rust's native webhook. Rust must take them on that call."""
    (call,) = need("joined_call")
    legs = [s for s in call.get("participant_states") or [] if s.get("role") == "owner"]
    expect(len(legs) == 1 and legs[0].get("leg_id"), "one owner leg with a leg id (python dialed it)", call)
    expect(bool(call.get("conference_id")), "a conference id on the call", call.get("conference_id"))
    leg = legs[0]["leg_id"]
    owner = base64.b64encode(json.dumps({"call_id": call["call_id"], "role": "owner"}).encode()).decode()
    p = {"call_control_id": leg, "client_state": owner, "conference_id": call["conference_id"]}
    states = []
    for et, extra in (
        ("call.answered", {}),
        ("conference.participant.joined", {}),
        ("call.hangup", {"hangup_cause": "normal_clearing"}),
    ):
        ok(telnyx_send(et, p | extra), 204)
        c, _ = call_and_handoff(call["call_id"], call["handoff_request_id"])
        states.append(c.get("state"))
    expect(
        states == ["OWNER_JOINING", "HUMAN_JOINED", "AI_RESUMED"],
        "OWNER_JOINING after the owner answers, HUMAN_JOINED once in the conference, AI_RESUMED after the owner hangs up",
        states,
    )
    texts = [t.get("text") for t in c.get("transcript") or []]
    expect(
        "[owner joined the call]" in texts and "[owner left the call]" in texts,
        "the joined and left turns in the transcript",
        texts[-6:],
    )
    return f"owner leg {leg} of conference {call['conference_id']}: states {states}; read identically by both services"


# =================================================================================================================
# H. Hygiene
# =================================================================================================================
@check("H01", "no kept log contains the shared secret, the Photon secret or a token signed with the secret")
def h01() -> str:
    tokens = set(TOKENS_SENT)
    for k in ("rs_token", "py_token"):
        if k in S:
            tokens.add(S[k])
    needles = {"AUTH_SECRET": SECRET, "PHOTON_WEBHOOK_SECRET": PHOTON_SECRET}
    needles["PHOTON_WEBHOOK_SECRET key"] = PHOTON_SECRET.removeprefix("whsec_")
    needles["the Telnyx test private key"] = TELNYX_TEST_KEY
    needles |= {f"token#{i}": t for i, t in enumerate(sorted(tokens))}
    found = []
    kept = sorted(
        {RUST_LOG, PY_LOG} | set(RUST_LOG.parent.glob("*.log")) | set(RUST_LOG.parent.glob("*.txt"))
    )
    for log in kept:
        text = log.read_text(errors="replace")
        # Any token this run's AUTH_SECRET signed, whoever minted it (the services and the tools mint their own).
        for t in set(re.findall(r"[A-Za-z0-9_-]{16,}\.[0-9a-f]{64}", text)):
            enc, sig = t.rsplit(".", 1)
            body = base64.urlsafe_b64decode(enc + "=" * (-len(enc) % 4))
            if hmac.compare_digest(hmac.new(SECRET.encode(), body, hashlib.sha256).hexdigest(), sig):
                found.append(f"a token signed with AUTH_SECRET in {log.name}")
        found += [f"{name} in {log.name}" for name, v in needles.items() if v and v in text]
        # a token's signature alone is enough to forge nothing, but must not be logged either
        found += [
            f"signature of {name} in {log.name}"
            for name, v in needles.items()
            if "." in v and v.rsplit(".", 1)[1] in text
        ]
    expect(not found, "no secret or token in the logs", found)
    return (
        f"checked {len(needles)} values ({len(tokens)} tokens sent) and every token signed with AUTH_SECRET "
        f"against {len(kept)} kept files ({', '.join(x.name for x in kept)}): none found"
    )


# =================================================================================================================
def main() -> int:
    results, unexpected = [], []
    for cid, title, fn in CHECKS:
        try:
            detail, outcome = fn() or "", "PASS"
            PASSED.add(cid)
        except Skip as s:
            detail, outcome = str(s), "SKIP"
            if not isinstance(s, NotNeeded):
                unexpected.append(cid)
        except Fail as f:
            detail, outcome = str(f), "FAIL"
        except Exception as exc:  # noqa: BLE001 - a crash in one check is that check's FAIL
            tb = traceback.extract_tb(exc.__traceback__)[-1]
            detail, outcome = (
                f"unexpected {type(exc).__name__}: {_short(str(exc), 300)} (driver.py:{tb.lineno})",
                "FAIL",
            )
        print(f"{cid} {outcome:<4} {title} :: {detail}", flush=True)
        results.append({"id": cid, "result": outcome, "title": title, "detail": detail})
    totals = {k: sum(1 for r in results if r["result"] == k) for k in ("PASS", "FAIL", "SKIP")}
    print(
        f"SUMMARY {len(results)} checks: {totals['PASS']} PASS, {totals['FAIL']} FAIL, {totals['SKIP']} SKIP"
        + (
            f" ({len(unexpected)} not expected, so the gate is not met: {' '.join(unexpected)})"
            if unexpected
            else ""
        ),
        flush=True,
    )
    if RESULTS:
        Path(RESULTS).write_text(json.dumps({"run": RUN, "totals": totals, "results": results}, indent=1))
    for c in _CLIENTS.values():
        c.close()
    return 1 if totals["FAIL"] else 3 if unexpected else 0


if __name__ == "__main__":
    sys.exit(main())
