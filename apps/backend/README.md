# Second Site backend (Rust)

A Rust replacement for the Python backend in `apps/api`, **in progress**. The Python code is the
specification: the port keeps its HTTP surface, JSON field names, enum strings, validation rules,
PostgreSQL schema (alembic head) and token format, so the web app and the generated TypeScript
contracts work unchanged. Photon (owner messaging) and the LangChain supervisor stay in Python.

## Crates

| Crate | Path | Contents |
|---|---|---|
| `ss-core` | `crates/core` | Ids, UTC timestamps, money, shared enums, the state-transition helper, the project and business-profile contracts, `ValidationError`. |
| `ss-domain` | `crates/domain` | Domain contracts and deterministic engines, no HTTP and no database: evidence and its validator, permits and permit matching, property identity, requirements, feasibility, finance, ranking, geometry, layout validation, the layout solver, bill of materials, the fixture catalog, the fixture and data-kit providers. |
| `ss-server` | `crates/server` | The binary `second-site-backend`: axum HTTP API, PostgreSQL and in-memory stores, auth, events, tasks, approvals, property search and analysis, the Matterport, Vexcel and LA permit adapters, media, the design routes. |
| `ss-nessie` | `crates/nessie` | Capital One Nessie v2 sandbox client and fixture bank (mock money only). Not used by the service yet. |

Dependency direction: `ss-server` -> `ss-domain` -> `ss-core`; `ss-nessie` -> `ss-domain`.

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
- Like the Python app, the service reads `.env` and `../../.env` relative to its working directory (`apps/backend`, so the repository `.env`); the environment wins.
- On PostgreSQL the schema is migrated to alembic head at start-up: alembic's own SQL, alembic's `alembic_version` bookkeeping, a no-op when already at head. `second-site-backend migrate` only migrates.
- Run the Python app on another port (for example 8001) when both run together. They must share `DATABASE_URL` and `AUTH_SECRET`: a token minted by either service verifies in the other.

## Run with Docker Compose

```
docker compose -f infra/compose.yaml up --build                 # postgres, redis, api, backend, worker
docker compose -f infra/compose.yaml --profile web up --build   # plus the web app on :3000
```

| Service | Inside the network | Host port | Calls |
|---|---|---|---|
| `backend` (Rust, `infra/docker/backend.Dockerfile`) | `backend:8000` | 8000 (`BACKEND_HOST_PORT`) | postgres; `PYTHON_SERVICE_URL=http://api:8000` for owner texts and forwarding, once built |
| `api` (Python) | `api:8000` | 8001 (`API_HOST_PORT`), for debugging | postgres, redis; `BACKEND_API_URL=http://backend:8000` for the supervisor tools |
| `web` | `web:3000` | 3000 | the browser calls `http://localhost:8000`, the Rust service |
| `postgres`, `redis`, `worker` | unchanged | 5432, 6379, none | |

Details:
- **Order.** `backend` starts after `api` is healthy, because `api` runs `alembic upgrade head` first. The Rust start-up migration then finds the schema at head and does nothing. The two must not migrate an empty database at the same time: alembic takes no lock, so one of them would fail.
- **Settings.** Both services read the same `../.env`, so they share `AUTH_SECRET`. Compose sets the database, store, host, port, fixture directory and storage path of `backend` itself, so a local `.env` cannot change them inside the container.
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

The service reads every environment variable of `apps/api/app/core/config.py` under the same name, with the same default. It adds these:

| Name | Meaning | Default |
|---|---|---|
| `STORE_BACKEND` | `postgres` or `memory` (in-memory, not durable; `/ready` says `degraded`) | `postgres` |
| `HOST`, `PORT` | listen address (uvicorn's command-line flags in Python) | `0.0.0.0`, `8000` |
| `FIXTURES_DIR` | directory with `properties/`, `permits/` and `geometry/` fixtures, read at run time | the repository's `fixtures/` (image: `/app/fixtures`) |
| `PYTHON_SERVICE_URL` | the Python app's origin (no `/api/v1`): owner texts and the forwarded paths | unset (neither is built yet) |

Notes:
- `DATABASE_URL` accepts `postgresql://` and SQLAlchemy's `postgresql+psycopg://`.
- `TASK_EXECUTION_MODE` must be `inline`: the Redis worker is not ported, and the service refuses to start otherwise.
- `APP_ENV=production` with the development `AUTH_SECRET` is refused.
- `NESSIE_PROVIDER_MODE`, `NESSIE_API_KEY` and `NESSIE_BASE_URL` belong to `ss-nessie`, sandbox only.
- `.env.example` lists every name.

## What still runs in Python

The Rust service forwards these to the Python app once the forwarder lands (server part 5); until then they exist only there:
- **Photon:** `/api/v1/owner/channel` (GET, PUT), `.../channel/poll`, `.../channel/simulate-text`, `/webhooks/photon`.
- **Chat:** `POST /api/v1/projects/{id}/chat`.
- **The call flow, until it is ported:** outreach, communications, calls, call simulation, handoffs and their responses, `/webhooks/telnyx`.

`/internal/v1/owner/notify` is a service-to-service route in Python and is never exposed through the Rust service.

## Honest limits

- **Fixture data:** the default providers serve fixture data, and every record says so.
  - Listings are synthetic (`FIXTURE_SYNTHETIC`), not CoStar records.
  - Permits are a stored snapshot of public LA records (`FIXTURE_REAL`), historical, not current capacity.
  - Floor geometry is a synthetic scan.
  - Catalog prices are illustrative, not supplier quotes.
- **Live adapters** (Matterport, Vexcel, LA Open Data, the CoStar data kit) have only been exercised against mock servers.
- **Layout solver:** valid layouts, but not coordinate-identical to the Python OR-Tools solver. Layouts are automated proposals, not approved or code-reviewed plans.
- **Not built yet:**
  - the forwarder to Python;
  - the call flow in Rust;
  - the evidence routes the supervisor's HTTP tools call;
  - the Nessie routes;
  - the Redis task worker.
- **Not tested here:** the Docker image and the compose stack have been checked statically only, not built or run in this repository's CI (CI runs `docker compose config -q`).

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
