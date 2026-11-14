"""Wire-parity vectors for the property slice: drive the Python app (FastAPI TestClient, fixture
providers, PostgreSQL) through the demo path and save every response body.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_property_slice.py)
Needs: the sandbox PostgreSQL (Unix socket /tmp/ss-pg). The script creates its own schema
       `wp_<uuid>` inside `secondsite_test`, migrates it with alembic, and drops it at the end; it
       never touches the `public` schema the Python test suite owns.

Output: property_slice.json, a list of steps {name, method, path, body, status, response}. Paths
use symbolic references that the Rust replay resolves the same way:
  {project}, {project_bare}      the demo project / a project without a business profile
  {property:<street_address>}    a stored property, found by street address
  {status_url}                   the status_url of the last TaskAccepted
Responses are raw. The Rust test (tests/wire_parity.rs) normalizes both sides identically; see
the comment there for the fields that are ignored.
"""

from __future__ import annotations

import contextlib
import json
import os
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

        with TestClient(app, raise_server_exceptions=False) as c:
            headers: dict[str, str] = {}

            def step(name: str, method: str, path: str, body=None, auth=True):
                r = c.request(method, resolve(path), json=body, headers=headers if auth else {})
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

    out = HERE / "property_slice.json"
    out.write_text(json.dumps(steps, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {out.name}: {len(steps)} steps")


if __name__ == "__main__":
    main()
