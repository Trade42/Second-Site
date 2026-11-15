# Second Site backend (Rust)

A Rust replacement for the Python backend in `apps/api`, **in progress**. The Python code is the
specification: the port keeps its HTTP surface, JSON field names, enum strings, validation rules,
PostgreSQL schema (alembic head) and token format, so the web app and the generated TypeScript
contracts work unchanged. Photon (owner messaging) and the LangChain supervisor stay in Python,
and so does a teammate's Studio router. The Rust service forwards their routes to the Python app.

## Crates

| Crate | Path | Contents |
|---|---|---|
| `ss-core` | `crates/core` | Ids, UTC timestamps, money, shared enums, the state-transition helper, the project and business-profile contracts, `ValidationError`. |
| `ss-domain` | `crates/domain` | Domain contracts and deterministic engines, no HTTP and no database: evidence and its validator, permits and permit matching, property identity, requirements, feasibility, finance, ranking, geometry, layout validation, the layout solver, bill of materials, the fixture catalog, the fixture and data-kit providers. Also the communications contracts and state machines, the call script, answer extraction and escalation rules, the Telnyx client state and fixture telephony provider, the xAI session builder, and the Photon and Telnyx webhook signature checks. |
| `ss-server` | `crates/server` | The binary `second-site-backend`: axum HTTP API, PostgreSQL and in-memory stores, auth, events, tasks, approvals, property search and analysis, the evidence routes, the Matterport, Vexcel and LA permit adapters, media, the design routes. The call flow: outreach, calls, the call simulator, handoffs, the Telnyx webhook and the live Telnyx client. Owner texts through the Python app, the forwarder to the Python app, and the Nessie routes. |
| `ss-nessie` | `crates/nessie` | Capital One Nessie v2 sandbox client, fixture bank, move-in payment plan, affordability and approval payloads (mock money only). `ss-server` uses it for the Nessie routes. |

Dependency direction: `ss-server` -> `ss-nessie` -> `ss-domain` -> `ss-core`. A crate may also use
the ones further down directly.

## Build and test

Rust stable, from `apps/backend`:

```
cargo build --release --locked -p ss-server      # target/release/second-site-backend
cargo test --workspace                           # in-memory store; PostgreSQL tests print SKIPPED
TEST_DATABASE_URL=postgres://secondsite:secondsite@localhost:5432/secondsite_test cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- With `TEST_DATABASE_URL` set, every PostgreSQL test creates its own schema, migrates it and drops it; the database itself is not reset. A test that prints `SKIPPED` did not run.
- Tests read `fixtures/` and `packages/contracts/schemas/` from the repository.
- `ss-domain` is compiled with `opt-level = 1` in the dev and test profiles (`Cargo.toml`). Its tests run about 3-4 times faster that way, and its library rebuilds about as fast as unoptimised.
- The rest of a debug build is unoptimised, and the layout solver is about 14 times slower in a debug build of the service than in a release build. Run the service from a release build.

## Run locally

```
apps/backend/scripts/run-local.sh            # PostgreSQL: needs DATABASE_URL
apps/backend/scripts/run-local.sh --memory   # in-memory demo store, no database, nothing durable
```

- The script builds the release binary and runs it in the foreground on `127.0.0.1:8000` (`HOST` and `PORT` in the environment change that). It sets `STORE_BACKEND`, `HOST`, `PORT` and `FIXTURES_DIR` itself, so a line for one of those four in a `.env` file has no effect here.
- It prints which store, database, fixture directory and address are in use.
- It refuses to start instead of falling back when `DATABASE_URL` is missing or `FIXTURES_DIR` does not hold the fixture files.
- Like the Python app, the service reads `.env` and `../../.env` relative to its working directory (`apps/backend`, so the repository `.env`); the environment wins. The `NESSIE_*` names are read the same way. The reading follows python-dotenv, including `${NAME}` interpolation, quoted values over several lines and lone CR line breaks. A `.env` or `../../.env` that exists but cannot be read, or is not valid UTF-8, stops the start-up with an error (as pydantic-settings does).
- On PostgreSQL the schema is migrated to alembic head at start-up: alembic's own SQL, alembic's `alembic_version` bookkeeping, a no-op when already at head. Then the service creates its own table, `nessie_simulation_records`, if it is missing. `second-site-backend migrate` only migrates.
- Run the Python app on another port (for example 8001) when both run together. They must share `DATABASE_URL` and `AUTH_SECRET`: a token minted by either service verifies in the other. See "Switching over".

## Run with Docker Compose

```
docker compose -f infra/compose.yaml up --build                 # postgres, redis, api, backend, worker
docker compose -f infra/compose.yaml --profile web up --build   # plus the web app on :3000
```

| Service | Inside the network | Host port | Calls |
|---|---|---|---|
| `backend` (Rust, `infra/docker/backend.Dockerfile`) | `backend:8000` | 8000 (`BACKEND_HOST_PORT`) | postgres; `PYTHON_SERVICE_URL=http://api:8000` for owner texts and the forwarded routes |
| `api` (Python) | `api:8000` | 8001 (`API_HOST_PORT`), for debugging | postgres, redis; `BACKEND_API_URL=http://backend:8000` for the supervisor tools |
| `web` | `web:3000` | 3000 | the browser calls `http://localhost:8000`, the Rust service |
| `postgres`, `redis`, `worker` | unchanged | 5432, 6379, none | |

Details:
- **Order.** `backend` starts after `api` is healthy, because `api` runs `alembic upgrade head` first. The Rust start-up migration then finds the schema at head and only adds its own table. The two must not migrate an empty database at the same time: alembic takes no lock, so one of them would fail.
- **Settings.** Both services get the lines of the same `../.env` in their environment (`env_file`), so they share `AUTH_SECRET`, and the `NESSIE_*` lines reach the Rust service. Compose sets the database, store, host, port, fixture directory and storage path of `backend` itself, so a local `.env` cannot change them inside the container.
- **Uploads.** Uploads land in the `backend-objects` volume. Python reads stored objects only in the media routes, which the Rust service now serves.

**Python-only stack (the old behaviour, Python on 8000):**

```
API_HOST_PORT=8000 API_BACKEND_API_URL= docker compose -f infra/compose.yaml up --build postgres redis api worker
```

Add `web` to the service list for the web app. `API_BACKEND_API_URL=` (empty) gives the supervisor its in-process tools again. Stop the default stack first (`docker compose -f infra/compose.yaml down`): a running `backend` container keeps host port 8000.

**Health.** The image has no curl. Its `HEALTHCHECK` and the compose health check run `scripts/healthcheck.sh` (bash `/dev/tcp`, `GET /health`, expects 200 and `"status":"ok"`).

## Smoke test

```
apps/backend/scripts/smoke.sh [http://localhost:8000]    # against a running service
apps/backend/scripts/smoke-selftest.sh                   # builds, runs and stops the service itself
```

**`smoke.sh`** follows the demo path the way the web app seeds it, one line per step, and stops at the first failed assertion:
- health and ready;
- dev token;
- fixture project with its confirmed profile;
- property search and analysis;
- properties, permit evidence and UNKNOWN assessments;
- cost scenarios, ranking and events;
- layouts for the best-ranked property with measured geometry, then the task result, the layout list, the geometry, one revision and the bill of materials.

It asserts the truth rules:
- fixture data is labelled as fixture data;
- UNKNOWN is counted as unknown in the ranking;
- money is integer cents;
- layouts are `PROPOSED`, never `APPROVED`;
- bill prices are fixture prices, not supplier quotes.

**`smoke-selftest.sh`** runs `smoke.sh` twice against the release binary:
- **Setup.** The binary listens on `127.0.0.1`, on a free port, in fixture mode, with a clean environment and no `.env`.
- **Passes.** First on the in-memory store, then on PostgreSQL in a private schema that it creates and drops (`SMOKE_PG_URL`, `PSQL`; `--memory-only` skips this pass).
- **Cleanup.** A trap stops the service on every exit path.

## Settings

The service reads every environment variable of `apps/api/app/core/config.py` under the same name, with the same default, except `BACKEND_API_URL` (the Python app's). It adds these:

| Name | Meaning | Default |
|---|---|---|
| `STORE_BACKEND` | `postgres` or `memory` (in-memory, not durable; `/ready` says `degraded`) | `postgres` |
| `HOST`, `PORT` | listen address (uvicorn's command-line flags in Python) | `0.0.0.0`, `8000` |
| `FIXTURES_DIR` | directory with `properties/`, `permits/` and `geometry/` fixtures, read at run time | the repository's `fixtures/` (image: `/app/fixtures`) |
| `PYTHON_SERVICE_URL` | the Python app's origin (no `/api/v1`): owner texts and the forwarded routes | unset: the forwarded routes answer 503, owner texts are recorded as not sent |
| `NESSIE_PROVIDER_MODE`, `NESSIE_API_KEY`, `NESSIE_BASE_URL` | the Nessie bank (see "Switching over", step 6) | empty: the fixture bank; base URL `https://v2-prod-api.nessieisreal.com` |

Notes:
- `DATABASE_URL` accepts `postgresql://` and SQLAlchemy's `postgresql+psycopg://`.
- `TASK_EXECUTION_MODE` must be `inline`: the Redis worker is not ported, and the service refuses to start otherwise.
- `APP_ENV=production` with the development `AUTH_SECRET` is refused.
- A blank `PYTHON_SERVICE_URL` counts as unset.
- Some names are read but used, if at all, only in `/ready`: the `PHOTON_*` names, `SUPERVISOR_MODEL`, `ANTHROPIC_API_KEY`, `AGENT_CHECKPOINTER`, `REDIS_URL`, `MATTERPORT_SDK_KEY` and `ALLOW_LIVE_PURCHASES`. The Photon and supervisor names belong on the Python app.
- `.env.example` lists every name.

## What still runs in Python

The Rust service forwards these routes to the Python app (`crates/server/src/http/routes/forward.rs`):
- **Photon:** `GET` and `PUT /api/v1/owner/channel`, `POST /api/v1/owner/channel/simulate-text`, `POST /api/v1/owner/channel/poll`, `POST /webhooks/photon`.
- **Chat:** `POST /api/v1/projects/{project_id}/chat`.
- **Studio**, a teammate's Python router, forwarded unchanged. The router is not in `apps/api` yet, so until it lands the Python app answers these with 404:
  - `GET /api/v1/studio/status`, `GET /api/v1/studio/sample`;
  - `POST /api/v1/studio/plans`, `GET /api/v1/studio/plans/{plan_id}`;
  - `POST .../plans/{plan_id}/mutate`, `POST .../plans/{plan_id}/propose`, `GET .../plans/{plan_id}/bom`, `GET .../plans/{plan_id}/bom.csv`;
  - `POST /api/v1/studio/products/search`, `POST /api/v1/studio/products/import`;
  - `POST /api/v1/studio/contractors/discover`, `POST /api/v1/studio/scopes`.

Every other route of the Python app is native in Rust, the call flow and `/webhooks/telnyx`
included, except `/internal/v1/owner/notify` (below).

**What is passed.**
- The method, the path, the raw query string, the raw body bytes and every end-to-end header (`authorization`, `content-type`, the webhook signature headers, cookies) go through unchanged. `x-request-id` is this request's id.
- The Rust service makes no token or project check on these routes; Python makes its own, with the same tokens.
- The answer comes back with its status, body and headers. Python's CORS headers are dropped: the Rust service's CORS policy is the only one.

**Rules.**
- The lists are explicit. A path must match its template segment by segment, or the answer is the ordinary 404 `{"detail": "Not Found"}` and nothing is forwarded. A method not on the list gets 405, `HEAD` included.
- A native Rust route of the same path and method always wins.

**Limits.**
- Request body: 2 MiB. Larger is 413 `PAYLOAD_TOO_LARGE`, not forwarded.
- Answer: 8 MiB. Larger is 502.
- Connect timeout 3 s. Overall timeout 60 s for the chat and the Studio routes, 15 s for the rest.
- Every request body, on every route (native or forwarded), is read under a 30 s idle bound between reads. A body that stops arriving is answered 408 `REQUEST_TIMEOUT`, retryable; on a forwarded route nothing was forwarded.

**When the Python app cannot answer.** Never a 2xx, so a provider whose webhook was not forwarded retries it:
- `PYTHON_SERVICE_URL` unset, blank, not a usable `http(s)` URL, or one that leads back to this service (it names this service's own `HOST`/`PORT`, or its `/health` was answered by this service): 503 `PYTHON_SERVICE_NOT_CONFIGURED`, not retryable, nothing sent.
- A forwarded request that arrives back at this service while it is forwarding that same request: 508 `PYTHON_SERVICE_LOOP`, not forwarded again.
- Connection refused or failed, answer cut off or too large: 502 `PYTHON_SERVICE_UNAVAILABLE`, retryable.
- Connect or overall timeout: 504 `PYTHON_SERVICE_UNAVAILABLE`, retryable. When the request had already been sent, the message says that whether it took effect is unknown.

`/internal/v1/owner/notify` is a service-to-service route in Python. The Rust service calls it to text the owner and never forwards anything under `/internal` (it answers 404).

## Switching over

How to run the two services together and point the web app at the Rust service. The end-to-end harness (step 8) ran steps 1, 2 (without the Telnyx key), 3 and 5 with both services on one machine: loopback, fixture providers, no TLS. Steps 4 and 6 and the Docker commands of step 7 have not been run. Nobody has switched a real deployment.

**1. One database, one `AUTH_SECRET`, the schema first.**
- Both services use the same `DATABASE_URL` and the same `AUTH_SECRET`. A token minted by either verifies in the other.
- Apply the schema once with the Python app's alembic: `(cd apps/api && uv run alembic upgrade head)`. The Python app never migrates at start-up.
- Then start the Rust service. Its start-up migration finds alembic head (log line `schema is at alembic head`) and only adds its own table.
- Never let alembic and a first start of the Rust service migrate an empty database at the same moment. Alembic takes no lock, and the Rust service's migration lock only orders Rust instances. In a real overlap the Rust service exits with a duplicate-key error ("could not open the store"). Compose orders them: `backend` waits until `api` is healthy, and `api` runs alembic before it serves.
- A Rust binary older than the alembic head logs "database is at an alembic revision this binary does not know; not migrating" and keeps serving. A new alembic revision therefore needs a Rust build that knows it.

**2. Rust service settings.**
- `DATABASE_URL` and `AUTH_SECRET`, as above.
- `PYTHON_SERVICE_URL`: the Python app's origin, `http(s)://host:port`, without `/api/v1`, credentials, query or fragment.
- `CORS_ORIGINS`: the web app's origin.
- `PUBLIC_BASE_URL`: the Rust origin (public HTTPS in production). When a call is dialled, the carrier is told to send its events to `{PUBLIC_BASE_URL}/webhooks/telnyx`, and the owner's recorded intro is played from `{PUBLIC_BASE_URL}/api/v1/media/public/...`. Both routes are in Rust.
- `TELNYX_PUBLIC_KEY`, if the Telnyx webhook is used. Without it every delivery is refused.
- Run a release build.
- **Check `/ready` before switching traffic.** Its entry `python_service` must say `"mode": "forward"`, `"status": "LIVE"`: the last `GET {PYTHON_SERVICE_URL}/health` answered 2xx.
  - The service checks that every 5 s, with a 2 s timeout. In the first seconds after start-up the entry is `NOT_STARTED` ("has not been answered yet"), or `BLOCKED` if the first check ran before the Python app was up. It turns `LIVE` within about 5 s of the Python app answering.
  - Unset, it is `not_configured` / `NOT_STARTED`. A failed check, or a last check older than 17 s (three intervals plus the timeout), is `BLOCKED`.
  - It is also `BLOCKED` from start-up when the URL names this service's own `HOST`/`PORT`, and `BLOCKED` while `/health` is answered by this service (its `/health` carries the header `x-second-site-backend: rust`, which Python's lacks).
  - The overall `status` of `/ready` reflects only the database.

**3. Python app settings.**
- `BACKEND_API_URL`: the Rust origin (no `/api/v1`), so the supervisor's tools call the Rust API. Empty means the tools run in-process.
- `PHOTON_WEBHOOK_SECRET` and the other `PHOTON_*` names: Python verifies Photon's webhook on the raw body the Rust service forwards.
- `PUBLIC_BASE_URL`: the same Rust origin. Python's own call code still runs when an owner answers a handoff by text (see "Honest limits"), and it builds the same Telnyx webhook and intro links.
- **Keep the Python app unreachable from outside.** `/internal/v1/owner/notify` is a service-to-service route; it accepts a `service`-purpose token signed with `AUTH_SECRET`. The Rust service never forwards `/internal`. `infra/compose.yaml` publishes the Python app on host port 8001 for debugging, which Docker binds on every host interface by default: remove that mapping, or bind it to `127.0.0.1`, anywhere but a developer machine.

**4. Web app.**
- Unchanged. `NEXT_PUBLIC_API_BASE_URL` is the Rust origin (default `http://localhost:8000`, `apps/web/src/lib/api.ts`).
- Next.js writes `NEXT_PUBLIC_*` values into the browser bundle at `next build`. The web image is built without the variable, so the browser uses that default. Set it at build time for any other origin.

**5. Photon webhook.** At Photon, set the webhook URL to the Rust origin's `/webhooks/photon`. The Rust service forwards it with the raw body and the `webhook-id`, `webhook-timestamp` and `webhook-signature` headers unchanged; Python verifies and dedupes it. The harness ran this with a signed test webhook, not with Photon itself.

**6. Nessie (mock money only).**
- Set `NESSIE_PROVIDER_MODE`, `NESSIE_API_KEY` and `NESSIE_BASE_URL` like any other setting: in the environment, or in `.env` / `../../.env` (with `run-local.sh` or a bare run in `apps/backend`, that is the repository `.env`; compose passes it through `env_file`). A `live` line in the repository `.env` makes a local run use the real sandbox.
- Empty, `fixture` or `disabled`: the in-memory fixture bank. `/ready` entry `nessie`: `MOCK`.
- `live` with `NESSIE_API_KEY`: the Capital One Nessie sandbox. `/ready` says `PARTIAL` (`CONFIGURED_UNVERIFIED`) until an authenticated call succeeds, then `LIVE`.
- `live` without a key: the fixture bank, and `/ready` says `BLOCKED`.
- Any other value: every Nessie route answers 503 `NESSIE_MISCONFIGURED`, and `/ready` says `BLOCKED`. There is no fallback.
- The key is sent only in the `Authorization` header, and never logged or returned.

**7. Docker (never run).** Nobody has built these images or started this stack.
- The build context is the repository root (`context: ..` in compose, `.` below). `.dockerignore` keeps out only `.git`, `.env`, `.env.*`, `.data` and `apps/backend/target`.
- Every other file in the checkout goes to the Docker daemon, including untracked local folders: a virtual environment, `node_modules`, editor or tool folders.
- Build from a clean clone of a commit that holds all of `apps/backend`.

From the repository root, with Docker Compose 2.24 or newer. The comments say what each command should print; none of them has been run:

```
docker compose -f infra/compose.yaml config -q                                   # no output, exit 0 (the CI job)
API_HOST_PORT=8000 API_BACKEND_API_URL= docker compose -f infra/compose.yaml config | grep -E 'BACKEND_API_URL|published'
                                                                                 # BACKEND_API_URL: "" and published "8000" for api
docker build -f infra/docker/backend.Dockerfile -t second-site-backend:local .   # succeeds; a few minutes cold
docker run --rm --entrypoint id second-site-backend:local                        # uid=10001(secondsite) gid=10001(secondsite)
docker run --rm --entrypoint ls second-site-backend:local -l /app/fixtures/permits /data   # readable snapshot; objects owned by secondsite
docker run -d --name ssb -e STORE_BACKEND=memory -p 127.0.0.1:8000:8000 second-site-backend:local
sleep 20; docker inspect --format '{{.State.Health.Status}}' ssb                 # healthy
apps/backend/scripts/smoke.sh http://localhost:8000                              # SMOKE TEST PASSED, after 21 steps
docker rm -f ssb
docker compose -f infra/compose.yaml down -v        # throw-away setup only: deletes the stack's volumes
docker compose -f infra/compose.yaml up --build -d
docker compose -f infra/compose.yaml ps                                          # api and backend "healthy"; backend started after api
docker compose -f infra/compose.yaml logs backend | grep -E 'schema is at alembic head'   # present; no "could not open the store"
apps/backend/scripts/smoke.sh http://localhost:8000                              # SMOKE TEST PASSED, against the Rust service
curl -s http://localhost:8001/health                                             # the Python API, debug port
docker compose -f infra/compose.yaml exec api printenv BACKEND_API_URL           # http://backend:8000
docker compose -f infra/compose.yaml down
API_HOST_PORT=8000 API_BACKEND_API_URL= docker compose -f infra/compose.yaml --profile web up --build -d postgres redis api worker web
scripts/smoke-test.sh                                                            # the Python smoke test passes on :8000
docker compose -f infra/compose.yaml ps backend                                  # no container
```

**8. Acceptance.**
- **`bash apps/backend/scripts/smoke-selftest.sh`** checks the Rust service on its own: the demo path and its truth labels (see "Smoke test"), on both stores. It does not start the Python app, and it does not touch forwarding, the call flow or Nessie.
- **`bash apps/backend/e2e/run.sh`** checks the two services together. It starts the Rust release binary and the Python app on loopback, on one private PostgreSQL schema with one `AUTH_SECRET`, each pointed at the other, fixture providers only. It then runs 69 checks:
  - tokens minted by either service are accepted by the other;
  - rows written through one service read the same through the other;
  - the supervisor's tools over HTTP against the Rust API;
  - owner texts through the notify route;
  - the chat, the owner channel and a signed Photon webhook through the Rust origin;
  - an outreach, its approval and a call (fixture telephony) through the Rust origin, with single use holding in the Python app too;
  - no secret in either log.
- Measured at commit `036d526`, it ended `67 PASS, 1 FAIL, 1 SKIP` (exit 1). The SKIP is a fallback that runs only when the Rust service has no outreach route.
- The FAIL is check D12, the one known difference. Free-form JSON maps come back from the Rust service with their keys sorted, for example `business_profile.space_requirements.required_rooms`. So the supervisor's `get_project_state` text differs in key order between in-process and HTTP mode. The JSON is equal. D12 can also fail on the order of the feasibility list after a re-analysis, an order both services inherit from Python.
- Not covered: a browser and the web app, real providers, a real model, the simulated call after dialling, handoffs, the Telnyx webhook, the Nessie routes, and media.
- The harness is not portable yet. It expects the local test database and build setup of the machine it was written on, and elsewhere it stops at set-up with exit 2. `apps/backend/e2e/README.md` says what it needs.

**9. Going back.**
- Use the Python-only stack above. Outside compose: stop the Rust service, serve the Python app on the web app's API origin, set `BACKEND_API_URL` empty, and point the Photon webhook and `PUBLIC_BASE_URL` back at the Python app.
- Data written through the Rust service is in the same tables, columns and JSON documents, so the Python app reads it as its own. The harness compared projects, properties, permits, feasibility, rankings, cost scenarios, events, tasks, layouts, geometry and the bill of materials, read through both services.
- The one exception is the Rust-owned table `nessie_simulation_records`: the Nessie sandbox ids and simulated payment attempts.
  - Alembic does not create or migrate it, and the Python app never reads it. `apps/api/alembic/env.py` names it, so `alembic check` and autogenerate leave it alone.
  - Its foreign key to `projects` cascades on delete.
  - After going back the Nessie routes are gone. Their approvals stay as ordinary approvals, and nothing in Python executes them.

## Honest limits

- **Fixture data:** the default providers serve fixture data, and every record says so.
  - Listings are synthetic (`FIXTURE_SYNTHETIC`), not CoStar records.
  - Permits are a stored snapshot of public LA records (`FIXTURE_REAL`), historical, not current capacity.
  - Floor geometry is a synthetic scan.
  - Catalog prices are illustrative, not supplier quotes.
- **Live adapters** (Matterport, Vexcel, LA Open Data, the CoStar data kit) have only been exercised against mock servers.
- **Layout solver:** valid layouts, but not coordinate-identical to the Python OR-Tools solver. Layouts are automated proposals, not approved or code-reviewed plans.
- **Simulated broker calls.** `POST .../calls/{call_id}/simulate` plays the carrier. It feeds scripted, Telnyx-shaped events with canned broker answers into the same handlers as real webhooks.
  - It is refused (409 `SIMULATOR_DISABLED`) with the live voice provider. So is `POST .../calls` with `"simulate": true`, before the approval is spent and before any carrier request (Python differs: it placed a real call labelled simulated).
  - The call is `simulated: true`. Its evidence is `FIXTURE_SYNTHETIC`, with the limitation "SIMULATED CALL: scripted demo answer, not a real conversation.", and the owner's summary starts with "[Simulated call]".
- **Broker answers** on any call are owner-reported evidence (`OWNER_REPORTED`, "not independently verified"), never verified capacity. Answer extraction copies two Python defects: some negations ("Absolutely not") are read as yes, and long numbers are cut ("9,500,000 sf" gives 500,000).
- **Live Telnyx.** The live client exists behind the existing gates:
  - It is used only with `VOICE_PROVIDER_MODE=live` and `TELNYX_API_KEY`. A dial also needs `ALLOW_LIVE_OUTBOUND_CALLS=true` (otherwise 403) and an approval.
  - It has only been run against a local mock server. No real call has been placed.
  - A dial answer means only that the carrier accepted the request: the call is `DIALING`. Answered and completed come only from carrier webhooks, or from the simulator's events of the same shape.
- **No realtime voice bridge.** The xAI part is a session builder only (`/ready`: "realtime audio bridge not implemented"). The assistant speaks through Telnyx text-to-speech commands and hears through Telnyx transcription events.
- **An owner who answers a handoff by text** is handled by Photon in Python. Python runs its own call code and telephony provider against the shared tables, without the Rust service's locks and checks. This has not been run against calls the Rust service started. The web route `POST .../handoffs/{handoff_id}/response` is the Rust path.
- **Approvals.** Placing a call and executing a simulated Nessie payment each need an APPROVED approval of this project, for exactly this payload, unexpired, single use.
  - Enforced at the action boundary by `ApprovalService::require_approved` (`crates/server/src/services/approvals.rs`), which locks the approval row.
  - Called by `CallOrchestrator::start` before the dial (`services/call_orchestration.rs`), and by the Nessie execute flow before the bank call (`services/nessie.rs`).
  - For a call, `mark_executed` spends the approval in the same transaction. For a Nessie payment, the approval is checked and the payment claimed (`IN_FLIGHT`) in one committed transaction before the bank call, and spent in a second transaction after it. A refusal by the carrier or the bank leaves it APPROVED. A Nessie payment whose outcome is unknown spends it, and so does an `IN_FLIGHT` claim whose outcome was never recorded: it becomes `MAY_EXIST` when next read.
  - No route sends a message to a broker. A non-voice outreach stores a draft and a PENDING `SEND_MESSAGE` approval, and nothing executes it. Texts to the owner go through the Python app and need no approval, as in Python.
- **Nessie** is Capital One's sandbox: mock money, never a real bank account.
  - The default is an in-memory fixture bank, labelled `FIXTURE_IN_MEMORY_BANK` in every answer. It forgets everything on restart. A stored sandbox then reads as gone and a new set-up replaces it. A payment already sent against the old fixture bank still blocks its plan item.
  - The live client has only been run against a local stub. Nothing was sent to the real sandbox, over TLS or otherwise.
  - Unknown: whether the real sandbox puts `payer_id` on a bill payment. If it does not, every first-month payment is reported as `MAY_EXIST` (fails closed).
  - Nessie writes no task or event. The activity feed shows only `approval.requested` and `approval.decided`.
  - `ALLOW_LIVE_PURCHASES` is not consulted.
  - A graceful shutdown waits up to 30 s (by default) for executes between their two phases; one still in flight then becomes `MAY_EXIST`. Docker's default stop timeout is 10 s, shorter than that wait.
  - Refresh can settle a `MAY_EXIST` purchase as `SENT` when the sandbox account's transactions show exactly one matching purchase whose description carries the plan item and attempt id; otherwise it stays `MAY_EXIST`.
  - Instances that share a database need synchronised clocks (NTP). Another instance treats an `IN_FLIGHT` claim as dead 60 s after its bank call must have ended, so a clock difference of more than 60 s can settle a claim whose call is still running.
  - Do not run an older build and this one on one database at the same time: an older build cannot decode a payment row written by this one (500 on that plan item).
- **Owner texts are best effort.** They are sent after the request commits, on a background task, with no retry.
  - Python app down, or `PYTHON_SERVICE_URL` unset: the Rust service records a FAILED communication and a `communication.owner_notified` event with `delivery: "failed"`, and logs a warning.
  - Timeout or 5xx: the same records with `delivery: "unknown"`. The text may still go out.
  - A text in flight when the Rust service stops is lost, and no record of it exists (from the code; not tested).
- **Forwarded webhooks.** If the Python app cannot be reached, a Photon webhook gets a 5xx. Whether Photon then redelivers is unverified.
- **The Redis task worker** is not ported. Tasks run inline.
- **Not tested here:** the Docker image and the compose stack have been checked statically only, never built or run (CI runs `docker compose config -q`).

## Golden vectors

`tests-golden/` holds JSON vectors produced by running the Python implementation, and the scripts
that produced them (pure functions; the server slice runs the Python app on a private schema).
Regenerate after changing the Python specification, for example:

```
(cd apps/api && .venv/bin/python ../backend/tests-golden/gen_core.py)
(cd apps/api && .venv/bin/python ../backend/tests-golden/gen_domain.py)
```

`tests-golden/costar_kit_synthetic*/` are **synthetic** directories shaped like the CoStar data kit
(copied from the Python test suite). They are not CoStar data.
