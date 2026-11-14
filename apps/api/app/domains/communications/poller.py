"""Photon polling: an alternative to webhooks when there is no public HTTPS endpoint (local dev, demos).
Uses GET /v1/projects/{id}/messages?direction=received and dedupes on the Photon message id, so polling and
webhooks can run side by side without double-processing."""

from __future__ import annotations

from datetime import UTC, datetime, timedelta

from sqlalchemy.orm import Session

from app.core.container import Container
from app.domains.communications.inbound import record_once
from app.integrations.photon import parse_message
from app.integrations.webhook_signatures import VerifiedWebhook

LOOKBACK = timedelta(minutes=30)


def poll_once(db: Session, c: Container, since: datetime | None = None) -> dict[str, int]:
    from app.api.routes.webhooks import handle_owner_message

    since = since or datetime.now(UTC) - LOOKBACK
    counts = {"fetched": 0, "new": 0, "routed": 0}
    token = None
    for _ in range(10):  # page limit per poll
        items, token = c.photon.list_received(since, token)
        for item in items:
            counts["fetched"] += 1
            msg = parse_message(item)
            if msg is None:
                continue
            verified = VerifiedWebhook("photon", f"msg:{msg.message_id}", c.photon.mode)
            if not record_once(db, verified, {"polled": True, "message": item}):
                continue
            counts["new"] += 1
            if handle_owner_message(db, c, msg) != "ignored":
                counts["routed"] += 1
        if not token:
            break
    return counts
