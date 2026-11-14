"""Photon 2.0 (Beta) messaging. Verified against https://photon.codes/docs/beta (2026-11-14):

- API origin https://api.photon.codes/v1, Bearer PHOTON_API_KEY, Idempotency-Key header on writes.
- Model: Users, Spaces, Messages. Send: POST /projects/{project_id}/messages
  {spaceId, senderId, parentMessageId?, content:{type:'text', format:'plain', text}}.
- Inbound: webhooks signed with Standard Webhooks (`whsec_` secret), dedupe on `webhook-id`;
  or Socket.IO with replay cursors. Exact event-type names come from the per-API-version catalog.
- Beta iMessage supports only text and images. A shared line cannot send an unsolicited first
  message (owner must text the agent first). An accepted send is NOT delivery confirmation.
- Photon voice = SIP trunking to an external voice agent; it does NOT provide 3-way conferencing.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol
from uuid import uuid4

import httpx

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.integrations.base import RetryPolicy, status_entry, with_retries

API = "https://api.photon.codes/v1"


@dataclass(frozen=True)
class SendResult:
    provider_message_id: str | None
    state: str  # "accepted" (async dispatch) | "recorded" (fixture) — never "delivered" without an event


@dataclass
class InboundMessage:
    event_id: str
    message_id: str
    platform: str
    sender_handle: str
    space_id: str
    text: str | None
    image_attachment_refs: list[str] = field(default_factory=list)


class PhotonProvider(Protocol):
    mode: str

    def send_text(
        self,
        space_id: str,
        sender_id: str,
        text: str,
        idempotency_key: str,
        parent_message_id: str | None = None,
    ) -> SendResult: ...

    def status(self) -> ProviderStatusEntry: ...


def parse_inbound(event: dict) -> InboundMessage | None:
    """Best-effort parse of a message event. Returns None for non-message events."""
    data = event.get("data") or event
    content = data.get("content") or {}
    if data.get("status") != "received":
        return None
    images = []
    if content.get("type") == "image":
        images.append(
            str(content.get("id") or content.get("attachmentId") or content.get("url") or "unknown")
        )
    return InboundMessage(
        event_id=str(event.get("id") or data.get("id")),
        message_id=str(data.get("id")),
        platform=str(data.get("platform")),
        sender_handle=str((data.get("sentBy") or {}).get("handle")),
        space_id=str((data.get("space") or {}).get("id")),
        text=content.get("text") if content.get("type") == "text" else None,
        image_attachment_refs=images,
    )


class LivePhotonProvider:
    mode = "live"

    def __init__(self, api_key: str, project_id: str, client: httpx.Client | None = None):
        self.project_id = project_id
        self.client = client or httpx.Client(timeout=20, headers={"Authorization": f"Bearer {api_key}"})

    def send_text(self, space_id, sender_id, text, idempotency_key, parent_message_id=None):
        body = {
            "spaceId": space_id,
            "senderId": sender_id,
            "content": {"type": "text", "format": "plain", "text": text},
        }
        if parent_message_id:
            body["parentMessageId"] = parent_message_id

        def call():
            r = self.client.post(
                f"{API}/projects/{self.project_id}/messages",
                json=body,
                headers={"Idempotency-Key": idempotency_key},
            )
            r.raise_for_status()
            return r.json()

        data = with_retries("photon", RetryPolicy(), call)
        return SendResult(provider_message_id=str(data.get("id")), state="accepted")

    def status(self):
        return status_entry(
            "photon",
            self.mode,
            IntegrationStatus.PARTIAL,
            True,
            "Send path per beta docs; not exercised end-to-end in CI.",
        )


class FixturePhotonProvider:
    mode = "fixture"

    def __init__(self):
        self.outbox: list[dict] = []

    def send_text(self, space_id, sender_id, text, idempotency_key, parent_message_id=None):
        for m in self.outbox:
            if m["idempotency_key"] == idempotency_key:
                return SendResult(m["id"], "recorded")
        mid = f"fixture-msg-{uuid4()}"
        self.outbox.append(
            {"id": mid, "space_id": space_id, "text": text, "idempotency_key": idempotency_key}
        )
        return SendResult(mid, "recorded")

    def status(self):
        return status_entry(
            "photon", self.mode, IntegrationStatus.MOCK, False, "In-memory outbox; nothing is sent."
        )
