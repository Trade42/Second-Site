#!/usr/bin/env bash
# Developer convenience: the whole Second Site stack on THIS machine, without Docker.
# LOOPBACK ONLY (127.0.0.1), DEV TOKENS ON (ALLOW_DEV_TOKENS=true). NOT A DEPLOYMENT: no TLS, no
# process supervision, one generated AUTH_SECRET in a local file. Never expose these ports.
#
# Usage: apps/backend/scripts/dev-stack.sh start|stop|status|logs [SERVICE] [-f]
#   start   build the Rust release binary (cargo decides what needs rebuilding), apply the schema
#           with alembic, then start in the background, on 127.0.0.1:
#             python   the Python app (uvicorn app.main:app), port 8001, BACKEND_API_URL -> rust
#             rust     the Rust service (second-site-backend), port 8000, PYTHON_SERVICE_URL -> python
#             mockups  a static server for mockups/, port 8090, only when that folder exists
#             web      the Next.js app (apps/web, npm run dev), port 3000, only when its
#                      node_modules exist (this script never runs npm install)
#           Success is reported only when both services answer /health and the Rust /ready shows
#           python_service LIVE. On any failure everything this start launched is stopped again.
#   stop    stop exactly the processes this script started (pid file + a per-start marker in the
#           process environment), TERM then KILL after 10 s.
#   status  each service, its address, its pid, and the Rust /ready (database and every provider).
#   logs    the last 40 lines of each log, or of one SERVICE; -f follows it.
#
# Settings. Both services read the caller's environment first, then their .env files
# (`.env` and `../../.env` relative to their working directory: apps/api and apps/backend, so the
# repository .env), exactly as they always do. This script never reads, prints or copies a value
# from a .env file; it only checks which NAMES are set there. It fills in what a fresh machine
# needs, only where the name is set nowhere:
#   DATABASE_URL   DEV_STACK_DATABASE_URL wins if set; else DATABASE_URL from the environment or a
#                  .env file; else, when the socket /tmp/ss-pg exists, the sandbox PostgreSQL:
#                  database secondsite_rust_test, schema DEV_STACK_SCHEMA (default dev_stack,
#                  created if missing; search_path pinned through the URL). Otherwise it refuses.
#   AUTH_SECRET    else generated once into .data/dev-stack/auth_secret (mode 600, git-ignored)
#                  and given to both services, so a token from either verifies in the other.
#   TASK_EXECUTION_MODE=inline, APP_ENV=development, OBJECT_STORAGE_LOCAL_DIR=.data/dev-stack/objects.
# Always set by this script (a .env line for them has no effect): HOST, PORT, STORE_BACKEND=postgres,
# FIXTURES_DIR (unless set in the environment), PYTHON_SERVICE_URL, BACKEND_API_URL,
# PUBLIC_BASE_URL, PUBLIC_WEB_URL, ALLOW_DEV_TOKENS=true, and CORS_ORIGINS (the web app and the
# mockups, localhost and 127.0.0.1 spellings, plus any CORS_ORIGINS of the environment).
# Provider modes (PERMIT_PROVIDER_MODE=live, NESSIE_PROVIDER_MODE, ...) pass through untouched.
#   DEV_STACK_RUST_PORT, DEV_STACK_PYTHON_PORT, DEV_STACK_MOCKUPS_PORT, DEV_STACK_WEB_PORT
#                  default 8000, 8001, 8090, 3000.
#   DEV_STACK_STATE  state directory (default .data/dev-stack in the repository; must be git-ignored).
#   DEV_STACK_PYTHON python of apps/api's virtualenv (default apps/api/.venv/bin/python).
#   DEV_STACK_CARGO  cargo command (default: orchestrator/scripts/cargo.sh where it exists, else cargo).
# No sudo, no system packages, no Docker.
set -euo pipefail

SCRIPTS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKEND="$(cd "$SCRIPTS/.." && pwd)"
ROOT="$(cd "$BACKEND/../.." && pwd)"
STATE=${DEV_STACK_STATE:-$ROOT/.data/dev-stack}
PY=${DEV_STACK_PYTHON:-$ROOT/apps/api/.venv/bin/python}
RS_PORT=${DEV_STACK_RUST_PORT:-8000}
PY_PORT=${DEV_STACK_PYTHON_PORT:-8001}
MOCK_PORT=${DEV_STACK_MOCKUPS_PORT:-8090}
WEB_PORT=${DEV_STACK_WEB_PORT:-3000}
RS_URL="http://127.0.0.1:$RS_PORT"
PY_URL="http://127.0.0.1:$PY_PORT"
SERVICES=(python rust mockups web)
SOCKET_DIR=/tmp/ss-pg

say() { printf 'dev-stack: %s\n' "$*"; }
die() { printf 'dev-stack: %s\n' "$*" >&2; exit 2; }

port_of() { case $1 in rust) echo "$RS_PORT" ;; python) echo "$PY_PORT" ;; mockups) echo "$MOCK_PORT" ;; web) echo "$WEB_PORT" ;; esac; }

# The state directory holds the secret and the logs: it must not be committable.
prepare_state() {
  mkdir -p "$STATE/pids" "$STATE/logs" "$STATE/bin" "$STATE/objects"
  chmod 700 "$STATE"
  if git -C "$ROOT" rev-parse --git-dir >/dev/null 2>&1; then
    git -C "$ROOT" check-ignore -q "$STATE/auth_secret" \
      || die "refusing: $STATE is not git-ignored (it would hold AUTH_SECRET); use .data/dev-stack or add it to .gitignore"
  fi
}

# Which .env file (if any) sets NAME, by name only; values are never read into this script.
dotenv_files() { printf '%s\n' "$ROOT/.env" "$ROOT/apps/api/.env" "$ROOT/apps/backend/.env"; }
dotenv_has() {
  local name=$1 f
  while read -r f; do
    if [[ -f $f ]] && grep -Eiq "^[[:space:]]*(export[[:space:]]+)?${name}[[:space:]]*=[[:space:]]*[^[:space:]#]" "$f"; then
      printf '%s' "${f#"$ROOT"/}"
      return 0
    fi
  done < <(dotenv_files)
  return 1
}
# Where a setting comes from: "environment", ".env" (a file name), or nothing (exit 1).
source_of() {
  local name=$1 f
  if [[ -n ${!name:-} ]]; then echo environment; return 0; fi
  if f=$(dotenv_has "$name"); then echo "$f"; return 0; fi
  return 1
}

# Is the process PID one this script started (with the current start's marker)?
own_pid() {
  local pid=$1 run
  [[ $pid =~ ^[0-9]+$ ]] && kill -0 "$pid" 2>/dev/null || return 1
  run=$(cat "$STATE/run_id" 2>/dev/null) || return 1
  tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null | grep -qxF "DEV_STACK_RUN=$run"
}
pid_of() { cat "$STATE/pids/$1.pid" 2>/dev/null || true; }

port_free() { # 0 when nothing listens on or holds 127.0.0.1:PORT
  "$PY" -I -c 'import socket, sys
s = socket.socket()
# SO_REUSEADDR: a TIME_WAIT left by the last stop is not "taken"; a listener still is.
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(1)
finally:
    s.close()' "$1" 2>/dev/null || return 1
  ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

stop_one() { # service
  local svc=$1 pid
  pid=$(pid_of "$svc")
  [[ -n $pid ]] || return 0
  if own_pid "$pid"; then
    # setsid made the process its own group leader: stop the group (npm/next spawn children).
    if [[ $(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ') == "$pid" ]]; then
      kill -TERM -- "-$pid" 2>/dev/null || true
    else
      kill -TERM "$pid" 2>/dev/null || true
    fi
    for _ in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$pid" 2>/dev/null; then
      say "$svc (pid $pid) did not stop within 10 s; sending SIGKILL"
      kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
      sleep 0.5
    fi
    if kill -0 "$pid" 2>/dev/null; then say "ERROR: $svc (pid $pid) is still running"; return 1; fi
    say "$svc stopped (pid $pid)"
  elif kill -0 "$pid" 2>/dev/null; then
    say "$svc: pid $pid is alive but is not a process this script started; left alone"
  else
    say "$svc: not running (stale pid $pid)"
  fi
  rm -f "$STATE/pids/$svc.pid"
}

cmd_stop() {
  local rc=0 svc
  [[ -d $STATE/pids ]] || { say "nothing to stop (no state directory $STATE)"; return 0; }
  # Reverse start order: web, mockups, rust, python.
  for svc in web mockups rust python; do stop_one "$svc" || rc=1; done
  return "$rc"
}

# launch SERVICE CWD CMD...: start detached in its own session, environment marked, log in logs/.
launch() {
  local svc=$1 dir=$2
  shift 2
  (cd "$dir" && exec setsid "$@") </dev/null >>"$STATE/logs/$svc.log" 2>&1 &
  echo $! >"$STATE/pids/$svc.pid"
  STARTED+=("$svc")
}

wait_http() { # name url pid seconds [grep-pattern]
  local name=$1 url=$2 pid=$3 secs=$4 pat=${5-} body
  for _ in $(seq 1 $((secs * 10))); do
    if body=$(curl -sf --noproxy '*' --max-time 1 "$url" 2>/dev/null) && { [[ -z $pat ]] || grep -q "$pat" <<<"$body"; }; then
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || { say "$name exited during start-up; last log lines:"; tail -n 20 "$STATE/logs/$name.log" >&2; return 1; }
    sleep 0.1
  done
  say "$name did not answer $url within $secs s"
  return 1
}

python_link() {
  curl -sf --noproxy '*' --max-time 2 "$RS_URL/ready" 2>/dev/null | "$PY" -I -c 'import json, sys
d = json.load(sys.stdin)
print(next((p["status"] for p in d.get("providers", []) if p.get("provider") == "python_service"), "ABSENT"))' 2>/dev/null
}

abort_start() {
  local s
  say "start failed; stopping what this start launched"
  for s in "${STARTED[@]}"; do stop_one "$s" || true; done
}

cmd_start() {
  local svc pid src db_desc auth_desc cargo out bin run_id
  command -v curl >/dev/null || die "curl is required"
  command -v setsid >/dev/null || die "setsid (util-linux) is required"
  [[ -x $PY ]] || die "no Python at $PY: create apps/api's virtualenv (cd apps/api && uv sync) or set DEV_STACK_PYTHON"
  prepare_state
  for svc in "${SERVICES[@]}"; do
    pid=$(pid_of "$svc")
    if [[ -n $pid ]] && own_pid "$pid"; then die "already running ($svc, pid $pid): run '$0 stop' first"; fi
    rm -f "$STATE/pids/$svc.pid"
  done
  for svc in python rust; do
    port_free "$(port_of "$svc")" || die "refusing to start: port $(port_of "$svc") on 127.0.0.1 ($svc) is taken"
  done
  local want_mockups=0 want_web=0
  if [[ -d $ROOT/mockups ]]; then
    want_mockups=1
    port_free "$MOCK_PORT" || die "refusing to start: port $MOCK_PORT on 127.0.0.1 (mockups) is taken"
  fi
  if [[ -d $ROOT/apps/web/node_modules ]]; then
    want_web=1
    port_free "$WEB_PORT" || die "refusing to start: port $WEB_PORT on 127.0.0.1 (web) is taken"
  fi

  # ---- settings (names checked, values from .env files never read) ----
  if [[ -n ${DEV_STACK_DATABASE_URL:-} ]]; then
    export DATABASE_URL=$DEV_STACK_DATABASE_URL
    db_desc="DEV_STACK_DATABASE_URL (value not printed)"
  elif src=$(source_of DATABASE_URL); then
    db_desc="DATABASE_URL from $src (value not printed)"
  elif [[ -S $SOCKET_DIR/.s.PGSQL.5432 ]]; then
    local schema=${DEV_STACK_SCHEMA:-dev_stack}
    [[ $schema =~ ^[a-z_][a-z0-9_]{0,62}$ ]] || die "DEV_STACK_SCHEMA=$schema is not a plain lower-case identifier"
    local base="postgresql+psycopg://secondsite:secondsite@localhost/secondsite_rust_test?host=$SOCKET_DIR"
    DEV_STACK_ADMIN_URL=$base "$PY" -I -c 'import os, sys, psycopg
from psycopg import sql
with psycopg.connect(os.environ["DEV_STACK_ADMIN_URL"].replace("+psycopg", ""), autocommit=True) as c:
    c.execute(sql.SQL("CREATE SCHEMA IF NOT EXISTS {}").format(sql.Identifier(sys.argv[1])))' "$schema" \
      || die "cannot create schema $schema in secondsite_rust_test over $SOCKET_DIR"
    export DATABASE_URL="$base&options=-csearch_path=$schema&application_name=dev-stack-$schema"
    db_desc="sandbox PostgreSQL ($SOCKET_DIR), database secondsite_rust_test, schema $schema"
  else
    die "no database: set DEV_STACK_DATABASE_URL (or DATABASE_URL), for example
  DEV_STACK_DATABASE_URL=postgresql://secondsite:secondsite@localhost:5432/secondsite
(the sandbox socket $SOCKET_DIR does not exist here, and this script never starts a database)"
  fi
  if src=$(source_of AUTH_SECRET); then
    auth_desc="AUTH_SECRET from $src (value not printed)"
    [[ $src == environment || $src == .env ]] \
      || say "WARNING: AUTH_SECRET is set in $src, which only one of the two services reads; set it in the environment or the repository .env"
  else
    if [[ ! -s $STATE/auth_secret ]]; then
      (umask 077 && "$PY" -I -c 'import secrets; print(secrets.token_urlsafe(48))' >"$STATE/auth_secret")
    fi
    chmod 600 "$STATE/auth_secret"
    AUTH_SECRET=$(cat "$STATE/auth_secret")
    export AUTH_SECRET
    auth_desc="AUTH_SECRET generated, kept in ${STATE#"$ROOT"/}/auth_secret (mode 600)"
  fi
  source_of TASK_EXECUTION_MODE >/dev/null || export TASK_EXECUTION_MODE=inline
  source_of APP_ENV >/dev/null || export APP_ENV=development
  source_of OBJECT_STORAGE_LOCAL_DIR >/dev/null || export OBJECT_STORAGE_LOCAL_DIR=$STATE/objects
  export FIXTURES_DIR=${FIXTURES_DIR:-$ROOT/fixtures}
  export ALLOW_DEV_TOKENS=true
  export STORE_BACKEND=postgres
  local cors="http://localhost:$WEB_PORT,http://127.0.0.1:$WEB_PORT,http://localhost:$MOCK_PORT,http://127.0.0.1:$MOCK_PORT"
  export CORS_ORIGINS="${CORS_ORIGINS:+$CORS_ORIGINS,}$cors"
  export PUBLIC_BASE_URL=$RS_URL PUBLIC_WEB_URL="http://localhost:$WEB_PORT"
  run_id=$("$PY" -I -c 'import secrets; print(secrets.token_hex(8))')
  echo "$run_id" >"$STATE/run_id"
  export DEV_STACK_RUN=$run_id

  # ---- the Rust binary ----
  if [[ -n ${DEV_STACK_CARGO:-} ]]; then cargo=$DEV_STACK_CARGO
  elif [[ -x $ROOT/orchestrator/scripts/cargo.sh ]]; then cargo=$ROOT/orchestrator/scripts/cargo.sh
  else cargo=cargo; fi
  say "building the release binary ($cargo build --release --locked -p ss-server; log ${STATE#"$ROOT"/}/logs/build.log)"
  # shellcheck disable=SC2086  # DEV_STACK_CARGO may carry arguments
  out=$(cd "$BACKEND" && $cargo build --release --locked -p ss-server --message-format=json-render-diagnostics 2>>"$STATE/logs/build.log") \
    || die "the release build failed (see $STATE/logs/build.log)"
  bin=$(sed -n 's/.*"executable":"\([^"]*second-site-backend\)".*/\1/p' <<<"$out" | tail -n 1)
  [[ -n $bin && -x $bin ]] || die "cargo did not report the second-site-backend executable"
  cp "$bin" "$STATE/bin/second-site-backend.new" && mv -f "$STATE/bin/second-site-backend.new" "$STATE/bin/second-site-backend"
  bin=$STATE/bin/second-site-backend   # a private copy: a later rebuild cannot swap it under the running service

  STARTED=()
  trap abort_start ERR
  set -E

  # ---- schema first (the order the runbook requires), then the Python app, then Rust ----
  say "applying the schema: alembic upgrade head (log ${STATE#"$ROOT"/}/logs/alembic.log)"
  (cd "$ROOT/apps/api" && "$PY" -B -m alembic upgrade head) >>"$STATE/logs/alembic.log" 2>&1 \
    || { tail -n 15 "$STATE/logs/alembic.log" >&2; die "alembic upgrade head failed"; }

  BACKEND_API_URL=$RS_URL launch python "$ROOT/apps/api" "$PY" -B -m uvicorn app.main:app \
    --host 127.0.0.1 --port "$PY_PORT" --log-level info
  wait_http python "$PY_URL/health" "$(pid_of python)" 90 || false
  HOST=127.0.0.1 PORT=$RS_PORT PYTHON_SERVICE_URL=$PY_URL launch rust "$BACKEND" "$bin"
  wait_http rust "$RS_URL/health" "$(pid_of rust)" 60 || false
  local link=
  for _ in $(seq 1 100); do
    link=$(python_link || true)
    [[ $link == LIVE ]] && break
    sleep 0.2
  done
  [[ $link == LIVE ]] || { say "rust /ready python_service is '${link:-no answer}', not LIVE, after 20 s"; false; }
  # Both answered, but a health answer can come from another process holding the port.
  own_pid "$(pid_of python)" || { say "python app exited although its port answered"; false; }
  own_pid "$(pid_of rust)" || { say "rust service exited although its port answered"; false; }

  if [[ $want_mockups == 1 ]]; then
    launch mockups "$ROOT" "$PY" -I -m http.server "$MOCK_PORT" --bind 127.0.0.1 --directory "$ROOT/mockups"
    wait_http mockups "http://127.0.0.1:$MOCK_PORT/" "$(pid_of mockups)" 15 || false
  fi
  if [[ $want_web == 1 ]]; then
    NEXT_PUBLIC_API_BASE_URL="http://localhost:$RS_PORT" launch web "$ROOT/apps/web" \
      npm run dev -- --hostname 127.0.0.1 --port "$WEB_PORT"
    wait_http web "http://127.0.0.1:$WEB_PORT/" "$(pid_of web)" 120 || false
  fi
  trap - ERR
  set +E

  say "UP. Developer stack on loopback only; dev tokens on; not a deployment."
  say "database:  $db_desc"
  say "secret:    $auth_desc"
  say "rust       $RS_URL      (pid $(pid_of rust); the API the web app calls; /ready python_service LIVE)"
  say "python     $PY_URL      (pid $(pid_of python); owner messaging, chat, studio, supervisor)"
  if [[ $want_mockups == 1 ]]; then say "mockups    http://localhost:$MOCK_PORT/  (pid $(pid_of mockups))"
  else say "mockups    not started: there is no mockups/ folder in this checkout (it lives on another branch)"; fi
  if [[ $want_web == 1 ]]; then say "web        http://localhost:$WEB_PORT/  (pid $(pid_of web))"
  else
    local inst="npm install"
    [[ -f $ROOT/apps/web/package-lock.json ]] && inst="npm ci"
    say "web        not started: apps/web has no node_modules. Run once: (cd apps/web && $inst)   then start again"
  fi
  say "logs:      ${STATE#"$ROOT"/}/logs/   status: $0 status   stop: $0 stop"
}

cmd_status() {
  local svc pid state
  for svc in "${SERVICES[@]}"; do
    pid=$(pid_of "$svc")
    if [[ -z $pid ]]; then state="not running"
    elif own_pid "$pid"; then state="running, pid $pid"
    else state="NOT running (stale pid file $pid)"; fi
    printf '%-8s http://127.0.0.1:%-5s %s\n' "$svc" "$(port_of "$svc")" "$state"
  done
  local body
  if body=$(curl -sf --noproxy '*' --max-time 5 "$RS_URL/ready" 2>/dev/null); then
    "$PY" -I -c 'import json, sys
d = json.loads(sys.argv[1])
print("rust /ready: %s; database: %s" % (d.get("status"), (d.get("checks") or {}).get("database")))
for p in d.get("providers") or []:
    print("  %-16s mode=%-12s status=%-8s %s" % (p.get("provider"), p.get("mode"), p.get("status"), (p.get("note") or "")[:120]))' "$body"
  else
    echo "rust /ready: no answer at $RS_URL"
  fi
  if curl -sf --noproxy '*' --max-time 3 "$PY_URL/health" >/dev/null 2>&1; then echo "python /health: ok"
  else echo "python /health: no answer at $PY_URL"; fi
}

cmd_logs() {
  local only=${1-} follow=${2-} svc f
  if [[ $only == -f ]]; then only=; follow=-f; fi
  for svc in build alembic "${SERVICES[@]}"; do
    [[ -z $only || $only == "$svc" ]] || continue
    f=$STATE/logs/$svc.log
    [[ -f $f ]] || continue
    if [[ $follow == -f && -n $only ]]; then exec tail -n 40 -f "$f"; fi
    printf '==> %s <==\n' "${f#"$ROOT"/}"
    tail -n 40 "$f"
  done
}

case ${1-} in
  start) cmd_start ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  logs) cmd_logs "${2-}" "${3-}" ;;
  -h|--help|help) sed -n '2,/^[^#]/{/^#/p}' "${BASH_SOURCE[0]}" ;;
  *) echo "usage: $0 start|stop|status|logs [SERVICE] [-f]   (see --help)" >&2; exit 2 ;;
esac
