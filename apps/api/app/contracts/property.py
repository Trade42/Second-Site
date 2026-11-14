from __future__ import annotations

from datetime import date
from enum import StrEnum
from uuid import UUID

from pydantic import Field

from .common import (
    Contract,
    CurrencyCode,
    DataOrigin,
    MatchStatus,
    NonNegativeMinor,
    UtcDatetime,
    new_id,
)
from .infrastructure import InfrastructureAssessment, InfrastructureObservation
from .permits import PermitRecord


class PropertyIdentity(Contract):
    """Canonical identity. Providers (CoStar, Matterport, Vexcel, LADBS) do NOT share IDs."""

    property_id: UUID = Field(default_factory=new_id)
    provider_property_ids: dict[str, str] = Field(
        default_factory=dict, description="provider -> provider-native id, e.g. {'costar': '...'}"
    )
    normalized_address: str = Field(description="Canonical uppercase form incl. direction and unit.")
    street_address: str
    unit_or_suite: str | None = None
    city: str
    state: str = Field(min_length=2, max_length=2)
    postal_code: str | None = None
    parcel_id: str | None = Field(default=None, description="APN when known.")
    latitude: float | None = Field(default=None, ge=-90, le=90)
    longitude: float | None = Field(default=None, ge=-180, le=180)
    identity_match_status: MatchStatus = MatchStatus.UNMATCHED
    identity_match_confidence: float | None = Field(default=None, ge=0, le=1)


class ListingType(StrEnum):
    LEASE = "LEASE"
    SALE = "SALE"


class CommercialListing(Contract):
    provider: str = Field(description="'costar' only when sourced from CoStar; fixtures say 'fixture'.")
    provider_listing_id: str | None = None
    data_origin: DataOrigin
    listing_type: ListingType = ListingType.LEASE
    property_type: str | None = None
    available_area_sqm: float | None = Field(default=None, gt=0)
    currency: CurrencyCode = "USD"
    asking_rent_monthly: NonNegativeMinor | None = None
    nnn_charges_monthly: NonNegativeMinor | None = None
    nnn_is_estimate: bool = Field(
        default=False,
        description="True when nnn_charges_monthly is a DERIVED estimate because the listing quoted none.",
    )
    security_deposit: NonNegativeMinor | None = None
    lease_term_months: int | None = Field(default=None, gt=0)
    available_from: date | None = None
    existing_buildout: str | None = Field(
        default=None,
        description="'open_shell' | 'office_buildout' | 'second_generation_salon' | null (unknown)",
    )
    lease_service_type: str | None = Field(
        default=None,
        description="Which operating expenses the rent includes, e.g. 'TripleNet', 'FullService', 'ModifiedGross'.",
    )
    building_name: str | None = None
    brokerage_company: str | None = None
    photo_urls: list[str] = Field(default_factory=list, description="Provider CDN photo links (not stored).")
    source_record_date: date | None = Field(
        default=None, description="When the provider last certified the record (snapshot freshness)."
    )
    listing_url: str | None = None
    disclaimer: str | None = None


class InteriorAsset(Contract):
    provider: str = Field(description="'matterport' or fixture")
    asset_id: str
    asset_type: str = Field(description="e.g. 'matterport_model', 'floor_plan_image', 'photo'")
    data_origin: DataOrigin
    url: str | None = None
    label: str | None = Field(default=None, description="Provider title, e.g. the Matterport model name.")
    area_sqm: float | None = Field(default=None, gt=0, description="Provider-measured floor area, if any.")
    matches_listing_unit: bool | None = Field(
        default=None,
        description="True when the asset is matched to the listed suite (not just the building).",
    )
    has_measured_geometry: bool = False


class ExteriorAsset(Contract):
    provider: str = Field(description="'vexcel' or fixture")
    asset_id: str
    asset_type: str = Field(description="e.g. 'oblique', 'orthomosaic', 'roof_analysis'")
    data_origin: DataOrigin
    captured_at: date | None = None
    url: str | None = None


class SourceManifestEntry(Contract):
    provider: str
    dataset: str | None = None
    status: str = Field(description="LIVE | SNAPSHOT | MOCK | PARTIAL | BLOCKED | NOT_STARTED | NOT_FOUND")
    record_count: int = 0
    retrieved_at: UtcDatetime | None = None
    note: str | None = None


class PropertyDigitalProfile(Contract):
    """Primary unified property intelligence object. Raw provider payloads are NOT embedded."""

    property_id: UUID
    project_id: UUID
    property_identity: PropertyIdentity
    commercial_listing: CommercialListing | None = None
    interior_assets: list[InteriorAsset] = Field(default_factory=list)
    exterior_assets: list[ExteriorAsset] = Field(default_factory=list)
    permit_records: list[PermitRecord] = Field(default_factory=list)
    building_observations: list[InfrastructureObservation] = Field(default_factory=list)
    geometry_reference: UUID | None = Field(default=None, description="SiteGeometry id if available.")
    infrastructure_assessments: list[InfrastructureAssessment] = Field(default_factory=list)
    missing_information: list[str] = Field(default_factory=list)
    source_manifest: list[SourceManifestEntry] = Field(default_factory=list)
    last_analyzed_at: UtcDatetime | None = None


class PropertySearchRequest(Contract):
    city: str = "Los Angeles"
    state: str = "CA"
    max_monthly_rent: NonNegativeMinor | None = None
    min_area_sqm: float | None = Field(default=None, gt=0)
    neighborhoods: list[str] = Field(default_factory=list)
    limit: int = Field(default=10, ge=1, le=50)
