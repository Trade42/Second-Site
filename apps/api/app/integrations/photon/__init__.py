"""Photon 2.0 (Beta) messaging. Verified against https://photon.codes/docs/beta (2026-11-14):

- API origin https://api.photon.codes/v1, `Authorization: Bearer <project key pho_sk_...>`, `Idempotency-Key` on writes.
- Send:  POST /v1/projects/{id}/messages {spaceId, senderId, parentMessageId?, content:{type:'text', format:'plain', text}}.
  A reply reuses the inbound message's `space.id` as spaceId and `receivedBy.id` (the agent's User id) as senderId.
- List:  GET  /v1/projects/{id}/messages?direction=received&order=asc&createdSince=..&platform=..&pageSize=..&pageToken=..
  -> {items: [PlatformMessage], nextPageToken?}. Used for polling when no public webhook URL exists.
- Message object: id, platform, status ('received' | 'dispatched' | ...), sentBy/receivedBy {id, kind 'human'|'agent',
  handle, platform}, space {id, kind 'direct'|...}, content {type:'text', format, text} (images: content type
  'attachment'; Beta iMessage supports only text and images).
- Webhooks: Standard Webhooks (`whsec_` secret), body = public event {id, type, data}; event type 'message.received'.
  Dedupe on `webhook-id`. A 2xx is not delivery confirmation; an accepted send is not delivery either.
- A shared iMessage line sends only in direct Spaces to a peer who has messaged it since the assignment: the owner
  must text the agent first. Photon voice is SIP trunking to an external agent (no 3-way conferencing).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Protocol
from uuid import uuid4

import httpx

from app.contracts import IntegrationStatus, ProviderStatusEntry
from app.integrations.base import ProviderError, ProviderErrorKind, RetryPolicy, status_entry, with_retries

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
    sender_user_id: str | None
    agent_user_id: str | None
    agent_handle: str | None
    space_id: str
    space_kind: str | None
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

    def list_received(
        self, created_since: datetime, page_token: str | None = None
    ) -> tuple[list[dict[str, Any]], str | None]: ...

    def status(self) -> ProviderStatusEntry: ...


def parse_message(msg: dict[str, Any], event_id: str | None = None) -> InboundMessage | None:
    """Parse a received PlatformMessage. Returns None for anything that is not an inbound human message."""
    if msg.get("status") != "received":
        return None
    sent = msg.get("sentBy") or {}
    recv = msg.get("receivedBy") or {}
    if sent.get("kind") not in (None, "human"):
        return None  # never react to our own (agent) messages
    content = msg.get("content") or {}
    ctype = content.get("type")
    images: list[str] = []
    if ctype in ("attachment", "image"):
        images.append(
            str(content.get("id") or content.get("attachmentId") or content.get("name") or "attachment")
        )
    elif ctype == "group":  # e.g. text + image parts
        for part in content.get("parts") or []:
            if part.get("type") in ("attachment", "image"):
                images.append(str(part.get("id") or part.get("name") or "attachment"))
    text = content.get("text") if ctype == "text" else None
    if ctype == "group" and text is None:
        texts = [
            p.get("text") for p in content.get("parts") or [] if p.get("type") == "text" and p.get("text")
        ]
        text = "\n".join(texts) or None
    space = msg.get("space") or {}
    return InboundMessage(
        event_id=str(event_id or msg.get("id")),
        message_id=str(msg.get("id")),
        platform=str(msg.get("platform") or sent.get("platform") or "unknown"),
        sender_handle=str(sent.get("handle")),
        sender_user_id=sent.get("id"),
        agent_user_id=recv.get("id"),
        agent_handle=recv.get("handle"),
        space_id=str(space.get("id")),
        space_kind=space.get("kind"),
        text=text,
        image_attachment_refs=images,
    )


def parse_inbound(event: dict[str, Any]) -> InboundMessage | None:
    """Parse a webhook/WebSocket public event {id, type, data}. Only 'message.received' events are messages."""
    etype = event.get("type")
    if etype is not None and etype != "message.received":
        return None
    data = event.get("data") or event
    msg = data.get("message") if isinstance(data.get("message"), dict) else data
    return parse_message(msg, event_id=event.get("id"))


class LivePhotonProvider:
    mode = "live"

    def __init__(self, api_key: str, project_id: str, client: httpx.Client | None = None):
        self.project_id = project_id
        self.configured = bool(api_key and project_id)
        self.client = client or httpx.Client(timeout=20, headers={"Authorization": f"Bearer {api_key}"})

    def send_text(self, space_id, sender_id, text, idempotency_key, parent_message_id=None):
        body: dict[str, Any] = {
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

    def list_received(self, created_since, page_token=None):
        params: dict[str, Any] = {
            "direction": "received",
            "order": "asc",
            "pageSize": 100,
            "createdSince": created_since.strftime("%Y-%m-%dT%H:%M:%SZ"),
        }
        if page_token:
            params["pageToken"] = page_token

        def call():
            r = self.client.get(f"{API}/projects/{self.project_id}/messages", params=params)
            r.raise_for_status()
            return r.json()

        data = with_retries("photon", RetryPolicy(), call)
        if not isinstance(data.get("items"), list):
            raise ProviderError("photon", ProviderErrorKind.BAD_RESPONSE, "list messages: missing items")
        return data["items"], data.get("nextPageToken")

    def status(self):
        return status_entry(
            "photon",
            self.mode,
            IntegrationStatus.PARTIAL,
            self.configured,
            "Send/list per Beta docs; owner channel, inbound routing and polling implemented; "
            "not yet exercised against a real Photon project.",
        )


class FixturePhotonProvider:
    """In-memory Photon: records sends, and lets tests/simulator inject inbound messages for polling."""

    mode = "fixture"

    def __init__(self):
        self.outbox: list[dict] = []
        self.inbox: list[dict] = []

    def send_text(self, space_id, sender_id, text, idempotency_key, parent_message_id=None):
        for m in self.outbox:
            if m["idempotency_key"] == idempotency_key:
                return SendResult(m["id"], "recorded")
        mid = f"fixture-msg-{uuid4()}"
        self.outbox.append(
            {
                "id": mid,
                "space_id": space_id,
                "sender_id": sender_id,
                "text": text,
                "idempotency_key": idempotency_key,
                "parent_message_id": parent_message_id,
            }
        )
        return SendResult(mid, "recorded")

    def inject_inbound(self, handle: str, text: str, space_id: str = "spc_fixture_owner") -> dict:
        msg = {
            "id": f"fixture-in-{uuid4()}",
            "platform": "imessage",
            "status": "received",
            "createdAt": datetime.now().astimezone().isoformat(),
            "sentBy": {"id": f"usr_{handle}", "kind": "human", "handle": handle},
            "receivedBy": {"id": "usr_fixture_agent", "kind": "agent", "handle": "+15555550199"},
            "space": {"id": space_id, "kind": "direct"},
            "content": {"type": "text", "format": "plain", "text": text},
        }
        self.inbox.append(msg)
        return msg

    def list_received(self, created_since, page_token=None):
        items, self.inbox = self.inbox, []
        return items, None

    def status(self):
        return status_entry(
            "photon", self.mode, IntegrationStatus.MOCK, False, "In-memory outbox/inbox; nothing is sent."
        )
