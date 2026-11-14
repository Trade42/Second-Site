"""ActivityEvent envelope + per-type versioned payloads.

Every event_type maps to exactly one payload model. Adding an event = add the enum member,
add a payload model, register it in EVENT_PAYLOADS, regenerate contracts.
"""

from __future__ import annotations

from enum import StrEnum
from typing import Any
from uuid import UUID

from pydantic import Field, model_validator

from .common import SCHEMA_VERSION, Contract, UtcDatetime, new_id, utcnow


class EventType(StrEnum):
    PROJECT_PROFILE_CONFIRMED = "project.profile.confirmed"
    PROPERTY_SEARCH_COMPLETED = "property.search.completed"
    PROPERTY_IDENTITY_RESOLVED = "property.identity.resolved"
    PROPERTY_PERMITS_INGESTED = "property.permits.ingested"
    PROPERTY_SPATIAL_ANALYSIS_COMPLETED = "property.spatial_analysis.completed"
    PROPERTY_ANALYSIS_COMPLETED = "property.analysis.completed"
    INFRASTRUCTURE_UNKNOWN_DETECTED = "infrastructure.unknown_detected"
    INFRASTRUCTURE_EVIDENCE_UPDATED = "infrastructure.evidence_updated"
    FINANCIAL_SCENARIO_UPDATED = "financial.scenario.updated"
    PROPERTY_RANKING_UPDATED = "property.ranking.updated"
    OUTREACH_APPROVAL_REQUESTED = "outreach.approval_requested"
    OUTREACH_APPROVED = "outreach.approved"
    OUTREACH_COMPLETED = "outreach.completed"
    CALL_STARTED = "call.started"
    CALL_OWNER_HANDOFF_REQUESTED = "call.owner_handoff_requested"
    CALL_OWNER_JOINED = "call.owner_joined"
    CALL_COMPLETED = "call.completed"
    CALL_FAILED = "call.failed"
    LAYOUT_VERSION_CREATED = "layout.version_created"
    LAYOUT_VALIDATION_FAILED = "layout.validation_failed"
    LAYOUT_APPROVED = "layout.approved"
    BOM_GENERATED = "bom.generated"
    BOM_RECALCULATION_REQUIRED = "bom.recalculation_required"
    BOM_APPROVED = "bom.approved"
    PROCUREMENT_OFFER_UPDATED = "procurement.offer_updated"
    LOGISTICS_TIMELINE_UPDATED = "logistics.timeline_updated"
    APPROVAL_REQUESTED = "approval.requested"
    APPROVAL_DECIDED = "approval.decided"
    TASK_STARTED = "task.started"
    TASK_COMPLETED = "task.completed"
    TASK_FAILED = "task.failed"


class ActorType(StrEnum):
    AGENT = "agent"
    USER = "user"
    SYSTEM = "system"
    PROVIDER = "provider"


# ---- payloads (v1.0) -------------------------------------------------------


class ProfileConfirmedPayload(Contract):
    profile_id: UUID


class PropertySearchCompletedPayload(Contract):
    property_ids: list[UUID]
    provider: str
    data_origin: str


class PropertyRefPayload(Contract):
    property_id: UUID
    summary: str | None = None


class PermitsIngestedPayload(Contract):
    property_id: UUID
    permit_record_ids: list[UUID]
    evidence_ids: list[UUID]
    source_dataset_ids: list[str]


class AnalysisCompletedPayload(Contract):
    property_id: UUID
    assessment_ids: list[UUID]
    unknown_requirement_ids: list[str] = Field(default_factory=list)


class UnknownDetectedPayload(Contract):
    property_id: UUID
    requirement_ids: list[str]
    verification_questions: list[str]


class EvidenceUpdatedPayload(Contract):
    property_id: UUID
    evidence_ids: list[UUID]
    reason: str


class ScenarioUpdatedPayload(Contract):
    property_id: UUID
    scenario_id: UUID
    long_term_cost: int
    comparison_horizon_months: int


class RankingUpdatedPayload(Contract):
    ranking_id: UUID
    ranking_version: int
    top_property_id: UUID | None


class OutreachPayload(Contract):
    communication_id: UUID
    property_id: UUID | None = None
    approval_id: UUID | None = None


class CallPayload(Contract):
    call_id: UUID
    state: str
    detail: str | None = None


class HandoffRequestedPayload(Contract):
    call_id: UUID
    handoff_request_id: UUID
    reason: str
    # NOTE: never include join tokens here.


class LayoutPayload(Contract):
    design_version_id: UUID | None
    property_id: UUID
    valid: bool
    issue_codes: list[str] = Field(default_factory=list)


class BomPayload(Contract):
    bom_id: UUID
    design_version_id: UUID
    estimated_total: int | None = None


class OfferPayload(Contract):
    offer_id: UUID
    bom_item_id: UUID


class TimelinePayload(Contract):
    milestone: str
    detail: str | None = None


class ApprovalPayload(Contract):
    approval_id: UUID
    action_type: str
    approval_status: str


class TaskPayload(Contract):
    task_id: UUID
    task_type: str
    status: str
    error_code: str | None = None


EVENT_PAYLOADS: dict[EventType, type[Contract]] = {
    EventType.PROJECT_PROFILE_CONFIRMED: ProfileConfirmedPayload,
    EventType.PROPERTY_SEARCH_COMPLETED: PropertySearchCompletedPayload,
    EventType.PROPERTY_IDENTITY_RESOLVED: PropertyRefPayload,
    EventType.PROPERTY_PERMITS_INGESTED: PermitsIngestedPayload,
    EventType.PROPERTY_SPATIAL_ANALYSIS_COMPLETED: PropertyRefPayload,
    EventType.PROPERTY_ANALYSIS_COMPLETED: AnalysisCompletedPayload,
    EventType.INFRASTRUCTURE_UNKNOWN_DETECTED: UnknownDetectedPayload,
    EventType.INFRASTRUCTURE_EVIDENCE_UPDATED: EvidenceUpdatedPayload,
    EventType.FINANCIAL_SCENARIO_UPDATED: ScenarioUpdatedPayload,
    EventType.PROPERTY_RANKING_UPDATED: RankingUpdatedPayload,
    EventType.OUTREACH_APPROVAL_REQUESTED: OutreachPayload,
    EventType.OUTREACH_APPROVED: OutreachPayload,
    EventType.OUTREACH_COMPLETED: OutreachPayload,
    EventType.CALL_STARTED: CallPayload,
    EventType.CALL_OWNER_HANDOFF_REQUESTED: HandoffRequestedPayload,
    EventType.CALL_OWNER_JOINED: CallPayload,
    EventType.CALL_COMPLETED: CallPayload,
    EventType.CALL_FAILED: CallPayload,
    EventType.LAYOUT_VERSION_CREATED: LayoutPayload,
    EventType.LAYOUT_VALIDATION_FAILED: LayoutPayload,
    EventType.LAYOUT_APPROVED: LayoutPayload,
    EventType.BOM_GENERATED: BomPayload,
    EventType.BOM_RECALCULATION_REQUIRED: BomPayload,
    EventType.BOM_APPROVED: BomPayload,
    EventType.PROCUREMENT_OFFER_UPDATED: OfferPayload,
    EventType.LOGISTICS_TIMELINE_UPDATED: TimelinePayload,
    EventType.APPROVAL_REQUESTED: ApprovalPayload,
    EventType.APPROVAL_DECIDED: ApprovalPayload,
    EventType.TASK_STARTED: TaskPayload,
    EventType.TASK_COMPLETED: TaskPayload,
    EventType.TASK_FAILED: TaskPayload,
}

_FORBIDDEN_PAYLOAD_KEYS = {"join_token", "token", "api_key", "secret", "password"}


class ActivityEvent(Contract):
    schema_version: str = SCHEMA_VERSION
    event_id: UUID = Field(default_factory=new_id)
    event_type: EventType
    project_id: UUID
    correlation_id: UUID
    causation_id: UUID | None = None
    idempotency_key: str = Field(min_length=1, max_length=255)
    actor_type: ActorType
    actor_id: str
    occurred_at: UtcDatetime = Field(default_factory=utcnow)
    payload: dict[str, Any]

    @model_validator(mode="after")
    def _validate_payload(self) -> ActivityEvent:
        if _FORBIDDEN_PAYLOAD_KEYS & set(self.payload):
            raise ValueError("event payloads must not carry tokens or secrets")
        model = EVENT_PAYLOADS[self.event_type]
        self.payload = model.model_validate(self.payload).model_dump(mode="json")
        return self

    def typed_payload(self) -> Contract:
        return EVENT_PAYLOADS[self.event_type].model_validate(self.payload)
