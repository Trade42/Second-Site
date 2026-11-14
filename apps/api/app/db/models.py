"""Business-state tables. Agent execution state (LangGraph checkpoints) lives in separate tables
managed by langgraph-checkpoint-postgres; conversation transcripts are referenced, not authoritative."""

from __future__ import annotations

from uuid import UUID

from sqlalchemy import Boolean, Float, ForeignKey, Identity, Index, Integer, String, Text, UniqueConstraint
from sqlalchemy import Uuid as SAUuid
from sqlalchemy.orm import Mapped, mapped_column

from .base import Base, DocumentMixin, JsonDoc, TimestampMixin

CASCADE = "CASCADE"


def pk() -> Mapped[UUID]:
    return mapped_column(SAUuid, primary_key=True)


def project_fk(nullable: bool = False) -> Mapped[UUID]:
    return mapped_column(SAUuid, ForeignKey("projects.id", ondelete=CASCADE), nullable=nullable, index=True)


def property_fk(nullable: bool = False) -> Mapped[UUID]:
    return mapped_column(SAUuid, ForeignKey("properties.id", ondelete=CASCADE), nullable=nullable, index=True)


class User(Base, TimestampMixin):
    __tablename__ = "users"
    id: Mapped[UUID] = pk()
    display_name: Mapped[str] = mapped_column(String(200), nullable=False)
    email: Mapped[str | None] = mapped_column(String(320), unique=True)
    phone: Mapped[str | None] = mapped_column(String(32))


class Project(Base, DocumentMixin):
    __tablename__ = "projects"
    id: Mapped[UUID] = pk()
    owner_id: Mapped[UUID] = mapped_column(SAUuid, ForeignKey("users.id", ondelete="RESTRICT"), index=True)
    status: Mapped[str] = mapped_column(String(32), nullable=False)


class BusinessRequirement(Base, DocumentMixin):
    __tablename__ = "business_requirements"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    confirmation_status: Mapped[str] = mapped_column(String(32), nullable=False)


class Property(Base, DocumentMixin):
    """PropertyIdentity + latest PropertyDigitalProfile (data)."""

    __tablename__ = "properties"
    __table_args__ = (
        UniqueConstraint("project_id", "normalized_address", name="uq_property_project_address"),
    )
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    normalized_address: Mapped[str] = mapped_column(String(400), nullable=False)
    parcel_id: Mapped[str | None] = mapped_column(String(64), index=True)
    data_origin: Mapped[str] = mapped_column(String(32), nullable=False)


class PropertySource(Base, TimestampMixin):
    """Raw provider payloads, kept separate from normalized consumer-facing records."""

    __tablename__ = "property_sources"
    __table_args__ = (UniqueConstraint("provider", "dataset", "source_record_id", "property_id"),)
    id: Mapped[UUID] = pk()
    property_id: Mapped[UUID] = property_fk()
    provider: Mapped[str] = mapped_column(String(64), nullable=False)
    dataset: Mapped[str | None] = mapped_column(String(64))
    source_record_id: Mapped[str] = mapped_column(String(128), nullable=False)
    data_origin: Mapped[str] = mapped_column(String(32), nullable=False)
    raw_payload: Mapped[dict] = mapped_column(JsonDoc, nullable=False)


class PropertyIdentityMatch(Base, TimestampMixin):
    __tablename__ = "property_identity_matches"
    id: Mapped[UUID] = pk()
    property_id: Mapped[UUID] = property_fk()
    provider: Mapped[str] = mapped_column(String(64), nullable=False)
    provider_record_id: Mapped[str] = mapped_column(String(128), nullable=False)
    match_status: Mapped[str] = mapped_column(String(32), nullable=False)
    match_confidence: Mapped[float | None] = mapped_column(Float)
    explanation: Mapped[str] = mapped_column(Text, nullable=False)


class PermitRecordRow(Base, DocumentMixin):
    __tablename__ = "permit_records"
    __table_args__ = (UniqueConstraint("property_id", "permit_number"),)
    id: Mapped[UUID] = pk()
    property_id: Mapped[UUID] = property_fk()
    permit_number: Mapped[str] = mapped_column(String(64), nullable=False, index=True)
    permit_category: Mapped[str] = mapped_column(String(32), nullable=False)
    match_status: Mapped[str] = mapped_column(String(32), nullable=False)


class SourceEvidenceRow(Base, DocumentMixin):
    """`property_observations` in the spec: every sourced observation, incl. conflicting ones."""

    __tablename__ = "property_observations"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID | None] = project_fk(nullable=True)
    property_id: Mapped[UUID | None] = property_fk(nullable=True)
    provider: Mapped[str] = mapped_column(String(64), nullable=False)
    observation_type: Mapped[str] = mapped_column(String(64), nullable=False, index=True)
    evidence_classification: Mapped[str] = mapped_column(String(32), nullable=False)
    data_origin: Mapped[str] = mapped_column(String(32), nullable=False)


class InfrastructureAssessmentRow(Base, DocumentMixin):
    __tablename__ = "infrastructure_assessments"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    property_id: Mapped[UUID] = property_fk()
    requirement_id: Mapped[str] = mapped_column(String(128), nullable=False)
    status: Mapped[str] = mapped_column(String(32), nullable=False)
    superseded: Mapped[bool] = mapped_column(Boolean, nullable=False, default=False)


class FeasibilityReport(Base, DocumentMixin):
    __tablename__ = "feasibility_reports"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    property_id: Mapped[UUID] = property_fk()


class RenovationWorkItemRow(Base, DocumentMixin):
    __tablename__ = "renovation_work_items"
    id: Mapped[UUID] = pk()
    property_id: Mapped[UUID] = property_fk()
    category: Mapped[str] = mapped_column(String(32), nullable=False)
    superseded: Mapped[bool] = mapped_column(Boolean, nullable=False, default=False)


class CostScenarioRow(Base, DocumentMixin):
    __tablename__ = "cost_scenarios"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    property_id: Mapped[UUID] = property_fk()
    long_term_cost: Mapped[int] = mapped_column(Integer, nullable=False)
    superseded: Mapped[bool] = mapped_column(Boolean, nullable=False, default=False)


class PropertyRanking(Base, DocumentMixin):
    __tablename__ = "property_rankings"
    __table_args__ = (UniqueConstraint("project_id", "ranking_version"),)
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    ranking_version: Mapped[int] = mapped_column(Integer, nullable=False)


class SiteGeometryRow(Base, DocumentMixin):
    __tablename__ = "site_geometries"
    id: Mapped[UUID] = pk()
    property_id: Mapped[UUID] = property_fk()
    scale_verified: Mapped[bool] = mapped_column(Boolean, nullable=False)


class DesignVersionRow(Base, DocumentMixin):
    __tablename__ = "design_versions"
    __table_args__ = (UniqueConstraint("property_id", "version_number"),)
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    property_id: Mapped[UUID] = property_fk()
    parent_version_id: Mapped[UUID | None] = mapped_column(
        SAUuid, ForeignKey("design_versions.id", ondelete="SET NULL")
    )
    version_number: Mapped[int] = mapped_column(Integer, nullable=False)
    approval_status: Mapped[str] = mapped_column(String(32), nullable=False)
    valid: Mapped[bool] = mapped_column(Boolean, nullable=False)


class Room(Base, TimestampMixin):
    __tablename__ = "rooms"
    id: Mapped[UUID] = pk()
    design_version_id: Mapped[UUID] = mapped_column(
        SAUuid, ForeignKey("design_versions.id", ondelete=CASCADE), index=True
    )
    room_key: Mapped[str] = mapped_column(String(64), nullable=False)
    room_type: Mapped[str] = mapped_column(String(64), nullable=False)
    area_sqm: Mapped[float] = mapped_column(Float, nullable=False)
    polygon: Mapped[list] = mapped_column(JsonDoc, nullable=False)


class DesignObject(Base, TimestampMixin):
    __tablename__ = "design_objects"
    id: Mapped[UUID] = pk()
    design_version_id: Mapped[UUID] = mapped_column(
        SAUuid, ForeignKey("design_versions.id", ondelete=CASCADE), index=True
    )
    room_key: Mapped[str | None] = mapped_column(String(64))
    item_type: Mapped[str] = mapped_column(String(64), nullable=False)
    footprint: Mapped[list] = mapped_column(JsonDoc, nullable=False)


class BillOfMaterialsRow(Base, DocumentMixin):
    __tablename__ = "bills_of_materials"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    design_version_id: Mapped[UUID] = mapped_column(
        SAUuid, ForeignKey("design_versions.id", ondelete=CASCADE), index=True
    )
    status: Mapped[str] = mapped_column(String(32), nullable=False)


class BomItemRow(Base, DocumentMixin):
    __tablename__ = "bom_items"
    id: Mapped[UUID] = pk()
    bom_id: Mapped[UUID] = mapped_column(
        SAUuid, ForeignKey("bills_of_materials.id", ondelete=CASCADE), index=True
    )
    room_key: Mapped[str | None] = mapped_column(String(64))


class SupplierOfferRow(Base, DocumentMixin):
    __tablename__ = "supplier_offers"
    id: Mapped[UUID] = pk()
    bom_item_id: Mapped[UUID] = mapped_column(
        SAUuid, ForeignKey("bom_items.id", ondelete=CASCADE), index=True
    )
    supplier: Mapped[str] = mapped_column(String(128), nullable=False)


class Communication(Base, DocumentMixin):
    __tablename__ = "communications"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    channel: Mapped[str] = mapped_column(String(32), nullable=False)
    state: Mapped[str] = mapped_column(String(32), nullable=False)


class CallSessionRow(Base, DocumentMixin):
    __tablename__ = "call_sessions"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    provider_call_id: Mapped[str | None] = mapped_column(String(128), unique=True)
    state: Mapped[str] = mapped_column(String(32), nullable=False)


class OwnerHandoffRequestRow(Base, DocumentMixin):
    __tablename__ = "owner_handoff_requests"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    call_id: Mapped[UUID] = mapped_column(
        SAUuid, ForeignKey("call_sessions.id", ondelete=CASCADE), index=True
    )
    status: Mapped[str] = mapped_column(String(32), nullable=False)


class Approval(Base, DocumentMixin):
    __tablename__ = "approvals"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    status: Mapped[str] = mapped_column(String(32), nullable=False, index=True)
    action_type: Mapped[str] = mapped_column(String(32), nullable=False)
    idempotency_key: Mapped[str] = mapped_column(String(255), nullable=False, unique=True)


class Task(Base, DocumentMixin):
    __tablename__ = "tasks"
    id: Mapped[UUID] = pk()
    project_id: Mapped[UUID] = project_fk()
    task_type: Mapped[str] = mapped_column(String(64), nullable=False)
    status: Mapped[str] = mapped_column(String(32), nullable=False, index=True)


class ActivityEventRow(Base, TimestampMixin):
    __tablename__ = "activity_events"
    __table_args__ = (Index("ix_activity_events_project_seq", "project_id", "seq"),)
    id: Mapped[UUID] = pk()
    seq: Mapped[int] = mapped_column(Integer, Identity(always=False), unique=True, nullable=False)
    project_id: Mapped[UUID] = project_fk()
    event_type: Mapped[str] = mapped_column(String(64), nullable=False, index=True)
    correlation_id: Mapped[UUID] = mapped_column(SAUuid, nullable=False, index=True)
    causation_id: Mapped[UUID | None] = mapped_column(SAUuid)
    idempotency_key: Mapped[str] = mapped_column(String(255), nullable=False, unique=True)
    envelope: Mapped[dict] = mapped_column(JsonDoc, nullable=False)


class ProcessedWebhookEvent(Base, TimestampMixin):
    __tablename__ = "processed_webhook_events"
    __table_args__ = (UniqueConstraint("provider", "provider_event_id"),)
    id: Mapped[UUID] = pk()
    provider: Mapped[str] = mapped_column(String(32), nullable=False)
    provider_event_id: Mapped[str] = mapped_column(String(255), nullable=False)
    verified: Mapped[bool] = mapped_column(Boolean, nullable=False)
    payload: Mapped[dict] = mapped_column(JsonDoc, nullable=False)
