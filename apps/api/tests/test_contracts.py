"""Schema validation, required fields, state transitions, serialization, money, geometry, events."""

from __future__ import annotations

import uuid
from datetime import UTC, datetime, timedelta
from decimal import Decimal

import pytest
from pydantic import ValidationError

from app.contracts import (
    CONTRACT_REGISTRY,
    ActionType,
    ActivityEvent,
    ActorType,
    ApprovalRequest,
    ApprovalStatus,
    AssessmentStatus,
    CallSession,
    CallState,
    CostKind,
    CostLineItem,
    CostScenario,
    DataOrigin,
    DesignApprovalStatus,
    DesignVersion,
    EventType,
    EvidenceClassification,
    GeometrySource,
    GeometryValidationReport,
    InfrastructureAssessment,
    InvalidStateTransition,
    ProjectContext,
    RenovationWorkItem,
    RiskCategory,
    SiteGeometry,
    SourceEvidence,
    Subsystem,
    TaskRecord,
    TaskStatus,
    TaskType,
    WorkCategory,
)
from app.contracts.events import EVENT_PAYLOADS

U = uuid.uuid4


def test_every_contract_has_a_json_schema():
    assert len(CONTRACT_REGISTRY) > 60
    for name, model in CONTRACT_REGISTRY.items():
        assert model.model_json_schema()["title"] == name


def test_every_event_type_has_a_payload_model():
    assert set(EVENT_PAYLOADS) == set(EventType)


def test_project_requires_fields_and_keeps_nullable_ids_null():
    with pytest.raises(ValidationError):
        ProjectContext(project_name="x", market={"city": "Los Angeles", "state": "CA"})  # owner_id missing
    p = ProjectContext(owner_id=U(), project_name="x", market={"city": "Los Angeles", "state": "CA"})
    assert p.selected_property_id is None and p.selected_design_version_id is None
    assert ProjectContext.model_validate_json(p.model_dump_json()) == p


def test_extra_fields_are_rejected():
    with pytest.raises(ValidationError):
        ProjectContext(owner_id=U(), project_name="x", market={"city": "LA", "state": "CA"}, surprise=1)


def test_naive_timestamps_rejected():
    with pytest.raises(ValidationError):
        ProjectContext(
            owner_id=U(),
            project_name="x",
            market={"city": "LA", "state": "CA"},
            created_at=datetime(2026, 11, 14, 12, 0),
        )


def test_money_is_integer_minor_units_and_ordered():
    with pytest.raises(ValidationError):
        RenovationWorkItem(
            property_id=U(),
            category=WorkCategory.PLUMBING,
            description="x",
            quantity=Decimal(1),
            unit="ea",
            unit_cost_low=500,
            unit_cost_expected=400,
            unit_cost_high=900,
        )
    with pytest.raises(ValidationError):
        RenovationWorkItem(
            property_id=U(),
            category=WorkCategory.PLUMBING,
            description="x",
            quantity=Decimal(1),
            unit="ea",
            unit_cost_low=1.5,
            unit_cost_expected=2,
            unit_cost_high=3,
        )


def test_cost_scenario_rejects_duplicate_lines():
    li = CostLineItem(
        line_item_id="a", category="X", description="x", kind=CostKind.ONE_TIME, low=1, expected=1, high=1
    )
    with pytest.raises(ValidationError, match="double counting"):
        CostScenario(
            project_id=U(),
            property_id=U(),
            opening_cost_low=1,
            opening_cost_expected=1,
            opening_cost_high=1,
            upfront_cash_expected=1,
            monthly_operating_cost=0,
            comparison_horizon_months=36,
            long_term_cost=1,
            contingency=0,
            line_items=[li, li],
        )


def test_historical_evidence_must_state_limitations():
    with pytest.raises(ValidationError, match="limitations"):
        SourceEvidence(
            provider="la_open_data",
            data_origin=DataOrigin.FIXTURE_REAL,
            observation_type="x",
            evidence_classification=EvidenceClassification.DOCUMENTED_HISTORICAL,
        )


def test_unknown_is_never_meets_and_needs_questions():
    with pytest.raises(ValidationError):
        InfrastructureAssessment(
            project_id=U(),
            property_id=U(),
            requirement_id="r",
            status=AssessmentStatus.MEETS,
            uncertainty="none",
        )  # MEETS without evidence
    with pytest.raises(ValidationError):
        InfrastructureAssessment(
            project_id=U(),
            property_id=U(),
            requirement_id="r",
            status=AssessmentStatus.UNKNOWN,
            uncertainty="?",
        )  # UNKNOWN without verification questions


def _site(**kw):
    base = dict(
        property_id=U(),
        source_id="s",
        geometry_source=GeometrySource.FIXTURE,
        data_origin=DataOrigin.FIXTURE_SYNTHETIC,
        floor_boundary=[(0, 0), (10, 0), (10, 8), (0, 8)],
    )
    base.update(kw)
    return SiteGeometry(**base)


def test_geometry_validity():
    _site()
    with pytest.raises(ValidationError, match="not a valid polygon"):
        _site(floor_boundary=[(0, 0), (10, 10), (10, 0), (0, 10)])  # bow-tie
    with pytest.raises(ValidationError):
        _site(floor_boundary=[(0, 0), (1, 1)])
    with pytest.raises(ValidationError, match="photo"):
        _site(geometry_source=GeometrySource.PHOTO_ESTIMATE, scale_verified=True)
    with pytest.raises(ValidationError):
        _site(units="ft")


def test_invalid_layout_cannot_be_approved():
    with pytest.raises(ValidationError):
        DesignVersion(
            project_id=U(),
            property_id=U(),
            version_number=1,
            geometry_id=U(),
            room_polygons=[],
            validation_report=GeometryValidationReport(valid=False),
            approval_status=DesignApprovalStatus.APPROVED,
        )


def test_approval_state_machine_and_payload_binding():
    a = ApprovalRequest(
        project_id=U(),
        requested_by_agent="t",
        action_type=ActionType.PURCHASE,
        action_payload={"sku": "x", "qty": 1},
        risk_category=RiskCategory.CONSEQUENTIAL,
        summary="s",
        expires_at=datetime.now(UTC) + timedelta(hours=1),
        authorized_owner_id=U(),
        idempotency_key="k",
    )
    assert a.action_payload_hash and len(a.action_payload_hash) == 64
    b = a.transition(ApprovalStatus.APPROVED).transition(ApprovalStatus.EXECUTED)
    assert b.approval_status == ApprovalStatus.EXECUTED
    with pytest.raises(InvalidStateTransition):
        b.transition(ApprovalStatus.APPROVED)
    with pytest.raises(InvalidStateTransition):
        a.transition(ApprovalStatus.REJECTED).transition(ApprovalStatus.APPROVED)


def test_call_state_machine():
    c = CallSession(project_id=U(), owner_id=U(), recipient={"phone": "+15555550100"}, approval_id=U())
    for s in [
        CallState.DIALING,
        CallState.INTRO,
        CallState.ACTIVE,
        CallState.HANDOFF_PENDING,
        CallState.OWNER_JOINING,
        CallState.HUMAN_JOINED,
        CallState.AI_RESUMED,
        CallState.COMPLETED,
    ]:
        c = c.transition(s)
    with pytest.raises(InvalidStateTransition):
        c.transition(CallState.ACTIVE)
    with pytest.raises(InvalidStateTransition):
        CallSession(project_id=U(), owner_id=U(), recipient={}, approval_id=U()).transition(
            CallState.HUMAN_JOINED
        )


def test_task_state_machine():
    t = TaskRecord(
        project_id=U(), task_type=TaskType.ANALYZE_PROPERTY, assigned_subsystem=Subsystem.AGENTS_FINANCE
    )
    with pytest.raises(InvalidStateTransition):
        t.transition(TaskStatus.COMPLETED)
    assert (
        t.transition(TaskStatus.RUNNING).transition(TaskStatus.FAILED).transition(TaskStatus.QUEUED).status
        == TaskStatus.QUEUED
    )


def test_event_envelope_validates_payload_and_forbids_secrets():
    ev = ActivityEvent(
        event_type=EventType.TASK_STARTED,
        project_id=U(),
        correlation_id=U(),
        idempotency_key="k",
        actor_type=ActorType.SYSTEM,
        actor_id="x",
        payload={"task_id": str(U()), "task_type": "x", "status": "RUNNING"},
    )
    assert ev.schema_version == "1.0"
    assert ActivityEvent.model_validate_json(ev.model_dump_json()).event_id == ev.event_id
    with pytest.raises(ValidationError):
        ActivityEvent(
            event_type=EventType.TASK_STARTED,
            project_id=U(),
            correlation_id=U(),
            idempotency_key="k",
            actor_type=ActorType.SYSTEM,
            actor_id="x",
            payload={"wrong": 1},
        )
    with pytest.raises(ValidationError, match="tokens or secrets"):
        ActivityEvent(
            event_type=EventType.CALL_OWNER_HANDOFF_REQUESTED,
            project_id=U(),
            correlation_id=U(),
            idempotency_key="k",
            actor_type=ActorType.AGENT,
            actor_id="x",
            payload={"call_id": str(U()), "handoff_request_id": str(U()), "reason": "X", "join_token": "t"},
        )
