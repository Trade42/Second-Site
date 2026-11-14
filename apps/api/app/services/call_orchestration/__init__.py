"""Call orchestration: approval-gated outbound broker calls, carrier-event state machine, owner handoff.

Flow (Telnyx Call Control; the call simulator emits the same event shapes in fixture mode):
  start()            APPROVED PLACE_CALL -> dial (client_state carries call_id + role)       -> DIALING
  call.answered      play the owner's genuine recorded intro, else speak the intro + disclosure -> INTRO
  machine detected   short voicemail message, hang up                                        -> COMPLETED
  speak/playback end start transcription, ask the first grounded question                    -> ACTIVE
  call.transcription escalation phrase -> OwnerHandoffRequest + Photon text to the owner       -> HANDOFF_PENDING
                     otherwise -> StructuredFinding -> SourceEvidence(OWNER_REPORTED) -> evidence_updated
  owner JOIN         dial the owner, conference the broker leg                                -> OWNER_JOINING
  owner leg joined                                                                           -> HUMAN_JOINED
  owner LATER/none   assistant defers the sensitive question and continues                    -> AI_RESUMED
  call.hangup        finalize: findings on the CommunicationSession, outreach.completed, owner summary
The assistant always identifies itself as the owner's automated assistant and never agrees to terms, payments or
commitments (AuthorityScope). Owner-owned: communications.
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
    CallSession,
    CallState,
    CommunicationState,
    DataOrigin,
    EventType,
    EvidenceClassification,
    HandoffReason,
    HandoffStatus,
    OwnerHandoffRequest,
    ParticipantState,
    SourceEvidence,
    TranscriptTurn,
)
from app.core.container import Container
from app.core.errors import ConflictError
from app.core.security import issue_token, opaque_reference, sign_media_key
from app.db import models as m
from app.domains.communications import call_script
from app.domains.communications.escalation import detect_escalation
from app.integrations.base import ProviderError
from app.integrations.telnyx import decode_client_state
from app.repositories import registry as repos

HANDOFF_TTL = timedelta(minutes=3)
_JOIN_TOKENS: dict[str, str] = {}  # reference -> token (server-side only; never in events)
TERMINAL = {CallState.COMPLETED, CallState.FAILED, CallState.DECLINED, CallState.CANCELLED}
NO_ANSWER_CAUSES = {
    "timeout",
    "no_answer",
    "user_busy",
    "call_rejected",
    "originator_cancel",
    "unallocated_number",
}


def call_payload(project_id: UUID, recipient: dict, property_id: UUID | None) -> dict:
    """Canonical payload the owner approves for PLACE_CALL."""
    return {
        "project_id": str(project_id),
        "property_id": str(property_id) if property_id else None,
        "recipient": recipient,
    }


def _now() -> datetime:
    return datetime.now(UTC)


class CallOrchestrator:
    def __init__(self, c: Container):
        self.c = c

    # ---- persistence / events -------------------------------------------------------------
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
            idempotency_key=key or f"call:{call.call_id}:{call.state.value}:{len(call.transcript)}",
            payload={"call_id": str(call.call_id), "state": call.state.value, "detail": detail},
        )

    def _say(self, db: Session, call: CallSession, text: str) -> CallSession:
        """Speak on the broker leg and keep the transcript."""
        if call.provider_call_id:
            try:
                self.c.telephony.speak(call.provider_call_id, text)
            except ProviderError as exc:
                text = f"{text} [speak failed: {exc.kind}]"
        call = call.model_copy(
            update={"transcript": [*call.transcript, TranscriptTurn(speaker="assistant", text=text)]}
        )
        return self._save(db, call)

    def _notify_owner(self, db: Session, call: CallSession, text: str, purpose: str, key: str) -> None:
        from app.domains.communications.owner_channel import notify_owner

        notify_owner(
            db,
            self.c,
            owner_id=call.owner_id,
            project_id=call.project_id,
            text=text,
            purpose=purpose,
            idempotency_key=key,
        )

    def _owner_name(self, db: Session, owner_id: UUID) -> str:
        user = db.get(m.User, owner_id)
        return (user.display_name if user else None) or "the business owner"

    # ---- start ------------------------------------------------------------------------------
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
        communication_id: UUID | None = None,
        simulated: bool = False,
    ) -> CallSession:
        payload = call_payload(project_id, recipient, property_id)
        appr = self.c.approvals.require_approved(db, approval_id, ActionType.PLACE_CALL, payload)
        script = call_script.build_script(
            repos.assessments.list(db, property_id=property_id) if property_id else []
        )
        call = CallSession(
            project_id=project_id,
            property_id=property_id,
            owner_id=owner_id,
            recipient=recipient,
            intro_recording_reference=intro_recording_reference,
            approval_id=appr.approval_id,
            communication_id=communication_id,
            script=script,
            simulated=simulated,
        )
        repos.calls.add(db, call)
        call = call.transition(CallState.DIALING)
        try:
            res = self.c.telephony.dial(
                recipient["phone"],
                self.c.settings.telnyx_from_number or "+10000000000",
                f"{self.c.settings.public_base_url}/webhooks/telnyx",
                f"call:{call.call_id}",
                {"call_id": str(call.call_id), "role": "counterparty"},
            )
        except ProviderError as err:
            call = call.transition(CallState.FAILED).model_copy(
                update={"failure_reason": err.message, "ended_at": _now()}
            )
            self._save(db, call)
            self._emit(db, call, EventType.CALL_FAILED, err.message)
            return call
        call = call.model_copy(
            update={
                "provider_call_id": res.call_control_id,
                "started_at": _now(),
                "participant_states": [
                    ParticipantState(role="counterparty", leg_id=res.call_control_id, state="dialing")
                ],
            }
        )
        self._save(db, call)
        self.c.approvals.mark_executed(db, appr)
        self._set_comm_state(db, call, CommunicationState.IN_PROGRESS)
        self._emit(db, call, EventType.CALL_STARTED, "dial initiated (not yet answered)")
        return call

    def advance(
        self, db: Session, call_id: UUID, target: CallState, detail: str | None = None
    ) -> CallSession:
        call = repos.calls.require(db, call_id).transition(target)
        if target in TERMINAL:
            call = call.model_copy(
                update={"ended_at": _now(), "failure_reason": detail if target == CallState.FAILED else None}
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

    # ---- carrier events ------------------------------------------------------------------------
    def find_call(self, db: Session, payload: dict[str, Any]) -> tuple[CallSession | None, str]:
        state = decode_client_state(payload.get("client_state"))
        if state.get("call_id"):
            try:
                call = repos.calls.get(db, UUID(state["call_id"]))
            except ValueError:
                call = None
            if call:
                return call, state.get("role", "counterparty")
        leg = payload.get("call_control_id")
        if leg:
            row = db.scalar(select(m.CallSessionRow).where(m.CallSessionRow.provider_call_id == leg))
            if row:
                return CallSession.model_validate(row.data), "counterparty"
        return None, "unknown"

    def handle_event(self, db: Session, event: dict[str, Any]) -> CallSession | None:
        data = event.get("data") or event
        et, p = data.get("event_type"), data.get("payload") or {}
        call, role = self.find_call(db, p)
        if call is None or call.state in TERMINAL:
            return call
        if role == "owner":
            return self._owner_leg_event(db, call, et, p)
        if et == "call.answered" and call.state == CallState.DIALING:
            return self._on_answered(db, call)
        if et == "call.machine.detection.ended" and p.get("result") == "machine":
            call = self._say(
                db,
                call,
                f"Hi, this is an automated assistant calling for {self._owner_name(db, call.owner_id)} about your "
                "commercial listing. We'll follow up by email. Thank you.",
            )
            if call.provider_call_id:
                self.c.telephony.hangup(call.provider_call_id)
            return call
        if et in ("call.speak.ended", "call.playback.ended") and call.state == CallState.INTRO:
            return self._begin_questions(db, call)
        if et == "call.transcription":
            td = p.get("transcription_data") or {}
            text = (td.get("transcript") or "").strip()
            if td.get("is_final", True) and text:
                return self.on_utterance(db, call, text)
            return call
        if et == "call.hangup":
            return self._on_hangup(db, call, p.get("hangup_cause"))
        return call

    def _on_answered(self, db: Session, call: CallSession) -> CallSession:
        call = call.transition(CallState.INTRO)
        call = call.model_copy(
            update={
                "participant_states": [
                    ParticipantState(role="counterparty", leg_id=call.provider_call_id, state="joined")
                ]
            }
        )
        self._save(db, call)
        owner = self._owner_name(db, call.owner_id)
        if call.intro_recording_reference and call.provider_call_id:
            token = sign_media_key(
                self.c.settings.auth_secret.get_secret_value(), call.intro_recording_reference
            )
            try:
                self.c.telephony.play(
                    call.provider_call_id, f"{self.c.settings.public_base_url}/api/v1/media/public/{token}"
                )
                call = call.model_copy(
                    update={
                        "transcript": [
                            *call.transcript,
                            TranscriptTurn(
                                speaker="system", text=f"[played {owner}'s recorded introduction]"
                            ),
                        ]
                    }
                )
                self._save(db, call)
            except ProviderError:
                pass
            return self._say(db, call, call.automation_disclosure)
        return self._say(
            db,
            call,
            f"Hi {call.recipient.get('name', 'there')}, I'm an automated assistant calling on behalf of {owner}, "
            f"who is looking at your listing for a second location. {call.automation_disclosure} "
            "I just have a few quick questions about the space. Is now an okay time?",
        )

    def _begin_questions(self, db: Session, call: CallSession) -> CallSession:
        call = call.transition(CallState.ACTIVE)
        self._save(db, call)
        if call.provider_call_id:
            try:
                self.c.telephony.start_transcription(call.provider_call_id)
            except ProviderError:
                pass
        return self._ask_next(db, call)

    def _ask_next(self, db: Session, call: CallSession) -> CallSession:
        if call.question_index < len(call.script):
            return self._say(db, call, call.script[call.question_index].question)
        call = self._say(
            db,
            call,
            "That's everything I needed. Thank you so much. "
            + ("The owner will follow up directly on the other items. " if call.deferred_questions else "")
            + "Have a great day.",
        )
        if call.provider_call_id:
            self.c.telephony.hangup(call.provider_call_id)
        return call

    def on_utterance(self, db: Session, call: CallSession, text: str) -> CallSession:
        call = call.model_copy(
            update={"transcript": [*call.transcript, TranscriptTurn(speaker="counterparty", text=text)]}
        )
        self._save(db, call)
        if call.state in (CallState.HUMAN_JOINED, CallState.OWNER_JOINING, CallState.HANDOFF_PENDING):
            return call  # the owner is (being) connected; the assistant stays quiet
        if call.state == CallState.INTRO:  # e.g. "sure, go ahead" before the intro finished
            return self._begin_questions(db, call)
        reason = detect_escalation(text)
        if reason is not None:
            call = self._say(
                db,
                call,
                "That's a decision for the owner, and I can't agree to anything on their behalf. "
                "Let me check if they can join right now.",
            )
            self.request_handoff(db, call.call_id, reason, text)
            return repos.calls.require(db, call.call_id)
        if call.question_index < len(call.script):
            q = call.script[call.question_index]
            finding = call_script.extract(q, text)
            call = call.model_copy(
                update={"findings": [*call.findings, finding], "question_index": call.question_index + 1}
            )
            self._save(db, call)
            self._record_finding(db, call, q.requirement_id, finding)
        return self._ask_next(db, call)

    def _record_finding(self, db: Session, call: CallSession, requirement_id: str | None, finding) -> None:
        evidence_id = None
        if call.property_id and finding.value is not None and finding.claim_type != "available_from":
            who = call.recipient.get("name") or "the listing contact"
            limits = [
                f"Stated by {who} ({call.recipient.get('role', 'broker')}) on a call on {_now():%Y-%m-%d}; "
                "not independently verified."
            ]
            if call.simulated:
                limits.insert(0, "SIMULATED CALL: scripted demo answer, not a real conversation.")
            ev = SourceEvidence(
                project_id=call.project_id,
                property_id=call.property_id,
                provider="broker_call",
                source_record_id=str(call.call_id),
                data_origin=DataOrigin.FIXTURE_SYNTHETIC if call.simulated else DataOrigin.LIVE,
                observation_type=finding.claim_type,
                extracted_value=finding.value,
                unit=finding.unit,
                evidence_classification=EvidenceClassification.OWNER_REPORTED,
                limitations=limits,
            )
            self.c.evidence.add(db, ev)
            evidence_id = ev.evidence_id
            self.c.events.emit(
                db,
                event_type=EventType.INFRASTRUCTURE_EVIDENCE_UPDATED,
                project_id=call.project_id,
                actor_type=ActorType.AGENT,
                actor_id="voice-handoff",
                idempotency_key=f"call-evidence:{ev.evidence_id}",
                payload={
                    "property_id": str(call.property_id),
                    "evidence_ids": [str(ev.evidence_id)],
                    "reason": f"broker answer on call ({finding.claim_type})",
                },
            )
        self.c.events.emit(
            db,
            event_type=EventType.CALL_FINDING_RECORDED,
            project_id=call.project_id,
            actor_type=ActorType.AGENT,
            actor_id="voice-handoff",
            idempotency_key=f"finding:{call.call_id}:{call.question_index}",
            payload={
                "call_id": str(call.call_id),
                "claim_type": finding.claim_type,
                "evidence_id": str(evidence_id) if evidence_id else None,
                "summary": call_script.describe(finding),
            },
        )

    def _on_hangup(self, db: Session, call: CallSession, cause: str | None) -> CallSession:
        if call.state == CallState.DIALING:
            target = CallState.DECLINED if cause == "call_rejected" else CallState.FAILED
            call = self.advance(db, call.call_id, target, f"not answered ({cause or 'unknown'})")
            self._set_comm_state(db, call, CommunicationState.FAILED)
            return call
        if call.state == CallState.INTRO:
            call = self.advance(
                db, call.call_id, CallState.COMPLETED, f"ended during intro ({cause or 'hangup'})"
            )
        elif call.state in (CallState.HANDOFF_PENDING, CallState.OWNER_JOINING):
            call = self.advance(db, call.call_id, CallState.FAILED, "counterparty hung up during handoff")
        else:
            call = self.advance(db, call.call_id, CallState.COMPLETED, cause or "normal_clearing")
        return self._finalize(db, call)

    def _finalize(self, db: Session, call: CallSession) -> CallSession:
        known = [f for f in call.findings if f.value is not None]
        summary = (
            f"Call with {call.recipient.get('name', 'the broker')} ended. "
            + (
                ("Learned: " + "; ".join(call_script.describe(f) for f in known) + ". ")
                if known
                else "No clear answers. "
            )
            + (f"Deferred to you: {'; '.join(call.deferred_questions)}." if call.deferred_questions else "")
        ).strip()
        call = call.model_copy(update={"outcome_summary": summary})
        self._save(db, call)
        if call.communication_id:
            comm = repos.communications.get(db, call.communication_id)
            if comm:
                repos.communications.upsert(
                    db,
                    comm.model_copy(
                        update={
                            "state": CommunicationState.COMPLETED,
                            "structured_findings": call.findings,
                            "transcript_reference": f"call_sessions/{call.call_id}/transcript",
                        }
                    ),
                )
                self.c.events.emit(
                    db,
                    event_type=EventType.OUTREACH_COMPLETED,
                    project_id=call.project_id,
                    actor_type=ActorType.AGENT,
                    actor_id="voice-handoff",
                    idempotency_key=f"outreach:{comm.communication_id}:completed",
                    payload={
                        "communication_id": str(comm.communication_id),
                        "property_id": str(call.property_id) if call.property_id else None,
                    },
                )
        self._notify_owner(
            db,
            call,
            ("[Simulated call] " if call.simulated else "") + summary,
            "call_summary",
            f"call-summary:{call.call_id}",
        )
        return call

    def _set_comm_state(self, db: Session, call: CallSession, state: CommunicationState) -> None:
        if call.communication_id:
            comm = repos.communications.get(db, call.communication_id)
            if comm and comm.state != state:
                repos.communications.upsert(db, comm.model_copy(update={"state": state}))

    # ---- owner handoff --------------------------------------------------------------------------
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
            expires_at=_now() + HANDOFF_TTL,
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
        # The text carries no token: JOIN only makes us call the owner's own registered number.
        self._notify_owner(
            db,
            call,
            f'Your assistant needs you on a call with {call.recipient.get("name", "the broker")}: "{question}" '
            f"Reply JOIN and I'll call you to connect, or LATER and I'll say you'll follow up. "
            f"Or open {self.c.settings.public_web_url}/activity",
            "handoff_request",
            f"handoff-notify:{handoff.handoff_request_id}",
        )
        handoff = handoff.transition(HandoffStatus.NOTIFIED)
        repos.handoffs.upsert(db, handoff)
        return handoff

    def _owner_phone(self, db: Session, owner_id: UUID) -> str | None:
        from app.domains.communications.owner_channel import channel_for_owner

        ch = channel_for_owner(db, owner_id)
        if ch and ch.handle.startswith("+"):
            return ch.handle
        user = db.get(m.User, owner_id)
        return user.phone if user and user.phone else None

    def owner_response(
        self, db: Session, handoff_id: UUID, user_id: UUID, response: HandoffStatus
    ) -> CallSession:
        h = repos.handoffs.require(db, handoff_id)
        if h.owner_id != user_id:
            raise ConflictError("not the call owner", error_code="FORBIDDEN")
        if h.expires_at < _now() and h.status == HandoffStatus.NOTIFIED:
            response = HandoffStatus.EXPIRED
        h = h.transition(response)
        repos.handoffs.upsert(db, h)
        if h.join_token_reference and response != HandoffStatus.ACCEPTED:
            _JOIN_TOKENS.pop(h.join_token_reference, None)
        call = repos.calls.require(db, h.call_id)
        if response == HandoffStatus.ACCEPTED:
            phone = self._owner_phone(db, call.owner_id)
            if phone is None:
                return self._resume(db, call, h.question, "owner accepted but has no phone number on file")
            try:
                if call.conference_id is None and call.provider_call_id:
                    conf = self.c.telephony.create_conference(call.provider_call_id, f"call-{call.call_id}")
                    call = self._save(db, call.model_copy(update={"conference_id": conf}))
                leg = self.c.telephony.dial(
                    phone,
                    self.c.settings.telnyx_from_number or "+10000000000",
                    f"{self.c.settings.public_base_url}/webhooks/telnyx",
                    f"owner-leg:{h.handoff_request_id}",
                    {"call_id": str(call.call_id), "role": "owner"},
                )
            except ProviderError as exc:
                return self._resume(db, call, h.question, f"could not reach the owner ({exc.kind})")
            call = self._save(
                db,
                call.model_copy(
                    update={
                        "participant_states": [
                            *call.participant_states,
                            ParticipantState(role="owner", leg_id=leg.call_control_id, state="dialing"),
                        ]
                    }
                ),
            )
            call = self._say(db, call, "Great news, the owner is joining the call now. One moment please.")
            return self.advance(db, call.call_id, CallState.OWNER_JOINING, "calling the owner")
        return self._resume(db, call, h.question, f"owner {response.value.lower()}")

    def _resume(self, db: Session, call: CallSession, question: str, why: str) -> CallSession:
        call = self._save(
            db, call.model_copy(update={"deferred_questions": [*call.deferred_questions, question]})
        )
        call = self.advance(db, call.call_id, CallState.AI_RESUMED, f"{why}; assistant defers: {question}")
        call = self._say(
            db,
            call,
            "The owner can't join right now, so I'll make sure they follow up with you directly on that. "
            "I can't agree to anything on their behalf.",
        )
        return self._ask_next(db, call)

    def _owner_leg_event(self, db: Session, call: CallSession, et: str | None, p: dict) -> CallSession:
        leg = p.get("call_control_id")
        if et == "call.answered" and call.state == CallState.OWNER_JOINING and call.conference_id and leg:
            try:
                self.c.telephony.join_conference(call.conference_id, leg)
            except ProviderError as exc:
                return self._resume(
                    db, call, call.deferred_questions[-1] if call.deferred_questions else "", str(exc)
                )
            return call
        if et in ("conference.participant.joined", "call.answered") and call.state == CallState.OWNER_JOINING:
            states = [
                s.model_copy(update={"state": "joined", "updated_at": _now()}) if s.role == "owner" else s
                for s in call.participant_states
            ]
            call = self._save(
                db,
                call.model_copy(
                    update={
                        "participant_states": states,
                        "transcript": [
                            *call.transcript,
                            TranscriptTurn(speaker="system", text="[owner joined the call]"),
                        ],
                    }
                ),
            )
            return self.advance(db, call.call_id, CallState.HUMAN_JOINED, "owner joined")
        if et == "call.hangup" and call.state == CallState.OWNER_JOINING:
            h = repos.handoffs.require(db, call.handoff_request_id) if call.handoff_request_id else None
            return self._resume(db, call, h.question if h else "", "owner did not answer")
        if et == "call.hangup" and call.state == CallState.HUMAN_JOINED:
            call = self._save(
                db,
                call.model_copy(
                    update={
                        "transcript": [
                            *call.transcript,
                            TranscriptTurn(speaker="system", text="[owner left the call]"),
                        ]
                    }
                ),
            )
            call = self.advance(db, call.call_id, CallState.AI_RESUMED, "owner left; assistant resumes")
            return self._ask_next(db, call)
        return call

    def expire_stale_handoffs(self, db: Session, call_id: UUID) -> CallSession:
        call = repos.calls.require(db, call_id)
        if call.handoff_request_id is None or call.state != CallState.HANDOFF_PENDING:
            return call
        h = repos.handoffs.require(db, call.handoff_request_id)
        if h.expires_at < _now() and h.status in (HandoffStatus.PENDING, HandoffStatus.NOTIFIED):
            return self.owner_response(db, h.handoff_request_id, h.owner_id, HandoffStatus.EXPIRED)
        return call

    def pending_handoff_for_owner(self, db: Session, owner_id: UUID) -> OwnerHandoffRequest | None:
        rows = db.scalars(
            select(m.OwnerHandoffRequestRow).where(
                m.OwnerHandoffRequestRow.status == HandoffStatus.NOTIFIED.value
            )
        ).all()
        pending = [OwnerHandoffRequest.model_validate(r.data) for r in rows]
        pending = [h for h in pending if h.owner_id == owner_id]
        return max(pending, key=lambda h: h.expires_at) if pending else None
