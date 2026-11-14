"""xAI realtime voice (Speech to Speech). Verified against docs.x.ai (2026-11-14):
- SIP: register a Direct SIP number; xAI sends a Standard-Webhooks-signed `realtime.call.incoming`
  webhook with data.call_id; join via `wss://api.x.ai/v1/realtime?call_id={call_id}` (Bearer XAI_API_KEY),
  then send `session.update` and `response.create`.
- Intended topology: Telnyx dials the counterparty, bridges/transfers the assistant leg to the xAI SIP
  destination, and owns the conference so the owner can join (HUMAN_JOINED).
STATUS: NOT_STARTED for live audio; this module defines the session config builder only.
"""

from __future__ import annotations

from app.contracts import CallSession, IntegrationStatus
from app.integrations.base import status_entry

REALTIME_URL = "wss://api.x.ai/v1/realtime?call_id={call_id}"


def build_session_update(call: CallSession, property_summary: str, questions: list[str]) -> dict:
    """`session.update` payload. Instructions encode disclosure + authority limits; tool calls go
    back to the backend (escalate_to_owner, record_finding)."""
    scope = call.authority_scope
    instructions = (
        f"{call.automation_disclosure} You are the business owner's assistant. You are not a human "
        f"employee and must say so if asked. Ask only about: {', '.join(scope.may_ask_about)}. "
        f"Never: {', '.join(scope.may_not)} — call escalate_to_owner instead. "
        f"Property: {property_summary}. Questions: " + " | ".join(questions)
    )
    return {
        "type": "session.update",
        "session": {
            "instructions": instructions,
            "tools": [
                {
                    "type": "function",
                    "name": "escalate_to_owner",
                    "parameters": {
                        "type": "object",
                        "properties": {"reason": {"type": "string"}, "question": {"type": "string"}},
                        "required": ["reason", "question"],
                    },
                },
                {
                    "type": "function",
                    "name": "record_finding",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "answer": {"type": "string"},
                            "claim_type": {"type": "string"},
                        },
                        "required": ["question", "answer"],
                    },
                },
            ],
        },
    }


def status(has_key: bool):
    return status_entry(
        "xai_voice",
        "fixture",
        IntegrationStatus.NOT_STARTED,
        has_key,
        "Session config builder only; realtime audio bridge not implemented.",
    )
