"""The voice session route (`POST /api/v1/projects/{id}/voice/session`, app/agents/voice_session.py).

The mint is stubbed with an httpx.MockTransport that records the request it gets; no test reaches xAI.
"""

from __future__ import annotations

import json
import logging
import re
from datetime import UTC, datetime, timedelta
from uuid import UUID, uuid4

import httpx
import pytest
from pydantic import SecretStr

from app.agents import codex_chat as cc
from app.agents import voice_session as vs
from app.agents.tools import ToolContext, build_tools
from app.contracts import ProjectContext
from app.core.security import issue_token
from app.db import models as m
from app.repositories import registry as repos

from .helpers import demo_profile

FAKE_KEY = "xai-test-key-never-real-0123456789"
SECRET = "ephemeral-secret-value-0123456789"


def _norm(s: str) -> str:
    return " ".join(s.split())


@pytest.fixture
def minted(monkeypatch, container):
    """Configure a fake key and answer the mint; returns the list of requests the provider got."""
    seen: list[httpx.Request] = []
    expires = int(datetime.now(UTC).timestamp()) + vs.SECRET_SECONDS

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, json={"value": SECRET, "expires_at": expires})

    monkeypatch.setattr(container.settings, "xai_api_key", SecretStr(FAKE_KEY))
    monkeypatch.setattr(vs, "_TRANSPORT", httpx.MockTransport(handler))
    return seen


def _answer(monkeypatch, container, handler):
    monkeypatch.setattr(container.settings, "xai_api_key", SecretStr(FAKE_KEY))
    monkeypatch.setattr(vs, "_TRANSPORT", httpx.MockTransport(handler))


@pytest.fixture
def ranked_project(client, db, container):
    """A project with a ranking (fixture data), owned by a fresh user; returns (headers, project_id)."""
    owner = uuid4()
    db.add(m.User(id=owner, display_name="o"))
    db.flush()
    p = ProjectContext(owner_id=owner, project_name="voice", market={"city": "Los Angeles", "state": "CA"})
    repos.projects.add(db, p)
    repos.profiles.add(db, demo_profile().model_copy(update={"project_id": p.project_id}))
    ctx = ToolContext(db=db, c=container, project_id=p.project_id, owner_id=owner)
    {t.name: t for t in build_tools(ctx)}["run_property_analysis"].invoke({})
    db.commit()
    token = issue_token(container.settings.auth_secret.get_secret_value(), owner)
    return {"Authorization": f"Bearer {token}"}, p.project_id


def _url(pid) -> str:
    return f"/api/v1/projects/{pid}/voice/session"


# ---- the rules -------------------------------------------------------------------------------------------------
FORM_RULES = ("Cite property ids", "Reply in plain text")


def _bullets(rules: str) -> list[str]:
    return [_norm(b) for b in rules.split("\n- ")[1:]]


def test_every_truth_rule_of_the_text_assistant_is_in_the_voice_rules():
    truth = [b for b in _bullets(cc.SYSTEM_RULES) if not b.startswith(FORM_RULES)]
    assert len(truth) == 6, truth  # answer from facts, UNKNOWN, permits/evidence, money, fixture, cannot act
    voice = _norm(vs.VOICE_RULES)
    for rule in truth:
        assert rule in voice, rule
    assert _norm(cc.SYSTEM_RULES.split("\n")[0]) in voice  # the same identity line
    # Only the form rules change: no citing ids aloud, short spoken turns.
    assert "Cite property ids" not in voice and "200 words" not in voice
    assert "at most about 60 words a turn" in voice and "never by reading out an id" in voice
    assert "No tables, no lists" in voice


def test_session_update_has_no_tools_and_the_facts():
    facts = {"data_notice": "fixture", "money_unit": "integer US cents", "top_properties": []}
    ev = vs.session_update(vs.build_instructions(facts))
    assert ev["type"] == "session.update"
    s = ev["session"]
    assert s["tools"] == []
    text = json.dumps(ev)
    for tool in ("web_search", "x_search", "file_search", "mcp", '"function"'):
        assert tool not in text, tool
    assert s["instructions"].startswith(vs.VOICE_RULES + "\n\nFACTS (JSON):\n")
    assert json.loads(s["instructions"].split("FACTS (JSON):\n", 1)[1]) == facts
    assert s["audio"]["input"]["format"] == {"type": "audio/pcm", "rate": 24000}
    assert s["audio"]["output"]["format"] == {"type": "audio/pcm", "rate": 24000}
    assert s["turn_detection"] == {"type": "server_vad"}


def test_disclosure_is_spoken_verbatim_first():
    ev = vs.disclosure_event()
    assert ev["item"]["type"] == "force_message" and ev["item"]["interruptible"] is False
    assert ev["item"]["content"] == [{"type": "output_text", "text": vs.DISCLOSURE}]
    for part in (
        "automated",
        "not a person",
        "this project's records",
        "cannot contact anyone",
        "buy anything",
    ):
        assert part in vs.DISCLOSURE


def test_claim_pattern_is_the_text_assistants_and_portable():
    assert vs._CLAIM_RE is cc._CLAIM_RE
    src = cc._CLAIM_RE.pattern
    for python_only in ("(?P", "(?i", "(?#", "\\A", "\\Z", "(?<"):
        assert python_only not in src
    assert re.compile(src, re.IGNORECASE).search("Sure, I'VE EMAILED the broker")


def test_facts_past_the_limit_are_cut():
    big = {"data_notice": "x", "top_properties": [{"evidence": "e" * (cc.MAX_FACTS_CHARS + 10)}]}
    out = vs.bound_facts(big)
    assert out["truncated"] is True and isinstance(out["top_properties"], str)
    assert len(json.dumps(out)) < cc.MAX_FACTS_CHARS
    small = {"data_notice": "x"}
    assert vs.bound_facts(small) is small


# ---- the route ------------------------------------------------------------------------------------------------
def test_owner_gets_one_grounded_session(client, ranked_project, minted, caplog):
    caplog.set_level(logging.DEBUG)
    h, pid = ranked_project
    r = client.post(_url(pid), json={"mode": "assistant"}, headers=h)
    assert r.status_code == 200, r.text
    assert r.headers["cache-control"] == "no-store"
    b = r.json()
    assert set(b) == {
        "mode", "provider", "websocket_url", "client_secret", "expires_at", "session_update", "disclosure",
        "disclosure_event", "label", "no_action_notice", "claim_pattern", "data_notice", "grounded_record_ids",
    }  # fmt: skip
    assert b["mode"] == "assistant" and b["provider"] == "xai"
    assert b["websocket_url"] == "wss://api.x.ai/v1/realtime?model=grok-voice-latest"
    assert b["client_secret"] == SECRET
    assert datetime.fromisoformat(b["expires_at"]) <= datetime.now(UTC) + timedelta(
        seconds=vs.SECRET_SECONDS + 1
    )
    assert b["label"] == (
        "Voice assistant: automated, answers from this project's records, cannot contact anyone or buy anything."
    )
    assert b["no_action_notice"] == cc.NO_ACTION_NOTICE
    assert b["claim_pattern"] == cc._CLAIM_RE.pattern
    assert b["disclosure"] == vs.DISCLOSURE and b["disclosure_event"] == vs.disclosure_event()
    assert "fixture (demo) data" in b["data_notice"]
    session = b["session_update"]["session"]
    assert session["tools"] == []
    facts = json.loads(session["instructions"].split("FACTS (JSON):\n", 1)[1])
    assert facts["data_notice"] == b["data_notice"]
    assert facts["ranking"].get("ranked"), "the fixture project has a ranking"
    ids = {UUID(i) for i in b["grounded_record_ids"]}
    assert pid in ids and UUID(facts["ranking"]["ranking_id"]) in ids
    assert {UUID(p["property_id"]) for p in facts["top_properties"]} <= ids
    # The one request to the provider: the mint, with the key in the Authorization header and nowhere else.
    assert len(minted) == 1
    req = minted[0]
    assert req.method == "POST" and str(req.url) == "https://api.x.ai/v1/realtime/client_secrets"
    assert req.headers["authorization"] == f"Bearer {FAKE_KEY}"
    assert json.loads(req.content) == {"expires_after": {"seconds": 120}}
    assert FAKE_KEY not in str(req.url) and FAKE_KEY.encode() not in req.content
    assert [k for k, v in req.headers.items() if FAKE_KEY in v] == ["authorization"]
    # Neither the key nor the secret is logged; nor is the project's data.
    logged = "\n".join(rec.getMessage() for rec in caplog.records)
    assert FAKE_KEY not in logged and SECRET not in logged and "FACTS" not in logged
    assert FAKE_KEY not in r.text


def test_other_modes_are_refused(client, ranked_project, minted):
    h, pid = ranked_project
    for body in ({"mode": "rehearsal"}, {}, {"mode": "assistant", "extra": 1}, {"mode": "ASSISTANT"}):
        r = client.post(_url(pid), json=body, headers=h)
        assert r.status_code == 422, (body, r.text)
    assert minted == []


def test_only_the_owner(client, ranked_project, minted, container):
    _h, pid = ranked_project
    assert client.post(_url(pid), json={"mode": "assistant"}).status_code == 401
    stranger = issue_token(container.settings.auth_secret.get_secret_value(), uuid4())
    r = client.post(_url(pid), json={"mode": "assistant"}, headers={"Authorization": f"Bearer {stranger}"})
    assert r.status_code in (403, 404), r.text
    assert SECRET not in r.text
    assert minted == []


def test_no_key_is_503(client, ranked_project, monkeypatch, container):
    h, pid = ranked_project
    calls = []
    monkeypatch.setattr(container.settings, "xai_api_key", None)
    monkeypatch.setattr(vs, "_TRANSPORT", httpx.MockTransport(lambda req: calls.append(req)))
    r = client.post(_url(pid), json={"mode": "assistant"}, headers=h)
    assert r.status_code == 503 and r.json()["error_code"] == "VOICE_NOT_CONFIGURED"
    assert r.json()["retryable"] is False and calls == []
    monkeypatch.setattr(container.settings, "xai_api_key", SecretStr("  "))
    assert client.post(_url(pid), json={"mode": "assistant"}, headers=h).status_code == 503


@pytest.mark.parametrize(
    ("status", "body", "retryable"),
    [
        (429, b'{"error":"PROVIDER-BODY-MARKER"}', True),
        (500, b"PROVIDER-BODY-MARKER", True),
        (503, b"PROVIDER-BODY-MARKER", True),
        (401, b'{"error":"PROVIDER-BODY-MARKER bad key"}', False),
        (400, b"PROVIDER-BODY-MARKER", False),
        (200, b"not json PROVIDER-BODY-MARKER", False),
        (200, b'{"value":"","expires_at":1}', False),
        (200, b'{"value":"PROVIDER-BODY-MARKER"}', False),
        (200, b'{"value":"PROVIDER-BODY-MARKER","expires_at":"soon"}', False),
    ],
)
def test_failed_mint_is_502(client, ranked_project, monkeypatch, container, caplog, status, body, retryable):
    caplog.set_level(logging.DEBUG)
    h, pid = ranked_project
    _answer(
        monkeypatch,
        container,
        lambda req: httpx.Response(status, content=body, headers={"x-provider": "HEADER-MARKER"}),
    )
    r = client.post(_url(pid), json={"mode": "assistant"}, headers=h)
    assert r.status_code == 502, r.text
    p = r.json()
    assert p["error_code"] == "PROVIDER_UNAVAILABLE" and p["retryable"] is retryable
    assert {"error_code", "message", "details", "request_id", "retryable"} == set(p)
    logged = "\n".join(rec.getMessage() for rec in caplog.records)
    for marker in ("PROVIDER-BODY-MARKER", "HEADER-MARKER", FAKE_KEY):
        assert marker not in r.text and marker not in logged


@pytest.mark.parametrize(
    ("exc", "kind"), [(httpx.ReadTimeout("t"), "timeout"), (httpx.ConnectError("c"), "network")]
)
def test_timeout_and_network_errors_are_retryable(client, ranked_project, monkeypatch, container, exc, kind):
    h, pid = ranked_project

    def handler(req):
        raise exc

    _answer(monkeypatch, container, handler)
    r = client.post(_url(pid), json={"mode": "assistant"}, headers=h)
    assert r.status_code == 502 and r.json()["retryable"] is True
    assert r.json()["details"] == {"provider": "xai", "kind": kind}


def test_fixture_instruction_size(client, ranked_project, minted):
    h, pid = ranked_project
    b = client.post(_url(pid), json={"mode": "assistant"}, headers=h).json()
    n = len(b["session_update"]["session"]["instructions"])
    print(f"fixture instructions chars={n}")
    assert len(vs.VOICE_RULES) < n < len(vs.VOICE_RULES) + cc.MAX_FACTS_CHARS + 20
