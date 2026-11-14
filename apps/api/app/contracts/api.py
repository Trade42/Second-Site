"""HTTP request/response shapes that are not domain records."""

from __future__ import annotations

from typing import Any
from uuid import UUID

from pydantic import Field

from .common import Contract, IntegrationStatus
from .project import BusinessExpansionProfile, Market, ProjectContext
from .tasks import TaskRecord


class ProblemDetail(Contract):
    error_code: str
    message: str
    details: dict[str, Any] = Field(default_factory=dict)
    request_id: str | None = None
    retryable: bool = False


class CreateProjectRequest(Contract):
    project_name: str = Field(min_length=1, max_length=200)
    market: Market
    business_profile: dict[str, Any] | None = Field(
        default=None, description="Optional BusinessExpansionProfile fields (project_id/profile_id assigned)."
    )


class ProjectEnvelope(Contract):
    project: ProjectContext
    business_profile: BusinessExpansionProfile | None = None


class ChatRequest(Contract):
    message: str = Field(min_length=1, max_length=8000)


class ChatResponse(Contract):
    reply: str
    task_ids: list[UUID] = Field(default_factory=list)
    mode: str = Field(description="'deep_agent' (live model) or 'deterministic' (no model configured)")
    grounded_record_ids: list[UUID] = Field(default_factory=list)


class TaskAccepted(Contract):
    task: TaskRecord
    status_url: str


class ProviderStatusEntry(Contract):
    provider: str
    mode: str
    status: IntegrationStatus
    credentials_present: bool
    note: str


class HealthResponse(Contract):
    status: str
    version: str


class ReadyResponse(Contract):
    status: str
    checks: dict[str, str]
    providers: list[ProviderStatusEntry]


class DevTokenResponse(Contract):
    user_id: UUID
    access_token: str
    token_type: str = "bearer"
