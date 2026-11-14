from __future__ import annotations

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from pydantic import ValidationError

from app.contracts import InvalidStateTransition, ProblemDetail
from app.core.errors import AppError
from app.core.logging import get_logger, request_id_var

log = get_logger(__name__)


def _problem(status: int, code: str, message: str, details=None, retryable=False) -> JSONResponse:
    body = ProblemDetail(
        error_code=code,
        message=message,
        details=details or {},
        request_id=request_id_var.get(),
        retryable=retryable,
    )
    return JSONResponse(status_code=status, content=body.model_dump(mode="json"))


def install_error_handlers(app: FastAPI) -> None:
    @app.exception_handler(AppError)
    async def app_error(_: Request, exc: AppError):
        return _problem(exc.status_code, exc.error_code, exc.message, exc.details, exc.retryable)

    @app.exception_handler(InvalidStateTransition)
    async def bad_transition(_: Request, exc: InvalidStateTransition):
        return _problem(
            409, "INVALID_STATE_TRANSITION", str(exc), {"current": exc.current, "target": exc.target}
        )

    @app.exception_handler(RequestValidationError)
    async def req_validation(_: Request, exc: RequestValidationError):
        return _problem(422, "VALIDATION_ERROR", "request validation failed", {"errors": exc.errors()})

    @app.exception_handler(ValidationError)
    async def model_validation(_: Request, exc: ValidationError):
        return _problem(
            422,
            "CONTRACT_VALIDATION_ERROR",
            "contract validation failed",
            {"errors": exc.errors(include_url=False, include_context=False)},
        )

    @app.exception_handler(Exception)
    async def unhandled(_: Request, exc: Exception):
        log.exception("unhandled error")
        return _problem(500, "INTERNAL_ERROR", "unexpected server error", retryable=True)
