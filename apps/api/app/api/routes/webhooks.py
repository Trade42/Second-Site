"""Provider webhooks: verify signature on the RAW body -> dedupe atomically -> route. Owner: communications."""

from __future__ import annotations

import json
import logging

from fastapi import APIRouter, Depends, Request, Response
from sqlalchemy.orm import Session

from app.api.deps import container, db_session
from app.contracts import ActorType, EventType
from app.core.config import ProviderMode
from app.core.container import Container
from app.domains.communications.inbound import record_once, route_owner_message
from app.domains.communications.owner_channel import channel_for_handle, latest_project_id
from app.integrations.base import ProviderError
from app.integrations.photon import InboundMessage, parse_inbound
from app.integrations.webhook_signatures import VerifiedWebhook, verify_standard_webhook, verify_telnyx
from app.services.call_orchestration import CallOrchestrator

router = APIRouter(prefix="/webhooks", tags=["webhooks"])
log = logging.getLogger(__name__)


def _mode(m: ProviderMode) -> str:
    return "live" if m == ProviderMode.LIVE else "fixture"


def handle_owner_message(db: Session, c: Container, msg: InboundMessage) -> str:
    """Route an inbound Photon message and reply in the same Space. Returns how it was handled."""
    handled_as, reply = route_owner_message(db, c, msg)
    ch = channel_for_handle(db, msg.sender_handle)
    project_id = latest_project_id(db, ch.owner_id) if ch else None
    if reply and msg.agent_user_id:
        try:
            c.photon.send_text(
                space_id=msg.space_id,
                sender_id=msg.agent_user_id,
                text=reply,
                idempotency_key=f"reply:{msg.message_id}",
                parent_message_id=msg.message_id,
            )
        except ProviderError as exc:
            log.warning("photon reply failed: %s", exc)
    if project_id:
        c.events.emit(
            db,
            event_type=EventType.COMMUNICATION_MESSAGE_RECEIVED,
            project_id=project_id,
            actor_type=ActorType.USER,
            actor_id=str(ch.owner_id),
            idempotency_key=f"photon-in:{msg.message_id}",
            payload={
                "channel": f"photon:{msg.platform}",
                "provider_message_id": msg.message_id,
                "handled_as": handled_as,
                "summary": (msg.text or "[image]")[:120],
            },
        )
    return handled_as


@router.post("/photon", status_code=204)
async def photon(request: Request, db: Session = Depends(db_session), c: Container = Depends(container)):
    body = await request.body()
    secret = c.settings.photon_webhook_secret.get_secret_value() if c.settings.photon_webhook_secret else None
    verified = verify_standard_webhook(
        "photon", secret, request.headers, body, _mode(c.settings.messaging_provider_mode)
    )
    event = json.loads(body)
    if not record_once(db, verified, event):
        return Response(status_code=204)  # duplicate: already accepted, do nothing again
    msg = parse_inbound(event)
    # Also dedupe on the message id so webhook delivery and polling never both process one message.
    if msg is not None and record_once(
        db, VerifiedWebhook("photon", f"msg:{msg.message_id}", verified.mode), event
    ):
        handle_owner_message(db, c, msg)
    return Response(status_code=204)


@router.post("/telnyx", status_code=204)
async def telnyx(request: Request, db: Session = Depends(db_session), c: Container = Depends(container)):
    body = await request.body()
    verified = verify_telnyx(
        c.settings.telnyx_public_key, request.headers, body, _mode(c.settings.voice_provider_mode)
    )
    event = json.loads(body)
    if not record_once(db, verified, event):
        return Response(status_code=204)
    CallOrchestrator(c).handle_event(db, event)
    return Response(status_code=204)
