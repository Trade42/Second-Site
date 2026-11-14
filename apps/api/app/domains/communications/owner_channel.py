"""Owner <-> assistant messaging over Photon.

- The owner registers the handle they text from (phone or iMessage email). The channel is PENDING until the owner
  texts the agent's number: Photon shared lines cannot start a conversation, and replies need the inbound message's
  space id and the agent's user id.
- `notify_owner` always records a CommunicationSession. When the channel is not linked yet, nothing is sent and the
  record says so ('not_linked'); the web app remains the source of truth for approvals.
"""

from __future__ import annotations

import re
from datetime import UTC, datetime
from uuid import UUID

from sqlalchemy import select
from sqlalchemy.orm import Session

from app.contracts import (
    ActorType,
    Channel,
    CommunicationSession,
    CommunicationState,
    EventType,
    OwnerChannel,
    OwnerChannelStatus,
)
from app.core.container import Container
from app.core.errors import AppError
from app.db import models as m
from app.integrations.base import ProviderError
from app.integrations.photon import InboundMessage
from app.repositories import registry as repos

E164 = re.compile(r"^\+[1-9]\d{7,14}$")
EMAIL = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")


def normalize_handle(handle: str) -> str:
    h = handle.strip()
    if EMAIL.match(h):
        return h.lower()
    digits = re.sub(r"[^\d+]", "", h)
    if not digits.startswith("+"):
        digits = "+1" + digits if len(digits) == 10 else "+" + digits
    if not E164.match(digits):
        raise AppError(
            "handle must be an E.164 phone number or an iMessage email", error_code="INVALID_HANDLE"
        )
    return digits


def channel_for_owner(db: Session, owner_id: UUID) -> OwnerChannel | None:
    rows = repos.owner_channels.list(db, owner_id=owner_id)
    return rows[-1] if rows else None


def channel_for_handle(db: Session, handle: str) -> OwnerChannel | None:
    try:
        h = normalize_handle(handle)
    except AppError:
        return None
    row = db.scalar(select(m.OwnerChannelRow).where(m.OwnerChannelRow.handle == h))
    return OwnerChannel.model_validate(row.data) if row else None


def register_channel(db: Session, owner_id: UUID, handle: str, platform: str = "imessage") -> OwnerChannel:
    h = normalize_handle(handle)
    existing = channel_for_handle(db, h)
    if existing and existing.owner_id != owner_id:
        raise AppError("this handle is registered to another account", error_code="HANDLE_TAKEN")
    current = channel_for_owner(db, owner_id)
    if current and current.handle == h:
        return current
    ch = OwnerChannel(owner_id=owner_id, handle=h, platform=platform)
    if current:  # owner switched handles: replace the record (one active channel per owner)
        db.execute(m.OwnerChannelRow.__table__.delete().where(m.OwnerChannelRow.owner_id == owner_id))
        db.flush()
    repos.owner_channels.add(db, ch)
    return ch


def link_from_inbound(db: Session, ch: OwnerChannel, msg: InboundMessage) -> OwnerChannel:
    ch = ch.model_copy(
        update={
            "status": OwnerChannelStatus.LINKED,
            "platform": msg.platform if msg.platform != "unknown" else ch.platform,
            "space_id": msg.space_id,
            "agent_user_id": msg.agent_user_id or ch.agent_user_id,
            "last_inbound_message_id": msg.message_id,
            "linked_at": ch.linked_at or datetime.now(UTC),
            "updated_at": datetime.now(UTC),
        }
    )
    repos.owner_channels.upsert(db, ch)
    return ch


def latest_project_id(db: Session, owner_id: UUID) -> UUID | None:
    projects = repos.projects.list(db, owner_id=owner_id)
    return projects[-1].project_id if projects else None


def notify_owner(
    db: Session,
    c: Container,
    *,
    owner_id: UUID,
    project_id: UUID,
    text: str,
    purpose: str,
    idempotency_key: str,
) -> CommunicationSession:
    """Text the owner through Photon when the channel is linked; always leave an auditable record."""
    ch = channel_for_owner(db, owner_id)
    linked = ch is not None and ch.status == OwnerChannelStatus.LINKED and ch.space_id and ch.agent_user_id
    comm = CommunicationSession(
        project_id=project_id,
        channel=Channel.PHOTON_IMESSAGE if (ch is None or ch.platform == "imessage") else Channel.PHOTON_SMS,
        external_participants=[{"role": "owner", "handle": ch.handle if ch else ""}],
        authorized_owner_id=owner_id,
        purpose=purpose,
        state=CommunicationState.IN_PROGRESS,
    )
    delivery = "not_linked"
    if linked:
        try:
            res = c.photon.send_text(
                space_id=ch.space_id,
                sender_id=ch.agent_user_id,
                text=text,
                idempotency_key=idempotency_key,
                parent_message_id=None,
            )
            comm = comm.model_copy(
                update={"provider_message_ids": [res.provider_message_id] if res.provider_message_id else []}
            )
            delivery = res.state
        except ProviderError as exc:
            delivery = f"failed:{exc.kind}"
            comm = comm.model_copy(update={"state": CommunicationState.FAILED})
    # 'accepted' means Photon took the message; delivery is only confirmed by a later provider event.
    comm = comm.model_copy(
        update={
            "state": CommunicationState.COMPLETED if delivery in ("accepted", "recorded") else comm.state,
            "transcript_reference": None,
            "structured_findings": [],
        }
    )
    if delivery == "not_linked":
        comm = comm.model_copy(update={"state": CommunicationState.FAILED})
    repos.communications.add(db, comm)
    c.events.emit(
        db,
        event_type=EventType.COMMUNICATION_OWNER_NOTIFIED,
        project_id=project_id,
        actor_type=ActorType.AGENT,
        actor_id="owner-messaging",
        idempotency_key=f"notify:{idempotency_key}",
        payload={
            "communication_id": str(comm.communication_id),
            "delivery": delivery.split(":")[0],
            "purpose": purpose,
        },
    )
    return comm
