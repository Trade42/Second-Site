from __future__ import annotations

from enum import StrEnum
from typing import Any
from uuid import UUID

from pydantic import Field

from .common import Contract, UtcDatetime, new_id, utcnow
from .state import check_transition


class Channel(StrEnum):
    PHOTON_IMESSAGE = "PHOTON_IMESSAGE"
    PHOTON_SMS = "PHOTON_SMS"
    EMAIL = "EMAIL"
    VOICE = "VOICE"
    WEB = "WEB"


class CommunicationState(StrEnum):
    DRAFT = "DRAFT"
    AWAITING_APPROVAL = "AWAITING_APPROVAL"
    APPROVED = "APPROVED"
    IN_PROGRESS = "IN_PROGRESS"
    COMPLETED = "COMPLETED"
    FAILED = "FAILED"
    CANCELLED = "CANCELLED"


class StructuredFinding(Contract):
    question: str
    answer: str | None = None
    claim_type: str | None = Field(default=None, description="e.g. 'electrical_service_amps'")
    value: Any = None
    unit: str | None = None
    speaker: str | None = None
    transcript_offset_s: float | None = None
    classification: str = "OWNER_REPORTED"


class CommunicationSession(Contract):
    communication_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID | None = None
    channel: Channel
    external_participants: list[dict[str, str]] = Field(
        default_factory=list, description="[{'role':'broker','name':'...','handle':'+1...'}]"
    )
    authorized_owner_id: UUID
    purpose: str
    state: CommunicationState = CommunicationState.DRAFT
    recording_reference: str | None = None
    transcript_reference: str | None = None
    structured_findings: list[StructuredFinding] = Field(default_factory=list)
    approval_id: UUID | None = None
    provider_message_ids: list[str] = Field(default_factory=list)
    created_at: UtcDatetime = Field(default_factory=utcnow)


class CallState(StrEnum):
    CREATED = "CREATED"
    DIALING = "DIALING"
    INTRO = "INTRO"
    ACTIVE = "ACTIVE"
    HANDOFF_PENDING = "HANDOFF_PENDING"
    OWNER_JOINING = "OWNER_JOINING"
    HUMAN_JOINED = "HUMAN_JOINED"
    AI_RESUMED = "AI_RESUMED"
    COMPLETED = "COMPLETED"
    FAILED = "FAILED"
    DECLINED = "DECLINED"
    CANCELLED = "CANCELLED"


_TERMINAL = frozenset()
CALL_TRANSITIONS: dict[CallState, frozenset[CallState]] = {
    CallState.CREATED: frozenset({CallState.DIALING, CallState.CANCELLED, CallState.FAILED}),
    CallState.DIALING: frozenset(
        {CallState.INTRO, CallState.DECLINED, CallState.FAILED, CallState.CANCELLED}
    ),
    CallState.INTRO: frozenset({CallState.ACTIVE, CallState.FAILED, CallState.COMPLETED}),
    CallState.ACTIVE: frozenset({CallState.HANDOFF_PENDING, CallState.COMPLETED, CallState.FAILED}),
    CallState.HANDOFF_PENDING: frozenset(
        {CallState.OWNER_JOINING, CallState.AI_RESUMED, CallState.COMPLETED, CallState.FAILED}
    ),
    CallState.OWNER_JOINING: frozenset({CallState.HUMAN_JOINED, CallState.AI_RESUMED, CallState.FAILED}),
    CallState.HUMAN_JOINED: frozenset({CallState.AI_RESUMED, CallState.COMPLETED, CallState.FAILED}),
    CallState.AI_RESUMED: frozenset({CallState.HANDOFF_PENDING, CallState.COMPLETED, CallState.FAILED}),
    CallState.COMPLETED: _TERMINAL,
    CallState.FAILED: _TERMINAL,
    CallState.DECLINED: _TERMINAL,
    CallState.CANCELLED: _TERMINAL,
}


class AuthorityScope(Contract):
    """What the assistant may discuss without the owner. Everything else escalates."""

    may_ask_about: list[str] = Field(
        default_factory=lambda: ["availability", "infrastructure", "permitted_use", "showing_times"]
    )
    may_not: list[str] = Field(
        default_factory=lambda: [
            "agree_to_terms",
            "discuss_deposits_or_payment",
            "offer_guarantor",
            "share_sensitive_personal_info",
        ]
    )


class ParticipantState(Contract):
    role: str = Field(description="'assistant' | 'counterparty' | 'owner'")
    leg_id: str | None = None
    state: str = Field(description="'dialing' | 'joined' | 'left' | 'failed'")
    updated_at: UtcDatetime = Field(default_factory=utcnow)


class CallSession(Contract):
    call_id: UUID = Field(default_factory=new_id)
    project_id: UUID
    property_id: UUID | None = None
    provider_call_id: str | None = None
    conference_id: str | None = None
    owner_id: UUID
    recipient: dict[str, str] = Field(description="{'role':'broker','name':'...','phone':'+1...'}")
    intro_recording_reference: str | None = Field(
        default=None, description="Owner's genuine recorded intro (object storage key). Never synthesized."
    )
    automation_disclosure: str = Field(
        default="This call is being handled by an automated assistant on the owner's behalf.",
        description="Disclosure played/stated per applicable law and provider rules.",
    )
    authority_scope: AuthorityScope = Field(default_factory=AuthorityScope)
    state: CallState = CallState.CREATED
    approval_id: UUID
    handoff_request_id: UUID | None = None
    participant_states: list[ParticipantState] = Field(default_factory=list)
    started_at: UtcDatetime | None = None
    ended_at: UtcDatetime | None = None
    failure_reason: str | None = None

    def transition(self, target: CallState) -> CallSession:
        check_transition("call", CALL_TRANSITIONS, self.state, target)
        return self.model_copy(update={"state": target})


class HandoffReason(StrEnum):
    BINDING_COMMITMENT = "BINDING_COMMITMENT"
    FINANCIAL_DETAILS = "FINANCIAL_DETAILS"
    GUARANTOR_REQUESTED = "GUARANTOR_REQUESTED"
    SENSITIVE_PERSONAL_INFO = "SENSITIVE_PERSONAL_INFO"
    COUNTERPARTY_REQUESTED_OWNER = "COUNTERPARTY_REQUESTED_OWNER"
    INSUFFICIENT_EVIDENCE = "INSUFFICIENT_EVIDENCE"
    EXCEEDS_AUTHORITY = "EXCEEDS_AUTHORITY"


class HandoffStatus(StrEnum):
    PENDING = "PENDING"
    NOTIFIED = "NOTIFIED"
    ACCEPTED = "ACCEPTED"
    DECLINED = "DECLINED"
    DEFERRED = "DEFERRED"
    EXPIRED = "EXPIRED"


HANDOFF_TRANSITIONS: dict[HandoffStatus, frozenset[HandoffStatus]] = {
    HandoffStatus.PENDING: frozenset({HandoffStatus.NOTIFIED, HandoffStatus.EXPIRED}),
    HandoffStatus.NOTIFIED: frozenset(
        {HandoffStatus.ACCEPTED, HandoffStatus.DECLINED, HandoffStatus.DEFERRED, HandoffStatus.EXPIRED}
    ),
    HandoffStatus.ACCEPTED: frozenset(),
    HandoffStatus.DECLINED: frozenset(),
    HandoffStatus.DEFERRED: frozenset(),
    HandoffStatus.EXPIRED: frozenset(),
}


class OwnerHandoffRequest(Contract):
    handoff_request_id: UUID = Field(default_factory=new_id)
    call_id: UUID
    project_id: UUID
    reason: HandoffReason
    question: str
    urgency: str = Field(default="HIGH", description="'HIGH' | 'NORMAL'")
    owner_id: UUID
    join_token_reference: str | None = Field(
        default=None,
        description="Opaque reference to a short-lived join token held server-side. Never the raw token.",
    )
    expires_at: UtcDatetime
    status: HandoffStatus = HandoffStatus.PENDING

    def transition(self, target: HandoffStatus) -> OwnerHandoffRequest:
        check_transition("handoff", HANDOFF_TRANSITIONS, self.status, target)
        return self.model_copy(update={"status": target})


class OutreachRequest(Contract):
    property_id: UUID
    channel: Channel
    recipient: dict[str, str]
    purpose: str
    questions: list[str] = Field(default_factory=list)
    intro_recording_reference: str | None = None
