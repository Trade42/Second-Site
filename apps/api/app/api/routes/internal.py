"""Internal link between the two Second Site services. Not for browsers and not part of the public contract.

After the switch the Rust API serves the HTTP surface and this app keeps Photon and the supervisor, on the same
PostgreSQL and the same AUTH_SECRET. The Rust service texts an owner only through `POST /internal/v1/owner/notify`,
which calls the existing `notify_owner` unchanged.

- Auth: `Authorization: Bearer <token>` in the usual token format, minted with purpose `service` for the owner the
  message is for (the token's user id must equal `owner_id`). Ordinary access tokens are refused here, and
  `api.deps.current_user` keeps refusing service tokens because it verifies purpose `access`.
- Idempotency: `notify_owner` records COMMUNICATION_OWNER_NOTIFIED with event key `notify:{idempotency_key}`. A
  repeated key answers with that earlier record and sends nothing. The check and the send run under a
  transaction-scoped PostgreSQL advisory lock on a SHA-256 of the event key, so two concurrent requests with one key
  text once: the second waits until the first commits, then finds its event.
- `delivery`: 'accepted' means Photon took the message, NOT that it was delivered (only a later provider event says
  that); 'recorded' is the fixture provider; 'not_linked' means nothing was sent because the owner has not texted
  the agent yet; 'failed' means the send failed. A failed send is still a 200 with delivery 'failed', so the caller
  is never blocked by it. Approvals are never granted by text.
"""

from __future__ import annotations

import hashlib
from typing import Literal
from uuid import UUID

from fastapi import APIRouter, Depends, Header
from pydantic import Field
from sqlalchemy import select, text
from sqlalchemy.orm import Session

from app.api.deps import container, db_session
from app.contracts import Contract, EventType
from app.core.container import Container
from app.core.errors import ConflictError, ForbiddenError, UnauthorizedError
from app.core.security import SERVICE_PURPOSE, verify_token
from app.db import models as m
from app.domains.communications.owner_channel import notify_owner

router = APIRouter(prefix="/internal/v1", tags=["internal"], include_in_schema=False)

DELIVERY_DESCRIPTION = (
    "'accepted': Photon took the message (not a delivery confirmation); 'recorded': fixture provider; "
    "'not_linked': nothing sent, the owner has not texted the agent yet; 'failed': the send failed."
)
Delivery = Literal["accepted", "recorded", "not_linked", "failed"]


# At least one non-whitespace character. A pattern, not a ValueError validator: the app's 422 handler cannot
# serialize the exception object a custom validator puts in the error context.
NOT_BLANK = r"\S"


class OwnerNotifyBody(Contract):
    owner_id: UUID
    project_id: UUID
    text: str = Field(min_length=1, max_length=1500, pattern=NOT_BLANK)
    purpose: str = Field(min_length=1, max_length=100, pattern=NOT_BLANK)
    # The stored event key is `notify:` + this, in a String(255) column.
    idempotency_key: str = Field(min_length=1, max_length=200, pattern=NOT_BLANK)


class OwnerNotifyResult(Contract):
    communication_id: UUID
    delivery: Delivery = Field(description=DELIVERY_DESCRIPTION)


def service_caller(
    authorization: str | None = Header(default=None), c: Container = Depends(container)
) -> UUID:
    """The owner id of a valid `service` token. Missing, malformed, expired or other-purpose tokens are 401."""
    if not authorization or not authorization.lower().startswith("bearer "):
        raise UnauthorizedError("missing bearer token")
    token = authorization.split(" ", 1)[1].strip()
    try:
        return verify_token(c.settings.auth_secret.get_secret_value(), token, purpose=SERVICE_PURPOSE)
    except (TypeError, ValueError) as exc:  # non-ASCII signature, unsigned-looking exp/user id: still a 401
        raise UnauthorizedError("malformed token") from exc


def _lock_key(event_key: str) -> int:
    """Signed 64-bit advisory-lock id for one notify key (a collision only serializes two keys)."""
    digest = hashlib.sha256(f"owner-notify:{event_key}".encode()).digest()
    return int.from_bytes(digest[:8], "big", signed=True)


def _lock(db: Session, event_key: str) -> None:
    """Held until this request's transaction commits or rolls back."""
    if db.get_bind().dialect.name == "postgresql":
        db.execute(text("SELECT pg_advisory_xact_lock(:k)"), {"k": _lock_key(event_key)})


def _recorded(db: Session, event_key: str, project_id: UUID) -> OwnerNotifyResult | None:
    """The result `notify_owner` recorded under this key for this project, if any."""
    row = db.scalar(select(m.ActivityEventRow).where(m.ActivityEventRow.idempotency_key == event_key))
    if row is None:
        return None
    if row.project_id != project_id or row.event_type != EventType.COMMUNICATION_OWNER_NOTIFIED.value:
        # Event keys are unique across all projects: notify_owner would text and then skip its event.
        raise ConflictError(
            "idempotency_key is already used by another record", error_code="IDEMPOTENCY_KEY_REUSED"
        )
    payload = row.envelope.get("payload") or {}
    return OwnerNotifyResult(communication_id=payload["communication_id"], delivery=payload["delivery"])


@router.post(
    "/owner/notify",
    response_model=OwnerNotifyResult,
    description="Text the owner through Photon via the existing notify_owner. " + DELIVERY_DESCRIPTION,
)
def owner_notify(
    body: OwnerNotifyBody,
    caller: UUID = Depends(service_caller),
    db: Session = Depends(db_session),
    c: Container = Depends(container),
) -> OwnerNotifyResult:
    """Text the owner (Rust service only). 'accepted' means Photon took the message, not that it was delivered.

    401 without a valid `service` token, 403 when the token is for another owner, 404 unless the project exists and
    belongs to that owner, 409 when the key was used by another project or record. A repeated key returns the earlier
    `{communication_id, delivery}` and sends nothing.
    """
    if caller != body.owner_id:
        raise ForbiddenError("service token was issued for another owner")
    c.authz.project_for_user(db, body.project_id, body.owner_id)  # 404 unless it is this owner's project
    event_key = f"notify:{body.idempotency_key}"
    _lock(db, event_key)
    earlier = _recorded(db, event_key, body.project_id)
    if earlier is not None:
        return earlier
    comm = notify_owner(
        db,
        c,
        owner_id=body.owner_id,
        project_id=body.project_id,
        text=body.text,
        purpose=body.purpose,
        idempotency_key=body.idempotency_key,
    )
    result = _recorded(db, event_key, body.project_id)
    if result is None or result.communication_id != comm.communication_id:
        # Only possible if a writer that does not take this lock used the same event key meanwhile.
        raise ConflictError(
            "idempotency_key was used concurrently by another writer", error_code="IDEMPOTENCY_KEY_REUSED"
        )
    return result
