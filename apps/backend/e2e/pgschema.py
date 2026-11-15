"""Private-schema helper for the e2e harness (run with the Python of apps/api/.venv, -I).

Usage (the database is E2E_PG_URL, a PostgreSQL URL: TCP with a password, or a Unix socket via ?host=/dir):
  pgschema.py create <schema>      create an empty schema (refuses an existing one)
  pgschema.py drop <schema>        end this run's connections (application_name = schema), drop it
  pgschema.py tables <schema>      table names in a schema (or in `public`, read only), one per line
  pgschema.py list                 non-system schemas of the database, one line
  pgschema.py dbname               the database name of E2E_PG_URL
  pgschema.py url <schema>         the services' DATABASE_URL: +psycopg, search_path and application_name set

Schema names must look like `e2e_<hex>`, so this helper can never create or drop `public` or another
session's schema.
"""

from __future__ import annotations

import os
import re
import sys
from urllib.parse import urlsplit, urlunsplit

import psycopg

SCHEMA_RE = re.compile(r"e2e_[0-9a-f]{12}")


def _url() -> str:
    url = os.environ.get("E2E_PG_URL", "")
    if urlsplit(url).scheme not in ("postgres", "postgresql"):
        raise SystemExit("E2E_PG_URL must be a postgresql:// URL")
    return url


def _connect() -> psycopg.Connection:
    return psycopg.connect(_url(), autocommit=True, application_name="e2e-admin")


def _schema(name: str) -> str:
    if not SCHEMA_RE.fullmatch(name):
        raise SystemExit(f"refusing schema name {name!r} (expected e2e_<12 hex>)")
    return name


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    cmd = argv[0]
    if cmd == "dbname":
        print(urlsplit(_url()).path.lstrip("/") or os.environ.get("USER", ""))
        return 0
    if cmd == "url":
        u = urlsplit(_url())
        # The values are not percent-encoded on purpose (as before): libpq, sqlx and alembic all read them as is.
        extra = f"options=-csearch_path={_schema(argv[1])}&application_name={argv[1]}"
        query = f"{u.query}&{extra}" if u.query else extra
        print(urlunsplit(("postgresql+psycopg", u.netloc, u.path, query, "")))
        return 0
    if cmd == "list":
        with _connect() as c:
            rows = c.execute(
                "SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' "
                "AND nspname <> 'information_schema' ORDER BY nspname"
            ).fetchall()
            db = c.info.dbname
        print(f"{db}: " + " ".join(r[0] for r in rows))
        return 0
    # Reading the table list of `public` is allowed (the harness proves it never writes there); nothing else is.
    schema = argv[1] if (cmd == "tables" and argv[1] == "public") else _schema(argv[1])
    with _connect() as c:
        if cmd == "create":
            c.execute(f'CREATE SCHEMA "{schema}"')
        elif cmd == "drop":
            c.execute(
                "SELECT pg_terminate_backend(pid) FROM pg_stat_activity "
                "WHERE pid <> pg_backend_pid() AND application_name = %s",
                (schema,),
            )
            c.execute(f'DROP SCHEMA IF EXISTS "{schema}" CASCADE')
            left = c.execute("SELECT count(*) FROM pg_namespace WHERE nspname = %s", (schema,)).fetchone()
            if left and left[0]:
                print(f"schema {schema} still exists", file=sys.stderr)
                return 1
        elif cmd == "tables":
            rows = c.execute(
                "SELECT tablename FROM pg_tables WHERE schemaname = %s ORDER BY tablename", (schema,)
            ).fetchall()
            for r in rows:
                print(r[0])
        else:
            print(__doc__, file=sys.stderr)
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
