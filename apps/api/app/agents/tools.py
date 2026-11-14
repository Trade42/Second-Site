"""Supervisor tools. Each tool declares a ToolCategory; external/consequential tools are wrapped by
`approval_guard`, which enforces ApprovalService at the call boundary (a prompt is not enforcement).

Tools receive a ToolContext (db session, project, owner) bound per request; they return compact JSON
strings grounded in persisted records (with record IDs) — never free-form claims.

Two modes, same tools, same JSON text:
- BACKEND_API_URL unset (default): in-process, against the database through the repositories.
- BACKEND_API_URL set: each tool calls the Rust API (`backend_client.py`) as the project owner. The outreach tool
  still only creates a PENDING approval (`POST .../outreach`); nothing in this mode can execute an external action.
  A failed call is returned to the model as a short error text (ToolException), never as an empty success.
"""

from __future__ import annotations

import json
from collections.abc import Callable, Iterable
from dataclasses import dataclass, field
from typing import Any
from uuid import UUID, uuid4

import httpx
from langchain_core.tools import StructuredTool
from sqlalchemy.orm import Session

from app.contracts import (
    ActionType,
    ApprovalStatus,
    BusinessExpansionProfile,
    Channel,
    CostScenario,
    InfrastructureAssessment,
    ProjectContext,
    PropertyRankingResult,
    PropertySearchRequest,
    SourceEvidence,
)
from app.core.container import Container
from app.domains.evidence.validator import validate_property
from app.repositories import registry as repos
from app.services.authorization import APPROVAL_REQUIRED, ToolCategory
from app.workflows.property_analysis import PropertyAnalysisWorkflow

from .backend_client import BackendAPIError, BackendClient, backend_client, parse_id


@dataclass
class ToolContext:
    db: Session
    c: Container
    project_id: UUID
    owner_id: UUID
    correlation_id: UUID = field(default_factory=uuid4)
    touched_records: list[UUID] = field(default_factory=list)
    # HTTP mode only: the transport that reaches the Rust API. None = a real network client. Tests pass an
    # httpx.MockTransport here to stand in for the Rust service.
    backend_transport: httpx.BaseTransport | None = None


TOOL_CATEGORIES: dict[str, ToolCategory] = {}

APPROVAL_REQUESTED_MESSAGE = "Owner approval requested; nothing was sent."


def _j(obj: Any) -> str:
    return json.dumps(obj, default=str)


# ---- output shapes, shared by both modes ---------------------------------------------------------
def _project_view(project: ProjectContext, profile: BusinessExpansionProfile | None) -> str:
    return _j(
        {
            "project": project.model_dump(mode="json"),
            "business_profile": profile.model_dump(mode="json") if profile else None,
        }
    )


def _ranking_view(ctx: ToolContext, rankings: list[PropertyRankingResult]) -> str:
    if not rankings:
        return _j({"status": "NO_RANKING"})
    r = rankings[-1]
    ctx.touched_records.append(r.ranking_id)
    return _j(
        {
            "ranking_id": r.ranking_id,
            "ranked": [x.model_dump(mode="json") for x in r.ranked_properties],
            "explanations": r.explanations,
        }
    )


def _feasibility_view(assessments: Iterable[InfrastructureAssessment]) -> str:
    return _j(
        [
            {
                "requirement": a.requirement_id,
                "status": a.status.value,
                "observed": a.observed_value,
                "questions": a.verification_questions,
                "evidence_ids": a.evidence_ids,
            }
            for a in assessments
        ]
    )


def _evidence_view(evidence: Iterable[SourceEvidence]) -> str:
    return _j(
        [
            {
                "id": e.evidence_id,
                "type": e.observation_type,
                "value": e.extracted_value,
                "unit": e.unit,
                "class": e.evidence_classification.value,
                "source": e.source_record_id,
                "limitations": e.limitations[:2],
            }
            for e in evidence
        ]
    )


def _scenarios_view(scenarios: Iterable[CostScenario]) -> str:
    return _j(
        [
            {
                "property_id": s.property_id,
                "opening_expected": s.opening_cost_expected,
                "monthly": s.monthly_operating_cost,
                "long_term": s.long_term_cost,
                "horizon": s.comparison_horizon_months,
            }
            for s in scenarios
        ]
    )


def approval_guard(ctx: ToolContext, action: ActionType, fn: Callable[..., str]) -> Callable[..., str]:
    """Never executes the action. Creates (or reuses) a PENDING approval and returns its id; the owner
    approves in the UI, and the execution endpoint re-checks `require_approved` before acting."""

    def wrapped(**kwargs: Any) -> str:
        payload = {"project_id": str(ctx.project_id), **kwargs}
        appr = ctx.c.approvals.request(
            ctx.db,
            project_id=ctx.project_id,
            owner_id=ctx.owner_id,
            agent="expansion-supervisor",
            action_type=action,
            payload=payload,
            summary=fn(**kwargs),
            idempotency_key=f"agent:{action.value}:{ctx.project_id}:{sorted(kwargs.items())}",
        )
        ctx.touched_records.append(appr.approval_id)
        return _j(
            {
                "status": "APPROVAL_REQUIRED",
                "approval_id": appr.approval_id,
                "message": APPROVAL_REQUESTED_MESSAGE,
            }
        )

    return wrapped


def http_outreach_guard(ctx: ToolContext, api: BackendClient) -> Callable[..., str]:
    """HTTP mode of `approval_guard` for PLACE_CALL. Never executes the action either: the only call is
    `POST .../outreach`, which creates a communication draft and a PENDING approval on the Rust side. The owner
    approves in the UI; the call itself needs that approval at the Rust execution boundary."""

    def wrapped(property_id: str, recipient_name: str, recipient_phone: str, questions: str) -> str:
        pid = parse_id(property_id, "property_id")
        appr = api.request_outreach(
            ctx.project_id,
            {
                "property_id": str(pid),
                "channel": Channel.VOICE.value,
                "recipient": {"name": recipient_name, "phone": recipient_phone},
                "purpose": questions,
                "questions": [questions],
            },
        )
        if (
            appr.project_id != ctx.project_id
            or appr.authorized_owner_id != ctx.owner_id
            or appr.action_type != ActionType.PLACE_CALL
        ):
            raise BackendAPIError(
                "Rust API returned an approval for another project, owner or action",
                error_code="BACKEND_CONTRACT_MISMATCH",
            )
        ctx.touched_records.append(appr.approval_id)
        message = APPROVAL_REQUESTED_MESSAGE
        if appr.approval_status != ApprovalStatus.PENDING:
            message = f"This outreach already has an approval in status {appr.approval_status.value}; nothing was sent."
        return _j({"status": "APPROVAL_REQUIRED", "approval_id": appr.approval_id, "message": message})

    return wrapped


def build_tools(ctx: ToolContext) -> list[StructuredTool]:
    # None unless BACKEND_API_URL is set: then every tool goes through the Rust API instead of the database.
    api = backend_client(ctx.c.settings, ctx.owner_id, ctx.backend_transport)
    wf = PropertyAnalysisWorkflow(ctx.c) if api is None else None

    def get_project_state() -> str:
        """Read the authoritative project record and business profile (read-only)."""
        if api is not None:
            env = api.get_project(ctx.project_id)
            return _project_view(env.project, env.business_profile)
        p = repos.projects.require(ctx.db, ctx.project_id)
        prof = repos.profiles.list(ctx.db, project_id=ctx.project_id)
        return _project_view(p, prof[-1] if prof else None)

    def run_property_analysis(city: str = "Los Angeles", state: str = "CA") -> str:
        """Search candidate properties, attach permit evidence, assess infrastructure, cost and rank (internal write)."""
        req = PropertySearchRequest(city=city, state=state)
        if api is not None:
            # Only city/state are sent, so the Rust route fills unset limits from the profile as in-process does.
            out = api.run_property_search(
                ctx.project_id, req.model_dump(mode="json", include={"city", "state"})
            )
        else:
            out = wf.run(ctx.db, ctx.project_id, req, ctx.correlation_id)
        ctx.touched_records += [UUID(x) for x in out["property_ids"]]
        return _j(out)

    def get_ranking() -> str:
        """Latest property ranking with deterministic explanations (read-only)."""
        if api is not None:
            return _ranking_view(ctx, api.list_rankings(ctx.project_id))
        return _ranking_view(ctx, repos.rankings.list(ctx.db, project_id=ctx.project_id))

    def get_feasibility(property_id: str) -> str:
        """Current infrastructure assessments for one property, including UNKNOWNs (read-only)."""
        if api is not None:
            pid = parse_id(property_id, "property_id")
            return _feasibility_view(api.property_feasibility(ctx.project_id, pid))
        prop = repos.properties.require(ctx.db, UUID(property_id))
        return _feasibility_view(prop.infrastructure_assessments)

    def get_property_evidence(property_id: str) -> str:
        """Sourced evidence (permits etc.) with classification and limitations (read-only)."""
        if api is not None:
            pid = parse_id(property_id, "property_id")
            return _evidence_view(api.property_evidence(ctx.project_id, pid))
        return _evidence_view(ctx.c.evidence.for_property(ctx.db, UUID(property_id)))

    def get_cost_scenarios() -> str:
        """Current cost scenarios per property (read-only, integer cents)."""
        if api is not None:
            return _scenarios_view(api.cost_scenarios(ctx.project_id))
        return _scenarios_view(repos.cost_scenarios.list(ctx.db, project_id=ctx.project_id, superseded=False))

    def validate_evidence(property_id: str) -> str:
        """Run the evidence/compliance validator on a property (read-only)."""
        if api is not None:
            pid = parse_id(property_id, "property_id")
            return _j({"problems": api.property_evidence_problems(ctx.project_id, pid)})
        prop = repos.properties.require(ctx.db, UUID(property_id))
        problems = validate_property(
            prop, ctx.c.evidence.for_property(ctx.db, prop.property_id), prop.infrastructure_assessments
        )
        return _j({"problems": problems})

    def _outreach_summary(property_id: str, recipient_name: str, recipient_phone: str, questions: str) -> str:
        return f"Call {recipient_name} ({recipient_phone}) about property {property_id}: {questions}"

    if api is not None:
        request_outreach_approval = http_outreach_guard(ctx, api)
    else:
        request_outreach_approval = approval_guard(ctx, ActionType.PLACE_CALL, _outreach_summary)
    request_outreach_approval.__doc__ = (
        "Ask the owner to approve contacting a broker/property manager. "
        "Does NOT contact anyone (external communication)."
    )

    # HTTP mode: a BackendAPIError (ToolException) becomes the tool's text for the model, marked as an error.
    handle_errors = api is not None
    spec: list[tuple[Callable[..., str], ToolCategory, str]] = [
        (get_project_state, ToolCategory.READ_ONLY, "get_project_state"),
        (run_property_analysis, ToolCategory.INTERNAL_WRITE, "run_property_analysis"),
        (get_ranking, ToolCategory.READ_ONLY, "get_ranking"),
        (get_feasibility, ToolCategory.READ_ONLY, "get_feasibility"),
        (get_property_evidence, ToolCategory.READ_ONLY, "get_property_evidence"),
        (get_cost_scenarios, ToolCategory.READ_ONLY, "get_cost_scenarios"),
        (validate_evidence, ToolCategory.READ_ONLY, "validate_evidence"),
    ]
    tools = [
        StructuredTool.from_function(fn, name=name, description=fn.__doc__, handle_tool_error=handle_errors)
        for fn, _cat, name in spec
    ]

    def request_outreach_approval_tool(
        property_id: str, recipient_name: str, recipient_phone: str, questions: str
    ) -> str:
        """Ask the owner to approve contacting a broker/property manager. Does NOT contact anyone."""
        return request_outreach_approval(
            property_id=property_id,
            recipient_name=recipient_name,
            recipient_phone=recipient_phone,
            questions=questions,
        )

    tools.append(
        StructuredTool.from_function(
            request_outreach_approval_tool,
            name="request_outreach_approval",
            description=request_outreach_approval_tool.__doc__,
            handle_tool_error=handle_errors,
        )
    )
    for _fn, cat, name in spec:
        TOOL_CATEGORIES[name] = cat
    TOOL_CATEGORIES["request_outreach_approval"] = ToolCategory.EXTERNAL_COMMUNICATION
    # Invariant: every tool that could act externally goes through approval_guard.
    assert all(
        TOOL_CATEGORIES[t.name] not in APPROVAL_REQUIRED or t.name == "request_outreach_approval"
        for t in tools
    )
    return tools
