from __future__ import annotations

from datetime import date
from enum import StrEnum
from uuid import UUID

from pydantic import Field

from .common import (
    SCHEMA_VERSION,
    Contract,
    CurrencyCode,
    NonNegativeMinor,
    UtcDatetime,
    new_id,
    utcnow,
)


class ProjectStatus(StrEnum):
    ONBOARDING = "ONBOARDING"
    SEARCHING = "SEARCHING"
    ANALYZING = "ANALYZING"
    COMPARING = "COMPARING"
    DESIGNING = "DESIGNING"
    PROCURING = "PROCURING"
    OPENING = "OPENING"
    ARCHIVED = "ARCHIVED"


class Market(Contract):
    city: str
    state: str = Field(min_length=2, max_length=2)
    country: str = "US"


class ProjectContext(Contract):
    project_id: UUID = Field(default_factory=new_id)
    owner_id: UUID
    project_name: str = Field(min_length=1, max_length=200)
    business_profile_id: UUID | None = None
    project_status: ProjectStatus = ProjectStatus.ONBOARDING
    market: Market
    selected_property_id: UUID | None = None
    selected_design_version_id: UUID | None = None
    current_budget_revision: int = Field(default=0, ge=0)
    schema_version: str = SCHEMA_VERSION
    record_version: int = Field(default=1, ge=1, description="Optimistic concurrency counter.")
    created_at: UtcDatetime = Field(default_factory=utcnow)
    updated_at: UtcDatetime = Field(default_factory=utcnow)


class ConfirmationStatus(StrEnum):
    DRAFT = "DRAFT"
    INFERRED = "INFERRED"  # filled by the onboarding agent, not yet confirmed
    CONFIRMED = "CONFIRMED"  # owner confirmed


class FieldProvenance(Contract):
    field: str
    source: str = Field(description="e.g. 'owner_form', 'onboarding_agent', 'chat:<message_id>'")
    confirmation_status: ConfirmationStatus
    note: str | None = None


class SpaceRequirements(Contract):
    min_area_sqm: float | None = Field(default=None, gt=0)
    max_area_sqm: float | None = Field(default=None, gt=0)
    required_rooms: dict[str, int] = Field(
        default_factory=dict, description="Room type -> count, e.g. {'treatment': 3}."
    )
    ground_floor_required: bool | None = None


class InfrastructureNeeds(Contract):
    minimize_plumbing_changes: bool = False
    sinks_per_treatment_room: int = Field(default=1, ge=0)
    min_electrical_service_amps: int | None = Field(default=None, gt=0)
    needs_dedicated_hvac_zoning: bool | None = None
    notes: list[str] = Field(default_factory=list)


class OperationalRequirements(Contract):
    operating_hours: str | None = None
    staff_count: int | None = Field(default=None, ge=0)
    parking_required: bool | None = None
    ada_accessible_required: bool = True


class DesignPreferences(Contract):
    interior_style: str | None = None
    notes: list[str] = Field(default_factory=list)


class RankingPriorities(Contract):
    """Relative weights. Normalized by the ranking service; must not all be zero."""

    total_cost: float = Field(default=0.5, ge=0)
    infrastructure_fit: float = Field(default=0.3, ge=0)
    evidence_certainty: float = Field(default=0.2, ge=0)
    comparison_horizon_months: int = Field(default=36, gt=0, le=240)


class BusinessExpansionProfile(Contract):
    profile_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    business_type: str = Field(min_length=1, description="e.g. 'facial_studio'")
    business_description: str | None = None
    current_location_count: int = Field(ge=0)
    target_market: dict[str, str] = Field(description="{'city': 'Los Angeles', 'state': 'CA'}")
    preferred_neighborhoods: list[str] = Field(default_factory=list)
    currency: CurrencyCode = "USD"
    opening_capital: NonNegativeMinor | None = None
    monthly_rent_limit: NonNegativeMinor | None = None
    working_capital_reserve: NonNegativeMinor | None = None
    space_requirements: SpaceRequirements
    infrastructure_requirements: InfrastructureNeeds = Field(default_factory=InfrastructureNeeds)
    operational_requirements: OperationalRequirements = Field(default_factory=OperationalRequirements)
    target_opening_date: date | None = None
    design_preferences: DesignPreferences = Field(default_factory=DesignPreferences)
    ranking_priorities: RankingPriorities = Field(default_factory=RankingPriorities)
    confirmation_status: ConfirmationStatus = ConfirmationStatus.DRAFT
    source_references: list[FieldProvenance] = Field(default_factory=list)
