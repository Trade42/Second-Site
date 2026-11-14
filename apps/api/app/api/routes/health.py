from __future__ import annotations

from fastapi import APIRouter, Depends
from sqlalchemy import text

from app.api.deps import container
from app.contracts import HealthResponse, IntegrationStatus, ReadyResponse
from app.core.container import Container
from app.db.session import get_engine
from app.integrations import xai_voice
from app.integrations.base import status_entry

router = APIRouter(tags=["health"])


@router.get("/health", response_model=HealthResponse)
def health(c: Container = Depends(container)) -> HealthResponse:
    return HealthResponse(status="ok", version=c.settings.app_version)


@router.get("/ready", response_model=ReadyResponse)
def ready(c: Container = Depends(container)) -> ReadyResponse:
    checks: dict[str, str] = {}
    try:
        with get_engine().connect() as conn:
            conn.execute(text("select 1"))
        checks["database"] = "ok"
    except Exception as exc:  # noqa: BLE001
        checks["database"] = f"error: {type(exc).__name__}"
    if c.settings.task_execution_mode == "redis":
        try:
            import redis

            redis.Redis.from_url(c.settings.redis_url).ping()
            checks["redis"] = "ok"
        except Exception as exc:  # noqa: BLE001
            checks["redis"] = f"error: {type(exc).__name__}"
    missing = c.settings.missing_for_live()
    if missing:
        checks["live_config"] = "missing: " + "; ".join(f"{k}={','.join(v)}" for k, v in missing.items())
    providers = [
        c.costar.status(),
        c.permits.status(),
        c.matterport.status(),
        c.vexcel.status(),
        c.photon.status(),
        c.telephony.status(),
        xai_voice.status(bool(c.settings.xai_api_key)),
        c.catalog.status(),
        status_entry(
            "langchain_deep_agents",
            "live" if c.settings.supervisor_model else "deterministic",
            IntegrationStatus.PARTIAL if c.settings.supervisor_model else IntegrationStatus.MOCK,
            bool(c.settings.anthropic_api_key),
            "create_deep_agent supervisor; deterministic fallback when SUPERVISOR_MODEL unset",
        ),
    ]
    ok = checks.get("database") == "ok" and all(v == "ok" for k, v in checks.items() if k == "redis")
    return ReadyResponse(status="ready" if ok else "degraded", checks=checks, providers=providers)
