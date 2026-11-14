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


# ---- owner messages over Photon ------------------------------------------------------------------

JOIN_WORDS = {"join", "yes", "y", "1", "call me", "connect"}
LATER_WORDS = {"later", "no", "n", "2", "not now", "decline"}


def route_owner_message(db: Session, c, msg) -> tuple[str, str | None]:
    """Route one inbound Photon message. Returns (handled_as, reply_text_or_None).

    Unknown senders are ignored (never answered): a shared line must not talk to strangers about a project.
    Approvals are never granted by text; the reply points to the authenticated web approval instead.
    """
    from app.agents.supervisor import run_chat
    from app.agents.tools import ToolContext
    from app.contracts import HandoffStatus, OwnerChannelStatus
    from app.domains.communications.owner_channel import (
        channel_for_handle,
        latest_project_id,
        link_from_inbound,
    )
    from app.repositories import registry as repos
    from app.services.call_orchestration import CallOrchestrator

    ch = channel_for_handle(db, msg.sender_handle)
    if ch is None:
        return "ignored", None
    first_link = ch.status != OwnerChannelStatus.LINKED
    ch = link_from_inbound(db, ch, msg)
    web = c.settings.public_web_url
    text = (msg.text or "").strip()
    word = text.lower().strip(" .!?")
    if msg.image_attachment_refs and not text:
        return "image", (
            "Got the photo, thanks. I've noted it on your project; photo analysis isn't connected yet, "
            "so describe anything important in a text."
        )
    if first_link and word in {"", "hi", "hello", "hey", "start", "link"}:
        return "linked", (
            "You're connected. I'll text you when a call needs you, when approvals are waiting, and with call "
            "summaries. Reply STATUS any time, or just ask me about your properties."
        )
    orch = CallOrchestrator(c)
    if word in JOIN_WORDS | LATER_WORDS:
        h = orch.pending_handoff_for_owner(db, ch.owner_id)
        if h is None:
            return "command", "There's no call waiting for you right now."
        status = HandoffStatus.ACCEPTED if word in JOIN_WORDS else HandoffStatus.DEFERRED
        call = orch.owner_response(db, h.handoff_request_id, ch.owner_id, status)
        if call.state.value == "OWNER_JOINING":
            return "command", "Calling you now. Pick up to join the broker."
        if status == HandoffStatus.ACCEPTED:
            return (
                "command",
                "I couldn't connect you (no phone on file or the call ended). I told them you'll follow up.",
            )
        return "command", "Okay. I told them you'll follow up directly, and I didn't agree to anything."
    if word in {"approve", "approved", "yes approve"}:
        return "command", f"For your protection, approvals happen in the app: {web}/activity"
    if word == "stop":
        repos.owner_channels.upsert(
            db, ch.model_copy(update={"status": OwnerChannelStatus.PENDING_FIRST_MESSAGE})
        )
        return "command", "Okay, I'll stop texting you. Text START to turn updates back on."
    project_id = latest_project_id(db, ch.owner_id)
    if project_id is None:
        return "chat", f"I don't see a project for you yet. Start one at {web}"
    if word == "status":
        return "command", _status_text(db, project_id, web)
    # Owner sentences that start with revise: or change: are saved here and never sent to chat.
    # Layout edits still happen in the app. This does not grant an approval or revise a layout.
    change = _design_change_words(text)
    if change is not None:
        _save_design_change(db, project_id, ch.owner_id, change, msg)
        return (
            "command",
            f"Noted. I saved this design change: {change}. Layout edits still happen in the app.",
        )
    ctx = ToolContext(db=db, c=c, project_id=project_id, owner_id=ch.owner_id)
    reply = run_chat(ctx, text, c.settings, thread_id=f"photon:{ch.owner_id}").reply
    return "chat", reply[:1500]


def _design_change_words(text: str) -> str | None:
    """The sentence after a leading 'revise:' or 'change:', or None when this is ordinary text."""
    lowered = text.lower()
    for prefix in ("revise:", "change:"):
        if lowered.startswith(prefix):
            return text[len(prefix):].strip()
    return None


def _save_design_change(db: Session, project_id, owner_id, words: str, msg) -> None:
    """Persist the owner's design sentence. Does not call DesignService.revise."""
    from app.contracts import Channel, CommunicationSession, CommunicationState, StructuredFinding
    from app.repositories import registry as repos

    platform = getattr(msg, "platform", "")
    repos.communications.add(
        db,
        CommunicationSession(
            project_id=project_id,
            channel=Channel.PHOTON_SMS if platform == "sms" else Channel.PHOTON_IMESSAGE,
            authorized_owner_id=owner_id,
            purpose="design change",
            state=CommunicationState.COMPLETED,
            structured_findings=[
                StructuredFinding(question="design change", answer=words, classification="OWNER_REPORTED")
            ],
            provider_message_ids=[msg.message_id] if getattr(msg, "message_id", None) else [],
        ),
    )


def _status_text(db: Session, project_id, web: str) -> str:
    from app.repositories import registry as repos

    rankings = repos.rankings.list(db, project_id=project_id)
    props = {p.property_id: p for p in repos.properties.list(db, project_id=project_id)}
    lines = []
    if rankings and rankings[-1].ranked_properties:
        top = rankings[-1].ranked_properties[0]
        p = props.get(top.property_id)
        if p:
            lines.append(
                f"Top pick: {p.property_identity.normalized_address} (${top.long_term_cost / 100:,.0f} over the horizon)."
            )
    pending = [
        a for a in repos.approvals.list(db, project_id=project_id) if a.approval_status.value == "PENDING"
    ]
    if pending:
        lines.append(f"{len(pending)} approval(s) waiting: {web}/activity")
    calls = [
        c
        for c in repos.calls.list(db, project_id=project_id)
        if c.state.value not in ("COMPLETED", "FAILED", "DECLINED", "CANCELLED")
    ]
    if calls:
        lines.append(f"{len(calls)} call(s) in progress.")
    return " ".join(lines) or "Nothing new yet."
