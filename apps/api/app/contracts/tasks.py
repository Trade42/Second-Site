from __future__ import annotations

from enum import StrEnum
from typing import Any
from uuid import UUID

from pydantic import Field

from .common import SCHEMA_VERSION, Contract, UtcDatetime, new_id, utcnow
from .state import check_transition


class TaskStatus(StrEnum):
    QUEUED = "QUEUED"
    RUNNING = "RUNNING"
    WAITING_APPROVAL = "WAITING_APPROVAL"
    COMPLETED = "COMPLETED"
    FAILED = "FAILED"
    CANCELLED = "CANCELLED"


TASK_TRANSITIONS: dict[TaskStatus, frozenset[TaskStatus]] = {
    TaskStatus.QUEUED: frozenset({TaskStatus.RUNNING, TaskStatus.CANCELLED}),
    TaskStatus.RUNNING: frozenset(
        {TaskStatus.COMPLETED, TaskStatus.FAILED, TaskStatus.WAITING_APPROVAL, TaskStatus.CANCELLED}
    ),
    TaskStatus.WAITING_APPROVAL: frozenset({TaskStatus.RUNNING, TaskStatus.CANCELLED, TaskStatus.FAILED}),
    TaskStatus.FAILED: frozenset({TaskStatus.QUEUED}),  # retry
    TaskStatus.COMPLETED: frozenset(),
    TaskStatus.CANCELLED: frozenset(),
}


class TaskType(StrEnum):
    CONFIRM_REQUIREMENTS = "confirm_requirements"
    SEARCH_PROPERTIES = "search_properties"
    RESOLVE_IDENTITY = "resolve_identity"
    INGEST_PERMITS = "ingest_permits"
    ANALYZE_PROPERTY = "analyze_property"
    EVALUATE_INFRASTRUCTURE = "evaluate_infrastructure"
    CALCULATE_COSTS = "calculate_costs"
    RANK_PROPERTIES = "rank_properties"
    GENERATE_LAYOUT = "generate_layout"
    REVISE_LAYOUT = "revise_layout"
    GENERATE_BOM = "generate_bom"
    OUTREACH = "outreach"
    PLACE_CALL = "place_call"
    SUPERVISOR_CHAT = "supervisor_chat"


class Subsystem(StrEnum):
    AGENTS_FINANCE = "agents-finance"
    PROPERTY_SPATIAL = "property-spatial"
    FRONTEND_PRODUCT = "frontend-product"
    COMMUNICATIONS = "communications"
    FOUNDATION = "foundation"


class TaskRecord(Contract):
    task_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    task_type: TaskType
    status: TaskStatus = TaskStatus.QUEUED
    assigned_subsystem: Subsystem
    input_reference: dict[str, Any] = Field(default_factory=dict)
    output_reference: dict[str, Any] = Field(default_factory=dict)
    dependencies: list[UUID] = Field(default_factory=list)
    attempt_count: int = Field(default=0, ge=0)
    error_code: str | None = None
    error_message: str | None = None
    correlation_id: UUID = Field(default_factory=new_id)
    created_at: UtcDatetime = Field(default_factory=utcnow)
    updated_at: UtcDatetime = Field(default_factory=utcnow)

    def transition(self, target: TaskStatus) -> TaskRecord:
        check_transition("task", TASK_TRANSITIONS, self.status, target)
        return self.model_copy(update={"status": target, "updated_at": utcnow()})


class AgentTaskRequest(Contract):
    """Structured delegation from the supervisor to a specialist. Free text is NOT the contract."""

    task_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    task_type: TaskType
    source_record_ids: list[UUID] = Field(default_factory=list)
    input_schema_version: str = SCHEMA_VERSION
    expected_output_schema: str = Field(description="Contract name, e.g. 'InfrastructureAssessment'")
    constraints: dict[str, Any] = Field(default_factory=dict)
    correlation_id: UUID = Field(default_factory=new_id)


class AgentTaskStatus(StrEnum):
    COMPLETED = "COMPLETED"
    PARTIAL = "PARTIAL"
    FAILED = "FAILED"
    NEEDS_APPROVAL = "NEEDS_APPROVAL"


class AgentTaskResult(Contract):
    task_id: UUID
    status: AgentTaskStatus
    output_record_ids: list[UUID] = Field(default_factory=list)
    evidence_ids: list[UUID] = Field(default_factory=list)
    unresolved_questions: list[str] = Field(default_factory=list)
    errors: list[str] = Field(default_factory=list)
    completed_at: UtcDatetime = Field(default_factory=utcnow)
