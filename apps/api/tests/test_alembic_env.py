"""alembic/env.py on the real PostgreSQL. Each test runs alembic in a private schema of the test database (pinned with
the `options=-csearch_path=...` URL parameter, never `public`), created and dropped here.

- e2e-1 D3: a DATABASE_URL that contains '%' (a percent-encoded password, a percent-encoded `options` value) reaches
  SQLAlchemy unchanged. Before, configparser interpolation refused it ("invalid interpolation syntax").
- e2e-1 D2: `alembic check` (and autogenerate) ignore the tables the Rust service creates outside alembic
  (env.py `RUST_OWNED_TABLES`), together with their indexes, and still report real drift.
"""

from __future__ import annotations

import os
import subprocess
import sys
from uuid import uuid4

import pytest
from sqlalchemy import create_engine, make_url, text

from .conftest import _PG_REASON, API_DIR, TEST_DB

RUST_OWNED_SQL = (
    API_DIR.parents[1]
    / "apps"
    / "backend"
    / "crates"
    / "server"
    / "migrations"
    / "rust-owned"
    / "nessie_simulation_records.sql"
)
# Used only when the Rust service's SQL file is absent: a table with the same name, a foreign key and an index.
STAND_IN_SQL = """
CREATE TABLE IF NOT EXISTS nessie_simulation_records (
    id UUID NOT NULL,
    project_id UUID NOT NULL,
    approval_id UUID,
    data JSONB NOT NULL,
    CONSTRAINT pk_nessie_simulation_records PRIMARY KEY (id),
    CONSTRAINT fk_nessie_simulation_records_project_id_projects FOREIGN KEY(project_id)
        REFERENCES projects (id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS ix_nessie_simulation_records_approval_id ON nessie_simulation_records (approval_id);
"""
NO_CHANGES = "No new upgrade operations detected."


@pytest.fixture
def schema():
    if _PG_REASON:
        pytest.skip(_PG_REASON)
    name = f"alembic_env_{uuid4().hex[:12]}"
    eng = create_engine(TEST_DB)
    with eng.begin() as conn:
        conn.execute(text(f"CREATE SCHEMA {name}"))
    try:
        yield name
    finally:
        with eng.begin() as conn:
            conn.execute(text(f"DROP SCHEMA IF EXISTS {name} CASCADE"))
        eng.dispose()


def _with_options(url: str, schema: str, *, encoded: bool) -> str:
    """`url` with `options=-csearch_path=<schema>`, the '=' percent-encoded as %3D when `encoded`."""
    value = f"-csearch_path%3D{schema}" if encoded else f"-csearch_path={schema}"
    return f"{url}{'&' if '?' in url else '?'}options={value}"


def _password_encoded(url: str) -> str:
    """The same URL with every byte of its password percent-encoded (it decodes to the same password)."""
    password = make_url(url).password
    if not password:
        pytest.skip("TEST_DATABASE_URL has no password to encode")
    scheme, rest = url.split("://", 1)
    userinfo, location = rest.rsplit("@", 1)
    user = userinfo.split(":", 1)[0]
    return f"{scheme}://{user}:{''.join(f'%{b:02X}' for b in password.encode())}@{location}"


def _alembic(url: str, *args: str) -> subprocess.CompletedProcess:
    """The CLI exactly as CI runs it (`alembic upgrade head && alembic check`), with DATABASE_URL = `url`."""
    env = {**os.environ, "DATABASE_URL": url, "ALEMBIC_SKIP_LOGGING": "1"}
    return subprocess.run(
        [sys.executable, "-m", "alembic", *args],
        cwd=API_DIR,
        env=env,
        capture_output=True,
        text=True,
        timeout=120,
    )


def _output(p: subprocess.CompletedProcess) -> str:
    """alembic 1.20 prints a command's `FAILED: ...` on stdout (and exits 255), tracebacks on stderr."""
    return p.stdout + p.stderr


def _tables(schema: str) -> set[str]:
    eng = create_engine(TEST_DB)
    try:
        with eng.connect() as conn:
            rows = conn.execute(
                text("SELECT table_name FROM information_schema.tables WHERE table_schema = :s"),
                {"s": schema},
            )
            return {r[0] for r in rows}
    finally:
        eng.dispose()


def _execute(schema: str, sql: str) -> None:
    eng = create_engine(TEST_DB)
    try:
        with eng.begin() as conn:
            conn.execute(text(f"SET LOCAL search_path TO {schema}"))
            conn.exec_driver_sql(sql)
    finally:
        eng.dispose()


def _migrated(schema: str) -> str:
    """Upgrade the private schema to head; returns its URL."""
    url = _with_options(TEST_DB, schema, encoded=False)
    up = _alembic(url, "upgrade", "head")
    assert up.returncode == 0, _output(up)
    return url


def _add_rust_owned_table(schema: str) -> None:
    _execute(schema, RUST_OWNED_SQL.read_text() if RUST_OWNED_SQL.exists() else STAND_IN_SQL)
    assert "nessie_simulation_records" in _tables(schema)


def _model_tables() -> set[str]:
    from app.db import models  # noqa: F401  (register tables)
    from app.db.base import Base

    return set(Base.metadata.tables)


# ---- D3: a DATABASE_URL with '%' -------------------------------------------------------------------------------


@pytest.mark.parametrize(
    "password_encoded, options_encoded",
    [(True, False), (False, True), (True, True)],
    ids=["encoded-password", "encoded-options", "both"],
)
def test_upgrade_takes_a_database_url_with_percent_signs(schema, password_encoded, options_encoded):
    base = _password_encoded(TEST_DB) if password_encoded else TEST_DB
    url = _with_options(base, schema, encoded=options_encoded)
    if password_encoded:
        assert "%" in url.split("?", 1)[0]
    if options_encoded:
        assert "%3D" in url

    up = _alembic(url, "upgrade", "head")
    # Before the fix: ValueError: invalid interpolation syntax in 'postgresql+psycopg://...' at position N
    assert up.returncode == 0, _output(up)
    # The decoded `options` took effect: every table is in the private schema.
    assert _tables(schema) == _model_tables() | {"alembic_version"}
    current = _alembic(url, "current")
    assert current.returncode == 0 and "(head)" in current.stdout, _output(current)
    check = _alembic(url, "check")
    assert check.returncode == 0 and NO_CHANGES in check.stdout, _output(check)


def test_the_url_reaches_sqlalchemy_unchanged(schema, monkeypatch):
    """The local server trusts socket connections, so a wrong password would not be refused. Prove instead that
    the URL env.py hands to SQLAlchemy is DATABASE_URL itself, byte for byte (alembic in process)."""
    from alembic.config import Config

    from alembic import command

    url = _with_options(_password_encoded(TEST_DB), schema, encoded=True)
    monkeypatch.setenv("DATABASE_URL", url)
    monkeypatch.setenv("ALEMBIC_SKIP_LOGGING", "1")
    cfg = Config(str(API_DIR / "alembic.ini"))
    command.upgrade(cfg, "head")
    handed = cfg.get_main_option("sqlalchemy.url")  # what engine_from_config and offline mode read back
    assert handed == url
    assert make_url(handed).password == make_url(TEST_DB).password
    assert make_url(handed).query["options"] == f"-csearch_path={schema}"
    assert "alembic_version" in _tables(schema)


def test_offline_sql_takes_a_database_url_with_percent_signs():
    url = _with_options(TEST_DB, "no_such_schema", encoded=True)  # offline mode never connects
    out = _alembic(url, "upgrade", "head", "--sql")
    assert out.returncode == 0, _output(out)
    assert "CREATE TABLE projects" in out.stdout


# ---- D2: tables the Rust service owns --------------------------------------------------------------------------


def test_check_ignores_the_tables_the_rust_service_owns(schema):
    url = _migrated(schema)
    before = _alembic(url, "check")
    assert before.returncode == 0 and NO_CHANGES in before.stdout, _output(before)

    _add_rust_owned_table(schema)  # what every Rust start-up does
    after = _alembic(url, "check")
    # Before the fix (exit 255): FAILED: New upgrade operations detected: [('remove_index', Index(
    #   'ix_nessie_simulation_records_approval_id', ...)), ('remove_table', Table('nessie_simulation_records', ...))]
    assert after.returncode == 0, _output(after)
    assert NO_CHANGES in after.stdout
    # Applying the head again leaves the Rust-owned table alone.
    assert _alembic(url, "upgrade", "head").returncode == 0
    assert "nessie_simulation_records" in _tables(schema)


@pytest.mark.parametrize(
    "drift, reported",
    [
        ("ALTER TABLE projects ADD COLUMN drift_probe integer", ("remove_column", "drift_probe")),
        ("DROP INDEX ix_projects_owner_id", ("add_index", "ix_projects_owner_id")),
        (
            # Only exact names are ignored: a table whose name merely starts with a Rust-owned name is reported.
            "CREATE TABLE nessie_simulation_records_copy (LIKE nessie_simulation_records INCLUDING ALL)",
            ("remove_table", "nessie_simulation_records_copy"),
        ),
    ],
    ids=["extra-column", "missing-index", "similar-table-name"],
)
def test_check_still_reports_real_drift(schema, drift, reported):
    url = _migrated(schema)
    _add_rust_owned_table(schema)
    _execute(schema, drift)
    out = _alembic(url, "check")
    text_out = _output(out)
    assert out.returncode != 0, text_out
    assert "FAILED: New upgrade operations detected" in text_out
    for word in reported:
        assert word in text_out, text_out
    assert (
        "Table('nessie_simulation_records'," not in text_out
    )  # the Rust-owned table itself is still ignored


def test_a_rust_owned_name_is_compared_once_the_python_models_define_that_table(schema, monkeypatch):
    """If an alembic revision adopts a Rust-owned table (a model with that name), the ignore list must not hide drift
    between the model and the database, even before someone removes the name from the list."""
    from alembic.config import Config
    from alembic.util import AutogenerateDiffsDetected
    from sqlalchemy import Column, Table, Uuid

    from alembic import command
    from app.db.base import Base

    url = _migrated(schema)
    _add_rust_owned_table(schema)
    adopted = Table("nessie_simulation_records", Base.metadata, Column("id", Uuid, primary_key=True))
    try:
        monkeypatch.setenv("DATABASE_URL", url)
        monkeypatch.setenv("ALEMBIC_SKIP_LOGGING", "1")
        with pytest.raises(AutogenerateDiffsDetected) as diffs:
            command.check(Config(str(API_DIR / "alembic.ini")))
        assert "nessie_simulation_records" in str(diffs.value)  # its other columns and its index are reported
    finally:
        Base.metadata.remove(adopted)
    assert "nessie_simulation_records" not in _model_tables()
