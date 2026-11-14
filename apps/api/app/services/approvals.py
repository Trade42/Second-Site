"""ApprovalService: the enforcement point for external and consequential actions.

`require_approved()` is called at the action boundary (inside the tool / provider call path),
not in a prompt. An approval authorizes only the exact payload hash the owner saw, once.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from typing import Any
from uuid import UUID

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.contracts import (
    ActionType,
    ActorType,
    ApprovalRequest,
    ApprovalStatus,
    EventType,
    InvalidStateTransition,
    RiskCategory,
    payload_hash,
)
from app.core.errors import ApprovalRequiredError, ConflictError, ForbiddenError
from app.db import models as m
from app.repositories import registry as repos

from .events import EventService

RISK = {
    ActionType.SEND_MESSAGE: RiskCategory.EXTERNAL_COMMUNICATION,
    ActionType.PLACE_CALL: RiskCategory.EXTERNAL_COMMUNICATION,
    ActionType.CONTACT_SUPPLIER: RiskCategory.EXTERNAL_COMMUNICATION,
    ActionType.PURCHASE: RiskCategory.CONSEQUENTIAL,
    ActionType.FINANCIAL_COMMITMENT: RiskCategory.CONSEQUENTIAL,
    ActionType.APPROVE_LAYOUT: RiskCategory.INTERNAL,
    ActionType.APPROVE_BOM: RiskCategory.CONSEQUENTIAL,
}


class ApprovalService:
    def __init__(self, events: EventService):
        self.events = events

    def request(
        self,
        db: Session,
        *,
        project_id: UUID,
        owner_id: UUID,
        agent: str,
        action_type: ActionType,
        payload: dict[str, Any],
        summary: str,
        idempotency_key: str,
        ttl: timedelta = timedelta(hours=24),
    ) -> ApprovalRequest:
        existing = db.scalar(select(m.Approval).where(m.Approval.idempotency_key == idempotency_key))
        if existing is not None:
            return ApprovalRequest.model_validate(existing.data)
        appr = ApprovalRequest(
            project_id=project_id,
            requested_by_agent=agent,
            action_type=action_type,
            action_payload=payload,
            risk_category=RISK[action_type],
            summary=summary,
            expires_at=datetime.now(UTC) + ttl,
            authorized_owner_id=owner_id,
            idempotency_key=idempotency_key,
        )
        repos.approvals.add(db, appr)
        self.events.emit(
            db,
            event_type=EventType.APPROVAL_REQUESTED,
            project_id=project_id,
            actor_type=ActorType.AGENT,
            actor_id=agent,
            idempotency_key=f"approval:{appr.approval_id}:requested",
            payload={
                "approval_id": str(appr.approval_id),
                "action_type": action_type.value,
                "approval_status": appr.approval_status.value,
            },
        )
        return appr

    def _expire_if_needed(self, db: Session, appr: ApprovalRequest) -> ApprovalRequest:
        if appr.approval_status in (
            ApprovalStatus.PENDING,
            ApprovalStatus.APPROVED,
        ) and appr.expires_at < datetime.now(UTC):
            appr = appr.transition(ApprovalStatus.EXPIRED)
            repos.approvals.upsert(db, appr)
        return appr

    def decide(
        self, db: Session, approval_id: UUID, user_id: UUID, decision: ApprovalStatus, presented_hash: str
    ) -> ApprovalRequest:
        appr = repos.approvals.require(db, approval_id)
        if appr.authorized_owner_id != user_id:
            raise ForbiddenError("only the authorized owner can decide this approval")
        appr = self._expire_if_needed(db, appr)
        if decision not in (ApprovalStatus.APPROVED, ApprovalStatus.REJECTED):
            raise ConflictError("decision must be APPROVED or REJECTED", error_code="INVALID_DECISION")
        if presented_hash != appr.action_payload_hash:
            raise ConflictError(
                "payload changed since it was presented; re-review required",
                error_code="APPROVAL_PAYLOAD_MISMATCH",
            )
        try:
            appr = appr.transition(decision)
        except InvalidStateTransition as exc:
            raise ConflictError(
                str(exc),
                error_code="INVALID_STATE_TRANSITION",
                details={"current": appr.approval_status.value},
            ) from exc
        appr = appr.model_copy(update={"decision_timestamp": datetime.now(UTC), "decided_by": user_id})
        repos.approvals.upsert(db, appr)
        self.events.emit(
            db,
            event_type=EventType.APPROVAL_DECIDED,
            project_id=appr.project_id,
            actor_type=ActorType.USER,
            actor_id=str(user_id),
            idempotency_key=f"approval:{appr.approval_id}:{decision.value}",
            payload={
                "approval_id": str(appr.approval_id),
                "action_type": appr.action_type.value,
                "approval_status": appr.approval_status.value,
            },
        )
        return appr

    def require_approved(
        self, db: Session, approval_id: UUID | None, action_type: ActionType, payload: dict[str, Any]
    ) -> ApprovalRequest:
        """Raise unless an unexpired APPROVED approval exists for exactly this action+payload."""
        if approval_id is None:
            raise ApprovalRequiredError(f"{action_type.value} requires owner approval")
        appr = self._expire_if_needed(db, repos.approvals.require(db, approval_id))
        if appr.action_type != action_type:
            raise ApprovalRequiredError("approval is for a different action type")
        if appr.approval_status != ApprovalStatus.APPROVED:
            raise ApprovalRequiredError(
                f"approval is {appr.approval_status.value}",
                details={"approval_status": appr.approval_status.value},
            )
        if payload_hash(payload) != appr.action_payload_hash:
            raise ApprovalRequiredError(
                "payload differs from the approved payload", error_code="APPROVAL_PAYLOAD_MISMATCH"
            )
        return appr

    def mark_executed(self, db: Session, appr: ApprovalRequest) -> ApprovalRequest:
        appr = appr.transition(ApprovalStatus.EXECUTED)
        repos.approvals.upsert(db, appr)
        return appr
