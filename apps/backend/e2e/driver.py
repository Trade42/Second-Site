"""End-to-end driver, part 1: numbered checks against the running Rust service and the running Python app.

Started by run.sh (do not run it by hand unless both processes are up): apps/api/.venv/bin/python -I -B driver.py
with these variables, all set by run.sh and nothing else inherited:
  E2E_REPO, E2E_RUST_URL, E2E_PY_URL       repository root and the two loopback origins
  E2E_RUST_LOG, E2E_PY_LOG                 the two processes' log files (JSON lines; used to count requests)
  E2E_AUTH_SECRET, E2E_PHOTON_SECRET       the per-run secrets both processes were started with (never printed)
  E2E_RESULTS                              where to write the results as JSON (optional)

Every check prints exactly one line: `<id> PASS|FAIL|SKIP <title> :: <detail>`. A failed check never stops the run.
A check whose input comes from an earlier check that did not pass is a SKIP that names that check.
Exit code: 1 when any check failed, else 0. Uses httpx only, plus the Python app's own modules (token minting,
webhook signing, the supervisor tools) imported from apps/api.
"""

from __future__ import annotations

import json
import os
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
RESULTS = os.environ.get("E2E_RESULTS")
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
        super().__init__(f"expected {expected}; got {_short(got, 600)}")


class Skip(Exception):
    pass


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
    return f"identical ({size} {'items' if isinstance(a.body, list) else 'bytes'}; raw JSON, types compared, key order ignored)"


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


@check("C08", "a second project created and searched through the python app")
def c08() -> str:
    (tok,) = need("py_token")
    r = py(
        "POST", "/api/v1/projects", token=tok, body=fixture_body() | {"project_name": f"E2E via Python {RUN}"}
    )
    status_is(r, 201)
    pid2 = r.body["project"]["project_id"]
    s = py("POST", proj(pid2, "/properties/search"), token=tok, body={"city": "Los Angeles", "state": "CA"})
    expect(s.status == 202 and s.body["task"]["status"] == "COMPLETED", "202 COMPLETED search", s.show())
    put("C08", pid2=pid2, search_task2=s.body["task"]["task_id"])
    return f"project {pid2}, search task COMPLETED (python)"


@check("C09", "rust reads the project python wrote")
def c09() -> str:
    (tok, pid2) = need("py_token", "pid2")
    return same_answer(proj(pid2), tok, "project envelope")


@check("C10", "rust reads python's properties, rankings, cost scenarios, events and task")
def c10() -> str:
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
    if failures:
        raise Fail(
            "identical answers for all five",
            " | ".join(failures) + (" || ok: " + ", ".join(out) if out else ""),
        )
    return ", ".join(out)


# =================================================================================================================
# D. The supervisor reaches Rust
# =================================================================================================================
def chat_via_python(message: str) -> tuple[R, list[dict[str, Any]]]:
    (tok, pid) = need("rs_token", "pid")
    off = log_offset(RUST_LOG)
    r = py("POST", proj(pid, "/chat"), token=tok, body={"message": message})
    return r, requests_since(RUST_LOG, off)


@check("D01", "chat 'rank the properties' (model-free supervisor) answers with rust data")
def d01() -> str:
    (tok, pid) = need("rs_token", "pid")
    r, reqs = chat_via_python("rank the properties")
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
    return f"{n} rust GET rankings during the chat; reply '{_short(reply, 90)}'; grounded on {latest}"


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
        expect(out.get("status") == "APPROVAL_REQUIRED" and out.get("approval_id"), "APPROVAL_REQUIRED", out)
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


# =================================================================================================================
# E. Rust's owner notify path as Python sees it
# =================================================================================================================
def notify(key: str, text: str) -> R:
    (owner, pid) = need("owner", "pid")
    svc = issue_token(SECRET, uuid.UUID(owner), ttl_s=300, purpose=SERVICE_PURPOSE)
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


@check("E01", "notify before the owner channel is linked: not_linked")
def e01() -> str:
    key = f"e2e-{RUN}-unlinked"
    r = notify(key, "E2E check: you should not receive this (no channel linked).")
    status_is(r, 200)
    expect(r.body.get("delivery") == "not_linked", "delivery not_linked", r.body)
    put("E01", key_unlinked=key, comm_unlinked=r.body["communication_id"])
    return f"200 not_linked, communication {r.body['communication_id']}"


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


@check("F01", "chat through the rust origin gives the python answer")
def f01() -> str:
    forwarder()
    (tok, pid) = need("rs_token", "pid")
    body = {"message": "what's the status?"}
    a, b = (
        rs("POST", proj(pid, "/chat"), token=tok, body=body),
        py("POST", proj(pid, "/chat"), token=tok, body=body),
    )
    expect(a.status == 200 and a.body == b.body, "the python answer", f"{a.show(150)} | {b.show(150)}")
    return "identical"


@check("F02", "GET /api/v1/owner/channel through the rust origin")
def f02() -> str:
    forwarder()
    (tok,) = need("rs_token")
    a, b = rs("GET", "/api/v1/owner/channel", token=tok), py("GET", "/api/v1/owner/channel", token=tok)
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
        rs("PUT", "/api/v1/owner/channel", token=tok, body=body),
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
    a = rs("POST", "/api/v1/owner/channel/simulate-text", token=tok, body=body)
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
    off = log_offset(PY_LOG)
    a = rs("POST", "/webhooks/photon", content=body, headers=photon_headers(body, f"evt_e2e_{RUN}"))
    reqs = requests_since(PY_LOG, off)
    expect(a.status == 204, "204", a.show())
    expect(count(reqs, "POST", "/webhooks/photon") == 1, "one POST /webhooks/photon at python", reqs)
    return "204; python verified the signature on the forwarded raw body"


@check("F06", "a POST /webhooks/photon with a broken signature through the rust origin is refused")
def f06() -> str:
    forwarder()
    body = PHOTON_INBOUND.read_bytes()
    h = photon_headers(body, f"evt_e2e_bad_{RUN}")
    h["webhook-signature"] = "v1,AAAA" + h["webhook-signature"][7:]
    a = rs("POST", "/webhooks/photon", content=body, headers=h)
    expect(
        a.status == 400 and (a.body or {}).get("error_code") == "INVALID_SIGNATURE",
        "400 INVALID_SIGNATURE",
        a.show(),
    )
    return "400 INVALID_SIGNATURE"


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


@check("G01", "POST .../outreach through the rust origin: a draft and a PENDING approval, nothing sent")
def g01() -> str:
    (tok, pid, owner, alh) = need("rs_token", "pid", "owner", "alhambra")
    r = rs("POST", proj(pid, "/outreach"), token=tok, body=outreach_body(alh))
    if r.status == 404:
        S["g01_absent"] = r.show(200)
        raise Skip(f"rust has no outreach route (not native, not forwarded): {r.show(200)}")
    return check_outreach_answer("G01", r, pid, owner)


@check("G02", "fallback: the same outreach on the python port, so rust's approval routes can be exercised")
def g02() -> str:
    if "g01_absent" not in S:
        raise Skip(
            "not needed: G01 created the approval through the rust origin"
            if "approval" in S
            else "G01 did not pass"
        )
    (tok, pid, owner, alh) = need("rs_token", "pid", "owner", "alhambra")
    r = py("POST", proj(pid, "/outreach"), token=tok, body=outreach_body(alh))
    return check_outreach_answer("G02", r, pid, owner) + " (python-written)"


@check("G03", "the approval appears in the rust approvals list")
def g03() -> str:
    (tok, pid, appr) = need("rs_token", "pid", "approval")
    r = rs("GET", proj(pid, "/approvals"), token=tok)
    status_is(r, 200)
    mine = next((a for a in r.body if a.get("approval_id") == appr["approval_id"]), None)
    expect(mine is not None, f"approval {appr['approval_id']}", [a.get("approval_id") for a in r.body])
    diffs = strict_diff(mine, appr)
    expect(not diffs, "the same approval document", diffs[:5])
    return f"listed, PENDING, identical document ({len(r.body)} approval(s) in the project)"


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
    return f"202 {r.body.get('state')}, simulated"


@check("G06", "what POST .../calls on the python port then does (fixture telephony; no call is placed)")
def g06() -> str:
    (tok, pid, comm, appr) = need("rs_token", "pid", "communication", "approval")
    need("approved")
    r = py("POST", proj(pid, "/calls"), token=tok, body={"communication_id": comm["communication_id"]})
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
def main() -> int:
    results = []
    for cid, title, fn in CHECKS:
        try:
            detail, outcome = fn() or "", "PASS"
            PASSED.add(cid)
        except Skip as s:
            detail, outcome = str(s), "SKIP"
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
        f"SUMMARY {len(results)} checks: {totals['PASS']} PASS, {totals['FAIL']} FAIL, {totals['SKIP']} SKIP",
        flush=True,
    )
    if RESULTS:
        Path(RESULTS).write_text(json.dumps({"run": RUN, "totals": totals, "results": results}, indent=1))
    for c in _CLIENTS.values():
        c.close()
    return 1 if totals["FAIL"] else 0


if __name__ == "__main__":
    sys.exit(main())
