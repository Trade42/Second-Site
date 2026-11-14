"""Telnyx Call Control. Endpoints verified against the Telnyx OpenAPI reference (2026-11-14):
POST /v2/calls (connection_id, to, from), POST /v2/conferences (call_control_id, name),
POST /v2/conferences/{id}/actions/join (call_control_id). Webhooks: Ed25519 (see webhook_signatures).

Live dialing additionally requires ALLOW_LIVE_OUTBOUND_CALLS=true and an APPROVED ApprovalRequest.
Automated tests never use the live provider.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol
from uuid import uuid4

import httpx

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.core.errors import ForbiddenError
from app.integrations.base import RetryPolicy, status_entry, with_retries

API = "https://api.telnyx.com/v2"


@dataclass(frozen=True)
class DialResult:
    call_control_id: str
    state: str  # "initiated" — answer arrives later via webhook


class TelephonyProvider(Protocol):
    mode: str

    def dial(self, to: str, from_: str, webhook_url: str, idempotency_key: str) -> DialResult: ...

    def create_conference(self, call_control_id: str, name: str) -> str: ...

    def join_conference(self, conference_id: str, call_control_id: str) -> None: ...

    def status(self) -> ProviderStatusEntry: ...


class LiveTelnyxProvider:
    mode = "live"

    def __init__(
        self, api_key: str, connection_id: str, allow_outbound: bool, client: httpx.Client | None = None
    ):
        self.connection_id, self.allow_outbound = connection_id, allow_outbound
        self.client = client or httpx.Client(timeout=20, headers={"Authorization": f"Bearer {api_key}"})

    def _post(self, path: str, body: dict, idem: str | None = None) -> dict:
        def call():
            headers = {"Idempotency-Key": idem} if idem else {}
            r = self.client.post(f"{API}{path}", json=body, headers=headers)
            r.raise_for_status()
            return r.json()["data"]

        return with_retries("telnyx", RetryPolicy(attempts=1), call)

    def dial(self, to, from_, webhook_url, idempotency_key):
        if not self.allow_outbound:
            raise ForbiddenError("live outbound calls disabled (ALLOW_LIVE_OUTBOUND_CALLS=false)")
        d = self._post(
            "/calls",
            {"connection_id": self.connection_id, "to": to, "from": from_, "webhook_url": webhook_url},
            idempotency_key,
        )
        return DialResult(d["call_control_id"], "initiated")

    def create_conference(self, call_control_id, name):
        return self._post("/conferences", {"call_control_id": call_control_id, "name": name})["id"]

    def join_conference(self, conference_id, call_control_id):
        self._post(f"/conferences/{conference_id}/actions/join", {"call_control_id": call_control_id})

    def status(self):
        return status_entry(
            "telnyx",
            self.mode,
            IntegrationStatus.PARTIAL,
            True,
            "Endpoints per OpenAPI; never dialed from CI.",
        )


@dataclass
class FixtureTelephonyProvider:
    mode: str = "fixture"
    calls: list[dict] = field(default_factory=list)
    conferences: dict[str, list[str]] = field(default_factory=dict)
    fail_next_dial: bool = False

    def dial(self, to, from_, webhook_url, idempotency_key):
        if self.fail_next_dial:
            self.fail_next_dial = False
            from app.integrations.base import ProviderError, ProviderErrorKind

            raise ProviderError("telnyx", ProviderErrorKind.UNAVAILABLE, "simulated dial failure")
        cid = f"fixture-call-{uuid4()}"
        self.calls.append({"id": cid, "to": to, "idempotency_key": idempotency_key})
        return DialResult(cid, "initiated")

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
