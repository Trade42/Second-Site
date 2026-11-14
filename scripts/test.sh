#!/usr/bin/env bash
# All local checks. DB tests need Postgres (TEST_DATABASE_URL); they skip with a reason otherwise.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
echo "== ruff";        (cd apps/api && uv run ruff check app tests && uv run ruff format --check app tests)
echo "== pytest";      (cd apps/api && uv run pytest -q)
echo "== contracts";   scripts/check-contracts.sh
echo "== web typecheck"; pnpm --filter @second-site/web typecheck
echo "== web lint";    pnpm --filter @second-site/web lint
