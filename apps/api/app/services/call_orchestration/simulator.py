"""Call simulator: plays the carrier's role in fixture mode by feeding Telnyx-shaped webhook events into the same
`CallOrchestrator.handle_event` that real webhooks use. Every simulated call is labeled `simulated=True`, its
evidence is FIXTURE_SYNTHETIC with a "SIMULATED CALL" limitation, and the owner summary says so.
Refuses to run when the voice provider is live.
"""

from __future__ import annotations

from typing import Any
from uuid import UUID, uuid4

from sqlalchemy.orm import Session

from app.contracts import CallSession, CallState, HandoffStatus
from app.core.container import Container
from app.core.errors import ConflictError
from app.integrations.telnyx import encode_client_state
from app.repositories import registry as repos

from . import CallOrchestrator

DEFAULT_ANSWERS = {
    "tenant_subpanel_amps": "The suite has its own 200 amp panel, I'd guess about a third of it is in use.",
    "in_unit_plumbing": "Yes, there's a sink in the back room and a restroom inside the suite.",
    "hvac_zonable": "Yes, the landlord's mechanical contractor said it can be split into zones.",
    "usable_area_sqft": "It's about 1,250 usable square feet.",
    "available_from": "It's available December first.",
}
ESCALATION_LINE = "Before we go further, the landlord will want first month and a deposit by Friday. Can you commit to that?"


def _event(event_type: str, payload: dict[str, Any]) -> dict[str, Any]:
    return {
        "data": {"record_type": "event", "event_type": event_type, "id": str(uuid4()), "payload": payload}
    }


class CallSimulator:
    def __init__(self, c: Container):
        if getattr(c.telephony, "mode", "fixture") == "live":
            raise ConflictError(
                "the simulator only runs with the fixture voice provider", error_code="SIMULATOR_DISABLED"
            )
        self.c = c
        self.orch = CallOrchestrator(c)

    def _leg_events(self, db: Session, call: CallSession, events: list[tuple[str, dict]]) -> CallSession:
        for et, extra in events:
            payload = {"call_control_id": call.provider_call_id, **extra}
            if "client_state" not in extra:
                payload["client_state"] = encode_client_state(
                    {"call_id": str(call.call_id), "role": "counterparty"}
                )
            self.orch.handle_event(db, _event(et, payload))
            call = repos.calls.require(db, call.call_id)
        return call

    def run(
        self,
        db: Session,
        call_id: UUID,
        owner_action: str = "accept",
        answers: dict[str, str] | None = None,
        escalate_after: int = 1,
    ) -> CallSession:
        """owner_action: 'accept' (owner joins), 'later' (owner defers) or 'none' (no response -> expires)."""
        call = repos.calls.require(db, call_id)
        if not call.simulated:
            raise ConflictError("this call was not created as a simulated call", error_code="NOT_SIMULATED")
        answers = {**DEFAULT_ANSWERS, **(answers or {})}
        call = self._leg_events(
            db, call, [("call.answered", {}), ("call.speak.ended", {"status": "completed"})]
        )
        asked = 0
        while call.state in (CallState.ACTIVE, CallState.AI_RESUMED) and call.question_index < len(
            call.script
        ):
            if asked == escalate_after and not call.handoff_request_id:
                call = self._say_broker(db, call, ESCALATION_LINE)
                call = self._owner_turn(db, call, owner_action)
                continue
            q = call.script[call.question_index]
            call = self._say_broker(db, call, answers.get(q.claim_type, "I'm not sure, I'd have to check."))
            asked += 1
        return self._leg_events(
            db, call, [("call.hangup", {"hangup_cause": "normal_clearing", "hangup_source": "callee"})]
        )

    def _say_broker(self, db: Session, call: CallSession, text: str) -> CallSession:
        return self._leg_events(
            db,
            call,
            [
                (
                    "call.transcription",
                    {"transcription_data": {"transcript": text, "is_final": True, "confidence": 0.95}},
                )
            ],
        )

    def _owner_turn(self, db: Session, call: CallSession, owner_action: str) -> CallSession:
        if call.state != CallState.HANDOFF_PENDING or not call.handoff_request_id:
            return call
        if owner_action == "accept":
            call = self.orch.owner_response(
                db, call.handoff_request_id, call.owner_id, HandoffStatus.ACCEPTED
            )
            owner_leg = next((s.leg_id for s in call.participant_states if s.role == "owner"), None)
            if call.state == CallState.OWNER_JOINING and owner_leg:
                state = encode_client_state({"call_id": str(call.call_id), "role": "owner"})
                for et in ("call.answered", "conference.participant.joined"):
                    self.orch.handle_event(
                        db,
                        _event(
                            et,
                            {
                                "call_control_id": owner_leg,
                                "client_state": state,
                                "conference_id": call.conference_id,
                            },
                        ),
                    )
                call = repos.calls.require(db, call.call_id)
                from app.contracts import TranscriptTurn

                call = repos.calls.upsert(
                    db,
                    call.model_copy(
                        update={
                            "transcript": [
                                *call.transcript,
                                TranscriptTurn(
                                    speaker="owner",
                                    text="Hi, it's the owner. Let's talk deposit terms directly.",
                                ),
                            ]
                        }
                    ),
                )
                for et in ("call.hangup",):
                    self.orch.handle_event(
                        db,
                        _event(
                            et,
                            {
                                "call_control_id": owner_leg,
                                "client_state": state,
                                "hangup_cause": "normal_clearing",
                            },
                        ),
                    )
                return repos.calls.require(db, call.call_id)
            return call
        status = HandoffStatus.DEFERRED if owner_action == "later" else HandoffStatus.EXPIRED
        if status == HandoffStatus.EXPIRED:
            h = repos.handoffs.require(db, call.handoff_request_id)
            from datetime import UTC, datetime, timedelta

            repos.handoffs.upsert(
                db, h.model_copy(update={"expires_at": datetime.now(UTC) - timedelta(seconds=1)})
            )
            return self.orch.expire_stale_handoffs(db, call.call_id)
        return self.orch.owner_response(db, call.handoff_request_id, call.owner_id, status)
