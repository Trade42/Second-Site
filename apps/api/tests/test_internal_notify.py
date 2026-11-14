"""POST /internal/v1/owner/notify: the Rust service texts the owner only through this route, which calls the existing
notify_owner unchanged. Service-purpose tokens, idempotency (sequential and concurrent), validation, ownership."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import threading
import time
from uuid import UUID, uuid4

import pytest
from fastapi.testclient import TestClient
from sqlalchemy import create_engine, func, select, text

from app.api.routes.internal import _lock_key
from app.contracts import CommunicationSession, CommunicationState, EventType
from app.core.security import SERVICE_PURPOSE, issue_token
from app.db import models as m
from app.integrations.base import ProviderError, ProviderErrorKind

from .conftest import TEST_DB

URL = "/internal/v1/owner/notify"
OWNER_PHONE = "+15555550142"


def _secret(container) -> str:
    return container.settings.auth_secret.get_secret_value()


def _bearer(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def _service(container, owner_id, ttl_s: int = 60) -> dict:
    return _bearer(issue_token(_secret(container), UUID(str(owner_id)), ttl_s=ttl_s, purpose=SERVICE_PURPOSE))


def _owner(client, container, projects: int = 1):
    """A fresh owner: (user id, access headers, project ids)."""
    uid = uuid4()
    h = _bearer(issue_token(_secret(container), uid))
    pids = []
    for i in range(projects):
        r = client.post(
            "/api/v1/projects",
            json={"project_name": f"Notify test {i}", "market": {"city": "Los Angeles", "state": "CA"}},
            headers=h,
        )
        assert r.status_code == 201, r.text
        pids.append(r.json()["project"]["project_id"])
    return uid, h, pids


def _link(client, h, container) -> None:
    """Register the owner's handle and let them text the agent first (Photon lines cannot text first)."""
    r = client.put("/api/v1/owner/channel", json={"handle": "(555) 555-0142"}, headers=h)
    assert r.status_code == 200, r.text
    container.photon.inject_inbound(OWNER_PHONE, "hi")
    assert client.post("/api/v1/owner/channel/poll", headers=h).json()["routed"] == 1
    assert client.get("/api/v1/owner/channel", headers=h).json()["channel"]["status"] == "LINKED"


def _body(uid, pid, key="rust-key-1", msg="Your call summary is ready in the app.", **over) -> dict:
    return {
        "owner_id": str(uid),
        "project_id": str(pid),
        "text": msg,
        "purpose": "call_summary",
        "idempotency_key": key,
        **over,
    }


@pytest.fixture
def sends(container):
    """Counts every call to the provider's send_text (the fixture provider dedupes by key on its own)."""
    calls: list[dict] = []
    original = container.photon.send_text

    def counting(**kwargs):
        calls.append(kwargs)
        return original(**kwargs)

    container.photon.send_text = counting
    return calls


def _comms(db, pid) -> list[CommunicationSession]:
    rows = db.scalars(select(m.Communication).where(m.Communication.project_id == UUID(pid)))
    return [CommunicationSession.model_validate(r.data) for r in rows]


def _notify_events(db, key: str) -> int:
    return db.scalar(
        select(func.count())
        .select_from(m.ActivityEventRow)
        .where(
            m.ActivityEventRow.idempotency_key == f"notify:{key}",
            m.ActivityEventRow.event_type == EventType.COMMUNICATION_OWNER_NOTIFIED.value,
        )
    )


def test_linked_owner_is_texted_once_and_a_repeat_returns_the_same_answer(client, container, db, sends):
    uid, h, (pid,) = _owner(client, container)
    _link(client, h, container)
    sent_before = len(sends)  # linking already sent the 'connected' reply through send_text
    r = client.post(URL, json=_body(uid, pid), headers=_service(container, uid))
    assert r.status_code == 200, r.text
    out = r.json()
    assert set(out) == {"communication_id", "delivery"}
    assert out["delivery"] == "recorded"  # fixture provider; live Photon answers 'accepted' (not delivered)
    assert len(sends) == sent_before + 1
    assert sends[-1]["text"] == "Your call summary is ready in the app."
    assert sends[-1]["idempotency_key"] == "rust-key-1"
    assert container.photon.outbox[-1]["text"] == "Your call summary is ready in the app."
    comms = [c for c in _comms(db, pid) if c.purpose == "call_summary"]
    assert len(comms) == 1 and str(comms[0].communication_id) == out["communication_id"]
    assert comms[0].state == CommunicationState.COMPLETED and comms[0].authorized_owner_id == uid
    assert _notify_events(db, "rust-key-1") == 1

    # Same key again, even with another text: nothing is sent, the earlier answer comes back.
    again = client.post(URL, json=_body(uid, pid, msg="a different text"), headers=_service(container, uid))
    assert again.status_code == 200 and again.json() == out
    assert len(sends) == sent_before + 1
    assert len([c for c in _comms(db, pid) if c.purpose == "call_summary"]) == 1
    assert _notify_events(db, "rust-key-1") == 1

    # A new key is a new text.
    third = client.post(URL, json=_body(uid, pid, key="rust-key-2"), headers=_service(container, uid))
    assert third.status_code == 200 and third.json()["communication_id"] != out["communication_id"]
    assert len(sends) == sent_before + 2


def test_owner_without_a_linked_channel_gets_nothing_but_a_record(client, container, db, sends):
    uid, h, (pid,) = _owner(client, container)
    r = client.post(URL, json=_body(uid, pid, key="no-channel"), headers=_service(container, uid))
    assert r.status_code == 200 and r.json()["delivery"] == "not_linked", r.text
    # Registered but the owner has not texted the agent yet: still not linked, still nothing sent.
    client.put("/api/v1/owner/channel", json={"handle": "(555) 555-0142"}, headers=h)
    r2 = client.post(URL, json=_body(uid, pid, key="pending-channel"), headers=_service(container, uid))
    assert r2.status_code == 200 and r2.json()["delivery"] == "not_linked", r2.text
    assert sends == [] and container.photon.outbox == []
    comms = _comms(db, pid)
    assert len(comms) == 2 and all(c.state == CommunicationState.FAILED for c in comms)
    assert {str(c.communication_id) for c in comms} == {
        r.json()["communication_id"],
        r2.json()["communication_id"],
    }
    # The repeat of a not_linked key answers the same and does not try again.
    r3 = client.post(URL, json=_body(uid, pid, key="no-channel"), headers=_service(container, uid))
    assert r3.json() == r.json() and len(_comms(db, pid)) == 2


def test_provider_failure_is_an_answer_not_an_error(client, container, db):
    uid, h, (pid,) = _owner(client, container)
    _link(client, h, container)

    def down(**_kwargs):
        raise ProviderError("photon", ProviderErrorKind.UNAVAILABLE, "503 from provider")

    container.photon.send_text = down
    r = client.post(URL, json=_body(uid, pid, key="provider-down"), headers=_service(container, uid))
    assert r.status_code == 200 and r.json()["delivery"] == "failed", r.text
    comm = next(c for c in _comms(db, pid) if str(c.communication_id) == r.json()["communication_id"])
    assert comm.state == CommunicationState.FAILED


def test_two_concurrent_requests_with_one_key_text_once(client, container, db, sends):
    from app.main import app

    uid, h, (pid,) = _owner(client, container)
    _link(client, h, container)
    sent_before = len(sends)
    counting = container.photon.send_text
    entered, release = threading.Event(), threading.Event()

    def gated(**kwargs):  # the first sender holds the advisory lock here until the test releases it
        entered.set()
        assert release.wait(15)
        return counting(**kwargs)

    container.photon.send_text = gated
    body, headers = _body(uid, pid, key="concurrent-key"), _service(container, uid)
    results: dict[str, object] = {}

    def post(name: str) -> None:
        results[name] = TestClient(app).post(URL, json=body, headers=headers)

    first = threading.Thread(target=post, args=("first",))
    first.start()
    assert entered.wait(15), "first request never reached the provider"
    second = threading.Thread(target=post, args=("second",))
    second.start()

    # The second request must be waiting on the transaction-scoped advisory lock for this key.
    key = _lock_key("notify:concurrent-key") & 0xFFFFFFFFFFFFFFFF
    eng = create_engine(TEST_DB)
    waiting = 0
    try:
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline and not waiting:
            with eng.connect() as conn:
                waiting = conn.execute(
                    text(
                        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted"
                        " AND database = (SELECT oid FROM pg_database WHERE datname = current_database())"
                        " AND classid::bigint = :hi AND objid::bigint = :lo AND objsubid = 1"
                    ),
                    {"hi": key >> 32, "lo": key & 0xFFFFFFFF},
                ).scalar_one()
            if not waiting:
                time.sleep(0.05)
    finally:
        eng.dispose()
        release.set()
        first.join(30)
        second.join(30)
    assert waiting == 1, "second request did not wait on the advisory lock"

    a, b = results["first"], results["second"]
    assert a.status_code == 200 and b.status_code == 200, (a.text, b.text)
    assert a.json() == b.json() and a.json()["delivery"] == "recorded"
    assert len(sends) == sent_before + 1  # one text
    assert len([c for c in _comms(db, pid) if c.purpose == "call_summary"]) == 1
    assert _notify_events(db, "concurrent-key") == 1


def test_only_a_service_token_for_that_owner_is_accepted(client, container, db, sends):
    uid, h, (pid,) = _owner(client, container)
    body = _body(uid, pid, key="auth-key")
    secret = _secret(container)

    def status(headers) -> tuple[int, str]:
        r = client.post(URL, json=body, headers=headers)
        return r.status_code, r.json()["error_code"]

    assert status({}) == (401, "UNAUTHORIZED")
    assert status(h) == (401, "UNAUTHORIZED")  # an ordinary access token for the same owner
    assert status(_bearer("not-a-token")) == (401, "UNAUTHORIZED")
    assert status(_service(container, uid, ttl_s=-10)) == (401, "UNAUTHORIZED")  # expired
    assert status(_bearer(issue_token("another-secret", uid, purpose=SERVICE_PURPOSE))) == (
        401,
        "UNAUTHORIZED",
    )
    media = issue_token(secret, uid, purpose="media")
    assert status(_bearer(media)) == (401, "UNAUTHORIZED")
    # Validly signed body with a non-ASCII signature (a 500 in verify_token on the ordinary routes): still 401.
    enc = issue_token(secret, uid, purpose=SERVICE_PURPOSE).rsplit(".", 1)[0]
    r = client.post(URL, json=body, headers={"Authorization": f"Bearer {enc}.{'é' * 64}".encode()})
    assert r.status_code == 401, r.text
    # Signed garbage inside: non-integer exp.
    bad_body = f"{SERVICE_PURPOSE}.{uid}.soon"
    bad = base64.urlsafe_b64encode(bad_body.encode()).decode().rstrip("=")
    sig = hmac.new(secret.encode(), bad_body.encode(), hashlib.sha256).hexdigest()
    assert status(_bearer(f"{bad}.{sig}")) == (401, "UNAUTHORIZED")

    other = uuid4()
    assert status(_service(container, other)) == (403, "FORBIDDEN")  # token for another owner
    assert sends == [] and _comms(db, pid) == []

    # A service token is not an access token on the ordinary API.
    assert client.get("/api/v1/projects", headers=_service(container, uid)).status_code == 401
    assert client.get(f"/api/v1/projects/{pid}", headers=_service(container, uid)).status_code == 401
    assert client.get("/api/v1/owner/channel", headers=_service(container, uid)).status_code == 401
    assert client.get("/api/v1/projects", headers=h).status_code == 200


def test_project_must_exist_and_belong_to_the_owner(client, container, db, sends):
    uid, _h, (pid,) = _owner(client, container)
    other_uid, _oh, (other_pid,) = _owner(client, container)
    r = client.post(URL, json=_body(uid, other_pid, key="foreign"), headers=_service(container, uid))
    assert r.status_code == 404 and r.json()["error_code"] == "NOT_FOUND", r.text
    r = client.post(URL, json=_body(uid, uuid4(), key="unknown"), headers=_service(container, uid))
    assert r.status_code == 404, r.text
    assert sends == [] and _comms(db, other_pid) == [] and _comms(db, pid) == []


def test_a_key_used_for_another_project_is_a_conflict(client, container, db):
    uid, _h, (p1, p2) = _owner(client, container, projects=2)
    first = client.post(URL, json=_body(uid, p1, key="shared"), headers=_service(container, uid))
    assert first.status_code == 200, first.text
    r = client.post(URL, json=_body(uid, p2, key="shared"), headers=_service(container, uid))
    assert r.status_code == 409 and r.json()["error_code"] == "IDEMPOTENCY_KEY_REUSED", r.text
    assert _comms(db, p2) == []


@pytest.mark.parametrize(
    "override",
    [
        {"text": "x" * 1501},
        {"text": ""},
        {"text": "   "},
        {"purpose": ""},
        {"purpose": " "},
        {"purpose": "p" * 101},
        {"idempotency_key": ""},
        {"idempotency_key": " \t"},
        {"idempotency_key": "k" * 201},
        {"owner_id": "not-a-uuid"},
        {"unexpected": "field"},
    ],
    ids=lambda o: next(iter(o)) + ":" + str(len(str(next(iter(o.values()))))),
)
def test_invalid_bodies_are_refused_and_nothing_is_sent(client, container, db, sends, override):
    uid, _h, (pid,) = _owner(client, container)
    r = client.post(URL, json=_body(uid, pid, **override), headers=_service(container, uid))
    assert r.status_code == 422 and r.json()["error_code"] == "VALIDATION_ERROR", r.text
    assert sends == [] and _comms(db, pid) == []


def test_longest_allowed_text_is_accepted(client, container, db):
    uid, _h, (pid,) = _owner(client, container)
    r = client.post(
        URL, json=_body(uid, pid, msg="x" * 1500, key="k" * 200), headers=_service(container, uid)
    )
    assert r.status_code == 200 and r.json()["delivery"] == "not_linked", r.text


def test_internal_route_is_not_in_the_public_contract(client):
    from app.main import app

    from .conftest import API_DIR

    assert not any(p.startswith("/internal") for p in app.openapi()["paths"])
    published = json.loads((API_DIR.parents[1] / "packages" / "contracts" / "openapi.json").read_text())
    assert not any(p.startswith("/internal") for p in published["paths"])
