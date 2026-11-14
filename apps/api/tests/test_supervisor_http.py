"""Supervisor tools in HTTP mode (BACKEND_API_URL set): each tool calls the Rust API as the owner and returns the same
JSON text as in-process. httpx.MockTransport stands in for the Rust service. The approval guard stays as strict:
the only outreach call creates a PENDING approval. Rust errors reach the model as short honest tool errors."""

from __future__ import annotations

import base64
import json
import re
import time
from datetime import UTC, datetime, timedelta
from types import SimpleNamespace
from uuid import UUID, uuid4

import httpx
import pytest
from langchain_core.language_models.fake_chat_models import GenericFakeChatModel
from langchain_core.messages import AIMessage

from app.agents import backend_client as bc
from app.agents.supervisor import DeterministicSupervisor, create_supervisor
from app.agents.tools import TOOL_CATEGORIES, ToolContext, build_tools
from app.contracts import (
    ActionType,
    ApprovalRequest,
    ApprovalStatus,
    Channel,
    CommunicationSession,
    CommunicationState,
    ProjectContext,
    RiskCategory,
    Subsystem,
    TaskAccepted,
    TaskRecord,
    TaskStatus,
    TaskType,
)
from app.core.config import Settings
from app.core.security import verify_token
from app.domains.evidence.validator import validate_property
from app.repositories import registry as repos
from app.services.authorization import ToolCategory

from .helpers import analyze_in_memory, demo_profile

RUST = "http://rust.test"
SECRET = "test-secret"
ALL_TOOLS = {
    "get_project_state",
    "run_property_analysis",
    "get_ranking",
    "get_feasibility",
    "get_property_evidence",
    "get_cost_scenarios",
    "validate_evidence",
    "request_outreach_approval",
}


def _http_settings(url: str | None = RUST) -> Settings:
    return Settings(BACKEND_API_URL=url, AUTH_SECRET=SECRET)


def _http_ctx(transport, project_id: UUID, owner_id: UUID, url: str | None = RUST) -> ToolContext:
    # No database and no services: HTTP mode must only use the settings and the transport.
    return ToolContext(
        db=None,
        c=SimpleNamespace(settings=_http_settings(url)),
        project_id=project_id,
        owner_id=owner_id,
        backend_transport=transport,
    )


def _j(obj) -> str:
    return json.dumps(obj, default=str)


def _problem(status: int, code: str, message: str, retryable: bool = False) -> httpx.Response:
    return httpx.Response(
        status,
        json={
            "error_code": code,
            "message": message,
            "details": {},
            "request_id": "req-1",
            "retryable": retryable,
        },
    )


def _assert_owner_token(request: httpx.Request, owner: UUID) -> None:
    auth = request.headers["authorization"]
    assert auth.startswith("Bearer ")
    token = auth.split(" ", 1)[1]
    assert verify_token(SECRET, token) == owner  # an ordinary access token for the owner
    enc = token.rsplit(".", 1)[0]
    purpose, _user, exp = base64.urlsafe_b64decode(enc + "=" * (-len(enc) % 4)).decode().split(".")
    assert purpose == "access" and 0 < int(exp) - time.time() <= bc.TOKEN_TTL_S + 1  # short-lived


class FakeRust:
    """Canned Rust API built from real contract objects (a pure in-memory analysis). Records every request."""

    def __init__(self):
        self.owner = uuid4()
        profile = demo_profile()
        self.project = ProjectContext(
            project_id=profile.project_id,
            owner_id=self.owner,
            project_name="Facial studio",
            market={"city": "Los Angeles", "state": "CA"},
            business_profile_id=profile.profile_id,
        )
        self.profile = profile
        by_addr, scenarios, assessments, _work, evidence, ranking = analyze_in_memory(profile)
        self.props = list(by_addr.values())
        self.pid = self.props[0].property_id
        self.assessments = assessments[self.pid]
        self.evidence = evidence[self.pid]
        self.scenarios = list(scenarios.values())
        older = ranking.model_copy(update={"ranking_id": uuid4(), "ranking_version": 1})
        self.ranking = ranking.model_copy(update={"ranking_version": 2})
        self.rankings = [older, self.ranking]
        self.task = TaskRecord(
            project_id=self.project.project_id,
            task_type=TaskType.ANALYZE_PROPERTY,
            assigned_subsystem=Subsystem.AGENTS_FINANCE,
            status=TaskStatus.COMPLETED,
            attempt_count=1,
            output_reference={
                "property_ids": [str(p.property_id) for p in self.props],
                "ranking_id": str(self.ranking.ranking_id),
                "ranking_version": 2,
            },
        )
        self.approval = ApprovalRequest(
            project_id=self.project.project_id,
            requested_by_agent="outreach",
            action_type=ActionType.PLACE_CALL,
            action_payload={"project_id": str(self.project.project_id), "property_id": str(self.pid)},
            risk_category=RiskCategory.EXTERNAL_COMMUNICATION,
            summary="VOICE to Dana Broker: panel size?",
            expires_at=datetime.now(UTC) + timedelta(hours=24),
            authorized_owner_id=self.owner,
            idempotency_key="outreach:x",
        )
        self.problems = ["evidence e1 is HISTORICAL but no limitation says so"]
        self.requests: list[httpx.Request] = []
        self.overrides: dict[tuple[str, str], object] = {}

    @property
    def base(self) -> str:
        return f"/api/v1/projects/{self.project.project_id}"

    def routes(self) -> dict[tuple[str, str], object]:
        b, p = self.base, f"{self.base}/properties/{self.pid}"
        communication = CommunicationSession(
            project_id=self.project.project_id,
            property_id=self.pid,
            channel=Channel.VOICE,
            external_participants=[{"name": "Dana Broker", "phone": "+15555550100"}],
            authorized_owner_id=self.owner,
            purpose="panel size?",
            state=CommunicationState.AWAITING_APPROVAL,
            approval_id=self.approval.approval_id,
        )
        return {
            ("GET", b): {
                "project": self.project.model_dump(mode="json"),
                "business_profile": self.profile.model_dump(mode="json"),
            },
            ("POST", f"{b}/properties/search"): TaskAccepted(
                task=self.task, status_url=f"{b}/tasks/{self.task.task_id}"
            ).model_dump(mode="json"),
            ("GET", f"{b}/rankings"): [r.model_dump(mode="json") for r in self.rankings],
            ("GET", f"{p}/feasibility"): [a.model_dump(mode="json") for a in self.assessments],
            ("GET", f"{p}/evidence"): [e.model_dump(mode="json") for e in self.evidence],
            ("GET", f"{p}/evidence/validation"): {"problems": self.problems},
            ("GET", f"{b}/cost-scenarios"): [s.model_dump(mode="json") for s in self.scenarios],
            ("POST", f"{b}/outreach"): {
                "communication": communication.model_dump(mode="json"),
                "approval": self.approval.model_dump(mode="json"),
            },
        }

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        key = (request.method, request.url.path)
        if key in self.overrides:
            override = self.overrides[key]
            return override(request) if callable(override) else override
        routes = self.routes()
        if key in routes:
            return httpx.Response(202 if request.method == "POST" else 200, json=routes[key])
        return _problem(404, "NOT_FOUND", "property not found")

    def ctx(self) -> ToolContext:
        return _http_ctx(httpx.MockTransport(self), self.project.project_id, self.owner)


@pytest.fixture
def rust() -> FakeRust:
    return FakeRust()


def test_each_tool_calls_its_rust_route_as_the_owner_and_returns_the_in_process_text(rust):
    ctx = rust.ctx()
    tools = {t.name: t for t in build_tools(ctx)}
    assert set(tools) == ALL_TOOLS
    pid = str(rust.pid)
    b, p = rust.base, f"{rust.base}/properties/{pid}"

    def call(name: str, args: dict, method: str, path: str) -> tuple[str, httpx.Request]:
        before = len(rust.requests)
        out = tools[name].invoke(args)
        assert len(rust.requests) == before + 1, name
        req = rust.requests[-1]
        assert (req.method, req.url.path) == (method, path), name
        assert str(req.url).startswith(RUST)
        _assert_owner_token(req, rust.owner)
        return out, req

    out, _ = call("get_project_state", {}, "GET", b)
    assert out == _j(
        {
            "project": rust.project.model_dump(mode="json"),
            "business_profile": rust.profile.model_dump(mode="json"),
        }
    )

    out, req = call("run_property_analysis", {}, "POST", f"{b}/properties/search")
    assert json.loads(req.content) == {"city": "Los Angeles", "state": "CA"}
    assert req.extensions["timeout"]["read"] == bc.SEARCH_TIMEOUT.read == 120.0
    assert out == _j(rust.task.output_reference)

    out, req = call("get_ranking", {}, "GET", f"{b}/rankings")
    assert req.extensions["timeout"]["read"] == bc.DEFAULT_TIMEOUT.read == 20.0
    r = rust.ranking  # the latest version, not the first
    assert out == _j(
        {
            "ranking_id": r.ranking_id,
            "ranked": [x.model_dump(mode="json") for x in r.ranked_properties],
            "explanations": r.explanations,
        }
    )

    out, _ = call("get_feasibility", {"property_id": pid}, "GET", f"{p}/feasibility")
    assert out == _j(
        [
            {
                "requirement": a.requirement_id,
                "status": a.status.value,
                "observed": a.observed_value,
                "questions": a.verification_questions,
                "evidence_ids": a.evidence_ids,
            }
            for a in rust.assessments
        ]
    )
    assert any(a["status"] == "UNKNOWN" for a in json.loads(out))  # UNKNOWN stays UNKNOWN

    out, _ = call("get_property_evidence", {"property_id": pid}, "GET", f"{p}/evidence")
    assert out == _j(
        [
            {
                "id": e.evidence_id,
                "type": e.observation_type,
                "value": e.extracted_value,
                "unit": e.unit,
                "class": e.evidence_classification.value,
                "source": e.source_record_id,
                "limitations": e.limitations[:2],
            }
            for e in rust.evidence
        ]
    )
    assert json.loads(out), "fixture property has permit evidence"

    out, _ = call("get_cost_scenarios", {}, "GET", f"{b}/cost-scenarios")
    assert out == _j(
        [
            {
                "property_id": s.property_id,
                "opening_expected": s.opening_cost_expected,
                "monthly": s.monthly_operating_cost,
                "long_term": s.long_term_cost,
                "horizon": s.comparison_horizon_months,
            }
            for s in rust.scenarios
        ]
    )

    out, _ = call("validate_evidence", {"property_id": pid}, "GET", f"{p}/evidence/validation")
    assert out == _j({"problems": rust.problems})

    args = {
        "property_id": pid,
        "recipient_name": "Dana Broker",
        "recipient_phone": "+15555550100",
        "questions": "panel size?",
    }
    out, req = call("request_outreach_approval", args, "POST", f"{b}/outreach")
    assert json.loads(req.content) == {
        "property_id": pid,
        "channel": "VOICE",
        "recipient": {"name": "Dana Broker", "phone": "+15555550100"},
        "purpose": "panel size?",
        "questions": ["panel size?"],
    }
    assert out == _j(
        {
            "status": "APPROVAL_REQUIRED",
            "approval_id": rust.approval.approval_id,
            "message": "Owner approval requested; nothing was sent.",
        }
    )

    # Exactly the eight tool routes were called; nothing that executes, decides or sends.
    assert len(rust.requests) == 8
    assert not any(re.search(r"/calls|/decision|/handoffs|/purchase", str(r.url)) for r in rust.requests)
    assert rust.pid in ctx.touched_records and rust.ranking.ranking_id in ctx.touched_records
    assert rust.approval.approval_id in ctx.touched_records
    assert TOOL_CATEGORIES["request_outreach_approval"] == ToolCategory.EXTERNAL_COMMUNICATION


def test_problem_detail_is_a_short_honest_tool_error(rust):
    tools = {t.name: t for t in build_tools(rust.ctx())}
    other = uuid4()  # not served by the fake: Rust answers 404 ProblemDetail
    out = json.loads(tools["get_feasibility"].invoke({"property_id": str(other)}))
    assert out == {
        "status": "ERROR",
        "error_code": "NOT_FOUND",
        "message": "property not found",
        "http_status": 404,
        "retryable": False,
        "request_id": "req-1",
    }
    rust.overrides[("GET", f"{rust.base}/rankings")] = _problem(
        503, "PROVIDER_UNAVAILABLE", "store unavailable", retryable=True
    )
    out = json.loads(tools["get_ranking"].invoke({}))
    assert out["status"] == "ERROR" and out["http_status"] == 503 and out["retryable"] is True
    assert out != {"status": "NO_RANKING"}  # an error is never an empty success
    rust.overrides[("GET", f"{rust.base}/cost-scenarios")] = httpx.Response(
        502, text="<html>bad gateway</html>"
    )
    out = json.loads(tools["get_cost_scenarios"].invoke({}))
    assert out["error_code"] == "HTTP_502" and out["status"] == "ERROR"


def test_timeouts_and_unreachable_service_are_bounded_errors(rust):
    tools = {t.name: t for t in build_tools(rust.ctx())}

    def timeout(request):
        raise httpx.ReadTimeout("timed out", request=request)

    rust.overrides[("GET", f"{rust.base}/cost-scenarios")] = timeout
    out = json.loads(tools["get_cost_scenarios"].invoke({}))
    assert out["error_code"] == "BACKEND_TIMEOUT" and out["retryable"] is True and out["http_status"] is None
    rust.overrides[("POST", f"{rust.base}/outreach")] = timeout
    args = {"property_id": str(rust.pid), "recipient_name": "B", "recipient_phone": "+1", "questions": "q"}
    out = json.loads(tools["request_outreach_approval"].invoke(args))
    assert out["error_code"] == "BACKEND_TIMEOUT" and "unknown" in out["message"]  # never "requested"

    def refused(request):
        raise httpx.ConnectError("connection refused", request=request)

    rust.overrides[("GET", rust.base)] = refused
    out = json.loads(tools["get_project_state"].invoke({}))
    assert out["error_code"] == "BACKEND_UNAVAILABLE" and out["status"] == "ERROR"
    assert all(
        t is not None for t in (bc.DEFAULT_TIMEOUT.read, bc.DEFAULT_TIMEOUT.connect, bc.SEARCH_TIMEOUT.read)
    )


def test_failed_task_and_contract_drift_are_errors_not_results(rust):
    tools = {t.name: t for t in build_tools(rust.ctx())}
    failed = rust.task.model_copy(
        update={
            "status": TaskStatus.FAILED,
            "output_reference": {},
            "error_code": "ConflictError",
            "error_message": "no candidate properties found",
        }
    )
    rust.overrides[("POST", f"{rust.base}/properties/search")] = httpx.Response(
        202, json=TaskAccepted(task=failed, status_url="/x").model_dump(mode="json")
    )
    out = json.loads(tools["run_property_analysis"].invoke({}))
    assert out["status"] == "ERROR" and out["error_code"] == "ConflictError"
    assert "no candidate properties found" in out["message"]
    rust.overrides[("GET", f"{rust.base}/rankings")] = httpx.Response(200, json=[{"ranking_id": "x"}])
    out = json.loads(tools["get_ranking"].invoke({}))
    assert out["error_code"] == "BACKEND_CONTRACT_MISMATCH"
    rust.overrides[("GET", f"{rust.base}/properties/{rust.pid}/evidence/validation")] = httpx.Response(
        200, json={"ok": True}
    )
    out = json.loads(tools["validate_evidence"].invoke({"property_id": str(rust.pid)}))
    assert out["error_code"] == "BACKEND_CONTRACT_MISMATCH"
    # A real empty list is an honest empty answer.
    rust.overrides[("GET", f"{rust.base}/rankings")] = httpx.Response(200, json=[])
    assert json.loads(tools["get_ranking"].invoke({})) == {"status": "NO_RANKING"}


def test_outreach_guard_reports_reused_approvals_and_checks_ownership(rust):
    tools = {t.name: t for t in build_tools(rust.ctx())}
    args = {"property_id": str(rust.pid), "recipient_name": "B", "recipient_phone": "+1", "questions": "q"}
    routes = rust.routes()
    body = routes[("POST", f"{rust.base}/outreach")]
    approved = {**body, "approval": {**body["approval"], "approval_status": "APPROVED"}}
    rust.overrides[("POST", f"{rust.base}/outreach")] = httpx.Response(202, json=approved)
    out = json.loads(tools["request_outreach_approval"].invoke(args))
    assert out["status"] == "APPROVAL_REQUIRED" and "APPROVED; nothing was sent" in out["message"]
    foreign = {**body, "approval": {**body["approval"], "authorized_owner_id": str(uuid4())}}
    rust.overrides[("POST", f"{rust.base}/outreach")] = httpx.Response(202, json=foreign)
    out = json.loads(tools["request_outreach_approval"].invoke(args))
    assert out["error_code"] == "BACKEND_CONTRACT_MISMATCH"


def test_ids_are_checked_and_only_tool_routes_can_be_reached(rust):
    tools = {t.name: t for t in build_tools(rust.ctx())}
    for name, args in [
        ("get_feasibility", {"property_id": "../../calls"}),
        ("get_property_evidence", {"property_id": "p"}),
        ("validate_evidence", {"property_id": ""}),
        (
            "request_outreach_approval",
            {"property_id": "p", "recipient_name": "B", "recipient_phone": "+1", "questions": "q"},
        ),
    ]:
        out = json.loads(tools[name].invoke(args))
        assert out["error_code"] == "INVALID_ARGUMENT", name
    assert rust.requests == []  # nothing was sent for a bad id

    client = bc.BackendClient(RUST, SECRET, rust.owner, transport=httpx.MockTransport(rust))
    project = rust.project.project_id
    for method, path in [
        ("POST", f"/api/v1/projects/{project}/calls"),
        ("POST", f"/api/v1/approvals/{uuid4()}/decision"),
        ("POST", f"/api/v1/projects/{project}/handoffs"),
        ("GET", f"/api/v1/projects/{project}/properties/{rust.pid}/feasibility/../../calls"),
        ("DELETE", f"/api/v1/projects/{project}"),
    ]:
        with pytest.raises(bc.RouteNotAllowed):
            client._send(method, path)
    assert rust.requests == []
    with pytest.raises(ValueError):
        bc.BackendClient("localhost:8000", SECRET, rust.owner)


def test_unset_or_blank_url_means_in_process_mode(rust):
    assert bc.backend_client(_http_settings(None), rust.owner) is None
    assert bc.backend_client(_http_settings(""), rust.owner) is None
    assert bc.backend_client(_http_settings(RUST + "/"), rust.owner).base_url == RUST


class ScriptedToolModel(GenericFakeChatModel):
    def bind_tools(self, tools, **kwargs):
        return self


def test_deep_agent_sees_the_rust_error_as_a_tool_error_and_continues(rust):
    from langgraph.checkpoint.memory import InMemorySaver

    missing = str(uuid4())
    script = iter(
        [
            AIMessage(
                content="",
                tool_calls=[{"name": "get_feasibility", "args": {"property_id": missing}, "id": "c1"}],
            ),
            AIMessage(content="", tool_calls=[{"name": "get_ranking", "args": {}, "id": "c2"}]),
            AIMessage(content="That property is not in this project; here is the ranking."),
        ]
    )
    agent = create_supervisor(rust.ctx(), ScriptedToolModel(messages=script), InMemorySaver())
    res = agent.invoke(
        {"messages": [{"role": "user", "content": "Is it feasible?"}]},
        config={"configurable": {"thread_id": "http-1"}},
    )
    tool_msgs = [mm for mm in res["messages"] if mm.type == "tool"]
    assert [t.name for t in tool_msgs] == ["get_feasibility", "get_ranking"]
    assert tool_msgs[0].status == "error" and json.loads(tool_msgs[0].content)["error_code"] == "NOT_FOUND"
    assert tool_msgs[1].status == "success" and json.loads(tool_msgs[1].content)["ranking_id"]


# ---- the same tools against the Python API itself (same HTTP surface the Rust API mirrors) ---------------------


def _python_api_transport(client, db, container, project_id: str, seen: list) -> httpx.MockTransport:
    """Forwards to the FastAPI app (TestClient). The two evidence routes the Python app does not have are answered
    from the database the way the Rust service is adding them."""
    evidence_route = re.compile(
        rf"^/api/v1/projects/{project_id}/properties/([0-9a-f-]{{36}})/evidence(/validation)?$"
    )

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        m = evidence_route.match(request.url.path)
        if m and request.method == "GET":
            prop = repos.properties.get(db, UUID(m.group(1)))
            if prop is None or str(prop.project_id) != project_id:
                return _problem(404, "NOT_FOUND", "property not found")
            ev = container.evidence.for_property(db, prop.property_id)
            if m.group(2):
                return httpx.Response(
                    200, json={"problems": validate_property(prop, ev, prop.infrastructure_assessments)}
                )
            return httpx.Response(200, json=[e.model_dump(mode="json") for e in ev])
        headers = {k: v for k, v in request.headers.items() if k in ("authorization", "content-type")}
        r = client.request(request.method, request.url.path, headers=headers, content=request.content)
        return httpx.Response(
            r.status_code, headers={"content-type": r.headers["content-type"]}, content=r.content
        )

    return httpx.MockTransport(handler)


def test_http_tools_return_the_same_text_as_in_process_on_real_data(client, container, db, demo_project_body):
    from app.core.security import issue_token

    owner = uuid4()
    h = {"Authorization": f"Bearer {issue_token(SECRET, owner)}"}
    r = client.post("/api/v1/projects", json=demo_project_body, headers=h)
    assert r.status_code == 201, r.text
    project_id = r.json()["project"]["project_id"]
    seen: list[httpx.Request] = []
    http_ctx = _http_ctx(
        _python_api_transport(client, db, container, project_id, seen), UUID(project_id), owner
    )
    local_ctx = ToolContext(db=db, c=container, project_id=UUID(project_id), owner_id=owner)
    remote = {t.name: t for t in build_tools(http_ctx)}

    run = json.loads(remote["run_property_analysis"].invoke({}))
    assert set(run) == {"property_ids", "ranking_id", "ranking_version"} and run["property_ids"]
    assert (seen[-1].method, seen[-1].url.path) == (
        "POST",
        f"/api/v1/projects/{project_id}/properties/search",
    )
    db.expire_all()
    local = {t.name: t for t in build_tools(local_ctx)}
    assert not any(t.handle_tool_error for t in local.values())  # in-process tools unchanged
    assert all(t.handle_tool_error for t in remote.values())

    assert remote["get_project_state"].invoke({}) == local["get_project_state"].invoke({})
    assert remote["get_ranking"].invoke({}) == local["get_ranking"].invoke({})
    assert remote["get_cost_scenarios"].invoke({}) == local["get_cost_scenarios"].invoke({})
    for pid in run["property_ids"]:
        args = {"property_id": pid}
        # In-process reads the assessments embedded in the property; HTTP reads the feasibility route.
        assert remote["get_feasibility"].invoke(args) == local["get_feasibility"].invoke(args), pid
        assert remote["get_property_evidence"].invoke(args) == local["get_property_evidence"].invoke(args), (
            pid
        )
        assert remote["validate_evidence"].invoke(args) == local["validate_evidence"].invoke(args), pid
    for req in seen:
        _assert_owner_token(req, owner)

    # Outreach in HTTP mode: a PENDING approval through the outreach route; nothing is called or texted.
    sent_before = len(container.photon.outbox)
    args = {
        "property_id": run["property_ids"][0],
        "recipient_name": "Dana Broker",
        "recipient_phone": "+15555550100",
        "questions": "What is the electrical panel size?",
    }
    out = json.loads(remote["request_outreach_approval"].invoke(args))
    assert (
        out["status"] == "APPROVAL_REQUIRED"
        and out["message"] == "Owner approval requested; nothing was sent."
    )
    db.expire_all()
    appr = repos.approvals.get(db, UUID(out["approval_id"]))
    assert appr.approval_status == ApprovalStatus.PENDING and appr.action_type == ActionType.PLACE_CALL
    assert appr.authorized_owner_id == owner and str(appr.project_id) == project_id
    assert repos.calls.list(db, project_id=UUID(project_id)) == []
    assert len(container.photon.outbox) == sent_before  # owner channel not linked: nothing texted

    # The model-free supervisor runs on the HTTP tools too.
    reply = DeterministicSupervisor(http_ctx).chat("rank the properties")
    assert reply.mode == "deterministic" and reply.reply.startswith("Ranking (deterministic engine):")
    assert not any(re.search(r"/calls|/decision", r.url.path) for r in seen)
