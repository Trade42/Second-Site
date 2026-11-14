"""Call orchestration: approval-gated outbound calls, guarded state machine, owner handoff.

Topology (see docs/AGENT_ARCHITECTURE.md): Telnyx dials the counterparty and owns a conference;
the assistant leg is xAI realtime voice over SIP; on escalation the owner is notified (Photon) with a
short-lived join token and joins the conference (HUMAN_JOINED). No owner response -> the assistant
defers the sensitive question (AI_RESUMED). Owner-owned: communications.
"""

from __future__ import annotations

from datetime import UTC, datetime, timedelta
from uuid import UUID

from sqlalchemy.orm import Session

from app.contracts import (
    ActionType,
    ActorType,
    CallSession,
    CallState,
    EventType,
    HandoffReason,
    HandoffStatus,
    OwnerHandoffRequest,
    ParticipantState,
)
from app.core.container import Container
from app.core.errors import ConflictError
from app.core.security import issue_token, opaque_reference
from app.integrations.base import ProviderError
from app.repositories import registry as repos

HANDOFF_TTL = timedelta(minutes=3)
_JOIN_TOKENS: dict[str, str] = {}  # reference -> token (server-side only; never in events)


def call_payload(project_id: UUID, recipient: dict, property_id: UUID | None) -> dict:
    """Canonical payload the owner approves for PLACE_CALL."""
    return {
        "project_id": str(project_id),
        "property_id": str(property_id) if property_id else None,
        "recipient": recipient,
    }


class CallOrchestrator:
    def __init__(self, c: Container):
        self.c = c

    def _save(self, db: Session, call: CallSession) -> CallSession:
        repos.calls.upsert(db, call)
        return call

    def _emit(
        self, db: Session, call: CallSession, et: EventType, detail: str | None = None, key: str | None = None
    ):
        self.c.events.emit(
            db,
            event_type=et,
            project_id=call.project_id,
            actor_type=ActorType.AGENT,
            actor_id="voice-handoff",
            idempotency_key=key or f"call:{call.call_id}:{call.state.value}",
            payload={"call_id": str(call.call_id), "state": call.state.value, "detail": detail},
        )

    def start(
        self,
        db: Session,
        *,
        project_id: UUID,
        owner_id: UUID,
        recipient: dict,
        property_id: UUID | None,
        approval_id: UUID | None,
        intro_recording_reference: str | None,
    ) -> CallSession:
        payload = call_payload(project_id, recipient, property_id)
        appr = self.c.approvals.require_approved(db, approval_id, ActionType.PLACE_CALL, payload)
        call = CallSession(
            project_id=project_id,
            property_id=property_id,
            owner_id=owner_id,
            recipient=recipient,
            intro_recording_reference=intro_recording_reference,
            approval_id=appr.approval_id,
        )
        repos.calls.add(db, call)
        call = call.transition(CallState.DIALING)
        try:
            res = self.c.telephony.dial(
                recipient["phone"],
                self.c.settings.telnyx_from_number or "+10000000000",
                f"{self.c.settings.public_base_url}/webhooks/telnyx",
                f"call:{call.call_id}",
            )
        except ProviderError as err:
            call = call.transition(CallState.FAILED).model_copy(
                update={"failure_reason": err.message, "ended_at": datetime.now(UTC)}
            )
            self._save(db, call)
            self._emit(db, call, EventType.CALL_FAILED, err.message)
            return call
        call = call.model_copy(
            update={
                "provider_call_id": res.call_control_id,
                "started_at": datetime.now(UTC),
                "participant_states": [
                    ParticipantState(role="counterparty", leg_id=res.call_control_id, state="dialing")
                ],
            }
        )
        self._save(db, call)
        self.c.approvals.mark_executed(db, appr)
        self._emit(db, call, EventType.CALL_STARTED, "dial initiated (not yet answered)")
        return call

    def advance(
        self, db: Session, call_id: UUID, target: CallState, detail: str | None = None
    ) -> CallSession:
        call = repos.calls.require(db, call_id).transition(target)
        if target in (CallState.COMPLETED, CallState.FAILED, CallState.DECLINED, CallState.CANCELLED):
            call = call.model_copy(
                update={
                    "ended_at": datetime.now(UTC),
                    "failure_reason": detail if target == CallState.FAILED else None,
                }
            )
        self._save(db, call)
        et = {
            CallState.COMPLETED: EventType.CALL_COMPLETED,
            CallState.FAILED: EventType.CALL_FAILED,
            CallState.HUMAN_JOINED: EventType.CALL_OWNER_JOINED,
        }.get(target)
        if et:
            self._emit(db, call, et, detail)
        return call

    def request_handoff(
        self, db: Session, call_id: UUID, reason: HandoffReason, question: str
    ) -> OwnerHandoffRequest:
        call = repos.calls.require(db, call_id)
        if call.state not in (CallState.ACTIVE, CallState.AI_RESUMED):
            raise ConflictError(
                f"cannot hand off from {call.state.value}", error_code="INVALID_STATE_TRANSITION"
            )
        ref = opaque_reference()
        _JOIN_TOKENS[ref] = issue_token(
            self.c.settings.auth_secret.get_secret_value(),
            call.owner_id,
            ttl_s=int(HANDOFF_TTL.total_seconds()),
            purpose=f"join:{call.call_id}",
        )
        handoff = OwnerHandoffRequest(
            call_id=call.call_id,
            project_id=call.project_id,
            reason=reason,
            question=question,
            owner_id=call.owner_id,
            join_token_reference=ref,
            expires_at=datetime.now(UTC) + HANDOFF_TTL,
        )
        repos.handoffs.add(db, handoff)
        call = call.transition(CallState.HANDOFF_PENDING).model_copy(
            update={"handoff_request_id": handoff.handoff_request_id}
        )
        self._save(db, call)
        self.c.events.emit(
            db,
            event_type=EventType.CALL_OWNER_HANDOFF_REQUESTED,
            project_id=call.project_id,
            actor_type=ActorType.AGENT,
            actor_id="voice-handoff",
            idempotency_key=f"handoff:{handoff.handoff_request_id}",
            payload={
                "call_id": str(call.call_id),
                "handoff_request_id": str(handoff.handoff_request_id),
                "reason": reason.value,
            },
        )
        # Notify the owner (fixture outbox in dev). Notification text carries no token, only an app link.
        self.c.photon.send_text(
            space_id=f"owner:{call.owner_id}",
            sender_id="agent",
            text=f"Your assistant needs you on a call: {question} Open Second Site to join.",
            idempotency_key=f"handoff-notify:{handoff.handoff_request_id}",
        )
        handoff = handoff.transition(HandoffStatus.NOTIFIED)
        repos.handoffs.upsert(db, handoff)
        return handoff

    def owner_response(
        self, db: Session, handoff_id: UUID, user_id: UUID, response: HandoffStatus
    ) -> CallSession:
        h = repos.handoffs.require(db, handoff_id)
        if h.owner_id != user_id:
            raise ConflictError("not the call owner", error_code="FORBIDDEN")
        if h.expires_at < datetime.now(UTC) and h.status == HandoffStatus.NOTIFIED:
            response = HandoffStatus.EXPIRED
        h = h.transition(response)
        repos.handoffs.upsert(db, h)
        if h.join_token_reference:
            _JOIN_TOKENS.pop(h.join_token_reference, None) if response != HandoffStatus.ACCEPTED else None
        call = repos.calls.require(db, h.call_id)
        if response == HandoffStatus.ACCEPTED:
            if call.conference_id is None and call.provider_call_id:
                conf = self.c.telephony.create_conference(call.provider_call_id, f"call-{call.call_id}")
                call = call.model_copy(update={"conference_id": conf})
                self._save(db, call)
            self.advance(db, call.call_id, CallState.OWNER_JOINING)
            return self.advance(db, call.call_id, CallState.HUMAN_JOINED, "owner joined")
        # Declined / deferred / expired: assistant resumes and defers the sensitive question.
        return self.advance(
            db,
            call.call_id,
            CallState.AI_RESUMED,
            f"owner {response.value.lower()}; assistant defers: {h.question}",
        )

    def expire_stale_handoffs(self, db: Session, call_id: UUID) -> CallSession:
        call = repos.calls.require(db, call_id)
        if call.handoff_request_id is None or call.state != CallState.HANDOFF_PENDING:
            return call
        h = repos.handoffs.require(db, call.handoff_request_id)
        if h.expires_at < datetime.now(UTC) and h.status in (HandoffStatus.PENDING, HandoffStatus.NOTIFIED):
            return self.owner_response(db, h.handoff_request_id, h.owner_id, HandoffStatus.EXPIRED)
        return call
