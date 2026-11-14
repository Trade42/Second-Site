from __future__ import annotations

from enum import StrEnum
from uuid import UUID

from pydantic import Field, model_validator

from .common import Contract, EvidenceClassification, new_id


class RequirementCategory(StrEnum):
    ELECTRICAL = "ELECTRICAL"
    PLUMBING = "PLUMBING"
    HVAC = "HVAC"
    SPACE = "SPACE"
    ACCESSIBILITY = "ACCESSIBILITY"
    FIRE_LIFE_SAFETY = "FIRE_LIFE_SAFETY"
    ZONING = "ZONING"


class Severity(StrEnum):
    HARD = "HARD"  # failing it disqualifies the property
    SOFT = "SOFT"  # failing it adds cost / risk


class InfrastructureRequirement(Contract):
    requirement_id: str = Field(
        description="Stable template key, e.g. 'facial_studio.plumbing.treatment_sinks'"
    )
    business_type: str
    requirement_category: RequirementCategory
    description: str
    required_value: float | str | bool | None = None
    required_unit: str | None = None
    severity: Severity
    verification_method: str = Field(description="How a professional would verify it.")


class AssessmentStatus(StrEnum):
    MEETS = "MEETS"
    MAY_REQUIRE_WORK = "MAY_REQUIRE_WORK"
    FAILS = "FAILS"
    UNKNOWN = "UNKNOWN"  # never treated as MEETS


class ReviewStatus(StrEnum):
    AUTOMATED = "AUTOMATED"
    NEEDS_REVIEW = "NEEDS_REVIEW"
    REVIEWED = "REVIEWED"


class InfrastructureObservation(Contract):
    observation_id: UUID = Field(default_factory=new_id)
    property_id: UUID | None = None
    observation_type: str
    value: float | str | bool | None = None
    unit: str | None = None
    classification: EvidenceClassification
    evidence_ids: list[UUID] = Field(default_factory=list)
    limitations: list[str] = Field(default_factory=list)
    description: str


class InfrastructureAssessment(Contract):
    assessment_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID
    requirement_id: str
    status: AssessmentStatus
    observed_value: float | str | bool | None = None
    required_value: float | str | bool | None = None
    evidence_ids: list[UUID] = Field(default_factory=list)
    limitations: list[str] = Field(default_factory=list)
    proposed_work: list[str] = Field(default_factory=list)
    verification_questions: list[str] = Field(default_factory=list)
    uncertainty: str = Field(description="Plain-language statement of what is not known.")
    review_status: ReviewStatus = ReviewStatus.AUTOMATED

    @model_validator(mode="after")
    def _grounding(self) -> InfrastructureAssessment:
        if self.status == AssessmentStatus.MEETS and not self.evidence_ids:
            raise ValueError("MEETS requires supporting evidence_ids")
        if self.status == AssessmentStatus.UNKNOWN and not self.verification_questions:
            raise ValueError("UNKNOWN assessments must list verification_questions")
        return self
