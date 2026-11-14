"""Communications end to end: Photon owner channel + commands, approval-gated broker call driven by Telnyx-shaped
events (simulator), owner handoff and conference, findings -> evidence -> feasibility, provider request shapes."""

from __future__ import annotations

import json
from datetime import UTC, datetime
from uuid import UUID

import httpx
import pytest

from app.contracts import AssessmentStatus, CallState
from app.domains.communications.call_script import AVAILABILITY, build_script, extract
from app.integrations.photon import LivePhotonProvider, parse_inbound
from app.integrations.telnyx import LiveTelnyxProvider, decode_client_state
from app.repositories import registry as repos

OWNER_PHONE = "+15555550142"


def _project(client, h, body):
    pid = client.post("/api/v1/projects", json=body, headers=h).json()["project"]["project_id"]
    r = client.post(
        f"/api/v1/projects/{pid}/properties/search", json={"city": "Los Angeles", "state": "CA"}, headers=h
    )
    assert r.status_code == 202, r.text
    return pid


def _link_owner(client, h, container):
    r = client.put("/api/v1/owner/channel", json={"handle": "(555) 555-0142"}, headers=h)
    assert (
        r.json()["channel"]["status"] == "PENDING_FIRST_MESSAGE"
        and r.json()["channel"]["handle"] == OWNER_PHONE
    )
    container.photon.inject_inbound(OWNER_PHONE, "hi")
    assert client.post("/api/v1/owner/channel/poll", headers=h).json()["routed"] == 1
    ch = client.get("/api/v1/owner/channel", headers=h).json()["channel"]
    assert ch["status"] == "LINKED" and ch["space_id"] == "spc_fixture_owner"
    assert "You're connected" in container.photon.outbox[-1]["text"]


def _approved_simulated_call(client, h, pid, container):
    props = client.get(f"/api/v1/projects/{pid}/properties", headers=h).json()
    prop = next(p for p in props if "HOLLYWOOD" in p["property_identity"]["normalized_address"])
    out = client.post(
        f"/api/v1/projects/{pid}/outreach",
        headers=h,
        json={
            "property_id": prop["property_id"],
            "channel": "VOICE",
            "recipient": {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"},
            "purpose": "Verify electrical, plumbing and HVAC",
        },
    ).json()
    appr = out["approval"]
    client.post(
        f"/api/v1/approvals/{appr['approval_id']}/decision",
        headers=h,
        json={"decision": "APPROVED", "action_payload_hash": appr["action_payload_hash"]},
    )
    call = client.post(
        f"/api/v1/projects/{pid}/calls",
        json={"communication_id": out["communication"]["communication_id"]},
        headers=h,
    ).json()
    assert (
        call["simulated"] is True and call["state"] == "DIALING"
    )  # fixture voice -> always a labeled simulation
    return prop, out, call


def test_simulated_call_end_to_end_with_owner_join(client, auth, demo_project_body, container, db):
    h, _ = auth
    pid = _project(client, h, demo_project_body)
    _link_owner(client, h, container)
    prop, out, call = _approved_simulated_call(client, h, pid, container)
    assert any("approve in the app" in m["text"] for m in container.photon.outbox)  # link, not "reply YES"
    claims = [q["claim_type"] for q in call["script"]]
    assert "tenant_subpanel_amps" in claims and claims[-1] == "available_from"
    before = {
        a["requirement_id"].split(".", 1)[1]: a["status"]
        for a in client.get(f"/api/v1/projects/{pid}/properties/{prop['property_id']}", headers=h).json()[
            "infrastructure_assessments"
        ]
    }
    assert before["electrical.tenant_service"] == "UNKNOWN"

    r = client.post(
        f"/api/v1/projects/{pid}/calls/{call['call_id']}/simulate", json={"owner_action": "accept"}, headers=h
    )
    assert r.status_code == 200, r.text
    done = r.json()
    assert done["state"] == "COMPLETED" and done["simulated"]
    speakers = [t["speaker"] for t in done["transcript"]]
    assert speakers[0] == "assistant" and "owner" in speakers
    assert "automated assistant" in done["transcript"][0]["text"]  # disclosure on answer
    assert any(
        "can't agree to anything" in t["text"] for t in done["transcript"] if t["speaker"] == "assistant"
    )
    assert any(t["text"] == "[owner joined the call]" for t in done["transcript"])
    amps = next(f for f in done["findings"] if f["claim_type"] == "tenant_subpanel_amps")
    assert amps["value"] == 200 and amps["unit"] == "A"
    # Commands sent to the carrier (fixture): speak + transcription + hangup, and a conference with the owner leg
    cmds = [c["cmd"] for c in container.telephony.commands]
    assert "transcription_start" in cmds and cmds[-1] == "hangup" and container.telephony.conferences

    after = client.get(f"/api/v1/projects/{pid}/properties/{prop['property_id']}", headers=h).json()
    by = {a["requirement_id"].split(".", 1)[1]: a for a in after["infrastructure_assessments"]}
    assert by["electrical.tenant_service"]["status"] == "MEETS"  # 200A reported >= 100A required
    assert "broker-reported" in by["electrical.tenant_service"]["observed_value"]
    assert any("SIMULATED CALL" in lim for lim in by["electrical.tenant_service"]["limitations"])
    ev = [
        e
        for e in repos.evidence.list(db, property_id=UUID(prop["property_id"]))
        if e.provider == "broker_call"
    ]
    assert ev and all(e.data_origin.value == "FIXTURE_SYNTHETIC" for e in ev)

    comms = {
        c["communication_id"]: c
        for c in client.get(f"/api/v1/projects/{pid}/communications", headers=h).json()
    }
    outreach = comms[out["communication"]["communication_id"]]
    assert outreach["state"] == "COMPLETED" and outreach["structured_findings"]
    summary = container.photon.outbox[-1]["text"]
    assert summary.startswith("[Simulated call]") and "tenant_subpanel_amps: 200 A" in summary
    types = [
        e["event_type"] for e in client.get(f"/api/v1/projects/{pid}/events", headers=h).json()["events"]
    ]
    for t in (
        "call.finding_recorded",
        "call.owner_handoff_requested",
        "call.owner_joined",
        "outreach.completed",
        "infrastructure.evidence_updated",
        "property.ranking.updated",
        "communication.owner_notified",
    ):
        assert t in types, t


def test_owner_replies_later_over_photon(client, auth, demo_project_body, container, db):
    h, _ = auth
    pid = _project(client, h, demo_project_body)
    _link_owner(client, h, container)
    _, _, call = _approved_simulated_call(client, h, pid, container)
    from app.services.call_orchestration.simulator import CallSimulator

    sim = CallSimulator(container)
    db.expire_all()
    c = repos.calls.require(db, UUID(call["call_id"]))
    c = sim._leg_events(db, c, [("call.answered", {}), ("call.speak.ended", {})])
    c = sim._say_broker(db, c, "Before anything, can you commit to the deposit by Friday?")
    assert c.state == CallState.HANDOFF_PENDING
    assert "Reply JOIN" in container.photon.outbox[-1]["text"]
    db.commit()  # the API request below uses its own session
    container.photon.inject_inbound(OWNER_PHONE, "LATER")
    client.post("/api/v1/owner/channel/poll", headers=h)
    db.expire_all()
    c = repos.calls.require(db, c.call_id)
    assert c.state == CallState.AI_RESUMED and c.deferred_questions
    assert "follow up directly" in container.photon.outbox[-1]["text"]


def test_owner_commands_and_chat(client, auth, demo_project_body, container):
    h, _ = auth
    _project(client, h, demo_project_body)
    _link_owner(client, h, container)
    for text, expect in [
        ("STATUS", "Top pick:"),
        ("approve", "approvals happen in the app"),
        ("JOIN", "no call waiting"),
        ("Which property is best?", ""),
    ]:
        container.photon.inject_inbound(OWNER_PHONE, text)
        client.post("/api/v1/owner/channel/poll", headers=h)
        reply = container.photon.outbox[-1]
        assert reply["space_id"] == "spc_fixture_owner" and reply["sender_id"] == "usr_fixture_agent"
        assert expect in reply["text"]
    n = len(container.photon.outbox)
    container.photon.inject_inbound("+15555550999", "who are you?")  # unknown sender: never answered
    client.post("/api/v1/owner/channel/poll", headers=h)
    assert len(container.photon.outbox) == n
    container.photon.inject_inbound(OWNER_PHONE, "STOP")
    client.post("/api/v1/owner/channel/poll", headers=h)
    assert (
        client.get("/api/v1/owner/channel", headers=h).json()["channel"]["status"] == "PENDING_FIRST_MESSAGE"
    )


def test_unanswered_and_voicemail_calls(client, auth, demo_project_body, container, db):
    h, _ = auth
    pid = _project(client, h, demo_project_body)
    _, _, call = _approved_simulated_call(client, h, pid, container)
    from app.services.call_orchestration import CallOrchestrator
    from app.services.call_orchestration.simulator import CallSimulator

    db.expire_all()
    c = repos.calls.require(db, UUID(call["call_id"]))
    sim = CallSimulator(container)
    sim._leg_events(db, c, [("call.machine.detection.ended", {"result": "machine"})])
    assert (
        "follow up by email" in container.telephony.commands[-2]["text"]
        and container.telephony.commands[-1]["cmd"] == "hangup"
    )
    c = sim._leg_events(db, c, [("call.hangup", {"hangup_cause": "timeout"})])
    assert c.state == CallState.FAILED and "not answered" in c.failure_reason
    assert (
        CallOrchestrator(container).handle_event(
            db, {"data": {"event_type": "call.answered", "payload": {"call_control_id": "nope"}}}
        )
        is None
    )


def test_call_script_and_extraction():
    from app.contracts import InfrastructureAssessment

    def a(rid, st):
        return InfrastructureAssessment(
            project_id=UUID(int=1),
            property_id=UUID(int=2),
            requirement_id=rid,
            status=st,
            uncertainty="x",
            verification_questions=["?"],
        )

    script = build_script(
        [
            a("fs.electrical.tenant_service", AssessmentStatus.UNKNOWN),
            a("fs.space.min_area", AssessmentStatus.FAILS),
        ]
    )
    assert [q.claim_type for q in script] == ["tenant_subpanel_amps", "available_from"] and script[
        0
    ].requirement_id
    q = script[0]
    assert extract(q, "It's a 400 amp service, 3 phase").value == 400
    assert extract(q, "Not sure, maybe 200 amps? I'd have to check").value is None  # unsure -> no number
    plumb = build_script([a("fs.plumbing.treatment_sinks", AssessmentStatus.MAY_REQUIRE_WORK)])[0]
    assert extract(plumb, "Yes, there's a sink in back").value is True
    assert extract(plumb, "No, it's all open").value is False
    assert extract(AVAILABILITY, "January 1st").value == "January 1st"


def test_signed_intro_media_link(client, container):
    from app.core.security import sign_media_key

    key = container.storage.put("p1", "intro.mp3", b"ID3fake", "audio/mpeg")
    tok = sign_media_key(container.settings.auth_secret.get_secret_value(), key)
    r = client.get(f"/api/v1/media/public/{tok}")
    assert r.status_code == 200 and r.content == b"ID3fake"
    assert client.get(f"/api/v1/media/public/{tok[:-2]}xx").status_code == 401


def test_photon_and_telnyx_request_shapes():
    seen: list[httpx.Request] = []

    def photon_handler(req: httpx.Request) -> httpx.Response:
        seen.append(req)
        if req.method == "GET":
            assert req.url.params["direction"] == "received" and req.url.params["createdSince"].endswith("Z")
            return httpx.Response(200, json={"items": [], "nextPageToken": None})
        body = json.loads(req.content)
        assert req.headers["idempotency-key"] == "k1"
        assert body == {
            "spaceId": "spc1",
            "senderId": "usr_agent",
            "parentMessageId": "msg1",
            "content": {"type": "text", "format": "plain", "text": "hi"},
        }
        return httpx.Response(200, json={"id": "pho_msg_1", "status": "accepted"})

    ph = LivePhotonProvider(
        "pho_sk_test", "pho_prj_1", client=httpx.Client(transport=httpx.MockTransport(photon_handler))
    )
    assert ph.send_text("spc1", "usr_agent", "hi", "k1", "msg1").state == "accepted"
    assert ph.list_received(datetime(2026, 11, 14, tzinfo=UTC)) == ([], None)
    assert seen[0].url.path == "/v1/projects/pho_prj_1/messages"

    cmds: list[tuple[str, dict]] = []

    def telnyx_handler(req: httpx.Request) -> httpx.Response:
        cmds.append((req.url.path, json.loads(req.content)))
        if req.url.path == "/v2/calls":
            return httpx.Response(200, json={"data": {"call_control_id": "v3:abc"}})
        if req.url.path == "/v2/conferences":
            return httpx.Response(200, json={"data": {"id": "conf1"}})
        return httpx.Response(200, json={"data": {"result": "ok"}})

    tx = LiveTelnyxProvider(
        "KEY",
        "conn1",
        allow_outbound=True,
        client=httpx.Client(transport=httpx.MockTransport(telnyx_handler)),
    )
    assert (
        tx.dial(
            "+15555550100",
            "+15555550111",
            "https://x/webhooks/telnyx",
            "idem",
            {"call_id": "c1", "role": "counterparty"},
        ).call_control_id
        == "v3:abc"
    )
    tx.speak("v3:abc", "hello")
    tx.play("v3:abc", "https://x/a.mp3")
    tx.start_transcription("v3:abc")
    assert tx.create_conference("v3:abc", "call-1") == "conf1"
    tx.join_conference("conf1", "v3:owner")
    tx.hangup("v3:abc")
    paths = [p for p, _ in cmds]
    assert paths == [
        "/v2/calls",
        "/v2/calls/v3:abc/actions/speak",
        "/v2/calls/v3:abc/actions/playback_start",
        "/v2/calls/v3:abc/actions/transcription_start",
        "/v2/conferences",
        "/v2/conferences/conf1/actions/join",
        "/v2/calls/v3:abc/actions/hangup",
    ]
    dial = cmds[0][1]
    assert dial["connection_id"] == "conn1" and dial["answering_machine_detection"] == "detect"
    assert decode_client_state(dial["client_state"]) == {"call_id": "c1", "role": "counterparty"}
    assert cmds[1][1] == {"payload": "hello", "voice": "female", "language": "en-US"}
    blocked = LiveTelnyxProvider(
        "KEY",
        "conn1",
        allow_outbound=False,
        client=httpx.Client(transport=httpx.MockTransport(telnyx_handler)),
    )
    with pytest.raises(Exception, match="disabled"):
        blocked.dial("+1", "+1", "u", "k")


def test_parse_inbound_ignores_agent_and_other_events():
    msg = {
        "id": "m1",
        "platform": "imessage",
        "status": "received",
        "sentBy": {"id": "u1", "kind": "human", "handle": "+1"},
        "receivedBy": {"id": "a1", "kind": "agent"},
        "space": {"id": "s1"},
        "content": {"type": "text", "format": "plain", "text": "hey"},
    }
    assert parse_inbound({"id": "e1", "type": "message.received", "data": msg}).agent_user_id == "a1"
    assert parse_inbound({"id": "e2", "type": "message.updated", "data": msg}) is None
    agent_msg = {**msg, "sentBy": {"id": "a1", "kind": "agent"}}
    assert parse_inbound({"id": "e3", "type": "message.received", "data": agent_msg}) is None
    img = {**msg, "content": {"type": "attachment", "id": "att1"}}
    assert parse_inbound({"id": "e4", "type": "message.received", "data": img}).image_attachment_refs == [
        "att1"
    ]


def test_revise_and_change_save_design_words_without_chat(monkeypatch):
    """revise: and change: are saved and answered in inbound routing. They never reach run_chat."""
    from types import SimpleNamespace
    from unittest.mock import MagicMock
    from uuid import UUID

    from app.contracts import OwnerChannel, OwnerChannelStatus
    from app.domains.communications.inbound import route_owner_message
    from app.integrations.photon import InboundMessage

    owner_id = UUID("00000000-0000-0000-0000-000000000001")
    project_id = UUID("00000000-0000-0000-0000-000000000002")
    channel = OwnerChannel(
        owner_id=owner_id, handle="+15555550142", status=OwnerChannelStatus.LINKED
    )
    monkeypatch.setattr(
        "app.domains.communications.owner_channel.channel_for_handle", lambda db, handle: channel
    )
    monkeypatch.setattr(
        "app.domains.communications.owner_channel.link_from_inbound", lambda db, ch, msg: ch
    )
    monkeypatch.setattr(
        "app.domains.communications.owner_channel.latest_project_id", lambda db, oid: project_id
    )
    chat_calls: list[str] = []

    def fake_run_chat(ctx, message, settings, model=None, thread_id=None):
        chat_calls.append(message)
        return SimpleNamespace(reply="from chat")

    def refuse_revise(*args, **kwargs):
        raise AssertionError("DesignService.revise must not run from an owner text")

    monkeypatch.setattr("app.agents.supervisor.run_chat", fake_run_chat)
    monkeypatch.setattr("app.domains.design.service.DesignService.revise", refuse_revise)

    def inbound(text: str, message_id: str) -> InboundMessage:
        return InboundMessage(
            event_id=message_id,
            message_id=message_id,
            platform="imessage",
            sender_handle="+15555550142",
            sender_user_id="u1",
            agent_user_id="a1",
            agent_handle=None,
            space_id="spc1",
            space_kind="direct",
            text=text,
        )

    container = SimpleNamespace(settings=SimpleNamespace(public_web_url="http://localhost:3000"))
    db = MagicMock()

    handled, reply = route_owner_message(
        db, container, inbound("revise: move the reception to the front", "m-revise")
    )
    assert handled == "command"
    assert reply == (
        "Noted. I saved this design change: move the reception to the front. "
        "Layout edits still happen in the app."
    )
    assert chat_calls == []

    handled, reply = route_owner_message(
        db, container, inbound("CHANGE: add a second treatment room", "m-change")
    )
    assert handled == "command"
    assert reply == (
        "Noted. I saved this design change: add a second treatment room. "
        "Layout edits still happen in the app."
    )
    assert "AI" not in reply
    assert chat_calls == []

    saved = [call.args[0].data for call in db.add.call_args_list]
    assert [row["structured_findings"][0]["answer"] for row in saved] == [
        "move the reception to the front",
        "add a second treatment room",
    ]
    assert all(row["purpose"] == "design change" for row in saved)
    assert [row["provider_message_ids"] for row in saved] == [["m-revise"], ["m-change"]]

    handled, reply = route_owner_message(
        db, container, inbound("Can you change the reception later?", "m-chat")
    )
    assert handled == "chat" and reply == "from chat"
    assert chat_calls == ["Can you change the reception later?"]


def test_simulated_owner_text_endpoint(client, auth, demo_project_body):
    h, _ = auth
    _project(client, h, demo_project_body)
    assert (
        client.post("/api/v1/owner/channel/simulate-text", json={"text": "hi"}, headers=h).status_code == 409
    )
    client.put("/api/v1/owner/channel", json={"handle": OWNER_PHONE}, headers=h)
    r = client.post("/api/v1/owner/channel/simulate-text", json={"text": "hi"}, headers=h).json()
    assert r["routed"] and "connected" in r["reply"]
    r = client.post("/api/v1/owner/channel/simulate-text", json={"text": "status"}, headers=h).json()
    assert "Top pick" in r["reply"]
