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

Settings come from the environment of `run.sh`; set-up stops with exit 2 and names the variable when one is missing.

| variable | meaning | default |
|---|---|---|
| `E2E_PG_URL` | a running PostgreSQL 16 database the harness may create and drop private schemas `e2e_<12 hex>` in: `postgresql://user:password@host:port/db` over TCP, or `postgresql://user@localhost/db?host=/socket/dir` (a socket; `localhost` is added when the host is empty) | none (this repository's sandbox socket instance, only where that socket exists) |
| `E2E_CARGO` | the cargo command, run in `apps/backend` (may carry arguments, e.g. `cargo +stable`) | `cargo` (this repository's sandbox wrapper, only where it exists) |
| `CARGO_TARGET_DIR` | where `release/second-site-backend` is built and found (`--no-build` uses it as is) | `apps/backend/target` |
| `E2E_PYTHON` | the Python of the `apps/api` virtualenv (the app, alembic, `httpx`, `psycopg`) | `apps/api/.venv/bin/python` |

With the compose stack's database, for example:
`E2E_PG_URL=postgresql://secondsite:secondsite@localhost:5432/secondsite bash apps/backend/e2e/run.sh`.

- The harness never starts, stops or reconfigures PostgreSQL. It writes only to its own schema and checks that
  the table list of `public` is unchanged at the end. A password in `E2E_PG_URL` is masked in the kept files.
- Also needed: `curl`, `bash`, GNU coreutils (`timeout`, `sha256sum`, `date -r`); tested on Linux only.
- If the crate does not compile (other writers mid-edit), the harness waits 120 s and retries, up to 5 times,
  then falls back to the last binary that built and prints its build time.
- Nothing else: no Redis, no Docker, no network beyond 127.0.0.1 and the database, no provider key.

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
- **One database URL for both:** `E2E_PG_URL` as `postgresql+psycopg://...` with
  `options=-csearch_path=<schema>&application_name=<schema>` added (`pgschema.py url`). Rust strips the
  `+psycopg` suffix itself. The `options` value pins `search_path` for psycopg and sqlx alike.
- **Always stopped.** An `EXIT`/`INT`/`TERM`/`HUP` trap stops the driver and both processes (SIGTERM, then SIGKILL
  after 10 s) and drops the schema in every exit path, also when the signal arrives during the teardown itself
  (each step is idempotent and resumes). The driver runs in the background, so a signal acts at once. At the
  end the harness prints how many of its processes are left (always 0) and the schemas of the database.

## The schema, applied once

The Python app never migrates at start-up. The Rust binary migrates at every start-up, and also with
`second-site-backend migrate`, alembic-compatibly: it writes `alembic_version`, applies `0001`/`0002`, and takes
a per-schema advisory lock. The current tree also creates the Rust-owned table `nessie_simulation_records`
outside alembic. By default the harness runs `alembic upgrade head` on the empty schema; the Rust start then
logs `schema is at alembic head`. `--migrate-with=rust` does the reverse: after `second-site-backend migrate`,
`alembic upgrade head` must apply 0 revisions. The harness runs `alembic check` before and after the Rust
start-up and prints both results.

## Output

Each check prints one line, `<id> PASS|FAIL|SKIP <title> :: <detail>`, and the run ends with a `SUMMARY` line.
A failed check never stops the run. A check whose input comes from a check that did not pass is a `SKIP` that
names the missing input. The driver writes `results.json` to the run directory. Exit codes:

| code | meaning |
|---|---|
| 0 | every check passed (G02 may be a SKIP: its fallback was not needed) |
| 1 | at least one check failed |
| 2 | the harness itself could not set up or tear down (build, database, start-up, schema drop) |
| 3 | no check failed, but at least one was skipped (a route answered 404, for example), so the gate is not met; the `SUMMARY` line names them |

The driver counts requests in the two processes' JSON request logs (`rust.log`, `python.log`) to prove which
service was reached.

## The checks

**A. The demo path through the Rust origin.** A01–A02 health and ready (database `ok`; when the build has the
forwarder, `python_service` must be `LIVE`; where Rust's provider list disagrees with Python's `/ready`, the
detail says so). A03 dev token, verified with Python's `verify_token`. A04 the demo inputs equal
`fixtures/projects/facial_studio.json` and are labelled `FIXTURE_SYNTHETIC`. A05 create the fixture project
(profile `CONFIRMED`). A06 search, a 202 that runs inline, and its `status_url`. A07 listings, interior assets (scans) and exterior assets
(imagery) are `provider: fixture`, `data_origin: FIXTURE_*`, never CoStar, Matterport or Vexcel. A08 permit evidence
(every permit `FIXTURE_*`, `Permit Finaled` kept as a historical status). A09 `UNKNOWN` assessments in the feasibility route and in the
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
that took. C10–C11 Rust reads Python's project, properties, rankings, cost scenarios, events and task; C11 also requires
that Rust's analysis (first project) and Python's (second project) give every assessment the same status. C12
read-after-write: 30 projects created through Python, each read through Rust at once; 30 the other way round;
30 Python-to-Python as a baseline.

**D. The supervisor reaches Rust.** D01–D02 `POST .../chat` on the Python port ("rank the properties",
"what's the status?") answers with Rust data. Rust's request log must show the calls, and no tool error may
appear. D01 also records whether the chat's `x-request-id` reaches Rust. D03–D10 each of the eight supervisor
tools in HTTP mode (`app.agents.tools.build_tools` with `BACKEND_API_URL` set, in the driver process) against
the real Rust service. The evidence and outreach tools are `SKIP` when Rust answers 404; D10 requires a new `PENDING` approval. D11 a chat on a fresh
project with no ranking makes the supervisor run the search through Rust. D12 the read-only tools give the
same text in-process (Python reading the shared schema, read only) and over HTTP (Rust). D13 after the second
analysis, the feasibility routes of both services agree; the detail records their order against the property
record.

**E. The owner-notify route as Python serves it** (the driver calls it; Rust's own notifier is checked in G01).
E01 an ordinary access token is refused (401); with a `service` token before the channel is linked the answer is
`not_linked`. E02 the channel is linked through `PUT /api/v1/owner/channel`
and `simulate-text "hi"` (fixture Photon). E03 the next notify answers `recorded`. E04 the same key answers the
same, and no communication or event is added. E05–E07 the notify events reach the Rust origin: in the list,
through the `after=` cursor, and through the SSE stream.

**F. Through the forwarder.** Every check is `SKIP` ("forwarder not built yet") when Rust answers
`GET /api/v1/owner/channel` with 404, except F07. F01–F06 send through the Rust origin with a fresh
`x-request-id` and require that id once in Rust's request log and once in Python's (so none can pass with Python
alone). F01–F04 chat, GET/PUT owner channel and simulate-text equal the Python answers. F05 a signed
`POST /webhooks/photon`, signed with Python's `standard_webhooks_sign` and the run's secret, is accepted: 204 from
Python and from Rust. F06 a broken signature: 400 `INVALID_SIGNATURE`, from Python. F07 runs with or without the forwarder: `/internal/v1/owner/notify` through the
Rust origin (POST, GET, trailing slash) is Rust's own 404, and Python logs no `/internal` request.

**G. Outreach and calls, as far as they exist.** G01 `POST .../outreach` through the Rust origin creates a draft
and a `PENDING` `PLACE_CALL` approval, and nothing is sent. Rust's notifier must reach Python's `/internal` notify
(answered 200, so Python accepted Rust's service token) and the linked owner's text must be `recorded`. G02 is a fallback used only when Rust has no outreach route:
the same outreach on the Python port. G03 the approval is in Rust's approvals list, and its payload and hash
equal Python's `call_payload` / `payload_hash`; a call while it is `PENDING` is refused (409) by both services. G04 approve through Rust; Python reads `APPROVED`. G05
`POST .../calls` through the Rust origin: a labelled simulation with fixture telephony. Nothing more is driven
on the call. G06 Python and Rust then refuse a second execution of that approval (409); without a Rust calls
route, the check records what Python's route does instead.

**H. Hygiene.** H01 no file the run keeps contains `AUTH_SECRET`, the Photon secret, any bearer token the
driver sent (or its signature), or any token signed with `AUTH_SECRET`, whoever minted it.

## Lint

`apps/api/.venv/bin/ruff check --config apps/api/pyproject.toml apps/backend/e2e` and
`ruff format --check` with the same `--config`. Ruff finds no configuration of its own under `apps/backend`, so
the repository settings must be passed explicitly. `bash -n apps/backend/e2e/run.sh`.

## Not covered

No browser and no web app. No live provider, no real model (the model-free supervisor only), no real Photon,
Telnyx or Nessie. Not exercised at all: cross-project isolation (a record of one project used under another
project's URL), the Telnyx webhook and the call after `DIALING`, handoffs, the Nessie and media routes, the
Studio routes the forwarder carries, concurrent writers of both services on one project, a restart of either
service while the other runs.
