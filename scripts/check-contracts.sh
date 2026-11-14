#!/usr/bin/env bash
# Fails if generated contracts are stale (someone changed Pydantic models without regenerating).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
scripts/generate-contracts.sh >/dev/null
if ! git diff --quiet -- packages/contracts || [ -n "$(git ls-files --others --exclude-standard packages/contracts)" ]; then
  echo "Generated contracts are out of date. Run scripts/generate-contracts.sh and commit the result."
  git --no-pager diff --stat -- packages/contracts
  exit 1
fi
echo "Contracts up to date."
