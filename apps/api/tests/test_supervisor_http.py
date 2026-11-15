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


def test_allow_list_matches_the_whole_path(rust):
    # `re.match` with `$` also accepts a trailing newline; the allow-list must match the path exactly.
    client = bc.BackendClient(RUST, SECRET, rust.owner, transport=httpx.MockTransport(rust))
    for path in (f"{rust.base}\n", f"{rust.base}/rankings\n"):
        with pytest.raises(bc.RouteNotAllowed):
            client._send("GET", path)
    assert rust.requests == []


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


# ---- the model-free supervisor on a failing Rust API (R6) ----------------------------------------------------


def _assert_honest_failure(reply, tool: str, *texts: str) -> None:
    """Not a ranking, not a project status: a plain statement that the backend failed, with the tool's error text."""
    assert reply.mode == "deterministic"
    assert "Ranking (deterministic engine)" not in reply.reply and "#1:" not in reply.reply
    assert "is in status" not in reply.reply
    assert reply.reply.startswith("No ranking to show:") or reply.reply.startswith(
        "No project status to show:"
    )
    assert tool in reply.reply
    for text in texts:
        assert text in reply.reply, reply.reply


def test_model_free_supervisor_answers_a_rust_error_honestly(rust):
    rust.overrides[("GET", f"{rust.base}/rankings")] = _problem(
        503, "PROVIDER_UNAVAILABLE", "store unavailable", retryable=True
    )
    reply = DeterministicSupervisor(rust.ctx()).chat("rank the properties")
    _assert_honest_failure(
        reply,
        "get_ranking",
        "could not be reached or returned an error",
        "PROVIDER_UNAVAILABLE",
        "store unavailable",
    )

    def refused(request):
        raise httpx.ConnectError("connection refused", request=request)

    rust.overrides[("GET", rust.base)] = refused
    reply = DeterministicSupervisor(rust.ctx()).chat("what's the status?")
    _assert_honest_failure(reply, "get_project_state", "could not be reached", "BACKEND_UNAVAILABLE")
    assert reply.reply.startswith("No project status to show:")


def test_model_free_supervisor_reports_a_failed_analysis_and_a_missing_ranking(rust):
    rust.overrides[("GET", f"{rust.base}/rankings")] = httpx.Response(200, json=[])  # no ranking yet
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
    reply = DeterministicSupervisor(rust.ctx()).chat("find properties")
    _assert_honest_failure(reply, "run_property_analysis", "ConflictError", "no candidate properties found")
    assert "analysis ran" not in reply.reply

    # The search completes but Rust still lists no ranking: an unusable answer, not an empty ranking.
    del rust.overrides[("POST", f"{rust.base}/properties/search")]
    ctx = rust.ctx()
    reply = DeterministicSupervisor(ctx).chat("find properties")
    _assert_honest_failure(reply, "get_ranking", "NO_RANKING", "The property analysis ran")
    assert [r.method for r in rust.requests[-3:]] == ["GET", "POST", "GET"]
    assert reply.grounded_record_ids == ctx.touched_records  # the properties the analysis returned


class _Canned:
    """Stands in for one tool: always answers with this text."""

    def __init__(self, text: str):
        self.text = text

    def invoke(self, _args):
        return self.text


@pytest.mark.parametrize(
    ("prompt", "tool", "text", "expect"),
    [
        ("rank the properties", "get_ranking", "<html>bad gateway</html>", "not JSON"),
        ("rank the properties", "get_ranking", "[]", "not a JSON object"),
        ("rank the properties", "get_ranking", '{"ranking_id": "r1"}', "no ranked properties"),
        (
            "rank the properties",
            "get_ranking",
            '{"ranking_id": "r1", "ranked": [{"rank": 1, "property_id": "p1"}], "explanations": {}}',
            "rank or explanation",
        ),
        (
            "rank the properties",
            "get_ranking",
            '{"status": "ERROR"}',
            "could not be reached or returned an error",
        ),
        ("what's the status?", "get_project_state", "null", "not a JSON object"),
        ("what's the status?", "get_project_state", '{"project": {"project_name": "x"}}', "project_status"),
        ("what's the status?", "get_project_state", '{"business_profile": null}', "project_status"),
    ],
    ids=["not-json", "list", "no-ranked", "no-explanation", "bare-error", "null", "no-status", "no-project"],
)
def test_model_free_supervisor_never_crashes_on_an_unusable_tool_answer(rust, prompt, tool, text, expect):
    sup = DeterministicSupervisor(rust.ctx())
    sup.tools[tool] = _Canned(text)
    reply = sup.chat(prompt)
    _assert_honest_failure(reply, tool, expect)
    assert rust.requests == []  # the canned tool replaced the only call this prompt makes


@pytest.fixture
def local_http():
    """Real HTTP servers on ephemeral 127.0.0.1 ports. `start(answer)` returns (origin, hits); `answer(method, path)`
    gives (status, json body). Each hit records method, request target and headers."""
    import threading
    from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

    servers = []

    def start(answer):
        hits: list[tuple[str, str, dict[str, str]]] = []

        class Handler(BaseHTTPRequestHandler):
            def _any(self):
                length = int(self.headers.get("Content-Length") or 0)
                if length:
                    self.rfile.read(length)
                hits.append((self.command, self.path, {k.lower(): v for k, v in self.headers.items()}))
                status, body = answer(self.command, self.path)
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = _any

            def log_message(self, *_args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        servers.append((server, thread))
        return f"http://127.0.0.1:{server.server_port}", hits

    yield start
    for server, thread in servers:
        server.shutdown()
        server.server_close()
        thread.join(5)


def test_chat_route_answers_a_rust_outage_with_an_honest_reply(client, container, local_http, monkeypatch):
    from app.core.security import issue_token

    unavailable = {
        "error_code": "PROVIDER_UNAVAILABLE",
        "message": "database unavailable",
        "details": {},
        "request_id": "req-9",
        "retryable": True,
    }
    rust_url, hits = local_http(lambda _m, _p: (503, unavailable))
    owner = uuid4()
    h = {"Authorization": f"Bearer {issue_token(container.settings.auth_secret.get_secret_value(), owner)}"}
    r = client.post(
        "/api/v1/projects",
        json={"project_name": "Chat outage", "market": {"city": "Los Angeles", "state": "CA"}},
        headers=h,
    )
    assert r.status_code == 201, r.text
    project_id = r.json()["project"]["project_id"]
    monkeypatch.setattr(container.settings, "backend_api_url", rust_url)  # HTTP mode, real socket transport
    for message, tool in (
        ("rank the properties", "get_ranking"),
        ("what's the status?", "get_project_state"),
    ):
        r = client.post(f"/api/v1/projects/{project_id}/chat", json={"message": message}, headers=h)
        assert r.status_code == 200, r.text  # was a 500 INTERNAL_ERROR (KeyError) before
        out = r.json()
        assert out["mode"] == "deterministic" and out["grounded_record_ids"] == []
        assert out["reply"].startswith(
            "No ranking to show:" if tool == "get_ranking" else "No project status"
        )
        assert tool in out["reply"] and "PROVIDER_UNAVAILABLE" in out["reply"]
        assert "database unavailable" in out["reply"]
    assert [p for _m, p, _h in hits] == [
        f"/api/v1/projects/{project_id}/rankings",
        f"/api/v1/projects/{project_id}",
    ]


# ---- the client to the Rust API ignores ambient proxy settings (R8) ------------------------------------------


def test_proxy_environment_does_not_change_where_the_request_goes(monkeypatch, local_http):
    rust_url, rust_hits = local_http(lambda _m, _p: (200, []))
    proxy_url, proxy_hits = local_http(lambda _m, _p: (502, {"message": "via proxy", "error_code": "PROXY"}))
    for var in ("NO_PROXY", "no_proxy"):
        monkeypatch.delenv(var, raising=False)
    for var in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
        monkeypatch.setenv(var, proxy_url)

    # Control: a default httpx client in this environment really goes through the proxy.
    assert httpx.get(f"{rust_url}/control").status_code == 502
    assert [p for _m, p, _h in proxy_hits] == [f"{rust_url}/control"] and rust_hits == []

    owner, project = uuid4(), uuid4()
    tools = {t.name: t for t in build_tools(_http_ctx(None, project, owner, url=rust_url))}
    assert json.loads(tools["get_ranking"].invoke({})) == {
        "status": "NO_RANKING"
    }  # answered by the Rust stand-in
    assert [(m, p) for m, p, _h in rust_hits] == [("GET", f"/api/v1/projects/{project}/rankings")]
    assert verify_token(SECRET, rust_hits[0][2]["authorization"].split(" ", 1)[1]) == owner
    assert len(proxy_hits) == 1  # only the control request; the owner's token never reached the proxy
    assert "authorization" not in proxy_hits[0][2]


# ---- the request id crosses from Python to Rust (e2e-1 D5) ---------------------------------------------------


def test_the_current_request_id_goes_to_rust_and_none_is_invented(rust):
    from app.core.logging import request_id_var

    tools = {t.name: t for t in build_tools(rust.ctx())}

    def call_tools() -> list[httpx.Request]:
        rust.requests.clear()
        tools["get_ranking"].invoke({})
        tools["get_project_state"].invoke({})
        tools["run_property_analysis"].invoke({})
        assert [(r.method, r.url.path.rsplit("/", 1)[-1]) for r in rust.requests] == [
            ("GET", "rankings"),
            ("GET", str(rust.project.project_id)),
            ("POST", "search"),
        ]
        return list(rust.requests)

    # Outside a request (the Photon poller, a script): nothing is set, so nothing is sent.
    assert request_id_var.get() is None
    assert all("x-request-id" not in r.headers for r in call_tools())
    for rid, wire in (
        ("e2e-chat-1", b"e2e-chat-1"),
        # Starlette decodes header values as latin-1, so a received id is sent back as exactly the bytes that came in.
        ("caf\xe9-7", b"caf\xe9-7"),
    ):
        token = request_id_var.set(rid)
        try:
            sent = call_tools()
        finally:
            request_id_var.reset(token)
        assert [r.headers.get_list("x-request-id") for r in sent] == [[rid]] * 3
        assert all(dict(r.headers.raw)[b"x-request-id"] == wire for r in sent)
        _assert_owner_token(sent[0], rust.owner)  # the other headers are unchanged
    assert all("x-request-id" not in r.headers for r in call_tools())


def test_chat_route_sends_its_request_id_to_rust(client, container, local_http, monkeypatch):
    from app.core.security import issue_token

    rust_url, hits = local_http(lambda _m, _p: (200, []))  # no ranking yet
    owner = uuid4()
    h = {"Authorization": f"Bearer {issue_token(container.settings.auth_secret.get_secret_value(), owner)}"}
    r = client.post(
        "/api/v1/projects",
        json={"project_name": "Chat request id", "market": {"city": "Los Angeles", "state": "CA"}},
        headers=h,
    )
    assert r.status_code == 201, r.text
    project_id = r.json()["project"]["project_id"]
    monkeypatch.setattr(container.settings, "backend_api_url", rust_url)  # HTTP mode, real socket transport
    chat = f"/api/v1/projects/{project_id}/chat"

    r = client.post(
        chat, json={"message": "rank the properties"}, headers={**h, "x-request-id": "e2e-chat-42"}
    )
    assert r.status_code == 200 and r.headers["x-request-id"] == "e2e-chat-42", r.text
    # No ranking yet, so the model-free supervisor also runs the search: both calls carry the chat's id.
    assert [(p, hdrs.get("x-request-id")) for _m, p, hdrs in hits] == [
        (f"/api/v1/projects/{project_id}/rankings", "e2e-chat-42"),
        (f"/api/v1/projects/{project_id}/properties/search", "e2e-chat-42"),
    ]

    # Without one, the app's request middleware assigns an id; Rust receives that same id.
    hits.clear()
    r = client.post(chat, json={"message": "rank the properties"}, headers=h)
    assert r.status_code == 200, r.text
    assigned = r.headers["x-request-id"]
    assert UUID(assigned) and [hdrs.get("x-request-id") for _m, _p, hdrs in hits] == [assigned, assigned]


def test_a_request_id_byte_outside_ascii_reaches_rust_unchanged(client, container, local_http, monkeypatch):
    """Through a real server (uvicorn on an ephemeral port) and raw HTTP: the test client can neither send nor read
    back a header byte outside ASCII. Before the client sent the id as bytes, the same value as a str made httpx raise
    UnicodeEncodeError inside the tool call."""
    import http.client
    import threading

    import uvicorn

    from app.core.security import issue_token
    from app.main import app

    rust_url, hits = local_http(lambda _m, _p: (200, []))
    owner = uuid4()
    token = issue_token(container.settings.auth_secret.get_secret_value(), owner)
    r = client.post(
        "/api/v1/projects",
        json={"project_name": "Chat request id bytes", "market": {"city": "Los Angeles", "state": "CA"}},
        headers={"Authorization": f"Bearer {token}"},
    )
    assert r.status_code == 201, r.text
    chat = f"/api/v1/projects/{r.json()['project']['project_id']}/chat"
    monkeypatch.setattr(container.settings, "backend_api_url", rust_url)

    server = uvicorn.Server(
        uvicorn.Config(app, host="127.0.0.1", port=0, log_level="warning", lifespan="off")
    )
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()
    try:
        deadline = time.monotonic() + 10
        while not server.started and time.monotonic() < deadline:
            time.sleep(0.02)
        assert server.started
        port = server.servers[0].sockets[0].getsockname()[1]
        body = json.dumps({"message": "rank the properties"}).encode()
        conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        conn.putrequest("POST", chat)
        for name, value in (
            ("Authorization", f"Bearer {token}"),
            ("Content-Type", "application/json"),
            ("Content-Length", str(len(body))),
            ("x-request-id", b"caf\xe9-9"),
        ):
            conn.putheader(name, value)
        conn.endheaders(body)
        resp = conn.getresponse()
        answer = resp.read()
        conn.close()
    finally:
        server.should_exit = True
        thread.join(10)
    assert resp.status == 200, answer
    assert resp.getheader("x-request-id") == "caf\xe9-9"  # http.client reads header bytes as latin-1
    assert [hdrs.get("x-request-id") for _m, _p, hdrs in hits] == ["caf\xe9-9"] * 2  # so does the stand-in
