# Infrastructure decisions

| Area | Choice | Version (pinned via lockfiles) |
|---|---|---|
| Python | 3.12, uv (`apps/api/uv.lock`) | uv 0.11.32 |
| API | FastAPI + Uvicorn, Pydantic v2 + pydantic-settings | fastapi 0.162.7, pydantic 2.14.3 |
| DB | PostgreSQL 16, SQLAlchemy 2.1, Alembic, psycopg 3 | sqlalchemy 2.1.8, alembic 1.20.1 |
| Agents | deepagents, langchain, langgraph, langgraph-checkpoint-postgres, langchain-anthropic | deepagents 0.7.38, langgraph 1.2.22 |
| Geometry / optimization | Shapely, OR-Tools CP-SAT | shapely 2.2.0, ortools 9.15 |
| Queue | Redis list + `app/workers/worker.py` (no Celery) | redis-py 8.1 |
| Web | Next.js App Router, React, TS, Tailwind v4, lucide, react-konva | next 16.4.6, react 19.3.1, ts 5.9.3 |
| Contracts → TS | openapi-typescript + openapi-fetch | 7.13.0 / 0.17.0 |
| JS packages | pnpm workspace (`pnpm-lock.yaml`) | pnpm 10.28 |
| Containers | Docker Compose (`infra/compose.yaml`): postgres, redis, api, worker, web (profile) | |
| Deploy | Any VM + Caddy (TLS, SSE/WS flush) — `infra/oracle/` runbook; nothing provisioned | |
| Logging | JSON to stdout with request IDs; secret-like keys redacted | |
| Auth | HMAC bearer tokens (dev); replace with IdP for production | |

Not introduced on purpose: Kubernetes, per-agent services, Celery, a vector DB, heavy ML in the default runtime
(IfcOpenShell is an optional extra: `uv sync --extra ifc`). shadcn/ui is represented by hand-written primitives in
`apps/web/src/components/ui` (CLI init needs network to ui.shadcn.com; adopt later).
