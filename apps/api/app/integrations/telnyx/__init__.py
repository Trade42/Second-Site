"""Telnyx Call Control. Verified against the Telnyx API reference + webhook catalog (2026-11-14):

Commands (POST, Bearer TELNYX_API_KEY, base https://api.telnyx.com/v2):
- /calls {connection_id, to, from, webhook_url, client_state(base64), answering_machine_detection:'detect', timeout_secs}
- /calls/{call_control_id}/actions/speak {payload, voice:'female'|..., language?, client_state?}
- /calls/{call_control_id}/actions/playback_start {audio_url, client_state?}
- /calls/{call_control_id}/actions/transcription_start {transcription_engine:'Telnyx'|'Google'|..., transcription_tracks?}
- /calls/{call_control_id}/actions/hangup {client_state?}
- /conferences {call_control_id, name}  ->  {id};  /conferences/{id}/actions/join {call_control_id}
Webhooks (Ed25519, see webhook_signatures): {data:{event_type, id, occurred_at, payload:{call_control_id, client_state,
...}}}; events used: call.initiated, call.answered, call.machine.detection.ended {result}, call.speak.ended,
call.playback.ended, call.transcription {transcription_data:{transcript, is_final, confidence}} (no client_state),
call.hangup {hangup_cause, hangup_source}, conference.participant.joined {conference_id}.

Live dialing additionally requires ALLOW_LIVE_OUTBOUND_CALLS=true and an APPROVED ApprovalRequest.
Automated tests never use the live provider.
"""

from __future__ import annotations

import base64
import json
from dataclasses import dataclass, field
from typing import Any, Protocol
from uuid import uuid4

import httpx

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.core.errors import ForbiddenError
from app.integrations.base import RetryPolicy, status_entry, with_retries

API = "https://api.telnyx.com/v2"
VOICE = "female"


def encode_client_state(state: dict[str, Any]) -> str:
    return base64.b64encode(json.dumps(state, separators=(",", ":")).encode()).decode()


def decode_client_state(value: str | None) -> dict[str, Any]:
    if not value:
        return {}
    try:
        out = json.loads(base64.b64decode(value))
        return out if isinstance(out, dict) else {}
    except (ValueError, json.JSONDecodeError):
        return {}


@dataclass(frozen=True)
class DialResult:
    call_control_id: str
    state: str  # "initiated" — answer arrives later via webhook


class TelephonyProvider(Protocol):
    mode: str

    def dial(
        self, to: str, from_: str, webhook_url: str, idempotency_key: str, client_state: dict | None = None
    ) -> DialResult: ...

    def speak(self, call_control_id: str, text: str) -> None: ...

    def play(self, call_control_id: str, audio_url: str) -> None: ...

    def start_transcription(self, call_control_id: str) -> None: ...

    def hangup(self, call_control_id: str) -> None: ...

    def create_conference(self, call_control_id: str, name: str) -> str: ...

    def join_conference(self, conference_id: str, call_control_id: str) -> None: ...

    def status(self) -> ProviderStatusEntry: ...


class LiveTelnyxProvider:
    mode = "live"

    def __init__(
        self, api_key: str, connection_id: str, allow_outbound: bool, client: httpx.Client | None = None
    ):
        self.connection_id, self.allow_outbound = connection_id, allow_outbound
        self.configured = bool(api_key and connection_id)
        self.client = client or httpx.Client(timeout=20, headers={"Authorization": f"Bearer {api_key}"})

    def _post(self, path: str, body: dict, idem: str | None = None) -> dict:
        def call():
            headers = {"Idempotency-Key": idem} if idem else {}
            r = self.client.post(f"{API}{path}", json=body, headers=headers)
            r.raise_for_status()
            return r.json().get("data") or {}

        return with_retries("telnyx", RetryPolicy(attempts=1), call)

    def dial(self, to, from_, webhook_url, idempotency_key, client_state=None):
        if not self.allow_outbound:
            raise ForbiddenError("live outbound calls disabled (ALLOW_LIVE_OUTBOUND_CALLS=false)")
        body = {
            "connection_id": self.connection_id,
            "to": to,
            "from": from_,
            "webhook_url": webhook_url,
            "answering_machine_detection": "detect",
            "timeout_secs": 40,
        }
        if client_state:
            body["client_state"] = encode_client_state(client_state)
        d = self._post("/calls", body, idempotency_key)
        return DialResult(d["call_control_id"], "initiated")

    def speak(self, call_control_id, text):
        self._post(
            f"/calls/{call_control_id}/actions/speak", {"payload": text, "voice": VOICE, "language": "en-US"}
        )

    def play(self, call_control_id, audio_url):
        self._post(f"/calls/{call_control_id}/actions/playback_start", {"audio_url": audio_url})

    def start_transcription(self, call_control_id):
        self._post(
            f"/calls/{call_control_id}/actions/transcription_start", {"transcription_engine": "Telnyx"}
        )

    def hangup(self, call_control_id):
        self._post(f"/calls/{call_control_id}/actions/hangup", {})

    def create_conference(self, call_control_id, name):
        return self._post("/conferences", {"call_control_id": call_control_id, "name": name})["id"]

    def join_conference(self, conference_id, call_control_id):
        self._post(f"/conferences/{conference_id}/actions/join", {"call_control_id": call_control_id})

    def status(self):
        return status_entry(
            "telnyx",
            self.mode,
            IntegrationStatus.PARTIAL,
            self.configured,
            "Call flow (dial, intro, transcription turns, handoff conference) per API reference; "
            "never dialed from CI" + ("" if self.allow_outbound else "; live outbound disabled") + ".",
        )


@dataclass
class FixtureTelephonyProvider:
    """Records every command; the call simulator plays the carrier's role by emitting webhook-shaped events."""

    mode: str = "fixture"
    calls: list[dict] = field(default_factory=list)
    conferences: dict[str, list[str]] = field(default_factory=dict)
    commands: list[dict] = field(default_factory=list)
    fail_next_dial: bool = False

    def dial(self, to, from_, webhook_url, idempotency_key, client_state=None):
        if self.fail_next_dial:
            self.fail_next_dial = False
            from app.integrations.base import ProviderError, ProviderErrorKind

            raise ProviderError("telnyx", ProviderErrorKind.UNAVAILABLE, "simulated dial failure")
        for c in self.calls:
            if c["idempotency_key"] == idempotency_key:
                return DialResult(c["id"], "initiated")
        cid = f"fixture-call-{uuid4()}"
        self.calls.append(
            {"id": cid, "to": to, "idempotency_key": idempotency_key, "client_state": client_state}
        )
        return DialResult(cid, "initiated")

    def speak(self, call_control_id, text):
        self.commands.append({"cmd": "speak", "leg": call_control_id, "text": text})

    def play(self, call_control_id, audio_url):
        self.commands.append({"cmd": "playback_start", "leg": call_control_id, "audio_url": audio_url})

    def start_transcription(self, call_control_id):
        self.commands.append({"cmd": "transcription_start", "leg": call_control_id})

    def hangup(self, call_control_id):
        self.commands.append({"cmd": "hangup", "leg": call_control_id})

    def create_conference(self, call_control_id, name):
        conf = f"fixture-conf-{uuid4()}"
        self.conferences[conf] = [call_control_id]
        return conf

    def join_conference(self, conference_id, call_control_id):
        self.conferences.setdefault(conference_id, []).append(call_control_id)

    def status(self):
        return status_entry(
            "telnyx", self.mode, IntegrationStatus.MOCK, False, "Simulated calls; no PSTN traffic."
        )
