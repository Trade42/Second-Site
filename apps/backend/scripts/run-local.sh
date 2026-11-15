#!/usr/bin/env bash
# Build the Rust service (release, --locked) and run it in the foreground, settings from the
# environment. Ctrl-C stops it.
#
# Usage: apps/backend/scripts/run-local.sh [--memory] [--bin PATH] [--workdir DIR]
#   --memory       in-memory store: a demo fallback when no database is available. Nothing is
#                  durable and /ready says "degraded". Same as STORE_BACKEND=memory.
#   --bin PATH     run this binary instead of building one.
#   --workdir DIR  working directory of the service (default: apps/backend). Like the Python app
#                  in apps/api, the service also reads DIR/.env and DIR/../../.env (from the
#                  default directory that is the repository's .env); the environment wins.
#
# Settings (all names in .env.example; the ones this script checks or sets):
#   DATABASE_URL   required for the PostgreSQL store, in the environment or one of those .env
#                  files. postgresql:// and postgresql+psycopg:// both work. Without it the
#                  binary would silently use its built-in localhost default, so this script
#                  refuses to start instead.
#   STORE_BACKEND  postgres (default) or memory.
#   FIXTURES_DIR   default: the repository's fixtures/ directory.
#   HOST, PORT     default 127.0.0.1 and 8000 here (the binary's own default host is 0.0.0.0).
#   This script exports STORE_BACKEND, FIXTURES_DIR, HOST and PORT, so a line for one of those
#   four in a .env file has no effect: set them in the environment.
#   CARGO          cargo command used to build (default: cargo).
set -euo pipefail

SCRIPTS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BACKEND="$(cd "$SCRIPTS/.." && pwd)"
ROOT="$(cd "$BACKEND/../.." && pwd)"

say() { printf 'run-local: %s\n' "$*" >&2; }
die() { printf 'run-local: refusing to start: %s\n' "$*" >&2; exit 2; }

memory=0
bin=
workdir=$BACKEND
while [[ $# -gt 0 ]]; do
  case $1 in
    --memory) memory=1; shift ;;
    --bin) [[ $# -ge 2 ]] || die "--bin needs a path"; bin=$2; shift 2 ;;
    --workdir) [[ $# -ge 2 ]] || die "--workdir needs a directory"; workdir=$2; shift 2 ;;
    -h|--help) sed -n '2,/^[^#]/{/^#/p}' "${BASH_SOURCE[0]}"; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done
[[ -d $workdir ]] || die "--workdir $workdir is not a directory"
workdir="$(cd "$workdir" && pwd)"

# Store. The value is exported, so a STORE_BACKEND line in a .env file cannot change it.
store=postgres
if [[ $memory == 1 ]]; then
  store=memory
elif [[ -n ${STORE_BACKEND:-} ]]; then
  case $STORE_BACKEND in
    memory) store=memory ;;
    postgres|postgresql) store=postgres ;;
    *) die "STORE_BACKEND=$STORE_BACKEND: expected 'postgres' or 'memory'" ;;
  esac
fi
export STORE_BACKEND=$store

# Where the service would find a setting: the environment first, then the two .env files it
# reads (by name only; values in those files are never printed).
defined_in_dotenv() {
  local name=$1 f
  for f in "$workdir/.env" "$workdir/../../.env"; do
    if [[ -f $f ]] && grep -Eiq "^[[:space:]]*(export[[:space:]]+)?${name}[[:space:]]*=[[:space:]]*[^[:space:]#]" "$f"; then
      printf '%s' "$(cd "$(dirname "$f")" && pwd)/.env"
      return 0
    fi
  done
  return 1
}
redact_url() { sed -E 's#(://[^/:@]+):[^@/]*@#\1:***@#' <<<"$1"; }

if [[ $store == postgres ]]; then
  if [[ -n ${DATABASE_URL:-} ]]; then
    db_desc="DATABASE_URL from the environment: $(redact_url "$DATABASE_URL")"
  elif src=$(defined_in_dotenv DATABASE_URL); then
    db_desc="DATABASE_URL from $src"
  else
    die "DATABASE_URL is not set (environment, $workdir/.env, $workdir/../../.env). Set it, for example
  DATABASE_URL=postgresql://secondsite:secondsite@localhost:5432/secondsite
or run with --memory for the in-memory demo store (not durable)."
  fi
fi

# Fixtures: read at run time by the fixture providers (listings, permit snapshot, floor geometry).
fixtures=${FIXTURES_DIR:-$ROOT/fixtures}
[[ -d $fixtures ]] || die "FIXTURES_DIR=$fixtures is not a directory"
fixtures="$(cd "$fixtures" && pwd)"
for f in properties/la_candidates.json permits/la_permits_snapshot.json geometry; do
  [[ -e $fixtures/$f ]] || die "FIXTURES_DIR=$fixtures has no $f"
done
export FIXTURES_DIR=$fixtures

export HOST=${HOST:-127.0.0.1}
export PORT=${PORT:-8000}
if ! [[ $PORT =~ ^[0-9]{1,5}$ ]] || ((10#$PORT > 65535)); then die "PORT=$PORT is not a port number"; fi

if [[ -z $bin ]]; then
  say "building the release binary (cargo build --release --locked -p ss-server)"
  cargo=${CARGO:-cargo}
  out=$(cd "$BACKEND" && "$cargo" build --release --locked -p ss-server \
    --message-format=json-render-diagnostics) || die "the release build failed (output above)"
  bin=$(sed -n 's/.*"executable":"\([^"]*second-site-backend\)".*/\1/p' <<<"$out" | tail -n 1)
  [[ -n $bin ]] || die "cargo did not report the second-site-backend executable"
fi
[[ -f $bin && -x $bin ]] || die "$bin is not an executable file"
bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"   # the service runs in $workdir

say "binary:    $bin"
if [[ $store == memory ]]; then
  say "store:     memory (in-memory, NOT durable: all data is lost when the process exits)"
else
  say "store:     postgres ($db_desc)"
fi
say "fixtures:  $FIXTURES_DIR"
say "listen:    http://$HOST:$PORT"
say "workdir:   $workdir (also reads $workdir/.env and $workdir/../../.env)"
if [[ -n ${PYTHON_SERVICE_URL:-} ]]; then
  say "python:    PYTHON_SERVICE_URL is set (used once the forwarder lands)"
else
  say "python:    PYTHON_SERVICE_URL unset (Photon, chat and the call flow are served by the Python app)"
fi
cd "$workdir"
exec "$bin"
