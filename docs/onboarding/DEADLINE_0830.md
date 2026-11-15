# Jasleen's agent: 8:30 am ET push deadline (Sun Nov 15, 2026)

You are Jasleen's agent (GitHub `jasleenkgarcha`, team lead). Work in `/Users/jasleenkaur/Second-Site/Second-Site`, branch `dev/communications`. Code pushes stop at **8:30 am ET**. Be fast and safe.

## Already done (do not redo)
- PR #4 (Photon design-change texts) is merged to `main`.
- Landing page redesign is pushed on `1-html-mockups` (dab454e).
- Photon onboarding interview (S8), recording link (S7) and the `inbound.py:90-91` simulated-call reply are handed to Sal's team.

## Push before 8:30: the local Photon work on `dev/communications`
Uncommitted files (checked: no keys or tokens in them):
- `apps/api/app/domains/communications/canned.py` (new): hard-coded text replies used only while `SUPERVISOR_MODEL` is empty
- `apps/api/app/domains/communications/inbound.py`: 5-line hook that calls it
- `apps/api/app/agents/supervisor.py`: passes `ANTHROPIC_API_KEY` to the model and strips thinking blocks from replies
- `fly.toml`, `.dockerignore`, `infra/docker/fly.Dockerfile`, `infra/docker/fly-start.sh`, `infra/docker/fly_seed.py`: Fly deploy of the Photon server
- `docs/onboarding/JASLEEN_AGENT.md`, `docs/onboarding/DEADLINE_0830.md`

Steps:
```bash
cd /Users/jasleenkaur/Second-Site/Second-Site
git status --short
cd apps/api && uv run ruff format app tests && uv run ruff check app tests && uv run pytest -q tests/test_comms_flow.py tests/test_approvals_comms.py && cd ../..
git add apps/api/app/domains/communications/canned.py apps/api/app/domains/communications/inbound.py apps/api/app/agents/supervisor.py fly.toml .dockerignore infra/docker/fly.Dockerfile infra/docker/fly-start.sh infra/docker/fly_seed.py docs/onboarding/JASLEEN_AGENT.md docs/onboarding/DEADLINE_0830.md
git diff --cached | grep -E 'sk-ant|PHOTON_API_KEY=.+|ANTHROPIC_API_KEY=.+' && echo "STOP: secret staged" || echo "clean"
git commit -m "Photon: hard-coded text replies without a model, model key wiring, Fly deploy"
git fetch origin && git merge origin/main --no-edit
git push origin dev/communications
```
If tests fail and there is no time to fix, drop the failing file from the commit instead of pushing broken code.

## Rules
- **Never** stage or commit `.env`, `.codex/`, or any API key. No `git add -A`, no `git add .`.
- Never force-push. Do not push to `main` directly; `dev/communications` is enough for the deadline. Open a PR to `main` only if Jasleen asks.
- After pushing, post on the board: `tools/board post "[Jasleen's Cursor] Pushed Photon text replies, model wiring and Fly deploy files to dev/communications (<sha>). inbound.py change is a 5-line hook; @Trade42 it is yours to keep or replace with the onboarding work."`

## Landing page (decision for Jasleen, not code for you)
Mihir has local, unpushed edits to `mockups/landing.html` (shorter lede, no 3D captions, copy rewrite). If Jasleen wants them in, Mihir must push to `1-html-mockups` before 8:30. Do not edit `landing.html` yourself.

## Truthfulness (AGENTS.md)
Unknown stays UNKNOWN. Texts never approve anything or claim a real call happened. No real messages, calls or purchases without an APPROVED owner approval in code.
