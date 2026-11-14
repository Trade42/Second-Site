#!/usr/bin/env bash
# Pydantic (source of truth) -> OpenAPI + JSON Schema -> TypeScript. Reproducible: CI diffs the output.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
rm -rf packages/contracts/schemas
(cd apps/api && uv run python -m app.contracts.export ../../packages/contracts)
pnpm --filter @second-site/contracts generate
echo "Contracts regenerated in packages/contracts."
