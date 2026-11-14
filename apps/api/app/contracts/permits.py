from __future__ import annotations

from datetime import date
from enum import StrEnum
from uuid import UUID

from pydantic import Field

from .common import Contract, DataOrigin, EvidenceClassification, MatchStatus, new_id


class PermitCategory(StrEnum):
    ELECTRICAL = "ELECTRICAL"
    PLUMBING = "PLUMBING"
    HVAC = "HVAC"
    FIRE_SPRINKLER = "FIRE_SPRINKLER"
    BUILDING = "BUILDING"
    OTHER_MECHANICAL = "OTHER_MECHANICAL"
    OTHER = "OTHER"


class PermitScope(StrEnum):
    BUILDING = "BUILDING"  # no unit on the record
    UNIT = "UNIT"  # explicit suite/unit
    FLOOR = "FLOOR"  # floor/common area designation
    UNKNOWN = "UNKNOWN"


class ExtractedInfrastructureFact(Contract):
    """A technical value parsed from a permit description. Permit status != verification."""

    fact_type: str = Field(description="e.g. 'electrical_service_amps', 'water_service_diameter'")
    value: float | str | bool
    unit: str | None = None
    source_text: str = Field(description="Exact substring the value was extracted from.")
    classification: EvidenceClassification = EvidenceClassification.DOCUMENTED_HISTORICAL
    limitations: list[str] = Field(default_factory=list)


class PermitRecord(Contract):
    permit_record_id: UUID = Field(default_factory=new_id)
    property_id: UUID | None = None
    permit_number: str
    permit_category: PermitCategory
    permit_status: str = Field(description="Source status verbatim, e.g. 'Permit Finaled'.")
    issued_date: date | None = None
    finaled_date: date | None = Field(
        default=None, description="status_date when status is finaled; otherwise null."
    )
    work_description: str
    original_address: str
    normalized_address: str
    unit_or_suite: str | None = None
    scope: PermitScope = PermitScope.UNKNOWN
    parcel_id: str | None = None
    source_dataset: str
    source_url: str | None = None
    data_origin: DataOrigin
    matched_property_identity: UUID | None = None
    match_status: MatchStatus = MatchStatus.UNMATCHED
    match_confidence: float | None = Field(default=None, ge=0, le=1)
    match_explanation: str | None = None
    extracted_infrastructure_facts: list[ExtractedInfrastructureFact] = Field(default_factory=list)
    source_evidence_ids: list[UUID] = Field(default_factory=list)
