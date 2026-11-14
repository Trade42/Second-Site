"""Supervisor tools. Each tool declares a ToolCategory; external/consequential tools are wrapped by
`approval_guard`, which enforces ApprovalService at the call boundary (a prompt is not enforcement).

Tools receive a ToolContext (db session, project, owner) bound per request; they return compact JSON
strings grounded in persisted records (with record IDs) — never free-form claims.
"""

from __future__ import annotations

import json
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any
from uuid import UUID, uuid4

from langchain_core.tools import StructuredTool
from sqlalchemy.orm import Session

from app.contracts import ActionType, PropertySearchRequest
from app.core.container import Container
from app.domains.evidence.validator import validate_property
from app.repositories import registry as repos
from app.services.authorization import APPROVAL_REQUIRED, ToolCategory
from app.workflows.property_analysis import PropertyAnalysisWorkflow


@dataclass
class ToolContext:
    db: Session
    c: Container
    project_id: UUID
    owner_id: UUID
    correlation_id: UUID = field(default_factory=uuid4)
    touched_records: list[UUID] = field(default_factory=list)


TOOL_CATEGORIES: dict[str, ToolCategory] = {}


def _j(obj: Any) -> str:
    return json.dumps(obj, default=str)


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
                "message": "Owner approval requested; nothing was sent.",
            }
        )

    return wrapped


def build_tools(ctx: ToolContext) -> list[StructuredTool]:
    wf = PropertyAnalysisWorkflow(ctx.c)

    def get_project_state() -> str:
        """Read the authoritative project record and business profile (read-only)."""
        p = repos.projects.require(ctx.db, ctx.project_id)
        prof = repos.profiles.list(ctx.db, project_id=ctx.project_id)
        return _j(
            {
                "project": p.model_dump(mode="json"),
                "business_profile": prof[-1].model_dump(mode="json") if prof else None,
            }
        )

    def run_property_analysis(city: str = "Los Angeles", state: str = "CA") -> str:
        """Search candidate properties, attach permit evidence, assess infrastructure, cost and rank (internal write)."""
        out = wf.run(
            ctx.db, ctx.project_id, PropertySearchRequest(city=city, state=state), ctx.correlation_id
        )
        ctx.touched_records += [UUID(x) for x in out["property_ids"]]
        return _j(out)

    def get_ranking() -> str:
        """Latest property ranking with deterministic explanations (read-only)."""
        rs = repos.rankings.list(ctx.db, project_id=ctx.project_id)
        if not rs:
            return _j({"status": "NO_RANKING"})
        r = rs[-1]
        ctx.touched_records.append(r.ranking_id)
        return _j(
            {
                "ranking_id": r.ranking_id,
                "ranked": [x.model_dump(mode="json") for x in r.ranked_properties],
                "explanations": r.explanations,
            }
        )

    def get_feasibility(property_id: str) -> str:
        """Current infrastructure assessments for one property, including UNKNOWNs (read-only)."""
        prop = repos.properties.require(ctx.db, UUID(property_id))
        return _j(
            [
                {
                    "requirement": a.requirement_id,
                    "status": a.status.value,
                    "observed": a.observed_value,
                    "questions": a.verification_questions,
                    "evidence_ids": a.evidence_ids,
                }
                for a in prop.infrastructure_assessments
            ]
        )

    def get_property_evidence(property_id: str) -> str:
        """Sourced evidence (permits etc.) with classification and limitations (read-only)."""
        ev = ctx.c.evidence.for_property(ctx.db, UUID(property_id))
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
                for e in ev
            ]
        )

    def get_cost_scenarios() -> str:
        """Current cost scenarios per property (read-only, integer cents)."""
        return _j(
            [
                {
                    "property_id": s.property_id,
                    "opening_expected": s.opening_cost_expected,
                    "monthly": s.monthly_operating_cost,
                    "long_term": s.long_term_cost,
                    "horizon": s.comparison_horizon_months,
                }
                for s in repos.cost_scenarios.list(ctx.db, project_id=ctx.project_id, superseded=False)
            ]
        )

    def validate_evidence(property_id: str) -> str:
        """Run the evidence/compliance validator on a property (read-only)."""
        prop = repos.properties.require(ctx.db, UUID(property_id))
        problems = validate_property(
            prop, ctx.c.evidence.for_property(ctx.db, prop.property_id), prop.infrastructure_assessments
        )
        return _j({"problems": problems})

    def _outreach_summary(property_id: str, recipient_name: str, recipient_phone: str, questions: str) -> str:
        return f"Call {recipient_name} ({recipient_phone}) about property {property_id}: {questions}"

    request_outreach_approval = approval_guard(ctx, ActionType.PLACE_CALL, _outreach_summary)
    request_outreach_approval.__doc__ = (
        "Ask the owner to approve contacting a broker/property manager. "
        "Does NOT contact anyone (external communication)."
    )

    spec: list[tuple[Callable[..., str], ToolCategory, str]] = [
        (get_project_state, ToolCategory.READ_ONLY, "get_project_state"),
        (run_property_analysis, ToolCategory.INTERNAL_WRITE, "run_property_analysis"),
        (get_ranking, ToolCategory.READ_ONLY, "get_ranking"),
        (get_feasibility, ToolCategory.READ_ONLY, "get_feasibility"),
        (get_property_evidence, ToolCategory.READ_ONLY, "get_property_evidence"),
        (get_cost_scenarios, ToolCategory.READ_ONLY, "get_cost_scenarios"),
        (validate_evidence, ToolCategory.READ_ONLY, "validate_evidence"),
    ]
    tools = [StructuredTool.from_function(fn, name=name, description=fn.__doc__) for fn, _cat, name in spec]

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
