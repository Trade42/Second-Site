"""Approvals, webhooks (signature + dedupe), outreach and the call/handoff state machine."""

from __future__ import annotations

import base64
import json
import time
from datetime import UTC, datetime, timedelta
from uuid import uuid4

import pytest

from app.contracts import ActionType, ApprovalStatus, CallState, HandoffReason, HandoffStatus
from app.core.errors import ApprovalRequiredError, ConflictError, ForbiddenError
from app.integrations.webhook_signatures import standard_webhooks_sign
from app.repositories import registry as repos
from app.services.call_orchestration import CallOrchestrator

from .conftest import FIXTURES, TELNYX_PRIVATE, TEST_PHOTON_SECRET


def _project(client, h, body):
    return client.post("/api/v1/projects", json=body, headers=h).json()["project"]["project_id"]


def _outreach(client, h, pid, phone="+15555550100"):
    r = client.post(
        f"/api/v1/projects/{pid}/outreach",
        headers=h,
        json={
            "property_id": str(uuid4()),
            "channel": "VOICE",
            "recipient": {"role": "broker", "name": "Test Broker", "phone": phone},
            "purpose": "Ask about electrical capacity",
        },
    )
    assert r.status_code == 202, r.text
    return r.json()


def test_approval_lifecycle_and_enforcement(client, auth, demo_project_body):
    h, _ = auth
    pid = _project(client, h, demo_project_body)
    out = _outreach(client, h, pid)
    appr = out["approval"]
    assert appr["approval_status"] == "PENDING"
    # Executing before approval is refused at the action boundary.
    r = client.post(
        f"/api/v1/projects/{pid}/calls",
        json={"communication_id": out["communication"]["communication_id"]},
        headers=h,
    )
    assert r.status_code == 409 and r.json()["error_code"] == "APPROVAL_REQUIRED"
    # Hash mismatch (owner saw a different payload) is refused.
    r = client.post(
        f"/api/v1/approvals/{appr['approval_id']}/decision",
        headers=h,
        json={"decision": "APPROVED", "action_payload_hash": "0" * 64},
    )
    assert r.json()["error_code"] == "APPROVAL_PAYLOAD_MISMATCH"
    ok = client.post(
        f"/api/v1/approvals/{appr['approval_id']}/decision",
        headers=h,
        json={"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]},
    )
    assert ok.json()["approval_status"] == "APPROVED"
    again = client.post(
        f"/api/v1/approvals/{appr['approval_id']}/decision",
        headers=h,
        json={"decision": "REJECTED", "action_payload_hash": appr["action_payload_hash"]},
    )
    assert again.status_code == 409 and again.json()["error_code"] == "INVALID_STATE_TRANSITION"
    call = client.post(
        f"/api/v1/projects/{pid}/calls",
        json={"communication_id": out["communication"]["communication_id"]},
        headers=h,
    )
    assert call.status_code == 202 and call.json()["state"] == "DIALING", call.text
    # Approval is single-use: EXECUTED now, a second call is refused.
    r = client.post(
        f"/api/v1/projects/{pid}/calls",
        json={"communication_id": out["communication"]["communication_id"]},
        headers=h,
    )
    assert r.status_code == 409


def test_expired_and_rejected_approvals(db, container):
    from app.db import models as m

    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    from app.contracts import ProjectContext

    p = ProjectContext(owner_id=owner, project_name="t", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, p)
    payload = {"sku": "bed-1", "qty": 3}
    purchase = container.approvals.request(
        db,
        project_id=p.project_id,
        owner_id=owner,
        agent="procurement",
        action_type=ActionType.PURCHASE,
        payload=payload,
        summary="Buy 3 beds",
        idempotency_key="purchase-1",
    )
    container.approvals.decide(
        db, purchase.approval_id, owner, ApprovalStatus.REJECTED, purchase.action_payload_hash
    )
    with pytest.raises(ApprovalRequiredError):  # owner declined the purchase
        container.approvals.require_approved(db, purchase.approval_id, ActionType.PURCHASE, payload)
    exp = container.approvals.request(
        db,
        project_id=p.project_id,
        owner_id=owner,
        agent="procurement",
        action_type=ActionType.PURCHASE,
        payload=payload,
        summary="Buy",
        idempotency_key="purchase-2",
        ttl=timedelta(seconds=-1),
    )
    with pytest.raises(Exception, match="EXPIRED|not allowed"):
        container.approvals.decide(
            db, exp.approval_id, owner, ApprovalStatus.APPROVED, exp.action_payload_hash
        )
    assert repos.approvals.require(db, exp.approval_id).approval_status == ApprovalStatus.EXPIRED
    with pytest.raises(ForbiddenError):  # someone else cannot decide
        container.approvals.decide(
            db, purchase.approval_id, uuid4(), ApprovalStatus.APPROVED, purchase.action_payload_hash
        )


def _signed_photon(body: bytes, msg_id: str, ts: int | None = None, secret: str = TEST_PHOTON_SECRET) -> dict:
    ts = ts or int(time.time())
    return {
        "webhook-id": msg_id,
        "webhook-timestamp": str(ts),
        "webhook-signature": standard_webhooks_sign(secret, msg_id, ts, body),
        "content-type": "application/json",
    }


def test_photon_webhook_signature_and_dedupe(client, db):
    from sqlalchemy import func, select

    from app.db import models as m

    body = (FIXTURES / "communications" / "photon_inbound_text.json").read_bytes()
    hdr = _signed_photon(body, "evt_fixture_0001")
    assert client.post("/webhooks/photon", content=body, headers=hdr).status_code == 204
    assert (
        client.post("/webhooks/photon", content=body, headers=hdr).status_code == 204
    )  # duplicate accepted, no-op
    db.expire_all()
    # one row for the event id and one for the message id (shared with polling dedupe)
    assert db.scalar(select(func.count()).select_from(m.ProcessedWebhookEvent)) == 2
    row = db.scalar(
        select(m.ProcessedWebhookEvent).where(m.ProcessedWebhookEvent.provider_event_id == "evt_fixture_0001")
    )
    assert row.verified is False and row.payload["mode"] == "fixture"  # fixture secret != live verification
    bad = dict(hdr, **{"webhook-signature": "v1," + base64.b64encode(b"x" * 32).decode()})
    r = client.post("/webhooks/photon", content=body, headers=bad)
    assert r.status_code == 400 and r.json()["error_code"] == "INVALID_SIGNATURE"
    stale = _signed_photon(body, "evt_old", ts=int(time.time()) - 3600)
    assert client.post("/webhooks/photon", content=body, headers=stale).status_code == 400
    tampered = body.replace(b"fourth", b"fifth")
    assert (
        client.post("/webhooks/photon", content=tampered, headers=_signed_photon(body, "evt_t")).status_code
        == 400
    )


def test_telnyx_webhook_ed25519(client):
    body = json.dumps({"data": {"id": "tx-evt-1", "event_type": "call.answered", "payload": {}}}).encode()
    ts = str(int(time.time()))
    sig = base64.b64encode(TELNYX_PRIVATE.sign(f"{ts}|".encode() + body)).decode()
    hdr = {"telnyx-signature-ed25519": sig, "telnyx-timestamp": ts}
    assert client.post("/webhooks/telnyx", content=body, headers=hdr).status_code == 204
    assert client.post("/webhooks/telnyx", content=body + b" ", headers=hdr).status_code == 400


def _approved_call(db, container, phone: str | None = "auto"):
    from app.contracts import ProjectContext
    from app.db import models as m
    from app.domains.communications.owner_channel import register_channel
    from app.services.call_orchestration import call_payload

    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    if phone == "auto":
        phone = f"+1555556{uuid4().int % 10000:04d}"
    if phone:
        register_channel(db, owner, phone)
        container.photon.inject_inbound(phone, "hi")
        from app.domains.communications.poller import poll_once

        poll_once(db, container)  # owner texted first -> channel LINKED
    p = ProjectContext(owner_id=owner, project_name="t", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, p)
    recipient = {"role": "broker", "name": "B", "phone": "+15555550100"}
    payload = call_payload(p.project_id, recipient, None)
    a = container.approvals.request(
        db,
        project_id=p.project_id,
        owner_id=owner,
        agent="t",
        action_type=ActionType.PLACE_CALL,
        payload=payload,
        summary="call",
        idempotency_key=f"c-{uuid4()}",
    )
    container.approvals.decide(db, a.approval_id, owner, ApprovalStatus.APPROVED, a.action_payload_hash)
    return owner, p, recipient, a


def test_call_handoff_accept_decline_and_timeout(db, container):
    orch = CallOrchestrator(container)
    for response in (HandoffStatus.ACCEPTED, HandoffStatus.DECLINED, None):
        owner, p, recipient, a = _approved_call(db, container, phone=f"+1555555{uuid4().int % 10000:04d}")
        call = orch.start(
            db,
            project_id=p.project_id,
            owner_id=owner,
            recipient=recipient,
            property_id=None,
            approval_id=a.approval_id,
            intro_recording_reference="obj/owner-intro.m4a",
        )
        assert call.state == CallState.DIALING and call.provider_call_id.startswith("fixture-call-")
        orch.advance(db, call.call_id, CallState.INTRO)
        orch.advance(db, call.call_id, CallState.ACTIVE)
        h = orch.request_handoff(
            db, call.call_id, HandoffReason.FINANCIAL_DETAILS, "Can you do a 2-month deposit?"
        )
        assert h.status == HandoffStatus.NOTIFIED and h.join_token_reference.startswith("ref_")
        assert any("needs you on a call" in m["text"] for m in container.photon.outbox)
        assert all("ref_" not in m["text"] for m in container.photon.outbox)  # no token material in texts
        if response is None:  # owner never responds -> expires -> assistant defers
            stored = repos.handoffs.require(db, h.handoff_request_id)
            repos.handoffs.upsert(
                db, stored.model_copy(update={"expires_at": datetime.now(UTC) - timedelta(seconds=1)})
            )
            final = orch.expire_stale_handoffs(db, call.call_id)
            assert final.state == CallState.AI_RESUMED
            assert repos.handoffs.require(db, h.handoff_request_id).status == HandoffStatus.EXPIRED
            assert final.deferred_questions == ["Can you do a 2-month deposit?"]
        elif response == HandoffStatus.ACCEPTED:
            final = orch.owner_response(db, h.handoff_request_id, owner, response)
            assert final.state == CallState.OWNER_JOINING and final.conference_id
            owner_leg = next(s.leg_id for s in final.participant_states if s.role == "owner")
            from app.integrations.telnyx import encode_client_state

            cs = encode_client_state({"call_id": str(call.call_id), "role": "owner"})
            joined = orch.handle_event(
                db,
                {
                    "data": {
                        "event_type": "call.answered",
                        "payload": {"call_control_id": owner_leg, "client_state": cs},
                    }
                },
            )
            assert container.telephony.conferences[final.conference_id][-1] == owner_leg
            joined = orch.handle_event(
                db,
                {
                    "data": {
                        "event_type": "conference.participant.joined",
                        "payload": {"call_control_id": owner_leg, "client_state": cs},
                    }
                },
            )
            assert joined.state == CallState.HUMAN_JOINED
        else:
            final = orch.owner_response(db, h.handoff_request_id, owner, response)
            assert final.state == CallState.AI_RESUMED
        events = container.events.list(db, p.project_id)
        for _, e in events:
            assert "join_token" not in json.dumps(e.payload)


def test_owner_accept_without_phone_defers(db, container):
    orch = CallOrchestrator(container)
    owner, p, recipient, a = _approved_call(db, container, phone=None)
    call = orch.start(
        db,
        project_id=p.project_id,
        owner_id=owner,
        recipient=recipient,
        property_id=None,
        approval_id=a.approval_id,
        intro_recording_reference=None,
    )
    orch.advance(db, call.call_id, CallState.INTRO)
    orch.advance(db, call.call_id, CallState.ACTIVE)
    h = orch.request_handoff(db, call.call_id, HandoffReason.BINDING_COMMITMENT, "Sign today?")
    final = orch.owner_response(db, h.handoff_request_id, owner, HandoffStatus.ACCEPTED)
    assert final.state == CallState.AI_RESUMED and "Sign today?" in final.deferred_questions
    comms = repos.communications.list(db, project_id=p.project_id)
    assert any(
        c.purpose == "handoff_request" and c.state.value == "FAILED" for c in comms
    )  # not linked: not sent


def test_call_provider_failure_and_disconnect(db, container):
    orch = CallOrchestrator(container)
    owner, p, recipient, a = _approved_call(db, container)
    container.telephony.fail_next_dial = True
    call = orch.start(
        db,
        project_id=p.project_id,
        owner_id=owner,
        recipient=recipient,
        property_id=None,
        approval_id=a.approval_id,
        intro_recording_reference=None,
    )
    assert call.state == CallState.FAILED and "simulated" in call.failure_reason
    owner, p, recipient, a = _approved_call(db, container)
    call = orch.start(
        db,
        project_id=p.project_id,
        owner_id=owner,
        recipient=recipient,
        property_id=None,
        approval_id=a.approval_id,
        intro_recording_reference=None,
    )
    orch.advance(db, call.call_id, CallState.INTRO)
    orch.advance(db, call.call_id, CallState.ACTIVE)
    dropped = orch.advance(db, call.call_id, CallState.FAILED, "counterparty disconnected")
    assert dropped.state == CallState.FAILED and dropped.ended_at is not None
    with pytest.raises(ConflictError):
        orch.request_handoff(db, call.call_id, HandoffReason.BINDING_COMMITMENT, "sign?")


def test_escalation_policy():
    from app.domains.communications.escalation import detect_escalation

    assert detect_escalation("Will you need a guarantor?") == HandoffReason.GUARANTOR_REQUESTED
    assert detect_escalation("We need first month and deposit by Friday") == HandoffReason.FINANCIAL_DETAILS
    assert detect_escalation("Can I talk to the owner?") == HandoffReason.COUNTERPARTY_REQUESTED_OWNER
    assert detect_escalation("Is there a sink in the back room?") is None
