"""Provider adapter conventions.

Every provider has: a Protocol (interface), typed I/O, error classification, a retry/timeout policy,
a `status()` that tells the truth (LIVE / MOCK / PARTIAL / BLOCKED / NOT_STARTED), and a fixture
implementation. Live implementations must not invent SDK methods — document the verified API.
"""

from __future__ import annotations

import time
from collections.abc import Callable
from dataclasses import dataclass
from enum import StrEnum
from pathlib import Path
from typing import TypeVar

import httpx

from app.contracts import IntegrationStatus, ProviderStatusEntry

T = TypeVar("T")

REPO_ROOT = Path(__file__).resolve().parents[4]
FIXTURES_DIR = REPO_ROOT / "fixtures"


class ProviderErrorKind(StrEnum):
    AUTH = "AUTH"
    NOT_FOUND = "NOT_FOUND"
    RATE_LIMITED = "RATE_LIMITED"
    TIMEOUT = "TIMEOUT"
    UNAVAILABLE = "UNAVAILABLE"
    BAD_RESPONSE = "BAD_RESPONSE"
    NOT_CONFIGURED = "NOT_CONFIGURED"


class ProviderError(Exception):
    def __init__(self, provider: str, kind: ProviderErrorKind, message: str):
        super().__init__(f"[{provider}] {kind}: {message}")
        self.provider, self.kind, self.message = provider, kind, message

    @property
    def retryable(self) -> bool:
        return self.kind in {
            ProviderErrorKind.RATE_LIMITED,
            ProviderErrorKind.TIMEOUT,
            ProviderErrorKind.UNAVAILABLE,
        }


@dataclass(frozen=True)
class RetryPolicy:
    attempts: int = 3
    backoff_s: float = 0.5
    timeout_s: float = 20.0


def classify_http_error(provider: str, exc: Exception) -> ProviderError:
    if isinstance(exc, httpx.TimeoutException):
        return ProviderError(provider, ProviderErrorKind.TIMEOUT, str(exc))
    if isinstance(exc, httpx.HTTPStatusError):
        code = exc.response.status_code
        kind = {
            401: ProviderErrorKind.AUTH,
            403: ProviderErrorKind.AUTH,
            404: ProviderErrorKind.NOT_FOUND,
            429: ProviderErrorKind.RATE_LIMITED,
        }.get(code)
        if kind is None:
            kind = ProviderErrorKind.UNAVAILABLE if code >= 500 else ProviderErrorKind.BAD_RESPONSE
        return ProviderError(provider, kind, f"HTTP {code}")
    if isinstance(exc, httpx.TransportError):
        return ProviderError(provider, ProviderErrorKind.UNAVAILABLE, str(exc))
    return ProviderError(provider, ProviderErrorKind.BAD_RESPONSE, str(exc))


def with_retries(provider: str, policy: RetryPolicy, fn: Callable[[], T]) -> T:
    last: ProviderError | None = None
    for attempt in range(policy.attempts):
        try:
            return fn()
        except ProviderError as err:
            last = err
        except Exception as exc:  # noqa: BLE001
            last = classify_http_error(provider, exc)
        if not last.retryable:
            raise last
        time.sleep(policy.backoff_s * (2**attempt))
    assert last is not None
    raise last


def status_entry(
    provider: str, mode: str, status: IntegrationStatus, creds: bool, note: str
) -> ProviderStatusEntry:
    return ProviderStatusEntry(
        provider=provider, mode=mode, status=status, credentials_present=creds, note=note
    )
