"""Test harness.

- Pure tests need nothing. DB/API tests need PostgreSQL at TEST_DATABASE_URL
  (default postgresql+psycopg://secondsite:secondsite@localhost:5432/secondsite_test) and are
  skipped with a clear reason when it is unreachable.
- The schema is created with `alembic upgrade head` (so migrations are exercised), tables are
  truncated between tests.
- All providers are fixtures. No live network calls unless RUN_LIVE_TESTS=1 (marker `live`).
"""

from __future__ import annotations

import base64
import os
import subprocess
import sys
from pathlib import Path

import pytest

TEST_DB = os.environ.get(
    "TEST_DATABASE_URL", "postgresql+psycopg://secondsite:secondsite@localhost:5432/secondsite_test"
)
os.environ["DATABASE_URL"] = TEST_DB
os.environ.setdefault("APP_ENV", "test")
os.environ.setdefault("AUTH_SECRET", "test-secret")
os.environ["SUPERVISOR_MODEL"] = ""
TEST_PHOTON_SECRET = "whsec_" + base64.b64encode(b"photon-test-secret-key-32-bytes!").decode()
os.environ["PHOTON_WEBHOOK_SECRET"] = TEST_PHOTON_SECRET

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey  # noqa: E402
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat  # noqa: E402

TELNYX_PRIVATE = Ed25519PrivateKey.generate()
os.environ["TELNYX_PUBLIC_KEY"] = base64.b64encode(
    TELNYX_PRIVATE.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
).decode()

API_DIR = Path(__file__).resolve().parents[1]
FIXTURES = API_DIR.parents[1] / "fixtures"


def _pg_available() -> str | None:
    try:
        from sqlalchemy import create_engine, text

        eng = create_engine(TEST_DB)
        with eng.connect() as c:
            c.execute(text("select 1"))
        eng.dispose()
        return None
    except Exception as exc:  # noqa: BLE001
        return f"PostgreSQL not reachable at TEST_DATABASE_URL ({type(exc).__name__})"


_PG_REASON = _pg_available()
_MIGRATED = False


@pytest.fixture(scope="session")
def migrated_db():
    global _MIGRATED
    if _PG_REASON:
        pytest.skip(_PG_REASON)
    if not _MIGRATED:
        from sqlalchemy import create_engine, text

        eng = create_engine(TEST_DB)
        with eng.begin() as c:
            c.execute(text("DROP SCHEMA public CASCADE; CREATE SCHEMA public;"))
        eng.dispose()
        env = {**os.environ, "DATABASE_URL": TEST_DB, "ALEMBIC_SKIP_LOGGING": "1"}
        subprocess.run(
            [sys.executable, "-m", "alembic", "upgrade", "head"],
            cwd=API_DIR,
            env=env,
            check=True,
            capture_output=True,
        )
        _MIGRATED = True
    yield TEST_DB


@pytest.fixture
def container():
    from app.core.config import get_settings
    from app.core.container import build_container, set_container

    get_settings.cache_clear()
    c = build_container(get_settings())
    set_container(c)
    yield c
    set_container(None)


@pytest.fixture
def db(migrated_db, container):
    from sqlalchemy import text

    from app.db.base import Base
    from app.db.session import get_engine, session_factory

    eng = get_engine()
    with eng.begin() as conn:
        conn.execute(
            text(
                "TRUNCATE "
                + ", ".join(t.name for t in Base.metadata.sorted_tables)
                + " RESTART IDENTITY CASCADE"
            )
        )
    s = session_factory(eng)()
    yield s
    s.rollback()
    s.close()


@pytest.fixture
def client(db, container):
    from fastapi.testclient import TestClient

    from app.main import app

    with TestClient(app) as tc:
        yield tc


@pytest.fixture
def auth(client):
    tok = client.post("/api/v1/dev/token").json()
    return {"Authorization": f"Bearer {tok['access_token']}"}, tok["user_id"]


@pytest.fixture
def demo_project_body():
    import json

    fx = json.loads((FIXTURES / "projects" / "facial_studio.json").read_text())
    return {
        "project_name": fx["project_name"],
        "market": fx["market"],
        "business_profile": fx["business_profile"],
    }
