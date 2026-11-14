"""Domain errors -> standardized ProblemDetail responses."""

from __future__ import annotations

from typing import Any


class AppError(Exception):
    status_code = 400
    error_code = "BAD_REQUEST"
    retryable = False

    def __init__(self, message: str, *, details: dict[str, Any] | None = None, error_code: str | None = None):
        super().__init__(message)
        self.message = message
        self.details = details or {}
        if error_code:
            self.error_code = error_code


class NotFoundError(AppError):
    status_code = 404
    error_code = "NOT_FOUND"


class ForbiddenError(AppError):
    status_code = 403
    error_code = "FORBIDDEN"


class UnauthorizedError(AppError):
    status_code = 401
    error_code = "UNAUTHORIZED"


class ConflictError(AppError):
    status_code = 409
    error_code = "CONFLICT"


class ApprovalRequiredError(AppError):
    status_code = 409
    error_code = "APPROVAL_REQUIRED"


class ProviderUnavailableError(AppError):
    status_code = 503
    error_code = "PROVIDER_UNAVAILABLE"
    retryable = True


class InvalidSignatureError(AppError):
    status_code = 400
    error_code = "INVALID_SIGNATURE"
