"""Wire- and row-parity vectors for the call flow (server part 4): drive the Python app (FastAPI
TestClient, fixture telephony, fixture Photon, PostgreSQL) through outreach, the approval
decision, call starts, the simulator with and without a handoff, Telnyx webhooks for both legs,
the handoff response and the list routes, and save every response, the project's events and the
call-flow rows.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_comms_slice.py)
Needs: the sandbox PostgreSQL (Unix socket /tmp/ss-pg). The script creates its own schema
       `wc_<uuid>` inside `secondsite_test`, migrates it with alembic, and drops it at the end; it
       never touches the `public` schema the Python test suite owns. (Self-contained on purpose:
       gen_property_slice.py is being extended by another writer.)

Output: comms_slice.json = {"owner_channel": ..., "steps": [...], "rows": {...}}.

Each step is {name, method, path, body_text, auth, kind, status, response, capture}:
  - `path` and `body_text` hold symbolic references `{name}` that both sides resolve against
    their own ids. `body_text` is the exact JSON text Python sent (key order matters: the
    outreach approval key embeds Python's `str(recipient)`), with the references unresolved;
    `{cs:<ref>:<role>}` is the base64 `client_state` of that call id and role.
  - `kind` "webhook": the body is a Telnyx event, signed by the replay with the test key
    (Ed25519 seed bytes([7] * 32), TELNYX_PUBLIC_KEY below) at the time it is sent;
    "webhook_bad_signature": the same, signed with another key (seed bytes([9] * 32)).
  - `capture`: {ref: dotted path into the response} records references for later steps
    (`call.participant_states[role=owner].leg_id` selects by a field).
The owner channel: Python links it through Photon (PUT /owner/channel, an inbound "hi", a poll),
before the project exists so that Photon writes no project event; those routes stay in the
Python service, so they are not steps. `owner_channel` is the linked
channel document; the Rust replay stores the same row before the first step, as Python's Photon
code would.

What the Rust replay excludes, and why: the communication rows and the
`communication.owner_notified` events that Python's `notify_owner` writes inline. In the Rust
service those are written by the Python service itself (`POST /internal/v1/owner/notify`), so
the Rust replay answers that route with a recording fake and Rust writes neither.
"""

from __future__ import annotations

import base64
import contextlib
import json
import os
import re
import subprocess
import sys
import tempfile
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[3]
API = REPO / "apps" / "api"
sys.path.insert(0, str(API))

DSN = "host=/tmp/ss-pg user=secondsite dbname=secondsite_test"
DATABASE_URL = "postgresql+psycopg://secondsite:secondsite@/secondsite_test?host=/tmp/ss-pg"
OWNER_PHONE = "+15555550142"
SEED = bytes([7] * 32)
BAD_SEED = bytes([9] * 32)


@contextlib.contextmanager
def private_schema():
    import psycopg

    name = f"wc_{uuid.uuid4().hex}"
    with psycopg.connect(DSN, autocommit=True) as c:
        c.execute(f"CREATE SCHEMA {name}")
    env = {**os.environ, "DATABASE_URL": DATABASE_URL, "PGOPTIONS": f"-c search_path={name}",
           "ALEMBIC_SKIP_LOGGING": "1"}
    try:
        subprocess.run([sys.executable, "-m", "alembic", "upgrade", "head"], cwd=API, env=env,
                       check=True, capture_output=True)
        os.environ["DATABASE_URL"] = DATABASE_URL
        os.environ["PGOPTIONS"] = env["PGOPTIONS"]
        yield name
    finally:
        with psycopg.connect(DSN, autocommit=True) as c:
            c.execute(f"DROP SCHEMA {name} CASCADE")


def public_key_b64() -> str:
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

    pub = Ed25519PrivateKey.from_private_bytes(SEED).public_key()
    return base64.b64encode(pub.public_bytes(Encoding.Raw, PublicFormat.Raw)).decode()


def configure(objects_dir: str) -> None:
    # The settings of apps/api/tests/conftest.py and of the Rust test harness.
    os.environ.update({"APP_ENV": "test", "AUTH_SECRET": "test-secret", "SUPERVISOR_MODEL": "",
                       "BACKEND_API_URL": "", "OBJECT_STORAGE_LOCAL_DIR": objects_dir,
                       "TELNYX_PUBLIC_KEY": public_key_b64()})
    for m in ("PROPERTY", "PERMIT", "MATTERPORT", "VEXCEL", "MESSAGING", "VOICE", "PROCUREMENT"):
        os.environ[f"{m}_PROVIDER_MODE"] = "fixture"


def sign(body: bytes, seed: bytes) -> dict[str, str]:
    import time

    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

    ts = str(int(time.time()))
    sig = Ed25519PrivateKey.from_private_bytes(seed).sign(f"{ts}|".encode() + body)
    return {"telnyx-signature-ed25519": base64.b64encode(sig).decode(), "telnyx-timestamp": ts,
            "content-type": "application/json"}


def dig(doc, path: str):
    """`a.b[k=v].c` into a JSON document."""
    cur = doc
    for part in path.split("."):
        m = re.fullmatch(r"(\w+)\[(\w+)=(\w+)\]", part)
        if m:
            cur = next(x for x in cur[m.group(1)] if x[m.group(2)] == m.group(3))
        elif part.isdigit():
            cur = cur[int(part)]
        else:
            cur = cur[part]
    return cur


def main() -> None:
    fx = json.loads((REPO / "fixtures" / "projects" / "facial_studio.json").read_text())
    steps: list[dict] = []
    rows: dict[str, list] = {}
    owner_channel = None
    with tempfile.TemporaryDirectory() as objects, private_schema():
        configure(objects)
        from fastapi.testclient import TestClient

        from app.core.container import get_container
        from app.integrations.telnyx import encode_client_state
        from app.main import app

        refs: dict[str, str] = {}

        def resolve(text: str) -> str:
            def cs(m):
                return encode_client_state({"call_id": refs[m.group(1)], "role": m.group(2)})

            text = re.sub(r"\{cs:(\w+):(\w+)\}", cs, text)
            for k, v in refs.items():
                text = text.replace("{" + k + "}", v)
            assert not re.search(r"\{[a-z][a-z0-9_:]*\}", text), text
            return text

        with TestClient(app, raise_server_exceptions=False) as c:
            headers: dict[str, str] = {}

            def step(name, method, path, body=None, auth=True, kind="http", capture=None):
                body_text = json.dumps(body) if body is not None else None
                url = resolve(path)
                sent = resolve(body_text).encode() if body_text is not None else None
                if kind.startswith("webhook"):
                    seed = BAD_SEED if kind == "webhook_bad_signature" else SEED
                    r = c.post(url, content=sent, headers=sign(sent, seed))
                else:
                    h = dict(headers) if auth else {}
                    if sent is not None:
                        h["content-type"] = "application/json"
                    r = c.request(method, url, content=sent, headers=h)
                resp = r.json() if r.content else None
                for ref, p in (capture or {}).items():
                    refs[ref] = str(dig(resp, p))
                steps.append({"name": name, "method": method, "path": path, "body_text": body_text,
                              "auth": auth, "kind": kind, "status": r.status_code, "response": resp,
                              "capture": capture or {}})
                return resp

            tok = c.post("/api/v1/dev/token").json()
            headers["authorization"] = f"Bearer {tok['access_token']}"
            # Link the owner's channel through Photon (Python-only routes; not replayed). Before
            # the project exists, so Photon's inbound handling emits no project event.
            cont = get_container()
            c.put("/api/v1/owner/channel", json={"handle": OWNER_PHONE}, headers=headers)
            cont.photon.inject_inbound(OWNER_PHONE, "hi")
            assert c.post("/api/v1/owner/channel/poll", headers=headers).json()["routed"] == 1
            owner_channel = c.get("/api/v1/owner/channel", headers=headers).json()["channel"]
            assert owner_channel["status"] == "LINKED"
            step("create project", "POST", "/api/v1/projects",
                 {"project_name": fx["project_name"], "market": fx["market"],
                  "business_profile": fx["business_profile"]},
                 capture={"project": "project.project_id"})
            step("search", "POST", "/api/v1/projects/{project}/properties/search",
                 {"city": "Los Angeles", "state": "CA"})
            props = step("properties", "GET", "/api/v1/projects/{project}/properties")
            streets = [p["property_identity"]["normalized_address"] for p in props]
            refs["hollywood"] = next(p["property_id"] for p, s in zip(props, streets) if "HOLLYWOOD" in s)
            refs["other_property"] = next(p["property_id"] for p, s in zip(props, streets)
                                          if "HOLLYWOOD" not in s)

            recipient = {"role": "broker", "name": "Dana Broker", "phone": "+15555550100"}
            # 1. Accept: the owner joins through the simulator.
            step("outreach 1", "POST", "/api/v1/projects/{project}/outreach",
                 {"property_id": "{hollywood}", "channel": "VOICE", "recipient": recipient,
                  "purpose": "Verify electrical, plumbing and HVAC"},
                 capture={"comm1": "communication.communication_id",
                          "approval1": "approval.approval_id", "hash1": "approval.action_payload_hash"})
            step("communications after outreach", "GET", "/api/v1/projects/{project}/communications")
            step("call before approval", "POST", "/api/v1/projects/{project}/calls",
                 {"communication_id": "{comm1}"})
            step("approve 1", "POST", "/api/v1/approvals/{approval1}/decision",
                 {"decision": "APPROVED", "action_payload_hash": "{hash1}"})
            step("call 1", "POST", "/api/v1/projects/{project}/calls", {"communication_id": "{comm1}"},
                 capture={"call1": "call_id"})
            step("call 1 again", "POST", "/api/v1/projects/{project}/calls",
                 {"communication_id": "{comm1}"})
            step("simulate 1 accept", "POST", "/api/v1/projects/{project}/calls/{call1}/simulate",
                 {"owner_action": "accept"})
            step("hollywood after call 1", "GET", "/api/v1/projects/{project}/properties/{hollywood}")
            # 2. Later, with an intro recording and other answers.
            step("outreach 2", "POST", "/api/v1/projects/{project}/outreach",
                 {"property_id": "{other_property}", "channel": "VOICE",
                  "recipient": {"name": "Sam Lister", "role": "listing agent", "phone": "+15555550101"},
                  "purpose": "Availability", "intro_recording_reference": "obj/owner-intro.m4a"},
                 capture={"comm2": "communication.communication_id",
                          "approval2": "approval.approval_id", "hash2": "approval.action_payload_hash"})
            step("approve 2", "POST", "/api/v1/approvals/{approval2}/decision",
                 {"decision": "APPROVED", "action_payload_hash": "{hash2}"})
            step("call 2", "POST", "/api/v1/projects/{project}/calls", {"communication_id": "{comm2}"},
                 capture={"call2": "call_id"})
            step("simulate 2 later", "POST", "/api/v1/projects/{project}/calls/{call2}/simulate",
                 {"owner_action": "later",
                  "answers": {"usable_area_sqft": "Not sure, I'd have to check the plans."}})
            # 3. No response: the handoff expires.
            step("outreach 3", "POST", "/api/v1/projects/{project}/outreach",
                 {"property_id": "{hollywood}", "channel": "VOICE",
                  "recipient": {"role": "broker", "name": "Lee", "phone": "+15555550102"},
                  "purpose": "Second opinion"},
                 capture={"comm3": "communication.communication_id",
                          "approval3": "approval.approval_id", "hash3": "approval.action_payload_hash"})
            step("approve 3", "POST", "/api/v1/approvals/{approval3}/decision",
                 {"decision": "APPROVED", "action_payload_hash": "{hash3}"})
            step("call 3", "POST", "/api/v1/projects/{project}/calls", {"communication_id": "{comm3}"},
                 capture={"call3": "call_id"})
            step("simulate 3 none", "POST", "/api/v1/projects/{project}/calls/{call3}/simulate",
                 {"owner_action": "none"})
            # 4. Carrier webhooks for both legs, and the web handoff response.
            step("outreach 4", "POST", "/api/v1/projects/{project}/outreach",
                 {"property_id": "{other_property}", "channel": "VOICE",
                  "recipient": {"role": "broker", "name": "Kim", "phone": "+15555550103"},
                  "purpose": "Electrical"},
                 capture={"comm4": "communication.communication_id",
                          "approval4": "approval.approval_id", "hash4": "approval.action_payload_hash"})
            step("approve 4", "POST", "/api/v1/approvals/{approval4}/decision",
                 {"decision": "APPROVED", "action_payload_hash": "{hash4}"})
            step("call 4", "POST", "/api/v1/projects/{project}/calls", {"communication_id": "{comm4}"},
                 capture={"call4": "call_id", "leg4": "provider_call_id"})

            def wh(name, eid, et, payload, kind="webhook"):
                step(name, "POST", "/webhooks/telnyx",
                     {"data": {"record_type": "event", "id": eid, "event_type": et, "payload": payload}},
                     auth=False, kind=kind)

            broker = "{cs:call4:counterparty}"
            wh("wh speak before answer", "w1", "call.speak.ended",
               {"call_control_id": "{leg4}", "client_state": broker})
            wh("wh answered", "w2", "call.answered", {"call_control_id": "{leg4}", "client_state": broker})
            wh("wh answered replay", "w2", "call.answered",
               {"call_control_id": "{leg4}", "client_state": broker})
            wh("wh bad signature", "w3", "call.hangup",
               {"call_control_id": "{leg4}", "client_state": broker}, kind="webhook_bad_signature")
            wh("wh speak ended", "w4", "call.speak.ended",
               {"call_control_id": "{leg4}", "client_state": broker})
            wh("wh answer 1", "w5", "call.transcription",
               {"call_control_id": "{leg4}", "transcription_data":
                {"transcript": "It has a 100 amp subpanel.", "is_final": True, "confidence": 0.9}})
            wh("wh partial", "w6", "call.transcription",
               {"call_control_id": "{leg4}", "transcription_data": {"transcript": "Will", "is_final": False}})
            wh("wh escalation", "w7", "call.transcription",
               {"call_control_id": "{leg4}", "transcription_data":
                {"transcript": "Will you need a guarantor?", "is_final": True}})
            step("calls during handoff", "GET", "/api/v1/projects/{project}/calls")
            step("handoffs", "GET", "/api/v1/projects/{project}/handoffs",
                 capture={"handoff4": "3.handoff_request_id"})
            step("handoff accept", "POST", "/api/v1/projects/{project}/handoffs/{handoff4}/response",
                 {"response": "ACCEPTED"},
                 capture={"owner_leg4": "participant_states[role=owner].leg_id"})
            step("handoff accept again", "POST",
                 "/api/v1/projects/{project}/handoffs/{handoff4}/response", {"response": "DECLINED"})
            owner = "{cs:call4:owner}"
            wh("wh owner answered", "w8", "call.answered",
               {"call_control_id": "{owner_leg4}", "client_state": owner})
            wh("wh owner joined", "w9", "conference.participant.joined",
               {"call_control_id": "{owner_leg4}", "client_state": owner})
            wh("wh broker to owner", "w10", "call.transcription",
               {"call_control_id": "{leg4}", "transcription_data":
                {"transcript": "Great, let's talk terms.", "is_final": True}})
            wh("wh owner left", "w11", "call.hangup",
               {"call_control_id": "{owner_leg4}", "client_state": owner, "hangup_cause": "normal_clearing"})
            wh("wh answer 2", "w12", "call.transcription",
               {"call_control_id": "{leg4}", "transcription_data":
                {"transcript": "December first.", "is_final": True}})
            wh("wh hangup", "w13", "call.hangup",
               {"call_control_id": "{leg4}", "client_state": broker, "hangup_cause": "normal_clearing",
                "hangup_source": "callee"})
            wh("wh unknown call", "w14", "call.answered", {"call_control_id": "nope"})
            # Errors and lists.
            unknown = "00000000-0000-4000-8000-000000000001"
            refs["unknown"] = unknown
            step("simulate unknown call", "POST", "/api/v1/projects/{project}/calls/{unknown}/simulate", {})
            step("call unknown communication", "POST", "/api/v1/projects/{project}/calls",
                 {"communication_id": "{unknown}"})
            step("handoff unknown", "POST", "/api/v1/projects/{project}/handoffs/{unknown}/response",
                 {"response": "DEFERRED"})
            step("outreach bad channel", "POST", "/api/v1/projects/{project}/outreach",
                 {"property_id": "{hollywood}", "channel": "PIGEON", "recipient": {}, "purpose": "x"})
            step("handoff bad response", "POST",
                 "/api/v1/projects/{project}/handoffs/{handoff4}/response", {"response": "MAYBE"})
            step("calls", "GET", "/api/v1/projects/{project}/calls")
            step("handoffs final", "GET", "/api/v1/projects/{project}/handoffs")
            step("communications", "GET", "/api/v1/projects/{project}/communications")
            step("approvals", "GET", "/api/v1/projects/{project}/approvals")
            step("hollywood final", "GET", "/api/v1/projects/{project}/properties/{hollywood}")
            step("events", "GET", "/api/v1/projects/{project}/events?limit=500")
            step("no token", "GET", "/api/v1/projects/{project}/calls", auth=False)

        # The call-flow rows Python stored.
        import psycopg

        schema = os.environ["PGOPTIONS"].split("=", 1)[1]
        with psycopg.connect(DSN, autocommit=True) as conn:
            conn.execute(f"SET search_path={schema}")
            q = {
                "communications": "SELECT id, project_id, channel, state, data, record_version FROM communications ORDER BY created_at, id",
                "call_sessions": "SELECT id, project_id, provider_call_id, state, data, record_version FROM call_sessions ORDER BY created_at, id",
                "owner_handoff_requests": "SELECT id, project_id, call_id, status, data, record_version FROM owner_handoff_requests ORDER BY created_at, id",
                "processed_webhook_events": "SELECT provider, provider_event_id, verified, payload FROM processed_webhook_events ORDER BY created_at, provider_event_id",
            }
            for table, sql in q.items():
                cur = conn.execute(sql)
                cols = [d.name for d in cur.description]
                rows[table] = [{k: (str(v) if isinstance(v, uuid.UUID) else v) for k, v in zip(cols, r)}
                               for r in cur.fetchall()]

    out = {"owner_channel": owner_channel, "steps": steps, "rows": rows}
    (HERE / "comms_slice.json").write_text(json.dumps(out, indent=1, sort_keys=False) + "\n")
    print(f"{len(steps)} steps, rows: {{{', '.join(f'{k}: {len(v)}' for k, v in rows.items())}}}")


if __name__ == "__main__":
    main()
