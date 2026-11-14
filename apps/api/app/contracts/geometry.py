from __future__ import annotations

from enum import StrEnum
from uuid import UUID

from pydantic import Field, field_validator, model_validator
from shapely.geometry import Polygon
from shapely.validation import explain_validity

from .common import Contract, DataOrigin, NonNegativeMinor, UtcDatetime, new_id, utcnow

Coord = tuple[float, float]


def validate_ring(points: list[Coord], label: str = "polygon") -> list[Coord]:
    if len(points) < 3:
        raise ValueError(f"{label} needs at least 3 points")
    poly = Polygon(points)
    if not poly.is_valid:
        raise ValueError(f"{label} is not a valid polygon: {explain_validity(poly)}")
    if poly.area <= 0:
        raise ValueError(f"{label} has zero area")
    return points


class GeometrySource(StrEnum):
    MATTERPORT_MEASURED = "MATTERPORT_MEASURED"
    CAD_OR_IFC = "CAD_OR_IFC"
    OWNER_MEASURED = "OWNER_MEASURED"
    PHOTO_ESTIMATE = "PHOTO_ESTIMATE"  # can never be scale_verified
    FIXTURE = "FIXTURE"


class Opening(Contract):
    kind: str = Field(description="'entrance' | 'exit' | 'window'")
    start: Coord
    end: Coord


class Fixture(Contract):
    kind: str = Field(description="e.g. 'sink', 'water_heater', 'electrical_panel', 'drain'")
    location: Coord
    evidence_ids: list[UUID] = Field(default_factory=list)


class SiteGeometry(Contract):
    site_geometry_id: UUID = Field(default_factory=new_id)
    property_id: UUID
    source_id: str = Field(description="Provider asset id the geometry was derived from.")
    geometry_source: GeometrySource
    data_origin: DataOrigin
    units: str = Field(default="m", pattern="^m$", description="Internal geometry is always meters.")
    coordinate_reference: str = "local_floor_origin"
    floor_boundary: list[Coord]
    fixed_obstacles: list[list[Coord]] = Field(default_factory=list)
    fixed_walls: list[tuple[Coord, Coord]] = Field(default_factory=list)
    entrances: list[Opening] = Field(default_factory=list)
    exits: list[Opening] = Field(default_factory=list)
    windows: list[Opening] = Field(default_factory=list)
    fixture_locations: list[Fixture] = Field(default_factory=list)
    restricted_areas: list[list[Coord]] = Field(default_factory=list)
    scale_verified: bool = False
    measurement_uncertainty_m: float | None = Field(default=None, ge=0)

    @field_validator("floor_boundary")
    @classmethod
    def _boundary(cls, v: list[Coord]) -> list[Coord]:
        return validate_ring(v, "floor_boundary")

    @field_validator("fixed_obstacles", "restricted_areas")
    @classmethod
    def _rings(cls, v: list[list[Coord]]) -> list[list[Coord]]:
        return [validate_ring(r, "obstacle") for r in v]

    @model_validator(mode="after")
    def _photo_not_verified(self) -> SiteGeometry:
        if self.geometry_source == GeometrySource.PHOTO_ESTIMATE and self.scale_verified:
            raise ValueError("photo-derived geometry cannot be scale_verified")
        return self


class RoomRequirement(Contract):
    room_type: str
    count: int = Field(ge=1)
    min_area_sqm: float = Field(gt=0)
    min_width_m: float = Field(gt=0)
    needs_plumbing: bool = False


class LayoutPreferences(Contract):
    style: str | None = None
    prefer_rooms_near_plumbing: bool = True
    circulation_width_m: float = Field(default=1.2, gt=0)


class RoomPolygon(Contract):
    room_id: str
    room_type: str
    polygon: list[Coord]
    area_sqm: float = Field(gt=0)

    @field_validator("polygon")
    @classmethod
    def _poly(cls, v: list[Coord]) -> list[Coord]:
        return validate_ring(v, "room polygon")


class FurniturePlacement(Contract):
    placement_id: str
    room_id: str
    item_type: str
    footprint: list[Coord]

    @field_validator("footprint")
    @classmethod
    def _poly(cls, v: list[Coord]) -> list[Coord]:
        return validate_ring(v, "furniture footprint")


class FinishSpecification(Contract):
    room_id: str | None = None
    surface: str = Field(description="'floor' | 'wall' | 'ceiling'")
    material: str
    area_sqm: float = Field(gt=0)


class ValidationIssue(Contract):
    code: str
    message: str
    room_ids: list[str] = Field(default_factory=list)


class GeometryValidationReport(Contract):
    valid: bool
    issues: list[ValidationIssue] = Field(default_factory=list)
    checked_at: UtcDatetime = Field(default_factory=utcnow)
    disclaimer: str = (
        "Automated geometric check only. Not a code-compliance review or professionally approved plan."
    )


class DesignApprovalStatus(StrEnum):
    DRAFT = "DRAFT"
    PROPOSED = "PROPOSED"
    APPROVED = "APPROVED"
    SUPERSEDED = "SUPERSEDED"
    REJECTED = "REJECTED"


class DesignVersion(Contract):
    design_version_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID
    parent_version_id: UUID | None = None
    version_number: int = Field(ge=1)
    geometry_id: UUID
    room_polygons: list[RoomPolygon]
    furniture_placements: list[FurniturePlacement] = Field(default_factory=list)
    proposed_walls: list[tuple[Coord, Coord]] = Field(default_factory=list)
    finish_specifications: list[FinishSpecification] = Field(default_factory=list)
    validation_report: GeometryValidationReport
    estimated_construction_cost: NonNegativeMinor | None = None
    created_from_revision_request: UUID | None = None
    approval_status: DesignApprovalStatus = DesignApprovalStatus.DRAFT

    @model_validator(mode="after")
    def _approved_must_be_valid(self) -> DesignVersion:
        if self.approval_status == DesignApprovalStatus.APPROVED and not self.validation_report.valid:
            raise ValueError("an invalid layout cannot be approved")
        return self


class RevisionStatus(StrEnum):
    RECEIVED = "RECEIVED"
    PROCESSING = "PROCESSING"
    APPLIED = "APPLIED"
    INFEASIBLE = "INFEASIBLE"
    REJECTED = "REJECTED"


class DesignRevisionRequest(Contract):
    revision_request_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID
    base_design_version_id: UUID
    requested_changes: list[str] = Field(min_length=1)
    room_requirement_overrides: list[RoomRequirement] = Field(default_factory=list)
    preserved_constraints: list[str] = Field(default_factory=list)
    requested_by: UUID
    source_channel: str = Field(description="'web' | 'photon' | 'agent'")
    status: RevisionStatus = RevisionStatus.RECEIVED


class LayoutGenerationRequest(Contract):
    property_id: UUID
    site_geometry_id: UUID | None = None
    room_requirements: list[RoomRequirement]
    preferences: LayoutPreferences = Field(default_factory=LayoutPreferences)
    candidate_count: int = Field(default=2, ge=1, le=5)
