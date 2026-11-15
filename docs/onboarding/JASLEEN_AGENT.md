# Jasleen's agent

Open the folder that contains `apps/` and `tools/board`:

`/Users/jasleenkaur/Second-Site/Second-Site`

In Cursor: **File → Open Folder…**, then choose that folder. Press **Cmd+B** if the left sidebar is hidden. The top icon (two overlapping pages) is the file list.

You are Jasleen's agent. Her GitHub login is `jasleenkgarcha`. She is the team lead of Second Site at HackPrinceton Fall 2026. Her only coding job is Photon. Deadline: 8:00 am ET, Sunday, November 15, 2026.

## What to do next

1. Merge pull request #4 once you are ready. Checks are green. The author merges her own PR. Do not merge it from a chat until Jasleen says to.
   https://github.com/Trade42/Second-Site/pull/4
2. After it is on `main`, Photon coding for this hackathon is done. Do not rebuild the `revise:` / `change:` path.
3. Keep reading the board. Reply when a post mentions `@jasleenkgarcha` or `@all` and it changes Photon. Stay out of the website, finance, live calls, and the Rust branch.
4. A real iPhone text needs a Photon beta account later. There is no iOS app to build. The website is the product. Photon only replies in iMessage.

## What Second Site is

Second Site helps a facial-studio owner pick a second shop in Los Angeles.

The owner has $180,000 to open. They need 3 treatment rooms, 1 reception, and 1 storage room. The look is warm and minimalist, with as little new plumbing as possible.

The demo compares two real addresses:

- **5380 E Alhambra Ave.** Old permits show an 800 amp electrical service and a 2-inch water line. Those permits are history. They do not prove spare power or water is available today.
- **6922 W Hollywood Blvd #302.** A permit covers one break-room sink. A permit for suite 304 does not belong to suite 302.

Cheaper rent can lose once renovation, furniture, and 36 months of rent are added. Advertised rents are sample numbers, not CoStar records. Do not change ranking or dollar totals. The website says "assistant", never "AI assistant". A text cannot approve spending.

## Who owns what

| Person | GitHub | Job |
|---|---|---|
| Jasleen | `@jasleenkgarcha` | Team lead. Photon texting only. |
| Mihir | `@mihirmalaviya` | The website in `apps/web`. |
| Sal | `@Trade42` | The Rust backend and the rest of the Python API, except Photon. |
| Ira | `@tiwari-ira` | Seen on the board. Do not assign her work. |

Finance and live phone calls are nobody's job. Do not dial.

Sal may add `POST /internal/v1/owner/notify` in a new file under `apps/api/app/api/routes/`. It must call the existing `notify_owner`. A repeated idempotency key must not text twice. A failed notify must not block an approval or a call. He must not edit `integrations/photon` or `route_owner_message`. Text design changes do not need a new Rust route. More Rust work stays on `dev/backend-rust` until Sal says it is ready. Do not build on that branch.

## The board

The board is pinned GitHub issue #1, not a separate app:

https://github.com/Trade42/Second-Site/issues/1

From the repo root:

```bash
export BOARD_REPO=Trade42/Second-Site
export BOARD_ISSUE=1
tools/board read -n 15
```

After every change:

```bash
tools/board post "[Jasleen's Agent] <what changed, which file, what you need>"
```

Use a straight apostrophe. `@mihirmalaviya` is the website. `@Trade42` is the Rust backend. `@all` is everyone. Read posts that are not for Jasleen. Reply only when they affect Photon.

## What is already done

Leave these alone. They already work:

- `FixturePhotonProvider` and `LivePhotonProvider` in `apps/api/app/integrations/photon/__init__.py`. Do not edit the live provider.
- `apps/api/app/domains/communications/owner_channel.py`
- JOIN, LATER, STATUS, and STOP in `route_owner_message()`
- Approvals stay in the app. Never grant an approval from a text.

The design-change text is built on branch `dev/communications` and is in PR #4. In `apps/api/app/domains/communications/inbound.py`, a text that starts with `revise:` or `change:` does not go to `run_chat`. The words are saved on a communication record with purpose `design change`. The reply is:

`Noted. I saved this design change: <their words>. Layout edits still happen in the app.`

It does not call `DesignService.revise`. The test is `test_revise_and_change_save_design_words_without_chat` in `apps/api/tests/test_comms_flow.py`. The older parser test `test_parse_inbound_ignores_agent_and_other_events` also passed. Postgres is not running. Skip tests that need it.

There is no `python` command. From `apps/api`, run `source "$HOME/.local/bin/env"` and then `uv run pytest`.

## Do not touch

- `apps/web`
- Finance, ranking, and dollar totals
- Telnyx, xAI voice, and live calls
- `LivePhotonProvider`
- `apps/api/app/contracts/`
- The Rust service

## How to see the files

This Cursor chat may still be in the empty `HackPrinceton` folder. The code is in `/Users/jasleenkaur/Second-Site/Second-Site`. Open that folder. Photon lives in `apps/api/app/domains/communications/inbound.py`.
