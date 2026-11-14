"""Shared primitives for every Second Site contract.

Conventions (see docs/CONTRACTS.md):
- Internal IDs are UUIDs; timestamps are timezone-aware UTC.
- Money is integer minor units (cents) + ISO 4217 currency. Never floats.
- Geometry is meters internally.
- Unknown values are ``None`` or an explicit UNKNOWN enum, never a guessed default.
"""

from __future__ import annotations

from datetime import UTC, datetime
from enum import StrEnum
from typing import Annotated
from uuid import UUID, uuid4

from pydantic import AfterValidator, BaseModel, ConfigDict, Field

SCHEMA_VERSION = "1.0"


def utcnow() -> datetime:
    return datetime.now(UTC)


def new_id() -> UUID:
    return uuid4()


def _require_utc(value: datetime) -> datetime:
    if value.tzinfo is None:
        raise ValueError("timestamps must be timezone-aware (UTC)")
    return value.astimezone(UTC)


UtcDatetime = Annotated[datetime, AfterValidator(_require_utc)]


def _currency(value: str) -> str:
    if len(value) != 3 or not value.isalpha() or not value.isupper():
        raise ValueError("currency must be an ISO 4217 code such as 'USD'")
    return value


CurrencyCode = Annotated[str, AfterValidator(_currency)]
# Integer minor units (e.g. cents). Negative values are allowed only where a field says so.
MinorUnits = Annotated[int, Field(description="Integer minor currency units (e.g. cents).")]
NonNegativeMinor = Annotated[int, Field(ge=0, description="Non-negative integer minor currency units.")]


class Contract(BaseModel):
    """Base for all contracts: strict field names, no silent extras."""

    model_config = ConfigDict(
        extra="forbid",
        populate_by_name=True,
        use_enum_values=False,
        # Response schemas mark defaulted fields required (they are always present when serialized).
        json_schema_serialization_defaults_required=True,
    )


class Money(Contract):
    amount_minor: MinorUnits
    currency: CurrencyCode = "USD"


class Measurement(Contract):
    value: float
    unit: str = Field(description="Explicit unit, e.g. 'A', 'V', 'in', 'm', 'm2', 'sqft'.")


class EvidenceClassification(StrEnum):
    """How an assertion is known. Never collapsed into a single confidence score."""

    DOCUMENTED_HISTORICAL = "DOCUMENTED_HISTORICAL"
    VISUALLY_OBSERVED = "VISUALLY_OBSERVED"
    GEOMETRY_MEASURED = "GEOMETRY_MEASURED"
    OWNER_REPORTED = "OWNER_REPORTED"
    PROFESSIONALLY_VERIFIED = "PROFESSIONALLY_VERIFIED"
    DERIVED_ESTIMATE = "DERIVED_ESTIMATE"
    UNKNOWN = "UNKNOWN"
    CONFLICTING = "CONFLICTING"


class VerificationStatus(StrEnum):
    UNVERIFIED = "UNVERIFIED"
    SOURCE_CONFIRMED = "SOURCE_CONFIRMED"  # the record exists at the source as described
    CORROBORATED = "CORROBORATED"  # independent sources agree
    PROFESSIONALLY_VERIFIED = "PROFESSIONALLY_VERIFIED"
    DISPUTED = "DISPUTED"


class MatchStatus(StrEnum):
    EXACT = "EXACT"
    PROBABLE = "PROBABLE"
    AMBIGUOUS = "AMBIGUOUS"
    UNMATCHED = "UNMATCHED"
    CONFLICTING = "CONFLICTING"
    NEEDS_REVIEW = "NEEDS_REVIEW"


class DataOrigin(StrEnum):
    """Where a record's content came from. Fixtures must never masquerade as live data."""

    LIVE = "LIVE"
    FIXTURE_REAL = "FIXTURE_REAL"  # snapshot of a genuine public record
    FIXTURE_SYNTHETIC = "FIXTURE_SYNTHETIC"  # invented demo data
    USER_PROVIDED = "USER_PROVIDED"
    DERIVED = "DERIVED"


class IntegrationStatus(StrEnum):
    LIVE = "LIVE"
    MOCK = "MOCK"
    PARTIAL = "PARTIAL"
    BLOCKED = "BLOCKED"
    NOT_STARTED = "NOT_STARTED"


class Point2D(Contract):
    x: float
    y: float


class SourceReference(Contract):
    """Lightweight pointer used inside other contracts."""

    evidence_id: UUID | None = None
    description: str
    url: str | None = None
