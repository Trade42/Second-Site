#!/usr/bin/env bash
# Runs API (:8000) and web (:3000) together. Ctrl-C stops both.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
trap 'kill 0' EXIT
(cd apps/api && uv run uvicorn app.main:app --reload --port 8000) &
(pnpm --filter @second-site/web dev) &
wait
