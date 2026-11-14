#!/usr/bin/env bash
# One-time local setup. Safe to re-run. Usage: scripts/bootstrap.sh [--no-docker]
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
say() { printf '\n==> %s\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { echo "Missing prerequisite: $1 ($2)"; exit 1; }; }

say "Checking prerequisites"
need uv "https://docs.astral.sh/uv/  (curl -LsSf https://astral.sh/uv/install.sh | sh)"
need pnpm "corepack enable  (Node >= 20.9)"
need node "https://nodejs.org (>= 20.9)"
need gh "https://cli.github.com then: gh auth login  (needed for tools/board)"

say "Environment file"
if [ ! -f .env ]; then cp .env.example .env; echo "Created .env from .env.example (fixture mode, no secrets)."; else echo ".env exists (left untouched)."; fi

say "Python dependencies (apps/api)"
(cd apps/api && uv sync)

say "JavaScript dependencies"
pnpm install

say "Database + Redis"
if [ "${1:-}" != "--no-docker" ] && command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  docker compose -f infra/compose.yaml up -d postgres redis
  echo "Waiting for Postgres..."; for _ in $(seq 1 40); do docker compose -f infra/compose.yaml exec -T postgres pg_isready -U secondsite >/dev/null 2>&1 && break; sleep 1; done
  docker compose -f infra/compose.yaml exec -T postgres psql -U secondsite -tc "SELECT 1 FROM pg_database WHERE datname='secondsite_test'" | grep -q 1 \
    || docker compose -f infra/compose.yaml exec -T postgres createdb -U secondsite secondsite_test
else
  echo "Docker unavailable: start PostgreSQL 16 + Redis yourself (see docs/runbooks/local-without-docker.md)."
fi

say "Migrations"
(cd apps/api && uv run alembic upgrade head) || { echo "Migration failed: is Postgres reachable at DATABASE_URL?"; exit 1; }

say "Contracts"
scripts/generate-contracts.sh

say "Health check"
(cd apps/api && uv run python -c "from app.main import app; print('API imports OK:', len(app.openapi()['paths']), 'paths')")
command -v gh >/dev/null && tools/board whoami || true
echo; echo "Done. Next: scripts/dev.sh  (API :8000, web :3000)"
