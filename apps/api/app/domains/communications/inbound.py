"""Inbound webhook processing: verify -> dedupe (atomic with persistence) -> route."""

from __future__ import annotations

from uuid import uuid4

from sqlalchemy import select
from sqlalchemy.exc import IntegrityError
from sqlalchemy.orm import Session

from app.db import models as m
from app.integrations.webhook_signatures import VerifiedWebhook


def record_once(db: Session, verified: VerifiedWebhook, payload: dict) -> bool:
    """True if this provider event is new. Uses the unique (provider, provider_event_id) constraint."""
    exists = db.scalar(
        select(m.ProcessedWebhookEvent.id).where(
            m.ProcessedWebhookEvent.provider == verified.provider,
            m.ProcessedWebhookEvent.provider_event_id == verified.event_id,
        )
    )
    if exists:
        return False
    try:
        with db.begin_nested():
            db.add(
                m.ProcessedWebhookEvent(
                    id=uuid4(),
                    provider=verified.provider,
                    provider_event_id=verified.event_id,
                    verified=verified.mode == "live",
                    payload={"mode": verified.mode, "event": payload},
                )
            )
            db.flush()
    except IntegrityError:
        return False
    return True
