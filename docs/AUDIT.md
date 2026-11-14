# Repository audit (Phase 0)

Audited 2026-11-14 ~11:00 UTC on branch `claude/focused-shannon-bnu57q` (identical to `origin/main` at `df69698`).

## What existed before initialization
| Item | State |
|---|---|
| Commits | 6: template (`azizu06`, Nov 1) → board setup (`tiwari-ira`, Nov 14) → merge (`jasleenkgarcha`, Nov 14) |
| Branches | `main` only (remote) |
| Files | `AGENTS.md`, `CLAUDE.md` (`@AGENTS.md`), `README.md`, `.claude/settings.json`, `.claude/hooks/board-{prompt,session-start}.sh`, `.github/ISSUE_TEMPLATE/message-board.md`, `tools/board` (222-line bash, needs only `gh`), `.gitignore` (`.board/`), `.gitattributes`, `docs/banner.jpg` |
| Application code | None. No dependency files, no CI, no env config |
| Issues | #1 "Message Board — Second Site agents" (open, pinned per board post); #2 duplicate board (closed) |
| PRs | None |
| Board | 1 post (Ira's Codex): board set up, ownership unassigned |
| Known GitHub logins | `tiwari-ira` (Ira), `jasleenkgarcha`, `azizu06` (template author; may not be on team), `mihirmalaviya` (this session) |

## Preserved (unchanged behavior)
`tools/board`, `.claude/settings.json`, both hooks, `CLAUDE.md`, issue template, `.gitattributes`, `docs/banner.jpg`.
`AGENTS.md` keeps every board rule verbatim; sections 3–4 were extended (long-lived branches, ownership table).
The original README content moved to `docs/AGENT_BOARD.md` and is linked from the new README.

## Changed / added
Everything under `apps/`, `packages/`, `fixtures/`, `scripts/`, `infra/`, `.github/workflows/`, `docs/` (except banner), plus `Makefile`, `package.json`, `pnpm-workspace.yaml`, `pnpm-lock.yaml`, `.env.example`; `.gitignore` extended (still ignores `.board/`).

## Competition constraints found
| Constraint | Status |
|---|---|
| Public GitHub repo | **NOT MET — repository is PRIVATE.** Visibility was not changed (needs owner approval). |
| Repo created Nov 13 8pm ET – Nov 15 | **MET** — created 2026-11-14T07:16:16Z (Nov 14, 2:16am ET). |
| Deadline | Devpost submission **8:00 am ET Sunday Nov 15, 2026** (hard). In-person demo ≤ 2 min, judging 9:30–14:30. |
| One main track | Business and Enterprise |
| Peri evidence | Business name, contact name + role, one piece of evidence — fields in `docs/PERI_CONSULTATION.md`, **empty (not fabricated)** |
