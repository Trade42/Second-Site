"""Provider webhooks: verify signature on the RAW body -> dedupe atomically -> route. Owner: communications."""

from __future__ import annotations

import json

from fastapi import APIRouter, Depends, Request, Response
from sqlalchemy.orm import Session

from app.api.deps import container, db_session
from app.core.config import ProviderMode
from app.core.container import Container
from app.domains.communications.inbound import record_once
from app.integrations.photon import parse_inbound
from app.integrations.webhook_signatures import verify_standard_webhook, verify_telnyx

router = APIRouter(prefix="/webhooks", tags=["webhooks"])


def _mode(m: ProviderMode) -> str:
    return "live" if m == ProviderMode.LIVE else "fixture"


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
    if msg is not None:
        # Routing to project/design workflows is implemented in dev/communications.
        pass
    return Response(status_code=204)


@router.post("/telnyx", status_code=204)
async def telnyx(request: Request, db: Session = Depends(db_session), c: Container = Depends(container)):
    body = await request.body()
    verified = verify_telnyx(
        c.settings.telnyx_public_key, request.headers, body, _mode(c.settings.voice_provider_mode)
    )
    if not record_once(db, verified, json.loads(body)):
        return Response(status_code=204)
    # Call-control event -> CallOrchestrator.advance(...) mapping is implemented in dev/communications.
    return Response(status_code=204)
