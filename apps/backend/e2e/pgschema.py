"""Private-schema helper for the e2e harness (run with apps/api/.venv/bin/python -I).

Usage:
  pgschema.py create <db> <schema>      create an empty schema (refuses an existing one)
  pgschema.py drop <db> <schema>        end this run's connections (application_name = schema), drop it
  pgschema.py tables <db> <schema>      table names in a schema (or in `public`, read only), one per line
  pgschema.py list <db> [<db> ...]      non-system schemas of each database, one line per database

Only the local test PostgreSQL over its Unix socket (/tmp/ss-pg). Schema names must look like `e2e_<hex>`, so
this helper can never create or drop `public` or another session's schema.
"""

from __future__ import annotations

import re
import sys

import psycopg

SOCKET_DIR = "/tmp/ss-pg"
ALLOWED_DBS = {"secondsite_rust_test", "secondsite_test"}
SCHEMA_RE = re.compile(r"e2e_[0-9a-f]{12}")


def _connect(db: str) -> psycopg.Connection:
    if db not in ALLOWED_DBS:
        raise SystemExit(f"refusing database {db!r}")
    return psycopg.connect(
        host=SOCKET_DIR, user="secondsite", dbname=db, autocommit=True, application_name="e2e-admin"
    )


def _schema(name: str) -> str:
    if not SCHEMA_RE.fullmatch(name):
        raise SystemExit(f"refusing schema name {name!r} (expected e2e_<12 hex>)")
    return name


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    cmd, db = argv[0], argv[1]
    if cmd == "list":
        for d in argv[1:]:
            with _connect(d) as c:
                rows = c.execute(
                    "SELECT nspname FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' "
                    "AND nspname <> 'information_schema' ORDER BY nspname"
                ).fetchall()
            print(f"{d}: " + " ".join(r[0] for r in rows))
        return 0
    # Reading the table list of `public` is allowed (the harness proves it never writes there); nothing else is.
    schema = argv[2] if (cmd == "tables" and argv[2] == "public") else _schema(argv[2])
    with _connect(db) as c:
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
