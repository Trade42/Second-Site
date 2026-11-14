"""Second Site API entry point."""

from __future__ import annotations

import time
import uuid

from fastapi import FastAPI, Request
from fastapi.middleware.cors import CORSMiddleware

from app.api.errors import install_error_handlers
from app.api.routes import (
    approvals,
    communications,
    design,
    dev,
    events,
    health,
    media,
    projects,
    properties,
    tasks,
    webhooks,
)
from app.contracts import SCHEMA_VERSION
from app.core.config import get_settings
from app.core.logging import configure_logging, get_logger, request_id_var

# Register event subscribers (import side effect).
from app.workflows import property_analysis  # noqa: F401

settings = get_settings()
configure_logging(settings.log_level)
log = get_logger("app.request")

app = FastAPI(
    title="Second Site API",
    version=settings.app_version,
    description="Contract-first backend for Second Site. Schemas are generated from Pydantic contracts.",
    separate_input_output_schemas=True,
)
app.add_middleware(
    CORSMiddleware,
    allow_origins=settings.cors_origin_list,
    allow_credentials=True,
    allow_methods=["*"],
    allow_headers=["*"],
)
install_error_handlers(app)


@app.middleware("http")
async def request_context(request: Request, call_next):
    rid = request.headers.get("x-request-id") or str(uuid.uuid4())
    token = request_id_var.set(rid)
    start = time.perf_counter()
    try:
        response = await call_next(request)
    finally:
        request_id_var.reset(token)
    response.headers["x-request-id"] = rid
    log.info(
        "request",
        extra={
            "fields": {
                "method": request.method,
                "path": request.url.path,
                "status": response.status_code,
                "request_id": rid,
                "ms": round((time.perf_counter() - start) * 1000, 1),
            }
        },
    )
    return response


def custom_openapi() -> dict:
    """OpenAPI + every canonical contract (incl. ones not used by a route, e.g. event payloads)."""
    if app.openapi_schema:
        return app.openapi_schema
    from fastapi.openapi.utils import get_openapi

    from app.contracts.export import all_contract_schemas

    schema = get_openapi(
        title=app.title,
        version=app.version,
        description=app.description,
        routes=app.routes,
        separate_input_output_schemas=True,
    )
    comps = schema.setdefault("components", {}).setdefault("schemas", {})
    for name, sch in all_contract_schemas().items():
        comps.setdefault(name, sch)
    schema["info"]["x-schema-version"] = SCHEMA_VERSION
    app.openapi_schema = schema
    return schema


app.openapi = custom_openapi  # type: ignore[method-assign]

for r in (
    health.router,
    dev.router,
    projects.router,
    properties.router,
    design.router,
    communications.router,
    approvals.router,
    media.router,
    media.imagery_router,
    tasks.router,
    events.router,
    webhooks.router,
):
    app.include_router(r)
