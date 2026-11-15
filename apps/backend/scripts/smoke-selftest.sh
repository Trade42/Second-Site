#!/usr/bin/env bash
# Self-test of smoke.sh against a real release binary, twice:
#   1. in-memory store;
#   2. PostgreSQL, in a private schema created for this run in the test database and dropped
#      afterwards (the service is pointed at it with search_path in DATABASE_URL's options).
# Each pass starts the binary on 127.0.0.1 on a free port, in an empty working directory (so no
# .env file is read), with a clean environment in fixture mode (no live provider can be called),
# probes it with healthcheck.sh (the image's health command), runs smoke.sh, and stops it. A trap
# stops the service and drops the schema on every exit path.
#
# Usage: apps/backend/scripts/smoke-selftest.sh [--memory-only] [--bin PATH] [--smoke PATH]
#   --memory-only  skip the PostgreSQL pass (it is reported as skipped, not passed)
#   --bin PATH     test this binary instead of building one (cargo build --release --locked)
#   --smoke PATH   run another copy of smoke.sh (used to show that a broken assertion fails)
# Environment:
#   SMOKE_PG_URL  admin and service URL of the test database
#                 (default postgres://secondsite@localhost/secondsite_rust_test?host=/tmp/ss-pg)
#   PSQL          psql binary used to create and drop the schema (default: psql)
#   CARGO         cargo command used to build (default: cargo)
set -euo pipefail

SCRIPTS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKEND="$(cd "$SCRIPTS/.." && pwd)"
ROOT="$(cd "$BACKEND/../.." && pwd)"
PG_URL=${SMOKE_PG_URL:-postgres://secondsite@localhost/secondsite_rust_test?host=/tmp/ss-pg}
PSQL=${PSQL:-psql}

memory_only=0
smoke=$SCRIPTS/smoke.sh
BIN=
while [[ $# -gt 0 ]]; do
  case $1 in
    --memory-only) memory_only=1; shift ;;
    --bin) [[ $# -ge 2 ]] || { echo "--bin needs a path" >&2; exit 2; }; BIN=$2; shift 2 ;;
    --smoke) [[ $# -ge 2 ]] || { echo "--smoke needs a path" >&2; exit 2; }; smoke=$2; shift 2 ;;
    -h|--help) sed -n '2,/^[^#]/{/^#/p}' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) echo "unknown argument: $1 (see --help)" >&2; exit 2 ;;
  esac
done
[[ -f $smoke ]] || { echo "selftest: no smoke script at $smoke" >&2; exit 2; }

say() { printf 'selftest: %s\n' "$*"; }
TMP=$(mktemp -d)
SERVICE_PID=
SMOKE_PID=
SCHEMA=

stop_service() {
  [[ -n $SERVICE_PID ]] || return 0
  if kill -0 "$SERVICE_PID" 2>/dev/null; then
    kill -TERM "$SERVICE_PID" 2>/dev/null || true
    for _ in $(seq 1 50); do kill -0 "$SERVICE_PID" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$SERVICE_PID" 2>/dev/null; then
      say "service $SERVICE_PID did not stop on TERM; killing it"
      kill -KILL "$SERVICE_PID" 2>/dev/null || true
    fi
  fi
  wait "$SERVICE_PID" 2>/dev/null || true
  SERVICE_PID=
}
psql_admin() { "$PSQL" -X -q -v ON_ERROR_STOP=1 -At "$PG_URL" "$@"; }
drop_schema() {
  [[ -n $SCHEMA ]] || return 0
  if psql_admin -c "SET client_min_messages = warning" -c "DROP SCHEMA IF EXISTS $SCHEMA CASCADE"; then
    say "dropped schema $SCHEMA"
  else
    say "WARNING: could not drop schema $SCHEMA; drop it by hand"
  fi
  SCHEMA=
}
cleanup() {
  local rc=$?
  if [[ -n $SMOKE_PID ]]; then
    kill -TERM "$SMOKE_PID" 2>/dev/null || true
    wait "$SMOKE_PID" 2>/dev/null || true
  fi
  stop_service
  drop_schema
  rm -rf "$TMP"
  exit "$rc"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

free_port() {
  python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

if [[ -z $BIN ]]; then
  say "building the release binary"
  out=$(cd "$BACKEND" && "${CARGO:-cargo}" build --release --locked -p ss-server \
    --message-format=json-render-diagnostics) || { say "release build failed"; exit 1; }
  BIN=$(sed -n 's/.*"executable":"\([^"]*second-site-backend\)".*/\1/p' <<<"$out" | tail -n 1)
fi
[[ -f $BIN && -x $BIN ]] || { say "no executable binary at ${BIN:-(none reported by cargo)}"; exit 1; }
say "binary $BIN ($(stat -c %s "$BIN") bytes)"

# run_pass NAME [run-local.sh options]: start, probe, smoke, stop. DATABASE_URL comes from the caller.
run_pass() {
  local pass=$1; shift
  local port dir log
  port=$(free_port)
  dir=$TMP/$pass
  mkdir -p "$dir/run/work"
  log=$dir/service.log
  say "[$pass] starting on 127.0.0.1:$port (log: service output is shown only on failure)"
  # A clean environment: fixture providers only, development settings, a throw-away signing
  # secret, and a working directory without .env files.
  env -i PATH="$PATH" HOME="$dir" LANG=C.UTF-8 \
    APP_ENV=development LOG_LEVEL=WARNING ALLOW_DEV_TOKENS=true TASK_EXECUTION_MODE=inline \
    AUTH_SECRET="selftest-$RANDOM$RANDOM$RANDOM" CORS_ORIGINS=http://localhost:3000 \
    PROPERTY_PROVIDER_MODE=fixture PERMIT_PROVIDER_MODE=fixture MATTERPORT_PROVIDER_MODE=fixture \
    VEXCEL_PROVIDER_MODE=fixture VOICE_PROVIDER_MODE=fixture MESSAGING_PROVIDER_MODE=fixture \
    PROCUREMENT_PROVIDER_MODE=fixture OBJECT_STORAGE_LOCAL_DIR="$dir/objects" \
    HOST=127.0.0.1 PORT="$port" FIXTURES_DIR="$ROOT/fixtures" \
    ${DATABASE_URL:+DATABASE_URL="$DATABASE_URL"} \
    bash "$SCRIPTS/run-local.sh" --bin "$BIN" --workdir "$dir/run/work" "$@" >"$log" 2>&1 &
  SERVICE_PID=$!
  local up=0
  for _ in $(seq 1 100); do
    if ! kill -0 "$SERVICE_PID" 2>/dev/null; then break; fi
    if HEALTHCHECK_TIMEOUT=2 bash "$SCRIPTS/healthcheck.sh" 127.0.0.1 "$port" 2>/dev/null; then up=1; break; fi
    sleep 0.2
  done
  if [[ $up != 1 ]]; then
    say "[$pass] the service did not become healthy; its output:"
    sed 's/^/    /' "$log"
    return 1
  fi
  sed -n 's/^run-local: /    /p' "$log"
  say "[$pass] health probe (the image's HEALTHCHECK command): $(bash "$SCRIPTS/healthcheck.sh" 127.0.0.1 "$port" && echo healthy)"
  # In the background, so that a signal reaches the trap at once instead of after smoke.sh.
  local rc=0
  bash "$smoke" "http://127.0.0.1:$port" &
  SMOKE_PID=$!
  wait "$SMOKE_PID" || rc=$?
  SMOKE_PID=
  stop_service
  if [[ $rc != 0 ]]; then
    say "[$pass] smoke.sh exited $rc; last service output:"
    tail -n 20 "$log" | sed 's/^/    /'
    return "$rc"
  fi
  # The service logs Python-style JSON ("level": "ERROR", with a space).
  if grep -Eq '"level": ?"ERROR"' "$log"; then
    say "[$pass] the service logged errors:"
    grep -E '"level": ?"ERROR"' "$log" | sed 's/^/    /'
    return 1
  fi
  say "[$pass] PASSED; service stopped cleanly"
}

results=()
run_pass memory --memory || { say "memory pass FAILED"; exit 1; }
results+=("memory: PASSED")

if [[ $memory_only == 1 ]]; then
  results+=("postgres: SKIPPED (--memory-only)")
else
  command -v "$PSQL" >/dev/null || { say "psql not found (set PSQL=/path/to/psql or pass --memory-only)"; exit 1; }
  SCHEMA=smoke_$(python3 -I -c 'import uuid; print(uuid.uuid4().hex[:16])')
  psql_admin -c "CREATE SCHEMA $SCHEMA" || { SCHEMA=; say "could not create a schema at $PG_URL"; exit 1; }
  say "[postgres] private schema $SCHEMA in ${PG_URL%%\?*}"
  sep='?'; [[ $PG_URL == *\?* ]] && sep='&'
  DATABASE_URL="$PG_URL${sep}options=-c%20search_path%3D$SCHEMA" run_pass postgres \
    || { say "postgres pass FAILED"; exit 1; }
  head=$(psql_admin -c "SELECT version_num FROM $SCHEMA.alembic_version")
  layouts=$(psql_admin -c "SELECT count(*) FROM $SCHEMA.design_versions")
  elsewhere=$(psql_admin -c "SELECT count(*) FROM information_schema.tables WHERE table_name = 'design_versions' AND table_schema = 'public'")
  say "[postgres] schema $SCHEMA: alembic head $head, $layouts design versions stored; design_versions tables in public: $elsewhere"
  [[ $head == 0002 && $layouts -ge 1 ]] || { say "the run did not write the private schema"; exit 1; }
  drop_schema
  results+=("postgres: PASSED (schema dropped)")
fi

for r in "${results[@]}"; do say "$r"; done
say "SELFTEST PASSED"
