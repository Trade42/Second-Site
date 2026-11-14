from __future__ import annotations

import hashlib
import json
from enum import StrEnum
from typing import Any
from uuid import UUID

from pydantic import Field

from .common import Contract, UtcDatetime, new_id, utcnow
from .state import check_transition


class ApprovalStatus(StrEnum):
    PENDING = "PENDING"
    APPROVED = "APPROVED"
    REJECTED = "REJECTED"
    EXPIRED = "EXPIRED"
    CANCELLED = "CANCELLED"
    EXECUTED = "EXECUTED"


APPROVAL_TRANSITIONS: dict[ApprovalStatus, frozenset[ApprovalStatus]] = {
    ApprovalStatus.PENDING: frozenset(
        {ApprovalStatus.APPROVED, ApprovalStatus.REJECTED, ApprovalStatus.EXPIRED, ApprovalStatus.CANCELLED}
    ),
    ApprovalStatus.APPROVED: frozenset(
        {ApprovalStatus.EXECUTED, ApprovalStatus.EXPIRED, ApprovalStatus.CANCELLED}
    ),
    ApprovalStatus.REJECTED: frozenset(),
    ApprovalStatus.EXPIRED: frozenset(),
    ApprovalStatus.CANCELLED: frozenset(),
    ApprovalStatus.EXECUTED: frozenset(),
}


class ActionType(StrEnum):
    SEND_MESSAGE = "SEND_MESSAGE"
    PLACE_CALL = "PLACE_CALL"
    CONTACT_SUPPLIER = "CONTACT_SUPPLIER"
    PURCHASE = "PURCHASE"
    FINANCIAL_COMMITMENT = "FINANCIAL_COMMITMENT"
    APPROVE_LAYOUT = "APPROVE_LAYOUT"
    APPROVE_BOM = "APPROVE_BOM"


class RiskCategory(StrEnum):
    EXTERNAL_COMMUNICATION = "EXTERNAL_COMMUNICATION"
    CONSEQUENTIAL = "CONSEQUENTIAL"  # purchases, money, binding terms
    INTERNAL = "INTERNAL"


def payload_hash(payload: dict[str, Any]) -> str:
    """Approval binds to this exact payload. Any change -> new approval required."""
    canonical = json.dumps(payload, sort_keys=True, separators=(",", ":"), default=str)
    return hashlib.sha256(canonical.encode()).hexdigest()


class ApprovalRequest(Contract):
    approval_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    requested_by_agent: str
    action_type: ActionType
    action_payload: dict[str, Any]
    action_payload_hash: str = ""
    risk_category: RiskCategory
    approval_status: ApprovalStatus = ApprovalStatus.PENDING
    summary: str = Field(description="What the owner is approving, in plain language.")
    expires_at: UtcDatetime
    authorized_owner_id: UUID
    decision_timestamp: UtcDatetime | None = None
    decided_by: UUID | None = None
    idempotency_key: str
    created_at: UtcDatetime = Field(default_factory=utcnow)

    def model_post_init(self, __context: Any) -> None:
        if not self.action_payload_hash:
            self.action_payload_hash = payload_hash(self.action_payload)

    def transition(self, target: ApprovalStatus) -> ApprovalRequest:
        check_transition("approval", APPROVAL_TRANSITIONS, self.approval_status, target)
        return self.model_copy(update={"approval_status": target})


class ApprovalDecision(Contract):
    decision: ApprovalStatus = Field(description="APPROVED or REJECTED")
    action_payload_hash: str = Field(description="Hash of the payload the owner saw; must match.")
    note: str | None = None
