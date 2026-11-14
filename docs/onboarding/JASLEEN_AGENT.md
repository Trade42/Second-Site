# Jasleen's Cursor agent

Paste this whole file as the first message in a new Cursor Agent chat. Open the `Second-Site` folder first (the one that contains `apps/` and `tools/board`). You are already set up. Do not reinstall tools.

You are Jasleen's agent. Jasleen's GitHub login is `jasleenkgarcha`. She is the team lead of Second Site. Her only coding job is Photon: read an owner's text, save a pretend reply, and take a design-change sentence from that text.

## Read the board first, then keep posting

From the repo root, not from `apps/api`:

```bash
export BOARD_REPO=Trade42/Second-Site
export BOARD_ISSUE=1
tools/board read -n 15
```

After every change, and at the end of the session, post a short update:

```bash
export BOARD_REPO=Trade42/Second-Site
export BOARD_ISSUE=1
tools/board post "[Jasleen's Agent] <what changed, which file, what you need>"
```

The post must start with `[Jasleen's Agent]` and a straight apostrophe. Mention `@mihirmalaviya` for the website and `@Trade42` for the Rust backend. Use `@all` when everyone needs it. If a post is not addressed to Jasleen, read it and reply only when it changes her files.

## What is already done

Do not rewrite these. The parser test already passed on her laptop:

```bash
cd apps/api && uv run pytest tests/test_comms_flow.py::test_parse_inbound_ignores_agent_and_other_events -q
```

Already on `main` (`aa300aa`):

- `apps/api/app/integrations/photon/__init__.py` — `FixturePhotonProvider` is the pretend inbox. `LivePhotonProvider` talks to the real Photon account. Leave the live class alone.
- `apps/api/app/domains/communications/owner_channel.py` — register a handle, link it on the first text, `notify_owner`.
- `apps/api/app/domains/communications/inbound.py` — `route_owner_message()` already handles JOIN, LATER, STATUS, STOP, and "approve". Approvals stay in the app. Never grant an approval from a text.
- `apps/api/app/domains/communications/poller.py` — polling.

`uv` is installed. Tests run with `uv run pytest` from `apps/api`. There is no `python` or `uv` until a new terminal runs `source "$HOME/.local/bin/env"`. Postgres is not running. Skip any test that needs it.

## The one thing left to build

Text design-revision intake. Mihir's agent confirmed it is not built. The hook is `route_owner_message()` in `apps/api/app/domains/communications/inbound.py`.

When the owner's text starts with `revise:` or `change:`, do not send that sentence to `run_chat`. Reply in plain language that the change was saved, and keep the words. Example reply: `Noted. I saved this design change: warmer walls. Layout edits still happen in the app.`

Stay inside `inbound.py` and a new test next to the other Photon tests. Do not call `DesignService.revise` yet. That path needs a current layout and room overrides, and it is not this task. Do not add a Rust route. Sal (`@Trade42`) may add `POST /internal/v1/owner/notify` later. It must call the existing `notify_owner`, and a repeated idempotency key must not text twice. Do not edit that until Sal's route exists.

Product copy says "assistant", never "AI assistant".

## Do not touch

- `apps/web` — Mihir (`@mihirmalaviya`) owns the whole website.
- `dev/agents-finance`, ranking, cents, and the supervisor. That seat is unassigned. Jasleen is not on finance.
- `integrations/telnyx`, `integrations/xai_voice`, and live call handoff. Must not dial.
- `LivePhotonProvider` and any real Photon API key.
- `apps/api/app/contracts/` unless you ask on the board and wait.
- The Rust service. It is Sal's, on `dev/backend-rust`.

## How to check the work

From `apps/api`, with `uv` on the path:

```bash
source "$HOME/.local/bin/env"
uv run pytest tests/test_comms_flow.py::test_parse_inbound_ignores_agent_and_other_events -q
```

Add one test that a `revise:` text is saved and is not treated as an approval. Run that test the same way. Then post the result on the board.

## Team right now

- Jasleen (`@jasleenkgarcha`): Photon only. Team lead.
- Mihir (`@mihirmalaviya`): entire website UI. His agent is also wiring sponsor data and has told us the communications flow on `main` is already there.
- Sal (`@Trade42`): Rust backend and everything in the Python API except Photon and the LangChain supervisor. He asked before editing Jasleen's files. The answer already posted: he may add the notify route with the limits above, and he should not edit `integrations/photon` or `route_owner_message` while the revision text is being added.
