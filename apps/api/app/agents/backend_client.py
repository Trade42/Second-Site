"""HTTP client for the Rust Second Site API, used by the supervisor tools when BACKEND_API_URL is set.

- Each request carries a fresh short-lived access token minted for the project owner with `issue_token`, so the
  Rust API applies the same owner and project checks as for the web app. It also carries the `x-request-id` of the
  request being served, when there is one, so both services log the same id.
- Only the routes the tools need can be called (`ALLOWED_ROUTES`, method + path). Path ids are UUIDs, checked
  before a path is built. Nothing here can execute an external action: the one outreach write,
  `POST .../outreach`, only creates a PENDING approval and a draft.
- Responses are validated against the same Pydantic contracts the in-process tools use, so the tools return the
  same JSON text in both modes. A non-2xx answer (ProblemDetail), a timeout, an unreachable service or a body
  that does not match the contract raises `BackendAPIError`: a ToolException whose text is a short honest error
  for the model. Nothing is swallowed or turned into an empty success.
"""

from __future__ import annotations

import json
import re
from typing import Any
from uuid import UUID

import httpx
from langchain_core.tools import ToolException
from pydantic import TypeAdapter, ValidationError

from app.contracts import (
    ApprovalRequest,
    CommunicationSession,
    CostScenario,
    InfrastructureAssessment,
    ProjectEnvelope,
    PropertyRankingResult,
    SourceEvidence,
    TaskAccepted,
    TaskStatus,
)
from app.core.config import Settings
from app.core.logging import request_id_var
from app.core.security import issue_token

TOKEN_TTL_S = 300
DEFAULT_TIMEOUT = httpx.Timeout(20.0, connect=5.0)
# The search route runs the whole analysis inline (listings, permits, enrichment, ranking) before it answers.
SEARCH_TIMEOUT = httpx.Timeout(120.0, connect=5.0)
MAX_MESSAGE_CHARS = 300
# `status` of a tool error text (`BackendAPIError.tool_text`); the model-free supervisor detects errors by it.
TOOL_ERROR_STATUS = "ERROR"

_ID = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}"
_PROJECT = rf"/api/v1/projects/{_ID}"
ALLOWED_ROUTES: tuple[tuple[str, re.Pattern[str]], ...] = tuple(
    (method, re.compile(rf"^{pattern}$"))
    for method, pattern in (
        ("GET", _PROJECT),
        ("POST", rf"{_PROJECT}/properties/search"),
        ("GET", rf"{_PROJECT}/rankings"),
        ("GET", rf"{_PROJECT}/properties/{_ID}/feasibility"),
        ("GET", rf"{_PROJECT}/properties/{_ID}/evidence"),
        ("GET", rf"{_PROJECT}/properties/{_ID}/evidence/validation"),
        ("GET", rf"{_PROJECT}/cost-scenarios"),
        ("POST", rf"{_PROJECT}/outreach"),  # creates a PENDING approval; never sends or calls
    )
)


class RouteNotAllowed(RuntimeError):
    """A programming error: a tool tried to reach a Rust route outside ALLOWED_ROUTES. Never sent."""


class BackendAPIError(ToolException):
    """A failed Rust API call, worded for the model (the ToolException text is a short JSON object)."""

    def __init__(
        self,
        message: str,
        *,
        error_code: str,
        http_status: int | None = None,
        retryable: bool = False,
        request_id: str | None = None,
    ):
        self.message = message[:MAX_MESSAGE_CHARS]
        self.error_code = error_code
        self.http_status = http_status
        self.retryable = retryable
        self.request_id = request_id
        super().__init__(self.tool_text())

    def tool_text(self) -> str:
        out: dict[str, Any] = {
            "status": TOOL_ERROR_STATUS,
            "error_code": self.error_code,
            "message": self.message,
            "http_status": self.http_status,
            "retryable": self.retryable,
        }
        if self.request_id:
            out["request_id"] = self.request_id
        return json.dumps(out)


def parse_id(value: str, name: str) -> UUID:
    """A tool argument that becomes part of a path must be a UUID (never a path fragment)."""
    try:
        return UUID(str(value))
    except ValueError:
        raise BackendAPIError(
            f"{name} must be a UUID, got {str(value)[:60]!r}", error_code="INVALID_ARGUMENT"
        ) from None


def _problem(resp: httpx.Response) -> BackendAPIError:
    try:
        body = resp.json()
    except ValueError:
        body = None
    if isinstance(body, dict) and isinstance(body.get("message"), str):
        return BackendAPIError(
            body["message"],
            error_code=str(body.get("error_code") or f"HTTP_{resp.status_code}"),
            http_status=resp.status_code,
            retryable=bool(body.get("retryable", False)),
            request_id=body.get("request_id") if isinstance(body.get("request_id"), str) else None,
        )
    return BackendAPIError(
        f"Rust API answered HTTP {resp.status_code} without a ProblemDetail body",
        error_code=f"HTTP_{resp.status_code}",
        http_status=resp.status_code,
        retryable=resp.status_code >= 500,
    )


def _request_id_header() -> dict[str, bytes]:
    """`x-request-id` of the request being served (`request_id_var`, set by the app's request middleware from the
    incoming header, or generated there), so one supervisor answer can be traced through both services' logs.
    Outside a request (the Photon poller, a script) nothing is set and nothing is sent; an id is never invented here.
    Sent as the latin-1 bytes Starlette decoded it from, i.e. exactly what came in; a str value outside ASCII would
    make httpx raise."""
    rid = request_id_var.get()
    if not rid:
        return {}
    try:
        return {"x-request-id": rid.encode("latin-1")}
    except UnicodeEncodeError:
        # Not from a header (only code could set such a value): send none rather than fail the call.
        return {}


def _contract(adapter: TypeAdapter, data: Any, name: str) -> Any:
    try:
        return adapter.validate_python(data)
    except ValidationError as exc:
        first = exc.errors(include_url=False, include_context=False)[0]
        where = ".".join(str(x) for x in first.get("loc", ())) or "body"
        raise BackendAPIError(
            f"Rust API answer does not match the {name} contract ({where}: {first.get('msg')})",
            error_code="BACKEND_CONTRACT_MISMATCH",
        ) from exc


_ENVELOPE = TypeAdapter(ProjectEnvelope)
_TASK = TypeAdapter(TaskAccepted)
_RANKINGS = TypeAdapter(list[PropertyRankingResult])
_ASSESSMENTS = TypeAdapter(list[InfrastructureAssessment])
_EVIDENCE = TypeAdapter(list[SourceEvidence])
_SCENARIOS = TypeAdapter(list[CostScenario])
_PROBLEMS = TypeAdapter(list[str])
_APPROVAL = TypeAdapter(ApprovalRequest)
_COMMUNICATION = TypeAdapter(CommunicationSession)
_IDS = TypeAdapter(list[UUID])


class BackendClient:
    def __init__(
        self,
        base_url: str,
        auth_secret: str,
        owner_id: UUID,
        transport: httpx.BaseTransport | None = None,
    ):
        url = base_url.strip().rstrip("/")
        if not url.startswith(("http://", "https://")):
            raise ValueError("BACKEND_API_URL must be an http(s) origin, e.g. http://localhost:8000")
        self.base_url = url
        self._secret = auth_secret
        self.owner_id = owner_id
        self._transport = transport

    # ---- transport ----------------------------------------------------------------------------
    def _send(
        self, method: str, path: str, *, body: Any = None, timeout: httpx.Timeout = DEFAULT_TIMEOUT
    ) -> Any:
        if not any(m == method and rx.fullmatch(path) for m, rx in ALLOWED_ROUTES):
            raise RouteNotAllowed(f"{method} {path} is not a supervisor tool route")
        token = issue_token(self._secret, self.owner_id, ttl_s=TOKEN_TTL_S)
        try:
            # trust_env=False: proxy variables never apply, so the owner's token goes only to the Rust API. It also
            # ignores SSL_CERT_FILE / SSL_CERT_DIR: an https origin with a private CA needs explicit configuration.
            with httpx.Client(
                base_url=self.base_url, transport=self._transport, timeout=timeout, trust_env=False
            ) as client:
                resp = client.request(
                    method,
                    path,
                    json=body,
                    headers={
                        "Authorization": f"Bearer {token}",
                        "Accept": "application/json",
                        **_request_id_header(),
                    },
                )
        except httpx.TimeoutException as exc:
            outcome = "" if method == "GET" else " Whether the request took effect is unknown."
            raise BackendAPIError(
                f"Rust API did not answer {method} {path} in time ({type(exc).__name__}).{outcome}",
                error_code="BACKEND_TIMEOUT",
                retryable=True,
            ) from exc
        except httpx.HTTPError as exc:
            raise BackendAPIError(
                f"Rust API unreachable for {method} {path} ({type(exc).__name__})",
                error_code="BACKEND_UNAVAILABLE",
                retryable=True,
            ) from exc
        if not resp.is_success:
            raise _problem(resp)
        try:
            return resp.json()
        except ValueError as exc:
            raise BackendAPIError(
                f"Rust API answered {method} {path} with a body that is not JSON",
                error_code="BACKEND_BAD_RESPONSE",
                http_status=resp.status_code,
            ) from exc

    # ---- one method per tool route ------------------------------------------------------------
    def get_project(self, project_id: UUID) -> ProjectEnvelope:
        return _contract(_ENVELOPE, self._send("GET", f"/api/v1/projects/{project_id}"), "ProjectEnvelope")

    def run_property_search(self, project_id: UUID, body: dict[str, Any]) -> dict[str, Any]:
        """POST .../properties/search (202, runs inline). The completed task's output, else an error."""
        data = self._send(
            "POST", f"/api/v1/projects/{project_id}/properties/search", body=body, timeout=SEARCH_TIMEOUT
        )
        task = _contract(_TASK, data, "TaskAccepted").task
        if task.status == TaskStatus.FAILED:
            raise BackendAPIError(
                f"property analysis task {task.task_id} FAILED: {task.error_message or 'no message'}",
                error_code=task.error_code or "TASK_FAILED",
                http_status=202,
            )
        if task.status != TaskStatus.COMPLETED:
            raise BackendAPIError(
                f"property analysis task {task.task_id} is {task.status.value}; no results yet",
                error_code="TASK_NOT_FINISHED",
                http_status=202,
                retryable=True,
            )
        out = task.output_reference
        _contract(_IDS, out.get("property_ids"), "TaskRecord.output_reference.property_ids")
        return out

    def list_rankings(self, project_id: UUID) -> list[PropertyRankingResult]:
        data = self._send("GET", f"/api/v1/projects/{project_id}/rankings")
        return _contract(_RANKINGS, data, "list[PropertyRankingResult]")

    def property_feasibility(self, project_id: UUID, property_id: UUID) -> list[InfrastructureAssessment]:
        data = self._send("GET", f"/api/v1/projects/{project_id}/properties/{property_id}/feasibility")
        return _contract(_ASSESSMENTS, data, "list[InfrastructureAssessment]")

    def property_evidence(self, project_id: UUID, property_id: UUID) -> list[SourceEvidence]:
        data = self._send("GET", f"/api/v1/projects/{project_id}/properties/{property_id}/evidence")
        return _contract(_EVIDENCE, data, "list[SourceEvidence]")

    def property_evidence_problems(self, project_id: UUID, property_id: UUID) -> list[str]:
        data = self._send(
            "GET", f"/api/v1/projects/{project_id}/properties/{property_id}/evidence/validation"
        )
        if not isinstance(data, dict) or set(data) != {"problems"}:
            raise BackendAPIError(
                'Rust API answer does not match {"problems": [string]}',
                error_code="BACKEND_CONTRACT_MISMATCH",
            )
        return _contract(_PROBLEMS, data["problems"], "problems")

    def cost_scenarios(self, project_id: UUID) -> list[CostScenario]:
        data = self._send("GET", f"/api/v1/projects/{project_id}/cost-scenarios")
        return _contract(_SCENARIOS, data, "list[CostScenario]")

    def request_outreach(self, project_id: UUID, body: dict[str, Any]) -> ApprovalRequest:
        """POST .../outreach: a communication draft + PENDING approval. Nothing is sent or called."""
        data = self._send("POST", f"/api/v1/projects/{project_id}/outreach", body=body)
        if not isinstance(data, dict) or set(data) != {"communication", "approval"}:
            raise BackendAPIError(
                "Rust API answer does not match the OutreachAccepted contract",
                error_code="BACKEND_CONTRACT_MISMATCH",
            )
        _contract(_COMMUNICATION, data["communication"], "CommunicationSession")
        return _contract(_APPROVAL, data["approval"], "ApprovalRequest")


def backend_client(
    settings: Settings, owner_id: UUID, transport: httpx.BaseTransport | None = None
) -> BackendClient | None:
    """A client when BACKEND_API_URL is set, else None (the tools then run in-process, as before)."""
    url = (settings.backend_api_url or "").strip()
    if not url:
        return None
    return BackendClient(url, settings.auth_secret.get_secret_value(), owner_id, transport=transport)
