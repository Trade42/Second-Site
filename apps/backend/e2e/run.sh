#!/usr/bin/env bash
# End-to-end harness, part 1: the Rust service (second-site-backend) and the Python app (uvicorn app.main:app)
# running side by side over real loopback sockets, on one private PostgreSQL schema, with fixture providers only.
#
# Usage: bash apps/backend/e2e/run.sh [--keep] [--no-build] [--migrate-with=alembic|rust] [--driver=PATH]
#   --keep               leave the private schema in secondsite_rust_test for inspection (it is printed)
#   --no-build           skip the cargo build and use the last release binary that built
#   --migrate-with=X     who applies the schema on the empty private schema: alembic (default) or rust
#   --driver=PATH        run another copy of driver.py (used to prove that a broken expectation FAILs)
#
# What it starts: exactly two processes, both bound to 127.0.0.1 on free ports chosen at run time, both
# started with `env -i` (nothing inherited from the caller's environment), with every setting passed
# explicitly, from an empty working directory created under the run directory, so neither process can find a
# `.env` or `../../.env` file (both read those relative to their working directory). Both are stopped by
# this script in every exit path (EXIT/INT/TERM/HUP trap), and the schema is dropped unless --keep.
#
# Exit code: 0 when every check passed or skipped; 1 when a check failed; 2 when the harness itself could not
# set up or tear down (build, database, start-up); the driver's code otherwise.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
E2E="$REPO/apps/backend/e2e"
PY="$REPO/apps/api/.venv/bin/python"
O="$REPO/orchestrator"
TARGET="${CARGO_TARGET_DIR:-$O/runtime/rust/target-${SECOND_SITE_ROLE:-shared}}"
BIN_SRC="$TARGET/release/second-site-backend"
DB=secondsite_rust_test
SOCK=/tmp/ss-pg

KEEP=0
BUILD=1
MIGRATE_WITH=alembic
DRIVER="$E2E/driver.py"
for arg in "$@"; do
  case "$arg" in
    --keep) KEEP=1 ;;
    --no-build) BUILD=0 ;;
    --migrate-with=alembic) MIGRATE_WITH=alembic ;;
    --migrate-with=rust) MIGRATE_WITH=rust ;;
    --driver=*) DRIVER="${arg#--driver=}" ;;
    -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown option: $arg (see --help)" >&2; exit 2 ;;
  esac
done
DRIVER="$(cd "$(dirname "$DRIVER")" && pwd)/$(basename "$DRIVER")"

RUN="$(mktemp -d "${TMPDIR:-/tmp}/ss-e2e.XXXXXX")"
CWD="$RUN/cwd/a/b"          # ./.env -> $RUN/cwd/a/b/.env, ../../.env -> $RUN/cwd/.env: neither exists
mkdir -p "$CWD" "$RUN/home" "$RUN/bin" "$RUN/objects-python" "$RUN/objects-rust"

say() { printf '[e2e] %s\n' "$*" | tee -a "$RUN/harness.log"; }

RS_PID=""
PY_PID=""
SCHEMA=""
SCHEMA_CREATED=0
TORN_DOWN=0

stop_proc() { # pid name
  local pid="$1" name="$2" i
  [ -n "$pid" ] || return 0
  if kill -0 "$pid" 2>/dev/null; then
    kill -TERM "$pid" 2>/dev/null
    for i in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$pid" 2>/dev/null; then
      say "$name (pid $pid) did not stop within 10 s of SIGTERM; sending SIGKILL"
      kill -KILL "$pid" 2>/dev/null
    fi
  fi
  wait "$pid" 2>/dev/null
  if kill -0 "$pid" 2>/dev/null; then say "ERROR: $name (pid $pid) is still running"; return 1; fi
  say "$name stopped (pid $pid)"
}

teardown() {
  local rc=0
  [ "$TORN_DOWN" = 1 ] && return 0
  TORN_DOWN=1
  stop_proc "$PY_PID" "python app" || rc=2
  stop_proc "$RS_PID" "rust service" || rc=2
  PY_PID="" RS_PID=""
  if [ "$SCHEMA_CREATED" = 1 ]; then
    if [ "$KEEP" = 1 ]; then
      say "--keep: schema $SCHEMA left in $DB (drop it with: $PY -I $E2E/pgschema.py drop $DB $SCHEMA)"
    elif "$PY" -I "$E2E/pgschema.py" drop "$DB" "$SCHEMA" >>"$RUN/harness.log" 2>&1; then
      say "schema $SCHEMA dropped"
    else
      say "ERROR: could not drop schema $SCHEMA (see $RUN/harness.log)"
      rc=2
    fi
  fi
  rm -f "$RUN/bin/second-site-backend"
  return "$rc"
}

on_exit() {
  local rc=$?
  trap - EXIT INT TERM HUP
  teardown || { [ "$rc" = 0 ] && rc=2; }
  say "run directory (logs): $RUN"
  exit "$rc"
}
trap on_exit EXIT
trap 'say "interrupted (SIGINT)"; exit 130' INT
trap 'say "terminated (SIGTERM)"; exit 143' TERM
trap 'say "hangup (SIGHUP)"; exit 129' HUP

fatal() { say "SETUP FAILED: $*"; exit 2; }

say "repository $REPO; run directory $RUN; migrate with $MIGRATE_WITH; driver $DRIVER"
[ -x "$PY" ] || fatal "no Python at $PY"
[ -S "$SOCK/.s.PGSQL.5432" ] || fatal "no PostgreSQL socket at $SOCK (bash orchestrator/scripts/testdb.sh status); this harness never starts one"
[ ! -e "$CWD/.env" ] && [ ! -e "$RUN/cwd/.env" ] || fatal "a .env file exists where a process would read it"

# ---- 1. the binary -------------------------------------------------------------------------------------------
BIN_ORIGIN=""
if [ "$BUILD" = 1 ]; then
  for attempt in 1 2 3 4 5 6; do
    say "cargo build --release -p ss-server (attempt $attempt of 6; log $RUN/build.log)"
    if "$O/scripts/cargo.sh" build --release -p ss-server >>"$RUN/build.log" 2>&1; then
      BIN_ORIGIN="built by this run (attempt $attempt)"
      break
    fi
    say "build failed: $(grep -m1 -E '^error' "$RUN/build.log" || tail -n1 "$RUN/build.log")"
    if [ "$attempt" -lt 6 ]; then say "waiting 120 s for the other writers (retry $attempt of 5)"; sleep 120; fi
  done
  if [ -z "$BIN_ORIGIN" ]; then
    [ -x "$BIN_SRC" ] || fatal "the crate does not compile and no earlier release binary exists at $BIN_SRC"
    BIN_ORIGIN="FALLBACK: the crate did not compile after 6 attempts; using the last binary that built"
  fi
else
  [ -x "$BIN_SRC" ] || fatal "--no-build but no release binary at $BIN_SRC"
  BIN_ORIGIN="--no-build: the last release binary that built"
fi
cp "$BIN_SRC" "$RUN/bin/second-site-backend" || fatal "could not copy $BIN_SRC"
BIN="$RUN/bin/second-site-backend"   # a private copy: another writer's rebuild cannot swap it mid-run
say "binary: $BIN_ORIGIN; built $(date -u -r "$BIN_SRC" '+%Y-%m-%dT%H:%M:%SZ'); sha256 $(sha256sum "$BIN" | cut -c1-16)...; HEAD $(git -C "$REPO" rev-parse --short HEAD) with $(git -C "$REPO" status --porcelain -- apps/backend/crates | wc -l) uncommitted paths under apps/backend/crates"

# ---- 2. private schema, ports, secrets -----------------------------------------------------------------------
SCHEMA="e2e_$("$PY" -I -c 'import secrets; print(secrets.token_hex(6))')"
"$PY" -I "$E2E/pgschema.py" tables "$DB" public >"$RUN/public-tables-before.txt" 2>>"$RUN/harness.log" \
  || fatal "cannot reach database $DB over $SOCK"
"$PY" -I "$E2E/pgschema.py" create "$DB" "$SCHEMA" >>"$RUN/harness.log" 2>&1 || fatal "could not create schema $SCHEMA"
SCHEMA_CREATED=1
say "private schema $SCHEMA created in $DB"

read -r RS_PORT PY_PORT < <("$PY" -I -c '
import socket
socks = [socket.socket() for _ in range(2)]
for s in socks:
    s.bind(("127.0.0.1", 0))
print(*[s.getsockname()[1] for s in socks])
for s in socks:
    s.close()')
[ -n "${RS_PORT:-}" ] && [ -n "${PY_PORT:-}" ] || fatal "could not pick free ports"
RS_URL="http://127.0.0.1:$RS_PORT"
PY_URL="http://127.0.0.1:$PY_PORT"
AUTH_SECRET="e2e-$("$PY" -I -c 'import secrets; print(secrets.token_hex(24))')"
PHOTON_SECRET="whsec_$("$PY" -I -c 'import base64, secrets; print(base64.b64encode(secrets.token_bytes(32)).decode())')"

# One DATABASE_URL value for both processes (Rust strips the SQLAlchemy `+psycopg` suffix itself). `options`
# pins search_path to the private schema for libpq (psycopg) and for sqlx alike; application_name lets the
# drop step end this run's connections only. The `=` inside the options value is deliberately NOT
# percent-encoded: apps/api/alembic/env.py hands DATABASE_URL to configparser, which rejects any `%`
# ("invalid interpolation syntax"), so a percent-encoded URL cannot be migrated by alembic (see README).
DATABASE_URL="postgresql+psycopg://secondsite:secondsite@localhost/$DB?host=$SOCK&options=-csearch_path=$SCHEMA&application_name=$SCHEMA"

COMMON=(
  "PATH=/usr/bin:/bin"
  "HOME=$RUN/home"
  "LANG=C.UTF-8"
  "APP_ENV=test"
  "LOG_LEVEL=INFO"
  "AUTH_SECRET=$AUTH_SECRET"
  "ALLOW_DEV_TOKENS=true"
  "DATABASE_URL=$DATABASE_URL"
  "TASK_EXECUTION_MODE=inline"
  "SUPERVISOR_MODEL="
  "AGENT_CHECKPOINTER=memory"
  "PROPERTY_PROVIDER_MODE=fixture"
  "PERMIT_PROVIDER_MODE=fixture"
  "MATTERPORT_PROVIDER_MODE=fixture"
  "VEXCEL_PROVIDER_MODE=fixture"
  "MESSAGING_PROVIDER_MODE=fixture"
  "VOICE_PROVIDER_MODE=fixture"
  "PROCUREMENT_PROVIDER_MODE=fixture"
  "PHOTON_WEBHOOK_SECRET=$PHOTON_SECRET"
  "OBJECT_STORAGE_MODE=local"
  "ALLOW_LIVE_OUTBOUND_CALLS=false"
  "ALLOW_LIVE_PURCHASES=false"
  "PUBLIC_BASE_URL=$RS_URL"
  "PUBLIC_WEB_URL=http://127.0.0.1:3000"
  "CORS_ORIGINS=http://127.0.0.1:3000"
)
PY_ENV=("${COMMON[@]}" "BACKEND_API_URL=$RS_URL" "OBJECT_STORAGE_LOCAL_DIR=$RUN/objects-python")
RS_ENV=("${COMMON[@]}" "HOST=127.0.0.1" "PORT=$RS_PORT" "PYTHON_SERVICE_URL=$PY_URL" "STORE_BACKEND=postgres"
  "FIXTURES_DIR=$REPO/fixtures" "OBJECT_STORAGE_LOCAL_DIR=$RUN/objects-rust")
ALEMBIC_ENV=("${COMMON[@]}" "PYTHONPATH=$REPO/apps/api")

# The environment of each process, with secret-shaped values masked (for the report).
mask() { sed -E 's/^(AUTH_SECRET|PHOTON_WEBHOOK_SECRET)=.*/\1=<generated per run, not printed>/'; }
printf '%s\n' "${RS_ENV[@]}" | mask >"$RUN/env-rust.txt"
printf '%s\n' "${PY_ENV[@]}" | mask >"$RUN/env-python.txt"
say "rust service $RS_URL, python app $PY_URL (environments: $RUN/env-rust.txt, $RUN/env-python.txt)"

# ---- 3. the schema, applied once -----------------------------------------------------------------------------
alembic() { (cd "$CWD" && exec env -i "${ALEMBIC_ENV[@]}" "$PY" -B -m alembic -c "$REPO/apps/api/alembic.ini" "$@"); }
n=$("$PY" -I "$E2E/pgschema.py" tables "$DB" "$SCHEMA" | wc -l)
[ "$n" = 0 ] || fatal "the new schema $SCHEMA is not empty ($n tables)"
say "schema $SCHEMA is empty (0 tables); alembic current before: '$(alembic current 2>/dev/null | tr -d '\n')'"
if [ "$MIGRATE_WITH" = alembic ]; then
  alembic upgrade head >"$RUN/alembic-upgrade.log" 2>&1 || fatal "alembic upgrade head failed (see $RUN/alembic-upgrade.log)"
  say "schema applied by alembic: $(grep -c 'Running upgrade' "$RUN/alembic-upgrade.log") revisions ($(grep -o 'Running upgrade.*' "$RUN/alembic-upgrade.log" | tr '\n' ';'))"
else
  (cd "$CWD" && exec env -i "${RS_ENV[@]}" "$BIN" migrate) >"$RUN/rust-migrate.log" 2>&1 \
    || fatal "second-site-backend migrate failed (see $RUN/rust-migrate.log)"
  say "schema applied by the Rust binary: $(grep -o '"msg": "[^"]*"[^}]*' "$RUN/rust-migrate.log" | tail -n1)"
  alembic upgrade head >"$RUN/alembic-upgrade.log" 2>&1 || fatal "alembic upgrade head after the Rust migration failed"
  n=$(grep -c 'Running upgrade' "$RUN/alembic-upgrade.log")
  [ "$n" = 0 ] || fatal "alembic applied $n revisions on top of the Rust migration (expected none)"
  say "alembic upgrade head afterwards: nothing to do (0 revisions applied)"
fi
say "alembic current: $(alembic current 2>/dev/null | tr '\n' ' ')"
if alembic check >"$RUN/alembic-check.log" 2>&1; then
  say "alembic check: $(tail -n1 "$RUN/alembic-check.log")"
else
  say "alembic check reports a difference between the models and the schema: $(tail -n1 "$RUN/alembic-check.log")"
fi
say "tables in $SCHEMA: $("$PY" -I "$E2E/pgschema.py" tables "$DB" "$SCHEMA" | wc -l)"

# ---- 4. start both processes ---------------------------------------------------------------------------------
(cd "$CWD" && exec env -i "${RS_ENV[@]}" "$BIN") >"$RUN/rust.log" 2>&1 &
RS_PID=$!
(cd "$CWD" && exec env -i "${PY_ENV[@]}" "$PY" -I -B -m uvicorn --app-dir "$REPO/apps/api" \
  --host 127.0.0.1 --port "$PY_PORT" --log-level info app.main:app) >"$RUN/python.log" 2>&1 &
PY_PID=$!
say "started rust service (pid $RS_PID) and python app (pid $PY_PID)"

wait_health() { # name url pid log seconds
  local name="$1" url="$2" pid="$3" log="$4" secs="$5" i
  for i in $(seq 1 $((secs * 10))); do
    if curl -sf --noproxy '*' -o /dev/null --max-time 1 "$url/health"; then
      say "$name healthy after ~$((i / 10)).$((i % 10)) s"
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      say "$name exited before it was healthy; last log lines:"
      tail -n 20 "$log" | tee -a "$RUN/harness.log"
      return 1
    fi
    sleep 0.1
  done
  say "$name not healthy after $secs s"
  return 1
}
wait_health "rust service" "$RS_URL" "$RS_PID" "$RUN/rust.log" 30 || fatal "rust service did not start"
wait_health "python app" "$PY_URL" "$PY_PID" "$RUN/python.log" 90 || fatal "python app did not start"
# Rust probes GET {PYTHON_SERVICE_URL}/health in the background (when its build has the forwarder); its first
# probe usually runs before the Python app is up. Wait (max 15 s) until rust's /ready reports the link LIVE.
link_status() {
  curl -sf --noproxy '*' --max-time 1 "$RS_URL/ready" | "$PY" -I -c 'import json, sys
d = json.load(sys.stdin)
print(next((p["status"] for p in d.get("providers", []) if p.get("provider") == "python_service"), "ABSENT"))' 2>/dev/null
}
for i in $(seq 1 150); do
  LINK="$(link_status)"
  if [ "$LINK" = LIVE ] || [ "$LINK" = ABSENT ]; then break; fi
  sleep 0.1
done
say "rust /ready python_service entry: ${LINK:-no answer} after ~$((i / 10)).$((i % 10)) s of waiting"
say "rust start-up migration: $(grep -o '"msg": "[^"]*\(alembic\|migrat\)[^"]*"' "$RUN/rust.log" | head -n1)"
if alembic check >"$RUN/alembic-check-after-rust.log" 2>&1; then
  say "alembic check after the rust start-up: $(tail -n1 "$RUN/alembic-check-after-rust.log")"
else
  say "alembic check after the rust start-up FAILS: $(tail -n1 "$RUN/alembic-check-after-rust.log")"
fi
say "tables in $SCHEMA after the rust start-up: $("$PY" -I "$E2E/pgschema.py" tables "$DB" "$SCHEMA" | wc -l)"

# ---- 5. the driver -------------------------------------------------------------------------------------------
DRV_ENV=(
  "PATH=/usr/bin:/bin" "HOME=$RUN/home" "LANG=C.UTF-8"
  "E2E_REPO=$REPO" "E2E_RUST_URL=$RS_URL" "E2E_PY_URL=$PY_URL"
  "E2E_RUST_LOG=$RUN/rust.log" "E2E_PY_LOG=$RUN/python.log"
  "E2E_AUTH_SECRET=$AUTH_SECRET" "E2E_PHOTON_SECRET=$PHOTON_SECRET"
  "E2E_RESULTS=$RUN/results.json" "E2E_SCHEMA=$SCHEMA" "E2E_DATABASE_URL=$DATABASE_URL"
)
say "driver: $DRIVER"
(cd "$CWD" && exec env -i "${DRV_ENV[@]}" timeout 900 "$PY" -I -B "$DRIVER") 2>&1 | tee "$RUN/driver.log"
DRIVER_RC=${PIPESTATUS[0]}
say "driver exit code $DRIVER_RC"

# ---- 6. stop, drop, prove nothing is left --------------------------------------------------------------------
"$PY" -I "$E2E/pgschema.py" tables "$DB" public >"$RUN/public-tables-after.txt" 2>>"$RUN/harness.log"
if cmp -s "$RUN/public-tables-before.txt" "$RUN/public-tables-after.txt"; then
  say "public schema of $DB: table list unchanged ($(wc -l <"$RUN/public-tables-after.txt") tables)"
else
  say "ERROR: the table list of $DB.public changed during the run"
  DRIVER_RC=2
fi
RS_SAVED=$RS_PID
PY_SAVED=$PY_PID
teardown || DRIVER_RC=2
for p in "$RS_SAVED" "$PY_SAVED"; do
  if kill -0 "$p" 2>/dev/null; then say "ERROR: pid $p still alive"; DRIVER_RC=2; fi
done
say "processes left: $(pgrep -af "$RUN/bin/second-site-backend|uvicorn .*--port $PY_PORT" | grep -v pgrep | wc -l)"
LEFT=$("$PY" -I "$E2E/pgschema.py" list secondsite_rust_test secondsite_test)
printf '%s\n' "$LEFT" | while read -r line; do say "schemas $line"; done
if [ "$KEEP" = 0 ] && printf '%s\n' "$LEFT" | grep -qw "$SCHEMA"; then
  say "ERROR: schema $SCHEMA still exists"
  DRIVER_RC=2
fi
exit "$DRIVER_RC"
