# End-to-end harness, part 1: the Rust service and the Python app together

`run.sh` builds the Rust release binary, starts it **and** the Python app on loopback, both on one private
PostgreSQL schema with one `AUTH_SECRET`, points each at the other, and runs `driver.py`. The driver runs 69
numbered checks against both live services, then the harness stops both processes and drops the schema.
Finding integration defects is the point: a check that fails is reported, never worked around.

```
bash apps/backend/e2e/run.sh                         # build, run, tear down
bash apps/backend/e2e/run.sh --keep                  # leave the private schema for inspection (name is printed)
bash apps/backend/e2e/run.sh --no-build              # use the last release binary that built
bash apps/backend/e2e/run.sh --migrate-with=rust     # the Rust binary applies the schema, alembic must find nothing to do
bash apps/backend/e2e/run.sh --driver=/path/copy.py  # run another copy of the driver (used for the mutation proof)
```

Set `TMPDIR` to an on-disk directory if `/tmp` is a small RAM volume. Each run directory holds the logs (a few
hundred kB) plus a private 17 MB copy of the binary, which is deleted at teardown. The run directory, with every
log, stays after the run; its path is the last line printed.

## What it needs

- The local PostgreSQL 16 on its Unix socket `/tmp/ss-pg` (`bash orchestrator/scripts/testdb.sh status`). The
  harness never starts, stops or reconfigures it. It uses only the `secondsite_rust_test` database and creates
  and drops its own schema `e2e_<12 hex>`. It checks that the table list of `public` is unchanged at the end.
- `apps/api/.venv` (the Python app, alembic, `httpx`, `psycopg`) and the cargo wrapper
  `orchestrator/scripts/cargo.sh`. If the crate does not compile because other writers are mid-edit, the
  harness waits 120 s and retries, up to 5 times. After that it falls back to the last binary that built and
  prints its build time.
- Nothing else: no Redis, no Docker, no network beyond 127.0.0.1, no provider key.

## What it starts, and how

Exactly two processes, both bound to `127.0.0.1` on free ports chosen at run time:

| process | command | told about the other through |
|---|---|---|
| Rust service | `<run dir>/bin/second-site-backend` (private copy of the release binary) | `PYTHON_SERVICE_URL=http://127.0.0.1:<python port>` |
| Python app | `apps/api/.venv/bin/python -I -B -m uvicorn --app-dir apps/api --host 127.0.0.1 --port <p> app.main:app` | `BACKEND_API_URL=http://127.0.0.1:<rust port>` |

- **Clean environment.** Both are started with `env -i`, so nothing from the caller's environment is
  inherited: no proxy variables, no keys, no `BACKEND_API_URL` left over from a shell. Every setting is passed
  explicitly. `run.sh` writes the exact environment of each process to `env-rust.txt` and `env-python.txt` in
  the run directory, with the two generated secrets masked.
- **No `.env`.** Both services read `.env` and `../../.env` relative to their working directory (the
  pydantic-settings `env_file` setting and the Rust port of it). Both are started from `<run dir>/cwd/a/b`, an
  empty directory created for the run, so neither path exists. `run.sh` checks this before it starts them.
  The repository `.env` is never read, opened or printed.
- **Fixture providers only.** All seven `*_PROVIDER_MODE` variables are `fixture`. `SUPERVISOR_MODEL` is empty
  (the model-free supervisor). `ALLOW_LIVE_OUTBOUND_CALLS` and `ALLOW_LIVE_PURCHASES` are `false`. No real key is
  set. `AUTH_SECRET` and `PHOTON_WEBHOOK_SECRET` are random values generated per run.
- **One database URL for both:**
  `postgresql+psycopg://secondsite:secondsite@localhost/secondsite_rust_test?host=/tmp/ss-pg&options=-csearch_path=<schema>&application_name=<schema>`.
  Rust strips the `+psycopg` suffix itself. The `options` value pins `search_path` for psycopg and sqlx alike.
  The `=` inside it is deliberately not percent-encoded: `apps/api/alembic/env.py` passes `DATABASE_URL` to
  configparser, which rejects any `%`.
- **Always stopped.** An `EXIT`/`INT`/`TERM`/`HUP` trap stops both processes (SIGTERM, then SIGKILL after 10 s)
  and drops the schema in every exit path. At the end the harness prints how many of its processes are left
  (always 0) and the schemas of both test databases.

## The schema, applied once

The Python app never migrates at start-up. The Rust binary migrates at every start-up, and also with
`second-site-backend migrate`, alembic-compatibly: it writes `alembic_version`, applies `0001`/`0002`, and takes
a per-schema advisory lock. The current tree also creates the Rust-owned table `nessie_simulation_records`
outside alembic. By default the harness runs `alembic upgrade head` on the empty schema; the Rust start then
logs `schema is at alembic head`. `--migrate-with=rust` does the reverse: after `second-site-backend migrate`,
`alembic upgrade head` must apply 0 revisions. The harness runs `alembic check` before and after the Rust
start-up and prints both results. The full finding is in `orchestrator/reports/e2e-1.md`.

## Output

Each check prints one line, `<id> PASS|FAIL|SKIP <title> :: <detail>`, and the run ends with a `SUMMARY` line.
A failed check never stops the run. A check whose input comes from a check that did not pass is a `SKIP` that
names the missing input. The driver writes `results.json` to the run directory. Exit codes:

| code | meaning |
|---|---|
| 0 | no check failed |
| 1 | at least one check failed |
| 2 | the harness itself could not set up or tear down (build, database, start-up, schema drop) |

The driver counts requests in the two processes' JSON request logs (`rust.log`, `python.log`) to prove which
service was reached.

## The checks

**A. The demo path through the Rust origin.** A01–A02 health and ready (database `ok`; when the build has the
forwarder, `python_service` must be `LIVE`; where Rust's provider list disagrees with Python's `/ready`, the
detail says so). A03 dev token, verified with Python's `verify_token`. A04 the demo inputs equal
`fixtures/projects/facial_studio.json` and are labelled `FIXTURE_SYNTHETIC`. A05 create the fixture project
(profile `CONFIRMED`). A06 search, a 202 that runs inline, and its `status_url`. A07 listings are
`provider: fixture`, `data_origin: FIXTURE_*`, never CoStar. A08 permit evidence (`FIXTURE_REAL`,
`Permit Finaled` kept as a historical status). A09 `UNKNOWN` assessments in the feasibility route and in the
property records. A10 cost scenarios in integer minor units. A11 ranking v1, ranks 1 and 2. A12 every event type
of the Python smoke test. A13 layout generation on the property whose interior asset has measured geometry.
A14 the task. A15 the layouts are `PROPOSED`, valid, and carry the not-approved disclaimer. A16 the fixture
geometry is labelled `FIXTURE` / `FIXTURE_SYNTHETIC`, `scale_verified: false`, never as measured. A17 a revision
(a fourth treatment room) becomes version 3. A18 the bill: every price `FIXTURE_SYNTHETIC`, "not a supplier
quote".

**B. One token for both services.** B01 a Rust token works on Python. B02 Python's dev token names the same
owner. B03 a Python token works on Rust. B04 a token signed with another secret gets 401 from both. B05 a
`service`-purpose token gets 401 on ordinary routes of both.

**C. Each service reads what the other wrote.** Each compares the same GET on both ports with one token: raw
JSON, number and boolean types compared, key order ignored. The detail also says whether the bytes were
identical. Nothing else is normalised. C01–C08: project, properties, rankings, cost scenarios, events,
layouts + geometry + stored bill, tasks, permits and feasibility, all written by Rust. C09 a second project
created and searched through Python; the check waits until Rust sees the completed task and reports how long
that took. C10–C11 Rust reads Python's project, properties, rankings, cost scenarios, events and task. C12
read-after-write: 30 projects created through Python, each read through Rust at once; 30 the other way round;
30 Python-to-Python as a baseline.

**D. The supervisor reaches Rust.** D01–D02 `POST .../chat` on the Python port ("rank the properties",
"what's the status?") answers with Rust data. Rust's request log must show the calls, and no tool error may
appear. D01 also records whether the chat's `x-request-id` reaches Rust. D03–D10 each of the eight supervisor
tools in HTTP mode (`app.agents.tools.build_tools` with `BACKEND_API_URL` set, in the driver process) against
the real Rust service. The evidence and outreach tools are `SKIP` when Rust answers 404. D11 a chat on a fresh
project with no ranking makes the supervisor run the search through Rust. D12 the read-only tools give the
same text in-process (Python reading the shared schema, read only) and over HTTP (Rust). D13 after the second
analysis, the feasibility routes of both services agree; the detail records their order against the property
record.

**E. Rust's owner-notify path as Python sees it.** E01 `POST /internal/v1/owner/notify` with a `service` token
before the channel is linked answers `not_linked`. E02 the channel is linked through `PUT /api/v1/owner/channel`
and `simulate-text "hi"` (fixture Photon). E03 the next notify answers `recorded`. E04 the same key answers the
same, and no communication or event is added. E05–E07 the notify events reach the Rust origin: in the list,
through the `after=` cursor, and through the SSE stream.

**F. Through the forwarder.** Every check is `SKIP` ("forwarder not built yet") when Rust answers
`GET /api/v1/owner/channel` with 404, except F07. F01–F04 chat, GET/PUT owner channel and simulate-text through
the Rust origin equal the Python answers. F05 a signed `POST /webhooks/photon`, signed with Python's
`standard_webhooks_sign` and the run's secret, is accepted (204) and reaches Python once. F06 a broken signature
gives 400 `INVALID_SIGNATURE`. F07 runs with or without the forwarder: `/internal/v1/owner/notify` through the
Rust origin (POST, GET, trailing slash) is Rust's own 404, and Python logs no `/internal` request.

**G. Outreach and calls, as far as they exist.** G01 `POST .../outreach` through the Rust origin creates a draft
and a `PENDING` `PLACE_CALL` approval, and nothing is sent. The detail says whether the route is native or
forwarded and what Rust's owner notify answered. G02 is a fallback used only when Rust has no outreach route:
the same outreach on the Python port. G03 the approval is in Rust's approvals list, and its payload and hash
equal Python's `call_payload` / `payload_hash`. G04 approve through Rust; Python reads `APPROVED`. G05
`POST .../calls` through the Rust origin: a labelled simulation with fixture telephony. Nothing more is driven
on the call. G06 Python then refuses a second execution of that approval (409); without a Rust calls route,
the check records what Python's route does instead.

**H. Hygiene.** H01 neither log contains `AUTH_SECRET`, the Photon secret, or any bearer token (or its
signature) that the run used.

## Lint

`apps/api/.venv/bin/ruff check --config apps/api/pyproject.toml apps/backend/e2e` and
`ruff format --check` with the same `--config`. Ruff finds no configuration of its own under `apps/backend`, so
the repository settings must be passed explicitly. `bash -n apps/backend/e2e/run.sh`.

## Not covered

No browser and no web app. No live provider, no real model (the model-free supervisor only), no real Photon,
Telnyx or Nessie. The concurrent-writer event-order window (Python writers do not take Rust's per-project event
lock) is not exercised.
