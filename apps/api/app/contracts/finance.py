from __future__ import annotations

from decimal import Decimal
from enum import StrEnum
from uuid import UUID

from pydantic import Field, model_validator

from .common import Contract, CurrencyCode, MinorUnits, NonNegativeMinor, UtcDatetime, new_id, utcnow


class WorkCategory(StrEnum):
    PLUMBING = "PLUMBING"
    ELECTRICAL = "ELECTRICAL"
    HVAC = "HVAC"
    PARTITIONS = "PARTITIONS"
    FINISHES = "FINISHES"
    ACCESSIBILITY = "ACCESSIBILITY"
    PERMITS_FEES = "PERMITS_FEES"
    DESIGN_PROFESSIONAL = "DESIGN_PROFESSIONAL"
    OTHER = "OTHER"


class RenovationWorkItem(Contract):
    work_item_id: UUID = Field(default_factory=new_id)
    property_id: UUID
    category: WorkCategory
    description: str
    quantity: Decimal = Field(gt=0)
    unit: str
    currency: CurrencyCode = "USD"
    unit_cost_low: NonNegativeMinor
    unit_cost_expected: NonNegativeMinor
    unit_cost_high: NonNegativeMinor
    requires_professional_quote: bool = True
    assumptions: list[str] = Field(default_factory=list)
    evidence_ids: list[UUID] = Field(default_factory=list)
    source_assessment_id: UUID | None = None

    @model_validator(mode="after")
    def _ordered(self) -> RenovationWorkItem:
        if not (self.unit_cost_low <= self.unit_cost_expected <= self.unit_cost_high):
            raise ValueError("unit costs must satisfy low <= expected <= high")
        return self


class CostKind(StrEnum):
    ONE_TIME = "ONE_TIME"
    MONTHLY = "MONTHLY"
    REFUNDABLE_DEPOSIT = "REFUNDABLE_DEPOSIT"  # cash requirement, excluded from long-term cost


class CostLineItem(Contract):
    line_item_id: str = Field(description="Deterministic key; duplicates are rejected.")
    category: str
    description: str
    kind: CostKind
    low: MinorUnits
    expected: MinorUnits
    high: MinorUnits
    source_work_item_id: UUID | None = None
    evidence_ids: list[UUID] = Field(default_factory=list)


class CostScenario(Contract):
    scenario_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID
    design_version_id: UUID | None = None
    currency: CurrencyCode = "USD"
    opening_cost_low: NonNegativeMinor
    opening_cost_expected: NonNegativeMinor
    opening_cost_high: NonNegativeMinor
    upfront_cash_expected: NonNegativeMinor = Field(
        description="Opening cost + refundable deposits: cash needed before opening."
    )
    monthly_operating_cost: NonNegativeMinor
    comparison_horizon_months: int = Field(gt=0)
    long_term_cost: NonNegativeMinor = Field(
        description="opening_cost_expected + monthly_operating_cost * horizon (deposits excluded)."
    )
    contingency: NonNegativeMinor
    line_items: list[CostLineItem]
    assumptions: list[str] = Field(default_factory=list)
    excluded_costs: list[str] = Field(default_factory=list)
    source_evidence_ids: list[UUID] = Field(default_factory=list)
    calculated_at: UtcDatetime = Field(default_factory=utcnow)

    @model_validator(mode="after")
    def _no_duplicate_lines(self) -> CostScenario:
        keys = [li.line_item_id for li in self.line_items]
        if len(keys) != len(set(keys)):
            raise ValueError("duplicate line_item_id in cost scenario (double counting)")
        if not (self.opening_cost_low <= self.opening_cost_expected <= self.opening_cost_high):
            raise ValueError("opening costs must satisfy low <= expected <= high")
        return self


class HardConstraintResult(Contract):
    property_id: UUID
    constraint: str
    passed: bool | None = Field(description="None when it cannot be determined (unknown).")
    detail: str


class RankedProperty(Contract):
    rank: int = Field(ge=1)
    property_id: UUID
    total_score: float
    disqualified: bool = False
    long_term_cost: NonNegativeMinor | None = None
    unknown_requirement_count: int = 0


class PropertyRankingResult(Contract):
    ranking_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    ranking_version: int = Field(ge=1)
    criteria_weights: dict[str, float]
    hard_constraint_results: list[HardConstraintResult]
    ranked_properties: list[RankedProperty]
    normalized_scores: dict[str, dict[str, float]] = Field(
        description="property_id -> criterion -> score in [0,1]"
    )
    uncertainty_penalties: dict[str, float] = Field(description="property_id -> penalty in [0,1]")
    cost_scenario_ids: list[UUID]
    explanations: dict[str, str] = Field(description="property_id -> plain-language explanation")
    calculated_at: UtcDatetime = Field(default_factory=utcnow)
