#!/usr/bin/env bash
# Health probe for the Rust service that needs only bash and coreutils, because the runtime image
# (infra/docker/backend.Dockerfile) has no curl. The image's HEALTHCHECK and the compose service
# run it; smoke-selftest.sh runs it against a real binary.
#
# Usage: healthcheck.sh [HOST] [PORT]   (defaults: 127.0.0.1 and $PORT, else 8000)
# Exit 0 only when GET /health answers HTTP 200 with "status":"ok" within HEALTHCHECK_TIMEOUT
# seconds (default 5).
set -euo pipefail

if [[ ${1:-} != --probe ]]; then
  host=${1:-127.0.0.1}
  port=${2:-${PORT:-8000}}
  exec timeout "${HEALTHCHECK_TIMEOUT:-5}" bash "$0" --probe "$host" "$port"
fi

host=$2
port=$3
if ! exec 3<>"/dev/tcp/$host/$port"; then
  echo "unhealthy: cannot connect to $host:$port" >&2
  exit 1
fi
printf 'GET /health HTTP/1.1\r\nHost: %s:%s\r\nAccept: application/json\r\nConnection: close\r\n\r\n' \
  "$host" "$port" >&3
IFS= read -r status <&3 || status=
status=${status%$'\r'}
body=$(cat <&3)
case $status in
  "HTTP/1."?" 200"*) ;;
  *) echo "unhealthy: ${status:-no status line} from $host:$port" >&2; exit 1 ;;
esac
case $body in
  *'"status":"ok"'*) exit 0 ;;
  *) echo "unhealthy: /health answered 200 without \"status\":\"ok\"" >&2; exit 1 ;;
esac
