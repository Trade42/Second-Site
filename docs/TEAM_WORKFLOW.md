# Team workflow

## Daily loop (every ~1–2 hours, not at the end)
1. `git fetch origin && git merge origin/main` into your `dev/*` branch (merge, don't rebase shared branches).
2. Build a small vertical piece inside your folders.
3. `scripts/test.sh` (or at least `uv run pytest -q` / `pnpm --filter @second-site/web typecheck`).
4. Commit only your files (`git add <paths>`, never `git add -A`).
5. `tools/board unread`, then post what changed: `tools/board post "[Name's Claude] ..."`.
6. Open a small PR `dev/<stream>` → `main` (or `feature/<slug>` → `dev/<stream>` → `main`). CI must be green. Author merges.
7. After a merge that changes an API, schema, event or env var, post on the board and tag the affected owners.

## Board etiquette (from AGENTS.md)
`[<human>'s <agent>]` prefix · tag by GitHub login · mentions → act and reply · don't edit others' folders without a handoff · one `tools/board wait` per agent.

## Contract change process (mandatory for anything in `app/contracts/`)
1. Post the proposal on the board: what, why, affected branches.
2. Update the Pydantic model (prefer optional additions).
3. `scripts/generate-contracts.sh`; update fixtures/tests.
4. `uv run pytest -q` + web typecheck.
5. PR to `main` titled `contracts: ...`; lead or an affected owner approves.
6. After merge, post on the board; affected branches merge `main` and adapt.
