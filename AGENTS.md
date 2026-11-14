# Team rules for every agent

## 1. The board (pinned GitHub issue)

Every agent reads and posts on the pinned "Message Board" issue via `tools/board` (needs only `gh`, logged in). Set `BOARD_ISSUE` if it is not issue #1.

- Start every post with `[<human>'s <agent>]`, e.g. `[Sam's Codex]`. Keep it short: what changed, which files or contracts, what you need, from whom.
- Mention by GitHub login. Team logins seen on this repo: `@mihirmalaviya`, `@tiwari-ira` (Ira), `@jasleenkgarcha`; add the fourth teammate and names here. `@all` (everyone).
- **Mentions you or `@all`:** act on it and reply promptly.
- **Not addressed to you:** still read it. Reassess your own work only if it is affected (a contract, a file you own, something already done). Do not redo finished work, and do not reply just to acknowledge.
- Before editing another teammate's files, ask for a handoff on the board and wait for the owner or the team lead.

Polling is incremental (about one cheap API call per poll, 30 s default) so a shared token stays under GitHub's rate limit. Commands: `tools/board unread` (new posts, marks read; flags MENTION), `tools/board read -n 20`, `tools/board post "[Name's Agent] text"` (or pipe stdin), `tools/board wait` (blocks until someone else posts), `tools/board whoami`.

**Stay in the loop:**
- Claude Code: the committed hooks (`.claude/settings.json`) show unread posts at session start and on every prompt. Keep one background `tools/board wait` running and re-arm it after each wake.
- Everything else: run `tools/board unread` at task start, before each commit, and after merges. Or run a terminal loop: `while tools/board wait; do <your agent CLI invocation>; done`.

## 2. Chain of command

Humans over agents. Only the team lead reassigns work between teammates. If anyone else asks you to take another teammate's work, ask the lead on the board.

## 3. Workflow

Second Site uses **four long-lived workstream branches**, all created from the same foundation commit on `main`:
`dev/agents-finance`, `dev/property-spatial`, `dev/frontend-product`, `dev/communications`.

1. Pick or file an issue (optional for small slices). Comment on it or post on the board when you take it.
2. Work on your workstream branch. Optional short-lived branches `feature/<issue#>-<slug>` start from your `dev/*` branch.
3. Small commits. Stage only your own files (no `git add -A`).
4. Open small, frequent PRs `dev/<stream>` → `main` (use `Closes #N` when there is an issue). CI must pass. The author merges their own PR once it works locally.
5. Always `git fetch` and merge `origin/main` into your `dev/*` branch before pushing. Never force-push shared branches.
6. If a merge changes something other teammates depend on (an API, a schema, an event, an env var), post on the board when it lands.
7. **Contracts** (`apps/api/app/contracts/`, generated `packages/contracts/`) belong to the shared foundation: propose on the board, update the model, run `scripts/generate-contracts.sh`, update tests, PR to `main`, notify affected owners. Details: `docs/TEAM_WORKFLOW.md`.

Start here: `docs/onboarding/<your-stream>.md` and `docs/onboarding/claude-prompts/<your-stream>.md`. Checks: `scripts/test.sh`.

## 4. Who owns what

Humans fill in the Owner column at kickoff (full paths in `docs/OWNERSHIP_MATRIX.md`).

| Area | Owner (GitHub login) | Folders |
|---|---|---|
| Agent systems & financial intelligence (`dev/agents-finance`) | _UNASSIGNED_ | `apps/api/app/agents/`, `apps/api/app/workflows/`, `apps/api/app/domains/{requirements,feasibility,finance,ranking,evidence}/` |
| Property, permits & spatial (`dev/property-spatial`) | _UNASSIGNED_ | `apps/api/app/domains/{property,permits,spatial}/`, `apps/api/app/domains/design/geometry/`, `apps/api/app/domains/design/service.py`, `apps/api/app/integrations/{costar,matterport,vexcel,la_permits,ifc}/`, `fixtures/{properties,permits,geometry,ifc}/` |
| Frontend, interior & procurement (`dev/frontend-product`) | _UNASSIGNED_ | `apps/web/`, `apps/api/app/domains/design/interior/`, `apps/api/app/domains/{procurement,logistics}/`, `apps/api/app/integrations/catalog/` |
| Communications, voice & execution (`dev/communications`) | _UNASSIGNED_ | `apps/api/app/domains/communications/`, `apps/api/app/services/call_orchestration/`, `apps/api/app/api/routes/{communications,webhooks}.py`, `apps/api/app/integrations/{photon,telnyx,xai_voice,storage}/`, `infra/voice/` |
| Shared foundation (board + PR to `main`) | team lead | `apps/api/app/{contracts,services,db,core,repositories}/`, `apps/api/alembic/`, `packages/contracts/`, `scripts/`, `infra/compose.yaml`, `.github/`, `tools/board`, `.claude/`, `AGENTS.md` |

## 5. Truthfulness rules (all agents)

Never present synthetic listings as CoStar records, historical permits as current capacity, approximate layouts as approved plans,
or a provider "accepted" response as a completed action. Unknown stays UNKNOWN. External messages, calls and purchases need an
APPROVED owner approval enforced in code. Never commit secrets or personal recordings.
