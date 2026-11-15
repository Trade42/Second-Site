"""Wire-parity vectors for the property slice: drive the Python app (FastAPI TestClient, fixture
providers, PostgreSQL) through the demo path and save every response body.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_property_slice.py)
Needs: the sandbox PostgreSQL (Unix socket /tmp/ss-pg). The script creates its own schema
       `wp_<uuid>` inside `secondsite_test`, migrates it with alembic, and drops it at the end; it
       never touches the `public` schema the Python test suite owns.

Output: property_slice.json, a list of steps {name, method, path, body, status, response}. Paths
(and body strings that are exactly one reference) use symbolic references that the Rust replay
resolves the same way:
  {project}, {project_bare}      the demo project / a project without a business profile
  {property:<street_address>}    a stored property, found by street address
  {status_url}                   the status_url of the search's TaskAccepted
  {layout_status_url}            the status_url of the first layout generation
  {layout:<version_number>}      a design version listed by the "layouts" step
  {geometry}                     the geometry_id of the first listed layout
Responses are raw. The Rust test (tests/wire_parity.rs) normalizes both sides identically; see
the comment there for the fields that are ignored.

Since part 3 the demo path continues into the design studio (layout generation, list, geometry,
bill of materials, revisions, the error cases), and design_rows.json records the rows the
design routes stored (site_geometries, design_versions, rooms, bills_of_materials), read back
from the private schema before it is dropped.
"""

from __future__ import annotations

import contextlib
import json
import os
import re
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
API = REPO / "apps" / "api"
sys.path.insert(0, str(API))

DSN = "host=/tmp/ss-pg user=secondsite dbname=secondsite_test"
DATABASE_URL = "postgresql+psycopg://secondsite:secondsite@/secondsite_test?host=/tmp/ss-pg"


@contextlib.contextmanager
def private_schema():
    import psycopg

    name = f"wp_{uuid.uuid4().hex}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"CREATE SCHEMA {name}")
    env = {**os.environ, "DATABASE_URL": DATABASE_URL, "PGOPTIONS": f"-c search_path={name}",
           "ALEMBIC_SKIP_LOGGING": "1"}
    try:
        subprocess.run([sys.executable, "-m", "alembic", "upgrade", "head"], cwd=API, env=env,
                       check=True, capture_output=True)
        os.environ["DATABASE_URL"] = DATABASE_URL
        os.environ["PGOPTIONS"] = env["PGOPTIONS"]
        yield name
    finally:
        with psycopg.connect(DSN, autocommit=True) as c:
            c.execute(f"DROP SCHEMA {name} CASCADE")


def configure(objects_dir: str) -> None:
    # The same settings as apps/api/tests/conftest.py and the Rust test harness.
    os.environ.update({"APP_ENV": "test", "AUTH_SECRET": "test-secret", "SUPERVISOR_MODEL": "",
                       "OBJECT_STORAGE_LOCAL_DIR": objects_dir})
    for m in ("PROPERTY", "PERMIT", "MATTERPORT", "VEXCEL", "MESSAGING", "VOICE", "PROCUREMENT"):
        os.environ[f"{m}_PROVIDER_MODE"] = "fixture"


def main() -> None:
    fx = json.loads((REPO / "fixtures" / "projects" / "facial_studio.json").read_text())
    steps: list[dict] = []
    with tempfile.TemporaryDirectory() as objects, private_schema():
        configure(objects)
        from fastapi.testclient import TestClient

        from app.main import app

        refs: dict[str, str] = {}

        def resolve(path: str) -> str:
            for k, v in refs.items():
                path = path.replace("{" + k + "}", v)
            assert "{" not in path, path
            return path

        def resolve_body(v):
            # Only strings that are exactly one reference are replaced.
            if isinstance(v, str):
                m = re.fullmatch(r"\{([^{}]+)\}", v)
                return refs[m.group(1)] if m and m.group(1) in refs else v
            if isinstance(v, list):
                return [resolve_body(x) for x in v]
            if isinstance(v, dict):
                return {k: resolve_body(x) for k, x in v.items()}
            return v

        with TestClient(app, raise_server_exceptions=False) as c:
            headers: dict[str, str] = {}

            def step(name: str, method: str, path: str, body=None, auth=True):
                r = c.request(method, resolve(path), json=resolve_body(body), headers=headers if auth else {})
                try:
                    resp = r.json()
                except ValueError:
                    resp = {"_non_json": r.headers.get("content-type"), "_length": len(r.content)}
                steps.append({"name": name, "method": method, "path": path, "body": body,
                              "auth": auth, "status": r.status_code, "response": resp})
                return resp

            tok = step("dev_token", "POST", "/api/v1/dev/token", auth=False)
            headers["Authorization"] = f"Bearer {tok['access_token']}"
            created = step("create_project", "POST", "/api/v1/projects",
                           {"project_name": fx["project_name"], "market": fx["market"]})
            refs["project"] = created["project"]["project_id"]
            step("put_business_profile", "PUT", "/api/v1/projects/{project}/business-profile",
                 fx["business_profile"])
            accepted = step("search", "POST", "/api/v1/projects/{project}/properties/search",
                            {"city": "Los Angeles", "state": "CA"})
            refs["status_url"] = accepted["status_url"]
            step("task_status", "GET", "{status_url}")
            props = step("properties", "GET", "/api/v1/projects/{project}/properties")
            streets = []
            for p in props:
                street = p["property_identity"]["street_address"]
                refs[f"property:{street}"] = p["property_id"]
                streets.append(street)
            for street in streets:
                base = "/api/v1/projects/{project}/properties/{property:" + street + "}"
                step(f"property {street}", "GET", base)
                step(f"permits {street}", "GET", base + "/permits")
                step(f"feasibility {street}", "GET", base + "/feasibility")
            step("rankings", "GET", "/api/v1/projects/{project}/rankings")
            step("cost_scenarios", "GET", "/api/v1/projects/{project}/cost-scenarios")

            # Re-analysis of the first property: supersedes its outputs, ranks again.
            first = "/api/v1/projects/{project}/properties/{property:" + streets[0] + "}"
            step("analyze", "POST", first + "/analyze")
            step("feasibility after analyze", "GET", first + "/feasibility")
            step("rankings after analyze", "GET", "/api/v1/projects/{project}/rankings")
            step("cost_scenarios after analyze", "GET", "/api/v1/projects/{project}/cost-scenarios")
            # A second search finds the stored addresses again.
            step("search again", "POST", "/api/v1/projects/{project}/properties/search", {})
            step("properties after second search", "GET", "/api/v1/projects/{project}/properties")
            step("rankings after second search", "GET", "/api/v1/projects/{project}/rankings")

            # Errors.
            bare = step("create_project_without_profile", "POST", "/api/v1/projects",
                        {"project_name": "x", "market": {"city": "Los Angeles", "state": "CA"}})
            refs["project_bare"] = bare["project"]["project_id"]
            step("search without profile", "POST", "/api/v1/projects/{project_bare}/properties/search", {})
            step("property of another project", "GET",
                 "/api/v1/projects/{project_bare}/properties/{property:" + streets[0] + "}")
            step("ortho in fixture mode", "GET", "/api/v1/media/vexcel/ortho/{property:" + streets[0] + "}",
                 auth=False)
            step("ortho of an unknown property", "GET",
                 "/api/v1/media/vexcel/ortho/00000000-0000-4000-8000-000000000000", auth=False)
            step("search with a bad limit", "POST", "/api/v1/projects/{project}/properties/search",
                 {"limit": 51})

            # Design studio (test_api_smoke.py's room program), then its error cases.
            hw, alh = "{property:6922 W HOLLYWOOD BLVD}", "{property:5380 E ALHAMBRA AVE}"
            reqs = [
                {"room_type": "treatment", "count": 3, "min_area_sqm": 10, "min_width_m": 2.8,
                 "needs_plumbing": True},
                {"room_type": "reception", "count": 1, "min_area_sqm": 12, "min_width_m": 3},
                {"room_type": "storage", "count": 1, "min_area_sqm": 4, "min_width_m": 1.5},
            ]
            reqs4 = [dict(reqs[0], count=4)] + reqs[1:]
            twelve = [dict(reqs[0], count=12, min_area_sqm=12)]
            layouts_path = "/api/v1/projects/{project}/layouts"
            gen = step("generate layouts", "POST", layouts_path + "/generate",
                       {"property_id": hw, "room_requirements": reqs})
            refs["layout_status_url"] = gen["status_url"]
            step("generate task status", "GET", "{layout_status_url}")
            layouts = step("layouts", "GET", layouts_path)
            for lv in layouts:
                refs[f"layout:{lv['version_number']}"] = lv["design_version_id"]
            refs["geometry"] = layouts[0]["geometry_id"]
            step("geometry", "GET", layouts_path + "/geometry/{geometry}")
            step("property after generate", "GET", "/api/v1/projects/{project}/properties/" + hw)
            step("bom", "GET", layouts_path + "/{layout:1}/bom")
            step("bom again", "GET", layouts_path + "/{layout:1}/bom")
            step("bom of version 2", "GET", layouts_path + "/{layout:2}/bom")
            step("generate on the stored geometry", "POST", layouts_path + "/generate",
                 {"property_id": hw, "room_requirements": reqs, "candidate_count": 1,
                  "preferences": {"style": "warm_minimalist"}})
            step("revise applied", "POST", layouts_path + "/{layout:1}/revisions",
                 {"requested_changes": ["add a fourth treatment room"], "room_requirement_overrides": reqs4})
            step("revise infeasible", "POST", layouts_path + "/{layout:1}/revisions",
                 {"requested_changes": ["twelve rooms"], "room_requirement_overrides": twelve})
            step("revise unstructured", "POST", layouts_path + "/{layout:1}/revisions",
                 {"requested_changes": ["make it brighter"]})
            step("revise without changes", "POST", layouts_path + "/{layout:1}/revisions",
                 {"requested_changes": []})
            step("layouts after revisions", "GET", layouts_path)
            step("generate without measured geometry", "POST", layouts_path + "/generate",
                 {"property_id": alh, "room_requirements": reqs})
            step("generate an infeasible program", "POST", layouts_path + "/generate",
                 {"property_id": hw, "room_requirements": twelve})
            step("generate for an unknown property", "POST", layouts_path + "/generate",
                 {"property_id": "00000000-0000-4000-8000-000000000000", "room_requirements": reqs})
            step("generate with a bad candidate_count", "POST", layouts_path + "/generate",
                 {"property_id": hw, "room_requirements": reqs, "candidate_count": 6})
            step("layouts of another project", "GET", "/api/v1/projects/{project_bare}/layouts")
            step("bom under another project", "GET", "/api/v1/projects/{project_bare}/layouts/{layout:1}/bom")
            step("geometry under another project", "GET",
                 "/api/v1/projects/{project_bare}/layouts/geometry/{geometry}")
            step("bom of an unknown layout", "GET",
                 layouts_path + "/00000000-0000-4000-8000-000000000000/bom")
            step("events", "GET", "/api/v1/projects/{project}/events?limit=500")
            rows = stored_design_rows()

    out = HERE / "property_slice.json"
    out.write_text(json.dumps(steps, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {out.name}: {len(steps)} steps")
    out = HERE / "design_rows.json"
    out.write_text(json.dumps(rows, indent=1, ensure_ascii=False, default=str) + "\n")
    print(f"wrote {out.name}: " + ", ".join(f"{k} {len(v)}" for k, v in rows.items()))


# The rows of the design tables, every column but the timestamps, in a deterministic order
# (rows written in one transaction share created_at).
DESIGN_ROWS = {
    "site_geometries": "SELECT id, property_id, scale_verified, data, schema_version, record_version "
                       "FROM site_geometries ORDER BY created_at, id",
    "design_versions": "SELECT id, project_id, property_id, parent_version_id, version_number, approval_status, "
                       "valid, data, schema_version, record_version FROM design_versions "
                       "ORDER BY property_id, version_number",
    "rooms": "SELECT r.id, r.design_version_id, r.room_key, r.room_type, r.area_sqm, r.polygon FROM rooms r "
             "JOIN design_versions d ON d.id = r.design_version_id ORDER BY d.version_number, r.room_key",
    "bills_of_materials": "SELECT b.id, b.project_id, b.design_version_id, b.status, b.data, b.schema_version, "
                          "b.record_version FROM bills_of_materials b JOIN design_versions d "
                          "ON d.id = b.design_version_id ORDER BY d.version_number, b.created_at",
}


def stored_design_rows() -> dict:
    import psycopg
    from psycopg.rows import dict_row

    with psycopg.connect(DSN, options=os.environ["PGOPTIONS"], row_factory=dict_row) as c:
        return {t: [dict(r) for r in c.execute(q).fetchall()] for t, q in DESIGN_ROWS.items()}


if __name__ == "__main__":
    main()
