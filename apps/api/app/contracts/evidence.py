from __future__ import annotations

from typing import Any
from uuid import UUID

from pydantic import Field, model_validator

from .common import (
    Contract,
    DataOrigin,
    EvidenceClassification,
    UtcDatetime,
    VerificationStatus,
    new_id,
    utcnow,
)


class SourceEvidence(Contract):
    """One sourced observation. Conflicting evidence coexists; nothing is overwritten."""

    evidence_id: UUID = Field(default_factory=new_id)
    project_id: UUID | None = None
    property_id: UUID | None = None
    provider: str = Field(description="e.g. 'la_open_data', 'costar', 'matterport', 'owner', 'ifc_bench'")
    dataset: str | None = Field(default=None, description="e.g. 'ysqd-apz7'")
    source_record_id: str | None = Field(default=None, description="Provider-native ID, e.g. permit number.")
    source_url: str | None = None
    source_asset_reference: str | None = None
    data_origin: DataOrigin
    retrieved_at: UtcDatetime = Field(default_factory=utcnow)
    source_effective_at: UtcDatetime | None = Field(
        default=None, description="When the fact was true at the source (e.g. permit final date)."
    )
    observation_type: str = Field(description="e.g. 'electrical_service', 'water_service', 'sink_present'")
    extracted_value: Any = None
    unit: str | None = None
    evidence_classification: EvidenceClassification
    match_confidence: float | None = Field(default=None, ge=0, le=1)
    verification_status: VerificationStatus = VerificationStatus.UNVERIFIED
    limitations: list[str] = Field(default_factory=list)

    @model_validator(mode="after")
    def _historical_needs_limitations(self) -> SourceEvidence:
        if (
            self.evidence_classification == EvidenceClassification.DOCUMENTED_HISTORICAL
            and not self.limitations
        ):
            raise ValueError(
                "DOCUMENTED_HISTORICAL evidence must state its limitations "
                "(historical records do not establish current conditions)."
            )
        return self
